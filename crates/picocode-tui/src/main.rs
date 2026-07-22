mod app;
mod highlight;
mod history;
mod input;
mod markdown;
mod ui;

use clap::Parser;

use picocode_core::event::{AgentEvent, WorkerCmd};
use picocode_core::{agent, config, models};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = config::Args::parse();
    let smoke = args.smoke.clone();
    let smoke_attach = args.smoke_attach.clone();
    let smoke_steer = args.smoke_steer.clone();
    let print = args.print.clone();
    let print_attach = args.attach.clone();
    let mut cfg = config::Config::from_args(args)?;
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
    let (cmd_tx, steer) = agent::spawn(&cfg, event_tx.clone(), cancel_rx)?;

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
    let result = app::App::new(&cfg, event_tx, cmd_tx, steer, cancel_tx)
        .run(terminal, event_rx)
        .await;
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
            AgentEvent::ToolResult { output } => {
                let first = output.lines().next().unwrap_or("");
                eprintln!("[tool result] {first} … ({} bytes)", output.len());
            }
            AgentEvent::ApprovalRequest { name, respond, .. } => {
                eprintln!("[denied] {name} needs confirmation — run with --bypass to allow");
                let _ = respond.send(false);
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
            AgentEvent::ModelList { .. } => {}
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
            AgentEvent::Cancelled => println!("\n[cancelled]"),
            AgentEvent::Error(e) => println!("\n[error] {e}"),
            AgentEvent::TurnComplete => break,
        }
    }
    println!();
    Ok(())
}
