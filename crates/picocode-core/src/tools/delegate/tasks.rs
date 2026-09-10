//! Turn-owned child tasks. Tools borrow the registry through runtime
//! extensions; the worker owns its lifetime and joins children before the
//! turn's undo frame can be closed or another turn can start.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::approval::ApprovalGate;
use crate::event::AgentEvent;
use crate::tools::ToolError;

pub(super) const MAX_RUNNING: usize = 4;
const MAX_TASKS: usize = 32;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Runtime-only context, supplied by the worker rather than tool arguments.
#[derive(Clone)]
pub(crate) struct ParentRequest {
    pub intent: String,
    pub approvals: ApprovalGate,
    pub(super) tasks: Tasks,
}

pub(crate) struct SubagentScope {
    pub request: ParentRequest,
}

impl SubagentScope {
    pub fn new(intent: String) -> Self {
        Self {
            request: ParentRequest {
                intent,
                approvals: ApprovalGate::default(),
                tasks: Tasks::default(),
            },
        }
    }

    pub async fn collect_pending(&self) -> Vec<AgentReport> {
        let ids: Vec<_> = self
            .request
            .tasks
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|task| !task.collected)
            .map(|task| task.id)
            .collect();
        let mut reports = Vec::new();
        for id in ids {
            if let Ok(report) = self.request.tasks.result(id, true).await {
                reports.push(report);
            }
        }
        reports
    }

    /// Abort first, then join: no child may still be emitting events or
    /// editing files when the worker reports cancellation to the UI.
    pub async fn stop(&self) {
        self.request.tasks.abort();
        self.join().await;
    }

    pub async fn join(&self) {
        let handles: Vec<_> = self
            .request
            .tasks
            .0
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|task| task.handle.take())
            .collect();
        for handle in handles {
            let _ = handle.await;
        }
    }
}

impl Drop for SubagentScope {
    fn drop(&mut self) {
        // Also covers worker replacement, workspace changes and app shutdown.
        self.request.tasks.abort();
    }
}

#[derive(Serialize)]
pub(crate) struct AgentReport {
    agent_id: u64,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct Task {
    id: u64,
    result: watch::Receiver<Option<Result<String, String>>>,
    handle: Option<JoinHandle<()>>,
    collected: bool,
}

#[derive(Clone, Default)]
pub(super) struct Tasks(Arc<Mutex<Vec<Task>>>);

impl Tasks {
    pub fn spawn<F>(
        &self,
        tx: mpsc::Sender<AgentEvent>,
        run: impl FnOnce(u64) -> F + Send + 'static,
    ) -> Result<u64, ToolError>
    where
        F: Future<Output = Result<String, ToolError>> + Send + 'static,
    {
        let mut tasks = self.0.lock().unwrap();
        if tasks.len() >= MAX_TASKS {
            return Err(ToolError::new(
                "subagent limit reached: 32 tasks per parent turn",
            ));
        }
        if tasks.iter().filter(|t| t.result.borrow().is_none()).count() >= MAX_RUNNING {
            return Err(ToolError::new(format!(
                "{MAX_RUNNING} subagents are already running; use agent_result to collect one before starting another"
            )));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (result_tx, result) = watch::channel(None);
        let handle = tokio::spawn(async move {
            let outcome = std::panic::AssertUnwindSafe(async {
                tx.send(AgentEvent::SubagentStarted { id })
                    .await
                    .map_err(|_| ToolError::new("UI channel closed"))?;
                run(id).await
            })
            .catch_unwind()
            .await;
            let outcome = match outcome {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(_) => Err("subagent panicked".into()),
            };
            let _ = tx
                .send(AgentEvent::SubagentFinished {
                    id,
                    success: outcome.is_ok(),
                })
                .await;
            result_tx.send_replace(Some(outcome));
        });
        tasks.push(Task {
            id,
            result,
            handle: Some(handle),
            collected: false,
        });
        Ok(id)
    }

    pub async fn result(&self, id: u64, wait: bool) -> Result<AgentReport, ToolError> {
        let mut receiver = self
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|task| task.id == id)
            .map(|task| task.result.clone())
            .ok_or_else(|| ToolError::new(format!("unknown subagent {id} in this turn")))?;
        let outcome = loop {
            let outcome = receiver.borrow().clone();
            if outcome.is_some() {
                break outcome;
            }
            if !wait {
                break None;
            }
            if receiver.changed().await.is_err() {
                break Some(Err("subagent was cancelled".into()));
            }
        };
        if outcome.is_some()
            && let Some(task) = self.0.lock().unwrap().iter_mut().find(|task| task.id == id)
        {
            task.collected = true;
        }
        Ok(match outcome {
            None => AgentReport {
                agent_id: id,
                status: "running",
                output: None,
                error: None,
            },
            Some(Ok(output)) => AgentReport {
                agent_id: id,
                status: "completed",
                output: Some(output),
                error: None,
            },
            Some(Err(error)) => AgentReport {
                agent_id: id,
                status: "failed",
                output: None,
                error: Some(error),
            },
        })
    }

