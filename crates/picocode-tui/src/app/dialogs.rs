//! Dialog state (approval, question, pickers, forms) and the small
//! informational commands: `/config`, `/status`, `/permissions`, `/jobs`.

use rust_i18n::t;
use tokio::sync::oneshot;

use picocode_core::config::{Group, SettingId};
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
    /// Index into [`App::settings_order`] — the selectable rows only, so
    /// the section headings never take the cursor.
    pub selected: usize,
}

/// One `/config` row the TUI can put the cursor on: the shared table, plus
/// the reasoning toggle, which only the TUI has (the GUI collapses
/// reasoning per entry instead), and the raw-transcript toggle, which both
/// front ends have but neither keeps in [`picocode_core::config::Config`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Setting {
    Shared(SettingId),
    Reasoning,
    RawView,
}

/// Where the reasoning toggle is remembered between runs.
pub(super) const REASONING_KEY: &str = "tui_reasoning";

impl Setting {
    pub fn group(self) -> Group {
        match self {
            Setting::Shared(id) => id.group(),
            Setting::Reasoning | Setting::RawView => Group::Interface,
        }
    }

    pub fn label(self) -> String {
        match self {
            Setting::Shared(id) => id.label(),
            Setting::Reasoning => t!("set_reasoning").to_string(),
            Setting::RawView => picocode_core::config::raw_view_label(),
        }
    }

    pub fn is_action(self) -> bool {
        matches!(self, Setting::Shared(id) if id.is_action())
    }
}

/// A line of the `/config` dialog as rendered: a section heading, or a
/// setting with its current value.
pub enum SettingsRow {
    Header(String),
    Setting {
        name: String,
        value: String,
        hint: &'static str,
    },
}

/// Every selectable `/config` row, in display order: the shared table plus
/// the TUI's own view toggles, grouped by section.
fn settings_order() -> Vec<Setting> {
    let mut order = Vec::new();
    for group in Group::ALL {
        order.extend(
            SettingId::SHARED
                .into_iter()
                .filter(|id| id.group() == group)
                .map(Setting::Shared),
        );
        if group == Setting::Reasoning.group() {
            order.push(Setting::Reasoning);
            order.push(Setting::RawView);
        }
    }
    order
}

