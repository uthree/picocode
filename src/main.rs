mod agent;
mod app;
mod approval;
mod config;
mod event;
mod session;
mod tools;
mod ui;

use clap::Parser;

use crate::event::{AgentEvent, WorkerCmd};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = config::Args::parse();
    let smoke = args.smoke.clone();
    let mut cfg = config::Config::from_args(args)?;
    if smoke.is_some() {
        cfg.yolo = true;
    }

    // Fail before entering the TUI if the provider client can't be built
    // (e.g. a missing API key).
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(());
    let cmd_tx = agent::spawn(&cfg, event_tx.clone(), cancel_rx)?;

    if let Some(prompt) = smoke {
        return run_smoke(prompt, event_rx, cmd_tx).await;
    }

    let terminal = ratatui::init();
    // Mouse capture enables wheel scrolling in the transcript. Terminal-native
    // text selection still works with Shift (or Option on macOS) held.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    );
    let result = app::App::new(&cfg, event_tx, cmd_tx, cancel_tx)
        .run(terminal, event_rx)
        .await;
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    result
}

/// Headless debug mode: run one prompt and print the event stream.
async fn run_smoke(
    prompt: String,
    mut event_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    cmd_tx: tokio::sync::mpsc::Sender<WorkerCmd>,
) -> anyhow::Result<()> {
    use std::io::Write;
    cmd_tx.send(WorkerCmd::Prompt(prompt)).await?;
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
            AgentEvent::Usage { input, output } => println!("\n[usage] ctx={input} out={output}"),
            AgentEvent::Compacted { messages, summary } => {
                println!("\n[compacted] {messages} messages\n{summary}");
            }
            AgentEvent::ShellOutput { output } => println!("[shell]\n{output}"),
            AgentEvent::Cancelled => println!("\n[cancelled]"),
            AgentEvent::Error(e) => println!("\n[error] {e}"),
            AgentEvent::TurnComplete => break,
        }
    }
    println!();
    Ok(())
}
