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
/// Pastes at least this many lines (or chars) long are collapsed into a
/// `[Pasted text #n +N lines]` placeholder in the input box.
const PASTE_COLLAPSE_LINES: usize = 6;
const PASTE_COLLAPSE_CHARS: usize = 500;

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
    ("/model", "Pick a model (dialog) or switch: /model <name>"),
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
    /// File name / language hint for syntax highlighting (Diff entries).
    #[serde(default)]
    pub lang: Option<String>,
}

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
    /// The addition, spelled as it would appear in picocode.toml.
    pub fn label(&self) -> String {
        match self {
            AlwaysAllow::Tool(name) => format!("allow_tools += {name}"),
            AlwaysAllow::Bash(patterns) => format!("allow_bash += {}", patterns.join(", ")),
        }
    }
}

/// State of the `/resume` selection dialog.
pub struct SessionPicker {
    pub sessions: Vec<session::SessionSummary>,
    pub selected: usize,
}

/// One row in the `/model` selection dialog.
pub struct ModelChoice {
    /// Name accepted by the model switch: a config entry name, or a model id
    /// the provider reported serving.
    pub name: String,
    /// Display detail: the entry's label (and URL), or the provider name for
    /// served ids.
    pub detail: String,
    pub active: bool,
}

/// State of the `/model` selection dialog.
pub struct ModelPicker {
    pub items: Vec<ModelChoice>,
    pub selected: usize,
}

/// State of the `/config` settings dialog. The rows are fixed; the values
/// are read live from the app so external changes (Shift+Tab, Ctrl+T) show
/// up while the dialog is open.
pub struct SettingsMenu {
    pub selected: usize,
}

