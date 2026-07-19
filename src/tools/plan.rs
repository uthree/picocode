//! `submit_plan` — plan-mode approval: the model submits its plan, the user
//! approves it in a dialog, and approval switches picocode to edit mode so
//! the same turn can go on to execute it.

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::ToolError;
use crate::config::{Mode, ModeHandle};
use crate::event::AgentEvent;

const APPROVE: &str = "Approve — switch to edit mode and execute";
const REVISE: &str = "Keep planning — the plan needs changes";

#[derive(Deserialize)]
pub struct PlanArgs {
    plan: String,
}

pub struct SubmitPlan {
    tx: mpsc::Sender<AgentEvent>,
    mode: ModeHandle,
}

impl SubmitPlan {
    pub fn new(tx: mpsc::Sender<AgentEvent>, mode: ModeHandle) -> Self {
        Self { tx, mode }
    }
}

impl Tool for SubmitPlan {
    const NAME: &'static str = "submit_plan";
    type Error = ToolError;
    type Args = PlanArgs;
    type Output = String;

    fn description(&self) -> String {
        "Submit your implementation plan for user approval. Only meaningful in \
         plan mode, once your investigation is done: pass the complete plan \
         (goal, steps, files to touch, verification). If the user approves, \
         picocode switches to edit mode and you must execute the plan \
         immediately; otherwise revise the plan and submit again."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The full implementation plan to show the user"
                }
            },
            "required": ["plan"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let plan = args.plan.trim().to_string();
        if plan.is_empty() {
            return Err(ToolError::new("plan must not be empty"));
        }
        if self.mode.get() != Mode::Plan {
            return Ok(
                "picocode is not in plan mode, so there is no plan to approve. \
                       Proceed with the task directly."
                    .to_string(),
            );
        }
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(AgentEvent::UserQuestion {
                title: "Plan approval".to_string(),
                question: format!("{plan}\n\nExecute this plan?"),
                options: vec![APPROVE.to_string(), REVISE.to_string()],
                respond,
            })
            .await
            .map_err(|_| ToolError::new("UI channel closed"))?;
        match rx.await {
            Ok(Some(0)) => {
                self.mode.set(Mode::Edit);
                Ok(
                    "The user approved the plan and picocode is now in edit mode. \
                    Execute the plan now, step by step."
                        .to_string(),
                )
            }
            Ok(_) => Ok(
                "The user did not approve the plan. Ask what should change, \
                         or revise the plan and call submit_plan again."
                    .to_string(),
            ),
            Err(_) => Err(ToolError::new("plan approval cancelled")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_switches_to_edit_mode() {
        let (tx, mut rx) = mpsc::channel(4);
        let mode = ModeHandle::new(Mode::Plan);
        let tool = SubmitPlan::new(tx, mode.clone());
        let call = tokio::spawn(async move {
            tool.call(PlanArgs {
                plan: "1. write hello.txt".into(),
            })
            .await
        });

        let Some(AgentEvent::UserQuestion {
            title,
            question,
            respond,
            ..
        }) = rx.recv().await
        else {
            panic!("expected a UserQuestion event");
        };
        assert_eq!(title, "Plan approval");
        assert!(question.contains("1. write hello.txt"));
        respond.send(Some(0)).unwrap();

        assert!(call.await.unwrap().unwrap().contains("edit mode"));
        assert_eq!(mode.get(), Mode::Edit);
    }

    #[tokio::test]
    async fn rejection_and_wrong_mode_keep_state() {
        // Rejected (or dismissed): stays in plan mode, asks for a revision.
        let (tx, mut rx) = mpsc::channel(4);
        let mode = ModeHandle::new(Mode::Plan);
        let tool = SubmitPlan::new(tx.clone(), mode.clone());
        let call = tokio::spawn(async move { tool.call(PlanArgs { plan: "x".into() }).await });
        let Some(AgentEvent::UserQuestion { respond, .. }) = rx.recv().await else {
            panic!("expected a UserQuestion event");
        };
        respond.send(Some(1)).unwrap();
        assert!(call.await.unwrap().unwrap().contains("did not approve"));
        assert_eq!(mode.get(), Mode::Plan);

        // Outside plan mode: no dialog, the model is told to just proceed.
        let mode = ModeHandle::new(Mode::Edit);
        let tool = SubmitPlan::new(tx, mode);
        let out = tool.call(PlanArgs { plan: "x".into() }).await.unwrap();
        assert!(out.contains("not in plan mode"));
        assert!(rx.try_recv().is_err());
    }
}
