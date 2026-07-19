//! Application state and the main event loop.

use std::path::PathBuf;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::Config;
use crate::event::{AgentEvent, WorkerCmd};
use crate::session;

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

/// Slash commands with a short description, used by the completion popup.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/clear", "Clear conversation history"),
    ("/compact", "Summarize history to free context"),
    ("/model", "List models or switch: /model <name>"),
    ("/resume", "Pick a saved session to resume"),
    ("/read-only", "Mode: reads only, every write asks"),
    ("/edit", "Mode: file writes run freely"),
    ("/plan", "Mode: investigate and plan, writes blocked"),
    (
        "/bypass",
        "Mode: run EVERYTHING unconfirmed (isolated envs)",
    ),
    ("/quit", "Exit picocode"),
    ("/exit", "Exit picocode"),
];

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    ToolOut,
    /// A colored line diff shown for file-writing tool calls ("+ "/"- "/"  "
    /// prefixed lines).
    Diff,
    Notice,
    /// A prominent warning (e.g. entering bypass mode).
    Warning,
    /// The conversation summary produced by /compact.
    Summary,
    Error,
    /// Rendered verbatim without wrapping (startup logo).
    Logo,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
}

pub struct PendingApproval {
    pub name: String,
    pub args: String,
    respond: oneshot::Sender<bool>,
}

/// State of the `/resume` selection dialog.
pub struct SessionPicker {
    pub sessions: Vec<session::SessionSummary>,
    pub selected: usize,
}

/// State of the `ask_user` / `submit_plan` option dialog.
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
    /// Number of prompts submitted but not yet completed.
    pub running: usize,
    pub spinner: usize,
    pub pending: Option<PendingApproval>,
    /// Open `ask_user` dialog, if any.
    pub question: Option<PendingQuestion>,
    /// Context size (input tokens) of the latest completion request.
    pub ctx_tokens: u64,
    /// Total output tokens across the session.
    pub out_tokens: u64,
    /// Output tokens reported for the turn in progress.
    pub turn_out: u64,
    /// Streamed deltas since the last usage report — a live estimate of
    /// decoded tokens between usage updates.
    pub delta_est: u64,
    pub model_label: String,
    /// Active config; provider/model/base_url track the current /model choice.
    cfg: Config,
    /// Event channel handed to newly spawned workers on model switch.
    event_tx: mpsc::Sender<AgentEvent>,
    /// Command channel of the current worker (replaced on model switch).
    cmd_tx: mpsc::Sender<WorkerCmd>,
    /// Signals the worker to abort the generation in progress (Esc).
    cancel_tx: watch::Sender<()>,
    /// Id of the session being written; a fresh one is issued by /clear.
    session_id: String,
    /// Where sessions are stored (None disables persistence, e.g. no $HOME).
    sessions_dir: Option<PathBuf>,
    /// Open `/resume` dialog, if any (captures the arrow/Enter keys).
    pub session_picker: Option<SessionPicker>,
    should_quit: bool,
    assistant_open: bool,
    reasoning_open: bool,
}

