//! The chat view: transcript, input row, status bar, and the approval /
//! question dialogs — a gpui rendering of picocode-core's event stream.

use gpui::prelude::*;
use gpui::{AnyElement, ClickEvent, Context, Entity, ScrollHandle, SharedString, Window, div, px};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, StyledExt};
use tokio::sync::{mpsc, oneshot, watch};

use picocode_core::config::{self, Config, Mode};
use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::transcript::{Entry, EntryKind, diff_lines};
use picocode_core::{agent, approval, models, state};

const TOOL_OUTPUT_MAX_LINES: usize = 12;
const DIFF_MAX_LINES: usize = 30;

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
        // Plain Enter submits; a secondary Enter (Shift+Enter, Cmd+Enter)
        // keeps the newline the multi-line input just inserted.
        if let InputEvent::PressEnter { secondary: false } = ev {
            self.submit(window, cx);
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.approval.is_some() || self.question.is_some() {
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
            _ if text.starts_with("/model ") => {
                let name = text["/model ".len()..].trim().to_string();
                self.switch_model(&name, cx);
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
            .relative()
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
            .child(self.render_status_bar(cx))
            .children(self.render_menu(cx))
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
