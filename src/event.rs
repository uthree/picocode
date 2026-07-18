//! Channel message types shared between the TUI (App) and the agent worker.

use rig::completion::Message;
use tokio::sync::oneshot;

/// Events sent from the agent worker / approval hook to the TUI.
pub enum AgentEvent {
    /// Streamed assistant text delta.
    TextDelta(String),
    /// Streamed reasoning (thinking) delta.
    ReasoningDelta(String),
    /// The model emitted a tool call.
    ToolCall { name: String, args: String },
    /// A tool produced a result (or a skip reason).
    ToolResult { output: String },
    /// A destructive tool call awaits user approval.
    ApprovalRequest {
        name: String,
        args: String,
        respond: oneshot::Sender<bool>,
    },
    /// Token usage for one completion request within the run.
    Usage { input: u64, output: u64 },
    /// The conversation history was compacted into a summary.
    /// `messages == 0` means there was nothing to compact.
    Compacted { messages: usize, summary: String },
    /// Output of a user-typed `!` shell command.
    ShellOutput { output: String },
    /// The user stopped the current generation with Esc.
    Cancelled,
    /// The current run finished (successfully or not).
    TurnComplete,
    /// An error occurred during the run.
    Error(String),
}

/// Commands sent from the TUI to the agent worker.
pub enum WorkerCmd {
    /// Run the agent on a user prompt.
    Prompt(String),
    /// Clear the conversation history.
    Clear,
    /// Summarize the history and replace it with the summary.
    Compact,
    /// Record a user-run `!` shell command and its output in the history so
    /// the model has it as context.
    ShellRecord { command: String, output: String },
    /// Send a copy of the history back (used when switching models).
    TakeHistory(oneshot::Sender<Vec<Message>>),
    /// Replace the history (seeds a freshly spawned worker on model switch).
    SeedHistory(Vec<Message>),
}