/// Number of rows in the `/config` dialog (mode, reasoning, max turns,
/// bash timeout, model).
pub const SETTINGS_ROWS: usize = 5;

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
    /// True while a completion request is in flight but no tokens have
    /// arrived yet — the status bar shows "waiting" instead of "running".
    pub waiting: bool,
    pub spinner: usize,
    pub pending: Option<PendingApproval>,
    /// Open `ask_user` dialog, if any.
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
    pub model_label: String,
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
            waiting: false,
            spinner: 0,
            pending: None,
            question: None,
            ctx_tokens: 0,
            turn_out: 0,
            total_out: 0,
            delta_est: 0,
            model_label: cfg.model_label(),
            pasted: Vec::new(),
            available_models: Vec::new(),
            cfg: cfg.clone(),
            event_tx,
            cmd_tx,
            cancel_tx,
            session_id: session::new_id(),
            sessions_dir: session::sessions_dir(&cfg.root),
            session_picker: None,
            model_picker: None,
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
        if cfg.mode.get() == crate::config::Mode::Bypass {
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
        let Some(path) = crate::state::state_path(&self.cfg.root) else {
            return;
        };
        let _ = crate::state::save(
            &path,
            &crate::state::LastModel {
                entry: self.cfg.active_model.clone(),
                provider: crate::config::provider_name(self.cfg.provider).to_string(),
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

        // The /model dialog captures navigation keys while open.
        if let Some(picker) = &mut self.model_picker {
            let count = picker.items.len();
            match key.code {
                KeyCode::Up if count > 0 => picker.selected = (picker.selected + count - 1) % count,
                KeyCode::Down if count > 0 => picker.selected = (picker.selected + 1) % count,
                KeyCode::Enter if count > 0 => {
                    let name = picker.items[picker.selected].name.clone();
                    self.model_picker = None;
                    self.switch_model(&name).await;
                }
                KeyCode::Esc | KeyCode::Char('q') => self.model_picker = None,
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
            // Arrows drive the completion popup when it's open; in a
            // multi-line input they move the cursor between lines.
            KeyCode::Up | KeyCode::Down => {
                let up = key.code == KeyCode::Up;
                if !self.completions().is_empty() {
                    self.move_completion(up);
                } else if self.input.contains('\n') {
                    self.move_input_line(up);
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

        match text.as_str() {
            "/quit" | "/q" | "/exit" => self.should_quit = true,
            "/clear" => {
                self.entries.clear();
                self.ctx_tokens = 0;
                self.turn_out = 0;
                self.total_out = 0;
                self.delta_est = 0;
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
                    self.waiting = true;
                    self.follow = true;
                    self.turn_out = 0;
                    self.delta_est = 0;
                    self.push(EntryKind::Notice, "Compacting conversation…".to_string());
                } else {
                    self.push(EntryKind::Error, "The agent worker has stopped".to_string());
                }
            }
            "/permissions" => self.show_permissions(),
            "/config" | "/settings" => self.settings = Some(SettingsMenu { selected: 0 }),
            "/status" | "/usage" => self.show_status(),
            "/read-only" => self.set_mode(crate::config::Mode::ReadOnly),
            "/edit" => self.set_mode(crate::config::Mode::Edit),
            "/plan" => self.set_mode(crate::config::Mode::Plan),
            "/bypass" => self.set_mode(crate::config::Mode::Bypass),
            "/model" => self.open_model_picker(),
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
            _ => self.send_prompt(text.clone(), text).await,
        }
    }

    /// Send a user prompt to the agent worker. `display` is what the
    /// transcript shows (paste placeholders kept), `prompt` what the model
    /// receives (pastes expanded).
    async fn send_prompt(&mut self, display: String, prompt: String) {
        self.close_blocks();
        self.push(EntryKind::User, display);
        if self.cmd_tx.send(WorkerCmd::Prompt(prompt)).await.is_ok() {
            self.running += 1;
            self.waiting = true;
            self.follow = true;
            self.turn_out = 0;
            self.delta_est = 0;
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

        let root = self.cfg.root.clone();
        let timeout = self.cfg.bash_timeout.clone();
        let event_tx = self.event_tx.clone();
        let cmd_tx = self.cmd_tx.clone();
        let mut cancel = self.cancel_tx.subscribe();
        tokio::spawn(async move {
            use rig::tool::Tool;
            let tool = crate::tools::Bash::new(root, timeout, event_tx.clone());
            let call = tool.call(crate::tools::BashArgs {
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
            crate::config::provider_name(provider),
            crate::models::base_url(provider, base.as_deref())
        );
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = crate::models::fetch(provider, base.as_deref())
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
        self.model_picker = Some(ModelPicker { items, selected });
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
        let keep = picker.items.get(picker.selected).map(|c| c.name.clone());
        let items = self.model_choices();
        let selected = keep
            .and_then(|k| items.iter().position(|c| c.name == k))
            .or_else(|| items.iter().position(|c| c.active))
            .unwrap_or(0);
        self.model_picker = Some(ModelPicker { items, selected });
    }

    /// `/model <name>`: spawn a worker for the named entry — or for a model
    /// the provider reported serving — and carry the conversation history
    /// over to it.
    async fn switch_model(&mut self, name: &str) {
        if self.running > 0 {
            self.push(
                EntryKind::Error,
                "Cannot switch models while a turn is running".to_string(),
            );
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
                    return;
                }
                new_cfg.provider = entry.provider;
                new_cfg.model = entry.model.clone();
                new_cfg.base_url = entry.base_url.clone();
                new_cfg.active_model = Some(entry.name.clone());
                new_cfg.context_window = entry
                    .context_window
                    .unwrap_or(crate::config::DEFAULT_CONTEXT_WINDOW);
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
                new_cfg.context_window = crate::config::DEFAULT_CONTEXT_WINDOW;
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

        // The cached model list belongs to the endpoint it was fetched from.
        let endpoint_changed =
            new_cfg.provider != self.cfg.provider || new_cfg.base_url != self.cfg.base_url;
        self.cmd_tx = new_tx; // dropping the old sender shuts the old worker down
        self.cfg = new_cfg;
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

    /// `/permissions`: show what the current mode and config rules do.
    fn show_permissions(&mut self) {
        use crate::config::Mode;
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
                 confined to {}. Commands with $( ), backticks or > never auto-run. \
                 `!` commands are typed by you and skip all rules.",
                mode.label(),
                list(&rules.deny_tools),
                list(&rules.deny_bash),
                list(&rules.allow_tools),
                list(&rules.allow_bash),
                self.cfg.root.display(),
            ),
        );
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
            ("max turns", self.cfg.max_turns.get().to_string(), "← →"),
            (
                "bash timeout",
                format!("{}s", self.cfg.bash_timeout.get()),
                "← →",
            ),
            ("model", self.model_label.clone(), "Enter"),
        ]
    }

    /// ←/→ on a `/config` row: change the value in place. Every change
    /// applies immediately (max turns from the next prompt on).
    fn adjust_setting(&mut self, delta: i64) {
        let Some(menu) = &self.settings else { return };
        match menu.selected {
            // Same cycle as Shift+Tab; bypass stays /bypass-only, and
            // adjusting away from it lands on read-only.
            0 => {
                let cycle = crate::config::Mode::CYCLE;
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
                self.cfg.max_turns.set(turns.clamp(10, 200) as usize);
            }
            3 => {
                let secs = self.cfg.bash_timeout.get() as i64 + delta * 30;
                self.cfg.bash_timeout.set(secs.clamp(30, 1800) as u64);
            }
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
        let entry = match &self.cfg.active_model {
            Some(name) => format!(" — [[models]] entry `{name}`"),
            None => String::new(),
        };
        let endpoint = crate::models::base_url(self.cfg.provider, self.cfg.base_url.as_deref());
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
                 output        {} tokens this turn · {} this conversation\n\
                 session       {} — {prompts} prompts, {saved}\n\
                 project       {}\n\
                 config        {config}\n\
                 instructions  {instructions}",
                self.model_label,
                self.cfg.mode.get().label(),
                self.ctx_tokens,
                self.cfg.context_window,
                self.turn_out + self.delta_est,
                self.total_out + self.delta_est,
                self.session_id,
                self.cfg.root.display(),
            ),
        );
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
                let always = match crate::approval::bash_command(&name, &args) {
                    Some(cmd) => {
                        let patterns = crate::config::bash_allow_patterns(&cmd);
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
            AgentEvent::ShellOutput { output } => {
                self.close_blocks();
                self.push(EntryKind::ToolOut, output);
            }
            AgentEvent::BackgroundDone {
                id,
                command,
                output,
            } => {
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
                // Record it in the history so the model sees the result on
                // its next turn.
                let tx = self.cmd_tx.clone();
                tokio::spawn(async move {
                    let _ = tx
                        .send(WorkerCmd::BackgroundRecord {
                            id,
                            command,
                            output,
                        })
                        .await;
                });
            }
            AgentEvent::Cancelled => {
                self.waiting = false;
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
                // A queued prompt starts processing right away.
                self.waiting = self.running > 0;
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
        self.entries.push(Entry {
            kind,
            text,
            lang: None,
        });
    }

    /// Push a Diff entry carrying the file name so the renderer can pick the
    /// right syntax for highlighting.
    fn push_diff(&mut self, text: String, lang: &str) {
        self.entries.push(Entry {
            kind: EntryKind::Diff,
            text,
            lang: Some(lang.to_string()),
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
            && let (Some(path), Some(old), Some(new)) =
                (get("path"), get("old_string"), get("new_string"))
        {
            self.push(EntryKind::Tool, format!("{name} {path}"));
            let diff = crate::highlight::diff_lines(old, new).join("\n");
            self.push_diff(clamp_lines(&diff, DIFF_MAX_LINES), path);
            return;
        }
        if name == "write_file"
            && let (Some(path), Some(content)) = (get("path"), get("content"))
        {
            self.push(EntryKind::Tool, format!("{name} {path}"));
            let diff: Vec<String> = content.lines().map(|l| format!("+ {l}")).collect();
            self.push_diff(clamp_lines(&diff.join("\n"), DIFF_MAX_LINES), path);
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

fn spawn_input_thread() -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel(64);
    std::thread::spawn(move || {
        use ratatui::crossterm::event;
        // Key releases (reported on Windows) are ignored by the app and
        // would break the paste-run detection below, so drop them here.
        let release = |ev: &Event| matches!(ev, Event::Key(k) if k.kind == KeyEventKind::Release);
        loop {
            let Ok(first) = event::read() else {
                return;
            };
            if release(&first) {
                continue;
            }
            // Drain everything already queued: a clipboard paste delivers its
            // characters in one burst, while human keystrokes arrive one per
            // read. The batch lets `coalesce_paste` tell the two apart on
            // terminals without bracketed paste.
            let mut batch = vec![first];
            while batch.len() < 4096 && event::poll(Duration::ZERO).unwrap_or(false) {
                match event::read() {
                    Ok(ev) if release(&ev) => {}
                    Ok(ev) => batch.push(ev),
                    Err(_) => return,
                }
            }
            for ev in coalesce_paste(batch) {
                if tx.blocking_send(ev).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// The text a key event would type, if any: plain character, Enter and Tab
/// presses, and bracketed-paste events.
fn textual(ev: &Event) -> Option<String> {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            let plain = k.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            match k.code {
                KeyCode::Char(c) if plain => Some(c.to_string()),
                KeyCode::Enter if plain => Some("\n".to_string()),
                KeyCode::Tab if plain => Some("\t".to_string()),
                _ => None,
            }
        }
        Event::Paste(s) => Some(s.clone()),
        _ => None,
    }
}

/// Paste detection for terminals without bracketed paste: within a burst of
/// simultaneously-arriving events, a run of plain text keys with a newline
/// *inside* it can only be a multi-line paste. Such runs are replaced by a
/// single `Event::Paste` so the newlines are inserted instead of each Enter
/// submitting a message. Runs whose only newlines trail at the end (text
/// then Enter — e.g. keystrokes bunched up by a laggy connection, or a
/// scripted command) replay as normal key presses so the Enter still
/// submits. Anything else passes through unchanged.
fn coalesce_paste(batch: Vec<Event>) -> Vec<Event> {
    if batch.len() < 2 {
        return batch;
    }

    fn flush(out: &mut Vec<Event>, run: &mut Vec<Event>, text: &mut String) {
        if text.trim_end_matches('\n').contains('\n') {
            run.clear();
            out.push(Event::Paste(std::mem::take(text)));
        } else {
            out.append(run);
            text.clear();
        }
    }

    let mut out: Vec<Event> = Vec::new();
    let mut run: Vec<Event> = Vec::new();
    let mut text = String::new();
    for ev in batch {
        match textual(&ev) {
            Some(t) => {
                text.push_str(&t);
                run.push(ev);
            }
            None => {
                flush(&mut out, &mut run, &mut text);
                out.push(ev);
            }
        }
    }
    flush(&mut out, &mut run, &mut text);
    out
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

/// Placeholder for the n-th pasted block, or None when the paste is short
/// enough to go into the input verbatim.
fn paste_placeholder(text: &str, n: usize) -> Option<String> {
    let lines = text.lines().count().max(1);
    if lines < PASTE_COLLAPSE_LINES && text.chars().count() < PASTE_COLLAPSE_CHARS {
        return None;
    }
    let plural = if lines == 1 { "line" } else { "lines" };
    Some(format!("[Pasted text #{n} +{lines} {plural}]"))
}

/// Replace every paste placeholder in a submitted message with its full text.
/// Placeholders the user edited no longer match and are sent as-is.
fn expand_pastes(text: &str, pasted: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (placeholder, content) in pasted {
        if out.contains(placeholder.as_str()) {
            out = out.replace(placeholder.as_str(), content);
        }
    }
    out
}

/// (row, column) of a char cursor within a multi-line string, in chars.
pub fn line_col(text: &str, cursor: usize) -> (usize, usize) {
    let mut row = 0;
    let mut col = 0;
    for c in text.chars().take(cursor) {
        if c == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

/// Rows for the `/model` dialog: the configured `[[models]]` entries first,
/// then the models the provider reported serving, skipping ids an entry
/// already covers (same name, or same model on the same endpoint).
fn model_choices(
    models: &[crate::config::ModelEntry],
    active_model: Option<&str>,
    provider: crate::config::Provider,
    base_url: Option<&str>,
    current_model: &str,
    available: &[String],
) -> Vec<ModelChoice> {
    let mut items: Vec<ModelChoice> = models
        .iter()
        .map(|m| {
            let mut detail = m.label();
            if let Some(url) = &m.base_url {
                detail.push_str(&format!(" @ {url}"));
            }
            ModelChoice {
                name: m.name.clone(),
                detail,
                active: active_model == Some(m.name.as_str()),
            }
        })
        .collect();
    for id in available {
        let covered = models.iter().any(|m| {
            m.name == *id
                || (m.model == *id && m.provider == provider && m.base_url.as_deref() == base_url)
        });
        if !covered {
            items.push(ModelChoice {
                name: id.clone(),
                detail: crate::config::provider_name(provider).to_string(),
                active: active_model.is_none() && current_model == id,
            });
        }
    }
    items
}

/// Char cursor for a (row, column) position, clamping the column to the
/// line's length (used by Up/Down in the input box).
pub fn cursor_at(text: &str, row: usize, col: usize) -> usize {
    let mut cursor = 0;
    for (i, line) in text.split('\n').enumerate() {
        let len = line.chars().count();
        if i == row {
            return cursor + col.min(len);
        }
        cursor += len + 1;
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_tracks_newlines() {
        assert_eq!(line_col("abc", 2), (0, 2));
        assert_eq!(line_col("ab\ncd", 3), (1, 0));
        assert_eq!(line_col("ab\ncd", 5), (1, 2));
        assert_eq!(line_col("", 0), (0, 0));
        // Multi-byte chars count as one column.
        assert_eq!(line_col("あい\nう", 4), (1, 1));
    }

    #[test]
    fn cursor_at_clamps_to_line_length() {
        let text = "long line\nab\nmiddle";
        assert_eq!(cursor_at(text, 0, 4), 4);
        // Column clamped to the shorter line.
        assert_eq!(cursor_at(text, 1, 7), 12);
        assert_eq!(cursor_at(text, 2, 0), 13);
        // A row past the end lands at the end of the text.
        assert_eq!(cursor_at(text, 9, 0), text.chars().count());
    }

    #[test]
    fn model_choices_merge_config_and_served_models() {
        use crate::config::{ModelEntry, Provider};
        let models = vec![
            ModelEntry {
                name: "local".into(),
                provider: Provider::Ollama,
                model: "qwen3:4b".into(),
                base_url: None,
                context_window: None,
            },
            ModelEntry {
                name: "vllm".into(),
                provider: Provider::Openai,
                model: "qwen3:8b".into(),
                base_url: Some("http://host:8000/v1".into()),
                context_window: None,
            },
        ];
        let available = vec!["qwen3:0.6b".into(), "qwen3:4b".into()];
        let items = model_choices(
            &models,
            Some("local"),
            Provider::Ollama,
            None,
            "qwen3:4b",
            &available,
        );
        // Config entries first; qwen3:4b is covered by `local` on the same
        // endpoint, so only qwen3:0.6b is appended.
        let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["local", "vllm", "qwen3:0.6b"]);
        assert!(items[0].active);
        assert!(!items[2].active);
        assert_eq!(items[1].detail, "openai/qwen3:8b @ http://host:8000/v1");
        assert_eq!(items[2].detail, "ollama");

        // Ad-hoc selection: no active entry, the current model id is marked.
        let items = model_choices(
            &models,
            None,
            Provider::Ollama,
            None,
            "qwen3:0.6b",
            &available,
        );
        assert!(items.iter().any(|c| c.name == "qwen3:0.6b" && c.active));
        assert!(items.iter().all(|c| c.name != "local" || !c.active));
    }

    #[test]
    fn long_pastes_collapse_into_placeholders() {
        // Short pastes stay verbatim.
        assert_eq!(paste_placeholder("one\ntwo", 1), None);
        assert_eq!(paste_placeholder("short", 3), None);
        // Collapse by line count…
        let six_lines = "a\nb\nc\nd\ne\nf";
        assert_eq!(
            paste_placeholder(six_lines, 1).as_deref(),
            Some("[Pasted text #1 +6 lines]")
        );
        // …or by size, even on a single line.
        let big = "x".repeat(600);
        assert_eq!(
            paste_placeholder(&big, 2).as_deref(),
            Some("[Pasted text #2 +1 line]")
        );

        let pasted = vec![
            (
                "[Pasted text #1 +6 lines]".to_string(),
                six_lines.to_string(),
            ),
            ("[Pasted text #2 +1 line]".to_string(), big.clone()),
        ];
        // Expansion swaps every placeholder for its full text.
        assert_eq!(
            expand_pastes("see [Pasted text #1 +6 lines] end", &pasted),
            format!("see {six_lines} end")
        );
        assert_eq!(
            expand_pastes(
                "[Pasted text #1 +6 lines]\n[Pasted text #2 +1 line]",
                &pasted
            ),
            format!("{six_lines}\n{big}")
        );
        // Edited placeholders no longer match and are left alone.
        assert_eq!(
            expand_pastes("[Pasted text #1 +6 line]", &pasted),
            "[Pasted text #1 +6 line]"
        );
    }

    #[test]
    fn paste_bursts_coalesce_into_paste_events() {
        let key = |c: char| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let enter = || Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let up = || Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        // A burst with a newline inside the text can only be a paste.
        let out = coalesce_paste(vec![key('a'), key('b'), enter(), key('c')]);
        assert!(matches!(&out[..], [Event::Paste(s)] if s.as_str() == "ab\nc"));

        // A lone Enter keeps submitting.
        let out = coalesce_paste(vec![enter()]);
        assert!(matches!(&out[..], [Event::Key(_)]));

        // A fast burst without newlines passes through unchanged.
        let out = coalesce_paste(vec![key('h'), key('i')]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| matches!(e, Event::Key(_))));

        // Text with only a trailing Enter is a typed command bunched up in
        // transit (or a scripted one) — it replays and still submits.
        let out = coalesce_paste(vec![key('l'), key('s'), enter()]);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|e| matches!(e, Event::Key(_))));

        // Non-text events break the run.
        let out = coalesce_paste(vec![key('a'), enter(), key('b'), up(), key('c')]);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], Event::Paste(s) if s.as_str() == "a\nb"));
        assert!(matches!(&out[1], Event::Key(k) if k.code == KeyCode::Up));
        assert!(matches!(&out[2], Event::Key(k) if k.code == KeyCode::Char('c')));

        // A bracketed-paste event merges with keys from the same burst.
        let out = coalesce_paste(vec![Event::Paste("x\ny".into()), key('z')]);
        assert!(matches!(&out[..], [Event::Paste(s)] if s.as_str() == "x\nyz"));

        // Modified keys (e.g. Ctrl+C in a burst) are never swallowed.
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let out = coalesce_paste(vec![key('a'), enter(), ctrl_c]);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[2], Event::Key(k) if k.modifiers == KeyModifiers::CONTROL));
    }

    #[test]
    fn line_col_and_cursor_at_roundtrip() {
        let text = "one\ntwo three\nよん";
        for cursor in 0..=text.chars().count() {
            let (row, col) = line_col(text, cursor);
            assert_eq!(cursor_at(text, row, col), cursor);
        }
    }
}
