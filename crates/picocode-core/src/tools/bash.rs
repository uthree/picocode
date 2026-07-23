use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::{ToolError, truncate_output};
use crate::config::NumHandle;
use crate::event::AgentEvent;
use crate::sandbox::SandboxCtx;

const MAX_OUTPUT_BYTES: usize = 20_000;

/// Ids for backgrounded (timed-out) commands, unique across the process.
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// One live background job in the registry.
struct Job {
    command: String,
    started: Instant,
    /// Aborting drops the reader task, whose child has `kill_on_drop` —
    /// the process dies with it.
    abort: tokio::task::AbortHandle,
}

/// Registry of running background jobs, shared between the bash tool (which
/// registers timed-out commands) and the front ends (`/jobs` list / kill).
/// One per app, so jobs survive worker respawns on model switches.
#[derive(Clone, Default)]
pub struct BackgroundJobs(Arc<Mutex<HashMap<u64, Job>>>);

impl BackgroundJobs {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, id: u64, command: String, abort: tokio::task::AbortHandle) {
        self.0.lock().unwrap().insert(
            id,
            Job {
                command,
                started: Instant::now(),
                abort,
            },
        );
    }

    fn remove(&self, id: u64) {
        self.0.lock().unwrap().remove(&id);
    }

    /// Running jobs as (id, command, elapsed), oldest first.
    pub fn list(&self) -> Vec<(u64, String, Duration)> {
        let mut jobs: Vec<_> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(id, j)| (*id, j.command.clone(), j.started.elapsed()))
            .collect();
        jobs.sort_by_key(|(id, ..)| *id);
        jobs
    }

    /// Kill job `id`: aborting the reader task drops the child process
    /// (`kill_on_drop`), and the job's wrapper reports a `BackgroundDone`
    /// with a "killed" note. Returns whether the job existed.
    pub fn kill(&self, id: u64) -> bool {
        match self.0.lock().unwrap().remove(&id) {
            Some(job) => {
                job.abort.abort();
                true
            }
            None => false,
        }
    }
}

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
}

pub struct Bash {
    /// Backend + root: commands run on the workspace's host (local or the
    /// remote SSH connection) inside its root.
    ws: crate::backend::Workspace,
    /// Timeout in seconds, shared with the `/config` dialog.
    timeout: NumHandle,
    /// Where backgrounded commands report their completion.
    notify: mpsc::Sender<AgentEvent>,
    /// Registry the timed-out commands are tracked in (`/jobs`).
    jobs: BackgroundJobs,
    /// Sandbox settings + live mode (local only); `SandboxCtx::off()` for
    /// user-typed `!` commands.
    sandbox: SandboxCtx,
}

impl Bash {
    pub fn new(
        ws: crate::backend::Workspace,
        timeout: NumHandle,
        notify: mpsc::Sender<AgentEvent>,
        jobs: BackgroundJobs,
        sandbox: SandboxCtx,
    ) -> Self {
        Self {
            ws,
            timeout,
            notify,
            jobs,
            sandbox,
        }
    }
}

/// Aborts the command task on drop unless disarmed. Dropping the tool future
/// (Esc, or the stream being cancelled) thereby kills the child process via
/// `kill_on_drop`; a timeout disarms the guard first so the command survives
/// as a background job.
struct AbortOnDrop(Option<tokio::task::AbortHandle>);

impl AbortOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

impl Tool for Bash {
    const NAME: &'static str = "bash";
    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    fn description(&self) -> String {
        let shell = if cfg!(windows) { "cmd.exe" } else { "sh" };
        format!(
            "Run a shell command ({shell}) in the working directory and return its \
             output. Use for builds, tests, git, and anything the other tools \
             don't cover. A command still running after {}s is moved to the \
             background and its output is added to the conversation when it \
             finishes.",
            self.timeout.get()
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let timeout = Duration::from_secs(self.timeout.get().max(1));
        let mut child = self
            .ws
            .backend
            .shell(&args.command, &self.ws.root, &self.sandbox)
            .map_err(|e| ToolError::new(format!("command setup failed: {e:#}")))?
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::new(format!("failed to spawn command: {e}")))?;

        // Read/wait in a task of its own so a timeout can leave the command
        // running in the background instead of dropping (and killing) it.
        let (mut o, mut e) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let mut task = tokio::spawn(async move {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            use tokio::io::AsyncReadExt;
            let (r1, r2, status) = tokio::join!(
                o.read_to_end(&mut stdout),
                e.read_to_end(&mut stderr),
                child.wait()
            );
            r1.and(r2)
                .map_err(|e| format!("failed to read output: {e}"))?;
            let status = status.map_err(|e| format!("wait failed: {e}"))?;
            Ok::<_, String>(format_output(&stdout, &stderr, status))
        });
        let mut guard = AbortOnDrop(Some(task.abort_handle()));

        match tokio::time::timeout(timeout, &mut task).await {
            Ok(joined) => {
                guard.disarm();
                match joined {
                    Ok(Ok(text)) => Ok(text),
                    Ok(Err(msg)) => Err(ToolError::new(msg)),
                    Err(e) => Err(ToolError::new(format!("command task failed: {e}"))),
                }
            }
            Err(_) => {
                // Timed out: keep the command running as a background job and
                // report its output through the event channel when it's done.
                guard.disarm();
                let id = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
                let command = args.command.clone();
                let notify = self.notify.clone();
                let jobs = self.jobs.clone();
                jobs.insert(id, command.clone(), task.abort_handle());
                // Status-bar job counter; the matching decrement rides on
                // `BackgroundDone` below.
                let _ = notify
                    .send(AgentEvent::BackgroundStarted {
                        id,
                        command: command.clone(),
                    })
                    .await;
                tokio::spawn(async move {
                    let output = match task.await {
                        Ok(Ok(text)) => text,
                        Ok(Err(msg)) => format!("error: {msg}"),
                        // Aborted = killed via `/jobs kill` (the registry
                        // entry is already gone).
                        Err(e) if e.is_cancelled() => "(killed by the user)".to_string(),
                        Err(e) => format!("error: command task failed: {e}"),
                    };
                    jobs.remove(id);
                    let _ = notify
                        .send(AgentEvent::BackgroundDone {
                            id,
                            command,
                            output,
                        })
                        .await;
                });
                Ok(format!(
                    "Still running after {}s — moved to background as job #{id}. \
                     Its output will be added to the conversation when it finishes; \
                     don't re-run the command, continue with other work or tell the \
                     user you are waiting for it.",
                    timeout.as_secs()
                ))
            }
        }
    }
}

