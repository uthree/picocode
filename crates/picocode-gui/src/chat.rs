//! The chat view: transcript, input row, status bar, and the approval /
//! question dialogs — a gpui rendering of picocode-core's event stream.

use gpui::prelude::*;
use gpui::{AnyElement, ClickEvent, Context, Entity, ScrollHandle, SharedString, Window, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, StyledExt};
use tokio::sync::{mpsc, oneshot, watch};

use std::path::PathBuf;

use picocode_core::config::{self, Config, Mode};
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::transcript::{Entry, EntryKind, diff_lines};
use picocode_core::{agent, approval, models, session, state};

const TOOL_OUTPUT_MAX_LINES: usize = 12;
const DIFF_MAX_LINES: usize = 30;

gpui::actions!(picocode_gui, [AcceptCompletion]);

/// Slash commands the GUI supports, with a short description for the
/// completion popup.
const COMMANDS: &[(&str, &str)] = &[
    ("/clear", "Clear conversation history"),
    ("/compact", "Summarize history to free context"),
    ("/model", "Pick a model (menu) or switch: /model <name>"),
    ("/resume", "Pick a saved session to resume"),
    ("/read-only", "Mode: reads only, every write asks"),
    ("/edit", "Mode: file writes run freely"),
    ("/plan", "Mode: investigate and plan, writes blocked"),
    (
        "/bypass",
        "Mode: run EVERYTHING unconfirmed (isolated envs)",
    ),
    ("/permissions", "Show the effective permission rules"),
    ("/config", "Edit settings in a dialog"),
    ("/settings", "Alias of /config"),
    ("/status", "Show model, token usage and session info"),
    ("/usage", "Alias of /status"),
    ("/quit", "Exit picocode"),
    ("/exit", "Exit picocode"),
];

/// Diff row backgrounds (translucent, so they read on both themes).
const DIFF_ADD_BG: u32 = 0x3fb95033;
const DIFF_DEL_BG: u32 = 0xf8514933;

