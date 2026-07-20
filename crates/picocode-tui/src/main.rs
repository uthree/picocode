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
    let mut cfg = config::Config::from_args(args)?;
    if smoke.is_some() {
        cfg.mode.set(config::Mode::Bypass);
    }
    // No model configured anywhere: use the first model Ollama serves.
    if cfg.model.is_empty() {
        cfg.model = pick_ollama_model(&cfg).await?;
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
    let result = app::App::new(&cfg, event_tx, cmd_tx, cancel_tx)
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

/// Pick the first model the Ollama server reports serving; when it can't,
/// explain how to set up a model provider instead of starting broken.
async fn pick_ollama_model(cfg: &config::Config) -> anyhow::Result<String> {
    let base = models::base_url(config::Provider::Ollama, cfg.base_url.as_deref());
    const HINT: &str = "configure a model provider instead:\n  \
         - add a [[models]] entry to picocode.toml (see the README), or\n  \
         - pass --provider and --model on the command line";
    match models::fetch(config::Provider::Ollama, cfg.base_url.as_deref()).await {
        Ok(list) => match list.into_iter().next() {
            Some(model) => Ok(model),
            None => anyhow::bail!(
                "Ollama at {base} has no models pulled.\n\
                 Pull one (e.g. `ollama pull qwen3:4b`), or {HINT}"
            ),
        },
        Err(e) => anyhow::bail!(
            "No model is configured and Ollama is not reachable at {base} ({e:#}).\n\
             Install and start it (https://ollama.com/download), or {HINT}"
        ),
    }
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
            AgentEvent::BackgroundStarted { id } => println!("[background job #{id} started]"),
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
