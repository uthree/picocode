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
use tokio::sync::{mpsc, watch};

use crate::approval::ApprovalHook;
use crate::config::{Config, Provider};
use crate::event::{AgentEvent, WorkerCmd};
use crate::tools;

/// Build the agent for the configured provider and spawn the worker task.
/// Returns the command channel the TUI uses to drive it. A signal on
/// `cancel_rx` aborts the generation in progress (Esc in the TUI).
pub fn spawn(
    cfg: &Config,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel_rx: watch::Receiver<()>,
) -> anyhow::Result<mpsc::Sender<WorkerCmd>> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>(32);
    let cfg = cfg.clone();

    // The concrete `Agent<M>` type differs per provider, so the builder chain
    // lives in a macro and each arm spawns its own typed worker.
    macro_rules! spawn_for {
        ($client:expr) => {{
            let client = $client;
            let root = cfg.root.clone();
            let agent = client
                .agent(&cfg.model)
                .preamble(&system_prompt(&cfg))
                .tool(tools::ReadFile::new(root.clone()))
                .tool(tools::ListFiles::new(root.clone()))
                .tool(tools::Grep::new(root.clone()))
                .tool(tools::WriteFile::new(root.clone()))
                .tool(tools::EditFile::new(root.clone()))
                .tool(tools::Bash::new(
                    root,
                    cfg.bash_timeout.clone(),
                    event_tx.clone(),
                ))
                .tool(tools::WebFetch::new())
                .tool(tools::WebSearch::new(cfg.search.clone()))
                .tool(tools::AskUser::new(event_tx.clone()))
                .tool(tools::SubmitPlan::new(event_tx.clone(), cfg.mode.clone()))
                .max_tokens(8192)
                .build();
            // A second, tool-less agent used by /compact: it only ever needs
            // to read the history and write a summary.
            let compactor = client
                .agent(&cfg.model)
                .preamble(COMPACT_PREAMBLE)
                .max_tokens(8192)
                .build();
            tokio::spawn(worker(agent, compactor, cmd_rx, event_tx, cfg, cancel_rx));
        }};
    }

    // With a configured base_url the client is built directly (bypassing the
    // *_BASE_URL env vars); otherwise `from_env` handles env-based setup.
    match cfg.provider {
        Provider::Ollama => spawn_for!(match cfg.base_url.clone() {
            Some(url) => {
                let key = std::env::var("OLLAMA_API_KEY").unwrap_or_default();
                ollama::Client::builder()
                    .api_key(ollama::OllamaApiKey::from(key.as_str()))
                    .base_url(&url)
                    .build()?
            }
            None => ollama::Client::from_env()?,
        }),
        Provider::Anthropic => spawn_for!(match cfg.base_url.clone() {
            Some(url) => {
                let key = std::env::var("ANTHROPIC_API_KEY")
                    .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY is not set"))?;
                anthropic::Client::builder()
                    .api_key(key)
                    .base_url(&url)
                    .build()?
            }
            None => anthropic::Client::from_env()?,
        }),
        Provider::Openai => spawn_for!(match cfg.base_url.clone() {
            Some(url) => {
                // Local OpenAI-compatible servers usually don't check the key.
                let key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "unused".into());
                openai::Client::builder()
                    .api_key(&key)
                    .base_url(&url)
                    .build()?
            }
            None => openai::Client::from_env()?,
        }),
    }
    Ok(cmd_tx)
}

fn system_prompt(cfg: &Config) -> String {
    let mut prompt = match &cfg.system_prompt {
        Some(custom) => custom.replace("{root}", &cfg.root.display().to_string()),
        None => default_system_prompt(cfg),
    };
    for (name, content) in &cfg.instructions {
        prompt.push_str(&format!(
            "\n\nProject instructions from {name} (follow them):\n{content}"
        ));
    }
    prompt
}

