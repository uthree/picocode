//! Handling the worker's `AgentEvent` stream: streamed deltas, tool calls,
//! approvals, usage figures, background jobs and turn boundaries.

use gpui::{Context, Window};
use rust_i18n::t;

use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::transcript::EntryKind;

use super::{Approval, ChatView, Menu, Question, TOOL_OUTPUT_MAX_LINES, clip, est_tokens};

impl ChatView {
    // ---------- events from the agent worker ----------

    pub(super) fn on_agent_event(
        &mut self,
        ev: AgentEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // No explicit scroll-follow here: the bottom-aligned virtual list
        // sticks to the bottom on its own while the user hasn't scrolled up,
        // and pins the position (like the TUI) while they have.
        match ev {
            AgentEvent::TextDelta(s) => {
                self.waiting = false;
                self.est_out += est_tokens(&s);
                self.speed.record(est_tokens(&s));
                self.append(EntryKind::Assistant, &s);
            }
            AgentEvent::ReasoningDelta(s) => {
                self.waiting = false;
                self.est_out += est_tokens(&s);
                self.speed.record(est_tokens(&s));
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
            AgentEvent::AutoDecision {
                name,
                allowed,
                reason,
            } => {
                let (kind, key) = if allowed {
                    (EntryKind::Notice, "auto_approved")
                } else {
                    (EntryKind::Warning, "auto_refused")
                };
                self.push(kind, t!(key, name = name, reason = reason).to_string());
            }
            AgentEvent::GoalCheck {
                round,
                max,
                done,
                reason,
            } => {
                self.goal_round = round;
                if done {
                    self.goal = None;
                    self.goal_round = 0;
                    self.push(
                        EntryKind::Notice,
                        t!("goal_reached", reason = reason).into(),
                    );
                } else if round >= max {
                    self.push(
                        EntryKind::Warning,
                        t!("goal_gave_up", max = max, reason = reason).into(),
                    );
                } else {
                    self.push(
                        EntryKind::Notice,
                        t!("goal_continuing", round = round, max = max, reason = reason).into(),
                    );
                }
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
            AgentEvent::ContextBreakdown(breakdown) => {
                self.context_info = Some(breakdown);
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
            AgentEvent::FormModelList {
                provider,
                base_url,
                result,
            } => {
                let current_base = self.add_model.as_ref().map(|d| d.base(cx));
                if let Some(dlg) = &mut self.add_model {
                    // Drop stale replies from before the dialog changed.
                    if dlg.provider == provider && current_base == Some(base_url) {
                        match result {
                            Ok(mut names) => {
                                names.sort();
                                dlg.note = t!("add_model_fetched", n = names.len()).to_string();
                                dlg.fetched = names;
                            }
                            Err(e) => {
                                dlg.note = t!("add_model_fetch_failed", error = e).to_string()
                            }
                        }
                    }
                }
            }
            AgentEvent::Compacted { messages, summary } => {
                if messages == 0 {
                    self.push(EntryKind::Notice, t!("nothing_to_compact").to_string());
                } else {
                    self.push(EntryKind::Notice, t!("compacted", n = messages).to_string());
                    self.push(EntryKind::Summary, summary);
                }
                self.running = false;
                self.est_out = 0;
                self.autosave(cx);
                self.flush_queued();
            }
            AgentEvent::ShellOutput { output } => self.push(EntryKind::ToolOut, output),
            AgentEvent::Pruned { outputs } => {
                self.push(EntryKind::Notice, t!("pruned", n = outputs).to_string());
            }
            AgentEvent::Undone { summary } => {
                if summary.is_empty() {
                    self.push(EntryKind::Notice, t!("undo_nothing").to_string());
                } else {
                    self.push(
                        EntryKind::Notice,
                        t!("undo_done", files = summary).to_string(),
                    );
                    // Persist the history record the worker just added.
                    self.autosave(cx);
                }
            }
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
                if self
                    .cmd_tx
                    .try_send(WorkerCmd::Prompt {
                        text: prompt,
                        attachments: Vec::new(),
                    })
                    .is_ok()
                {
                    self.running = true;
                    self.waiting = true;
                }
            }
            AgentEvent::Cancelled => {
                self.speed.reset();
                self.push(EntryKind::Notice, t!("cancelled").to_string());
                // Stop means stop: give held-back prompts to the input box
                // (and their attachments back to the staging row) instead of
                // firing them on the TurnComplete that follows.
                if !self.queued.is_empty() {
                    let queued = std::mem::take(&mut self.queued);
                    let mut text = queued
                        .iter()
                        .map(|(t, _)| t.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    for (_, attachments) in queued {
                        for att in attachments {
                            if !self.pending_attachments.contains(&att) {
                                self.pending_attachments.push(att);
                            }
                        }
                    }
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
                self.speed.reset();
                // A cancelled or failed completion never reports usage; drop
                // its estimate rather than carrying it into the idle counter.
                self.est_out = 0;
                // Tools may have switched branches during the turn.
                self.git_branch = picocode_core::git::branch(&self.cfg.root);
                self.autosave(cx);
                self.flush_queued();
            }
            AgentEvent::Error(e) => {
                self.waiting = false;
                self.push(EntryKind::Error, e);
            }
        }
    }
}
