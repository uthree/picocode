//! Application state and the main event loop.

use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::event::{AgentEvent, WorkerCmd};

const TOOL_OUTPUT_MAX_LINES: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    ToolOut,
    Notice,
    Error,
}

pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
}

pub struct PendingApproval {
    pub name: String,
    pub args: String,
    respond: oneshot::Sender<bool>,
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
    /// Number of prompts submitted but not yet completed.
    pub running: usize,
    pub spinner: usize,
    pub pending: Option<PendingApproval>,
    /// Context size (input tokens) of the latest completion request.
    pub ctx_tokens: u64,
    /// Total output tokens across the session.
    pub out_tokens: u64,
    pub model_label: String,
    should_quit: bool,
    assistant_open: bool,
    reasoning_open: bool,
}

impl App {
    pub fn new(cfg: &Config) -> Self {
        let mut app = Self {
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            follow: true,
            top_line: 0,
            last_total_lines: 0,
            last_view_height: 0,
            show_reasoning: false,
            running: 0,
            spinner: 0,
            pending: None,
            ctx_tokens: 0,
            out_tokens: 0,
            model_label: cfg.model_label(),
            should_quit: false,
            assistant_open: false,
            reasoning_open: false,
        };
        app.push(EntryKind::Notice, format!("picocode — {} (cwd: {})", app.model_label, cfg.root.display()));
        if cfg.yolo {
            app.push(EntryKind::Notice, "--yolo: skipping all tool approvals".to_string());
        }
        app
    }

    pub async fn run(
        mut self,
        mut terminal: DefaultTerminal,
        mut agent_rx: mpsc::Receiver<AgentEvent>,
        cmd_tx: mpsc::Sender<WorkerCmd>,
    ) -> anyhow::Result<()> {
        let mut term_rx = spawn_input_thread();
        let mut tick = tokio::time::interval(Duration::from_millis(120));

        loop {
            terminal.draw(|f| crate::ui::draw(f, &mut self))?;
            tokio::select! {
                Some(ev) = term_rx.recv() => self.handle_terminal_event(ev, &cmd_tx).await,
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

    async fn handle_terminal_event(&mut self, ev: Event, cmd_tx: &mpsc::Sender<WorkerCmd>) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key, cmd_tx).await;
            }
            Event::Paste(text) => {
                let text = text.replace(['\r', '\n'], " ");
                self.insert_str(&text);
            }
            _ => {}
        }
    }

    async fn handle_key(&mut self, key: KeyEvent, cmd_tx: &mpsc::Sender<WorkerCmd>) {
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
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.resolve_approval(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.resolve_approval(false),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Enter => self.submit(cmd_tx).await,
            KeyCode::Char(c) if !ctrl => self.insert_char(c),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let i = self.byte_index();
                    self.input.remove(i);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let i = self.byte_index();
                    self.input.remove(i);
                }
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

    async fn submit(&mut self, cmd_tx: &mpsc::Sender<WorkerCmd>) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;

        match text.as_str() {
            "/quit" | "/q" | "/exit" => self.should_quit = true,
            "/clear" => {
                self.entries.clear();
                self.ctx_tokens = 0;
                self.assistant_open = false;
                self.reasoning_open = false;
                self.follow = true;
                self.top_line = 0;
                let _ = cmd_tx.send(WorkerCmd::Clear).await;
                self.push(EntryKind::Notice, "Conversation history cleared".to_string());
            }
            _ => {
                self.close_blocks();
                self.push(EntryKind::User, text.clone());
                if cmd_tx.send(WorkerCmd::Prompt(text)).await.is_ok() {
                    self.running += 1;
                    self.follow = true;
                } else {
                    self.push(EntryKind::Error, "The agent worker has stopped".to_string());
                }
            }
        }
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
        let new_top = current_top.saturating_add_signed(delta as isize).min(max_top);
        self.top_line = new_top;
        self.follow = new_top >= max_top;
    }

    fn resolve_approval(&mut self, approve: bool) {
        if let Some(p) = self.pending.take() {
            let label = if approve { "✔ approved" } else { "✘ denied" };
            self.push(EntryKind::Notice, format!("{label}: {}", p.name));
            let _ = p.respond.send(approve);
        }
    }

    // ----- agent events ----------------------------------------------------

    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta(s) => {
                if !self.assistant_open {
                    self.close_blocks();
                    self.push(EntryKind::Assistant, String::new());
                    self.assistant_open = true;
                }
                self.append_to_last(&s);
            }
            AgentEvent::ReasoningDelta(s) => {
                if !self.reasoning_open {
                    self.close_blocks();
                    self.push(EntryKind::Reasoning, String::new());
                    self.reasoning_open = true;
                }
                self.append_to_last(&s);
            }
            AgentEvent::ToolCall { name, args } => {
                self.close_blocks();
                let args = compact_one_line(&args, 200);
                self.push(EntryKind::Tool, format!("{name} {args}"));
            }
            AgentEvent::ToolResult { output } => {
                self.close_blocks();
                let text = clamp_lines(output.trim_end(), TOOL_OUTPUT_MAX_LINES);
                if !text.is_empty() {
                    self.push(EntryKind::ToolOut, text);
                }
            }
            AgentEvent::ApprovalRequest { name, args, respond } => {
                self.pending = Some(PendingApproval { name, args, respond });
            }
            AgentEvent::Usage { input, output } => {
                self.ctx_tokens = input;
                self.out_tokens += output;
            }
            AgentEvent::TurnComplete => {
                self.running = self.running.saturating_sub(1);
                self.close_blocks();
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
