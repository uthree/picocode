//! Human-in-the-loop approval for destructive tool calls.
//!
//! Implemented as a rig [`AgentHook`]: each tool call is first checked against
//! the config allow/deny rules ([`ApprovalRules::decide`]); calls that resolve
//! to `Ask` send an [`AgentEvent::ApprovalRequest`] to the TUI and await the
//! user's y/n decision. Denials are returned to the model as the tool result so
//! it can adapt instead of failing the run.

use rig::agent::{AgentHook, HookContext, StepEvent, StepEventKind};
use rig::completion::CompletionModel;
use rig::tool::Tool;
use tokio::sync::{mpsc, oneshot};

use crate::config::{Decision, ModeHandle, RulesHandle};
use crate::event::AgentEvent;
use crate::tools::{Bash, DESTRUCTIVE_TOOLS};

pub struct ApprovalHook {
    tx: mpsc::Sender<AgentEvent>,
    rules: RulesHandle,
    mode: ModeHandle,
}

impl ApprovalHook {
    pub fn new(tx: mpsc::Sender<AgentEvent>, rules: RulesHandle, mode: ModeHandle) -> Self {
        Self { tx, rules, mode }
    }
}

/// Extract the `command` string from the bash tool's JSON args.
pub(crate) fn bash_command(tool_name: &str, args: &str) -> Option<String> {
    if tool_name != Bash::NAME {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

impl<M: CompletionModel> AgentHook<M> for ApprovalHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> rig::agent::Flow {
        use rig::agent::Flow;

        let StepEvent::ToolCall {
            tool_name, args, ..
        } = event
        else {
            return Flow::Continue;
        };
        let command = bash_command(tool_name, args);
        let destructive = DESTRUCTIVE_TOOLS.contains(&tool_name);
        match self
            .rules
            .decide(self.mode.get(), tool_name, command.as_deref(), destructive)
        {
            Decision::Allow => return Flow::Continue,
            Decision::Deny(reason) => return Flow::Skip { reason },
            Decision::Ask => {}
        }

        let (respond, decision) = oneshot::channel();
        let request = AgentEvent::ApprovalRequest {
            name: tool_name.to_string(),
            args: args.to_string(),
            respond,
        };
        if self.tx.send(request).await.is_err() {
            return Flow::Terminate {
                reason: "the UI has shut down".to_string(),
            };
        }

        // Fail closed: anything but an explicit approval denies the call.
        match decision.await {
            Ok(true) => Flow::Continue,
            _ => Flow::Skip {
                reason: format!(
                    "The user denied this `{tool_name}` call. Do not retry the same call; \
                     explain what you wanted to do and ask the user, or take a different approach."
                ),
            },
        }
    }

    fn observes(&self, kind: StepEventKind) -> bool {
        // Skip the high-frequency streaming delta events; the steering
        // `ToolCall` event fires regardless of this hint.
        !matches!(
            kind,
            StepEventKind::TextDelta | StepEventKind::ToolCallDelta
        )
    }
}