/// Which status-bar popup menu is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Mode,
    Model,
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
    /// Completion prefix locked at the first Tab press, so cycling keeps
    /// the full candidate list even after the input holds a full match.
    comp_prefix: Option<String>,
    /// Set while Tab fills the input, so the resulting Change event doesn't
    /// reset `comp_prefix`.
    completing: bool,
    /// Context tokens of the last completion request / output tokens so far.
    tokens_in: u64,
    tokens_out: u64,
    scroll: ScrollHandle,
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
                .placeholder("Type a message — Enter to send, Shift+Enter for a newline")
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.subscribe_in(&input, window, Self::on_input_event)
            .detach();

        // Pump agent events from the tokio channel into this view. tokio's
        // mpsc receiver is executor-agnostic, so awaiting it on gpui's
        // executor is fine.
        cx.spawn(async move |this, cx| {
            while let Some(ev) = event_rx.recv().await {
                let alive = this.update(cx, |view, cx| {
                    view.on_agent_event(ev, cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let sessions_dir = session::sessions_dir(&cfg.root);
        let view = Self {
            cfg,
            entries: Vec::new(),
            input,
            event_tx,
            cmd_tx,
            cancel_tx,
            rt,
            running: false,
            approval: None,
            question: None,
            menu: None,
            available_models: Vec::new(),
            session_id: session::new_id(),
            sessions_dir,
            session_picker: None,
            settings_open: false,
            show_reasoning: true,
            comp_prefix: None,
            completing: false,
            tokens_in: 0,
            tokens_out: 0,
            scroll: ScrollHandle::new(),
        };
        view.save_last_model();
        view
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

    fn on_agent_event(&mut self, ev: AgentEvent, _cx: &mut Context<Self>) {
        // Follow the stream only while the view is already at the bottom —
        // scrolling up pins the position (like the TUI), and scrolling back
        // down resumes following.
        let follow = self.is_scrolled_to_bottom();
        match ev {
            AgentEvent::TextDelta(s) => self.append(EntryKind::Assistant, &s),
            AgentEvent::ReasoningDelta(s) => self.append(EntryKind::Reasoning, &s),
            AgentEvent::ToolCall { name, args } => self.push_tool_call(&name, &args),
            AgentEvent::ToolResult { output } => {
                self.push(EntryKind::ToolOut, clip(&output, TOOL_OUTPUT_MAX_LINES));
            }
            AgentEvent::ApprovalRequest {
                name,
                args,
                respond,
            } => {
                self.approval = Some(Approval {
                    name,
                    args,
                    respond,
                });
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
            }
            AgentEvent::Usage { input, output } => {
                self.tokens_in = input;
                self.tokens_out = output;
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
                            format!("Could not list models on {label}: {e}"),
                        );
                    }
                }
            },
            AgentEvent::Compacted { messages, summary } => {
                if messages == 0 {
                    self.push(EntryKind::Notice, "nothing to compact".to_string());
                } else {
                    self.push(EntryKind::Notice, format!("compacted {messages} messages"));
                    self.push(EntryKind::Summary, summary);
                }
                self.running = false;
                self.autosave();
            }
            AgentEvent::ShellOutput { output } => self.push(EntryKind::ToolOut, output),
            AgentEvent::BackgroundStarted { id } => {
                self.push(
                    EntryKind::Notice,
                    format!("bash timed out — moved to background job #{id}"),
                );
            }
            AgentEvent::BackgroundDone {
                id,
                command,
                output,
            } => {
                self.push(EntryKind::Notice, format!("background job #{id} finished"));
                self.push(EntryKind::ToolOut, clip(&output, TOOL_OUTPUT_MAX_LINES));
                // Prompt the model with the result so it reacts to it, like
                // the TUI does.
                let prompt =
                    format!("[background job #{id} finished] `{command}` output:\n{output}");
                if self.cmd_tx.try_send(WorkerCmd::Prompt(prompt)).is_ok() {
                    self.running = true;
                }
            }
            AgentEvent::Cancelled => self.push(EntryKind::Notice, "cancelled".to_string()),
            AgentEvent::TurnComplete => {
                self.running = false;
                self.autosave();
            }
            AgentEvent::Error(e) => self.push(EntryKind::Error, e),
        }
        if follow {
            self.scroll.scroll_to_bottom();
        }
    }

    /// Whether the transcript is scrolled to (within a few pixels of) the
    /// bottom. Scroll offsets grow negative downwards, so the bottom sits at
    /// `-max_offset`; a fresh, unscrolled view reports 0/0 and counts as at
    /// the bottom.
    fn is_scrolled_to_bottom(&self) -> bool {
        let max = self.scroll.max_offset().height;
        self.scroll.offset().y <= -max + px(4.)
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
            self.push(
                EntryKind::Error,
                "Cannot resume while a turn is running".to_string(),
            );
            cx.notify();
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(
                EntryKind::Error,
                "Session storage is unavailable (no $HOME)".to_string(),
            );
            cx.notify();
            return;
        };
        // Resuming the current session would be a no-op, so it isn't offered.
        let sessions: Vec<_> = session::list(&dir)
            .into_iter()
            .filter(|s| s.id != self.session_id)
            .collect();
        if sessions.is_empty() {
            self.push(
                EntryKind::Notice,
                "No saved sessions for this project yet".to_string(),
            );
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
            self.push(
                EntryKind::Error,
                "Cannot resume while a turn is running".to_string(),
            );
            cx.notify();
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(
                EntryKind::Error,
                "Session storage is unavailable (no $HOME)".to_string(),
            );
            cx.notify();
            return;
        };
        if id == self.session_id {
            self.push(EntryKind::Notice, "That is the current session".to_string());
            cx.notify();
            return;
        }

        let saved = match session::load(&dir, id) {
            Ok(s) => s,
            Err(e) => {
                self.push(EntryKind::Error, format!("Failed to resume: {e:#}"));
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
            self.push(EntryKind::Error, "The agent worker has stopped".to_string());
            cx.notify();
            return;
        }

        self.entries.clear();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.push(
            EntryKind::Notice,
            format!(
                "Resumed session {id} — {messages} messages, last saved with {}",
                saved.model
            ),
        );
        self.entries.extend(saved.entries);
        self.session_id = id.to_string();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// `/status` (alias `/usage`): one-shot overview of the model, token
    /// usage, permission mode and session.
    fn show_status(&mut self) {
        let entry = match &self.cfg.active_model {
            Some(name) => format!(" — [[models]] entry `{name}`"),
            None => String::new(),
        };
        let endpoint = models::base_url(self.cfg.provider, self.cfg.base_url.as_deref());
        let pct = (self.context_ratio() * 100.0).round() as u64;
        let prompts = self
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::User)
            .count();
        let saved = match &self.sessions_dir {
            Some(dir) => format!("autosaved under {}", dir.display()),
            None => "not saved (no home directory)".to_string(),
        };
        let config = if self.cfg.config_files.is_empty() {
            "(built-in defaults)".to_string()
        } else {
            self.cfg.config_files.join(", ")
        };
        let instructions = if self.cfg.instructions.is_empty() {
            "(none found)".to_string()
        } else {
            let names: Vec<&str> = self
                .cfg
                .instructions
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();
            names.join(", ")
        };
        self.push(
            EntryKind::Notice,
            format!(
                "Status\n\
                 model         {}{entry}\n\
                 endpoint      {endpoint}\n\
                 mode          {} — /permissions shows the rules\n\
                 context       {} of {} tokens ({pct}%)\n\
                 output        {} tokens (last reported)\n\
                 session       {} — {prompts} prompts, {saved}\n\
                 project       {}\n\
                 config        {config}\n\
                 instructions  {instructions}",
                self.cfg.model_label(),
                self.cfg.mode.get().label(),
                self.tokens_in,
                self.cfg.context_window,
                self.tokens_out,
                self.session_id,
                self.cfg.root.display(),
            ),
        );
    }

    /// `/permissions`: show what the current mode and config rules do.
    fn show_permissions(&mut self) {
        let mode = self.cfg.mode.get();
        let mode_line = match mode {
            Mode::ReadOnly => "destructive calls ask unless allow-listed",
            Mode::Edit => "file writes run freely; other destructive calls ask unless allow-listed",
            Mode::Plan => "bash and file writes are denied; web tools ask unless allow-listed",
            Mode::Bypass => "EVERYTHING runs without confirmation (deny rules still apply)",
        };
        let rules = self.cfg.approval.snapshot();
        let list = |xs: &[String]| {
            if xs.is_empty() {
                "(none)".to_string()
            } else {
                xs.join(", ")
            }
        };
        self.push(
            EntryKind::Notice,
            format!(
                "Permissions — precedence: deny > mode (plan/bypass) > allow > ask\n\
                 mode         [{}] {mode_line}\n\
                 deny_tools   {}\n\
                 deny_bash    {}\n\
                 allow_tools  {}\n\
                 allow_bash   {}\n\
                 Local reads (read_file, list_files, grep) always run; file tools are \
                 confined to {}. Commands with $( ), backticks or > never auto-run.",
                mode.label(),
                list(&rules.deny_tools),
                list(&rules.deny_bash),
                list(&rules.allow_tools),
                list(&rules.allow_bash),
                self.cfg.root.display(),
            ),
        );
    }

    /// A `/config` row change. Every change applies immediately (max turns
    /// from the next prompt on). Mirrors the TUI's `/config` dialog.
    fn adjust_setting(&mut self, row: usize, delta: i64, cx: &mut Context<Self>) {
        match row {
            // Same cycle as the TUI: bypass stays menu/command-only, and
            // adjusting away from it lands on read-only.
            0 => {
                let cycle = Mode::CYCLE;
                let next = match cycle.iter().position(|m| *m == self.cfg.mode.get()) {
                    Some(i) if delta < 0 => cycle[(i + cycle.len() - 1) % cycle.len()],
                    Some(i) => cycle[(i + 1) % cycle.len()],
                    None => cycle[0],
                };
                self.cfg.mode.set(next);
            }
            1 => self.show_reasoning = !self.show_reasoning,
            2 => {
                let turns = self.cfg.max_turns.get() as i64 + delta * 10;
                self.cfg.max_turns.set(turns.clamp(10, 200) as u64);
            }
            3 => {
                let secs = self.cfg.bash_timeout.get() as i64 + delta * 30;
                self.cfg.bash_timeout.set(secs.clamp(30, 1800) as u64);
            }
            4 => {
                let lines = self.cfg.read_max_lines.get() as i64 + delta * 500;
                self.cfg.read_max_lines.set(lines.clamp(500, 10_000) as u64);
            }
            5 => {
                let bytes = self.cfg.read_max_line_bytes.get() as i64 + delta * 100;
                self.cfg
                    .read_max_line_bytes
                    .set(bytes.clamp(100, 5000) as u64);
            }
            6 => self.cfg.search.cycle_provider(delta),
            7 => {
                let n = self.cfg.search.snapshot().max_results as i64 + delta;
                self.cfg.search.set_max_results(n.clamp(1, 20) as usize);
            }
            // ±5% between 50 and 95; stepping below 50 turns it off.
            8 => {
                let cur = self.cfg.auto_compact.get() as i64;
                let next = if delta < 0 {
                    if cur <= 50 { 0 } else { cur - 5 }
                } else if cur == 0 {
                    50
                } else {
                    (cur + 5).min(95)
                };
                self.cfg.auto_compact.set(next as u64);
            }
            // Model: close the dialog and open the model menu.
            9 => {
                self.settings_open = false;
                self.toggle_menu(Menu::Model, cx);
            }
            _ => {}
        }
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
            self.entries.push(Entry {
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
        self.entries.push(Entry {
            kind,
            text,
            lang: None,
        });
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
            // Plain Enter submits; a secondary Enter (Shift+Enter, Cmd+Enter)
            // keeps the newline the multi-line input just inserted.
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
        // Clear even when only whitespace remains — the multi-line input
        // inserts the newline before PressEnter arrives, and empty Enters
        // must not accumulate blank lines.
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        if text.is_empty() {
            return;
        }

        match text.as_str() {
            "/clear" => {
                let _ = self.cmd_tx.try_send(WorkerCmd::Clear);
                self.entries.clear();
                self.tokens_in = 0;
                self.tokens_out = 0;
                // A cleared conversation starts a fresh session log.
                self.session_id = session::new_id();
                self.push(EntryKind::Notice, "conversation cleared".to_string());
            }
            "/compact" => {
                let _ = self.cmd_tx.try_send(WorkerCmd::Compact);
                self.push(EntryKind::Notice, "compacting…".to_string());
                self.running = true;
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
                    format!("{text}: not available in the GUI (yet) — try the TUI"),
                );
            }
            _ if self.running => {
                self.push(
                    EntryKind::Notice,
                    "still running — wait for the turn to finish or press Stop".to_string(),
                );
            }
            _ => self.send_prompt(text),
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Record a user prompt in the transcript and hand it to the worker.
    pub fn send_prompt(&mut self, text: String) {
        self.push(EntryKind::User, text.clone());
        let _ = self.cmd_tx.try_send(WorkerCmd::Prompt(text));
        self.running = true;
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
            self.push(
                EntryKind::Warning,
                "bypass mode: EVERY tool call now runs without confirmation (deny rules \
                 still apply). Meant for isolated environments such as containers."
                    .to_string(),
            );
        } else {
            self.push(EntryKind::Notice, format!("Mode: {}", mode.label()));
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
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            cx.notify();
            return;
        }
        let mut new_cfg = self.cfg.clone();
        match self.cfg.models.iter().find(|m| m.name == name) {
            Some(entry) => {
                if self.cfg.active_model.as_deref() == Some(name) {
                    self.push(
                        EntryKind::Notice,
                        format!("Already using {name} ({})", entry.label()),
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
                        format!("Already using {}", self.cfg.model_label()),
                    );
                    cx.notify();
                    return;
                }
                new_cfg.model = name.to_string();
                new_cfg.active_model = None;
                new_cfg.context_window = config::DEFAULT_CONTEXT_WINDOW;
            }
            None => {
                self.push(EntryKind::Error, format!("Unknown model `{name}`"));
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
                        format!("Failed to switch to `{name}`: {e:#}"),
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
            format!("Model switched to {name} ({})", self.cfg.model_label()),
        );
        if endpoint_changed {
            self.available_models.clear();
            self.refresh_models();
        }
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

    fn answer_approval(&mut self, approve: bool, always: bool, cx: &mut Context<Self>) {
        let Some(a) = self.approval.take() else {
            return;
        };
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

    fn answer_question(&mut self, answer: Option<usize>, cx: &mut Context<Self>) {
        if let Some(q) = self.question.take() {
            let _ = q.respond.send(answer);
        }
        cx.notify();
    }

    // ---------- rendering ----------

    fn render_entry(
        entry: &Entry,
        ix: usize,
        show_reasoning: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        match entry.kind {
            EntryKind::User => div()
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(theme.muted)
                .border_1()
                .border_color(theme.border)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Assistant => TextView::markdown(
                SharedString::from(format!("md-{ix}")),
                // TeX math spans become Unicode before rendering.
                SharedString::from(crate::math::render_math(&entry.text)),
                window,
                cx,
            )
            .into_any_element(),
            EntryKind::Reasoning => div()
                .italic()
                .text_sm()
                .text_color(muted)
                .child(if show_reasoning {
                    entry.text.clone()
                } else {
                    // Collapsed (the `/config` "reasoning" row): one line.
                    format!("✳ {}", one_line(&entry.text, 80))
                })
                .into_any_element(),
            EntryKind::Tool => div()
                .font_family(mono)
                .text_sm()
                .text_color(muted)
                .child(format!("⚙ {}", entry.text))
                .into_any_element(),
            EntryKind::ToolOut => div()
                .font_family(mono)
                .text_sm()
                .text_color(muted)
                .pl_4()
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Diff => div()
                .pl_4()
                .child(diff_element(&entry.text, mono))
                .into_any_element(),
            EntryKind::Notice | EntryKind::Logo => div()
                .text_sm()
                .text_color(muted)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Warning => div()
                .text_sm()
                .text_color(theme.warning)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Summary => div()
                .px_3()
                .py_2()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .text_sm()
                .text_color(muted)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Error => div()
                .text_sm()
                .text_color(theme.danger)
                .child(entry.text.clone())
                .into_any_element(),
        }
    }

    fn render_approval(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let a = self.approval.as_ref()?;
        let theme = cx.theme();
        let always_label = match approval::bash_command(&a.name, &a.args) {
            Some(cmd) => format!(
                "Always ({} …)",
                config::bash_allow_patterns(&cmd).join(", ")
            ),
            None => format!("Always ({})", a.name),
        };
        Some(
            overlay()
                .child(
                    div()
                        .v_flex()
                        .w(px(560.))
                        .max_h(px(420.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child(format!("Run {}?", a.name)))
                        .child(
                            div()
                                .id("approval-args")
                                .flex_1()
                                .overflow_y_scroll()
                                .text_sm()
                                .child(approval_body(
                                    a,
                                    theme.mono_font_family.clone(),
                                    theme.muted_foreground,
                                )),
                        )
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .justify_end()
                                .child(Button::new("deny").label("Deny").on_click(cx.listener(
                                    |this, _, _, cx| this.answer_approval(false, false, cx),
                                )))
                                .child(Button::new("always").label(always_label).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.answer_approval(true, true, cx)
                                    }),
                                ))
                                .child(Button::new("approve").primary().label("Approve").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.answer_approval(true, false, cx)
                                    }),
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_question(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let q = self.question.as_ref()?;
        let theme = cx.theme();
        let options = q.options.iter().enumerate().map(|(ix, opt)| {
            Button::new(SharedString::from(format!("opt-{ix}")))
                .label(opt.clone())
                .on_click(cx.listener(move |this, _, _, cx| this.answer_question(Some(ix), cx)))
                .into_any_element()
        });
        Some(
            overlay()
                .child(
                    div()
                        .v_flex()
                        .w(px(560.))
                        .max_h(px(480.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child(q.title.clone()))
                        .child(
                            div()
                                .id("question-body")
                                .flex_1()
                                .overflow_y_scroll()
                                .text_sm()
                                .child(q.question.clone()),
                        )
                        .child(div().v_flex().gap_2().children(options))
                        .child(div().h_flex().justify_end().child(
                            Button::new("dismiss").label("Dismiss").on_click(
                                cx.listener(|this, _, _, cx| this.answer_question(None, cx)),
                            ),
                        )),
                )
                .into_any_element(),
        )
    }

    /// Completion popup: matching slash commands, shown above the input
    /// while it holds a bare `/command` prefix. Click fills; Tab cycles.
    fn render_completions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.dialog_open() {
            return None;
        }
        let value = self.input.read(cx).value().to_string();
        if !value.starts_with('/') || value.contains(char::is_whitespace) {
            return None;
        }
        let prefix = self.comp_prefix.clone().unwrap_or_else(|| value.clone());
        let matches: Vec<(&str, &str)> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(&prefix))
            .copied()
            .collect();
        if matches.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let mut list = div()
            .id("completions")
            .v_flex()
            .max_h(px(240.))
            .overflow_y_scroll();
        for (name, desc) in matches {
            let fill = name.to_string();
            let active = name == value;
            list = list.child(
                div()
                    .id(SharedString::from(format!("comp-{name}")))
                    .cursor_pointer()
                    .h_flex()
                    .justify_between()
                    .gap_4()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .when(active, |s| s.bg(theme.muted))
                    .hover(|s| s.bg(theme.muted))
                    .child(div().font_family(mono.clone()).child(name))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(desc),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.completing = true;
                        this.input
                            .update(cx, |state, cx| state.set_value(&fill, window, cx));
                        cx.notify();
                    })),
            );
        }
        Some(
            div()
                .mx_3()
                .p_1()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .text_sm()
                .child(list)
                .into_any_element(),
        )
    }

    /// The `/resume` dialog: this project's saved sessions, newest first.
    fn render_session_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sessions = self.session_picker.as_ref()?;
        let theme = cx.theme();
        let mut list = div()
            .id("session-picker-list")
            .v_flex()
            .gap_1()
            .overflow_y_scroll();
        for (ix, s) in sessions.iter().enumerate() {
            let id = s.id.clone();
            let title = if s.snippet.is_empty() {
                id.clone()
            } else {
                s.snippet.clone()
            };
            let detail = format!(
                "{} · {} messages · {}",
                session::age(s.modified),
                s.messages,
                s.model
            );
            list = list.child(
                menu_row(
                    SharedString::from(format!("session-{ix}")),
                    title,
                    detail,
                    false,
                    theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.resume_session(&id.clone(), cx))),
            );
        }
        Some(
            overlay()
                .child(
                    div()
                        .v_flex()
                        .w(px(560.))
                        .max_h(px(480.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child("Resume a session"))
                        .child(list)
                        .child(
                            div().h_flex().justify_end().child(
                                Button::new("resume-cancel")
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.session_picker = None;
                                        cx.notify();
                                    })),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The `/config` dialog: the same rows as the TUI's settings dialog,
    /// adjusted with −/+ buttons; every change applies immediately.
    fn render_settings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings_open {
            return None;
        }
        let theme = cx.theme();
        let search = self.cfg.search.snapshot();
        let rows: [(&str, String); 10] = [
            ("mode", self.cfg.mode.get().label().to_string()),
            (
                "reasoning",
                if self.show_reasoning {
                    "shown".to_string()
                } else {
                    "collapsed".to_string()
                },
            ),
            ("max turns", self.cfg.max_turns.get().to_string()),
            ("bash timeout", format!("{}s", self.cfg.bash_timeout.get())),
            ("read lines", self.cfg.read_max_lines.get().to_string()),
            ("line bytes", self.cfg.read_max_line_bytes.get().to_string()),
            ("web search", search.provider.label().to_string()),
            ("results", search.max_results.to_string()),
            (
                "auto-compact",
                match self.cfg.auto_compact.get() {
                    0 => "off".to_string(),
                    pct => format!("{pct}%"),
                },
            ),
            ("model", self.cfg.model_label()),
        ];

        let mut panel = div().v_flex().gap_1();
        for (ix, (label, value)) in rows.into_iter().enumerate() {
            // The model row is a single button opening the model menu; the
            // others adjust in place with −/+.
            let controls: AnyElement = if ix == 9 {
                Button::new("cfg-model")
                    .label(value)
                    .on_click(cx.listener(move |this, _, _, cx| this.adjust_setting(9, 1, cx)))
                    .into_any_element()
            } else {
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new(SharedString::from(format!("cfg-dec-{ix}")))
                            .ghost()
                            .label("−")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.adjust_setting(ix, -1, cx)),
                            ),
                    )
                    .child(div().min_w(px(110.)).text_center().child(value))
                    .child(
                        Button::new(SharedString::from(format!("cfg-inc-{ix}")))
                            .ghost()
                            .label("+")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.adjust_setting(ix, 1, cx)),
                            ),
                    )
                    .into_any_element()
            };
            panel = panel.child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_1()
                    .child(label.to_string())
                    .child(controls),
            );
        }

        Some(
            overlay()
                .child(
                    div()
                        .v_flex()
                        .w(px(460.))
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().font_bold().child("Settings"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("Changes apply immediately, for this session only."),
                        )
                        .child(panel)
                        .child(
                            div().h_flex().justify_end().child(
                                Button::new("settings-close")
                                    .primary()
                                    .label("Close")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_open = false;
                                        cx.notify();
                                    })),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Fraction of the model's context window used by the latest request.
    fn context_ratio(&self) -> f64 {
        self.tokens_in as f64 / self.cfg.context_window.max(1) as f64
    }

    /// Status bar, matching the TUI's layout: the clickable mode chip and
    /// the activity state on the left; the context gauge and the clickable
    /// model chip on the right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let mode = self.cfg.mode.get();

        // Context usage gauge, colored by pressure (same thresholds as the
        // TUI: red ≥ 85%, yellow ≥ 60%).
        let ratio = self.context_ratio().min(1.0);
        let gauge_color = if ratio >= 0.85 {
            theme.danger
        } else if ratio >= 0.6 {
            theme.warning
        } else {
            gpui::rgb(0x3fb950).into()
        };
        const GAUGE_W: f32 = 96.;
        let gauge = div()
            .h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .w(px(GAUGE_W))
                    .h(px(6.))
                    .rounded_full()
                    .bg(theme.muted)
                    .child(
                        div()
                            .w(px(GAUGE_W * ratio as f32))
                            .h_full()
                            .rounded_full()
                            .bg(gauge_color),
                    ),
            )
            .child(format!(
                "{}%  ↑ {} ↓ {}",
                (ratio * 100.0).round() as u64,
                self.tokens_in,
                self.tokens_out
            ));

        let state = if self.running {
            "● running"
        } else {
            "● idle"
        };

        let mode_chip = div()
            .id("mode-chip")
            .cursor_pointer()
            .rounded_md()
            .px_2()
            .text_color(mode_color(mode))
            .hover(|s| s.bg(theme.muted))
            .child(format!("[{}]", mode.label()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Mode, cx)));
        let model_chip = div()
            .id("model-chip")
            .cursor_pointer()
            .rounded_md()
            .px_2()
            .hover(|s| s.bg(theme.muted))
            .child(self.cfg.model_label())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Model, cx)));

        div()
            .h_flex()
            .justify_between()
            .px_3()
            .pb_2()
            .text_sm()
            .text_color(muted_fg)
            .child(div().h_flex().gap_3().child(mode_chip).child(state))
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .items_center()
                    .child(gauge)
                    .child(model_chip),
            )
            .into_any_element()
    }

    /// The open status-bar menu (mode or model picker), anchored above its
    /// chip — mode bottom-left, model bottom-right — with a click-away
    /// backdrop.
    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu?;
        let theme = cx.theme();
        let current_mode = self.cfg.mode.get();

        let mut panel = div()
            .v_flex()
            .w(px(340.))
            .max_h(px(360.))
            .p_1()
            .gap_1()
            .rounded_lg()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .shadow_lg();
        match menu {
            Menu::Mode => {
                for (mode, desc) in [
                    (Mode::ReadOnly, "destructive calls ask"),
                    (Mode::Edit, "file edits run without asking"),
                    (Mode::Plan, "investigate only; submits a plan"),
                    (Mode::Bypass, "EVERYTHING runs unconfirmed"),
                ] {
                    let active = mode == current_mode;
                    panel = panel.child(
                        menu_row(
                            SharedString::from(format!("mode-{}", mode.label())),
                            mode.label(),
                            desc.to_string(),
                            active,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.select_mode(mode, cx))),
                    );
                }
            }
            Menu::Model => {
                let choices = models::model_choices(
                    &self.cfg.models,
                    self.cfg.active_model.as_deref(),
                    self.cfg.provider,
                    self.cfg.base_url.as_deref(),
                    &self.cfg.model,
                    &self.available_models,
                );
                if choices.is_empty() {
                    panel = panel.child(
                        div()
                            .p_2()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Fetching the provider's model list…"),
                    );
                }
                let mut list = div()
                    .id("model-menu-list")
                    .v_flex()
                    .gap_1()
                    .overflow_y_scroll();
                for (ix, choice) in choices.iter().enumerate() {
                    let name = choice.name.clone();
                    list = list.child(
                        menu_row(
                            SharedString::from(format!("model-{ix}")),
                            &choice.name,
                            choice.detail.clone(),
                            choice.active,
                            theme,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.switch_model(&name.clone(), cx)),
                        ),
                    );
                }
                panel = panel.child(list);
            }
        }

        Some(
            div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .id("menu-backdrop")
                        .absolute()
                        .inset_0()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            cx.notify();
                        })),
                )
                .child({
                    let anchored = div().absolute().bottom(px(36.)).occlude();
                    match menu {
                        Menu::Mode => anchored.left(px(12.)),
                        Menu::Model => anchored.right(px(12.)),
                    }
                    .child(panel)
                })
                .into_any_element(),
        )
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let border = theme.border;
        let muted_fg = theme.muted_foreground;

        let show_reasoning = self.show_reasoning;
        let mut items: Vec<AnyElement> = Vec::new();
        for (ix, entry) in self.entries.iter().enumerate() {
            items.push(Self::render_entry(entry, ix, show_reasoning, window, cx));
        }
        if items.is_empty() {
            items.push(
                div()
                    .text_color(muted_fg)
                    .child(format!(
                        "picocode — {} in {}",
                        self.cfg.model_label(),
                        self.cfg.root.display()
                    ))
                    .into_any_element(),
            );
        }

        let send_or_stop: AnyElement = if self.running {
            Button::new("stop")
                .danger()
                .label("Stop")
                .on_click(cx.listener(Self::stop))
                .into_any_element()
        } else {
            Button::new("send")
                .primary()
                .label("Send")
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                .into_any_element()
        };

        div()
            .v_flex()
            .relative()
            .size_full()
            .bg(background)
            .on_action(cx.listener(Self::accept_completion))
            .child(
                div()
                    .id("transcript")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_4()
                    .child(div().v_flex().gap_2().children(items)),
            )
            .children(self.render_completions(cx))
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(border)
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .child(send_or_stop),
            )
            .child(self.render_status_bar(cx))
            .children(self.render_menu(cx))
            .children(self.render_settings(cx))
            .children(self.render_session_picker(cx))
            .children(self.render_approval(cx))
            .children(self.render_question(cx))
    }
}

