//! Workspace switching: the `/remote` picker, the add-remote form, and the
//! full respawn onto another (possibly remote) workspace.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use picocode_core::session;

use super::{AddRemoteForm, App, EntryKind, LOGO, RemotePicker};

/// A workspace connection that ran on the tokio runtime, on its way back to
/// the event loop. Carrying the label and snippet along keeps
/// [`App::apply_opened`] free of state that would have to survive the wait.
pub(super) struct Opened {
    /// What the user asked to open, for the success and failure notices.
    pub label: String,
    /// picocode.toml block to print after a successful connection made from
    /// the add-remote form.
    pub snippet: Option<String>,
    pub result: anyhow::Result<(
        picocode_core::config::Config,
        picocode_core::backend::Backend,
    )>,
}

impl App {
    /// `/remote` with no argument: open the workspace dialog.
    pub(super) fn open_remote_picker(&mut self) {
        self.remote_picker = Some(RemotePicker {
            items: picocode_core::workspace::workspace_choices(&self.cfg),
            selected: 0,
        });
    }

    /// Open the add-remote form (the `/remote` dialog's "+ add" row) with
    /// the host aliases from `~/.ssh/config` ready to pick.
    pub(super) fn open_add_remote(&mut self) {
        let hosts = picocode_core::workspace::ssh_hosts();
        let note = if hosts.is_empty() {
            "host: an ssh alias or user@host — authentication is your ssh setup".to_string()
        } else {
            format!(
                "{} host(s) from ~/.ssh/config below — ↓ to pick one",
                hosts.len()
            )
        };
        self.add_remote = Some(AddRemoteForm {
            name: String::new(),
            host: String::new(),
            path: String::new(),
            field: 1,
            hosts,
            note,
        });
    }

    /// Key handling for the add-remote form. Enter on a suggestion fills
    /// the host field; Enter on a form row connects.
    pub(super) async fn add_remote_key(&mut self, key: KeyEvent) {
        let Some(form) = &mut self.add_remote else {
            return;
        };
        let rows = 3 + form.hosts.len();
        match key.code {
            KeyCode::Esc => {
                self.add_remote = None;
                self.open_remote_picker();
            }
            KeyCode::Up => form.field = (form.field + rows - 1) % rows,
            KeyCode::Down | KeyCode::Tab => form.field = (form.field + 1) % rows,
            KeyCode::Char(c) if form.field == 0 => form.name.push(c),
            KeyCode::Char(c) if form.field == 1 => form.host.push(c),
            KeyCode::Char(c) if form.field == 2 => form.path.push(c),
            KeyCode::Backspace if form.field == 0 => {
                form.name.pop();
            }
            KeyCode::Backspace if form.field == 1 => {
                form.host.pop();
            }
            KeyCode::Backspace if form.field == 2 => {
                form.path.pop();
            }
            // A suggestion row fills the host and moves on to the path.
            // `hosts` is fixed once the form opens, so the index is in range
            // — `get` keeps it that way if that ever stops being true.
            KeyCode::Enter if form.field >= 3 => {
                if let Some(host) = form.hosts.get(form.field - 3) {
                    form.host = host.clone();
                }
                form.field = 2;
            }
            KeyCode::Enter => {
                let (host, path) = (form.host.trim().to_string(), form.path.trim().to_string());
                if host.is_empty() || path.is_empty() {
                    form.note = "host and path are both required".to_string();
                    return;
                }
                let name = form.name.trim().to_string();
                form.note = format!("connecting to {host}…");
                let target = format!("{host}:{path}");
                self.add_remote = None;
                let name = if name.is_empty() { &host } else { &name };
                let snippet = format!(
                    "To keep this remote, add it to picocode.toml:\n{}",
                    picocode_core::workspace::toml_snippet(name, &host, &path)
                );
                self.switch_workspace(&target, Some(snippet));
            }
            _ => {}
        }
    }

