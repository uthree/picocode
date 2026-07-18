//! Human-in-the-loop approval for destructive tool calls.
//!
//! Implemented as a rig [`AgentHook`]: before a destructive tool runs, the hook
//! sends an [`AgentEvent::ApprovalRequest`] to the TUI and awaits the user's
//! y/n decision. Denials are returned to the model as the tool result so it can
//! adapt instead of failing the run.

use rig::agent::{AgentHook, HookContext, StepEvent, StepEventKind};
use rig::completion::CompletionModel;
use tokio::sync::{mpsc, oneshot};

use crate::event::AgentEvent;
use crate::tools::DESTRUCTIVE_TOOLS;

pub struct ApprovalHook {
    tx: mpsc::Sender<AgentEvent>,
    yolo: bool,
}

impl ApprovalHook {
    pub fn new(tx: mpsc::Sender<AgentEvent>, yolo: bool) -> Self {
        Self { tx, yolo }
    }
}

impl<M: CompletionModel> AgentHook<M> for ApprovalHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> rig::agent::Flow {
        use rig::agent::Flow;

        let StepEvent::ToolCall { tool_name, args, .. } = event else {
            return Flow::Continue;
        };
        if self.yolo || !DESTRUCTIVE_TOOLS.contains(&tool_name) {
            return Flow::Continue;
        }

        let (respond, decision) = oneshot::channel();
        let request = AgentEvent::ApprovalRequest {
            name: tool_name.to_string(),
            args: args.to_string(),
            respond,
        };
        if self.tx.send(request).await.is_err() {
            return Flow::Terminate { reason: "the UI has shut down".to_string() };
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
        !matches!(kind, StepEventKind::TextDelta | StepEventKind::ToolCallDelta)
    }
}
