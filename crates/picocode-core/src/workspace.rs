//! Opening and switching the workspace the tools operate in.
//!
//! A workspace is a [`Config`] (where the root is, which rules apply) plus
//! the [`Backend`] the file tools go through. Both front ends open one at
//! startup and can swap it at runtime with `/remote`, so the connect →
//! re-resolve → hand back a fresh pair sequence lives here instead of
//! being written twice.

use crate::backend::{Backend, SshBackend};
use crate::config::{Args, Config, RemoteEntry, RemoteSpec};

/// Establish the backend for `cfg`'s workspace. Local is immediate; a
/// remote target opens the SSH connection and then applies the host's own
/// `picocode.toml` and instruction files over the local config. A remote
/// connection failure is an error — there is no workspace to operate on.
pub async fn connect(cfg: &mut Config) -> anyhow::Result<Backend> {
    let Some(spec) = cfg.remote.clone() else {
        return Ok(Backend::Local);
    };
    let ssh = SshBackend::connect(&spec.destination).await?;
    let backend = Backend::Ssh(std::sync::Arc::new(ssh));
    cfg.apply_workspace_settings(&backend).await?;
    Ok(backend)
}

/// Resolve a `/remote` argument: `local` (also `off`, `none`, or nothing)
/// means the local workspace, anything else is a `[[remotes]]` name or a
/// `host:/path` target.
pub fn parse_target(arg: &str, remotes: &[RemoteEntry]) -> anyhow::Result<Option<RemoteSpec>> {
    match arg.trim() {
        "" | "local" | "off" | "none" => Ok(None),
        spec => Ok(Some(RemoteSpec::parse(spec, remotes)?)),
    }
}

/// Open `target` as a fresh workspace: re-resolve the config from the
/// config files (the new root selects its own settings, like starting
/// there would) and connect the backend. Nothing in the caller changes
/// until this succeeds, so a failed switch leaves the current workspace
/// running.
pub async fn open(target: Option<&RemoteSpec>) -> anyhow::Result<(Config, Backend)> {
    let mut cfg = Config::from_args(Args::for_workspace(target.map(RemoteSpec::to_arg)))?;
    let backend = connect(&mut cfg).await?;
    // A mistyped remote path would otherwise only surface later, as every
    // tool failing at once.
    if backend.is_remote() && !backend.is_dir(&cfg.root).await {
        anyhow::bail!(
            "connected to {}, but `{}` is not a directory there",
            backend.label(),
            cfg.root.display()
        );
    }
    Ok((cfg, backend))
}

/// One row in a workspace listing: the local project, or a configured
/// `[[remotes]]` entry.
pub struct WorkspaceChoice {
    /// Argument that switches to this workspace (`local`, or the entry
    /// name) — what `/remote` takes.
    pub name: String,
    /// Display detail: the directory, or `host:path`.
    pub detail: String,
    pub active: bool,
}

/// Rows for the workspace pickers, shared by both front ends: the local
/// project first, then every configured remote.
pub fn workspace_choices(cfg: &Config) -> Vec<WorkspaceChoice> {
    let local_root = std::env::current_dir().unwrap_or_default();
    let mut out = vec![WorkspaceChoice {
        name: "local".to_string(),
        detail: crate::git::display_dir(if cfg.remote.is_none() {
            &cfg.root
        } else {
            &local_root
        }),
        active: cfg.remote.is_none(),
    }];
    for entry in &cfg.remotes {
        out.push(WorkspaceChoice {
            name: entry.name.clone(),
            detail: format!("{}:{}", entry.host, entry.path),
            active: cfg.remote.as_ref().is_some_and(|spec| {
                spec.destination == entry.host && spec.path == std::path::Path::new(&entry.path)
            }),
        });
    }
    out
}

/// A ready-to-paste `[[remotes]]` snippet for a remote entered in the
/// add-remote dialog, so a working connection can be made permanent in
/// picocode.toml.
pub fn toml_snippet(name: &str, host: &str, path: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    format!("[[remotes]]\nname = \"{name}\"\nhost = \"{host}\"\npath = \"{path}\"")
}

/// Host aliases from `~/.ssh/config`, offered as suggestions by the
/// add-remote dialogs.
pub fn ssh_hosts() -> Vec<String> {
    crate::backend::ssh_config_hosts()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_aliases_resolve_to_no_remote() {
        for arg in ["", "  ", "local", "off", "none"] {
            assert!(parse_target(arg, &[]).unwrap().is_none(), "{arg}");
        }
    }

    #[test]
    fn named_entries_and_host_paths_resolve() {
        let remotes = vec![RemoteEntry {
            name: "box".into(),
            host: "user@example".into(),
            path: "/srv/app".into(),
        }];
        let spec = parse_target("box", &remotes).unwrap().unwrap();
        assert_eq!(spec.destination, "user@example");
        assert_eq!(spec.to_arg(), "user@example:/srv/app");
        let spec = parse_target("host:/tmp/x", &remotes).unwrap().unwrap();
        assert_eq!(spec.destination, "host");
        assert!(parse_target("no-colon", &remotes).is_err());
    }

    /// Opening a live remote workspace: the host's own `picocode.toml` and
    /// instruction files win over the local config. Point
    /// `PICOCODE_SSH_TEST` at an ssh destination and `PICOCODE_SSH_PATH` at
    /// a directory on it holding the picocode.toml written by the test:
    /// `PICOCODE_SSH_TEST=pctest PICOCODE_SSH_PATH=/tmp/hostproj \
    ///  cargo test -p picocode-core opens_a_remote -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a live ssh host in PICOCODE_SSH_TEST"]
    async fn opens_a_remote_workspace_with_the_hosts_settings() {
        let dest = std::env::var("PICOCODE_SSH_TEST").expect("set PICOCODE_SSH_TEST");
        let path = std::env::var("PICOCODE_SSH_PATH").expect("set PICOCODE_SSH_PATH");
        let spec = parse_target(&format!("{dest}:{path}"), &[])
            .unwrap()
            .unwrap();

        let (cfg, backend) = open(Some(&spec)).await.expect("open");
        assert!(backend.is_remote(), "backend should be the ssh one");
        assert_eq!(cfg.root, spec.path);
        // From the host's picocode.toml, not the local one.
        assert_eq!(cfg.bash_timeout.get(), 333);
        assert!(
            cfg.approval.snapshot().allow_bash.contains(&"uname".into()),
            "host approval rules should apply"
        );
        assert_eq!(cfg.instruction_names, vec!["HOST.md".to_string()]);
        let (name, body) = cfg.instructions.first().expect("host instructions loaded");
        assert_eq!(name, "HOST.md");
        assert!(body.contains("BANANAPHONE"), "{body}");
        // Model entries stay local — the host's roster is ignored.
        assert!(!cfg.models.iter().any(|m| m.name == "should-be-ignored"));
    }
}
