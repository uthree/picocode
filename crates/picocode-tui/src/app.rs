//! Application state and the main event loop.

use std::path::PathBuf;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use tokio::sync::{mpsc, oneshot, watch};

use picocode_core::config::Config;
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::models::{ModelChoice, model_choices};
use picocode_core::session;
pub use picocode_core::transcript::{Entry, EntryKind};

use crate::history::InputHistory;
use crate::input::{cursor_at, expand_pastes, line_col, paste_placeholder, spawn_input_thread};

const TOOL_OUTPUT_MAX_LINES: usize = 12;
const DIFF_MAX_LINES: usize = 30;

pub const LOGO: &str = r"            ███                                          █████
           ░░░                                          ░░███
 ████████  ████   ██████   ██████   ██████   ██████   ███████   ██████
░░███░░███░░███  ███░░███ ███░░███ ███░░███ ███░░███ ███░░███  ███░░███
 ░███ ░███ ░███ ░███ ░░░ ░███ ░███░███ ░░░ ░███ ░███░███ ░███ ░███████
 ░███ ░███ ░███ ░███  ███░███ ░███░███  ███░███ ░███░███ ░███ ░███░░░
 ░███████  █████░░██████ ░░██████ ░░██████ ░░██████ ░░████████░░██████
 ░███░░░  ░░░░░  ░░░░░░   ░░░░░░   ░░░░░░   ░░░░░░   ░░░░░░░░  ░░░░░░
 ░███
 █████
░░░░░";

pub struct PendingApproval {
    pub name: String,
    pub args: String,
    /// What the `a` (always) answer whitelists.
    pub always: AlwaysAllow,
    respond: oneshot::Sender<bool>,
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

/// Number of rows in the `/config` dialog (mode, reasoning, bash timeout,
/// read limits, web search provider/results, auto-compact, model).
pub const SETTINGS_ROWS: usize = 9;

/// State of the `submit_plan` approval (question) dialog.
pub struct PendingQuestion {
    pub title: String,
    pub question: String,
    pub options: Vec<String>,
    pub selected: usize,
    respond: oneshot::Sender<Option<usize>>,
}

pub struct App {
    pub entries: Vec<Entry>,
    pub input: String,
    /// Cursor position in the input, in chars.
    pub cursor: usize,
    /// When true the view sticks to the bottom of the transcript; scrolling up
    /// switches to a fixed `top_line` so streaming output doesn't move the view.
    pub follow: bool,
    /// First visible transcript line when not following the bottom.
    pub top_line: usize,
    /// Layout info from the last render, used by the scroll key handlers.
    pub last_total_lines: usize,
    pub last_view_height: usize,
    /// Show full model reasoning instead of a collapsed one-liner.
    pub show_reasoning: bool,
    /// Selected index in the command-completion popup.
    pub comp_selected: usize,
    /// Filter prefix locked at the first Tab / arrow press, so cycling keeps
    /// the full candidate list even after the input is filled with a match.
    comp_prefix: Option<String>,
    /// Shell-style `↑`/`↓` recall of previously submitted messages.
    input_history: InputHistory,
    /// Number of prompts submitted but not yet completed.
    pub running: usize,
    /// True while a completion request is in flight but no tokens have
    /// arrived yet — the status bar shows "waiting" instead of "running".
    pub waiting: bool,
    pub spinner: usize,
    pub pending: Option<PendingApproval>,
    /// Open question dialog (`submit_plan`), if any.
    pub question: Option<PendingQuestion>,
    /// Context size (input tokens) of the latest completion request.
    pub ctx_tokens: u64,
    /// Output tokens reported for the turn in progress.
    pub turn_out: u64,
    /// Output tokens reported since the conversation started (`/status`).
    total_out: u64,
    /// Streamed deltas since the last usage report — a live estimate of
    /// decoded tokens between usage updates.
    pub delta_est: u64,
    /// Files staged with `/attach`, sent with the next prompt.
    pub attachments: Vec<picocode_core::attachment::Attachment>,
    /// Counter naming the temp PNGs saved from clipboard image pastes.
    clip_count: usize,
    /// Latest context composition reported by the worker, shown by /status.
    context_info: Option<picocode_core::context::Breakdown>,
    /// Rolling generation-speed meter behind the status bar's tok/s.
    pub speed: picocode_core::speed::SpeedMeter,
    /// MCP connections established at startup, reused across respawns.
    mcp: picocode_core::mcp::McpConnections,
    /// While `Some`, the input box edits the system prompt instead of a
    /// message (`/prompt`); holds the stashed (input, cursor) to restore.
    pub prompt_edit: Option<(String, usize)>,
    /// Backgrounded (timed-out) bash commands still running, shown in the
    /// status bar.
    pub background_jobs: usize,
    /// True while an auto-compaction is pending or has failed — blocks
    /// re-triggering until a compaction succeeds or a new prompt is sent,
    /// so a failing compactor can't retry in a loop.
    auto_compact_tried: bool,
    pub model_label: String,
    /// Git branch of the project root (refreshed after each turn), for the
    /// input-box title.
    pub git_branch: Option<String>,
    /// Collapsed pasted blocks as (placeholder, full text); the placeholder
    /// sits in the input and is expanded when the message is submitted.
    pasted: Vec<(String, String)>,
    /// Models the current provider reported serving (fetched in the
    /// background); `/model <name>` can switch to any of them ad hoc.
    available_models: Vec<String>,
    /// Active config; provider/model/base_url track the current /model choice.
    cfg: Config,
    /// Event channel handed to newly spawned workers on model switch.
    event_tx: mpsc::Sender<AgentEvent>,
    /// Command channel of the current worker (replaced on model switch).
    cmd_tx: mpsc::Sender<WorkerCmd>,
    /// Mid-turn steering queue shared with the worker: text sent while a
    /// turn runs is injected at the next tool-call boundary.
    steer: picocode_core::steer::SteerQueue,
    /// Registry of running background jobs (`/jobs` list / kill). App-owned
    /// so jobs survive worker respawns on model switches.
    jobs: picocode_core::tools::BackgroundJobs,
    /// Signals the worker to abort the generation in progress (Esc).
    cancel_tx: watch::Sender<()>,
    /// Id of the session being written; a fresh one is issued by /clear.
    session_id: String,
    /// Where sessions are stored (None disables persistence, e.g. no $HOME).
    sessions_dir: Option<PathBuf>,
    /// Open `/resume` dialog, if any (captures the arrow/Enter keys).
    pub session_picker: Option<SessionPicker>,
    /// Open `/model` dialog, if any (captures the arrow/Enter keys).
    pub model_picker: Option<ModelPicker>,
    /// Open add-model form (reached from the `/model` dialog), if any.
    pub add_model: Option<AddModelForm>,
    /// Open `/config` dialog, if any (captures the arrow/Enter keys).
    pub settings: Option<SettingsMenu>,
    should_quit: bool,
    assistant_open: bool,
    reasoning_open: bool,
}

impl App {
    pub fn new(
        cfg: &Config,
        event_tx: mpsc::Sender<AgentEvent>,
        cmd_tx: mpsc::Sender<WorkerCmd>,
        steer: picocode_core::steer::SteerQueue,
        jobs: picocode_core::tools::BackgroundJobs,
        cancel_tx: watch::Sender<()>,
        mcp: picocode_core::mcp::McpConnections,
    ) -> Self {
        let mut app = Self {
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            follow: true,
            top_line: 0,
            last_total_lines: 0,
            last_view_height: 0,
            show_reasoning: false,
            comp_selected: 0,
            comp_prefix: None,
            input_history: InputHistory::default(),
            running: 0,
            waiting: false,
            spinner: 0,
            pending: None,
            question: None,
            ctx_tokens: 0,
            turn_out: 0,
            total_out: 0,
            delta_est: 0,
            attachments: Vec::new(),
            clip_count: 0,
            context_info: None,
            speed: picocode_core::speed::SpeedMeter::default(),
            prompt_edit: None,
            mcp,
            background_jobs: 0,
            auto_compact_tried: false,
            model_label: cfg.model_label(),
            git_branch: picocode_core::git::branch(&cfg.root),
            pasted: Vec::new(),
            available_models: Vec::new(),
            cfg: cfg.clone(),
            event_tx,
            cmd_tx,
            steer,
            jobs,
            cancel_tx,
            session_id: session::new_id(),
            sessions_dir: session::sessions_dir(&cfg.root),
            session_picker: None,
            model_picker: None,
            add_model: None,
            settings: None,
            should_quit: false,
            assistant_open: false,
            reasoning_open: false,
        };
        app.push(EntryKind::Logo, LOGO.to_string());
        app.push(
            EntryKind::Notice,
            format!(
                "picocode — {} (cwd: {})",
                app.model_label,
                cfg.root.display()
            ),
        );
        if let Some(note) = &cfg.model_note {
            app.push(EntryKind::Notice, format!("Model: {note}"));
        }
        if !cfg.config_files.is_empty() {
            app.push(
                EntryKind::Notice,
                format!("Config: {}", cfg.config_files.join(", ")),
            );
        }
        app.push(
            EntryKind::Notice,
            format!("Mode: {} — Shift+Tab to switch", cfg.mode.get().label()),
        );
        if cfg.system_prompt.is_some() {
            app.push(
                EntryKind::Notice,
                "System prompt: overridden by config".to_string(),
            );
        }
        if !cfg.instructions.is_empty() {
            let names: Vec<&str> = cfg.instructions.iter().map(|(n, _)| n.as_str()).collect();
            app.push(
                EntryKind::Notice,
                format!("Instructions: {}", names.join(", ")),
            );
        }
        if cfg.models.len() > 1 {
            let names: Vec<&str> = cfg.models.iter().map(|m| m.name.as_str()).collect();
            app.push(
                EntryKind::Notice,
                format!("Models: {} — /model <name> to switch", names.join(", ")),
            );
        }
        if cfg.mode.get() == picocode_core::config::Mode::Bypass {
            app.push(
                EntryKind::Warning,
                "bypass mode: EVERY tool call runs without confirmation (deny rules \
                 still apply). Meant for isolated environments such as containers."
                    .to_string(),
            );
        }
        // Fetch the provider's model list in the background so `/model` can
        // offer and validate provider models right away.
        app.refresh_models();
        // Whatever model this run starts with is the one to restore next time.
        app.save_last_model();
        app
    }

