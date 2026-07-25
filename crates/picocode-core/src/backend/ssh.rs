//! Remote backend: the system `ssh` binary driven through a ControlMaster
//! socket. See the parent module for the rationale.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

/// Serial for unique control-socket names within the process.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A connected remote host. Dropping it tears the ControlMaster down.
pub struct SshBackend {
    /// ssh destination — an alias from `~/.ssh/config` or `user@host`.
    destination: String,
    /// Path to the ControlMaster socket shared by every later ssh call.
    control: PathBuf,
    label: String,
}

/// Single-quote a string for a POSIX remote shell: wrap in `'…'` and
/// escape embedded single quotes as `'\''`.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The `Host` aliases defined in `~/.ssh/config`, offered as suggestions
/// by the add-remote dialogs. Pattern entries (`Host *`, `Host web?`) are
/// skipped — they configure other hosts rather than name one. `Include`
/// directives are not followed, so a split config may list fewer hosts
/// than ssh itself knows; typing a destination always works.
pub fn config_hosts() -> Vec<String> {
    let Some(home) = crate::config::home_dir() else {
        return Vec::new();
    };
    match std::fs::read_to_string(home.join(".ssh/config")) {
        Ok(text) => parse_hosts(&text),
        Err(_) => Vec::new(),
    }
}

fn parse_hosts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line
            .split_once(char::is_whitespace)
            .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
            .map(|(_, rest)| rest)
        else {
            continue;
        };
        for alias in rest.split_whitespace() {
            if !alias.contains(['*', '?', '!']) && !out.iter().any(|h| h == alias) {
                out.push(alias.to_string());
            }
        }
    }
    out
}

impl SshBackend {
    /// Open a persistent connection to `destination` (an ssh alias or
    /// `user@host`). Authentication is entirely ssh's own (keys, agent,
    /// `~/.ssh/config`); we never handle credentials.
    pub async fn connect(destination: &str) -> anyhow::Result<Self> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // Keep the socket path short — unix sockets cap at ~104 bytes.
        let control = std::env::temp_dir().join(format!("pc-ssh-{}-{n}.sock", std::process::id()));

        // Establish the master in the background; ControlPersist keeps it
        // alive briefly after we exit so a clean shutdown doesn't race.
        let status = tokio::process::Command::new("ssh")
            .args(["-N", "-f", "-M", "-o", "ControlPersist=30", "-S"])
            .arg(&control)
            .arg(destination)
            .stdin(Stdio::null())
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("could not run ssh: {e}"))?;
        if !status.success() {
            anyhow::bail!(
                "ssh could not connect to `{destination}` (check ~/.ssh/config, keys, and that \
                 the host is reachable)"
            );
        }

        let mut backend = Self {
            destination: destination.to_string(),
            control,
            label: destination.to_string(),
        };
        // Resolve `user@host` for the label (an alias hides both).
        if let Ok(out) = backend
            .run("printf '%s@%s' \"$(whoami)\" \"$(hostname -s)\"")
            .await
            && out.status.success()
        {
            let who = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !who.is_empty() {
                backend.label = who;
            }
        }
        Ok(backend)
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// A base `ssh` command bound to the shared ControlMaster socket.
    fn ssh(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new("ssh");
        c.arg("-S").arg(&self.control).arg(&self.destination);
        c
    }

    /// Run a remote command through the master, capturing its output.
    async fn run(&self, remote: &str) -> io::Result<std::process::Output> {
        self.ssh()
            .arg("--")
            .arg(remote)
            .stdin(Stdio::null())
            .output()
            .await
    }

    /// Map a failed remote command to an io error carrying its stderr.
    fn check(out: std::process::Output, what: &str) -> io::Result<Vec<u8>> {
        if out.status.success() {
            Ok(out.stdout)
        } else {
            let msg = String::from_utf8_lossy(&out.stderr);
            Err(io::Error::other(format!("{what}: {}", msg.trim())))
        }
    }

    pub async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let out = self
            .run(&format!("cat -- {}", shq(&path.display().to_string())))
            .await?;
        Self::check(out, "read failed")
    }

    pub async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        // `cat > path` with the bytes on stdin: binary-safe.
        let mut child = self
            .ssh()
            .arg("--")
            .arg(format!("cat > {}", shq(&path.display().to_string())))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(data).await?;
        // stdin dropped here → EOF → remote cat finishes.
        let out = child.wait_with_output().await?;
        Self::check(out, "write failed").map(|_| ())
    }

    pub async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let out = self
            .run(&format!("mkdir -p -- {}", shq(&path.display().to_string())))
            .await?;
        Self::check(out, "mkdir failed").map(|_| ())
    }

    /// Modification time as epoch seconds (BSD `stat -f %m`, else GNU
    /// `stat -c %Y`). Second granularity is enough for stale detection.
    pub async fn mtime_secs(&self, path: &Path) -> io::Result<u64> {
        let p = shq(&path.display().to_string());
        let out = self
            .run(&format!(
                "stat -f %m -- {p} 2>/dev/null || stat -c %Y -- {p}"
            ))
            .await?;
        let text = String::from_utf8_lossy(&Self::check(out, "stat failed")?)
            .trim()
            .to_string();
        text.parse()
            .map_err(|_| io::Error::other(format!("unexpected stat output: {text}")))
    }

    /// `test -<flag> path` succeeded (`f` = file, `d` = dir). No `--`:
    /// POSIX/BSD `test` doesn't accept it (the path is quoted instead).
    pub async fn test(&self, path: &Path, flag: char) -> bool {
        self.run(&format!(
            "test -{flag} {}",
            shq(&path.display().to_string())
        ))
        .await
        .is_ok_and(|o| o.status.success())
    }

    /// Files under `base`, honoring `.gitignore` via `git ls-files`
    /// (tracked + unignored untracked), falling back to `find` off git.
    pub async fn walk_files(&self, base: &Path) -> io::Result<Vec<PathBuf>> {
        let dir = shq(&base.display().to_string());
        let out = self
            .run(&format!(
                "cd {dir} && (git ls-files -co --exclude-standard 2>/dev/null || find . -type f)"
            ))
            .await?;
        let text = String::from_utf8_lossy(&Self::check(out, "listing failed")?).into_owned();
        Ok(text
            .lines()
            .map(|l| PathBuf::from(l.strip_prefix("./").unwrap_or(l)))
            .filter(|p| !p.as_os_str().is_empty())
            .collect())
    }

    /// A local `ssh` child that runs `command` on the host inside `cwd`.
    /// The bash tool owns this like any local process (timeout, kill).
    pub fn shell(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        let mut c = self.ssh();
        c.arg("--").arg(format!(
            "cd {} && {command}",
            shq(&cwd.display().to_string())
        ));
        c
    }
}