// ---------- small helpers ----------

/// Full-window dimmed backdrop for dialogs.
fn overlay() -> gpui::Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.4))
}

/// Status-bar color per permission mode (mirrors the TUI's palette).
fn mode_color(mode: Mode) -> gpui::Hsla {
    let rgb = match mode {
        Mode::ReadOnly => 0x0ea5e9, // cyan
        Mode::Edit => 0xeab308,     // yellow
        Mode::Plan => 0x3b82f6,     // blue
        Mode::Bypass => 0xef4444,   // red
    };
    gpui::rgb(rgb).into()
}

/// One clickable row of a status-bar menu: name, dimmed detail, and a check
/// mark on the active item.
fn menu_row(
    id: SharedString,
    name: impl Into<SharedString>,
    detail: String,
    active: bool,
    theme: &gpui_component::theme::Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .cursor_pointer()
        .rounded_md()
        .px_2()
        .py_1()
        .hover(|s| s.bg(theme.muted))
        .child(
            div()
                .h_flex()
                .gap_2()
                .justify_between()
                .child(name.into())
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .text_sm()
                        .child(if active {
                            "✓".to_string()
                        } else {
                            String::new()
                        }),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(detail),
        )
}

/// Render "+ "/"- "/"  " diff text with add/remove row backgrounds.
fn diff_element(text: &str, mono: SharedString) -> AnyElement {
    let mut rows = div().v_flex().font_family(mono).text_sm();
    for line in text.lines() {
        let content = if line.is_empty() {
            " ".to_string()
        } else {
            line.to_string()
        };
        let row = div().px_1().child(content);
        let row = if line.starts_with('+') {
            row.bg(gpui::rgba(DIFF_ADD_BG))
        } else if line.starts_with('-') {
            row.bg(gpui::rgba(DIFF_DEL_BG))
        } else {
            row
        };
        rows = rows.child(row);
    }
    rows.into_any_element()
}

/// Body of the approval dialog: `edit_file` shows the path plus a colored
/// diff, `bash` the command line, anything else pretty-printed JSON args.
fn approval_body(a: &Approval, mono: SharedString, muted: gpui::Hsla) -> AnyElement {
    let parsed: Option<serde_json::Value> = serde_json::from_str(&a.args).ok();
    let get = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
    };
    if a.name == "edit_file"
        && let (Some(path), Some(new)) = (get("path"), get("new_string"))
    {
        let old = get("old_string").unwrap_or_default();
        let diff = diff_lines(old, new).join("\n");
        return div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .font_family(mono.clone())
                    .text_color(muted)
                    .child(format!("path: {path}")),
            )
            .child(diff_element(&clip(&diff, 200), mono))
            .into_any_element();
    }
    if a.name == "bash"
        && let Some(cmd) = get("command")
    {
        return div()
            .font_family(mono)
            .child(cmd.to_string())
            .into_any_element();
    }
    let pretty = parsed
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_else(|| a.args.clone());
    div()
        .font_family(mono)
        .text_color(muted)
        .child(clip(&pretty, 60))
        .into_any_element()
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
