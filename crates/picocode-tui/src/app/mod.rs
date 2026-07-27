//! Application state and the main event loop.
//!
//! The interaction logic is split across child modules — all further
//! `impl App` blocks on the same struct: [`dialogs`] (dialog state types,
//! `/config`, `/status`, `/jobs` and the approval answers), [`editor`]
//! (input-box editing, completion and attachment staging), [`events`]
//! (agent-event handling), [`models`] (model switching and the add-model
//! form), [`prompt`] (system-prompt editing and presets), [`sessions`]
//! (`/resume` and autosave) and [`workspace`] (the `/remote` switch and
//! its dialogs).

mod dialogs;
mod editor;
mod events;
mod models;
mod prompt;
mod sessions;
mod workspace;

pub use dialogs::{
    AddModelForm, AddRemoteForm, AlwaysAllow, ModelPicker, PendingApproval, PendingQuestion,
    RemotePicker, SETTINGS_ROWS, SessionPicker, SettingsMenu,
};

use std::path::PathBuf;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use tokio::sync::{mpsc, watch};

use picocode_core::config::Config;
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::session;
pub use picocode_core::transcript::{Entry, EntryKind};

use crate::history::InputHistory;
use crate::input::{expand_pastes, spawn_input_thread};

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
    /// Workspace backend (local or remote), reused across respawns.
    backend: picocode_core::backend::Backend,
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
    /// Open `/remote` dialog, if any (captures the arrow/Enter keys).
    pub remote_picker: Option<RemotePicker>,
    /// Open add-remote form (reached from the `/remote` dialog), if any.
    pub add_remote: Option<AddRemoteForm>,
    /// Open `/config` dialog, if any (captures the arrow/Enter keys).
    pub settings: Option<SettingsMenu>,
    should_quit: bool,
    assistant_open: bool,
    reasoning_open: bool,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &Config,
        event_tx: mpsc::Sender<AgentEvent>,
        cmd_tx: mpsc::Sender<WorkerCmd>,
        steer: picocode_core::steer::SteerQueue,
        jobs: picocode_core::tools::BackgroundJobs,
        cancel_tx: watch::Sender<()>,
        mcp: picocode_core::mcp::McpConnections,
        backend: picocode_core::backend::Backend,
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
            backend,
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
            sessions_dir: session::sessions_dir_for(cfg),
            session_picker: None,
            model_picker: None,
            add_model: None,
            remote_picker: None,
            add_remote: None,
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
        picocode_core::state::save_last_model(&app.cfg);
        app
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

        // The add-remote form captures keys while open.
        if self.add_remote.is_some() {
            self.add_remote_key(key).await;
            return;
        }

        // The /remote dialog captures navigation keys while open.
        if let Some(picker) = &mut self.remote_picker {
            let count = picker.items.len() + 1;
            match key.code {
                KeyCode::Up => picker.selected = (picker.selected + count - 1) % count,
                KeyCode::Down => picker.selected = (picker.selected + 1) % count,
                KeyCode::Enter => {
                    let selected = picker.selected;
                    let name = picker.items.get(selected).map(|c| c.name.clone());
                    self.remote_picker = None;
                    match name {
                        Some(name) => {
                            self.switch_workspace(&name).await;
                        }
                        None => self.open_add_remote(),
                    }
                }
                KeyCode::Esc => self.remote_picker = None,
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
                Command::Remote(None) => self.open_remote_picker(),
                Command::Remote(Some(target)) => {
                    self.switch_workspace(&target).await;
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

        // User-typed `!` commands run unsandboxed by design.
        tokio::spawn(picocode_core::tools::user_shell(
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
            "(stopped by Esc before finishing)",
        ));
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
        // A remote workspace shows host:path so the box makes the target
        // obvious; local shows the shortened directory.
        let dir = match &self.cfg.remote {
            Some(spec) => format!("{}:{}", self.backend.label(), spec.path.display()),
            None => picocode_core::git::display_dir(&self.cfg.root),
        };
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
