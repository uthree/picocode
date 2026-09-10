//! Human-in-the-loop approval for destructive tool calls.
//!
//! Implemented as a rig [`AgentHook`]: each tool call is first checked against
//! the config allow/deny rules ([`ApprovalRules::decide`]); calls that resolve
//! to `Ask` send an [`AgentEvent::ApprovalRequest`] to the TUI and await the
//! user's y/n decision. Denials are returned to the model as the tool result so
//! it can adapt instead of failing the run.
//!
//! In [`Mode::Auto`] the `Ask` answer comes from a second, tool-less
//! *reviewer* agent instead of the user (see [`review`]). It is a guarded
//! delegation, not a bypass: deny rules still win, the always-ask commands
//! ([`needs_human`]) still reach the user, and anything the reviewer cannot
//! answer — an error, a timeout, an unparsable reply — falls back to the
//! user's prompt rather than to "allow".

use std::sync::Arc;
use std::time::Duration;

use rig::agent::{Agent, AgentHook, HookContext, StepEvent, StepEventKind};
use rig::completion::{CompletionModel, Prompt};
use rig::tool::Tool;
use tokio::sync::{mpsc, oneshot};

use crate::config::{Decision, Mode, ModeHandle, RulesHandle, needs_human};
use crate::event::AgentEvent;
use crate::tools::{Bash, DESTRUCTIVE_TOOLS};

/// How long the reviewer may take before the call falls back to the user.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(90);
/// Cap on the tool arguments and the request text shown to the reviewer.
const REVIEW_ARG_CHARS: usize = 2000;

/// Preamble of the reviewer agent (built in [`crate::agent::spawn`]). It has
/// no tools: its only job is to answer one approval prompt at a time.
pub const REVIEW_PREAMBLE: &str = "You review tool calls made by a coding agent and decide \
     whether each one may run without asking the human. Approve calls that are a normal, \
     reversible step of the request the human made and that stay inside the working \
     directory. Refuse anything that deletes or overwrites data outside the working \
     directory, changes system or global configuration, installs software, publishes or \
     pushes anything, reads or sends credentials, contacts the network for no stated \
     reason, or has nothing to do with the request. When you are unsure, refuse. \
     Answer with a single line: `ALLOW: <short reason>` or `DENY: <short reason>`.";

/// One interactive dialog at a time across a parent and its children. A
/// waiting agent does not prevent the other agents from doing approved work.
#[derive(Clone, Default)]
pub(crate) struct ApprovalGate(Arc<tokio::sync::Mutex<()>>);

impl ApprovalGate {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.lock().await
    }
}

pub struct ApprovalHook<M: CompletionModel> {
    tx: mpsc::Sender<AgentEvent>,
    rules: RulesHandle,
    mode: ModeHandle,
    /// The reviewer agent that answers approval prompts in auto mode.
    reviewer: Arc<Agent<M>>,
    /// What the user asked for this turn, so the reviewer can judge whether
    /// a call belongs to the request.
    intent: String,
    /// The working directory, shown to the reviewer as the boundary calls
    /// are expected to stay inside.
    root: std::path::PathBuf,
    gate: ApprovalGate,
    agent_id: Option<u64>,
}

impl<M: CompletionModel> ApprovalHook<M> {
    pub fn new(
        tx: mpsc::Sender<AgentEvent>,
        rules: RulesHandle,
        mode: ModeHandle,
        reviewer: Arc<Agent<M>>,
        intent: String,
        root: std::path::PathBuf,
    ) -> Self {
        Self {
            tx,
            rules,
            mode,
            reviewer,
            intent,
            root,
            gate: ApprovalGate::default(),
            agent_id: None,
        }
    }

    pub(crate) fn with_gate(mut self, gate: ApprovalGate) -> Self {
        self.gate = gate;
        self
    }

    pub(crate) fn for_subagent(mut self, id: u64) -> Self {
        self.agent_id = Some(id);
        self
    }

    /// Ask the user (the normal approval dialog). `None` means the front end
    /// is gone.
    async fn ask_user(&self, tool_name: &str, args: &str) -> Option<bool> {
        let _guard = self.gate.lock().await;
        let (respond, decision) = oneshot::channel();
        let request = AgentEvent::ApprovalRequest {
            agent_id: self.agent_id,
            name: tool_name.to_string(),
            args: args.to_string(),
            respond,
        };
        self.tx.send(request).await.ok()?;
        // Fail closed: anything but an explicit approval denies the call.
        Some(matches!(decision.await, Ok(true)))
    }
}

