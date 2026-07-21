//! The chat view: state, agent events, commands, and the input row — a gpui
//! rendering of picocode-core's event stream. The visual pieces live in the
//! submodules: [`transcript`] (entry rendering), [`dialogs`] (approval /
//! question / settings / pickers) and [`status`] (status bar and its menus).

mod dialogs;
mod status;
mod transcript;

use status::mode_name;

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FocusHandle, ListAlignment, ListOffset, ListState,
    Pixels, Point, Window, div, list, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, StyledExt, Theme, ThemeMode};
use rust_i18n::t;
use tokio::sync::{mpsc, oneshot, watch};

use std::path::PathBuf;

use picocode_core::config::{self, Config, Mode};
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::transcript::{Entry, EntryKind, diff_lines};
use picocode_core::{agent, approval, models, session, state};

use crate::settings::{self, GuiSettings, ThemeSetting};

const TOOL_OUTPUT_MAX_LINES: usize = 12;
const DIFF_MAX_LINES: usize = 30;

gpui::actions!(picocode_gui, [AcceptCompletion, SubmitPrompt]);

/// Slash commands the GUI supports, paired with the locale key of their
/// description for the completion popup.
const COMMANDS: &[(&str, &str)] = &[
    ("/clear", "cmd_clear"),
    ("/compact", "cmd_compact"),
    ("/model", "cmd_model"),
    ("/resume", "cmd_resume"),
    ("/read-only", "cmd_read_only"),
    ("/edit", "cmd_edit"),
    ("/plan", "cmd_plan"),
    ("/bypass", "cmd_bypass"),
    ("/permissions", "cmd_permissions"),
    ("/config", "cmd_config"),
    ("/settings", "cmd_settings"),
    ("/status", "cmd_status"),
    ("/usage", "cmd_usage"),
    ("/quit", "cmd_quit"),
    ("/exit", "cmd_exit"),
];

/// Which status-bar popup menu is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Mode,
    Model,
    /// Details of running bash background jobs.
    Background,
    /// Context / token usage details.
    Context,
}

/// A destructive tool call waiting for the user's yes / no / always.
struct Approval {
    name: String,
    args: String,
    respond: oneshot::Sender<bool>,
}

/// A `submit_plan` approval dialog waiting for an option pick.
struct Question {
    title: String,
    question: String,
    options: Vec<String>,
    respond: oneshot::Sender<Option<usize>>,
}

pub struct ChatView {
    cfg: Config,
    entries: Vec<Entry>,
    input: Entity<InputState>,
    event_tx: mpsc::Sender<AgentEvent>,
    cmd_tx: mpsc::Sender<WorkerCmd>,
    cancel_tx: watch::Sender<()>,
    /// Handle of the tokio runtime the agent worker lives on, for spawning
    /// provider requests (model lists) and model switches.
    rt: tokio::runtime::Handle,
    running: bool,
    /// Keyboard focus while an approval / question dialog is open, so
    /// y/n/a/Esc reach the dialog instead of the text input.
    dialog_focus: FocusHandle,
    /// True while a completion request is in flight but no tokens have
    /// arrived yet — the status bar shows "waiting" instead of "generating".
    waiting: bool,
    approval: Option<Approval>,
    question: Option<Question>,
    /// Which status-bar menu is open, if any.
    menu: Option<Menu>,
    /// Model ids the provider reported serving (via `ModelList`).
    available_models: Vec<String>,
    /// Id of the session being written; a new one starts on `/clear`.
    session_id: String,
    sessions_dir: Option<PathBuf>,
    /// Rows of the open `/resume` dialog, if any.
    session_picker: Option<Vec<session::SessionSummary>>,
    /// Whether the `/config` dialog is open.
    settings_open: bool,
    /// Show reasoning entries in full, or collapsed to one line.
    show_reasoning: bool,
    /// Color theme: follow the system (default), or forced light/dark.
    theme_pref: ThemeSetting,
    /// Sparse overlay of `/config` values the user changed, persisted to
    /// disk and re-applied on the next start.
    saved: GuiSettings,
    /// Completion prefix locked at the first Tab press, so cycling keeps
    /// the full candidate list even after the input holds a full match.
    comp_prefix: Option<String>,
    /// Set while Tab fills the input, so the resulting Change event doesn't
    /// reset `comp_prefix`.
    completing: bool,
    /// Git branch of the project root (refreshed after each turn), shown
    /// above the input box.
    git_branch: Option<String>,
    /// Backgrounded (timed-out) bash commands still running:
    /// (id, command, start time). Shown in the status bar; clicking opens
    /// a details popup.
    bg_jobs: Vec<(u64, String, std::time::Instant)>,
    /// Context tokens of the last completion request / output tokens so far.
    tokens_in: u64,
    tokens_out: u64,
    /// Rough output-token estimate for the in-flight completion, accumulated
    /// from streamed deltas so the counter moves while the model generates.
    /// Snapped back to zero whenever the provider reports real usage.
    est_out: u64,
    /// RaTeX-rendered display formulas, keyed by (scale, color, tex).
    math_cache: crate::tex::MathCache,
    /// Prompts submitted while a turn was running, held back and sent one
    /// per completed turn (Stop returns them to the input box instead).
    queued: Vec<String>,
    /// Open right-click menu: (transcript entry index, click position).
    ctx_menu: Option<(usize, Point<Pixels>)>,
    /// Virtualized-list state for the transcript: only visible entries are
    /// rendered and measured. Bottom alignment gives chat-log scrolling —
    /// the view sticks to the bottom until the user scrolls up, and resumes
    /// following when scrolled back down. Must be kept in sync with
    /// `entries` (see `push_entry` / `reset_list`).
    list_state: ListState,
}

