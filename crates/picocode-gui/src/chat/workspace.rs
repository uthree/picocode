//! Workspace switching: the workspace menu's folder picker, the
//! add-remote dialog, and the full respawn onto another (possibly remote)
//! workspace.

use gpui::prelude::*;
use gpui::{Context, Entity, Window};
use gpui_component::input::InputState;
use rust_i18n::t;

use std::path::PathBuf;

use picocode_core::config::{self, Config};
use picocode_core::transcript::EntryKind;
use picocode_core::{agent, session};

use super::ChatView;

/// State of the add-remote dialog (opened from the workspace menu): an
/// SSH destination and a path on it, tried live. `~/.ssh/config` host
/// aliases are listed to click.
pub(super) struct AddRemote {
    /// Entry name — only used for the picocode.toml snippet.
    pub(super) name: Entity<InputState>,
    /// ssh destination: an alias or `user@host`.
    pub(super) host: Entity<InputState>,
    /// Working directory on the host.
    pub(super) path: Entity<InputState>,
    /// Host aliases read from `~/.ssh/config`.
    pub(super) hosts: Vec<String>,
    /// Status line: hint, progress, or error.
    pub(super) note: String,
}

impl ChatView {
    /// The workdir line's label: a remote workspace shows `host:path` so
    /// the target is unmistakable, local shows the shortened directory.
    pub(super) fn workdir_label(&self) -> String {
        match &self.cfg.remote {
            Some(spec) => format!("{}:{}", self.backend.label(), spec.path.display()),
            None => picocode_core::git::display_dir(&self.cfg.root),
        }
    }