impl App {
    pub fn new(
        cfg: &Config,
        event_tx: mpsc::Sender<AgentEvent>,
        cmd_tx: mpsc::Sender<WorkerCmd>,
        cancel_tx: watch::Sender<()>,
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
            running: 0,
            spinner: 0,
            pending: None,
            question: None,
            ctx_tokens: 0,
            out_tokens: 0,
            turn_out: 0,
            delta_est: 0,
            model_label: cfg.model_label(),
            cfg: cfg.clone(),
            event_tx,
            cmd_tx,
            cancel_tx,
            session_id: session::new_id(),
            sessions_dir: session::sessions_dir(&cfg.root),
            session_picker: None,
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
        if cfg.yolo {
            app.push(
                EntryKind::Notice,
                "--yolo: skipping all tool approvals".to_string(),
            );
        }
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
                let text = text.replace(['\r', '\n'], " ");
                self.insert_str(&text);
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

        // Approval modal captures y/n while pending.
        if self.pending.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.resolve_approval(true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.resolve_approval(false)
                }
                _ => {}
            }
            return;
        }

        // The ask_user dialog captures navigation keys while open.
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

        match key.code {
            KeyCode::Enter => self.submit().await,
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
            KeyCode::Up | KeyCode::Down => self.move_completion(key.code == KeyCode::Up),
            KeyCode::Char(c) if !ctrl => {
                self.insert_char(c);
                self.reset_completion();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let i = self.byte_index();
                    self.input.remove(i);
                    self.reset_completion();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let i = self.byte_index();
                    self.input.remove(i);
                    self.reset_completion();
                }
            }
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
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.reset_completion();

        match text.as_str() {
            "/quit" | "/q" | "/exit" => self.should_quit = true,
            "/clear" => {
                self.entries.clear();
                self.ctx_tokens = 0;
                self.assistant_open = false;
                self.reasoning_open = false;
                self.follow = true;
                self.top_line = 0;
                let _ = self.cmd_tx.send(WorkerCmd::Clear).await;
                // The cleared conversation stays on disk; start a fresh log.
                self.session_id = session::new_id();
                self.push(EntryKind::Logo, LOGO.to_string());
                self.push(
                    EntryKind::Notice,
                    "Conversation history cleared".to_string(),
                );
            }
            "/compact" => {
                self.close_blocks();
                if self.cmd_tx.send(WorkerCmd::Compact).await.is_ok() {
                    self.running += 1;
                    self.follow = true;
                    self.turn_out = 0;
                    self.delta_est = 0;
                    self.push(EntryKind::Notice, "Compacting conversation…".to_string());
                } else {
                    self.push(EntryKind::Error, "The agent worker has stopped".to_string());
                }
            }
            "/read-only" => self.set_mode(crate::config::Mode::ReadOnly),
            "/edit" => self.set_mode(crate::config::Mode::Edit),
            "/plan" => self.set_mode(crate::config::Mode::Plan),
            "/bypass" => self.set_mode(crate::config::Mode::Bypass),
            "/model" => self.list_models(),
            _ if text.starts_with("/model ") => {
                let name = text["/model ".len()..].trim().to_string();
                self.switch_model(&name).await;
            }
            "/resume" => self.open_session_picker(),
            _ if text.starts_with("/resume ") => {
                let id = text["/resume ".len()..].trim().to_string();
                self.resume_session(&id).await;
            }
            _ if text.starts_with('/') && !text.contains(' ') => {
                self.push(EntryKind::Error, format!("Unknown command: {text}"));
            }
            _ if text.starts_with('!') => self.run_shell(text),
            _ => {
                self.close_blocks();
                self.push(EntryKind::User, text.clone());
                if self.cmd_tx.send(WorkerCmd::Prompt(text)).await.is_ok() {
                    self.running += 1;
                    self.follow = true;
                    self.turn_out = 0;
                    self.delta_est = 0;
                } else {
                    self.push(EntryKind::Error, "The agent worker has stopped".to_string());
                }
            }
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
        let event_tx = self.event_tx.clone();
        let cmd_tx = self.cmd_tx.clone();
        tokio::spawn(async move {
            use rig::tool::Tool;
            let output = match crate::tools::Bash::new(root)
                .call(crate::tools::BashArgs {
                    command: command.clone(),
                })
                .await
            {
                Ok(out) => out,
                Err(e) => format!("error: {e}"),
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

    /// `/model` with no argument: list the configured model entries.
    fn list_models(&mut self) {
        if self.cfg.models.is_empty() {
            self.push(
                EntryKind::Notice,
                format!(
                    "No [[models]] entries in picocode.toml — using {}",
                    self.model_label
                ),
            );
            return;
        }
        let mut out = String::from("Models (/model <name> to switch):");
        for m in &self.cfg.models {
            let marker = if self.cfg.active_model.as_deref() == Some(&m.name) {
                "▸"
            } else {
                " "
            };
            out.push_str(&format!("\n{marker} {} — {}", m.name, m.label()));
            if let Some(url) = &m.base_url {
                out.push_str(&format!(" @ {url}"));
            }
        }
        self.push(EntryKind::Notice, out);
    }

    /// `/model <name>`: spawn a worker for the named entry and carry the
    /// conversation history over to it.
    async fn switch_model(&mut self, name: &str) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
            return;
        }
        let Some(entry) = self.cfg.models.iter().find(|m| m.name == name).cloned() else {
            let names: Vec<&str> = self.cfg.models.iter().map(|m| m.name.as_str()).collect();
            let hint = if names.is_empty() {
                "no [[models]] entries are configured".to_string()
            } else {
                format!("available: {}", names.join(", "))
            };
            self.push(EntryKind::Error, format!("Unknown model `{name}` — {hint}"));
            return;
        };
        if self.cfg.active_model.as_deref() == Some(name) {
            self.push(
                EntryKind::Notice,
                format!("Already using {name} ({})", entry.label()),
            );
            return;
        }

        let mut new_cfg = self.cfg.clone();
        new_cfg.provider = entry.provider;
        new_cfg.model = entry.model.clone();
        new_cfg.base_url = entry.base_url.clone();
        new_cfg.active_model = Some(entry.name.clone());
        new_cfg.context_window = entry
            .context_window
            .unwrap_or(crate::config::DEFAULT_CONTEXT_WINDOW);

        // Spawn first so a failure (e.g. missing API key) leaves the current
        // worker untouched.
        let new_tx = match crate::agent::spawn(
            &new_cfg,
            self.event_tx.clone(),
            self.cancel_tx.subscribe(),
        ) {
            Ok(tx) => tx,
            Err(e) => {
                self.push(
                    EntryKind::Error,
                    format!("Failed to switch to `{name}`: {e:#}"),
                );
                return;
            }
        };

        // Carry the conversation over to the new worker.
        let (htx, hrx) = oneshot::channel();
        if self.cmd_tx.send(WorkerCmd::TakeHistory(htx)).await.is_ok()
            && let Ok(history) = hrx.await
        {
            let _ = new_tx.send(WorkerCmd::SeedHistory(history)).await;
        }

        self.cmd_tx = new_tx; // dropping the old sender shuts the old worker down
        self.cfg = new_cfg;
        self.model_label = self.cfg.model_label();
        self.push(
            EntryKind::Notice,
            format!("Model switched to {name} ({})", self.model_label),
        );
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

    /// Slash-command candidates for the completion popup. Uses the locked
    /// prefix while cycling, otherwise the current input.
    pub fn completions(&self) -> Vec<(&'static str, &'static str)> {
        let filter = self.comp_prefix.as_deref().unwrap_or(&self.input);
        if !filter.starts_with('/') || filter.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(filter))
            .collect()
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
        self.input = matches[self.comp_selected].0.to_string();
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
        self.input = matches[self.comp_selected].0.to_string();
        self.cursor = self.input.chars().count();
    }

    fn reset_completion(&mut self) {
        self.comp_selected = 0;
        self.comp_prefix = None;
    }

    /// Shift+Tab: cycle the permission mode. Takes effect immediately, even
    /// for tool calls later in the turn currently running.
    /// Explicit mode switch via the /read-only, /edit, /plan and /bypass
    /// commands. Bypass is only reachable this way and comes with a warning.
    fn set_mode(&mut self, mode: crate::config::Mode) {
        self.cfg.mode.set(mode);
        if mode == crate::config::Mode::Bypass {
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

    /// Current permission mode, for the status bar.
    pub fn mode(&self) -> crate::config::Mode {
        self.cfg.mode.get()
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

    /// Close the ask_user dialog: `Some(selected)` on Enter, `None` on Esc.
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

    // ----- agent events ----------------------------------------------------

    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta(s) => {
                self.delta_est += 1;
                if !self.assistant_open {
                    self.close_blocks();
                    self.push(EntryKind::Assistant, String::new());
                    self.assistant_open = true;
                }
                self.append_to_last(&s);
            }
            AgentEvent::ReasoningDelta(s) => {
                self.delta_est += 1;
                if !self.reasoning_open {
                    self.close_blocks();
                    self.push(EntryKind::Reasoning, String::new());
                    self.reasoning_open = true;
                }
                self.append_to_last(&s);
            }
            AgentEvent::ToolCall { name, args } => {
                self.close_blocks();
                self.push_tool_call(&name, &args);
            }
            AgentEvent::ToolResult { output } => {
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
                self.pending = Some(PendingApproval {
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
                self.out_tokens += output;
                // Snap the live estimate to the reported figure.
                self.turn_out += output;
                self.delta_est = 0;
            }
            AgentEvent::ShellOutput { output } => {
                self.close_blocks();
                self.push(EntryKind::ToolOut, output);
            }
            AgentEvent::Cancelled => {
                self.close_blocks();
                // A cancelled stream drops the ask_user tool future, so an
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
                self.close_blocks();
                if self.running == 0 {
                    // Any dialog still open belongs to a dropped tool future.
                    self.question = None;
                    self.autosave();
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
        self.entries.push(Entry { kind, text });
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
            && let (Some(path), Some(old), Some(new)) =
                (get("path"), get("old_string"), get("new_string"))
        {
            self.push(EntryKind::Tool, format!("{name} {path}"));
            let diff = crate::highlight::diff_lines(old, new).join("\n");
            self.push(EntryKind::Diff, clamp_lines(&diff, DIFF_MAX_LINES));
            return;
        }
        if name == "write_file"
            && let (Some(path), Some(content)) = (get("path"), get("content"))
        {
            self.push(EntryKind::Tool, format!("{name} {path}"));
            let diff: Vec<String> = content.lines().map(|l| format!("+ {l}")).collect();
            self.push(
                EntryKind::Diff,
                clamp_lines(&diff.join("\n"), DIFF_MAX_LINES),
            );
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

fn spawn_input_thread() -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || {
        while let Ok(ev) = ratatui::crossterm::event::read() {
            if tx.blocking_send(ev).is_err() {
                break;
            }
        }
    });
    rx
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