    /// Remember the active model (best-effort) so the next start in this
    /// project resumes with it.
    fn save_last_model(&self) {
        let Some(path) = picocode_core::state::state_path(&self.cfg.root) else {
            return;
        };
        let _ = picocode_core::state::save(
            &path,
            &picocode_core::state::LastModel {
                entry: self.cfg.active_model.clone(),
                provider: picocode_core::config::provider_name(self.cfg.provider).to_string(),
                model: self.cfg.model.clone(),
                base_url: self.cfg.base_url.clone(),
            },
        );
    }

    pub async fn run(
        mut self,
        mut terminal: DefaultTerminal,
        mut agent_rx: mpsc::Receiver<AgentEvent>,
    ) -> anyhow::Result<()> {
        let mut term_rx = spawn_input_thread();
        let mut tick = tokio::time::interval(Duration::from_millis(120));

        loop {
            terminal.draw(|f| crate::ui::draw(f, &mut self))?;
            tokio::select! {
                Some(ev) = term_rx.recv() => self.handle_terminal_event(ev).await,
                Some(ev) = agent_rx.recv() => self.handle_agent_event(ev),
                _ = tick.tick(), if self.running > 0 => {
                    self.spinner = self.spinner.wrapping_add(1);
                }
                else => break,
            }
            // Batch-drain pending agent events so a streaming burst costs one
            // redraw instead of one per delta (keeps the UI responsive).
            let mut drained = 0;
            while drained < 128 {
                match agent_rx.try_recv() {
                    Ok(ev) => {
                        self.handle_agent_event(ev);
                        drained += 1;
                    }
                    Err(_) => break,
                }
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    // ----- terminal events -------------------------------------------------

    async fn handle_terminal_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key).await;
            }
            Event::Paste(text) => {
                // Keep pasted newlines; tabs become spaces so the cursor
                // math (unicode widths) stays correct.
                let text = text
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .replace('\t', "    ");
                self.insert_paste(text);
                self.reset_completion();
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.scroll_by(-3),
                MouseEventKind::ScrollDown => self.scroll_by(3),
                _ => {}
            },
            _ => {}
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            self.should_quit = true;
            return;
        }
        if ctrl && key.code == KeyCode::Char('t') {
            self.show_reasoning = !self.show_reasoning;
            return;
        }