    /// The workspace menu's "choose a folder" row: open a native directory
    /// picker and move the project root there (always a local project — a
    /// remote root is picked from the `[[remotes]]` rows instead, since
    /// there is no native picker for a host's filesystem).
    pub(super) fn pick_workdir(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if self.running {
            self.push(EntryKind::Error, t!("cd_while_running").to_string());
            cx.notify();
            return;
        }
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(root) = paths.pop()
            {
                let _ = this.update(cx, |view, cx| view.change_workdir(root, cx));
            }
        })
        .detach();
    }

    /// Switch the project root: rebuild the config for the new directory
    /// (its picocode.toml, instructions, saved state), spawn a fresh worker
    /// there, and start a new conversation. The current model is kept when
    /// the new project doesn't select one of its own.
    fn change_workdir(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.running {
            self.push(EntryKind::Error, t!("cd_while_running").to_string());
            cx.notify();
            return;
        }
        if root == self.cfg.root {
            cx.notify();
            return;
        }
        if let Err(e) = std::env::set_current_dir(&root) {
            self.push(
                EntryKind::Error,
                t!("cd_failed", error = format!("{e:#}")).to_string(),
            );
            cx.notify();
            return;
        }
        // Changing the working directory always opens a local project;
        // remote workspaces are entered with `/remote` (or --remote).
        let new_cfg = match config::Config::from_args(config::Args::for_workspace(None)) {
            Ok(cfg) => cfg,
            Err(e) => {
                self.push(
                    EntryKind::Error,
                    t!("cd_failed", error = format!("{e:#}")).to_string(),
                );
                cx.notify();
                return;
            }
        };
        // A picked directory is always a local project, so the workspace
        // drops back to the local backend even if a remote one was open.
        self.apply_workspace(new_cfg, picocode_core::backend::Backend::Local, cx);
    }

    /// `/remote <target>`: open another workspace by name (a `[[remotes]]`
    /// entry, `host:/path`, or `local`).
    pub(super) fn switch_workspace(&mut self, target: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if self.running {
            self.push(EntryKind::Error, t!("remote_while_running").to_string());
            return;
        }
        match picocode_core::workspace::parse_target(target, &self.cfg.remotes) {
            Ok(spec) => self.open_target(spec, None, cx),
            Err(e) => self.push(
                EntryKind::Error,
                t!("remote_failed", error = format!("{e:#}")).to_string(),
            ),
        }
    }

    /// Open the workspace `spec` describes (None = local). Connecting can
    /// block for a while (SSH auth, ProxyJump), so it runs on the tokio
    /// runtime and the swap happens back on the UI thread once it
    /// succeeds — a failed connection leaves the current workspace
    /// untouched. `snippet` is the picocode.toml block printed after a
    /// successful connection from the add-remote dialog.
    fn open_target(
        &mut self,
        spec: Option<picocode_core::config::RemoteSpec>,
        snippet: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if spec.as_ref().map(|s| s.to_arg()) == self.cfg.remote.as_ref().map(|s| s.to_arg()) {
            self.push(EntryKind::Notice, t!("remote_same").to_string());
            return;
        }
        let label = match &spec {
            Some(spec) => spec.to_arg(),
            None => t!("remote_local").to_string(),
        };
        self.push(
            EntryKind::Notice,
            t!("remote_opening", target = label).to_string(),
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.rt.spawn(async move {
            let opened = picocode_core::workspace::open(spec.as_ref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(opened);
        });
        cx.spawn(async move |this, cx| {
            let Ok(opened) = rx.await else { return };
            let _ = this.update(cx, |view, cx| {
                match opened {
                    Ok((cfg, backend)) => {
                        view.apply_workspace(cfg, backend, cx);
                        if let Some(snippet) = snippet {
                            view.push(
                                EntryKind::Notice,
                                format!("{}\n{snippet}", t!("remote_snippet_hint")),
                            );
                        }
                    }
                    Err(error) => {
                        view.push(
                            EntryKind::Error,
                            t!("remote_failed", error = error).to_string(),
                        );
                    }
                }
                view.scroll_to_bottom();
                cx.notify();
            });
        })
        .detach();
    }

    /// The workspace menu's "+ add a remote…" row: open the dialog with
    /// the `~/.ssh/config` aliases ready to click.
    pub(super) fn open_add_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let field = |placeholder: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = field(t!("add_remote_name_placeholder").to_string(), window, cx);
        let host = field(t!("add_remote_host_placeholder").to_string(), window, cx);
        let path = field(t!("add_remote_path_placeholder").to_string(), window, cx);
        host.update(cx, |state, cx| state.focus(window, cx));
        let hosts = picocode_core::workspace::ssh_hosts();
        let note = if hosts.is_empty() {
            t!("add_remote_hint").to_string()
        } else {
            t!("add_remote_hosts_found", count = hosts.len()).to_string()
        };
        self.add_remote = Some(AddRemote {
            name,
            host,
            path,
            hosts,
            note,
        });
        cx.notify();
    }

    /// A host row in the add-remote dialog: fill the destination field.
    pub(super) fn add_remote_pick_host(
        &mut self,
        host: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dlg) = &self.add_remote {
            let field = dlg.host.clone();
            field.update(cx, |state, cx| state.set_value(host, window, cx));
            cx.notify();
        }
    }

    /// The add-remote dialog's connect action: try the destination and
    /// path, and on success print a picocode.toml snippet to keep it.
    pub(super) fn add_remote_connect(&mut self, cx: &mut Context<Self>) {
        let Some(dlg) = &self.add_remote else {
            return;
        };
        let host = dlg.host.read(cx).value().trim().to_string();
        let path = dlg.path.read(cx).value().trim().to_string();
        let name = dlg.name.read(cx).value().trim().to_string();
        if host.is_empty() || path.is_empty() {
            if let Some(dlg) = &mut self.add_remote {
                dlg.note = t!("add_remote_need_fields").to_string();
            }
            cx.notify();
            return;
        }
        let spec = picocode_core::config::RemoteSpec {
            destination: host.clone(),
            path: path.clone().into(),
        };
        let snippet = picocode_core::workspace::toml_snippet(
            if name.is_empty() { &host } else { &name },
            &host,
            &path,
        );
        self.add_remote = None;
        if self.running {
            self.push(EntryKind::Error, t!("remote_while_running").to_string());
            cx.notify();
            return;
        }
        self.open_target(Some(spec), Some(snippet), cx);
    }

    /// Adopt a freshly opened workspace: respawn the worker on its backend
    /// and start a new conversation there (a different workspace gets its
    /// own session log, like launching picocode in it).
    fn apply_workspace(
        &mut self,
        mut new_cfg: Config,
        backend: picocode_core::backend::Backend,
        cx: &mut Context<Self>,
    ) {
        // The new workspace selects no model of its own: keep the current one.
        if new_cfg.model.is_empty() {
            new_cfg.provider = self.cfg.provider;
            new_cfg.model = self.cfg.model.clone();
            new_cfg.base_url = self.cfg.base_url.clone();
            new_cfg.active_model = None;
            new_cfg.context_window = self.cfg.context_window.clone();
        }
        // Keep the current permission mode and the persisted /config values.
        new_cfg.mode.set(self.cfg.mode.get());
        // The goal belonged to the conversation being left behind, and the
        // fresh worker starts without one.
        self.goal = None;
        self.goal_round = 0;
        // The send key belongs to the person at the keyboard, not to the
        // project — keep it across the switch (no rebinding needed).
        new_cfg.submit_key = self.cfg.submit_key;
        Self::apply_saved(&self.saved, &mut new_cfg);

        let (new_tx, new_steer) = {
            let _guard = self.rt.enter();
            match agent::spawn(
                &new_cfg,
                self.event_tx.clone(),
                self.cancel_tx.subscribe(),
                self.jobs.clone(),
                self.mcp.clone(),
                backend.clone(),
            ) {
                Ok(pair) => pair,
                Err(e) => {
                    self.push(
                        EntryKind::Error,
                        t!("remote_failed", error = format!("{e:#}")).to_string(),
                    );
                    return;
                }
            }
        };
        self.steer = new_steer;
        // Dropping the old sender shuts the old worker down.
        self.cmd_tx = new_tx;
        self.cfg = new_cfg;
        self.backend = backend;
        self.sessions_dir = session::sessions_dir_for(&self.cfg);
        // Sessions are per project: the sidebar now lists the new one's.
        self.refresh_sessions();
        self.git_branch = picocode_core::git::branch(&self.cfg.root);
        self.session_id = session::new_id();
        self.entries.clear();
        self.queued.clear();
        self.pending_attachments.clear();
        self.expanded_reasoning.clear();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.est_out = 0;
        self.available_models.clear();
        self.refresh_models();
        self.probe_context_limit();
        // A remote workspace names the host; a local one reads as the
        // familiar directory change.
        let notice = if self.backend.is_remote() {
            t!(
                "remote_switched",
                host = self.backend.label(),
                dir = self.cfg.root.display().to_string(),
                model = self.cfg.model_label()
            )
        } else {
            t!(
                "workdir_changed",
                dir = picocode_core::git::display_dir(&self.cfg.root),
                model = self.cfg.model_label()
            )
        };
        self.push(EntryKind::Notice, notice.to_string());
        self.reset_list();
        picocode_core::state::save_last_model(&self.cfg);
        cx.notify();
    }
}
