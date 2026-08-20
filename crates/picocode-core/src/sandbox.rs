//! OS-level sandboxing for model-initiated bash commands (opt-in via
//! `[sandbox]` in picocode.toml).
//!
//! The built-in file tools are already confined to the project root in
//! code; the open surface is the `bash` tool. When active, its commands
//! run inside an OS sandbox that confines writes to the project root,
//! the temp directories and any configured `allow_write` paths, and
//! (optionally) blocks network access:
//!
//! - macOS: the command is wrapped in `/usr/bin/sandbox-exec` with a
//!   generated Seatbelt profile. Officially deprecated but stable — the
//!   same mechanism Chromium and Bazel rely on.
//! - Linux: Landlock (kernel 5.13+), applied in the child between fork
//!   and exec — no helper binary, no privileges. Filesystem confinement
//!   is a hard requirement (unsupported kernels fail the command instead
//!   of silently running unconfined); the TCP block needs kernel 6.7+
//!   and quietly stays off on older kernels (documented).
//! - Windows: not supported — commands fail with a clear error while a
//!   sandbox is requested.
//!
//! `!` commands and the `after_edit` hook are user-authored and run
//! unsandboxed ([`SandboxCtx::off`]).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{Mode, ModeHandle};

/// When the sandbox applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Never (the default).
    #[default]
    Off,
    /// Only while the permission mode is bypass.
    Bypass,
    /// In every permission mode.
    Always,
}

/// The `[sandbox]` settings.
#[derive(Clone, Debug, Default)]
pub struct SandboxSettings {
    pub mode: SandboxMode,
    /// Allow network access from sandboxed commands (default: blocked).
    pub allow_network: bool,
    /// Extra write-allowed paths beyond the project root and temp dirs
    /// (a leading `~` expands to the home directory).
    pub allow_write: Vec<PathBuf>,
}

impl SandboxSettings {
    /// Whether commands should be sandboxed under `mode`.
    pub fn active(&self, mode: Mode) -> bool {
        match self.mode {
            SandboxMode::Off => false,
            SandboxMode::Always => true,
            SandboxMode::Bypass => mode == Mode::Bypass,
        }
    }

    /// One-line state for /permissions.
    pub fn describe(&self) -> String {
        let scope = match self.mode {
            SandboxMode::Off => return "off".to_string(),
            SandboxMode::Bypass => "in bypass mode",
            SandboxMode::Always => "always",
        };
        let net = if self.allow_network {
            "network allowed"
        } else {
            "network blocked"
        };
        format!("bash runs sandboxed {scope} (writes confined to the project root and temp; {net})")
    }
}

/// What the bash tool needs to decide and build a (possibly sandboxed)
/// command: the settings plus the live permission mode.
#[derive(Clone)]
pub struct SandboxCtx {
    pub settings: SandboxSettings,
    pub mode: ModeHandle,
}

impl SandboxCtx {
    /// A never-sandboxing context, for user-authored commands
    /// (`!` direct shell, the after_edit hook).
    pub fn off() -> Self {
        Self {
            settings: SandboxSettings::default(),
            mode: ModeHandle::new(Mode::default()),
        }
    }
}

/// Build the shell command for `command`, sandboxed when the settings
/// call for it under the current permission mode.
pub fn shell_for(
    command: &str,
    root: &Path,
    ctx: &SandboxCtx,
) -> anyhow::Result<tokio::process::Command> {
    if !ctx.settings.active(ctx.mode.get()) {
        return Ok(crate::tools::shell_command(command));
    }
    sandboxed(command, root, &ctx.settings)
}

/// Write-allowed paths: project root, temp dirs, /dev, plus the
/// configured extras (with `~` expanded). Paths are canonicalized where
/// possible so symlinked roots (e.g. /tmp → /private/tmp) match.
///
/// Only the Seatbelt and Landlock builds have a sandbox to feed; elsewhere
/// this would be dead code and fail the `-D warnings` clippy run.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_paths(root: &Path, settings: &SandboxSettings) -> Vec<PathBuf> {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut paths = vec![
        canonical(root),
        canonical(&std::env::temp_dir()),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/dev"),
    ];
    for extra in &settings.allow_write {
        let expanded = match (extra.strip_prefix("~"), std::env::var_os("HOME")) {
            (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
            _ => extra.clone(),
        };
        paths.push(canonical(&expanded));
    }
    paths.sort();
    paths.dedup();
    paths.retain(|p| p.exists());
    paths
}