/// [`settings_order`] with a heading in front of each section. Headings are
/// not selectable, so the cursor index counts settings only — the renderer
/// relies on the settings appearing here in exactly `settings_order`'s
/// order.
fn settings_rows(value: impl Fn(Setting) -> String) -> Vec<SettingsRow> {
    let mut rows = Vec::new();
    let mut group = None;
    for setting in settings_order() {
        if group != Some(setting.group()) {
            group = Some(setting.group());
            rows.push(SettingsRow::Header(setting.group().label()));
        }
        rows.push(SettingsRow::Setting {
            name: setting.label(),
            value: value(setting),
            // The model row opens the picker instead of cycling.
            hint: if setting.is_action() {
                "Enter"
            } else {
                "← →"
            },
        });
    }
    rows
}

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

    /// Every selectable `/config` row, in display order.
    pub fn settings_order(&self) -> Vec<Setting> {
        settings_order()
    }

    /// Rows of the `/config` dialog, section headings included. Values are
    /// rebuilt every frame so concurrent changes (Shift+Tab, Ctrl+T) show
    /// up while the dialog is open.
    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        settings_rows(|setting| self.setting_value(setting))
    }

    /// The displayed value of one row. Everything shared comes from the
    /// core table; the two exceptions are the model (the TUI tracks the
    /// resolved label, which can differ from the config while a switch is
    /// in flight) and the send key, where a terminal that cannot report the
    /// chosen combination has to say so.
    fn setting_value(&self, setting: Setting) -> String {
        match setting {
            Setting::Shared(SettingId::Model) => self.model_label.clone(),
            Setting::Shared(SettingId::SendKey) => self.send_key_label(),
            Setting::Shared(id) => id.value(&self.cfg),
            Setting::Reasoning => if self.show_reasoning {
                t!("reasoning_shown")
            } else {
                t!("reasoning_collapsed")
            }
            .to_string(),
            Setting::RawView => picocode_core::config::on_off(self.raw_view),
        }
    }

    /// The `/config` send-key value: the key itself, plus what the terminal
    /// actually does when it cannot report that combination.
    fn send_key_label(&self) -> String {
        let key = self.cfg.submit_key;
        if !self.enhanced_keys && super::needs_enhanced_keys(key) {
            t!("send_key_no_report", key = key.label()).to_string()
        } else {
            key.label().to_string()
        }
    }

    /// The row the cursor is on, or `None` if the dialog is closed.
    fn selected_setting(&self) -> Option<Setting> {
        let menu = self.settings.as_ref()?;
        self.settings_order().get(menu.selected).copied()
    }

    /// ←/→ on a `/config` row: change the value in place. Every change
    /// applies immediately, and is remembered for future runs unless the
    /// core table says otherwise (the mode is deliberately session-only).
    pub(super) fn adjust_setting(&mut self, delta: i64) {
        let Some(setting) = self.selected_setting() else {
            return;
        };
        match setting {
            Setting::Shared(id) => {
                id.adjust(&mut self.cfg, delta);
                id.save_into(&self.cfg, &mut self.saved);
                if id.is_per_selection() {
                    picocode_core::state::save_last_model(&self.cfg);
                }
            }
            Setting::Reasoning => {
                self.show_reasoning = !self.show_reasoning;
                self.saved.set_ui(REASONING_KEY, self.show_reasoning);
            }
            Setting::RawView => {
                self.raw_view = !self.raw_view;
                self.remember_raw_view();
            }
        }
        picocode_core::config::saved::save(&self.saved);
    }

    /// Ctrl+R: plain text instead of the rendered transcript, and back.
    /// Remembered like the `/config` rows, since it is one of them.
    pub(super) fn toggle_raw_view(&mut self) {
        self.raw_view = !self.raw_view;
        self.remember_raw_view();
        picocode_core::config::saved::save(&self.saved);
    }

    fn remember_raw_view(&mut self) {
        self.saved
            .set_ui(picocode_core::config::saved::RAW_VIEW_KEY, self.raw_view);
    }

    /// Enter/Space on a `/config` row: toggles act like →; the model row
    /// closes the dialog and opens the `/model` picker.
    pub(super) fn activate_setting(&mut self) {
        if self.selected_setting().is_some_and(Setting::is_action) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The renderer walks `settings_rows` and advances the cursor index
    /// only on `Setting` rows, so the settings it yields have to be exactly
    /// `settings_order`, in order — otherwise ←/→ lands on the wrong row.
    #[test]
    fn headings_do_not_disturb_the_cursor_index() {
        let rows = settings_rows(|s| format!("{s:?}"));
        let rendered: Vec<String> = rows
            .iter()
            .filter_map(|row| match row {
                SettingsRow::Setting { value, .. } => Some(value.clone()),
                SettingsRow::Header(_) => None,
            })
            .collect();
        let expected: Vec<String> = settings_order().iter().map(|s| format!("{s:?}")).collect();
        assert_eq!(rendered, expected);
        assert!(
            rows.len() > expected.len(),
            "no section headings were emitted"
        );
    }

    /// Every section runs once: a stray row would put a second heading with
    /// the same name further down the dialog.
    #[test]
    fn each_section_heading_appears_once() {
        let headings: Vec<String> = settings_rows(|_| String::new())
            .iter()
            .filter_map(|row| match row {
                SettingsRow::Header(title) => Some(title.clone()),
                SettingsRow::Setting { .. } => None,
            })
            .collect();
        let mut unique = headings.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(headings.len(), unique.len(), "{headings:?}");
    }

    /// The view toggles are the TUI's own rows; the rest come from core.
    #[test]
    fn the_shared_table_is_rendered_whole() {
        let order = settings_order();
        for id in SettingId::SHARED {
            assert!(order.contains(&Setting::Shared(id)), "{id:?} is missing");
        }
        assert!(order.contains(&Setting::Reasoning));
        assert!(order.contains(&Setting::RawView));
    }
}