    fn abort(&self) {
        for task in self.0.lock().unwrap().iter() {
            if let Some(handle) = &task.handle {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn running_limit_reopens_after_completion_and_turn_limit_stays_bounded() {
        let scope = SubagentScope::new("test".into());
        let tasks = &scope.request.tasks;
        let (tx, _events) = mpsc::channel(128);
        let mut held = Vec::new();
        for _ in 0..MAX_RUNNING {
            let (release, wait) = oneshot::channel();
            let id = tasks
                .spawn(tx.clone(), |_| async {
                    wait.await.unwrap();
                    Ok("report".into())
                })
                .unwrap();
            held.push((id, release));
        }
        assert!(
            tasks
                .spawn(tx.clone(), |_| async { Ok("unexpected".into()) })
                .unwrap_err()
                .0
                .contains("already running")
        );
        let (id, release) = held.pop().unwrap();
        assert_eq!(tasks.result(id, false).await.unwrap().status, "running");
        release.send(()).unwrap();
        assert_eq!(tasks.result(id, true).await.unwrap().status, "completed");
        for _ in MAX_RUNNING..MAX_TASKS {
            let id = tasks
                .spawn(tx.clone(), |_| async { Ok("report".into()) })
                .unwrap();
            assert_eq!(tasks.result(id, true).await.unwrap().status, "completed");
        }
        assert!(
            tasks
                .spawn(tx, |_| async { Ok("unexpected".into()) })
                .unwrap_err()
                .0
                .contains("32 tasks per parent turn")
        );
        scope.stop().await;
        for (id, release) in held {
            assert!(release.is_closed(), "child still running after stop");
            assert_eq!(tasks.result(id, true).await.unwrap().status, "failed");
        }
    }

    #[tokio::test]
    async fn dropping_scope_aborts_children_even_when_tools_retain_the_registry() {
        let scope = SubagentScope::new("test".into());
        let context = scope.request.clone();
        let (tx, _events) = mpsc::channel(4);
        let (mut release, wait) = oneshot::channel::<()>();
        let id = context
            .tasks
            .spawn(tx, |_| async {
                let _ = wait.await;
                Ok("unexpected".into())
            })
            .unwrap();
        drop(scope);
        tokio::time::timeout(std::time::Duration::from_secs(5), release.closed())
            .await
            .unwrap();
        assert_eq!(
            context.tasks.result(id, true).await.unwrap().status,
            "failed"
        );
    }

    #[tokio::test]
    async fn results_belong_to_their_parent_turn_and_panics_are_reported() {
        let scope = SubagentScope::new("one".into());
        let other = SubagentScope::new("two".into());
        let (tx, _events) = mpsc::channel(4);
        let id = scope
            .request
            .tasks
            .spawn(tx, |_| async { panic!("test child panic") })
            .unwrap();
        assert!(other.request.tasks.result(id, false).await.is_err());
        let report = scope.request.tasks.result(id, true).await.unwrap();
        assert_eq!(report.status, "failed");
        assert_eq!(report.error.as_deref(), Some("subagent panicked"));
        assert!(scope.collect_pending().await.is_empty());
        scope.join().await;
    }
}