#[cfg(target_os = "macos")]
fn sandboxed(
    command: &str,
    root: &Path,
    settings: &SandboxSettings,
) -> anyhow::Result<tokio::process::Command> {
    // Seatbelt string literals: escape backslashes and quotes.
    let escape = |p: &Path| {
        p.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    };
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    for path in write_paths(root, settings) {
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            escape(&path)
        ));
    }
    if !settings.allow_network {
        profile.push_str("(deny network*)\n");
    }
    let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
    c.arg("-p").arg(profile).arg("sh").arg("-c").arg(command);
    Ok(c)
}

#[cfg(target_os = "linux")]
fn sandboxed(
    command: &str,
    root: &Path,
    settings: &SandboxSettings,
) -> anyhow::Result<tokio::process::Command> {
    use anyhow::Context as _;
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr,
    };

    // Filesystem confinement is a hard requirement: an unsupported kernel
    // fails the command instead of silently running unconfined. The TCP
    // block (ABI v4, kernel 6.7+) is best-effort on top.
    let fs_abi = ABI::V2;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(fs_abi))
        .context("Landlock is unavailable on this kernel (needs 5.13+)")?;
    if !settings.allow_network {
        ruleset = ruleset
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
            .context("Landlock network rules")?;
    }
    // The ruleset (and the O_PATH fds behind the rules) are built before
    // fork; the child only applies it, so pre_exec stays allocation-free.
    let mut created = ruleset.create().context("creating the Landlock ruleset")?;
    created = created
        .add_rule(PathBeneath::new(
            PathFd::new("/").context("opening /")?,
            AccessFs::from_read(fs_abi),
        ))
        .context("adding the read rule")?;
    for path in write_paths(root, settings) {
        created = created
            .add_rule(PathBeneath::new(
                PathFd::new(&path).with_context(|| format!("opening {}", path.display()))?,
                AccessFs::from_all(fs_abi),
            ))
            .with_context(|| format!("allowing writes under {}", path.display()))?;
    }

    let mut cell = Some(created);
    let mut c = crate::tools::shell_command(command);
    unsafe {
        c.pre_exec(move || {
            if let Some(ruleset) = cell.take() {
                ruleset
                    .restrict_self()
                    .map_err(|e| std::io::Error::other(format!("landlock: {e}")))?;
            }
            Ok(())
        });
    }
    Ok(c)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandboxed(
    _command: &str,
    _root: &Path,
    _settings: &SandboxSettings,
) -> anyhow::Result<tokio::process::Command> {
    anyhow::bail!(
        "sandboxing is not supported on this platform; set [sandbox] mode = \"off\" \
         in picocode.toml"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_follows_mode() {
        let mut s = SandboxSettings::default();
        assert!(!s.active(Mode::Bypass));
        s.mode = SandboxMode::Bypass;
        assert!(s.active(Mode::Bypass));
        assert!(!s.active(Mode::Edit));
        s.mode = SandboxMode::Always;
        assert!(s.active(Mode::ReadOnly));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sandbox_confines_writes() {
        let run = |cmd: String, root: std::path::PathBuf| async move {
            let settings = SandboxSettings {
                mode: SandboxMode::Always,
                ..Default::default()
            };
            let out = sandboxed(&cmd, &root, &settings)
                .unwrap()
                .current_dir(&root)
                .output()
                .await
                .unwrap();
            out.status.success()
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Writing inside the project root works …
        assert!(run("touch inside.txt".into(), root.clone()).await);
        // … while writing outside is blocked. The outside path must not be
        // under a temp dir the sandbox allows, so use the home directory.
        if let Some(home) = std::env::var_os("HOME") {
            let target = PathBuf::from(home).join(".picocode-sandbox-test");
            let cmd = format!("touch {}", target.display());
            let confined = !run(cmd, root.clone()).await;
            let _ = std::fs::remove_file(&target);
            assert!(confined, "write outside the sandbox succeeded");
        }
    }
}