fn default_system_prompt(cfg: &Config) -> String {
    format!(
        "You are picocode, a coding agent running in a terminal. \
         Your working directory is: {root}\n\
         \n\
         Available tools: read_file, list_files, grep, write_file, edit_file, bash, \
         web_search, web_fetch, ask_user, submit_plan.\n\
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
         - Use web_search to look things up on the web, and web_fetch to read a URL the \
         user shares or a search result you want in full.\n\
         - When you need the user to decide between a few concrete alternatives, call \
         ask_user with short options instead of asking in plain text.\n\
         - If the user denies a tool call, do not retry it; explain and ask instead.\n\
         - Keep responses concise. Respond in the language the user writes in.",
        root = cfg.root.display()
    )
}

const COMPACT_PREAMBLE: &str = "You compress conversation history for a coding agent. \
     Write a faithful, concise summary that preserves everything needed to continue \
     the work: the user's goals and requests, key facts learned about the project \
     (files, structure, commands, findings), what was done and the results, and any \
     unresolved tasks or next steps. Plain text only. Reply with the summary only — \
     no preface, no commentary.";

const COMPACT_REQUEST: &str = "Summarize our entire conversation above so that you \
     could seamlessly continue the work from the summary alone. Reply with the \
     summary only.";

async fn worker<M>(
    agent: Agent<M>,
    compactor: Agent<M>,
    mut cmd_rx: mpsc::Receiver<WorkerCmd>,
    event_tx: mpsc::Sender<AgentEvent>,
    cfg: Config,
    mut cancel_rx: watch::Receiver<()>,
) where
    M: CompletionModel + 'static,
    M::StreamingResponse: GetTokenUsage,
{
    let mut history: Vec<Message> = Vec::new();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            WorkerCmd::Clear => history.clear(),
            WorkerCmd::ShellRecord { command, output } => {
                history.push(Message::user(format!(
                    "I ran this shell command myself in the working directory:\n\
                     $ {command}\n\nOutput:\n{output}"
                )));
            }
            WorkerCmd::BackgroundRecord {
                id,
                command,
                output,
            } => {
                history.push(Message::user(format!(
                    "The bash command that timed out and was moved to background \
                     job #{id} has finished:\n$ {command}\n\nOutput:\n{output}"
                )));
            }
            WorkerCmd::TakeHistory(tx) => {
                let _ = tx.send(history.clone());
            }
            WorkerCmd::SeedHistory(h) => history = h,
            WorkerCmd::Prompt(prompt) => {
                run_once(
                    &agent,
                    &mut history,
                    prompt,
                    &event_tx,
                    &cfg,
                    &mut cancel_rx,
                )
                .await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
            }
            WorkerCmd::Compact => {
                compact(&compactor, &mut history, &event_tx, &mut cancel_rx).await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
            }
        }
    }
}

/// Ask the tool-less compactor agent to summarize the history, then replace the
/// history with that summary. On failure the history is left untouched.
async fn compact<M>(
    compactor: &Agent<M>,
    history: &mut Vec<Message>,
    event_tx: &mpsc::Sender<AgentEvent>,
    cancel: &mut watch::Receiver<()>,
) where
    M: CompletionModel + 'static,
{
    if history.is_empty() {
        let _ = event_tx
            .send(AgentEvent::Compacted {
                messages: 0,
                summary: String::new(),
            })
            .await;
        return;
    }

    let messages = history.len();
    let _ = cancel.borrow_and_update(); // discard stale signals
    let result = tokio::select! {
        biased;
        _ = cancel.changed() => {
            let _ = event_tx.send(AgentEvent::Cancelled).await;
            return; // history untouched
        }
        res = async { compactor.prompt(COMPACT_REQUEST).history(history.clone()).await } => res,
    };
    match result {
        Ok(summary) => {
            let summary = summary.trim().to_string();
            if summary.is_empty() {
                let _ = event_tx
                    .send(AgentEvent::Error(
                        "compaction returned an empty summary; history unchanged".into(),
                    ))
                    .await;
                return;
            }
            history.clear();
            history.push(Message::user(format!(
                "Summary of our conversation so far (earlier messages were compacted to save context):\n\n{summary}"
            )));
            history.push(Message::assistant(
                "Understood — I'll continue from that summary.",
            ));
            let _ = event_tx
                .send(AgentEvent::Compacted { messages, summary })
                .await;
        }
        Err(e) => {
            let _ = event_tx
                .send(AgentEvent::Error(format!(
                    "compaction failed: {e}; history unchanged"
                )))
                .await;
        }
    }
}

