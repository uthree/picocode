//! rig Agent construction and the streaming worker task.
//!
//! The worker owns the typed `Agent<M>` and the conversation history. The TUI
//! talks to it exclusively through channels (`WorkerCmd` in, `AgentEvent` out),
//! which keeps the provider generics out of the UI code.

use futures::StreamExt;
use rig::agent::Agent;
use rig::completion::{CompletionModel, GetTokenUsage, Message};
use rig::message::{ToolResultContent, UserContent};
use rig::prelude::*;
use rig::providers::{anthropic, ollama, openai};
use rig::streaming::{StreamedAssistantContent, StreamedUserContent};
use tokio::sync::{mpsc, watch};

use crate::approval::ApprovalHook;
use crate::attachment::Attachment;
use crate::config::{Config, Provider};
use crate::event::{AgentEvent, WorkerCmd};
use crate::tools;

/// Build the agent for the configured provider and spawn the worker task.
/// Returns the command channel the front end uses to drive it, plus the
/// steering queue for mid-turn user messages (delivered at the next
/// tool-call boundary). A signal on `cancel_rx` aborts the generation in
/// progress (Esc in the TUI).
pub fn spawn(
    cfg: &Config,
    event_tx: mpsc::Sender<AgentEvent>,
    cancel_rx: watch::Receiver<()>,
    jobs: tools::BackgroundJobs,
) -> anyhow::Result<(mpsc::Sender<WorkerCmd>, crate::steer::SteerQueue)> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>(32);
    let steer = crate::steer::SteerQueue::new();
    let cfg = cfg.clone();

    // The concrete `Agent<M>` type differs per provider, so the builder chain
    // lives in a macro and each arm spawns its own typed worker. The macro
    // takes a completion-model expression (evaluated once for the agent and
    // once for the compactor) so an arm can pre-configure the model —
    // Anthropic enables prompt caching this way.
    let journal = crate::undo::UndoJournal::new();
    let stamps = tools::ReadStamps::default();
    macro_rules! spawn_for {
        ($model:expr) => {{
            let root = cfg.root.clone();
            let enabled = |name: &str| !cfg.disable_tools.iter().any(|t| t == name);
            let mut builder = rig::agent::AgentBuilder::new($model)
                .preamble(&system_prompt(&cfg))
                .tool(tools::ReadFile::new(
                    root.clone(),
                    cfg.read_max_lines.clone(),
                    cfg.read_max_line_bytes.clone(),
                    stamps.clone(),
                ))
                .tool(tools::ListFiles::new(root.clone()))
                .tool(tools::Grep::new(root.clone()))
                .tool(tools::EditFile::new(
                    root.clone(),
                    cfg.after_edit.clone(),
                    journal.clone(),
                    stamps.clone(),
                ))
                .tool(tools::Bash::new(
                    root,
                    cfg.bash_timeout.clone(),
                    event_tx.clone(),
                    jobs.clone(),
                ))
                .tool(tools::SubmitPlan::new(event_tx.clone(), cfg.mode.clone()));
            if enabled(tools::WebSearch::NAME) {
                builder = builder.tool(tools::WebSearch::new(cfg.search.clone()));
            }
            if enabled(tools::WebFetch::NAME) {
                builder = builder.tool(tools::WebFetch::new());
            }
            let agent = builder.max_tokens(8192).build();
            // A second, tool-less agent used by /compact: it only ever needs
            // to read the history and write a summary.
            let compactor = rig::agent::AgentBuilder::new($model)
                .preamble(COMPACT_PREAMBLE)
                .max_tokens(8192)
                .build();
            tokio::spawn(worker(
                agent,
                compactor,
                cmd_rx,
                event_tx,
                cfg,
                cancel_rx,
                journal,
                steer.clone(),
            ));
        }};
    }

    // With a configured base_url the client is built directly (bypassing the
    // *_BASE_URL env vars); otherwise `from_env` handles env-based setup.
    match cfg.provider {
        Provider::Ollama => {
            let client = match cfg.base_url.clone() {
                Some(url) => {
                    let key = std::env::var("OLLAMA_API_KEY").unwrap_or_default();
                    ollama::Client::builder()
                        .api_key(ollama::OllamaApiKey::from(key.as_str()))
                        .base_url(&url)
                        .build()?
                }
                None => ollama::Client::from_env()?,
            };
            spawn_for!(client.completion_model(&cfg.model));
        }
        Provider::Anthropic => {
            let client = match cfg.base_url.clone() {
                Some(url) => {
                    let key = std::env::var("ANTHROPIC_API_KEY")
                        .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY is not set"))?;
                    anthropic::Client::builder()
                        .api_key(key)
                        .base_url(&url)
                        .build()?
                }
                None => anthropic::Client::from_env()?,
            };
            // Automatic prompt caching: the API places and advances the
            // cache breakpoint itself, cutting cost/latency on the long
            // repeated prefix an agent loop resends every request.
            spawn_for!(client.completion_model(&cfg.model).with_automatic_caching());
        }
        Provider::Openai => {
            let client = match cfg.base_url.clone() {
                Some(url) => {
                    // Local OpenAI-compatible servers usually don't check the key.
                    let key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "unused".into());
                    openai::Client::builder()
                        .api_key(&key)
                        .base_url(&url)
                        .build()?
                }
                None => openai::Client::from_env()?,
            };
            spawn_for!(client.completion_model(&cfg.model));
        }
    }
    Ok((cmd_tx, steer))
}

