//! Channel message types shared between the TUI (App) and the agent worker.

use rig::completion::Message;
use tokio::sync::oneshot;

use crate::attachment::Attachment;
use crate::config::Provider;

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
    /// In auto mode: the reviewer model answered an approval prompt on the
    /// user's behalf. Reported so the user can see what ran unattended.
    AutoDecision {
        name: String,
        allowed: bool,
        reason: String,
    },
    /// In goal mode: the reviewer model judged whether the `/goal` condition
    /// is met after a turn. `round` counts the checks so far and `max` is
    /// the round limit, so a front end can show `2/10` and say when the
    /// loop stopped short.
    GoalCheck {
        round: u64,
        max: u64,
        done: bool,
        reason: String,
    },
    /// The model asked the user a question via a dialog (`submit_plan`'s
    /// approval). The answer is the selected index, or `None` if dismissed
    /// with Esc. `title` names the dialog box.
    UserQuestion {
        title: String,
        question: String,
        options: Vec<String>,
        respond: oneshot::Sender<Option<usize>>,
    },
    /// Token usage for one completion request within the run.
    Usage { input: u64, output: u64 },
    /// Estimated context composition, refreshed at the end of each turn
    /// (and after /compact). Backs the colored `/status` detail block.
    ContextBreakdown(crate::context::Breakdown),
    /// Result of asking the provider which models it serves. Updates the
    /// `/model` switch candidates and the dialog when open; `label` names
    /// the queried endpoint (for error reporting).
    ModelList {
        label: String,
        result: Result<Vec<String>, String>,
    },
    /// Result of the add-model form's model-list probe. Separate from
    /// [`ModelList`](Self::ModelList), which caches the *current* endpoint's
    /// models; the echoed provider/base identify which probe answered (a
    /// stale reply after the user changed the form is dropped).
    FormModelList {
        provider: Provider,
        base_url: Option<String>,
        result: Result<Vec<String>, String>,
    },
    /// The conversation history was compacted into a summary.
    /// `messages == 0` means there was nothing to compact.
    Compacted { messages: usize, summary: String },
    /// Old tool outputs were replaced with placeholders to relieve context
    /// pressure (the soft stage before full compaction).
    Pruned { outputs: usize },
    /// Output of a user-typed `!` shell command.
    ShellOutput { output: String },
    /// A bash command hit its timeout and was moved to the background
    /// (counted in the status bar; the GUI shows `command` in its
    /// background-jobs popup).
    BackgroundStarted { id: u64, command: String },
    /// A backgrounded bash command finished. The App displays the output and
    /// prompts the model with it so it reacts to the result.
    BackgroundDone {
        id: u64,
        command: String,
        output: String,
    },
    /// `/undo` finished: a human-readable per-file summary, or an empty
    /// string when there was nothing to undo.
    Undone { summary: String },
    /// The user stopped the current generation with Esc.
    Cancelled,
    /// The current run finished (successfully or not).
    TurnComplete,
    /// An error occurred during the run.
    Error(String),
}

/// Commands sent from the TUI to the agent worker.
pub enum WorkerCmd {
    /// Run the agent on a user prompt, with optional file attachments sent
    /// as multimodal message content.
    Prompt {
        text: String,
        attachments: Vec<Attachment>,
    },
    /// Clear the conversation history.
    Clear,
    /// Set (or clear, with `None`) the `/goal` condition: after each turn
    /// the worker asks the reviewer model whether it is met and keeps
    /// working until it is, the round limit is reached, or Esc stops it.
    SetGoal(Option<String>),
    /// Summarize the history and replace it with the summary.
    Compact,
    /// Record a user-run `!` shell command and its output in the history so
    /// the model has it as context.
    ShellRecord { command: String, output: String },
    /// Revert the file edits of the most recent turn that made any
    /// (`/undo`); repeatable to walk further back.
    Undo,
    /// Send a copy of the history back (used when switching models).
    TakeHistory(oneshot::Sender<Vec<Message>>),
    /// Replace the history (seeds a freshly spawned worker on model switch).
    SeedHistory(Vec<Message>),
}
