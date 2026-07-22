//! Conversation-history surgery for context savings.
//!
//! Two stages, both keeping the most recent turns untouched:
//! - [`prune_tool_outputs`] — the soft stage: bulky old tool outputs are
//!   replaced with a short placeholder once context pressure builds, long
//!   before full compaction is needed.
//! - The compaction in `agent.rs` summarizes only the messages older than
//!   [`KEEP_RECENT_TURNS`], so the working context (the current task's
//!   files and results) survives a compact verbatim.

use rig::completion::Message;
use rig::message::{ToolResultContent, UserContent};

/// User turns whose messages are never pruned or summarized away.
pub const KEEP_RECENT_TURNS: usize = 2;

/// What an old tool output is replaced with.
pub const PRUNED_PLACEHOLDER: &str =
    "[old tool output removed to save context — re-run the tool if you need it again]";

/// Tool-result texts at or below this size are kept (pruning them saves
/// nothing worth the churn).
const PRUNE_MIN_BYTES: usize = 200;

/// Whether a message is a real user prompt (text/media, no tool results) —
/// the start of a turn. Tool results are `User`-role messages too, so the
/// distinction is by content.
fn is_user_prompt(msg: &Message) -> bool {
    match msg {
        Message::User { content } => !content
            .iter()
            .any(|c| matches!(c, UserContent::ToolResult(_))),
        _ => false,
    }
}

/// Index of the message starting the `keep_turns`-th user turn from the
/// end: everything before it is "old". 0 when the history holds that few
/// turns (nothing is old).
pub fn keep_boundary(history: &[Message], keep_turns: usize) -> usize {
    let mut seen = 0;
    for (ix, msg) in history.iter().enumerate().rev() {
        if is_user_prompt(msg) {
            seen += 1;
            if seen >= keep_turns {
                return ix;
            }
        }
    }
    0
}

/// Replace bulky tool outputs older than the last [`KEEP_RECENT_TURNS`]
/// user turns with [`PRUNED_PLACEHOLDER`] (images count as bulky). Returns
/// how many outputs were replaced; already-pruned and small outputs are
/// left alone, so repeated calls converge to 0.
pub fn prune_tool_outputs(history: &mut [Message], keep_turns: usize) -> usize {
    let boundary = keep_boundary(history, keep_turns);
    let mut replaced = 0;
    for msg in &mut history[..boundary] {
        let Message::User { content } = msg else {
            continue;
        };
        for part in content.iter_mut() {
            let UserContent::ToolResult(tool_result) = part else {
                continue;
            };
            for item in tool_result.content.iter_mut() {
                match item {
                    ToolResultContent::Text(t) if t.text.len() > PRUNE_MIN_BYTES => {
                        t.text = PRUNED_PLACEHOLDER.to_string();
                        replaced += 1;
                    }
                    ToolResultContent::Image(_) => {
                        *item = ToolResultContent::text(PRUNED_PLACEHOLDER);
                        replaced += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::OneOrMany;
    use rig::message::ToolResult;

    fn tool_result_msg(text: &str) -> Message {
        Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: "t1".into(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text(text)),
            })),
        }
    }

    fn tool_text(msg: &Message) -> String {
        let Message::User { content } = msg else {
            panic!("not a user message");
        };
        let Some(UserContent::ToolResult(tr)) = content.iter().next() else {
            panic!("not a tool result");
        };
        match tr.content.iter().next() {
            Some(ToolResultContent::Text(t)) => t.text.clone(),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn prunes_only_old_bulky_outputs() {
        let big = "x".repeat(1_000);
        let mut history = vec![
            Message::user("turn one"),
            tool_result_msg(&big),
            tool_result_msg("small"),
            Message::assistant("done"),
            Message::user("turn two"),
            tool_result_msg(&big),
            Message::user("turn three"),
        ];

        // Keep the last 2 turns: only turn one's outputs are old.
        let replaced = prune_tool_outputs(&mut history, 2);
        assert_eq!(replaced, 1);
        assert_eq!(tool_text(&history[1]), PRUNED_PLACEHOLDER);
        assert_eq!(tool_text(&history[2]), "small"); // small: kept
        assert_eq!(tool_text(&history[5]), big); // recent: kept

        // Idempotent: nothing further to prune.
        assert_eq!(prune_tool_outputs(&mut history, 2), 0);
    }

    #[test]
    fn boundary_handles_short_histories() {
        let history = vec![Message::user("only turn"), Message::assistant("hi")];
        assert_eq!(keep_boundary(&history, 2), 0);
        assert_eq!(keep_boundary(&[], 2), 0);

        let mut history = vec![Message::user("a"), tool_result_msg(&"y".repeat(500))];
        // Everything is within the kept turns: no pruning.
        assert_eq!(prune_tool_outputs(&mut history, 2), 0);
    }
}