fn system_prompt(cfg: &Config) -> String {
    let (base, instructions) = system_prompt_parts(cfg);
    base + &instructions
}

/// The system prompt split as (base, appended instructions block) — the
/// context breakdown reports the two separately.
pub(crate) fn system_prompt_parts(cfg: &Config) -> (String, String) {
    let base = match &cfg.system_prompt {
        Some(custom) => custom.replace("{root}", &cfg.root.display().to_string()),
        None => default_system_prompt(cfg),
    };
    let mut instructions = String::new();
    for (name, content) in &cfg.instructions {
        instructions.push_str(&format!(
            "\n\nProject instructions from {name} (follow them):\n{content}"
        ));
    }
    (base, instructions)
}

fn default_system_prompt(cfg: &Config) -> String {
    let tool_names: Vec<&str> = crate::tools::ALL_TOOLS
        .iter()
        .copied()
        .filter(|name| !cfg.disable_tools.iter().any(|t| t == name))
        .collect();
    let web_rule = if tool_names.contains(&"web_search") && tool_names.contains(&"web_fetch") {
        "- Use web_search to look things up on the web, and web_fetch to read a URL the \
         user shares or a search result you want in full.\n"
    } else {
        ""
    };
    // Models default to unix command syntax; on Windows one line up front
    // beats letting them discover cmd.exe by trial and error.
    let os_rule = if cfg!(windows) {
        "- You are on Windows: the bash tool runs cmd.exe, so use Windows commands \
         (dir, type, …), not unix ones.\n"
    } else {
        ""
    };
    format!(
        "You are picocode, a coding agent running in a terminal. \
         Your working directory is: {root}\n\
         \n\
         Available tools: {tools}.\n\
         \n\
         Workflow:\n\
         1. Explore first: use list_files and grep to locate relevant files, and read_file \
         before editing anything.\n\
         2. Edit with edit_file: pass old_string (it must match exactly once) to replace \
         it, or omit old_string to create a file with new_string as its content.\n\
         3. Verify your changes with bash (build, test) when appropriate.\n\
         \n\
         Rules:\n\
         - Use the tools instead of guessing about the project.\n\
         {web_rule}\
         {os_rule}\
         - If the user denies a tool call, do not retry it; explain and ask instead.\n\
         - Keep responses concise. Respond in the language the user writes in.",
        root = cfg.root.display(),
        tools = tool_names.join(", "),
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

#[allow(clippy::too_many_arguments)]
async fn worker<M>(
    agent: Agent<M>,
    compactor: Agent<M>,
    mut cmd_rx: mpsc::Receiver<WorkerCmd>,
    event_tx: mpsc::Sender<AgentEvent>,
    cfg: Config,
    mut cancel_rx: watch::Receiver<()>,
    journal: crate::undo::UndoJournal,
    steer: crate::steer::SteerQueue,
) where
    M: CompletionModel + 'static,
    M::StreamingResponse: GetTokenUsage,
{
    let mut history: Vec<Message> = Vec::new();
    // Context tokens of the last completion request, for the pruning stage.
    let mut last_ctx: u64 = 0;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            WorkerCmd::Clear => {
                history.clear();
                last_ctx = 0;
            }
            WorkerCmd::Undo => {
                let restored = journal.undo();
                let summary = undo_summary(&restored);
                // Tell the model its edits were rolled back, the same way
                // `!` shell commands are recorded — otherwise it believes
                // its previous changes are still in place.
                if !summary.is_empty() {
                    history.push(Message::user(format!(
                        "I reverted the file changes from your last turn:\n{summary}"
                    )));
                }
                let _ = event_tx.send(AgentEvent::Undone { summary }).await;
            }
            WorkerCmd::ShellRecord { command, output } => {
                history.push(Message::user(format!(
                    "I ran this shell command myself in the working directory:\n\
                     $ {command}\n\nOutput:\n{output}"
                )));
            }
            WorkerCmd::TakeHistory(tx) => {
                let _ = tx.send(history.clone());
            }
            WorkerCmd::SeedHistory(h) => history = h,
            WorkerCmd::Prompt { text, attachments } => {
                // Soft context stage, before full compaction is needed: at
                // 2/3 of the auto-compact threshold, swap old tool outputs
                // for placeholders (recent turns stay untouched).
                let pct = cfg.auto_compact.get();
                if pct > 0 && last_ctx >= cfg.context_window.saturating_mul(pct * 2 / 3) / 100 {
                    let outputs = crate::history::prune_tool_outputs(
                        &mut history,
                        crate::history::KEEP_RECENT_TURNS,
                    );
                    if outputs > 0 {
                        let _ = event_tx.send(AgentEvent::Pruned { outputs }).await;
                    }
                }
                journal.begin_turn();
                // A steer message that raced the previous turn's end (pushed
                // just as it finished) joins this prompt instead of being
                // injected into an unrelated tool result later.
                let text = {
                    let mut pending = steer.drain();
                    if pending.is_empty() {
                        text
                    } else {
                        pending.push(text);
                        pending.join("\n\n")
                    }
                };
                let mut cancelled = run_once(
                    &agent,
                    &mut history,
                    text,
                    attachments,
                    &event_tx,
                    &cfg,
                    &mut cancel_rx,
                    &mut last_ctx,
                    &steer,
                )
                .await;
                // Steer messages that missed every tool boundary (or arrived
                // after the last one) run as follow-up prompts of the same
                // turn, so they are never silently dropped.
                while !cancelled {
                    let leftover = steer.drain();
                    if leftover.is_empty() {
                        break;
                    }
                    cancelled = run_once(
                        &agent,
                        &mut history,
                        leftover.join("\n\n"),
                        Vec::new(),
                        &event_tx,
                        &cfg,
                        &mut cancel_rx,
                        &mut last_ctx,
                        &steer,
                    )
                    .await;
                }
                if cancelled {
                    let dropped = steer.drain();
                    if !dropped.is_empty() {
                        let _ = event_tx
                            .send(AgentEvent::Error(format!(
                                "stopped — {} pending message(s) were not delivered",
                                dropped.len()
                            )))
                            .await;
                    }
                }
                let _ = event_tx
                    .send(AgentEvent::ContextBreakdown(crate::context::breakdown(
                        &cfg, &history, last_ctx,
                    )))
                    .await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
            }
            WorkerCmd::Compact => {
                compact(&compactor, &mut history, &event_tx, &mut cancel_rx).await;
                // The provider hasn't measured the compacted history yet, so
                // report estimates only (reported = 0).
                let _ = event_tx
                    .send(AgentEvent::ContextBreakdown(crate::context::breakdown(
                        &cfg, &history, 0,
                    )))
                    .await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
            }
        }
    }
}

