//! Filesystem + shell backend for the tools: local, or a remote host over
//! SSH (opt-in remote workspace mode).
//!
//! Every file tool (read_file, edit_file, list_files, grep) and the bash
//! tool go through a [`Backend`] instead of touching `std::fs`/`sh`
//! directly, so pointing the same tools at a remote host is a matter of
//! swapping the backend — the tool schemas and the system prompt never
//! change, which keeps the model-facing surface identical (the minimal
//! surface small local models need).
//!
//! The remote backend shells out to the system `ssh` binary through a
//! ControlMaster socket established once at connect time: one
//! authentication, and every later file op and command reuses the live
//! connection. Credentials, `~/.ssh/config`, ProxyJump, agent and
//! known_hosts are entirely the user's ssh setup — picocode never sees a
//! secret. File reads/writes go over `cat`, listing over
//! `git ls-files`/`find`; the bash tool's own process machinery
//! (timeout, background jobs, kill-on-drop) is unchanged because a remote
//! command is still a local `ssh` child process whose lifetime maps to
//! the remote command.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod ssh;

pub use ssh::{SshBackend, config_hosts as ssh_config_hosts};

/// A file/shell backend: the local machine or a remote SSH host.
#[derive(Clone)]
pub enum Backend {
    Local,
    Ssh(Arc<SshBackend>),
}

/// A backend plus the working-directory root the tools operate in — the
/// handle every file tool holds instead of a bare `root: PathBuf`.
#[derive(Clone)]
pub struct Workspace {
    pub backend: Backend,
    pub root: PathBuf,
}

impl Workspace {
    /// A local workspace rooted at `root` (the default).
    pub fn local(root: PathBuf) -> Self {
        Self {
            backend: Backend::Local,
            root,
        }
    }
}

impl Backend {
    /// Short label for the status line: "local" or "user@host".
    pub fn label(&self) -> String {
        match self {
            Backend::Local => "local".to_string(),
            Backend::Ssh(s) => s.label().to_string(),
        }
    }

    /// Whether file paths shown to the user should include the host.
    pub fn is_remote(&self) -> bool {
        matches!(self, Backend::Ssh(_))
    }

    /// Read a file's bytes.
    pub async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self {
            Backend::Local => tokio::fs::read(path).await,
            Backend::Ssh(s) => s.read(path).await,
        }
    }

    /// Write bytes to a file (creating it), replacing the old contents in
    /// one step rather than truncating and refilling.
    pub async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        match self {
            Backend::Local => write_atomically(path, data).await,
            Backend::Ssh(s) => s.write(path, data).await,
        }
    }

    /// Delete a file. Used by `/undo` to take back a file the turn created;
    /// a file that is already gone is not an error on either backend.
    pub async fn remove_file(&self, path: &Path) -> io::Result<()> {
        match self {
            Backend::Local => tokio::fs::remove_file(path).await,
            Backend::Ssh(s) => s.remove_file(path).await,
        }
    }

    /// Create a directory and all missing parents.
    pub async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        match self {
            Backend::Local => tokio::fs::create_dir_all(path).await,
            Backend::Ssh(s) => s.create_dir_all(path).await,
        }
    }

    /// The file's modification time, for stale-write detection. Remote
    /// hosts report second granularity.
    pub async fn mtime(&self, path: &Path) -> io::Result<SystemTime> {
        match self {
            Backend::Local => tokio::fs::metadata(path).await?.modified(),
            Backend::Ssh(s) => {
                let secs = s.mtime_secs(path).await?;
                Ok(UNIX_EPOCH + Duration::from_secs(secs))
            }
        }
    }

    pub async fn is_file(&self, path: &Path) -> bool {
        match self {
            Backend::Local => tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file()),
            Backend::Ssh(s) => s.test(path, 'f').await,
        }
    }

    pub async fn is_dir(&self, path: &Path) -> bool {
        match self {
            Backend::Local => tokio::fs::metadata(path).await.is_ok_and(|m| m.is_dir()),
            Backend::Ssh(s) => s.test(path, 'd').await,
        }
    }

    /// Relative paths of the files under `base`, honoring `.gitignore`
    /// (git-tracked plus unignored untracked files; a plain recursive
    /// listing when it isn't a git repo).
    pub async fn walk_files(&self, base: &Path) -> io::Result<Vec<PathBuf>> {
        match self {
            Backend::Local => local_walk(base).await,
            Backend::Ssh(s) => s.walk_files(base).await,
        }
    }

    /// Build the shell command for the bash tool / after_edit hook. Local
    /// commands may be sandboxed; remote commands run on the host over the
    /// shared ssh connection, `cd`'d into `cwd`. The returned value is a
    /// local child process either way, so the bash tool's timeout /
    /// background / kill logic is backend-agnostic.
    pub fn shell(
        &self,
        command: &str,
        cwd: &Path,
        sandbox: &crate::sandbox::SandboxCtx,
    ) -> anyhow::Result<tokio::process::Command> {
        match self {
            Backend::Local => {
                let mut c = crate::sandbox::shell_for(command, cwd, sandbox)?;
                c.current_dir(cwd);
                Ok(c)
            }
            // The sandbox is a local-only guard; a remote command runs with
            // the host's own permissions (documented).
            Backend::Ssh(s) => Ok(s.shell(command, cwd)),
        }
    }
}