/// Merge stdout/stderr, truncate, and append a non-zero exit code.
fn format_output(stdout: &[u8], stderr: &[u8], status: std::process::ExitStatus) -> String {
    let mut text = String::new();
    if !stdout.is_empty() {
        text.push_str(&String::from_utf8_lossy(stdout));
    }
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push_str("\n--- stderr ---\n");
        }
        text.push_str(&String::from_utf8_lossy(stderr));
    }
    let mut text = truncate_output(text.trim_end(), MAX_OUTPUT_BYTES);
    if text.is_empty() {
        text = "(no output)".to_string();
    }
    if !status.success() {
        text.push_str(&format!(
            "\n[exit code: {}]",
            status
                .code()
                .map_or("signal".to_string(), |c| c.to_string())
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(root: &std::path::Path, secs: u64) -> (Bash, mpsc::Receiver<AgentEvent>) {
        let (tx, rx) = mpsc::channel(8);
        (
            Bash::new(
                crate::backend::Workspace::local(root.to_path_buf()),
                NumHandle::new(secs),
                tx,
                BackgroundJobs::new(),
                SandboxCtx::off(),
            ),
            rx,
        )
    }

    #[tokio::test]
    async fn background_jobs_can_be_listed_and_killed() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let jobs = BackgroundJobs::new();
        let tool = Bash::new(
            crate::backend::Workspace::local(dir.path().to_path_buf()),
            NumHandle::new(1),
            tx,
            jobs.clone(),
            SandboxCtx::off(),
        );
        let out = tool
            .call(BashArgs {
                command: "sleep 30 && echo never".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("moved to background"), "{out}");
        let _ = rx.recv().await; // BackgroundStarted

        // Listed with its command…
        let listed = jobs.list();
        assert_eq!(listed.len(), 1);
        let (id, command, _) = &listed[0];
        assert_eq!(command, "sleep 30 && echo never");

        // …and killing reports a completion immediately (not after 30 s).
        assert!(jobs.kill(*id));
        assert!(!jobs.kill(*id)); // already gone
        let done = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("BackgroundDone within 5s")
            .expect("event");
        match done {
            AgentEvent::BackgroundDone { output, .. } => {
                assert!(output.contains("killed"), "{output}");
            }
            _ => panic!("expected BackgroundDone"),
        }
        assert!(jobs.list().is_empty());
    }

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _rx) = tool(dir.path(), 120);
        let out = tool
            .call(BashArgs {
                command: "echo hello && echo err >&2".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("hello"));
        assert!(out.contains("err"));
    }

    #[tokio::test]
    async fn reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _rx) = tool(dir.path(), 120);
        let out = tool
            .call(BashArgs {
                command: "exit 3".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("[exit code: 3]"));
    }

    #[tokio::test]
    async fn timed_out_command_backgrounds_and_reports_back() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, mut rx) = tool(dir.path(), 1);
        let out = tool
            .call(BashArgs {
                command: "sleep 2 && echo late".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("moved to background as job #"), "{out}");

        // Going to the background is announced (status-bar counter)…
        let ev = rx.recv().await.expect("background start event");
        assert!(matches!(ev, AgentEvent::BackgroundStarted { .. }));

        // …and the job keeps running and reports its output when done.
        let ev = rx.recv().await.expect("background completion event");
        match ev {
            AgentEvent::BackgroundDone {
                command, output, ..
            } => {
                assert_eq!(command, "sleep 2 && echo late");
                assert!(output.contains("late"), "{output}");
            }
            _ => panic!("expected BackgroundDone"),
        }
    }
}