/// Ask the tool-less compactor agent to summarize the messages older than
/// the last [`crate::history::KEEP_RECENT_TURNS`] user turns, then replace
/// that older part with the summary — the recent turns (the current task's
/// context) survive verbatim. On failure the history is left untouched.
async fn compact<M>(
    compactor: &Agent<M>,
    history: &mut Vec<Message>,
    event_tx: &mpsc::Sender<AgentEvent>,
    cancel: &mut watch::Receiver<()>,
) where
    M: CompletionModel + 'static,
{
    let boundary = crate::history::keep_boundary(history, crate::history::KEEP_RECENT_TURNS);
    if boundary == 0 {
        // Nothing older than the kept turns: nothing to compact.
        let _ = event_tx
            .send(AgentEvent::Compacted {
                messages: 0,
                summary: String::new(),
            })
            .await;
        return;
    }

    let old: Vec<Message> = history[..boundary].to_vec();
    let messages = old.len();
    let _ = cancel.borrow_and_update(); // discard stale signals
    let result = tokio::select! {
        biased;
        _ = cancel.changed() => {
            let _ = event_tx.send(AgentEvent::Cancelled).await;
            return; // history untouched
        }
        res = async { compactor.prompt(COMPACT_REQUEST).history(old).await } => res,
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
            let tail: Vec<Message> = history.split_off(boundary);
            history.clear();
            history.push(Message::user(format!(
                "Summary of our conversation so far (earlier messages were compacted to save context):\n\n{summary}"
            )));
            history.push(Message::assistant(
                "Understood — I'll continue from that summary.",
            ));
            history.extend(tail);
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

/// Drive one prompt through the multi-turn stream. Returns whether the run
/// was cancelled (Stop/Esc), so the worker can skip steer follow-ups.
#[allow(clippy::too_many_arguments)]
async fn run_once<M>(
    agent: &Agent<M>,
    history: &mut Vec<Message>,
    prompt: String,
    attachments: Vec<Attachment>,
    event_tx: &mpsc::Sender<AgentEvent>,
    cfg: &Config,
    cancel: &mut watch::Receiver<()>,
    last_ctx: &mut u64,
    steer: &crate::steer::SteerQueue,
) -> bool
where
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
             commands; edit_file/bash are blocked. When the plan is ready, \
             call submit_plan with the full plan text to ask the user for approval — \
             if approved you are switched to edit mode and must execute it.]"
        )
    } else {
        prompt
    };
    // Small local models often fail to attend to attached media unless the
    // text mentions it (observed with gemma on Ollama: the same request
    // flip-flops between describing the image and claiming there is none,
    // and an explicit note makes it reliable). List the attachments in the
    // prompt text.
    let prompt = if attachments.is_empty() {
        prompt
    } else {
        let list = attachments
            .iter()
            .map(|a| format!("{} ({})", a.name(), a.kind.label()))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{prompt}\n\n[{n} file(s) attached to this message: {list}]",
            n = attachments.len()
        )
    };
    // Attachments become extra content parts of the user message. Unreadable
    // files are reported and skipped rather than aborting the turn.
    let mut content = vec![UserContent::text(&prompt)];
    for att in &attachments {
        match att.to_user_content() {
            Ok(c) => content.push(c),
            Err(e) => {
                let _ = event_tx
                    .send(AgentEvent::Error(format!(
                        "could not read attachment {}: {e}",
                        att.path.display()
                    )))
                    .await;
            }
        }
    }
    let user_msg = Message::User {
        // Never empty: the prompt text is always the first item.
        content: rig::OneOrMany::many(content).expect("user content starts with the prompt text"),
    };
    let mut got_final = false;
    let mut cancelled = false;
    let mut attempt = 0usize;
    let _ = cancel.borrow_and_update(); // discard stale signals

    'attempts: loop {
        let hook = ApprovalHook::new(event_tx.clone(), cfg.approval.clone(), cfg.mode.clone());
        // rig's multi-turn driver needs some bound, but picocode doesn't cap
        // turns itself: the context window (with auto-compact) is the real
        // limit, so pass an effectively-unlimited value.
        let mut stream = agent
            .stream_chat(user_msg.clone(), history.clone())
            .max_turns(usize::MAX)
            .add_hook(hook)
            .add_hook(crate::steer::SteerHook::new(steer.clone()))
            .await;

        let mut reasoning_delta_seen = false;
        // Whether anything arrived this attempt. Retrying is only safe while
        // nothing has: once text streamed or a tool ran, a restart would
        // duplicate output (and possibly side effects), so mid-turn errors
        // are reported instead of retried.
        let mut progress = false;
        let mut transient: Option<String> = None;

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
            if item.is_ok() {
                progress = true;
            }
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
                    *last_ctx = call.usage.input_tokens;
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
                            history.push(user_msg.clone());
                            if !response.output.is_empty() {
                                history.push(Message::assistant(response.output.clone()));
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if !progress && attempt < RETRY_DELAYS_SECS.len() && is_transient(&msg) {
                        transient = Some(msg);
                        break;
                    }
                    let _ = event_tx.send(AgentEvent::Error(msg)).await;
                }
            }
        }

        if cancelled {
            drop(stream);
            let _ = event_tx.send(AgentEvent::Cancelled).await;
            break 'attempts;
        }
        // Transient failure before anything happened: back off and redo the
        // whole request (nothing to duplicate yet).
        if let Some(msg) = transient {
            drop(stream);
            let delay = std::time::Duration::from_secs(RETRY_DELAYS_SECS[attempt]);
            attempt += 1;
            let _ = event_tx
                .send(AgentEvent::Error(format!(
                    "provider error: {msg} — retrying in {}s (attempt {attempt}/{})",
                    delay.as_secs(),
                    RETRY_DELAYS_SECS.len(),
                )))
                .await;
            tokio::select! {
                biased;
                _ = cancel.changed() => {
                    cancelled = true;
                    let _ = event_tx.send(AgentEvent::Cancelled).await;
                    break 'attempts;
                }
                _ = tokio::time::sleep(delay) => {}
            }
            continue 'attempts;
        }
        break;
    }

    if !got_final {
        // The run was cancelled or errored out; keep the user's message
        // (attachments included) so the next turn still has it as context.
        history.push(user_msg);
    }
    cancelled
}

