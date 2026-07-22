//! Mid-turn steering: user messages sent while the agent is working are
//! injected at the next tool-call boundary instead of waiting for the turn
//! to end.
//!
//! The front ends push text into a shared [`SteerQueue`]; a [`SteerHook`]
//! drains it when a tool result comes back and appends the messages to that
//! result via [`Flow::RewriteResult`] — the rewritten result is what the
//! model sees *and* what is recorded into the history, so the injection
//! persists naturally. Multimodal tool outputs (the JSON image convention)
//! are left intact — a rewrite is delivered verbatim and would break their
//! parsing — so injection just waits for the next boundary. Messages still
//! queued when the turn ends are run as a follow-up prompt by the worker.

use std::sync::{Arc, Mutex};

use rig::agent::{AgentHook, Flow, HookContext, StepEvent};
use rig::completion::CompletionModel;

/// Shared queue of user messages awaiting injection. Cloned into the front
/// ends (producers) and the worker/hook (consumer).
#[derive(Clone, Default)]
pub struct SteerQueue(Arc<Mutex<Vec<String>>>);

impl SteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }

    /// Take everything queued (oldest first).
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// Whether a tool result is the multimodal JSON convention (`{"response":
/// …, "parts": […]}`); rewriting those would break rig's image parsing.
fn is_multimodal_output(result: &str) -> bool {
    result.starts_with('{')
        && serde_json::from_str::<serde_json::Value>(result).is_ok_and(|v| v.get("parts").is_some())
}

/// The tool-result text appended when messages are injected.
pub fn injection_note(texts: &[String]) -> String {
    format!(
        "\n\n[The user sent new instructions while you were working:\n{}\n\
         Take them into account before continuing.]",
        texts.join("\n---\n")
    )
}

/// rig hook that performs the injection at tool-result boundaries.
pub struct SteerHook {
    queue: SteerQueue,
}

impl SteerHook {
    pub fn new(queue: SteerQueue) -> Self {
        Self { queue }
    }
}

impl<M: CompletionModel> AgentHook<M> for SteerHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        let StepEvent::ToolResult { result, .. } = event else {
            return Flow::Continue;
        };
        if self.queue.is_empty() || is_multimodal_output(result) {
            return Flow::Continue;
        }
        let texts = self.queue.drain();
        Flow::RewriteResult {
            result: format!("{result}{}", injection_note(&texts)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_drains_in_order() {
        let q = SteerQueue::new();
        assert!(q.is_empty());
        q.push("first".into());
        q.push("second".into());
        assert!(!q.is_empty());
        assert_eq!(q.drain(), vec!["first".to_string(), "second".to_string()]);
        assert!(q.is_empty());
        assert!(q.drain().is_empty());
    }

    #[test]
    fn multimodal_outputs_are_recognized() {
        assert!(is_multimodal_output(
            r#"{"response":"note","parts":[{"type":"image","data":"aa","mimeType":"image/png"}]}"#
        ));
        assert!(!is_multimodal_output("Edited foo.rs: -1 +1 lines"));
        assert!(!is_multimodal_output(r#"{"ok":true}"#));
    }
}
