//! Dialog state (approval, question, pickers, forms) and the small
//! informational commands: `/config`, `/status`, `/permissions`, `/jobs`.

use tokio::sync::oneshot;

use picocode_core::models::ModelChoice;
use picocode_core::session;

use super::{App, EntryKind};

pub struct PendingApproval {
    pub name: String,
    pub args: String,
    /// What the `a` (always) answer whitelists.
    pub always: AlwaysAllow,
    pub(super) respond: oneshot::Sender<bool>,
}

/// The allow rule the approval dialog's "always" answer adds.
pub enum AlwaysAllow {
    /// Add the tool to `allow_tools`.
    Tool(String),
    /// Add these prefix patterns to `allow_bash`.
    Bash(Vec<String>),
}

impl AlwaysAllow {
    /// The addition, spelled as it would appear in picocode.toml (shown in
    /// the notice after `a`, ready to copy into `[approval]`).
    pub fn label(&self) -> String {
        match self {
            AlwaysAllow::Tool(name) => format!("allow_tools += {name}"),
            AlwaysAllow::Bash(patterns) => format!("allow_bash += {}", patterns.join(", ")),
        }
    }

    /// Plain-language preview of what `a` whitelists (shown in the dialog).
    pub fn describe(&self) -> String {
        match self {
            AlwaysAllow::Tool(name) => {
                format!("a: don't ask again for {name} this session")
            }
            AlwaysAllow::Bash(patterns) => {
                let list = patterns
                    .iter()
                    .map(|p| format!("\"{p} …\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("a: don't ask again for {list} commands this session")
            }
        }
    }
}

/// State of the `/resume` selection dialog.
pub struct SessionPicker {
    pub sessions: Vec<session::SessionSummary>,
    pub selected: usize,
}

/// State of the `/model` selection dialog. The list is followed by a
/// synthetic "+ add a provider / model…" row opening [`AddModelForm`].
/// Typing filters the list live.
pub struct ModelPicker {
    pub items: Vec<ModelChoice>,
    /// Selected index within the *filtered* view (the add row sits at
    /// `filtered().len()`).
    pub selected: usize,
    /// Live search text; matches name or detail, case-insensitive.
    pub filter: String,
}

impl ModelPicker {
    /// The rows the dialog shows: items matching the filter.
    pub fn filtered(&self) -> Vec<&ModelChoice> {
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|c| {
                needle.is_empty()
                    || c.name.to_lowercase().contains(&needle)
                    || c.detail.to_lowercase().contains(&needle)
            })
            .collect()
    }
}

/// State of the `/remote` workspace dialog: the local project and the
/// configured `[[remotes]]`, followed by a synthetic "+ add a remote…"
/// row opening [`AddRemoteForm`].
pub struct RemotePicker {
    pub items: Vec<picocode_core::workspace::WorkspaceChoice>,
    /// Selected index; the add row sits at `items.len()`.
    pub selected: usize,
}

/// State of the add-remote form (opened from the `/remote` dialog): an
/// SSH destination and a path on it, tried live. `~/.ssh/config` host
/// aliases are listed below the fields so a configured host is one
/// keypress away.
pub struct AddRemoteForm {
    /// Entry name — only used for the picocode.toml snippet.
    pub name: String,
    /// ssh destination: an alias or `user@host`.
    pub host: String,
    /// Working directory on the host.
    pub path: String,
    /// Focused row: 0 name, 1 host, 2 path, 3.. the host suggestions.
    pub field: usize,
    /// Host aliases read from `~/.ssh/config`.
    pub hosts: Vec<String>,
    /// One-line status under the form: hint, progress, or error.
    pub note: String,
}

/// State of the add-model form (opened from the `/model` dialog): pick a
/// provider, optionally point it at a base URL, and type or pick a model.
pub struct AddModelForm {
    pub provider: picocode_core::config::Provider,
    /// Endpoint override; empty uses the provider default.
    pub base_url: String,
    pub model: String,
    /// Focused row: 0 provider, 1 base URL, 2 model, 3.. the fetched list.
    pub field: usize,
    /// Models the probed endpoint reported serving (Tab fetches).
    pub fetched: Vec<String>,
    /// One-line status under the form: key hint, fetch progress, or error.
    pub note: String,
}

impl AddModelForm {
    /// The base URL as the switch/probe wants it (None = provider default).
    pub fn base(&self) -> Option<String> {
        let trimmed = self.base_url.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

/// State of the `/config` settings dialog. The rows are fixed; the values
/// are read live from the app so external changes (Shift+Tab, Ctrl+T) show
/// up while the dialog is open.
pub struct SettingsMenu {
    pub selected: usize,
}

/// Number of rows in the `/config` dialog (mode, send key, reasoning, bash
/// timeout, read limits, web search provider/results, auto-compact, model).
pub const SETTINGS_ROWS: usize = 11;

/// State of the `submit_plan` approval (question) dialog.
pub struct PendingQuestion {
    pub title: String,
    pub question: String,
    pub options: Vec<String>,
    pub selected: usize,
    pub(super) respond: oneshot::Sender<Option<usize>>,
}

impl App {
    /// `/jobs`: list running background jobs with their ids.
    pub(super) fn show_jobs(&mut self) {
        let jobs = self.jobs.list();
        if jobs.is_empty() {
            self.push(EntryKind::Notice, "No background jobs running".to_string());
            return;
        }
        let mut text = String::from("Background jobs (/jobs kill <id> stops one):");
        for (id, command, elapsed) in jobs {
            text.push_str(&format!(
                "
#{id}  {}s  {command}",
                elapsed.as_secs()
            ));
        }
        self.push(EntryKind::Notice, text);
    }

    /// `/jobs kill <id>`: stop a background job (its wrapper still reports
    /// a completion, which balances the status-bar counter).
    pub(super) fn kill_job(&mut self, id: u64) {
        if self.jobs.kill(id) {
            self.push(EntryKind::Notice, format!("Killed background job #{id}"));
        } else {
            self.push(EntryKind::Error, format!("No background job #{id}"));
        }
    }

    /// `/permissions`: show what the current mode and config rules do.
    /// `/trust` and `/trust revoke`. The gated settings are read at startup,
    /// so allowing them takes effect on the next run rather than now — say
    /// so instead of pretending otherwise.
    pub(super) fn run_trust(&mut self, allow: bool) {
        use picocode_core::config::trust::Outcome;
        let path = self.cfg.project_config.clone();
        let (kind, text) = match picocode_core::config::trust::apply(&path, allow) {
            Ok(Outcome::Trusted(settings)) => (
                EntryKind::Notice,
                format!(
                    "Trusted {} — {} apply from the next start of picocode.",
                    path.display(),
                    settings.join(", ")
                ),
            ),
            Ok(Outcome::TrustedNothingGated) => (
                EntryKind::Notice,
                format!(
                    "Trusted {} — it asks for nothing that was being held back.",
                    path.display()
                ),
            ),
            Ok(Outcome::NoConfig) => (
                EntryKind::Notice,
                format!("No project config to trust ({})", path.display()),
            ),
            Ok(Outcome::Revoked) => (
                EntryKind::Notice,
                format!(
                    "No longer trusting {} — its commands and approval rules stop \
                     applying at the next start.",
                    path.display()
                ),
            ),
            Ok(Outcome::WasNotTrusted) => (
                EntryKind::Notice,
                format!("{} was not trusted", path.display()),
            ),
            Err(e) => (EntryKind::Error, format!("Could not update trust: {e:#}")),
        };
        self.push(kind, text);
    }

    pub(super) fn show_permissions(&mut self) {
        let text = format!(
            "{} `!` commands are typed by you and skip all rules.",
            picocode_core::report::permissions_text(&self.cfg)
        );
        self.push(EntryKind::Notice, text);
    }

    /// Rows of the `/config` dialog: (name, current value, key hint). Values
    /// are rebuilt every frame so concurrent changes (Shift+Tab, Ctrl+T)
    /// stay in sync while the dialog is open.
    pub fn settings_rows(&self) -> [(&'static str, String, &'static str); SETTINGS_ROWS] {
        [
            ("mode", self.cfg.mode.get().label().to_string(), "← →"),
            ("send key", self.send_key_label(), "← →"),
            (
                "reasoning",
                if self.show_reasoning {
                    "shown".to_string()
                } else {
                    "collapsed".to_string()
                },
                "← →",
            ),
            (
                "bash timeout",
                format!("{}s", self.cfg.bash_timeout.get()),
                "← →",
            ),
            (
                "read lines",
                self.cfg.read_max_lines.get().to_string(),
                "← →",
            ),
            (
                "line bytes",
                self.cfg.read_max_line_bytes.get().to_string(),
                "← →",
            ),
            (
                "web search",
                self.cfg.search.snapshot().provider.label().to_string(),
                "← →",
            ),
            (
                "results",
                self.cfg.search.snapshot().max_results.to_string(),
                "← →",
            ),
            ("max tokens", self.cfg.max_tokens_label(), "← →"),
            (
                "auto-compact",
                match self.cfg.auto_compact.get() {
                    0 => "off".to_string(),
                    pct => format!("{pct}%"),
                },
                "← →",
            ),
            ("model", self.model_label.clone(), "Enter"),
        ]
    }

    /// The `/config` send-key value: the key itself, plus what the terminal
    /// actually does when it cannot report that combination.
    fn send_key_label(&self) -> String {
        let key = self.cfg.submit_key;
        if !self.enhanced_keys && super::needs_enhanced_keys(key) {
            format!("{} (terminal sends on Enter)", key.label())
        } else {
            key.label().to_string()
        }
    }

    /// ←/→ on a `/config` row: change the value in place. Every change
    /// applies immediately.
    pub(super) fn adjust_setting(&mut self, delta: i64) {
        let Some(menu) = &self.settings else { return };
        match menu.selected {
            // Same cycle as Shift+Tab; bypass stays /bypass-only, and
            // adjusting away from it lands on read-only.
            0 => self.cfg.mode.set(self.cfg.mode.get().cycled(delta)),
            // Session-only, like every other row here; `submit_key` in
            // picocode.toml makes it stick.
            1 => self.cfg.submit_key = self.cfg.submit_key.cycled(delta),
            2 => self.show_reasoning = !self.show_reasoning,
            3 => self.cfg.step_bash_timeout(delta),
            4 => self.cfg.step_read_lines(delta),
            5 => self.cfg.step_line_bytes(delta),
            6 => self.cfg.search.cycle_provider(delta),
            7 => self.cfg.search.step_max_results(delta),
            8 => self.cfg.step_max_tokens(delta),
            9 => self.cfg.step_auto_compact(delta),
            _ => {}
        }
    }

    /// Enter/Space on a `/config` row: toggles act like →; the model row
    /// closes the dialog and opens the `/model` picker.
    pub(super) fn activate_setting(&mut self) {
        let Some(menu) = &self.settings else { return };
        if menu.selected == SETTINGS_ROWS - 1 {
            self.settings = None;
            self.open_model_picker();
        } else {
            self.adjust_setting(1);
        }
    }

    /// `/status` (alias `/usage`): one-shot overview of the model, token
    /// usage, permission mode and session.
    pub(super) fn show_status(&mut self) {
        let prompts = self
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::User)
            .count();
        let text = picocode_core::report::status_text(
            &self.cfg,
            &picocode_core::report::StatusInfo {
                model_label: &self.model_label,
                ctx_tokens: self.ctx_tokens,
                output: format!(
                    "{} tokens this turn · {} this conversation",
                    self.turn_out + self.delta_est,
                    self.total_out + self.delta_est
                ),
                session_id: &self.session_id,
                prompts,
                sessions_dir: self.sessions_dir.as_deref(),
                mcp: (!self.mcp.is_empty()).then(|| self.mcp.summary()),
            },
        );
        self.push(EntryKind::Notice, text);
        // The colored context-composition block. Before the first turn no
        // worker report exists yet — estimate from the config alone.
        let breakdown = self
            .context_info
            .clone()
            .unwrap_or_else(|| picocode_core::context::breakdown(&self.cfg, &[], 0));
        self.push(EntryKind::Context, breakdown.encode());
    }

    /// Close the question dialog: `Some(selected)` on Enter, `None` on Esc.
    /// The tool call turns the answer into the tool result for the model.
    pub(super) fn resolve_question(&mut self, accept: bool) {
        if let Some(q) = self.question.take() {
            let _ = q.respond.send(accept.then_some(q.selected));
        }
    }

    pub(super) fn resolve_approval(&mut self, approve: bool) {
        if let Some(p) = self.pending.take() {
            let label = if approve {
                "✔ approved"
            } else {
                "✘ denied"
            };
            self.push(EntryKind::Notice, format!("{label}: {}", p.name));
            let _ = p.respond.send(approve);
        }
    }

    /// `a` in the approval dialog: approve the call and whitelist similar
    /// ones for the rest of the session.
    pub(super) fn resolve_approval_always(&mut self) {
        if let Some(p) = self.pending.take() {
            match &p.always {
                AlwaysAllow::Tool(name) => self.cfg.approval.allow_tool(name),
                AlwaysAllow::Bash(patterns) => self.cfg.approval.allow_bash(patterns),
            }
            self.push(
                EntryKind::Notice,
                format!(
                    "✔ approved: {} — {} for this session (put it in picocode.toml \
                     [approval] to keep it)",
                    p.name,
                    p.always.label()
                ),
            );
            let _ = p.respond.send(true);
        }
    }
}