/// Local gitignore-aware walk on a blocking thread (the `ignore` crate is
/// synchronous), returning file paths relative to `base`.
async fn local_walk(base: &Path) -> io::Result<Vec<PathBuf>> {
    let base = base.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        for entry in ignore::WalkBuilder::new(&base)
            .hidden(true)
            .git_ignore(true)
            .require_git(false)
            .build()
            .flatten()
        {
            if entry.file_type().is_some_and(|t| t.is_file())
                && let Ok(rel) = entry.path().strip_prefix(&base)
            {
                files.push(rel.to_path_buf());
            }
        }
        files
    })
    .await
    .map_err(io::Error::other)
}

/// Replace a file's contents in one step: write a sibling temp file, then
/// rename it over the target.
///
/// A plain write truncates first, so a crash, a full disk or a dropped
/// connection mid-write left the file empty or half-written — and the undo
/// journal only lives in memory, so there was nothing to recover from.
///
/// Two cases deliberately keep the plain write. A symlink target must be
/// written *through*, not replaced by a regular file. And a rename does not
/// carry the original's permissions, so those are copied onto the temp file
/// first — otherwise editing an executable script would silently drop its
/// mode bits.
async fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    let is_symlink = tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|m| m.file_type().is_symlink());
    let name = path.file_name();
    let (Some(dir), Some(name)) = (path.parent(), name) else {
        return tokio::fs::write(path, data).await;
    };
    if is_symlink {
        return tokio::fs::write(path, data).await;
    }

    let tmp = dir.join(format!(".{}.picocode-tmp", name.to_string_lossy()));
    tokio::fs::write(&tmp, data).await?;
    if let Ok(meta) = tokio::fs::metadata(path).await {
        let _ = tokio::fs::set_permissions(&tmp, meta.permissions()).await;
    }
    match tokio::fs::rename(&tmp, path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Leaving the temp file behind would be worse than the failure.
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_replace_the_file_without_a_truncated_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        let backend = Backend::Local;

        backend.write(&path, b"first").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        backend.write(&path, b"second").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        // No temp file survives a successful write.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("picocode-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The rename would otherwise replace the link with a regular file.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_is_written_through_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        Backend::Local.write(&link, b"new").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// A rename brings the temp file's fresh permissions with it, so an
    /// edited script would lose its executable bit.
    #[cfg(unix)]
    #[tokio::test]
    async fn permissions_survive_a_write() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sh");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        Backend::Local
            .write(&path, b"#!/bin/sh\necho hi\n")
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "{mode:o}");
    }
}
