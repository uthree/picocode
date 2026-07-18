use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Provider {
    Ollama,
    Anthropic,
    Openai,
}

/// picocode — a minimal TUI coding agent.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    /// LLM provider to use.
    #[arg(long, value_enum, default_value = "ollama")]
    pub provider: Provider,

    /// Model name. Defaults depend on the provider.
    #[arg(long)]
    pub model: Option<String>,

    /// Skip all tool-approval prompts (dangerous).
    #[arg(long)]
    pub yolo: bool,

    /// Maximum model turns (tool-call rounds) per user prompt.
    #[arg(long, default_value_t = 50)]
    pub max_turns: usize,

    /// Headless mode for debugging: run one prompt without the TUI and print
    /// events to stdout. Implies --yolo.
    #[arg(long, hide = true)]
    pub smoke: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub model: String,
    pub yolo: bool,
    pub max_turns: usize,
    /// Working directory the tools operate in.
    pub root: PathBuf,
}

impl Config {
    pub fn from_args(args: Args) -> anyhow::Result<Self> {
        let model = args.model.unwrap_or_else(|| {
            match args.provider {
                Provider::Ollama => "qwen3:4b",
                Provider::Anthropic => "claude-opus-4-8",
                Provider::Openai => "gpt-4o",
            }
            .to_string()
        });
        Ok(Self {
            provider: args.provider,
            model,
            yolo: args.yolo,
            max_turns: args.max_turns,
            root: std::env::current_dir()?,
        })
    }

    pub fn model_label(&self) -> String {
        let provider = match self.provider {
            Provider::Ollama => "ollama",
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
        };
        format!("{provider}/{}", self.model)
    }
}
