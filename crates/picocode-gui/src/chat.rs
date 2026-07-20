//! The chat view: transcript, input row, status bar, and the approval /
//! question dialogs — a gpui rendering of picocode-core's event stream.

use gpui::prelude::*;
use gpui::{AnyElement, ClickEvent, Context, Entity, ScrollHandle, SharedString, Window, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, StyledExt};
use tokio::sync::{mpsc, oneshot, watch};

use picocode_core::approval;
use picocode_core::config::{self, Config, Mode};
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::transcript::{Entry, EntryKind};

const TOOL_OUTPUT_MAX_LINES: usize = 12;

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
    cmd_tx: mpsc::Sender<WorkerCmd>,
    cancel_tx: watch::Sender<()>,
    running: bool,
    approval: Option<Approval>,
    question: Option<Question>,
    /// Context tokens of the last completion request / output tokens so far.
    tokens_in: u64,
    tokens_out: u64,
    scroll: ScrollHandle,
}

impl ChatView {
    pub fn new(
        cfg: Config,
        mut event_rx: mpsc::Receiver<AgentEvent>,
        cmd_tx: mpsc::Sender<WorkerCmd>,
        cancel_tx: watch::Sender<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Type a message — Enter to send"));
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

        Self {
            cfg,
            entries: Vec::new(),
            input,
            cmd_tx,
            cancel_tx,
            running: false,
            approval: None,
            question: None,
            tokens_in: 0,
            tokens_out: 0,
            scroll: ScrollHandle::new(),
        }
    }

    // ---------- events from the agent worker ----------

    fn on_agent_event(&mut self, ev: AgentEvent, _cx: &mut Context<Self>) {
        match ev {
            AgentEvent::TextDelta(s) => self.append(EntryKind::Assistant, &s),
            AgentEvent::ReasoningDelta(s) => self.append(EntryKind::Reasoning, &s),
            AgentEvent::ToolCall { name, args } => {
                self.push(EntryKind::Tool, format!("{name} {}", one_line(&args, 160)));
            }
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
            AgentEvent::ModelList { .. } => {}
            AgentEvent::Compacted { messages, summary } => {
                if messages == 0 {
                    self.push(EntryKind::Notice, "nothing to compact".to_string());
                } else {
                    self.push(EntryKind::Notice, format!("compacted {messages} messages"));
                    self.push(EntryKind::Summary, summary);
                }
                self.running = false;
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
            AgentEvent::TurnComplete => self.running = false,
            AgentEvent::Error(e) => self.push(EntryKind::Error, e),
        }
        self.scroll.scroll_to_bottom();
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
        if let InputEvent::PressEnter { .. } = ev {
            self.submit(window, cx);
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.approval.is_some() || self.question.is_some() {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));

        match text.as_str() {
            "/clear" => {
                let _ = self.cmd_tx.try_send(WorkerCmd::Clear);
                self.entries.clear();
                self.tokens_in = 0;
                self.tokens_out = 0;
                self.push(EntryKind::Notice, "conversation cleared".to_string());
            }
            "/compact" => {
                let _ = self.cmd_tx.try_send(WorkerCmd::Compact);
                self.push(EntryKind::Notice, "compacting…".to_string());
                self.running = true;
            }
            "/quit" | "/exit" => cx.quit(),
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
                SharedString::from(entry.text.clone()),
                window,
                cx,
            )
            .into_any_element(),
            EntryKind::Reasoning => div()
                .italic()
                .text_sm()
                .text_color(muted)
                .child(entry.text.clone())
                .into_any_element(),
            EntryKind::Tool => div()
                .font_family(mono)
                .text_sm()
                .text_color(muted)
                .child(format!("⚙ {}", entry.text))
                .into_any_element(),
            EntryKind::ToolOut | EntryKind::Diff => div()
                .font_family(mono)
                .text_sm()
                .text_color(muted)
                .pl_4()
                .child(entry.text.clone())
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
                                .font_family(theme.mono_font_family.clone())
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(clip(&a.args, 20)),
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

    fn status_line(&self) -> String {
        let mode = match self.cfg.mode.get() {
            Mode::ReadOnly => "read-only",
            Mode::Edit => "edit",
            Mode::Plan => "plan",
            Mode::Bypass => "bypass",
        };
        let state = if self.running {
            "● running"
        } else {
            "● idle"
        };
        format!("[{mode}]  {state}")
    }

    fn usage_line(&self) -> String {
        let pct = if self.cfg.context_window > 0 {
            (self.tokens_in as f64 / self.cfg.context_window as f64 * 100.0).round() as u64
        } else {
            0
        };
        format!(
            "↑ {} ↓ {}  {pct}%  {}",
            self.tokens_in,
            self.tokens_out,
            self.cfg.model_label()
        )
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let border = theme.border;
        let muted_fg = theme.muted_foreground;

        let mut items: Vec<AnyElement> = Vec::new();
        for (ix, entry) in self.entries.iter().enumerate() {
            items.push(Self::render_entry(entry, ix, window, cx));
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
            .size_full()
            .bg(background)
            .child(
                div()
                    .id("transcript")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_4()
                    .child(div().v_flex().gap_2().children(items)),
            )
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
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .px_3()
                    .pb_2()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(self.status_line())
                    .child(self.usage_line()),
            )
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