/// Backoff schedule for transient provider failures at the start of a turn.
const RETRY_DELAYS_SECS: [u64; 3] = [1, 2, 4];

/// Whether a provider error is worth retrying: connection trouble, timeouts,
/// rate limits and 5xx-style server errors (matched textually — rig flattens
/// provider errors to strings).
fn is_transient(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "connection",
        "connect error",
        // reqwest's opaque transport failure (it hides the io cause).
        "error sending request",
        "timed out",
        "timeout",
        "reset",
        "refused",
        "broken pipe",
        "unexpected eof",
        "dns error",
        "429",
        "500",
        "502",
        "503",
        "504",
        "529",
        "overloaded",
        "unavailable",
        "rate limit",
    ]
    .iter()
    .any(|needle| e.contains(needle))
}

/// Human-readable `/undo` result, one line per file; empty when there was
/// nothing to undo. Shown to the user and recorded for the model.
fn undo_summary(restored: &[crate::undo::Restored]) -> String {
    use crate::undo::Restored;
    restored
        .iter()
        .map(|r| match r {
            Restored::Reverted(p) => format!("restored {}", p.display()),
            Restored::Removed(p) => format!("deleted {} (the edit had created it)", p.display()),
            Restored::Failed(p, e) => format!("FAILED to restore {}: {e}", p.display()),
        })
        .collect::<Vec<_>>()
        .join("\n")
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
            bash_timeout: crate::config::NumHandle::new(120),
            read_max_lines: crate::config::NumHandle::new(2000),
            read_max_line_bytes: crate::config::NumHandle::new(500),
            auto_compact: crate::config::NumHandle::new(85),
            root: PathBuf::from("/tmp/proj"),
            approval: RulesHandle::new(ApprovalRules::default()),
            mode: ModeHandle::new(Mode::ReadOnly),
            search: crate::config::SearchHandle::new(SearchConfig {
                provider: SearchProvider::Duckduckgo,
                base_url: None,
                max_results: 5,
                api_key: None,
            }),
            disable_tools: Vec::new(),
            after_edit: None,
            system_prompt: None,
            instructions: Vec::new(),
            config_files: Vec::new(),
            context_window: crate::config::DEFAULT_CONTEXT_WINDOW,
        }
    }

    #[test]
    fn transient_errors_are_recognized() {
        assert!(is_transient(
            "error trying to connect: tcp connect error: Connection refused (os error 61)"
        ));
        assert!(is_transient(
            "HTTP status server error (503 Service Unavailable)"
        ));
        assert!(is_transient("request timed out"));
        assert!(is_transient(
            "CompletionError: HttpError: Http client error: error sending request for url (http://x/api/chat)"
        ));
        assert!(is_transient("429 Too Many Requests"));
        assert!(!is_transient("invalid api key"));
        assert!(!is_transient("model `nope` not found"));
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
    fn default_prompt_reflects_disabled_tools() {
        let cfg = test_cfg();
        let p = system_prompt(&cfg);
        assert!(p.contains("web_search"));
        assert!(p.contains("Use web_search"));

        let mut cfg = test_cfg();
        cfg.disable_tools = vec!["web_search".into(), "web_fetch".into()];
        let p = system_prompt(&cfg);
        assert!(!p.contains("web_search"));
        assert!(!p.contains("web_fetch"));
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
