//! The chat view: state, the command dispatch and the input row — a gpui
//! rendering of picocode-core's event stream. The visual pieces live in
//! [`transcript`] (entry rendering), [`dialogs`] (approval / question /
//! settings / pickers) and [`status`] (status bar and its menus); the
//! interaction logic in [`events`] (agent events), [`input`] (completion,
//! attachments, paste), [`models`] (model switching), [`prompt`] (system
//! prompt), [`sessions`] (`/resume` + autosave) and [`workspace`] (the
//! `/remote` switch) — all further `impl ChatView` blocks on this struct.

mod dialogs;
mod events;
mod input;
mod models;
mod prompt;
mod sessions;
mod status;
mod transcript;
mod workspace;

use models::AddModel;
use status::mode_name;
use workspace::AddRemote;

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FocusHandle, ListAlignment, ListOffset, ListState,
    Pixels, Point, Window, div, list, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, StyledExt, Theme};
use rust_i18n::t;
use tokio::sync::{mpsc, oneshot, watch};

use std::path::PathBuf;

use picocode_core::attachment::Attachment;
use picocode_core::config::{Config, Mode};
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::transcript::{Entry, EntryKind, diff_lines};
use picocode_core::{approval, config, session};

use crate::settings::{self, GuiSettings, ThemeSetting};

const TOOL_OUTPUT_MAX_LINES: usize = 12;
const DIFF_MAX_LINES: usize = 30;

gpui::actions!(
    picocode_gui,
    [AcceptCompletion, SubmitPrompt, PasteClipboard]
);