impl ChatView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        mut event_rx: mpsc::Receiver<AgentEvent>,
        event_tx: mpsc::Sender<AgentEvent>,
        cmd_tx: mpsc::Sender<WorkerCmd>,
        cancel_tx: watch::Sender<()>,
        rt: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder(t!("placeholder").to_string())
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.subscribe_in(&input, window, Self::on_input_event)
            .detach();

        // Follow live system light/dark switches while the preference is
        // "system" (the startup sync happens in gpui_component::init).
        window
            .observe_window_appearance({
                let this = cx.entity().downgrade();
                move |window, cx| {
                    if let Some(this) = this.upgrade()
                        && this.read(cx).theme_pref == ThemeSetting::System
                    {
                        Theme::sync_system_appearance(Some(window), cx);
                    }
                }
            })
            .detach();

        // Pump agent events from the tokio channel into this view. tokio's
        // mpsc receiver is executor-agnostic, so awaiting it on gpui's
        // executor is fine.
        cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = event_rx.recv().await {
                let alive = this.update_in(cx, |view, window, cx| {
                    view.on_agent_event(ev, window, cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Re-apply the settings saved by earlier runs' /config changes
        // (a sparse overlay — untouched values keep following the config
        // file). The runtime handles are shared with the worker, so setting
        // them here is enough.
        let saved = settings::load();
        Self::apply_saved(&saved, &cfg);
        let theme_pref = saved.theme.unwrap_or(ThemeSetting::System);
        Self::apply_theme(theme_pref, cx);

        let sessions_dir = session::sessions_dir(&cfg.root);
        let git_branch = picocode_core::git::branch(&cfg.root);
        let view = Self {
            cfg,
            entries: Vec::new(),
            input,
            event_tx,
            cmd_tx,
            cancel_tx,
            rt,
            running: false,
            dialog_focus: cx.focus_handle(),
            waiting: false,
            approval: None,
            question: None,
            menu: None,
            available_models: Vec::new(),
            session_id: session::new_id(),
            sessions_dir,
            session_picker: None,
            settings_open: false,
            show_reasoning: saved.show_reasoning.unwrap_or(true),
            theme_pref,
            saved,
            comp_prefix: None,
            completing: false,
            git_branch,
            bg_jobs: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            est_out: 0,
            math_cache: crate::tex::MathCache::new(),
            queued: Vec::new(),
            ctx_menu: None,
            // The overdraw pre-measures entries near the viewport so
            // scrolling doesn't pop items in.
            list_state: ListState::new(0, ListAlignment::Bottom, px(512.)),
        };
        view.save_last_model();
        view
    }

    /// Apply the persisted /config overlay onto a config's shared handles
    /// (used at startup and when switching working directories).
    fn apply_saved(saved: &GuiSettings, cfg: &Config) {
        if let Some(v) = saved.bash_timeout {
            cfg.bash_timeout.set(v);
        }
        if let Some(v) = saved.read_max_lines {
            cfg.read_max_lines.set(v);
        }
        if let Some(v) = saved.read_max_line_bytes {
            cfg.read_max_line_bytes.set(v);
        }
        if let Some(v) = saved.auto_compact {
            cfg.auto_compact.set(v);
        }
        if let Some(p) = saved.search_provider {
            cfg.search.set_provider(p);
        }
        if let Some(n) = saved.search_max_results {
            cfg.search.set_max_results(n);
        }
    }

    fn apply_theme(pref: ThemeSetting, cx: &mut Context<Self>) {
        match pref {
            ThemeSetting::System => Theme::sync_system_appearance(None, cx),
            ThemeSetting::Light => Theme::change(ThemeMode::Light, None, cx),
            ThemeSetting::Dark => Theme::change(ThemeMode::Dark, None, cx),
        }
    }

    /// Remember the active model (best-effort) so the next start in this
    /// project resumes with it.
    fn save_last_model(&self) {
        let Some(path) = state::state_path(&self.cfg.root) else {
            return;
        };
        let _ = state::save(
            &path,
            &state::LastModel {
                entry: self.cfg.active_model.clone(),
                provider: config::provider_name(self.cfg.provider).to_string(),
                model: self.cfg.model.clone(),
                base_url: self.cfg.base_url.clone(),
            },
        );
    }

    // ---------- events from the agent worker ----------

    fn on_agent_event(&mut self, ev: AgentEvent, window: &mut Window, cx: &mut Context<Self>) {
        // No explicit scroll-follow here: the bottom-aligned virtual list
        // sticks to the bottom on its own while the user hasn't scrolled up,
        // and pins the position (like the TUI) while they have.
        match ev {
            AgentEvent::TextDelta(s) => {
                self.waiting = false;
                self.est_out += est_tokens(&s);
                self.append(EntryKind::Assistant, &s);
            }
            AgentEvent::ReasoningDelta(s) => {
                self.waiting = false;
                self.est_out += est_tokens(&s);
                self.append(EntryKind::Reasoning, &s);
            }
            AgentEvent::ToolCall { name, args } => {
                self.waiting = false;
                self.est_out += est_tokens(&args);
                self.push_tool_call(&name, &args);
            }
            AgentEvent::ToolResult { output } => {
                self.push(EntryKind::ToolOut, clip(&output, TOOL_OUTPUT_MAX_LINES));
            }
            AgentEvent::ApprovalRequest {
                name,
                args,
                respond,
            } => {
                self.waiting = false;
                self.approval = Some(Approval {
                    name,
                    args,
                    respond,
                });
                // y/n/a and Esc go to the dialog, not the text input.
                self.dialog_focus.focus(window);
            }
            AgentEvent::UserQuestion {
                title,
                question,
                options,
                respond,
            } => {
                self.question = Some(Question {
                    title,
                    question,
                    options,
                    respond,
                });
                self.dialog_focus.focus(window);
            }
            AgentEvent::Usage { input, output } => {
                self.tokens_in = input;
                self.tokens_out = output;
                // Real usage supersedes the streaming estimate; the next
                // completion in this run starts estimating from zero again.
                self.est_out = 0;
            }
            AgentEvent::ModelList { label, result } => match result {
                Ok(mut names) => {
                    names.sort();
                    self.available_models = names;
                }
                Err(e) => {
                    // Background refreshes fail silently; surface the error
                    // when the model menu is waiting on the list.
                    if self.menu == Some(Menu::Model) {
                        self.push(
                            EntryKind::Error,
                            t!("model_list_failed", label = label, error = e).to_string(),
                        );
                    }
                }
            },
            AgentEvent::Compacted { messages, summary } => {
                if messages == 0 {
                    self.push(EntryKind::Notice, t!("nothing_to_compact").to_string());
                } else {
                    self.push(EntryKind::Notice, t!("compacted", n = messages).to_string());
                    self.push(EntryKind::Summary, summary);
                }
                self.running = false;
                self.est_out = 0;
                self.autosave();
                self.flush_queued();
            }
            AgentEvent::ShellOutput { output } => self.push(EntryKind::ToolOut, output),
            AgentEvent::BackgroundStarted { id, command } => {
                self.bg_jobs.push((id, command, std::time::Instant::now()));
                self.push(EntryKind::Notice, t!("bg_started", id = id).to_string());
            }
            AgentEvent::BackgroundDone {
                id,
                command,
                output,
            } => {
                self.bg_jobs.retain(|(job_id, _, _)| *job_id != id);
                if self.bg_jobs.is_empty() && self.menu == Some(Menu::Background) {
                    self.menu = None;
                }
                self.push(EntryKind::Notice, t!("bg_done", id = id).to_string());
                self.push(EntryKind::ToolOut, clip(&output, TOOL_OUTPUT_MAX_LINES));
                // Prompt the model with the result so it reacts to it, like
                // the TUI does.
                let prompt =
                    format!("[background job #{id} finished] `{command}` output:\n{output}");
                if self.cmd_tx.try_send(WorkerCmd::Prompt(prompt)).is_ok() {
                    self.running = true;
                    self.waiting = true;
                }
            }
            AgentEvent::Cancelled => {
                self.push(EntryKind::Notice, t!("cancelled").to_string());
                // Stop means stop: give held-back prompts to the input box
                // instead of firing them on the TurnComplete that follows.
                if !self.queued.is_empty() {
                    let mut text = std::mem::take(&mut self.queued).join("\n");
                    let existing = self.input.read(cx).value().to_string();
                    if !existing.is_empty() {
                        text.push('\n');
                        text.push_str(&existing);
                    }
                    self.input
                        .update(cx, |state, cx| state.set_value(text, window, cx));
                    self.push(EntryKind::Notice, t!("queued_restored").to_string());
                }
            }
            AgentEvent::TurnComplete => {
                self.running = false;
                self.waiting = false;
                // A cancelled or failed completion never reports usage; drop
                // its estimate rather than carrying it into the idle counter.
                self.est_out = 0;
                // Tools may have switched branches during the turn.
                self.git_branch = picocode_core::git::branch(&self.cfg.root);
                self.autosave();
                self.flush_queued();
            }
            AgentEvent::Error(e) => {
                self.waiting = false;
                self.push(EntryKind::Error, e);
            }
        }
    }

    /// Jump the transcript to the bottom and resume following the stream.
    /// (Scrolling to the very end normalizes to the bottom-aligned list's
    /// "sticking" state on the next layout.)
    fn scroll_to_bottom(&self) {
        self.list_state.scroll_to(ListOffset {
            item_ix: self.list_state.item_count(),
            offset_in_item: px(0.),
        });
    }

    /// Rebuild the list state after a wholesale transcript change (clear,
    /// resume, entry-height-affecting settings). Drops the cached heights
    /// and snaps to the bottom.
    fn reset_list(&self) {
        self.list_state.reset(self.entries.len());
    }

    /// Snapshot the conversation to disk. Runs in the background after each
    /// completed turn; empty conversations are not written.
    fn autosave(&mut self) {
        let Some(dir) = self.sessions_dir.clone() else {
            return;
        };
        let id = self.session_id.clone();
        let cwd = self.cfg.root.display().to_string();
        let model = self.cfg.model_label();
        let entries: Vec<Entry> = self.entries.clone();
        let cmd_tx = self.cmd_tx.clone();
        let event_tx = self.event_tx.clone();
        self.rt.spawn(async move {
            let (htx, hrx) = oneshot::channel();
            if cmd_tx.send(WorkerCmd::TakeHistory(htx)).await.is_err() {
                return;
            }
            let Ok(history) = hrx.await else {
                return;
            };
            if history.is_empty() {
                return;
            }
            let file = session::SessionFile::new(cwd, model, history, entries);
            if let Err(e) = session::save(&dir, &id, &file) {
                let _ = event_tx
                    .send(AgentEvent::Error(format!("Failed to save session: {e:#}")))
                    .await;
            }
        });
    }

    /// `/resume`: open the session-selection dialog.
    fn open_session_picker(&mut self, cx: &mut Context<Self>) {
        if self.running {
            self.push(EntryKind::Error, t!("resume_while_running").to_string());
            cx.notify();
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(EntryKind::Error, t!("no_home").to_string());
            cx.notify();
            return;
        };
        // Resuming the current session would be a no-op, so it isn't offered.
        let sessions: Vec<_> = session::list(&dir)
            .into_iter()
            .filter(|s| s.id != self.session_id)
            .collect();
        if sessions.is_empty() {
            self.push(EntryKind::Notice, t!("no_sessions").to_string());
            cx.notify();
            return;
        }
        self.session_picker = Some(sessions);
        cx.notify();
    }

    /// Resume a session by id (picked in the dialog or given to
    /// `/resume <id>`): seed the worker with its history and restore the
    /// transcript.
    fn resume_session(&mut self, id: &str, cx: &mut Context<Self>) {
        self.session_picker = None;
        if self.running {
            self.push(EntryKind::Error, t!("resume_while_running").to_string());
            cx.notify();
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(EntryKind::Error, t!("no_home").to_string());
            cx.notify();
            return;
        };
        if id == self.session_id {
            self.push(EntryKind::Notice, t!("current_session").to_string());
            cx.notify();
            return;
        }

        let saved = match session::load(&dir, id) {
            Ok(s) => s,
            Err(e) => {
                self.push(
                    EntryKind::Error,
                    t!("resume_failed", error = format!("{e:#}")).to_string(),
                );
                cx.notify();
                return;
            }
        };
        let messages = saved.history.len();
        if self
            .cmd_tx
            .try_send(WorkerCmd::SeedHistory(saved.history))
            .is_err()
        {
            self.push(EntryKind::Error, t!("worker_stopped").to_string());
            cx.notify();
            return;
        }

        self.entries.clear();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.est_out = 0;
        self.push(
            EntryKind::Notice,
            t!("resumed", id = id, n = messages, model = saved.model).to_string(),
        );
        self.entries.extend(saved.entries);
        self.session_id = id.to_string();
        self.queued.clear();
        self.reset_list();
        cx.notify();
    }

    /// `/status` (alias `/usage`): one-shot overview of the model, token
    /// usage, permission mode and session.
    fn show_status(&mut self) {
        let prompts = self
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::User)
            .count();
        let text = picocode_core::report::status_text(
            &self.cfg,
            &picocode_core::report::StatusInfo {
                model_label: &self.cfg.model_label(),
                ctx_tokens: self.tokens_in,
                output: format!("{} tokens (last reported)", self.tokens_out),
                session_id: &self.session_id,
                prompts,
                sessions_dir: self.sessions_dir.as_deref(),
            },
        );
        self.push(EntryKind::Notice, text);
    }

    /// `/permissions`: show what the current mode and config rules do.
    fn show_permissions(&mut self) {
        let text = picocode_core::report::permissions_text(&self.cfg);
        self.push(EntryKind::Notice, text);
    }

    /// Cycle the theme preference and apply it.
    fn cycle_theme(&mut self, delta: i64, cx: &mut Context<Self>) {
        const CYCLE: [ThemeSetting; 3] = [
            ThemeSetting::System,
            ThemeSetting::Light,
            ThemeSetting::Dark,
        ];
        let i = CYCLE
            .iter()
            .position(|t| *t == self.theme_pref)
            .unwrap_or(0);
        self.theme_pref = if delta < 0 {
            CYCLE[(i + CYCLE.len() - 1) % CYCLE.len()]
        } else {
            CYCLE[(i + 1) % CYCLE.len()]
        };
        self.saved.theme = Some(self.theme_pref);
        Self::apply_theme(self.theme_pref, cx);
    }

    /// A `/config` row change. Every change applies immediately. Mirrors
    /// the TUI's `/config` dialog, plus the GUI-only theme row.
    fn adjust_setting(&mut self, row: usize, delta: i64, cx: &mut Context<Self>) {
        match row {
            0 => self.cycle_theme(delta, cx),
            // Same cycle as the TUI: bypass stays menu/command-only, and
            // adjusting away from it lands on read-only.
            1 => self.cfg.mode.set(self.cfg.mode.get().cycled(delta)),
            2 => {
                self.show_reasoning = !self.show_reasoning;
                self.saved.show_reasoning = Some(self.show_reasoning);
                // Reasoning entries change height everywhere, invalidating
                // the list's cached measurements.
                self.reset_list();
            }
            3 => {
                self.cfg.step_bash_timeout(delta);
                self.saved.bash_timeout = Some(self.cfg.bash_timeout.get());
            }
            4 => {
                self.cfg.step_read_lines(delta);
                self.saved.read_max_lines = Some(self.cfg.read_max_lines.get());
            }
            5 => {
                self.cfg.step_line_bytes(delta);
                self.saved.read_max_line_bytes = Some(self.cfg.read_max_line_bytes.get());
            }
            6 => {
                self.cfg.search.cycle_provider(delta);
                self.saved.search_provider = Some(self.cfg.search.snapshot().provider);
            }
            7 => {
                self.cfg.search.step_max_results(delta);
                self.saved.search_max_results = Some(self.cfg.search.snapshot().max_results);
            }
            8 => {
                self.cfg.step_auto_compact(delta);
                self.saved.auto_compact = Some(self.cfg.auto_compact.get());
            }
            // Model: close the dialog and open the model menu.
            9 => {
                self.settings_open = false;
                self.toggle_menu(Menu::Model, cx);
            }
            _ => {}
        }
        // Persist every touched value (the mode and model rows change
        // nothing in `saved`; rewriting the small file is harmless).
        settings::save(&self.saved);
        cx.notify();
    }

    /// Show a tool call: `edit_file` gets a path headline plus a colored
    /// diff; everything else keeps the compact JSON args line.
    fn push_tool_call(&mut self, name: &str, args: &str) {
        let parsed: Option<serde_json::Value> = serde_json::from_str(args).ok();
        let get = |k: &str| {
            parsed
                .as_ref()
                .and_then(|v| v.get(k))
                .and_then(|v| v.as_str())
        };
        if name == "edit_file"
            && let (Some(path), Some(new)) = (get("path"), get("new_string"))
        {
            // Without old_string the call is a whole-file create/overwrite,
            // which diffs as pure additions.
            let old = get("old_string").unwrap_or_default();
            self.push(EntryKind::Tool, format!("{name} {path}"));
            let diff = diff_lines(old, new).join("\n");
            self.push_entry(Entry {
                kind: EntryKind::Diff,
                text: clip(&diff, DIFF_MAX_LINES),
                lang: Some(path.to_string()),
            });
            return;
        }
        self.push(EntryKind::Tool, format!("{name} {}", one_line(args, 160)));
    }

    /// Append a streamed delta to the last entry of the same kind, or start
    /// a new entry (matches the TUI's transcript behavior).
    fn append(&mut self, kind: EntryKind, delta: &str) {
        match self.entries.last_mut() {
            Some(e) if e.kind == kind => e.text.push_str(delta),
            _ => self.push(kind, delta.to_string()),
        }
    }

    fn push(&mut self, kind: EntryKind, text: String) {
        self.push_entry(Entry {
            kind,
            text,
            lang: None,
        });
    }

    /// Append a transcript entry and register the new row with the
    /// virtualized list. In-place text growth (streamed deltas) needs no
    /// notification — visible items are re-measured every frame.
    fn push_entry(&mut self, entry: Entry) {
        let n = self.entries.len();
        self.entries.push(entry);
        self.list_state.splice(n..n, 1);
    }

    // ---------- user actions ----------

    fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        ev: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            // Plain Enter is rebound to SubmitPrompt and never reaches the
            // input (see main.rs), so this only fires for stray paths; a
            // secondary Enter (Shift+Enter, Cmd+Enter) keeps the newline the
            // multi-line input inserted.
            InputEvent::PressEnter { secondary: false } => self.submit(window, cx),
            // Typing anything resets the Tab-cycling anchor (unless the
            // change came from Tab itself filling the input).
            InputEvent::Change => {
                if self.completing {
                    self.completing = false;
                } else {
                    self.comp_prefix = None;
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// Plain Enter in the input: submit. Bound directly to this action so
    /// the multi-line input never inserts a newline at the cursor first
    /// (its own Enter handling inserts, then emits PressEnter — which left
    /// a stray newline in the submitted text when the cursor sat mid-line).
    fn on_submit_prompt(&mut self, _: &SubmitPrompt, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    /// Tab in the input: fill the first matching slash command, or cycle
    /// through the matches of the prefix locked at the first press.
    fn accept_completion(
        &mut self,
        _: &AcceptCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_open() {
            return;
        }
        let value = self.input.read(cx).value().to_string();
        if !value.starts_with('/') || value.contains(char::is_whitespace) {
            return;
        }
        let prefix = self.comp_prefix.clone().unwrap_or_else(|| value.clone());
        let matches: Vec<&str> = COMMANDS
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| name.starts_with(&prefix))
            .collect();
        if matches.is_empty() {
            self.comp_prefix = None;
            return;
        }
        let next = match matches.iter().position(|name| *name == value) {
            Some(i) => matches[(i + 1) % matches.len()],
            None => matches[0],
        };
        self.comp_prefix = Some(prefix);
        self.completing = true;
        self.input
            .update(cx, |state, cx| state.set_value(next, window, cx));
        cx.notify();
    }

    fn dialog_open(&self) -> bool {
        self.approval.is_some()
            || self.question.is_some()
            || self.session_picker.is_some()
            || self.settings_open
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog_open() {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        // Clear even when only whitespace remains, so empty submits can't
        // leave blank lines behind.
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        if text.is_empty() {
            return;
        }

        match text.as_str() {
            "/clear" => {
                let _ = self.cmd_tx.try_send(WorkerCmd::Clear);
                self.entries.clear();
                self.queued.clear();
                self.tokens_in = 0;
                self.tokens_out = 0;
                self.est_out = 0;
                // A cleared conversation starts a fresh session log.
                self.session_id = session::new_id();
                self.push(EntryKind::Notice, t!("cleared").to_string());
                self.reset_list();
            }
            "/compact" => {
                let _ = self.cmd_tx.try_send(WorkerCmd::Compact);
                self.push(EntryKind::Notice, t!("compacting").to_string());
                self.running = true;
                self.waiting = true;
            }
            "/quit" | "/exit" => cx.quit(),
            "/read-only" => self.select_mode(Mode::ReadOnly, cx),
            "/edit" => self.select_mode(Mode::Edit, cx),
            "/plan" => self.select_mode(Mode::Plan, cx),
            "/bypass" => self.select_mode(Mode::Bypass, cx),
            "/model" => self.toggle_menu(Menu::Model, cx),
            "/resume" => self.open_session_picker(cx),
            "/config" | "/settings" => self.settings_open = true,
            "/status" | "/usage" => self.show_status(),
            "/permissions" => self.show_permissions(),
            _ if text.starts_with("/model ") => {
                let name = text["/model ".len()..].trim().to_string();
                self.switch_model(&name, cx);
            }
            _ if text.starts_with("/resume ") => {
                let id = text["/resume ".len()..].trim().to_string();
                self.resume_session(&id, cx);
            }
            _ if text.starts_with('/') => {
                self.push(
                    EntryKind::Notice,
                    t!("not_available", cmd = text).to_string(),
                );
            }
            // Mid-turn prompts are held back (shown above the input box)
            // and sent one per completed turn, instead of being rejected.
            _ if self.running => self.queued.push(text),
            _ => self.send_prompt(text),
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    /// Record a user prompt in the transcript and hand it to the worker.
    pub fn send_prompt(&mut self, text: String) {
        self.push(EntryKind::User, text.clone());
        let _ = self.cmd_tx.try_send(WorkerCmd::Prompt(text));
        self.running = true;
        self.waiting = true;
    }

    /// Send the oldest held-back prompt, if any. Called when a turn ends;
    /// one prompt per turn, so each gets its own response.
    fn flush_queued(&mut self) {
        if self.queued.is_empty() {
            return;
        }
        let text = self.queued.remove(0);
        self.send_prompt(text);
    }

    fn stop(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.cancel_tx.send(());
        cx.notify();
    }

    /// Explicit mode switch (menu or /read-only etc.). Takes effect
    /// immediately, even for tool calls later in a turn already running.
    fn select_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.menu = None;
        if self.cfg.mode.get() == mode {
            cx.notify();
            return;
        }
        self.cfg.mode.set(mode);
        if mode == Mode::Bypass {
            self.push(EntryKind::Warning, t!("bypass_warning").to_string());
        } else {
            self.push(
                EntryKind::Notice,
                t!("mode_changed", mode = mode_name(mode)).to_string(),
            );
        }
        cx.notify();
    }

    /// Ask the provider for its model list in the background; the answer
    /// arrives as a `ModelList` event through the regular pump.
    fn refresh_models(&self) {
        let provider = self.cfg.provider;
        let base = self.cfg.base_url.clone();
        let label = format!(
            "{} @ {}",
            config::provider_name(provider),
            models::base_url(provider, base.as_deref())
        );
        let event_tx = self.event_tx.clone();
        self.rt.spawn(async move {
            let result = models::fetch(provider, base.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = event_tx.send(AgentEvent::ModelList { label, result }).await;
        });
    }

    /// Switch to a named `[[models]]` entry — or to a model the provider
    /// reported serving — and carry the conversation history over to the
    /// new worker (same flow as the TUI's `/model <name>`).
    fn switch_model(&mut self, name: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if self.running {
            self.push(EntryKind::Error, t!("switch_while_running").to_string());
            cx.notify();
            return;
        }
        let mut new_cfg = self.cfg.clone();
        match self.cfg.models.iter().find(|m| m.name == name) {
            Some(entry) => {
                if self.cfg.active_model.as_deref() == Some(name) {
                    self.push(
                        EntryKind::Notice,
                        t!(
                            "already_using",
                            name = format!("{name} ({})", entry.label())
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                }
                new_cfg.provider = entry.provider;
                new_cfg.model = entry.model.clone();
                new_cfg.base_url = entry.base_url.clone();
                new_cfg.active_model = Some(entry.name.clone());
                new_cfg.context_window = entry
                    .context_window
                    .unwrap_or(config::DEFAULT_CONTEXT_WINDOW);
            }
            // A model id the provider reported serving: switch ad hoc,
            // keeping the current provider and base URL.
            None if self.available_models.iter().any(|m| m == name) => {
                if self.cfg.active_model.is_none() && self.cfg.model == name {
                    self.push(
                        EntryKind::Notice,
                        t!("already_using", name = self.cfg.model_label()).to_string(),
                    );
                    cx.notify();
                    return;
                }
                new_cfg.model = name.to_string();
                new_cfg.active_model = None;
                new_cfg.context_window = config::DEFAULT_CONTEXT_WINDOW;
            }
            None => {
                self.push(
                    EntryKind::Error,
                    t!("unknown_model", name = name).to_string(),
                );
                cx.notify();
                return;
            }
        }

        // Spawn first so a failure (e.g. missing API key) leaves the current
        // worker untouched. agent::spawn calls tokio::spawn internally, so it
        // needs the runtime context entered.
        let new_tx = {
            let _guard = self.rt.enter();
            match agent::spawn(&new_cfg, self.event_tx.clone(), self.cancel_tx.subscribe()) {
                Ok(tx) => tx,
                Err(e) => {
                    self.push(
                        EntryKind::Error,
                        t!("switch_failed", name = name, error = format!("{e:#}")).to_string(),
                    );
                    cx.notify();
                    return;
                }
            }
        };

        // Carry the conversation over in the background; `running` blocks
        // prompts until the transfer's TurnComplete lands so a fast prompt
        // can't race the history seed. Dropping the old sender at the end of
        // the task shuts the old worker down.
        let old_tx = std::mem::replace(&mut self.cmd_tx, new_tx.clone());
        let event_tx = self.event_tx.clone();
        self.running = true;
        self.rt.spawn(async move {
            let (htx, hrx) = oneshot::channel();
            if old_tx.send(WorkerCmd::TakeHistory(htx)).await.is_ok()
                && let Ok(history) = hrx.await
            {
                let _ = new_tx.send(WorkerCmd::SeedHistory(history)).await;
            }
            let _ = event_tx.send(AgentEvent::TurnComplete).await;
        });

        // The cached model list belongs to the endpoint it was fetched from.
        let endpoint_changed =
            new_cfg.provider != self.cfg.provider || new_cfg.base_url != self.cfg.base_url;
        self.cfg = new_cfg;
        self.push(
            EntryKind::Notice,
            t!(
                "model_switched",
                name = name,
                label = self.cfg.model_label()
            )
            .to_string(),
        );
        if endpoint_changed {
            self.available_models.clear();
            self.refresh_models();
        }
        self.save_last_model();
        cx.notify();
    }

    /// Click on the workdir label: open a native directory picker and move
    /// the project root there.
    fn pick_workdir(&mut self, cx: &mut Context<Self>) {
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
        let args = config::Args {
            provider: None,
            model: None,
            base_url: None,
            bypass: false,
            smoke: None,
        };
        let mut new_cfg = match config::Config::from_args(args) {
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
        // The new project selects no model of its own: keep the current one.
        if new_cfg.model.is_empty() {
            new_cfg.provider = self.cfg.provider;
            new_cfg.model = self.cfg.model.clone();
            new_cfg.base_url = self.cfg.base_url.clone();
            new_cfg.active_model = None;
            new_cfg.context_window = self.cfg.context_window;
        }
        // Keep the current permission mode and the persisted /config values.
        new_cfg.mode.set(self.cfg.mode.get());
        Self::apply_saved(&self.saved, &new_cfg);

        let new_tx = {
            let _guard = self.rt.enter();
            match agent::spawn(&new_cfg, self.event_tx.clone(), self.cancel_tx.subscribe()) {
                Ok(tx) => tx,
                Err(e) => {
                    self.push(
                        EntryKind::Error,
                        t!("cd_failed", error = format!("{e:#}")).to_string(),
                    );
                    cx.notify();
                    return;
                }
            }
        };
        // Dropping the old sender shuts the old worker down; the new
        // directory starts a fresh conversation and session log.
        self.cmd_tx = new_tx;
        self.cfg = new_cfg;
        self.sessions_dir = session::sessions_dir(&self.cfg.root);
        self.git_branch = picocode_core::git::branch(&self.cfg.root);
        self.session_id = session::new_id();
        self.entries.clear();
        self.queued.clear();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.est_out = 0;
        self.available_models.clear();
        self.refresh_models();
        self.push(
            EntryKind::Notice,
            t!(
                "workdir_changed",
                dir = picocode_core::git::display_dir(&self.cfg.root),
                model = self.cfg.model_label()
            )
            .to_string(),
        );
        self.reset_list();
        self.save_last_model();
        cx.notify();
    }

    fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        if self.menu == Some(menu) {
            self.menu = None;
        } else {
            self.menu = Some(menu);
            if menu == Menu::Model {
                self.refresh_models();
            }
        }
        cx.notify();
    }

    fn answer_approval(
        &mut self,
        approve: bool,
        always: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(a) = self.approval.take() else {
            return;
        };
        self.input.update(cx, |state, cx| state.focus(window, cx));
        if approve && always {
            // Same runtime allow rules the TUI's `a` answer adds.
            match approval::bash_command(&a.name, &a.args) {
                Some(cmd) => {
                    let patterns = config::bash_allow_patterns(&cmd);
                    self.push(
                        EntryKind::Notice,
                        format!("allow_bash += {}", patterns.join(", ")),
                    );
                    self.cfg.approval.allow_bash(&patterns);
                }
                None => {
                    self.push(EntryKind::Notice, format!("allow_tools += {}", a.name));
                    self.cfg.approval.allow_tool(&a.name);
                }
            }
        }
        let _ = a.respond.send(approve);
        cx.notify();
    }

    fn answer_question(
        &mut self,
        answer: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(q) = self.question.take() {
            let _ = q.respond.send(answer);
            self.input.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let border = theme.border;
        let muted = theme.muted;
        let muted_fg = theme.muted_foreground;

        // The transcript is a virtualized list: only the entries in (or
        // near) the viewport are rendered and measured each frame, so long
        // conversations cost the same as short ones.
        let transcript: AnyElement = if self.entries.is_empty() {
            div()
                .text_color(muted_fg)
                .child(
                    t!(
                        "greeting",
                        model = self.cfg.model_label(),
                        root = self.cfg.root.display()
                    )
                    .to_string(),
                )
                .into_any_element()
        } else {
            list(
                self.list_state.clone(),
                cx.processor(|this, ix: usize, window, cx| this.render_item(ix, window, cx)),
            )
            .size_full()
            .into_any_element()
        };

        let send_or_stop: AnyElement = if self.running {
            Button::new("stop")
                .danger()
                .label(t!("stop").to_string())
                .on_click(cx.listener(Self::stop))
                .into_any_element()
        } else {
            Button::new("send")
                .primary()
                .label(t!("send").to_string())
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                .into_any_element()
        };

        div()
            .v_flex()
            .relative()
            .size_full()
            .bg(background)
            .on_action(cx.listener(Self::accept_completion))
            .on_action(cx.listener(Self::on_submit_prompt))
            .child(div().flex_1().p_4().child(transcript))
            .children(self.render_completions(cx))
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .p_3()
                    .border_t_1()
                    .border_color(border)
                    .children(self.render_queued(cx))
                    .child(
                        div()
                            .h_flex()
                            .gap_1()
                            .items_center()
                            .text_sm()
                            .text_color(muted_fg)
                            .child(
                                div()
                                    .id("workdir")
                                    .cursor_pointer()
                                    .rounded_md()
                                    .px_1()
                                    .hover(move |s| s.bg(muted))
                                    .child(picocode_core::git::display_dir(&self.cfg.root))
                                    .on_click(cx.listener(|this, _, _, cx| this.pick_workdir(cx))),
                            )
                            .children(self.git_branch.as_ref().map(|branch| {
                                div()
                                    .h_flex()
                                    .gap_0p5()
                                    .items_center()
                                    .child(
                                        gpui_component::Icon::default()
                                            .path("icons/git-branch.svg")
                                            .size_3p5()
                                            .flex_none(),
                                    )
                                    .child(branch.clone())
                            })),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.input)))
                            .child(send_or_stop),
                    ),
            )
            .child(self.render_status_bar(cx))
            .children(self.render_ctx_menu(window, cx))
            .children(self.render_menu(cx))
            .children(self.render_settings(cx))
            .children(self.render_session_picker(cx))
            .children(self.render_approval(cx))
            .children(self.render_question(cx))
    }
}

// ---------- small helpers ----------

/// Rough token count for one streamed delta. Ollama streams roughly one
/// token per chunk while cloud providers batch several, so take whichever
/// of "one chunk" and "~4 bytes per token" is larger. Only used to animate
/// the status-bar counter between real usage reports.
fn est_tokens(s: &str) -> u64 {
    (s.len() as u64 / 4).max(1)
}

/// Squeeze a JSON args string onto one line, truncated to `max` chars.
fn one_line(s: &str, max: usize) -> String {
    let mut out: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Cap multi-line output at `max_lines`, noting how much was dropped.
fn clip(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.trim_end().to_string();
    }
    let mut out = lines[..max_lines].join("\n");
    out.push_str(&format!("\n… ({} more lines)", lines.len() - max_lines));
    out
}
