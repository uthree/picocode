//! Handling the worker's `AgentEvent` stream: streamed deltas, tool calls,
//! approvals, usage figures, background jobs and turn boundaries.

use picocode_core::event::{AgentEvent, WorkerCmd};

use super::{
    AlwaysAllow, App, EntryKind, PendingApproval, PendingQuestion, TOOL_OUTPUT_MAX_LINES,
    clamp_lines, compact_one_line,
};

impl App {
    pub(super) fn handle_agent_event(&mut self, ev: AgentEvent) {
        let parent_stream = ev.subagent_id().map(|_| self.stream_entry);
        match ev {
            AgentEvent::TextDelta(s) => {
                self.waiting = false;
                self.delta_est += 1;
                self.speed.record(1);
                self.append(EntryKind::Assistant, &s);
            }
            AgentEvent::ReasoningDelta(s) => {
                self.waiting = false;
                self.delta_est += 1;
                self.speed.record(1);
                self.append(EntryKind::Reasoning, &s);
            }
            AgentEvent::ToolCall { name, args } => {
                self.waiting = false;
                self.close_blocks();
                self.push_tool_call(&name, &args);
            }
            AgentEvent::SubagentToolCall { id, name, args } => {
                self.waiting = false;
                self.close_blocks();
                self.push(EntryKind::Notice, format!("Subagent #{id}: {name}"));
                self.push_tool_call(&name, &args);
            }
            AgentEvent::SubagentStarted { id } => {
                self.close_blocks();
                self.push(EntryKind::Notice, format!("Subagent #{id} started"));
            }
            AgentEvent::SubagentFinished { id, success } => {
                self.close_blocks();
                let (kind, status) = if success {
                    (EntryKind::Notice, "finished")
                } else {
                    (EntryKind::Warning, "failed")
                };
                self.push(kind, format!("Subagent #{id} {status}"));
            }
            AgentEvent::SubagentToolResult { id, output } => {
                self.close_blocks();
                self.push(
                    EntryKind::ToolOut,
                    format!(
                        "Subagent #{id} result:\n{}",
                        clamp_lines(output.trim_end(), TOOL_OUTPUT_MAX_LINES)
                    ),
                );
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
                agent_id,
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
                    agent_id,
                    name,
                    args,
                    always,
                    respond,
                });
            }
            AgentEvent::AutoDecision {
                agent_id,
                name,
                allowed,
                reason,
            } => {
                self.close_blocks();
                let name = match agent_id {
                    Some(id) => format!("subagent #{id}: {name}"),
                    None => name,
                };
                let verb = if allowed { "approved" } else { "refused" };
                self.push(
                    if allowed {
                        EntryKind::Notice
                    } else {
                        EntryKind::Warning
                    },
                    format!("auto {verb} {name}: {reason}"),
                );
            }
            AgentEvent::GoalCheck {
                round,
                max,
                done,
                reason,
            } => {
                self.close_blocks();
                self.goal_round = round;
                if done {
                    self.goal = None;
                    self.goal_round = 0;
                    self.push(EntryKind::Notice, format!("Goal reached: {reason}"));
                } else if round >= max {
                    self.push(
                        EntryKind::Warning,
                        format!(
                            "Goal not reached after {max} follow-up turns — stopping. \
                             Still missing: {reason}"
                        ),
                    );
                } else {
                    self.push(
                        EntryKind::Notice,
                        format!("Goal not reached ({round}/{max}) — continuing: {reason}"),
                    );
                }
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
                self.close_blocks();
                self.ctx_tokens = input;
                // Snap the live estimate to the reported figure.
                self.turn_out += output;
                self.total_out += output;
                self.delta_est = 0;
            }
            AgentEvent::SubagentUsage { id, input, output } => {
                self.turn_out += output;
                self.total_out += output;
                self.close_blocks();
                self.push(
                    EntryKind::Notice,
                    format!("Subagent #{id}: {input} input / {output} output tokens"),
                );
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
            // A reply for the model that is still active; one for a model
            // switched away from would cap the new model by the old one's.
            AgentEvent::ContextLimit { model, limit } if model == self.cfg.model => {
                self.cfg.apply_context_limit(self.cfg.provider, limit);
            }
            AgentEvent::ContextLimit { .. } => {}
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
                                // A shorter list than last time leaves the
                                // cursor past the end; put it back on a row
                                // that exists.
                                let rows = 3 + form.fetched.len();
                                form.field = form.field.min(rows - 1);
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
                self.pending = None;
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
                    self.pending = None;
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
        if let Some(stream) = parent_stream {
            self.stream_entry = stream;
        }
    }
}