impl Drop for SshBackend {
    fn drop(&mut self) {
        // Close the ControlMaster (best effort; ControlPersist also expires
        // it). A blocking spawn keeps Drop synchronous.
        let _ = std::process::Command::new("ssh")
            .arg("-S")
            .arg(&self.control)
            .args(["-O", "exit"])
            .arg(&self.destination)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_hosts, shq};

    #[test]
    fn ssh_config_hosts_skip_patterns_and_comments() {
        let hosts = parse_hosts(
            "# comment\n\
             Host prod staging\n  HostName example.com\n\
             Host *\n  ForwardAgent yes\n\
             host lower-case\n\
             HostName not-a-host-line\n\
             Host prod\n",
        );
        assert_eq!(hosts, ["prod", "staging", "lower-case"]);
    }

    #[test]
    fn shell_quoting_escapes_single_quotes() {
        assert_eq!(shq("plain"), "'plain'");
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("it's"), r"'it'\''s'");
        assert_eq!(shq("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    /// Live round-trip against a real host. Set `PICOCODE_SSH_TEST` to an
    /// ssh destination (e.g. an alias) with a writable `/tmp`:
    /// `PICOCODE_SSH_TEST=pctest cargo test -p picocode-core ssh_roundtrip -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a live ssh host in PICOCODE_SSH_TEST"]
    async fn ssh_roundtrip() {
        use super::SshBackend;
        use std::path::Path;
        let dest = std::env::var("PICOCODE_SSH_TEST").expect("set PICOCODE_SSH_TEST");
        let ssh = SshBackend::connect(&dest).await.expect("connect");
        println!("label: {}", ssh.label());

        let dir = format!("/tmp/picocode-ssh-test-{}", std::process::id());
        ssh.create_dir_all(Path::new(&dir)).await.expect("mkdir");
        let file = format!("{dir}/hello.txt");
        let path = Path::new(&file);

        ssh.write(path, b"remote bytes\n").await.expect("write");
        assert!(ssh.test(path, 'f').await, "file should exist");
        let back = ssh.read(path).await.expect("read");
        assert_eq!(back, b"remote bytes\n");
        let m = ssh.mtime_secs(path).await.expect("mtime");
        assert!(m > 1_600_000_000, "plausible epoch, got {m}");

        // Binary round-trip (image bytes).
        let bin = format!("{dir}/blob");
        let data = vec![0u8, 159, 146, 150, 0, 255];
        ssh.write(Path::new(&bin), &data).await.expect("write bin");
        assert_eq!(ssh.read(Path::new(&bin)).await.unwrap(), data);

        // Shell runs on the host in the given cwd.
        let out = ssh
            .shell("pwd", Path::new(&dir))
            .output()
            .await
            .expect("shell");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), dir);

        // Walk lists the file we wrote (find fallback, not a git repo).
        let files = ssh.walk_files(Path::new(&dir)).await.expect("walk");
        assert!(files.iter().any(|p| p.ends_with("hello.txt")), "{files:?}");

        let _ = ssh
            .shell(&format!("rm -rf {dir}"), Path::new("/tmp"))
            .output()
            .await;
        println!("ssh round-trip OK");
    }
}