/// Which status-bar popup menu is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Mode,
    Model,
    /// Details of running bash background jobs.
    Background,
    /// Workspace picker: the local project, the configured `[[remotes]]`,
    /// and a native folder picker.
    Workspace,
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
    /// Mid-turn steering queue shared with the worker: text sent while a
    /// turn runs is injected at the next tool-call boundary.
    steer: picocode_core::steer::SteerQueue,
    /// Registry of running background jobs (list / kill). App-owned so
    /// jobs survive worker respawns on model switches.
    pub(super) jobs: picocode_core::tools::BackgroundJobs,
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
    /// Open add-model dialog (reached from the model menu), if any.
    add_model: Option<AddModel>,
    /// Open add-remote dialog (from the workspace menu), if any.
    add_remote: Option<AddRemote>,
    /// The `/prompt` system-prompt editor dialog while it is open.
    prompt_edit: Option<Entity<InputState>>,
    /// MCP connections established at startup, reused across respawns.
    mcp: picocode_core::mcp::McpConnections,
    /// Workspace backend (local or remote), reused across respawns.
    backend: picocode_core::backend::Backend,
    /// Live search box at the top of the model menu.
    pub(super) model_filter: Entity<InputState>,
    /// Reasoning entries the user expanded (indices into `entries`);
    /// everything else renders collapsed to a one-line preview.
    expanded_reasoning: std::collections::HashSet<usize>,
    /// Color theme: follow the system (default), or forced light/dark.
    theme_pref: ThemeSetting,
    /// Color-theme family (`theme::FAMILIES` label).
    theme_family: String,
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
    /// Each keeps the attachments staged when it was submitted.
    queued: Vec<(String, Vec<Attachment>)>,
    /// Files staged (drag & drop or the attach button) to send with the
    /// next prompt, shown as chips above the input box.
    pending_attachments: Vec<Attachment>,
    /// Counter naming the temp PNGs saved from clipboard image pastes.
    clip_count: usize,
    /// Latest context composition reported by the worker, shown by /status.
    context_info: Option<picocode_core::context::Breakdown>,
    /// Rolling generation-speed meter behind the status bar's tok/s.
    pub(super) speed: picocode_core::speed::SpeedMeter,
    /// Open right-click menu: (transcript entry index, click position).
    ctx_menu: Option<(usize, Point<Pixels>)>,
    /// Image opened full-size from a transcript thumbnail (click closes).
    image_preview: Option<std::path::PathBuf>,
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
        steer: picocode_core::steer::SteerQueue,
        jobs: picocode_core::tools::BackgroundJobs,
        cancel_tx: watch::Sender<()>,
        rt: tokio::runtime::Handle,
        mcp: picocode_core::mcp::McpConnections,
        backend: picocode_core::backend::Backend,
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
        let model_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("filter_models").to_string()));
        // Re-render the menu as the filter text changes.
        cx.subscribe_in(
            &model_filter,
            window,
            |_this: &mut Self, _, _: &InputEvent, _, cx| cx.notify(),
        )
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
        let theme_family = saved
            .theme_family
            .clone()
            .unwrap_or_else(|| crate::theme::FAMILIES[0].0.to_string());
        // The family itself is applied by theme::init once the registry
        // has loaded the bundled files; only the mode applies here.
        crate::theme::apply_mode(theme_pref, cx);

        let sessions_dir = session::sessions_dir_for(&cfg);
        let git_branch = picocode_core::git::branch(&cfg.root);
        let view = Self {
            cfg,
            entries: Vec::new(),
            input,
            event_tx,
            cmd_tx,
            steer,
            jobs,
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
            add_model: None,
            add_remote: None,
            prompt_edit: None,
            mcp,
            backend,
            model_filter,
            expanded_reasoning: std::collections::HashSet::new(),
            theme_pref,
            theme_family,
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
            pending_attachments: Vec::new(),
            clip_count: 0,
            context_info: None,
            speed: picocode_core::speed::SpeedMeter::default(),
            ctx_menu: None,
            image_preview: None,
            // The overdraw pre-measures entries near the viewport so
            // scrolling doesn't pop items in.
            list_state: ListState::new(0, ListAlignment::Bottom, px(512.)),
        };
        picocode_core::state::save_last_model(&view.cfg);
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

    /// Toggle one reasoning entry between the one-line preview and the
    /// full text (clicked in the transcript). Re-measures only that row,
    /// so the scroll position is preserved.
    pub(super) fn toggle_reasoning(&mut self, ix: usize) {
        if !self.expanded_reasoning.remove(&ix) {
            self.expanded_reasoning.insert(ix);
        }
        self.list_state.splice(ix..ix + 1, 1);
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
        crate::theme::apply_mode(self.theme_pref, cx);
    }

    /// Cycle the color-theme family and apply it (the appearance mode is
    /// untouched: the family only swaps the light/dark palette pair).
    fn cycle_theme_family(&mut self, delta: i64, cx: &mut Context<Self>) {
        self.theme_family = crate::theme::cycled(&self.theme_family, delta).to_string();
        self.saved.theme_family = Some(self.theme_family.clone());
        crate::theme::apply_family(&self.theme_family, cx);
    }

    /// A `/config` row change. Every change applies immediately. Mirrors
    /// the TUI's `/config` dialog, plus the GUI-only theme row.
    fn adjust_setting(
        &mut self,
        row: usize,
        delta: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match row {
            0 => self.cycle_theme(delta, cx),
            1 => self.cycle_theme_family(delta, cx),
            // Same cycle as the TUI: bypass stays menu/command-only, and
            // adjusting away from it lands on read-only.
            2 => self.cfg.mode.set(self.cfg.mode.get().cycled(delta)),
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
                self.toggle_menu(Menu::Model, window, cx);
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
                attachments: Vec::new(),
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
            attachments: Vec::new(),
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

    fn dialog_open(&self) -> bool {
        self.approval.is_some()
            || self.question.is_some()
            || self.session_picker.is_some()
            || self.settings_open
            || self.add_model.is_some()
            || self.prompt_edit.is_some()
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

        // `!<command>`: direct shell, like the TUI. While a turn runs it
        // joins the held-back queue and executes when the turn ends.
        if text.starts_with('!') {
            if self.running {
                self.queued.push((text, Vec::new()));
            } else {
                self.run_shell(text);
            }
            self.scroll_to_bottom();
            cx.notify();
            return;
        }
        use picocode_core::command::{Command, JobsAction, ParseOutcome};
        match picocode_core::command::parse(&text) {
            // Mid-turn text goes through the steering queue: the worker
            // injects it at the next tool-call boundary (or runs it as a
            // follow-up prompt of the same turn). Messages with attachments
            // can't ride a tool result, so those are held back and sent
            // one per completed turn as before.
            ParseOutcome::Prompt if self.running => {
                let attachments = std::mem::take(&mut self.pending_attachments);
                if attachments.is_empty() {
                    self.push(EntryKind::User, text.clone());
                    self.steer.push(text);
                } else {
                    self.queued.push((text, attachments));
                }
            }
            ParseOutcome::Prompt => {
                let attachments = std::mem::take(&mut self.pending_attachments);
                self.send_prompt(text, attachments);
            }
            ParseOutcome::Unknown { name } => {
                self.push(
                    EntryKind::Error,
                    t!("unknown_command", name = name).to_string(),
                );
            }
            ParseOutcome::Invalid { message } => {
                self.push(EntryKind::Error, message);
            }
            ParseOutcome::Command(command) => match command {
                Command::Clear => {
                    let _ = self.cmd_tx.try_send(WorkerCmd::Clear);
                    self.entries.clear();
                    self.queued.clear();
                    self.pending_attachments.clear();
                    self.context_info = None;
                    self.tokens_in = 0;
                    self.tokens_out = 0;
                    self.est_out = 0;
                    // A cleared conversation starts a fresh session log.
                    self.session_id = session::new_id();
                    self.expanded_reasoning.clear();
                    self.push(EntryKind::Notice, t!("cleared").to_string());
                    self.reset_list();
                }
                Command::Compact => {
                    let _ = self.cmd_tx.try_send(WorkerCmd::Compact);
                    self.push(EntryKind::Notice, t!("compacting").to_string());
                    self.running = true;
                    self.waiting = true;
                }
                Command::Undo => {
                    let _ = self.cmd_tx.try_send(WorkerCmd::Undo);
                }
                Command::Jobs(JobsAction::List) => self.toggle_menu(Menu::Background, window, cx),
                Command::Jobs(JobsAction::Kill(id)) => {
                    if !self.jobs.kill(id) {
                        self.push(EntryKind::Error, t!("bg_no_job", id = id).to_string());
                    } else {
                        self.push(EntryKind::Notice, t!("bg_killed", id = id).to_string());
                    }
                }
                Command::Quit => cx.quit(),
                Command::Mode(mode) => self.select_mode(mode, cx),
                Command::Model(None) => self.toggle_menu(Menu::Model, window, cx),
                Command::Model(Some(name)) => self.switch_model(&name, cx),
                Command::Resume(None) => self.open_session_picker(cx),
                Command::Resume(Some(id)) => self.resume_session(&id, cx),
                Command::Attach(None) => self.show_attachments(),
                Command::Attach(Some(arg)) => self.attach_command(&arg, cx),
                Command::SystemPrompt(action) => {
                    use picocode_core::command::PromptAction;
                    match action {
                        PromptAction::Edit => self.open_prompt_editor(window, cx),
                        PromptAction::Reset => self.apply_system_prompt(None, cx),
                        PromptAction::Preset(name) => self.apply_prompt_preset(&name, cx),
                    }
                }
                Command::Remote(None) => self.toggle_menu(Menu::Workspace, window, cx),
                Command::Remote(Some(target)) => self.switch_workspace(&target, cx),
                Command::Config => self.settings_open = true,
                Command::Status => self.show_status(),
                Command::Permissions => self.show_permissions(),
            },
        }
        self.scroll_to_bottom();
        cx.notify();
    }

    /// Record a user prompt in the transcript and hand it to the worker.
    pub fn send_prompt(&mut self, text: String, attachments: Vec<Attachment>) {
        self.push_entry(Entry {
            kind: EntryKind::User,
            text: text.clone(),
            lang: None,
            attachments: attachments
                .iter()
                .map(|a| a.path.display().to_string())
                .collect(),
        });
        let _ = self
            .cmd_tx
            .try_send(WorkerCmd::Prompt { text, attachments });
        self.running = true;
        self.waiting = true;
    }

    /// Send the oldest held-back prompt, if any. Called when a turn ends;
    /// one prompt per turn, so each gets its own response.
    fn flush_queued(&mut self) {
        if self.queued.is_empty() {
            return;
        }
        let (text, attachments) = self.queued.remove(0);
        if text.starts_with('!') {
            self.run_shell(text);
        } else {
            self.send_prompt(text, attachments);
        }
    }

    /// `!<command>`: run a shell command directly (no model, no approval —
    /// the user typed it). Mirrors the TUI: the output is shown in the
    /// transcript and recorded in the model's history so the next prompt
    /// can refer to it. Ends with a `TurnComplete` through the regular
    /// event pump, which resets `running` and flushes the queue.
    pub fn run_shell(&mut self, text: String) {
        let command = text[1..].trim().to_string();
        if command.is_empty() {
            self.push(EntryKind::Error, t!("shell_empty").to_string());
            return;
        }
        self.push(EntryKind::User, text);
        self.running = true;
        self.waiting = false;

        // User-typed `!` commands run unsandboxed by design.
        self.rt.spawn(picocode_core::tools::user_shell(
            picocode_core::backend::Workspace {
                backend: self.backend.clone(),
                root: self.cfg.root.clone(),
            },
            self.cfg.bash_timeout.clone(),
            self.event_tx.clone(),
            self.cmd_tx.clone(),
            self.jobs.clone(),
            self.cancel_tx.subscribe(),
            command,
            "(stopped before finishing)",
        ));
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

    /// Kill a background job from the jobs popup. Its wrapper still
    /// reports a completion, which balances the status-bar counter.
    pub(super) fn kill_job(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.jobs.kill(id) {
            self.push(EntryKind::Notice, t!("bg_killed", id = id).to_string());
        }
        cx.notify();
    }

    fn toggle_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu == Some(menu) {
            self.menu = None;
        } else {
            self.menu = Some(menu);
            if menu == Menu::Model {
                self.refresh_models();
                // A fresh search per open; focus it so typing filters
                // immediately.
                self.model_filter.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                    state.focus(window, cx);
                });
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
            .on_action(cx.listener(Self::on_paste_clipboard))
            // Files dragged from the OS anywhere onto the window become
            // staged attachments for the next prompt.
            .on_drop::<gpui::ExternalPaths>(cx.listener(
                |this, paths: &gpui::ExternalPaths, _, cx| {
                    this.add_attachments(paths.paths(), cx);
                },
            ))
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
                    .children(self.render_attachments(cx))
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
                                    .child(self.workdir_label())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_menu(Menu::Workspace, window, cx)
                                    })),
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
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .icon(
                                        gpui_component::Icon::default().path("icons/paperclip.svg"),
                                    )
                                    .tooltip(t!("attach_tooltip").to_string())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.pick_attachments(cx)),
                                    ),
                            )
                            .child(div().flex_1().child({
                                // A leading `!` means a direct shell command
                                // and a leading `/` a slash command; recolor
                                // the input's border so the mode is obvious
                                // while typing (mirrors the TUI's palette).
                                let input = Input::new(&self.input);
                                let value = self.input.read(cx).value();
                                if value.starts_with('!') {
                                    input.border_color(cx.theme().yellow)
                                } else if value.starts_with('/') {
                                    input.border_color(cx.theme().cyan)
                                } else {
                                    input
                                }
                            }))
                            .child(send_or_stop),
                    ),
            )
            .child(self.render_status_bar(cx))
            .children(self.render_ctx_menu(window, cx))
            .children(self.render_menu(cx))
            .children(self.render_settings(cx))
            .children(self.render_add_model(cx))
            .children(self.render_add_remote(cx))
            .children(self.render_prompt_edit(cx))
            .children(self.render_session_picker(cx))
            .children(self.render_approval(cx))
            .children(self.render_question(cx))
            .children(self.render_image_preview(cx))
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
