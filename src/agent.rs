//! rig Agent construction and the streaming worker task.
//!
//! The worker owns the typed `Agent<M>` and the conversation history. The TUI
//! talks to it exclusively through channels (`WorkerCmd` in, `AgentEvent` out),
//! which keeps the provider generics out of the UI code.

use futures::StreamExt;
use rig::agent::Agent;
use rig::completion::{CompletionModel, GetTokenUsage, Message};
use rig::message::ToolResultContent;
use rig::prelude::*;
use rig::providers::{anthropic, ollama, openai};
use rig::streaming::{StreamedAssistantContent, StreamedUserContent};
use tokio::sync::mpsc;

use crate::approval::ApprovalHook;
use crate::config::{Config, Provider};
use crate::event::{AgentEvent, WorkerCmd};
use crate::tools;

/// Build the agent for the configured provider and spawn the worker task.
/// Returns the command channel the TUI uses to drive it.
pub fn spawn(cfg: &Config, event_tx: mpsc::Sender<AgentEvent>) -> anyhow::Result<mpsc::Sender<WorkerCmd>> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>(32);
    let cfg = cfg.clone();

    // The concrete `Agent<M>` type differs per provider, so the builder chain
    // lives in a macro and each arm spawns its own typed worker.
    macro_rules! spawn_for {
        ($client:expr) => {{
            let root = cfg.root.clone();
            let agent = $client
                .agent(&cfg.model)
                .preamble(&system_prompt(&cfg))
                .tool(tools::ReadFile::new(root.clone()))
                .tool(tools::ListFiles::new(root.clone()))
                .tool(tools::Grep::new(root.clone()))
                .tool(tools::WriteFile::new(root.clone()))
                .tool(tools::EditFile::new(root.clone()))
                .tool(tools::Bash::new(root))
                .max_tokens(8192)
                .build();
            tokio::spawn(worker(agent, cmd_rx, event_tx, cfg));
        }};
    }

    match cfg.provider {
        Provider::Ollama => spawn_for!(ollama::Client::from_env()?),
        Provider::Anthropic => spawn_for!(anthropic::Client::from_env()?),
        Provider::Openai => spawn_for!(openai::Client::from_env()?),
    }
    Ok(cmd_tx)
}

fn system_prompt(cfg: &Config) -> String {
    format!(
        "You are picocode, a coding agent running in a terminal. \
         Your working directory is: {root}\n\
         \n\
         Available tools: read_file, list_files, grep, write_file, edit_file, bash.\n\
         \n\
         Workflow:\n\
         1. Explore first: use list_files and grep to locate relevant files, and read_file \
         before editing anything.\n\
         2. Edit with edit_file (the old_string must match exactly once) or create files \
         with write_file.\n\
         3. Verify your changes with bash (build, test) when appropriate.\n\
         \n\
         Rules:\n\
         - Use the tools instead of guessing about the project.\n\
         - If the user denies a tool call, do not retry it; explain and ask instead.\n\
         - Keep responses concise. Respond in the language the user writes in.",
        root = cfg.root.display()
    )
}

async fn worker<M>(
    agent: Agent<M>,
    mut cmd_rx: mpsc::Receiver<WorkerCmd>,
    event_tx: mpsc::Sender<AgentEvent>,
    cfg: Config,
) where
    M: CompletionModel + 'static,
    M::StreamingResponse: GetTokenUsage,
{
    let mut history: Vec<Message> = Vec::new();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            WorkerCmd::Clear => history.clear(),
            WorkerCmd::Prompt(prompt) => {
                run_once(&agent, &mut history, prompt, &event_tx, &cfg).await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
            }
        }
    }
}

async fn run_once<M>(
    agent: &Agent<M>,
    history: &mut Vec<Message>,
    prompt: String,
    event_tx: &mpsc::Sender<AgentEvent>,
    cfg: &Config,
) where
    M: CompletionModel + 'static,
    M::StreamingResponse: GetTokenUsage,
{
    let hook = ApprovalHook::new(event_tx.clone(), cfg.yolo);
    let mut stream = agent
        .stream_chat(prompt.clone(), history.clone())
        .max_turns(cfg.max_turns)
        .add_hook(hook)
        .await;

    let mut got_final = false;
    let mut reasoning_delta_seen = false;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
                StreamedAssistantContent::Text(t) => {
                    let _ = event_tx.send(AgentEvent::TextDelta(t.text)).await;
                }
                StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                    reasoning_delta_seen = true;
                    let _ = event_tx.send(AgentEvent::ReasoningDelta(reasoning)).await;
                }
                StreamedAssistantContent::Reasoning(reasoning) => {
                    // Providers that stream deltas also emit the aggregated
                    // block; only surface it when no deltas were streamed.
                    if !reasoning_delta_seen {
                        let text = reasoning_text(&reasoning);
                        if !text.is_empty() {
                            let _ = event_tx.send(AgentEvent::ReasoningDelta(text)).await;
                        }
                    }
                }
                StreamedAssistantContent::ToolCall { tool_call, .. } => {
                    let name = tool_call.function.name.clone();
                    let args = tool_call.function.arguments.to_string();
                    let _ = event_tx.send(AgentEvent::ToolCall { name, args }).await;
                }
                _ => {}
            },
            Ok(MultiTurnStreamItem::StreamUserItem(user_content)) => {
                #[allow(irrefutable_let_patterns)]
                if let StreamedUserContent::ToolResult { tool_result, .. } = user_content {
                    let output = tool_result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ToolResultContent::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = event_tx.send(AgentEvent::ToolResult { output }).await;
                }
            }
            Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                let _ = event_tx
                    .send(AgentEvent::Usage {
                        input: call.usage.input_tokens,
                        output: call.usage.output_tokens,
                    })
                    .await;
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                got_final = true;
                match response.messages {
                    // `messages` holds this run's new messages (prompt +
                    // assistant turns + tool results), excluding prior history.
                    Some(new_messages) => history.extend(new_messages),
                    None => {
                        history.push(Message::user(prompt.clone()));
                        if !response.output.is_empty() {
                            history.push(Message::assistant(response.output.clone()));
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                let _ = event_tx.send(AgentEvent::Error(e.to_string())).await;
            }
        }
    }

    if !got_final {
        // The run errored out; keep the user's message so the next turn still
        // has it as context.
        history.push(Message::user(prompt));
    }
}

fn reasoning_text(reasoning: &rig::message::Reasoning) -> String {
    use rig::message::ReasoningContent;
    reasoning
        .content
        .iter()
        .filter_map(|c| match c {
            ReasoningContent::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