        // Approval modal captures y/a/n while pending.
        if self.pending.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.resolve_approval(true)
                }
                KeyCode::Char('a') | KeyCode::Char('A') => self.resolve_approval_always(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.resolve_approval(false)
                }
                _ => {}
            }
            return;
        }

        // The question dialog captures navigation keys while open.
        if let Some(q) = &mut self.question {
            let count = q.options.len();
            match key.code {
                KeyCode::Up => q.selected = (q.selected + count - 1) % count,
                KeyCode::Down => q.selected = (q.selected + 1) % count,
                KeyCode::Enter => self.resolve_question(true),
                KeyCode::Esc => self.resolve_question(false),
                _ => {}
            }
            return;
        }

        // The /config dialog captures navigation keys while open.
        if self.settings.is_some() {
            match key.code {
                KeyCode::Up => {
                    let menu = self.settings.as_mut().unwrap();
                    menu.selected = (menu.selected + SETTINGS_ROWS - 1) % SETTINGS_ROWS;
                }
                KeyCode::Down => {
                    let menu = self.settings.as_mut().unwrap();
                    menu.selected = (menu.selected + 1) % SETTINGS_ROWS;
                }
                KeyCode::Left => self.adjust_setting(-1),
                KeyCode::Right => self.adjust_setting(1),
                KeyCode::Enter | KeyCode::Char(' ') => self.activate_setting(),
                KeyCode::Esc | KeyCode::Char('q') => self.settings = None,
                _ => {}
            }
            return;
        }

        // The add-model form captures keys while open.
        if self.add_model.is_some() {
            self.add_model_key(key).await;
            return;
        }

        // The /model dialog captures keys while open: typing searches, the
        // filtered list is followed by a synthetic "+ add…" row.
        if let Some(picker) = &mut self.model_picker {
            let count = picker.filtered().len() + 1;
            match key.code {
                KeyCode::Up => picker.selected = (picker.selected + count - 1) % count,
                KeyCode::Down => picker.selected = (picker.selected + 1) % count,
                KeyCode::Enter => {
                    let filtered = picker.filtered();
                    if picker.selected < filtered.len() {
                        let name = filtered[picker.selected].name.clone();
                        self.model_picker = None;
                        self.switch_model(&name).await;
                    } else {
                        self.model_picker = None;
                        self.open_add_model();
                    }
                }
                KeyCode::Esc => self.model_picker = None,
                KeyCode::Char(c) => {
                    picker.filter.push(c);
                    picker.selected = 0;
                }
                KeyCode::Backspace => {
                    picker.filter.pop();
                    picker.selected = 0;
                }
                _ => {}
            }
            return;
        }

        // The /resume dialog captures navigation keys while open.
        if let Some(picker) = &mut self.session_picker {
            let count = picker.sessions.len();
            match key.code {
                KeyCode::Up => picker.selected = (picker.selected + count - 1) % count,
                KeyCode::Down => picker.selected = (picker.selected + 1) % count,
                KeyCode::Enter => {
                    let id = picker.sessions[picker.selected].id.clone();
                    self.session_picker = None;
                    self.resume_session(&id).await;
                }
                KeyCode::Esc | KeyCode::Char('q') => self.session_picker = None,
                _ => {}
            }
            return;
        }

        // Ctrl+V: paste from the system clipboard — copied files and
        // images stage as attachments, text inserts like a terminal paste.
        if ctrl && key.code == KeyCode::Char('v') {
            self.paste_clipboard();
            return;
        }

        match key.code {
            // Alt+Enter (and Shift+Enter on terminals that report it) inserts
            // a newline; Ctrl+J below covers legacy raw-mode terminals.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.insert_char('\n');
                self.reset_completion();
            }
            KeyCode::Enter => {
                // Backslash continuation: `\` + Enter becomes a newline —
                // works on terminals that can't report modified Enter.
                if self.cursor > 0 && self.input.chars().nth(self.cursor - 1) == Some('\\') {
                    self.cursor -= 1;
                    let i = self.byte_index();
                    self.input.remove(i);
                    self.insert_char('\n');
                    self.reset_completion();
                } else {
                    self.submit().await;
                }
            }
            KeyCode::Char('j') if ctrl => {
                self.insert_char('\n');
                self.reset_completion();
            }
            KeyCode::Tab => self.complete(false),
            // Shift+Tab cycles the completion popup while it's open, and the
            // permission mode otherwise.
            KeyCode::BackTab => {
                if self.completions().is_empty() {
                    self.cycle_mode();
                } else {
                    self.complete(true);
                }
            }
            // Arrows drive the completion popup when it's open; otherwise
            // they move between lines in a multi-line input and, at its top
            // or bottom line, recall previously submitted messages
            // (shell-style history). While browsing the history, arrows keep
            // browsing even when a recalled command matches the popup.
            KeyCode::Up | KeyCode::Down => {
                let up = key.code == KeyCode::Up;
                if !self.input_history.browsing() && !self.completions().is_empty() {
                    self.move_completion(up);
                } else {
                    self.move_line_or_history(up);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.insert_char(c);
                self.reset_completion();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    // A paste placeholder is deleted as one unit.
                    if !self.delete_placeholder_before_cursor() {
                        self.cursor -= 1;
                        let i = self.byte_index();
                        self.input.remove(i);
                    }
                    self.reset_completion();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    if !self.delete_placeholder_at_cursor() {
                        let i = self.byte_index();
                        self.input.remove(i);
                    }
                    self.reset_completion();
                }
            }
            KeyCode::Esc if self.prompt_edit.is_some() => self.cancel_prompt_edit(),
            KeyCode::Esc if self.running > 0 => {
                let _ = self.cancel_tx.send(());
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            _ => {}
        }
    }

    async fn submit(&mut self) {
        // Enter while the input box edits the system prompt applies it.
        if self.prompt_edit.is_some() {
            self.apply_prompt_edit().await;
            return;
        }
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input_history.push(&text);
        self.input.clear();
        self.cursor = 0;
        self.reset_completion();

        // Expand collapsed pastes: the model (or shell) gets the full text
        // while the transcript keeps the compact placeholder. A message with
        // a paste in it is never a command.
        let expanded = expand_pastes(&text, &self.pasted);
        if expanded != text {
            if text.starts_with('!') {
                self.run_shell(expanded);
            } else {
                self.send_prompt(text, expanded).await;
            }
            return;
        }

        // Commands are single-line; a multi-line message is always a prompt
        // (or a multi-line `!` shell script).
        if text.contains('\n') {
            if text.starts_with('!') {
                self.run_shell(text);
            } else {
                self.send_prompt(text.clone(), text).await;
            }
            return;
        }

        if text.starts_with('!') {
            self.run_shell(text);
            return;
        }
        use picocode_core::command::{Command, JobsAction, ParseOutcome};
        match picocode_core::command::parse(&text) {
            ParseOutcome::Prompt => self.send_prompt(text.clone(), text).await,
            ParseOutcome::Unknown { name } => {
                self.push(EntryKind::Error, format!("Unknown command: {name}"));
            }
            ParseOutcome::Invalid { message } => {
                self.push(EntryKind::Error, message);
            }
            ParseOutcome::Command(command) => match command {
                Command::Quit => self.should_quit = true,
                Command::Clear => {
                    self.entries.clear();
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
                    let _ = self.cmd_tx.send(WorkerCmd::Clear).await;
                    // The cleared conversation stays on disk; start a fresh
                    // log.
                    self.session_id = session::new_id();
                    self.push(EntryKind::Logo, LOGO.to_string());
                    self.push(
                        EntryKind::Notice,
                        "Conversation history cleared".to_string(),
                    );
                }
                Command::Compact => {
                    self.close_blocks();
                    if self.cmd_tx.send(WorkerCmd::Compact).await.is_ok() {
                        self.begin_turn();
                        self.push(EntryKind::Notice, "Compacting conversation…".to_string());
                    } else {
                        self.push(EntryKind::Error, "The agent worker has stopped".to_string());
                    }
                }
                Command::Undo => {
                    let _ = self.cmd_tx.send(WorkerCmd::Undo).await;
                }
                Command::Jobs(JobsAction::List) => self.show_jobs(),
                Command::Jobs(JobsAction::Kill(id)) => self.kill_job(id),
                Command::Permissions => self.show_permissions(),
                Command::Config => self.settings = Some(SettingsMenu { selected: 0 }),
                Command::Status => self.show_status(),
                Command::Mode(mode) => self.set_mode(mode),
                Command::Model(None) => self.open_model_picker(),
                Command::Model(Some(name)) => self.switch_model(&name).await,
                Command::Resume(None) => self.open_session_picker(),
                Command::Resume(Some(id)) => self.resume_session(&id).await,
                Command::Attach(None) => self.show_attachments(),
                Command::Attach(Some(arg)) => self.attach(&arg),
                Command::SystemPrompt(action) => {
                    use picocode_core::command::PromptAction;
                    match action {
                        PromptAction::Edit => self.open_prompt_editor(),
                        PromptAction::Reset => self.apply_system_prompt(None).await,
                        PromptAction::Preset(name) => self.apply_prompt_preset(&name).await,
                    }
                }
            },
        }
    }

    /// Bookkeeping for a run the worker accepted (or will accept): the
    /// status bar switches to waiting and the per-turn output counters
    /// restart. Balanced by `TurnComplete`.
    fn begin_turn(&mut self) {
        self.running += 1;
        self.waiting = true;
        self.follow = true;
        self.turn_out = 0;
        self.delta_est = 0;
    }

    /// Queue a worker command from sync code after `begin_turn()`: sent in a
    /// task; a dead worker surfaces an error and closes the turn.
    fn send_worker_bg(&self, cmd: WorkerCmd) {
        let cmd_tx = self.cmd_tx.clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            if cmd_tx.send(cmd).await.is_err() {
                let _ = event_tx
                    .send(AgentEvent::Error(
                        "The agent worker has stopped".to_string(),
                    ))
                    .await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
            }
        });
    }

    /// Send a user prompt to the agent worker. `display` is what the
    /// transcript shows (paste placeholders kept), `prompt` what the model
    /// receives (pastes expanded). Files staged with `/attach` go along and
    /// the staging list is cleared.
    async fn send_prompt(&mut self, display: String, prompt: String) {
        self.close_blocks();
        // A new prompt re-arms auto-compaction (one attempt per user turn).
        self.auto_compact_tried = false;
        let attachments = std::mem::take(&mut self.attachments);
        self.entries.push(Entry {
            kind: EntryKind::User,
            text: display,
            lang: None,
            attachments: attachments.iter().map(|a| a.name()).collect(),
        });
        // Mid-turn text goes through the steering queue: the worker injects
        // it at the next tool-call boundary (or runs it as a follow-up
        // prompt of the same turn), instead of waiting in the command
        // channel until the turn ends. Attachments can't ride a tool
        // result, so those still queue as a regular prompt.
        if self.running > 0 && attachments.is_empty() {
            self.steer.push(prompt);
            return;
        }
        if self
            .cmd_tx
            .send(WorkerCmd::Prompt {
                text: prompt,
                attachments,
            })
            .await
            .is_ok()
        {
            self.begin_turn();
        } else {
            self.push(EntryKind::Error, "The agent worker has stopped".to_string());
        }
    }

    /// `/attach <path>`: stage a file to send with the next prompt
    /// (`/attach clear` unstages everything). Unsupported types and types
    /// the current provider can't take are refused with an explanation, so
    /// nothing is silently dropped later in the provider conversion.
    fn attach(&mut self, arg: &str) {
        if arg == "clear" {
            self.attachments.clear();
            self.push(EntryKind::Notice, "Attachments cleared".to_string());
            return;
        }
        self.stage_file(&self.cfg.root.join(arg), arg);
    }

    /// Stage one file as an attachment, shared by `/attach` and clipboard
    /// paste; `display` is how the file is referred to in notices.
    fn stage_file(&mut self, path: &std::path::Path, display: &str) {
        use picocode_core::attachment::Attachment;
        if !path.is_file() {
            self.push(EntryKind::Error, format!("Not a file: {display}"));
            return;
        }
        match Attachment::detect(path) {
            Some(att) if att.supported_by(self.cfg.provider) => {
                if self.attachments.contains(&att) {
                    self.push(EntryKind::Notice, format!("Already attached: {display}"));
                    return;
                }
                self.push(
                    EntryKind::Notice,
                    format!(
                        "📎 Attached {} ({} staged)",
                        display,
                        self.attachments.len() + 1
                    ),
                );
                self.attachments.push(att);
            }
            Some(_) => self.push(
                EntryKind::Warning,
                format!(
                    "The {} provider can't take this file type; not attached",
                    picocode_core::config::provider_name(self.cfg.provider)
                ),
            ),
            None => self.push(
                EntryKind::Notice,
                "This looks like an unsupported binary format — images, audio, PDF \
                 and text files can be attached"
                    .to_string(),
            ),
        }
    }

    /// `/attach` with no argument: list what's staged.
    fn show_attachments(&mut self) {
        if self.attachments.is_empty() {
            self.push(
                EntryKind::Notice,
                "No attachments staged. /attach <path> stages a file for the \
                 next prompt; /attach clear unstages all."
                    .to_string(),
            );
            return;
        }
        let list = self
            .attachments
            .iter()
            .map(|a| format!("  📎 {}", a.path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        self.push(
            EntryKind::Notice,
            format!("Staged for the next prompt:\n{list}"),
        );
    }

    /// `/prompt`: turn the input box into a system-prompt editor loaded
    /// with the current base prompt (custom or built-in). All the usual
    /// editing works — multi-line via Alt+Enter / `\`+Enter, paste, Ctrl+V.
    /// Enter applies, Esc restores the stashed draft.
    fn open_prompt_editor(&mut self) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot edit the system prompt while a turn is running".to_string(),
            );
            return;
        }
        self.prompt_edit = Some((std::mem::take(&mut self.input), self.cursor));
        self.input = picocode_core::agent::base_system_prompt(&self.cfg);
        self.cursor = self.input.chars().count();
        self.reset_completion();
        self.push(
            EntryKind::Notice,
            "Editing the system prompt — Enter applies (this session), Esc cancels. \
             `{root}` expands to the project root; project instructions are \
             appended automatically."
                .to_string(),
        );
    }

    /// Esc in prompt-edit mode: drop the draft, restore the stashed input.
    fn cancel_prompt_edit(&mut self) {
        if let Some((input, cursor)) = self.prompt_edit.take() {
            self.input = input;
            self.cursor = cursor;
            self.push(EntryKind::Notice, "System prompt unchanged".to_string());
        }
    }

    /// Enter in prompt-edit mode: apply the edited prompt.
    async fn apply_prompt_edit(&mut self) {
        let text = expand_pastes(self.input.trim(), &self.pasted);
        let Some((input, cursor)) = self.prompt_edit.take() else {
            return;
        };
        self.input = input;
        self.cursor = cursor;
        if text.is_empty() {
            self.push(
                EntryKind::Notice,
                "Empty prompt — system prompt unchanged (use /prompt reset for the built-in)"
                    .to_string(),
            );
            return;
        }
        // Editing the built-in into itself is not a customization.
        if self.cfg.system_prompt.is_none()
            && text == picocode_core::agent::base_system_prompt(&self.cfg)
        {
            self.push(EntryKind::Notice, "System prompt unchanged".to_string());
            return;
        }
        self.apply_system_prompt(Some(text)).await;
    }

    /// `/prompt <name>`: switch to a `[[prompts]]` preset (exact name, or
    /// a unique case-insensitive substring like /model).
    async fn apply_prompt_preset(&mut self, name: &str) {
        let names = || {
            self.cfg
                .prompts
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        if self.cfg.prompts.is_empty() {
            self.push(
                EntryKind::Error,
                "No [[prompts]] presets configured in picocode.toml".to_string(),
            );
            return;
        }
        let needle = name.to_lowercase();
        let exact = self.cfg.prompts.iter().find(|p| p.name == name);
        let matches: Vec<_> = self
            .cfg
            .prompts
            .iter()
            .filter(|p| p.name.to_lowercase().contains(&needle))
            .collect();
        let preset = match (exact, matches.as_slice()) {
            (Some(p), _) => p,
            (None, [p]) => p,
            (None, []) => {
                let names = names();
                self.push(
                    EntryKind::Error,
                    format!("Unknown prompt preset `{name}` (available: {names})"),
                );
                return;
            }
            (None, many) => {
                let names = many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>();
                self.push(
                    EntryKind::Error,
                    format!("`{name}` is ambiguous: {}", names.join(", ")),
                );
                return;
            }
        };
        let (preset_name, text) = (preset.name.clone(), preset.prompt.clone());
        if self.set_system_prompt(Some(text)).await {
            self.push(
                EntryKind::Notice,
                format!("System prompt switched to preset `{preset_name}`"),
            );
        }
    }

    /// Swap the system prompt (None = built-in default) by respawning the
    /// worker with the conversation carried over, like a model switch.
    /// Returns whether it happened; pushes the errors, callers the notices.
    async fn set_system_prompt(&mut self, prompt: Option<String>) -> bool {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot change the system prompt while a turn is running".to_string(),
            );
            return false;
        }
        let mut new_cfg = self.cfg.clone();
        new_cfg.system_prompt = prompt;
        if let Err(e) = self.respawn_worker(new_cfg).await {
            self.push(
                EntryKind::Error,
                format!("Failed to apply the system prompt: {e:#}"),
            );
            return false;
        }
        true
    }

    /// The editor's apply / `/prompt reset`, with their notices.
    async fn apply_system_prompt(&mut self, prompt: Option<String>) {
        if !self.set_system_prompt(prompt.clone()).await {
            return;
        }
        match prompt {
            Some(text) => {
                self.push(
                    EntryKind::Notice,
                    "System prompt updated for this session — the next message uses it".to_string(),
                );
                self.push(
                    EntryKind::Notice,
                    format!(
                        "To keep it, add this to picocode.toml:\n{}",
                        picocode_core::config::system_prompt_snippet(&text)
                    ),
                );
            }
            None => self.push(
                EntryKind::Notice,
                "System prompt reset to the built-in default".to_string(),
            ),
        }
    }

    /// `!<command>`: run a shell command directly (no model, no approval —
    /// the user typed it). The output is shown and recorded in the model's
    /// history so it can be referred to in the next prompt.
    fn run_shell(&mut self, text: String) {
        let command = text[1..].trim().to_string();
        if command.is_empty() {
            self.push(EntryKind::Error, "Empty shell command".to_string());
            return;
        }
        self.close_blocks();
        self.push(EntryKind::User, text);
        self.running += 1;
        self.follow = true;

        let root = self.cfg.root.clone();
        let timeout = self.cfg.bash_timeout.clone();
        let event_tx = self.event_tx.clone();
        let cmd_tx = self.cmd_tx.clone();
        let jobs = self.jobs.clone();
        let mut cancel = self.cancel_tx.subscribe();
        tokio::spawn(async move {
            use rig::tool::Tool;
            // User-typed `!` commands run unsandboxed by design.
            let tool = picocode_core::tools::Bash::new(
                root,
                timeout,
                event_tx.clone(),
                jobs,
                picocode_core::sandbox::SandboxCtx::off(),
            );
            let call = tool.call(picocode_core::tools::BashArgs {
                command: command.clone(),
            });
            // Esc drops the call future, which kills the process
            // (kill_on_drop) — unless it already went to the background.
            let output = tokio::select! {
                biased;
                _ = cancel.changed() => "(stopped by Esc before finishing)".to_string(),
                out = call => match out {
                    Ok(out) => out,
                    Err(e) => format!("error: {e}"),
                },
            };
            let _ = cmd_tx
                .send(WorkerCmd::ShellRecord {
                    command,
                    output: output.clone(),
                })
                .await;
            let _ = event_tx.send(AgentEvent::ShellOutput { output }).await;
            let _ = event_tx.send(AgentEvent::TurnComplete).await;
        });
    }

    /// Ask the provider for its model list in the background; the answer
    /// arrives as a `ModelList` event, which updates the switch candidates
    /// and an open `/model` dialog.
    fn refresh_models(&self) {
        let provider = self.cfg.provider;
        let base = self.cfg.base_url.clone();
        let label = format!(
            "{} @ {}",
            picocode_core::config::provider_name(provider),
            picocode_core::models::base_url(provider, base.as_deref())
        );
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = picocode_core::models::fetch(provider, base.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = event_tx.send(AgentEvent::ModelList { label, result }).await;
        });
    }

    /// `/model` with no argument: open the model-selection dialog with the
    /// configured entries plus the provider's served models, and refresh the
    /// latter in the background.
    fn open_model_picker(&mut self) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            return;
        }
        let items = self.model_choices();
        let selected = items.iter().position(|c| c.active).unwrap_or(0);
        self.model_picker = Some(ModelPicker {
            items,
            selected,
            filter: String::new(),
        });
        self.refresh_models();
    }

    /// Rows for the `/model` dialog: configured entries first, then the
    /// models the provider reported serving (minus ones an entry already
    /// covers).
    fn model_choices(&self) -> Vec<ModelChoice> {
        model_choices(
            &self.cfg.models,
            self.cfg.active_model.as_deref(),
            self.cfg.provider,
            self.cfg.base_url.as_deref(),
            &self.cfg.model,
            &self.available_models,
        )
    }

    /// Rebuild the open `/model` dialog after a fresh provider list arrived,
    /// keeping the selection on the same item where possible.
    fn rebuild_model_picker(&mut self) {
        let Some(picker) = &self.model_picker else {
            return;
        };
        let filter = picker.filter.clone();
        let keep = picker
            .filtered()
            .get(picker.selected)
            .map(|c| c.name.clone());
        let items = self.model_choices();
        let rebuilt = ModelPicker {
            items,
            selected: 0,
            filter,
        };
        let selected = keep
            .and_then(|k| rebuilt.filtered().iter().position(|c| c.name == k))
            .unwrap_or(0);
        self.model_picker = Some(ModelPicker {
            selected,
            ..rebuilt
        });
    }

    /// `/model <name>`: spawn a worker for the named entry — or for a model
    /// the provider reported serving — and carry the conversation history
    /// over to it. A partial name that matches exactly one candidate is
    /// completed automatically.
    async fn switch_model(&mut self, name: &str) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            return;
        }
        let name = match picocode_core::models::resolve_partial(
            name,
            &self.cfg.models,
            &self.available_models,
        ) {
            picocode_core::models::PartialMatch::Unique(full) => {
                if full != name {
                    self.push(EntryKind::Notice, format!("`{name}` matched {full}"));
                }
                full
            }
            picocode_core::models::PartialMatch::Ambiguous(matches) => {
                self.push(
                    EntryKind::Error,
                    format!("`{name}` is ambiguous: {}", matches.join(", ")),
                );
                return;
            }
            picocode_core::models::PartialMatch::None => name.to_string(),
        };
        let name = name.as_str();
        let mut new_cfg = self.cfg.clone();
        match self.cfg.models.iter().find(|m| m.name == name) {
            Some(entry) => {
                if self.cfg.active_model.as_deref() == Some(name) {
                    self.push(
                        EntryKind::Notice,
                        format!("Already using {name} ({})", entry.label()),
                    );
                    return;
                }
                new_cfg.provider = entry.provider;
                new_cfg.model = entry.model.clone();
                new_cfg.base_url = entry.base_url.clone();
                new_cfg.active_model = Some(entry.name.clone());
                new_cfg.context_window = entry
                    .context_window
                    .unwrap_or(picocode_core::config::DEFAULT_CONTEXT_WINDOW);
            }
            // A model id the provider reported serving: switch ad hoc,
            // keeping the current provider and base URL.
            None if self.available_models.iter().any(|m| m == name) => {
                if self.cfg.active_model.is_none() && self.cfg.model == name {
                    self.push(
                        EntryKind::Notice,
                        format!("Already using {}", self.model_label),
                    );
                    return;
                }
                new_cfg.model = name.to_string();
                new_cfg.active_model = None;
                new_cfg.context_window = picocode_core::config::DEFAULT_CONTEXT_WINDOW;
            }
            None => {
                let names: Vec<&str> = self.cfg.models.iter().map(|m| m.name.as_str()).collect();
                let hint = if names.is_empty() {
                    "run /model to list what the provider serves".to_string()
                } else {
                    format!(
                        "configured: {}; /model lists what the provider serves",
                        names.join(", ")
                    )
                };
                self.push(EntryKind::Error, format!("Unknown model `{name}` — {hint}"));
                return;
            }
        }

        self.apply_model_config(new_cfg, name).await;
    }

    /// Switch to an explicit provider/model/base-URL combination (the
    /// add-model form): an ad-hoc selection like `--provider`/`--model` on
    /// the command line, remembered per project. Returns whether it worked.
    async fn switch_custom(
        &mut self,
        provider: picocode_core::config::Provider,
        model: String,
        base_url: Option<String>,
    ) -> bool {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            return false;
        }
        let mut new_cfg = self.cfg.clone();
        new_cfg.provider = provider;
        new_cfg.model = model.clone();
        new_cfg.base_url = base_url;
        new_cfg.active_model = None;
        new_cfg.context_window = picocode_core::config::DEFAULT_CONTEXT_WINDOW;
        self.apply_model_config(new_cfg, &model).await
    }

    /// Respawn the worker for `new_cfg` and carry the conversation over;
    /// shared by the model switches and the `/prompt` editor. A spawn
    /// failure leaves everything untouched.
    async fn respawn_worker(&mut self, new_cfg: Config) -> anyhow::Result<()> {
        // Spawn first so a failure (e.g. missing API key) leaves the current
        // worker untouched.
        let (new_tx, new_steer) = picocode_core::agent::spawn(
            &new_cfg,
            self.event_tx.clone(),
            self.cancel_tx.subscribe(),
            self.jobs.clone(),
            self.mcp.clone(),
        )?;

        // Carry the conversation over to the new worker.
        let (htx, hrx) = oneshot::channel();
        if self.cmd_tx.send(WorkerCmd::TakeHistory(htx)).await.is_ok()
            && let Ok(history) = hrx.await
        {
            let _ = new_tx.send(WorkerCmd::SeedHistory(history)).await;
        }

        self.cmd_tx = new_tx; // dropping the old sender shuts the old worker down
        self.steer = new_steer;
        self.cfg = new_cfg;
        Ok(())
    }

    /// Switch to `new_cfg`'s model: respawn and report. Shared by the
    /// by-name switch and the add-model form.
    async fn apply_model_config(&mut self, new_cfg: Config, name: &str) -> bool {
        // The cached model list belongs to the endpoint it was fetched from.
        let endpoint_changed =
            new_cfg.provider != self.cfg.provider || new_cfg.base_url != self.cfg.base_url;
        if let Err(e) = self.respawn_worker(new_cfg).await {
            self.push(
                EntryKind::Error,
                format!("Failed to switch to `{name}`: {e:#}"),
            );
            return false;
        }
        self.model_label = self.cfg.model_label();
        self.push(
            EntryKind::Notice,
            format!("Model switched to {name} ({})", self.model_label),
        );
        if endpoint_changed {
            self.available_models.clear();
            self.refresh_models();
        }
        self.save_last_model();
        true
    }

    /// Open the add-model form (the `/model` dialog's "+ add" row).
    fn open_add_model(&mut self) {
        self.add_model = Some(AddModelForm {
            provider: self.cfg.provider,
            base_url: String::new(),
            model: String::new(),
            field: 0,
            fetched: Vec::new(),
            note: "Tab: fetch the endpoint's model list".to_string(),
        });
    }

    /// Key handling for the add-model form.
    async fn add_model_key(&mut self, key: KeyEvent) {
        let Some(form) = &mut self.add_model else {
            return;
        };
        let rows = 3 + form.fetched.len();
        match key.code {
            KeyCode::Esc => {
                self.add_model = None;
                self.open_model_picker();
            }
            KeyCode::Up => form.field = (form.field + rows - 1) % rows,
            KeyCode::Down => form.field = (form.field + 1) % rows,
            // Provider row: cycle. A different provider invalidates the
            // fetched list.
            KeyCode::Left | KeyCode::Right if form.field == 0 => {
                let delta = if key.code == KeyCode::Left { -1 } else { 1 };
                form.provider = form.provider.cycled(delta);
                form.fetched.clear();
                form.note = format!(
                    "{} — Tab: fetch the endpoint's model list",
                    form.provider.api_key_hint()
                );
            }
            KeyCode::Char(c) if form.field == 1 => form.base_url.push(c),
            KeyCode::Char(c) if form.field == 2 => form.model.push(c),
            KeyCode::Backspace if form.field == 1 => {
                form.base_url.pop();
            }
            KeyCode::Backspace if form.field == 2 => {
                form.model.pop();
            }
            KeyCode::Tab => {
                form.note = "fetching…".to_string();
                let provider = form.provider;
                let base = form.base();
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let result = picocode_core::models::fetch(provider, base.as_deref())
                        .await
                        .map_err(|e| format!("{e:#}"));
                    let _ = event_tx
                        .send(AgentEvent::FormModelList {
                            provider,
                            base_url: base,
                            result,
                        })
                        .await;
                });
            }
            KeyCode::Enter => {
                // A fetched row switches to that model; the form rows switch
                // to the typed one.
                let model = if form.field >= 3 {
                    form.fetched[form.field - 3].clone()
                } else {
                    form.model.trim().to_string()
                };
                if model.is_empty() {
                    form.note = "type a model name (or Tab to fetch, ↑↓ to pick one)".to_string();
                    return;
                }
                let provider = form.provider;
                let base = form.base();
                self.add_model = None;
                if self
                    .switch_custom(provider, model.clone(), base.clone())
                    .await
                {
                    self.push(
                        EntryKind::Notice,
                        format!(
                            "To keep this model across projects, add it to picocode.toml:\n{}",
                            picocode_core::models::toml_snippet(provider, &model, base.as_deref())
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    /// `/resume` with no argument: open the session-selection dialog.
    fn open_session_picker(&mut self) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot resume while a turn is running".to_string(),
            );
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(
                EntryKind::Error,
                "Session storage is unavailable (no $HOME)".to_string(),
            );
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
            return;
        }
        self.session_picker = Some(SessionPicker {
            sessions,
            selected: 0,
        });
    }

    /// Resume a session by id (picked in the dialog or given to `/resume <id>`):
    /// seed the worker with its history and restore the transcript.
    async fn resume_session(&mut self, id: &str) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot resume while a turn is running".to_string(),
            );
            return;
        }
        let Some(dir) = self.sessions_dir.clone() else {
            self.push(
                EntryKind::Error,
                "Session storage is unavailable (no $HOME)".to_string(),
            );
            return;
        };
        if id == self.session_id {
            self.push(EntryKind::Notice, "That is the current session".to_string());
            return;
        }

        let saved = match session::load(&dir, id) {
            Ok(s) => s,
            Err(e) => {
                self.push(EntryKind::Error, format!("Failed to resume: {e:#}"));
                return;
            }
        };
        let messages = saved.history.len();
        if self
            .cmd_tx
            .send(WorkerCmd::SeedHistory(saved.history))
            .await
            .is_err()
        {
            self.push(EntryKind::Error, "The agent worker has stopped".to_string());
            return;
        }

        self.entries.clear();
        self.close_blocks();
        self.ctx_tokens = 0;
        self.follow = true;
        self.top_line = 0;
        self.push(EntryKind::Logo, LOGO.to_string());
        self.push(
            EntryKind::Notice,
            format!(
                "Resumed session {id} — {messages} messages, last saved with {}",
                saved.model
            ),
        );
        self.entries.extend(saved.entries);
        self.session_id = id.to_string();
    }

    /// Snapshot the conversation to disk. Runs in the background after each
    /// completed turn; empty conversations are not written.
    fn autosave(&mut self) {
        let Some(dir) = self.sessions_dir.clone() else {
            return;
        };
        let id = self.session_id.clone();
        let cwd = self.cfg.root.display().to_string();
        let model = self.model_label.clone();
        let entries: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| e.kind != EntryKind::Logo)
            .cloned()
            .collect();
        let cmd_tx = self.cmd_tx.clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
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

    /// Candidates for the completion popup, as (text to fill the input
    /// with, description). Uses the locked prefix while cycling, otherwise
    /// the current input. Before the first space these are command names;
    /// after it, the command's argument candidates (model names, session
    /// ids, file paths).
    pub fn completions(&self) -> Vec<(String, String)> {
        // The prompt editor's content is never a command.
        if self.prompt_edit.is_some() {
            return Vec::new();
        }
        let filter = self.comp_prefix.as_deref().unwrap_or(&self.input);
        if !filter.starts_with('/') || filter.contains('\n') {
            return Vec::new();
        }
        match filter.split_once(' ') {
            None => picocode_core::command::COMMANDS
                .iter()
                .filter(|spec| spec.name.starts_with(filter))
                .map(|spec| (spec.name.to_string(), spec.description.to_string()))
                .collect(),
            Some((cmd, arg)) => self.arg_completions(cmd, arg.trim_start()),
        }
    }

    /// Argument candidates for `cmd`, filtered by the partial `arg`.
    fn arg_completions(&self, cmd: &str, arg: &str) -> Vec<(String, String)> {
        let needle = arg.to_lowercase();
        match cmd {
            "/model" => self
                .model_choices()
                .into_iter()
                .filter(|c| needle.is_empty() || c.name.to_lowercase().contains(&needle))
                .map(|c| (format!("{cmd} {}", c.name), c.detail))
                .collect(),
            "/resume" => {
                let Some(dir) = &self.sessions_dir else {
                    return Vec::new();
                };
                session::list(dir)
                    .into_iter()
                    .filter(|s| s.id != self.session_id)
                    .filter(|s| needle.is_empty() || s.id.to_lowercase().contains(&needle))
                    .map(|s| {
                        (
                            format!("{cmd} {}", s.id),
                            format!("{} messages · {}", s.messages, s.model),
                        )
                    })
                    .collect()
            }
            "/attach" => picocode_core::command::path_completions(&self.cfg.root, cmd, arg),
            "/jobs" => picocode_core::command::jobs_completions(&self.jobs, cmd, arg),
            "/prompt" => picocode_core::command::prompt_completions(&self.cfg.prompts, cmd, arg),
            _ => Vec::new(),
        }
    }

    /// `/jobs`: list running background jobs with their ids.
    fn show_jobs(&mut self) {
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
    fn kill_job(&mut self, id: u64) {
        if self.jobs.kill(id) {
            self.push(EntryKind::Notice, format!("Killed background job #{id}"));
        } else {
            self.push(EntryKind::Error, format!("No background job #{id}"));
        }
    }

    /// Tab completion: the first press fills the input with the highlighted
    /// candidate; subsequent presses cycle through the candidates matched by
    /// the prefix as it was when completion started.
    fn complete(&mut self, backwards: bool) {
        let was_cycling = self.comp_prefix.is_some();
        let matches = self.completions();
        if matches.is_empty() {
            self.comp_prefix = None;
            return;
        }
        if !was_cycling {
            self.comp_prefix = Some(self.input.clone());
        }
        let count = matches.len();
        self.comp_selected = self.comp_selected.min(count - 1);
        if was_cycling {
            self.comp_selected = if backwards {
                (self.comp_selected + count - 1) % count
            } else {
                (self.comp_selected + 1) % count
            };
        }
        self.input = matches[self.comp_selected].0.clone();
        self.cursor = self.input.chars().count();
    }

    /// Move the completion selection with the arrow keys (fills the input).
    fn move_completion(&mut self, up: bool) {
        let matches = self.completions();
        if matches.is_empty() {
            return;
        }
        if self.comp_prefix.is_none() {
            self.comp_prefix = Some(self.input.clone());
        }
        let count = matches.len();
        self.comp_selected = self.comp_selected.min(count - 1);
        self.comp_selected = if up {
            (self.comp_selected + count - 1) % count
        } else {
            (self.comp_selected + 1) % count
        };
        self.input = matches[self.comp_selected].0.clone();
        self.cursor = self.input.chars().count();
    }

    fn reset_completion(&mut self) {
        self.comp_selected = 0;
        self.comp_prefix = None;
        // Called on every edit — an edit also ends history browsing (the
        // recalled text stays and becomes the current input).
        self.input_history.stop();
    }

    /// `↑`/`↓` outside the completion popup: move between input lines when
    /// the cursor can, otherwise step through the submitted-message history.
    fn move_line_or_history(&mut self, up: bool) {
        let (row, _) = line_col(&self.input, self.cursor);
        let rows = self.input.split('\n').count();
        if up && row > 0 {
            self.move_input_line(true);
        } else if !up && row + 1 < rows {
            self.move_input_line(false);
        } else {
            // No history recall while the input box edits the system
            // prompt — a stray ↑ must not overwrite the draft.
            if self.prompt_edit.is_some() {
                return;
            }
            let recalled = if up {
                self.input_history.prev(&self.input)
            } else {
                self.input_history.next()
            };
            if let Some(text) = recalled {
                self.cursor = text.chars().count();
                self.input = text;
                // Not reset_completion(): that would end the browsing that
                // just moved here.
                self.comp_selected = 0;
                self.comp_prefix = None;
            }
        }
    }

    /// Shift+Tab: cycle the permission mode. Takes effect immediately, even
    /// for tool calls later in the turn currently running.
    /// Explicit mode switch via the /read-only, /edit, /plan and /bypass
    /// commands. Bypass is only reachable this way and comes with a warning.
    fn set_mode(&mut self, mode: picocode_core::config::Mode) {
        self.cfg.mode.set(mode);
        if mode == picocode_core::config::Mode::Bypass {
            self.push(
                EntryKind::Warning,
                "bypass mode: EVERY tool call now runs without confirmation (deny rules \
                 still apply). Meant for isolated environments such as containers. \
                 Shift+Tab or /read-only to leave."
                    .to_string(),
            );
        } else {
            self.push(EntryKind::Notice, format!("Mode: {}", mode.label()));
        }
    }

    fn cycle_mode(&mut self) {
        self.cfg.mode.set(self.cfg.mode.get().next());
    }

    /// `/permissions`: show what the current mode and config rules do.
    fn show_permissions(&mut self) {
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

    /// ←/→ on a `/config` row: change the value in place. Every change
    /// applies immediately.
    fn adjust_setting(&mut self, delta: i64) {
        let Some(menu) = &self.settings else { return };
        match menu.selected {
            // Same cycle as Shift+Tab; bypass stays /bypass-only, and
            // adjusting away from it lands on read-only.
            0 => self.cfg.mode.set(self.cfg.mode.get().cycled(delta)),
            1 => self.show_reasoning = !self.show_reasoning,
            2 => self.cfg.step_bash_timeout(delta),
            3 => self.cfg.step_read_lines(delta),
            4 => self.cfg.step_line_bytes(delta),
            5 => self.cfg.search.cycle_provider(delta),
            6 => self.cfg.search.step_max_results(delta),
            7 => self.cfg.step_auto_compact(delta),
            _ => {}
        }
    }

    /// Enter/Space on a `/config` row: toggles act like →; the model row
    /// closes the dialog and opens the `/model` picker.
    fn activate_setting(&mut self) {
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
    fn show_status(&mut self) {
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

    /// After a turn completes with nothing else running: compact the
    /// conversation automatically once the context usage crossed the
    /// configured threshold (`auto_compact` percent, 0 = disabled).
    /// `auto_compact_tried` keeps a failing compaction from retrying in a
    /// loop; it re-arms on the next user prompt or a successful compaction.
    fn maybe_auto_compact(&mut self) {
        let threshold = self.cfg.auto_compact.get();
        if threshold == 0 || self.auto_compact_tried {
            return;
        }
        let pct = self.context_ratio() * 100.0;
        if pct < threshold as f64 {
            return;
        }
        self.auto_compact_tried = true;
        self.close_blocks();
        self.begin_turn();
        self.push(
            EntryKind::Notice,
            format!(
                "Context {}% full (auto_compact threshold {threshold}%) — compacting the \
                 conversation…",
                pct.round()
            ),
        );
        self.send_worker_bg(WorkerCmd::Compact);
    }

    /// Current permission mode, for the status bar.
    pub fn mode(&self) -> picocode_core::config::Mode {
        self.cfg.mode.get()
    }

    /// "dir (branch)" for the input-box title.
    pub fn workdir_label(&self) -> String {
        let dir = picocode_core::git::display_dir(&self.cfg.root);
        match &self.git_branch {
            Some(branch) => format!("{dir} ({branch})"),
            None => dir,
        }
    }

    /// Fraction of the model's context window used by the latest request.
    pub fn context_ratio(&self) -> f64 {
        self.ctx_tokens as f64 / self.cfg.context_window.max(1) as f64
    }

    /// Scroll the transcript. The view is anchored to a fixed top line while
    /// scrolled up, so streaming output doesn't drag it along; scrolling past
    /// the bottom re-enables follow mode.
    fn scroll_by(&mut self, delta: i64) {
        let height = self.last_view_height;
        let max_top = self.last_total_lines.saturating_sub(height);
        if max_top == 0 {
            self.follow = true;
            return;
        }
        let current_top = if self.follow { max_top } else { self.top_line };
        let new_top = current_top
            .saturating_add_signed(delta as isize)
            .min(max_top);
        self.top_line = new_top;
        self.follow = new_top >= max_top;
    }

    /// Close the question dialog: `Some(selected)` on Enter, `None` on Esc.
    /// The tool call turns the answer into the tool result for the model.
    fn resolve_question(&mut self, accept: bool) {
        if let Some(q) = self.question.take() {
            let _ = q.respond.send(accept.then_some(q.selected));
        }
    }

    fn resolve_approval(&mut self, approve: bool) {
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
    fn resolve_approval_always(&mut self) {
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

    // ----- agent events ----------------------------------------------------

    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta(s) => {
                self.waiting = false;
                self.delta_est += 1;
                self.speed.record(1);
                if !self.assistant_open {
                    self.close_blocks();
                    self.push(EntryKind::Assistant, String::new());
                    self.assistant_open = true;
                }
                self.append_to_last(&s);
            }
            AgentEvent::ReasoningDelta(s) => {
                self.waiting = false;
                self.delta_est += 1;
                self.speed.record(1);
                if !self.reasoning_open {
                    self.close_blocks();
                    self.push(EntryKind::Reasoning, String::new());
                    self.reasoning_open = true;
                }
                self.append_to_last(&s);
            }
            AgentEvent::ToolCall { name, args } => {
                self.waiting = false;
                self.close_blocks();
                self.push_tool_call(&name, &args);
            }
            AgentEvent::ToolResult { output } => {
                // The next completion request follows right after a tool
                // result, so the run is back to waiting on the API.
                self.waiting = true;
                self.close_blocks();
                let text = clamp_lines(output.trim_end(), TOOL_OUTPUT_MAX_LINES);
                if !text.is_empty() {
                    self.push(EntryKind::ToolOut, text);
                }
            }
            AgentEvent::ApprovalRequest {
                name,
                args,
                respond,
            } => {
                // Waiting on the user now, not the API.
                self.waiting = false;
                // What "always" would whitelist: the command's prefix
                // patterns for bash, the tool name for everything else.
                let always = match picocode_core::approval::bash_command(&name, &args) {
                    Some(cmd) => {
                        let patterns = picocode_core::config::bash_allow_patterns(&cmd);
                        if patterns.is_empty() {
                            AlwaysAllow::Tool(name.clone())
                        } else {
                            AlwaysAllow::Bash(patterns)
                        }
                    }
                    None => AlwaysAllow::Tool(name.clone()),
                };
                self.pending = Some(PendingApproval {
                    name,
                    args,
                    always,
                    respond,
                });
            }
            AgentEvent::UserQuestion {
                title,
                question,
                options,
                respond,
            } => {
                self.waiting = false;
                self.question = Some(PendingQuestion {
                    title,
                    question,
                    options,
                    selected: 0,
                    respond,
                });
            }
            AgentEvent::Usage { input, output } => {
                self.ctx_tokens = input;
                // Snap the live estimate to the reported figure.
                self.turn_out += output;
                self.total_out += output;
                self.delta_est = 0;
            }
            AgentEvent::ContextBreakdown(breakdown) => {
                self.context_info = Some(breakdown);
            }
            AgentEvent::ModelList { label, result } => match result {
                Ok(mut names) => {
                    names.sort();
                    self.available_models = names;
                    self.rebuild_model_picker();
                }
                Err(e) => {
                    // Background refreshes fail silently; surface the error
                    // when a /model dialog is waiting on the list.
                    if let Some(picker) = &self.model_picker {
                        if picker.items.is_empty() {
                            self.model_picker = None;
                        }
                        self.push(
                            EntryKind::Error,
                            format!("Could not list models on {label}: {e}"),
                        );
                    }
                }
            },
            AgentEvent::FormModelList {
                provider,
                base_url,
                result,
            } => {
                if let Some(form) = &mut self.add_model {
                    // Drop stale replies from before the form changed.
                    if form.provider == provider && form.base() == base_url {
                        match result {
                            Ok(mut names) => {
                                names.sort();
                                form.note = format!("{} model(s) served", names.len());
                                form.fetched = names;
                            }
                            Err(e) => form.note = format!("fetch failed: {e}"),
                        }
                    }
                }
            }
            AgentEvent::ShellOutput { output } => {
                self.close_blocks();
                self.push(EntryKind::ToolOut, output);
            }
            AgentEvent::Pruned { outputs } => {
                self.push(
                    EntryKind::Notice,
                    format!("Trimmed {outputs} old tool outputs to save context"),
                );
            }
            AgentEvent::Undone { summary } => {
                self.close_blocks();
                if summary.is_empty() {
                    self.push(EntryKind::Notice, "Nothing to undo".to_string());
                } else {
                    self.push(EntryKind::Notice, format!("Undo:\n{summary}"));
                    self.autosave();
                }
            }
            AgentEvent::BackgroundStarted { .. } => {
                self.background_jobs += 1;
            }
            AgentEvent::BackgroundDone {
                id,
                command,
                output,
            } => {
                self.background_jobs = self.background_jobs.saturating_sub(1);
                self.close_blocks();
                self.push(
                    EntryKind::Notice,
                    format!(
                        "background job #{id} finished: $ {}",
                        compact_one_line(&command, 120)
                    ),
                );
                let text = clamp_lines(output.trim_end(), TOOL_OUTPUT_MAX_LINES);
                if !text.is_empty() {
                    self.push(EntryKind::ToolOut, text);
                }
                // Prompt the model with the result so it reacts on its own
                // (this also records the result in the history). Runs after
                // the current turn if one is streaming.
                self.begin_turn();
                self.send_worker_bg(WorkerCmd::Prompt {
                    text: format!(
                        "The bash command that was moved to background job #{id} has \
                         finished:\n$ {command}\n\nOutput:\n{output}\n\n\
                         Briefly report the result to the user and continue anything \
                         that was waiting on it."
                    ),
                    attachments: Vec::new(),
                });
            }
            AgentEvent::Cancelled => {
                self.waiting = false;
                self.speed.reset();
                self.close_blocks();
                // A cancelled stream drops the questioning tool future, so an
                // open dialog can no longer deliver its answer — close it.
                self.question = None;
                self.push(EntryKind::Notice, "Generation stopped (Esc)".to_string());
            }
            AgentEvent::Compacted { messages, summary } => {
                if messages == 0 {
                    self.push(
                        EntryKind::Notice,
                        "Nothing to compact — conversation history is empty".to_string(),
                    );
                } else {
                    // Mirror the model's new context: drop the old transcript
                    // and show what the model now remembers.
                    self.auto_compact_tried = false;
                    self.entries.clear();
                    self.close_blocks();
                    self.ctx_tokens = 0;
                    self.follow = true;
                    self.top_line = 0;
                    self.push(
                        EntryKind::Notice,
                        format!("Conversation compacted ({messages} messages → summary)"),
                    );
                    self.push(EntryKind::Summary, summary);
                }
            }
            AgentEvent::TurnComplete => {
                self.running = self.running.saturating_sub(1);
                // A queued prompt starts processing right away.
                self.waiting = self.running > 0;
                self.speed.reset();
                self.close_blocks();
                if self.running == 0 {
                    // Any dialog still open belongs to a dropped tool future.
                    self.question = None;
                    self.git_branch = picocode_core::git::branch(&self.cfg.root);
                    self.autosave();
                    self.maybe_auto_compact();
                }
            }
            AgentEvent::Error(s) => {
                self.close_blocks();
                self.push(EntryKind::Error, s);
            }
        }
    }

    // ----- helpers ---------------------------------------------------------

    fn push(&mut self, kind: EntryKind, text: String) {
        // A pushed entry ends any streaming block: otherwise deltas arriving
        // mid-stream (e.g. /status typed while the model generates) would be
        // appended to this entry instead of a fresh Assistant/Reasoning one.
        self.close_blocks();
        self.entries.push(Entry {
            kind,
            text,
            lang: None,
            attachments: Vec::new(),
        });
    }

    /// Push a Diff entry carrying the file name so the renderer can pick the
    /// right syntax for highlighting.
    fn push_diff(&mut self, text: String, lang: &str) {
        self.entries.push(Entry {
            kind: EntryKind::Diff,
            text,
            lang: Some(lang.to_string()),
            attachments: Vec::new(),
        });
    }

    /// Show a tool call: file-writing tools get a path headline plus a
    /// colored diff; everything else keeps the compact JSON args line.
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
            let diff = crate::highlight::diff_lines(old, new).join("\n");
            self.push_diff(clamp_lines(&diff, DIFF_MAX_LINES), path);
            return;
        }
        self.push(
            EntryKind::Tool,
            format!("{name} {}", compact_one_line(args, 200)),
        );
    }

    fn append_to_last(&mut self, s: &str) {
        if let Some(last) = self.entries.last_mut() {
            last.text.push_str(s);
        }
    }

    fn close_blocks(&mut self) {
        self.assistant_open = false;
        self.reasoning_open = false;
    }

    /// Ctrl+V: read the system clipboard. Copied files (Finder/Explorer)
    /// and raw image data (screenshots — saved to a temp PNG first) go
    /// through the `/attach` staging; plain text is a normal paste. The
    /// terminal's own paste keeps working independently of this.
    fn paste_clipboard(&mut self) {
        use picocode_core::clipboard::{self, Pasted};
        let dir = std::env::temp_dir().join(format!("picocode-{}", std::process::id()));
        self.clip_count += 1;
        match clipboard::read(&dir, self.clip_count) {
            Ok(Some(Pasted::Files(paths))) => {
                for path in paths {
                    self.stage_file(&path, &path.display().to_string());
                }
            }
            Ok(Some(Pasted::Image(path))) => {
                self.stage_file(&path, "clipboard image");
            }
            Ok(Some(Pasted::Text(text))) => {
                let text = text
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .replace('\t', "    ");
                self.insert_paste(text);
                self.reset_completion();
            }
            Ok(None) => self.push(
                EntryKind::Notice,
                "Clipboard is empty — copy a file, an image or text first".to_string(),
            ),
            Err(e) => self.push(EntryKind::Error, format!("Clipboard read failed: {e}")),
        }
    }

    /// Insert pasted text at the cursor: long pastes collapse into a
    /// `[Pasted text #n +N lines]` placeholder and the full text is kept
    /// aside until the message is submitted.
    fn insert_paste(&mut self, text: String) {
        match paste_placeholder(&text, self.pasted.len() + 1) {
            Some(placeholder) => {
                self.insert_str(&placeholder);
                self.pasted.push((placeholder, text));
            }
            None => self.insert_str(&text),
        }
    }

    /// If the text right before the cursor is a paste placeholder, delete it
    /// whole (Backspace).
    fn delete_placeholder_before_cursor(&mut self) -> bool {
        let byte = self.byte_index();
        let Some(len) = self
            .pasted
            .iter()
            .find(|(ph, _)| self.input[..byte].ends_with(ph.as_str()))
            .map(|(ph, _)| ph.len())
        else {
            return false;
        };
        let chars = self.input[byte - len..byte].chars().count();
        self.input.replace_range(byte - len..byte, "");
        self.cursor -= chars;
        true
    }

    /// If the text right at the cursor is a paste placeholder, delete it
    /// whole (Delete).
    fn delete_placeholder_at_cursor(&mut self) -> bool {
        let byte = self.byte_index();
        let Some(len) = self
            .pasted
            .iter()
            .find(|(ph, _)| self.input[byte..].starts_with(ph.as_str()))
            .map(|(ph, _)| ph.len())
        else {
            return false;
        };
        self.input.replace_range(byte..byte + len, "");
        true
    }

    /// Up/Down in a multi-line input: move the cursor a line, keeping the
    /// column where possible.
    fn move_input_line(&mut self, up: bool) {
        let (row, col) = line_col(&self.input, self.cursor);
        let rows = self.input.split('\n').count();
        let target = if up {
            row.checked_sub(1)
        } else {
            (row + 1 < rows).then_some(row + 1)
        };
        if let Some(r) = target {
            self.cursor = cursor_at(&self.input, r, col);
        }
    }

    fn insert_char(&mut self, c: char) {
        let i = self.byte_index();
        self.input.insert(i, c);
        self.cursor += 1;
    }

    fn insert_str(&mut self, s: &str) {
        let i = self.byte_index();
        self.input.insert_str(i, s);
        self.cursor += s.chars().count();
        self.comp_selected = 0;
    }

    fn byte_index(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }
}

/// Collapse a JSON args string into a single display line.
fn compact_one_line(s: &str, max_chars: usize) -> String {
    let s: String = s.chars().map(|c| if c == '\n' { '␤' } else { c }).collect();
    if s.chars().count() > max_chars {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…")
    } else {
        s
    }
}

fn clamp_lines(s: &str, max: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max {
        s.to_string()
    } else {
        let mut out = lines[..max].join("\n");
        out.push_str(&format!("\n… (+{} lines)", lines.len() - max));
        out
    }
}