    /// `/remote <target>`: open another workspace. The target's config is
    /// re-resolved from scratch (a remote one adds the host's own
    /// picocode.toml and instruction files) and the worker respawns on the
    /// new backend. The conversation does not travel along — a different
    /// workspace starts a fresh session log, like launching picocode there.
    ///
    /// This only *starts* the connection; [`App::apply_opened`] finishes it
    /// when the runtime reports back. Nothing changes until then, so a
    /// failed switch leaves the current workspace running.
    pub(super) fn switch_workspace(&mut self, target: &str, snippet: Option<String>) {
        use picocode_core::workspace;

        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot change the workspace while a turn is running".to_string(),
            );
            return;
        }
        if self.connecting {
            self.push(
                EntryKind::Error,
                "Already opening a workspace — wait for that to finish".to_string(),
            );
            return;
        }
        let spec = match workspace::parse_target(target, &self.cfg.remotes) {
            Ok(spec) => spec,
            Err(e) => {
                self.push(EntryKind::Error, format!("{e:#}"));
                return;
            }
        };
        if spec.as_ref().map(|s| s.to_arg()) == self.cfg.remote.as_ref().map(|s| s.to_arg()) {
            self.push(EntryKind::Notice, "Already on that workspace".to_string());
            return;
        }
        let label = match &spec {
            Some(spec) => spec.to_arg(),
            None => "the local workspace".to_string(),
        };
        self.push(EntryKind::Notice, format!("Opening {label}…"));

        // Connecting can block for a long time — SSH auth, a ProxyJump, an
        // unreachable host — so it runs on the tokio runtime and reports
        // back through `open_rx`. Awaiting it here would freeze the event
        // loop: no redraw, no keys, no way out.
        self.connecting = true;
        let tx = self.open_tx.clone();
        tokio::spawn(async move {
            let result = workspace::open(spec.as_ref()).await;
            let _ = tx
                .send(Opened {
                    label,
                    snippet,
                    result,
                })
                .await;
        });
    }

    /// Finish a switch started by [`App::switch_workspace`]. Nothing changed
    /// until now, so a failed connection leaves the current workspace
    /// running.
    pub(super) fn apply_opened(&mut self, opened: Opened) {
        let Opened {
            label,
            snippet,
            result,
        } = opened;
        self.connecting = false;
        let (mut new_cfg, backend) = match result {
            Ok(pair) => pair,
            Err(e) => {
                self.push(EntryKind::Error, format!("Failed to open {label}: {e:#}"));
                return;
            }
        };
        // The new workspace selects no model of its own: keep the current one.
        if new_cfg.model.is_empty() {
            new_cfg.provider = self.cfg.provider;
            new_cfg.model = self.cfg.model.clone();
            new_cfg.base_url = self.cfg.base_url.clone();
            new_cfg.active_model = None;
            new_cfg.context_window = self.cfg.context_window.clone();
        }
        new_cfg.mode.set(self.cfg.mode.get());
        // The goal belonged to the conversation being left behind, and the
        // fresh worker starts without one.
        self.goal = None;
        self.goal_round = 0;

        let (new_tx, new_steer) = match picocode_core::agent::spawn(
            &new_cfg,
            self.event_tx.clone(),
            self.cancel_tx.subscribe(),
            self.jobs.clone(),
            self.mcp.clone(),
            backend.clone(),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                self.push(EntryKind::Error, format!("Failed to open {label}: {e:#}"));
                return;
            }
        };
        // Dropping the old sender shuts the old worker down.
        self.cmd_tx = new_tx;
        self.steer = new_steer;
        self.cfg = new_cfg;
        self.backend = backend;
        self.model_label = self.cfg.model_label();
        self.sessions_dir = session::sessions_dir_for(&self.cfg);
        self.session_id = session::new_id();
        self.git_branch = picocode_core::git::branch(&self.cfg.root);
        self.entries.clear();
        self.attachments.clear();
        self.available_models.clear();
        self.auto_compact_tried = false;
        self.context_info = None;
        self.ctx_tokens = 0;
        self.turn_out = 0;
        self.total_out = 0;
        self.delta_est = 0;
        self.assistant_open = false;
        self.reasoning_open = false;
        self.follow = true;
        self.top_line = 0;
        self.push(EntryKind::Logo, LOGO.to_string());
        self.push(
            EntryKind::Notice,
            format!(
                "Workspace: {} — {} ({})",
                self.backend.label(),
                self.cfg.root.display(),
                self.model_label
            ),
        );
        if !self.cfg.instructions.is_empty() {
            let names: Vec<&str> = self
                .cfg
                .instructions
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();
            self.push(EntryKind::Notice, format!("Loaded {}", names.join(", ")));
        }
        self.refresh_models();
        self.probe_context_limit();
        // A connection made from the add-remote form is worth keeping.
        if let Some(snippet) = snippet {
            self.push(EntryKind::Notice, snippet);
        }
    }
}