async fn run_once<M>(
    agent: &Agent<M>,
    history: &mut Vec<Message>,
    prompt: String,
    event_tx: &mpsc::Sender<AgentEvent>,
    cfg: &Config,
    cancel: &mut watch::Receiver<()>,
) where
    M: CompletionModel + 'static,
    M::StreamingResponse: GetTokenUsage,
{
    // In plan mode, tell the model up front instead of letting it discover
    // the blocked tools by trial and error (the approval hook still denies
    // any write it attempts anyway).
    let prompt = if cfg.mode.get() == crate::config::Mode::Plan {
        format!(
            "{prompt}\n\n[picocode plan mode is active: investigate with the read-only \
             tools and put together a concise implementation plan — goal, steps, files \
             to touch, and how to verify. Do not modify files or run state-changing \
             commands; write_file/edit_file/bash are blocked. When the plan is ready, \
             call submit_plan with the full plan text to ask the user for approval — \
             if approved you are switched to edit mode and must execute it.]"
        )
    } else {
        prompt
    };
    let hook = ApprovalHook::new(event_tx.clone(), cfg.approval.clone(), cfg.mode.clone());
    let mut stream = agent
        .stream_chat(prompt.clone(), history.clone())
        .max_turns(cfg.max_turns.get())
        .add_hook(hook)
        .await;

    let mut got_final = false;
    let mut reasoning_delta_seen = false;
    let mut cancelled = false;
    let _ = cancel.borrow_and_update(); // discard stale signals

    loop {
        // Dropping the stream on cancel also aborts the in-flight request and
        // any tool execution it is driving.
        let item = tokio::select! {
            biased;
            _ = cancel.changed() => {
                cancelled = true;
                break;
            }
            item = stream.next() => match item {
                Some(item) => item,
                None => break,
            },
        };
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
            Ok(MultiTurnStreamItem::StreamUserItem(user_content)) =>
            {
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

    if cancelled {
        drop(stream);
        let _ = event_tx.send(AgentEvent::Cancelled).await;
    }
    if !got_final {
        // The run was cancelled or errored out; keep the user's message so
        // the next turn still has it as context.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ApprovalRules, Mode, ModeHandle, Provider, RulesHandle, SearchConfig, SearchProvider,
    };
    use std::path::PathBuf;

    fn test_cfg() -> Config {
        Config {
            provider: Provider::Ollama,
            model: "qwen3:4b".into(),
            base_url: None,
            models: Vec::new(),
            active_model: None,
            model_note: None,
            max_turns: crate::config::TurnsHandle::new(50),
            bash_timeout: crate::config::TimeoutHandle::new(120),
            root: PathBuf::from("/tmp/proj"),
            approval: RulesHandle::new(ApprovalRules::default()),
            mode: ModeHandle::new(Mode::ReadOnly),
            search: SearchConfig {
                provider: SearchProvider::Duckduckgo,
                base_url: None,
                max_results: 5,
                api_key: None,
            },
            system_prompt: None,
            instructions: Vec::new(),
            config_files: Vec::new(),
            context_window: crate::config::DEFAULT_CONTEXT_WINDOW,
        }
    }

    #[test]
    fn default_prompt_mentions_root_and_instructions() {
        let mut cfg = test_cfg();
        cfg.instructions = vec![("AGENTS.md".into(), "be nice".into())];
        let p = system_prompt(&cfg);
        assert!(p.contains("You are picocode"));
        assert!(p.contains("/tmp/proj"));
        assert!(p.contains("Project instructions from AGENTS.md"));
        assert!(p.contains("be nice"));
    }

    #[test]
    fn config_override_replaces_base_and_expands_root() {
        let mut cfg = test_cfg();
        cfg.system_prompt = Some("Custom bot. Workdir: {root}.".into());
        cfg.instructions = vec![("AGENTS.md".into(), "be nice".into())];
        let p = system_prompt(&cfg);
        assert!(p.starts_with("Custom bot. Workdir: /tmp/proj."));
        assert!(!p.contains("You are picocode"));
        // Instruction files are still appended after the override.
        assert!(p.contains("Project instructions from AGENTS.md"));
    }
}
