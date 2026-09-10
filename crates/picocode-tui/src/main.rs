mod app;
mod highlight;
mod history;
mod input;
mod markdown;
mod ui;

// The `/config` dialog's own strings; its row labels and section headings
// come from picocode-core's catalog so both front ends name them alike.
// The rest of the TUI is still English only.
rust_i18n::i18n!("locales", fallback = "en");

use clap::Parser;

use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::{agent, config, models};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    picocode_core::set_locale_from_system();
    let args = config::Args::parse();
    let smoke = args.smoke.clone();
    let smoke_attach = args.smoke_attach.clone();
    let smoke_steer = args.smoke_steer.clone();
    let smoke_goal = args.smoke_goal.clone();
    let print = args.print.clone();
    let print_attach = args.attach.clone();
    let mut cfg = config::Config::from_args(args)?;
    // Settings changed in an earlier run's `/config` are a sparse overlay
    // on the config file (untouched values keep following it), shared with
    // the GUI.
    let saved = config::saved::load();
    saved.apply(&mut cfg);
    if smoke.is_some() {
        cfg.mode.set(config::Mode::Bypass);
    }
    // No model configured anywhere: use the first model Ollama serves.
    if cfg.model.is_empty() {
        cfg.model = models::pick_ollama_model(&cfg).await?;
    }

    // Fail before entering the TUI if the provider client can't be built
    // (e.g. a missing API key).
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(());
    let jobs = picocode_core::tools::BackgroundJobs::new();
    // Remote workspace: connect over SSH and apply the host's picocode.toml
    // and instruction files before spawning the worker (a failure here is
    // fatal — there is no workspace to operate on).
    let backend = picocode_core::workspace::connect(&mut cfg).await?;
    // Opt-in MCP servers connect once here; failures are shown, not fatal.
    let (mcp, mcp_errors) = picocode_core::mcp::connect_all(&cfg.mcp_servers).await;
    for error in mcp_errors {
        let _ = event_tx
            .send(picocode_core::event::AgentEvent::Error(error))
            .await;
    }
    let (cmd_tx, steer) = agent::spawn(
        &cfg,
        event_tx.clone(),
        cancel_rx,
        jobs.clone(),
        mcp.clone(),
        backend.clone(),
    )?;

    if let Some(prompt) = smoke {
        let attachments = smoke_attach
            .as_deref()
            .and_then(picocode_core::attachment::Attachment::detect)
            .into_iter()
            .collect();
        // E2E for mid-turn steering: push the text while the turn runs.
        if let Some(text) = smoke_steer {
            let steer = steer.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                steer.push(text);
            });
        }
        // E2E for `/goal`: set the goal the worker judges after each turn.
        if let Some(goal) = smoke_goal {
            cmd_tx
                .send(picocode_core::event::WorkerCmd::SetGoal(Some(goal)))
                .await?;
        }
        return run_smoke(prompt, attachments, event_rx, cmd_tx).await;
    }
    if let Some(prompt) = print {
        let mut attachments = Vec::new();
        for path in &print_attach {
            match picocode_core::attachment::Attachment::detect(path) {
                Some(att) if att.supported_by(cfg.provider) => attachments.push(att),
                Some(_) => anyhow::bail!(
                    "--attach {}: this file type is not supported by the configured provider",
                    path.display()
                ),
                None => anyhow::bail!(
                    "--attach {}: unsupported binary format (images, audio, PDF and text work)",
                    path.display()
                ),
            }
        }
        return run_print(prompt, attachments, event_rx, cmd_tx).await;
    }

    let terminal = ratatui::init();
    // Mouse capture enables wheel scrolling in the transcript (terminal-native
    // text selection still works with Shift, or Option on macOS, held);
    // bracketed paste lets multi-line pastes arrive as one event instead of
    // the newlines submitting early. Separate calls: a terminal without
    // bracketed paste (legacy Windows console) shouldn't lose the mouse too.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    );
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableBracketedPaste
    );
    // Terminals implementing the kitty keyboard protocol can tell
    // Shift/Ctrl/Super+Enter apart from a plain one; ask them to, so the
    // configured send key (`submit_key`) actually works. Only the
    // disambiguation flag is requested — key releases and other extras would
    // change how the rest of the input arrives.
    let enhanced_keys = matches!(
        ratatui::crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    if enhanced_keys {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::PushKeyboardEnhancementFlags(
                ratatui::crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
    let result = app::App::new(
        &cfg,
        event_tx,
        cmd_tx,
        steer,
        jobs,
        cancel_tx,
        mcp,
        backend,
        enhanced_keys,
        saved,
    )
    .run(terminal, event_rx)
    .await;
    if enhanced_keys {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableBracketedPaste
    );
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    result
}

/// `-p/--print`: run one prompt headless. The reply streams to stdout so
/// pipes get clean text; tool activity, notices and errors go to stderr.
/// Tool calls needing confirmation are denied (no interactive approval) —
/// `--bypass` allows everything, like an isolated-environment run.
async fn run_print(
    prompt: String,
    attachments: Vec<picocode_core::attachment::Attachment>,
    mut event_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    cmd_tx: tokio::sync::mpsc::Sender<WorkerCmd>,
) -> anyhow::Result<()> {
    use std::io::Write;
    cmd_tx
        .send(WorkerCmd::Prompt {
            text: prompt,
            attachments,
        })
        .await?;
    while let Some(ev) = event_rx.recv().await {
        match ev {
            AgentEvent::TextDelta(s) => {
                print!("{s}");
                std::io::stdout().flush().ok();
            }
            AgentEvent::ReasoningDelta(_) => {}
            AgentEvent::ToolCall { name, args } => eprintln!("[tool] {name} {args}"),
            AgentEvent::SubagentToolCall { id, name, args } => {
                eprintln!("[subagent #{id} tool] {name} {args}");
            }
            AgentEvent::SubagentUsage { id, input, output } => {
                eprintln!("[subagent #{id} usage] input={input} output={output}");
            }
            AgentEvent::SubagentStarted { id } => eprintln!("[subagent #{id} started]"),
            AgentEvent::SubagentFinished { id, success } => {
                eprintln!("[subagent #{id} finished] success={success}")
            }
            AgentEvent::SubagentToolResult { id, output } => {
                eprintln!(
                    "[subagent #{id} result] {}",
                    output.lines().next().unwrap_or("")
                );
            }
            AgentEvent::ToolResult { output } => {
                let first = output.lines().next().unwrap_or("");
                eprintln!("[tool result] {first} … ({} bytes)", output.len());
            }
            AgentEvent::ApprovalRequest { name, respond, .. } => {
                eprintln!("[denied] {name} needs confirmation — run with --bypass to allow");
                let _ = respond.send(false);
            }
            AgentEvent::AutoDecision {
                agent_id,
                name,
                allowed,
                reason,
            } => {
                let name = match agent_id {
                    Some(id) => format!("subagent #{id}: {name}"),
                    None => name,
                };
                let verb = if allowed { "approved" } else { "refused" };
                eprintln!("[auto {verb}] {name}: {reason}");
            }
            AgentEvent::UserQuestion { title, respond, .. } => {
                eprintln!("[dismissed] {title}: no interactive input in print mode");
                let _ = respond.send(None);
            }
            AgentEvent::Error(e) => eprintln!("[error] {e}"),
            AgentEvent::ShellOutput { output } | AgentEvent::Undone { summary: output } => {
                eprintln!("{output}");
            }
            AgentEvent::BackgroundDone { id, .. } => eprintln!("[background job #{id} done]"),
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    println!();
    Ok(())
}

/// Headless debug mode: run one prompt and print the event stream.
async fn run_smoke(
    prompt: String,
    attachments: Vec<picocode_core::attachment::Attachment>,
    mut event_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    cmd_tx: tokio::sync::mpsc::Sender<WorkerCmd>,
) -> anyhow::Result<()> {
    use std::io::Write;
    cmd_tx
        .send(WorkerCmd::Prompt {
            text: prompt,
            attachments,
        })
        .await?;
    while let Some(ev) = event_rx.recv().await {
        match ev {
            AgentEvent::TextDelta(s) => {
                print!("{s}");
                std::io::stdout().flush().ok();
            }
            AgentEvent::ReasoningDelta(_) => {}
            AgentEvent::ToolCall { name, args } => println!("\n[tool] {name} {args}"),
            AgentEvent::SubagentToolCall { id, name, args } => {
                println!("\n[subagent #{id} tool] {name} {args}");
            }
            AgentEvent::SubagentUsage { id, input, output } => {
                println!("\n[subagent #{id} usage] input={input} output={output}");
            }
            AgentEvent::SubagentStarted { id } => println!("\n[subagent #{id} started]"),
            AgentEvent::SubagentFinished { id, success } => {
                println!("\n[subagent #{id} finished] success={success}")
            }
            AgentEvent::SubagentToolResult { id, output } => {
                println!(
                    "[subagent #{id} result] {}",
                    output.lines().next().unwrap_or("")
                );
            }
            AgentEvent::ToolResult { output } => {
                let first = output.lines().next().unwrap_or("");
                println!("[result] {first} ... ({} bytes)", output.len());
            }
            AgentEvent::ApprovalRequest { respond, .. } => {
                let _ = respond.send(true);
            }
            AgentEvent::UserQuestion {
                title,
                question,
                options,
                respond,
            } => {
                println!("\n[{title}] {question} {options:?} -> auto-picking the first");
                let _ = respond.send(Some(0));
            }
            AgentEvent::Usage { input, output } => println!("\n[usage] ctx={input} out={output}"),
            AgentEvent::ContextBreakdown(_)
            | AgentEvent::ModelList { .. }
            | AgentEvent::FormModelList { .. }
            | AgentEvent::ContextLimit { .. } => {}
            AgentEvent::Compacted { messages, summary } => {
                println!("\n[compacted] {messages} messages\n{summary}");
            }
            AgentEvent::ShellOutput { output } => println!("[shell]\n{output}"),
            AgentEvent::Pruned { outputs } => println!("[pruned {outputs} old tool outputs]"),
            AgentEvent::Undone { summary } => println!("[undo]\n{summary}"),
            AgentEvent::BackgroundStarted { id, .. } => println!("[background job #{id} started]"),
            AgentEvent::BackgroundDone { id, output, .. } => {
                println!("[background job #{id} done]\n{output}");
            }
            AgentEvent::AutoDecision {
                agent_id,
                name,
                allowed,
                reason,
            } => {
                let name = match agent_id {
                    Some(id) => format!("subagent #{id}: {name}"),
                    None => name,
                };
                let verb = if allowed { "approved" } else { "refused" };
                println!("[auto {verb}] {name}: {reason}");
            }
            AgentEvent::GoalCheck {
                round,
                max,
                done,
                reason,
            } => println!("[goal {round}/{max} done={done}] {reason}"),
            AgentEvent::Cancelled => println!("\n[cancelled]"),
            AgentEvent::Error(e) => println!("\n[error] {e}"),
            AgentEvent::TurnComplete => break,
        }
    }
    println!();
    Ok(())
}