/// Extract the `command` string from the bash tool's JSON args.
pub fn bash_command(tool_name: &str, args: &str) -> Option<String> {
    if tool_name != Bash::NAME {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

/// Ask the reviewer agent whether a call may run. `None` means it could not
/// answer (error, timeout, or a reply that named neither verdict) — the
/// caller then falls back to the user.
pub async fn review<M: CompletionModel + 'static>(
    reviewer: &Agent<M>,
    tool_name: &str,
    args: &str,
    intent: &str,
    root: &std::path::Path,
) -> Option<(bool, String)> {
    let question = format!(
        "Working directory: {root}\n\
         What the human asked for: {intent}\n\n\
         The agent wants to call this tool:\n\
         tool: {tool_name}\n\
         arguments: {args}\n\n\
         May it run without asking the human? Answer with one line, \
         `ALLOW: <short reason>` or `DENY: <short reason>`.",
        root = root.display(),
        intent = truncate(intent, REVIEW_ARG_CHARS),
        args = truncate(args, REVIEW_ARG_CHARS),
    );
    let answer = tokio::time::timeout(REVIEW_TIMEOUT, reviewer.prompt(question))
        .await
        .ok()?
        .ok()?;
    parse_verdict(&answer)
}

/// Read `ALLOW`/`DENY` (and the reason after it) out of the reviewer's
/// reply. Reasoning models pad their answer, so every line is scanned and
/// common decorations are stripped; a reply naming neither verdict — or
/// both on the same line — is rejected so the call falls back to the user.
fn parse_verdict(answer: &str) -> Option<(bool, String)> {
    for line in answer.lines() {
        let line = line
            .trim()
            .trim_start_matches(['-', '*', '#', '>', '`', ' ']);
        let upper = line.to_ascii_uppercase();
        // Longest form first, so the reason doesn't start with "ED".
        let (allowed, keyword) = if upper.starts_with("ALLOWED") {
            (true, 7)
        } else if upper.starts_with("ALLOW") {
            (true, 5)
        } else if upper.starts_with("DENIED") {
            (false, 6)
        } else if upper.starts_with("DENY") {
            (false, 4)
        } else {
            continue;
        };
        // The keyword must end the word: "allowance" is not a verdict.
        let rest = &line[keyword..];
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            continue;
        }
        let reason = rest
            .trim_start_matches([':', '-', '—', '*', '.', ' '])
            .trim()
            .trim_end_matches(['*', '`'])
            .trim()
            .to_string();
        return Some((allowed, reason));
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}… (truncated)")
}

impl<M: CompletionModel + 'static> AgentHook<M> for ApprovalHook<M> {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> rig::agent::Flow {
        use rig::agent::Flow;

        let StepEvent::ToolCall {
            tool_name, args, ..
        } = event
        else {
            return Flow::Continue;
        };
        let command = bash_command(tool_name, args);
        // Unknown names are external (MCP) tools: they can do anything,
        // so they need approval like the destructive built-ins.
        let destructive =
            DESTRUCTIVE_TOOLS.contains(&tool_name) || !crate::tools::ALL_TOOLS.contains(&tool_name);
        let mode = self.mode.get();
        match self
            .rules
            .decide(mode, tool_name, command.as_deref(), destructive)
        {
            Decision::Allow => return Flow::Continue,
            Decision::Deny(reason) => return Flow::Skip { reason },
            Decision::Ask => {}
        }

        // Auto mode: the reviewer answers instead of the user, unless the
        // call is one of the always-ask commands.
        if mode == Mode::Auto
            && !needs_human(tool_name, command.as_deref())
            && let Some((allowed, reason)) =
                review(&self.reviewer, tool_name, args, &self.intent, &self.root).await
        {
            {
                let _ = self
                    .tx
                    .send(AgentEvent::AutoDecision {
                        agent_id: self.agent_id,
                        name: tool_name.to_string(),
                        allowed,
                        reason: reason.clone(),
                    })
                    .await;
                if allowed {
                    return Flow::Continue;
                }
                return Flow::Skip {
                    reason: format!(
                        "This `{tool_name}` call was refused by picocode's automatic reviewer: \
                         {reason} Do not retry the same call; take a different approach, or \
                         explain what you need and let the user decide."
                    ),
                };
            }
        }
        // Not auto mode, an always-ask command, or the reviewer could not
        // answer: the user decides.
        match self.ask_user(tool_name, args).await {
            Some(true) => Flow::Continue,
            Some(false) => Flow::Skip {
                reason: format!(
                    "The user denied this `{tool_name}` call. Do not retry the same call; \
                     explain what you wanted to do and ask the user, or take a different approach."
                ),
            },
            None => Flow::Terminate {
                reason: "the UI has shut down".to_string(),
            },
        }
    }

    fn observes(&self, kind: StepEventKind) -> bool {
        // Skip the high-frequency streaming delta events; the steering
        // `ToolCall` event fires regardless of this hint.
        !matches!(
            kind,
            StepEventKind::TextDelta | StepEventKind::ToolCallDelta
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_are_read_out_of_padded_replies() {
        assert_eq!(
            parse_verdict("ALLOW: reads a project file"),
            Some((true, "reads a project file".to_string()))
        );
        assert_eq!(
            parse_verdict("Let me think.\n\n**DENY** — deletes /etc\n"),
            Some((false, "deletes /etc".to_string()))
        );
        assert_eq!(
            parse_verdict("- allowed: within the workspace"),
            Some((true, "within the workspace".to_string()))
        );
        // No verdict at all, or a word that merely starts like one.
        assert_eq!(parse_verdict("I am not sure about this call."), None);
        assert_eq!(parse_verdict("The allowance is unclear"), None);
    }
}
