//! Parallel delegation with explicit result collection. A turn-owned task
//! registry keeps children alive while the parent continues working.

use std::sync::Arc;

use futures::StreamExt;
use rig::agent::Agent;
use rig::completion::CompletionModel;
use rig::message::ToolResultContent;
use rig::prelude::*;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent};
use rig::tool::{Tool, ToolCallExtensions};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::ToolError;
use crate::approval::ApprovalHook;
use crate::config::{Config, Mode};
use crate::event::AgentEvent;

mod tasks;
pub(crate) use tasks::{ParentRequest, SubagentScope};

pub const DELEGATE_TASK: &str = "delegate_task";
const MAX_TURNS: usize = 32;
const MAX_OUTPUT_BYTES: usize = 24_000;

#[derive(Deserialize)]
pub struct DelegateArgs {
    task: String,
}

pub struct DelegateTask<M: CompletionModel> {
    make_agent: Arc<dyn Fn() -> Agent<M> + Send + Sync>,
    reviewer: Arc<Agent<M>>,
    cfg: Config,
    tx: mpsc::Sender<AgentEvent>,
}

impl<M: CompletionModel> DelegateTask<M> {
    pub(crate) fn new(
        make_agent: impl Fn() -> Agent<M> + Send + Sync + 'static,
        reviewer: Arc<Agent<M>>,
        cfg: Config,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Self {
        Self {
            make_agent: Arc::new(make_agent),
            reviewer,
            cfg,
            tx,
        }
    }
}

impl<M: CompletionModel + 'static> Tool for DelegateTask<M> {
    const NAME: &'static str = DELEGATE_TASK;
    type Error = ToolError;
    type Args = DelegateArgs;
    type Output = serde_json::Value;

    fn description(&self) -> String {
        format!(
            "Start a subagent in the background and immediately return its agent_id. \
             Continue your own work or start other independent tasks, then call \
             agent_result with that ID to collect the report. Up to {} children \
             run in parallel, each for {MAX_TURNS} model turns. They use the same \
             model, project and permission rules, with fresh conversations. Include \
             context, paths, constraints and expected output in task. Files are shared; \
             assign separate files to tasks that edit them.",
            tasks::MAX_RUNNING
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "A self-contained task with context, constraints and expected output"
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        self.call_with_extensions(args, &ToolCallExtensions::new())
            .await
    }

    async fn call_with_extensions(
        &self,
        args: Self::Args,
        extensions: &ToolCallExtensions,
    ) -> Result<Self::Output, ToolError> {
        let task = args.task.trim();
        if task.is_empty() {
            return Err(ToolError::new("task must not be empty"));
        }
        let parent = extensions
            .get::<ParentRequest>()
            .ok_or_else(|| ToolError::new("delegation requires the parent request context"))?;
        let mut prompt = format!(
            "The human's request to the parent agent:\n{}\n\n\
             Your delegated task:\n{task}",
            parent.intent
        );
        if self.cfg.mode.get() == Mode::Plan {
            prompt.push_str(
                "\n\nPlan mode is active: investigate with read-only tools and return \
                 findings or a plan to the parent. File edits and shell commands are blocked.",
            );
        }
        let make_agent = self.make_agent.clone();
        let reviewer = self.reviewer.clone();
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        let intent = parent.intent.clone();
        let approvals = parent.approvals.clone();
        let id = parent.tasks.spawn(self.tx.clone(), move |id| async move {
            let agent = make_agent();
            let hook = ApprovalHook::new(
                tx.clone(),
                cfg.approval.clone(),
                cfg.mode.clone(),
                reviewer,
                intent,
                cfg.root.clone(),
            )
            .with_gate(approvals)
            .for_subagent(id);
            run_child(&agent, prompt, hook, &tx, id).await
        })?;
        Ok(json!({ "agent_id": id, "status": "running" }))
    }
}

async fn run_child<M: CompletionModel + 'static>(
    agent: &Agent<M>,
    prompt: String,
    hook: ApprovalHook<M>,
    tx: &mpsc::Sender<AgentEvent>,
    id: u64,
) -> Result<String, ToolError> {
    let mut stream = agent
        .stream_chat(prompt, Vec::<rig::completion::Message>::new())
        .max_turns(MAX_TURNS)
        .tool_concurrency(1)
        .add_hook(hook)
        .await;
    while let Some(item) = stream.next().await {
        match item.map_err(|e| ToolError::new(format!("subagent failed: {e}")))? {
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            }) => {
                let _ = tx
                    .send(AgentEvent::SubagentToolCall {
                        id,
                        name: tool_call.function.name,
                        args: tool_call.function.arguments.to_string(),
                    })
                    .await;
            }
            MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            }) => {
                let output = tool_result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = tx.send(AgentEvent::SubagentToolResult { id, output }).await;
            }
            MultiTurnStreamItem::CompletionCall(call) => {
                let _ = tx
                    .send(AgentEvent::SubagentUsage {
                        id,
                        input: call.usage.input_tokens,
                        output: call.usage.output_tokens,
                    })
                    .await;
            }
            MultiTurnStreamItem::FinalResponse(response) => {
                let output = response.output.trim();
                if output.is_empty() {
                    return Err(ToolError::new("subagent returned an empty report"));
                }
                return Ok(super::truncate_output(output, MAX_OUTPUT_BYTES));
            }
            _ => {}
        }
    }
    Err(ToolError::new("subagent stopped without a final report"))
}

#[derive(Deserialize)]
pub struct ResultArgs {
    agent_id: u64,
    #[serde(default = "wait_by_default")]
    wait: bool,
}

fn wait_by_default() -> bool {
    true
}

pub struct AgentResult;

impl Tool for AgentResult {
    const NAME: &'static str = "agent_result";
    type Error = ToolError;
    type Args = ResultArgs;
    type Output = serde_json::Value;

    fn description(&self) -> String {
        "Get a subagent's status and final report by agent_id. By default, wait \
         until it finishes; pass wait=false to check without waiting. Collect \
         every delegated result before your final answer. IDs belong to the \
         current parent turn, and results can be read again during that turn."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "integer", "minimum": 1, "description": "ID returned by delegate_task" },
                "wait": { "type": "boolean", "default": true, "description": "Wait for completion (default true); false returns current status immediately" }
            },
            "required": ["agent_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        self.call_with_extensions(args, &ToolCallExtensions::new())
            .await
    }

    async fn call_with_extensions(
        &self,
        args: Self::Args,
        extensions: &ToolCallExtensions,
    ) -> Result<Self::Output, ToolError> {
        let parent = extensions
            .get::<ParentRequest>()
            .ok_or_else(|| ToolError::new("agent_result requires the parent request context"))?;
        let report = parent.tasks.result(args.agent_id, args.wait).await?;
        serde_json::to_value(report).map_err(|e| ToolError::new(e.to_string()))
    }
}
