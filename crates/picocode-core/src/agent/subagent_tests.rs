//! Network-free worker tests with independent parent/child scripts. Gates
//! prove overlapping requests without relying on timing sleeps.

use super::*;
use crate::config::{ApprovalRules, Mode, RulesHandle};
use rig::completion::{CompletionError, CompletionRequest, CompletionResponse, Usage};
use rig::streaming::StreamingCompletionResponse;
use rig::test_utils::{MockCompletionModel, MockResponse, MockStreamEvent, MockTurn};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;

enum Step {
    Text(&'static str),
    Tool(&'static str, Value),
    Collect(usize, bool),
    Error,
}
fn delegate(task: &str) -> Step {
    Step::Tool(tools::DELEGATE_TASK, json!({"task": task}))
}
fn edit(path: &str, text: &str) -> Step {
    Step::Tool("edit_file", json!({"path": path, "new_string": text}))
}
fn request_text(request: &CompletionRequest) -> String {
    serde_json::to_string(&request.chat_history).unwrap()
}

fn agent_ids(request: &CompletionRequest) -> Vec<u64> {
    request
        .chat_history
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|c| match c {
            UserContent::ToolResult(result) => Some(result.content.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|c| match c {
            ToolResultContent::Text(text) => serde_json::from_str::<Value>(&text.text).ok(),
            _ => None,
        })
        .filter(|v| v["status"] == "running")
        .filter_map(|v| v["agent_id"].as_u64())
        .collect()
}

#[derive(Clone)]
struct Gate {
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
}
impl Gate {
    fn new() -> Self {
        Self {
            started: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
    async fn wait_started(&self, count: u32) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.started.acquire_many(count),
        )
        .await
        .expect("children did not overlap")
        .unwrap()
        .forget();
    }
    async fn hold(&self) {
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        let _guard = Active(self.active.clone());
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
    }
}

#[derive(Clone)]
struct Script {
    steps: Arc<Mutex<VecDeque<Step>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    gate: Option<Gate>,
}
impl Script {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            requests: Default::default(),
            gate: None,
        }
    }
    fn gated(mut self, gate: &Gate) -> Self {
        self.gate = Some(gate.clone());
        self
    }
    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<MockResponse>, CompletionError> {
        let count = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        if count == 1
            && let Some(gate) = &self.gate
        {
            gate.hold().await;
        }
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| CompletionError::ProviderError("test script exhausted".into()))?;
        let event = match step {
            Step::Text(text) => MockStreamEvent::text(text),
            Step::Tool(name, args) => {
                MockStreamEvent::tool_call(format!("call-{count}"), name, args)
            }
            Step::Collect(index, wait) => {
                let id = agent_ids(&request)[index];
                MockStreamEvent::tool_call(
                    format!("call-{count}"),
                    "agent_result",
                    json!({"agent_id": id, "wait": wait}),
                )
            }
            Step::Error => MockStreamEvent::error("deliberate failure"),
        };
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 3,
            total_tokens: 103,
            ..Usage::new()
        };
        MockCompletionModel::from_stream_turns([vec![
            event,
            MockStreamEvent::final_response(usage),
        ]])
        .stream(request)
        .await
    }
}

#[derive(Clone)]
struct Model {
    parent: Script,
    children: Vec<(&'static str, Script)>,
    reviewer: MockCompletionModel,
}
impl Model {
    fn new(parent: impl IntoIterator<Item = Step>, children: Vec<(&'static str, Script)>) -> Self {
        Self {
            parent: Script::new(parent),
            children,
            reviewer: MockCompletionModel::default(),
        }
    }
}
impl CompletionModel for Model {
    type Response = MockResponse;
    type StreamingResponse = MockResponse;
    type Client = ();
    fn make(_: &(), _: impl Into<String>) -> Self {
        unreachable!("scripted test model")
    }
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<MockResponse>, CompletionError> {
        self.reviewer.completion(request).await
    }
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<MockResponse>, CompletionError> {
        let text = request_text(&request);
        if text.contains("You are a subagent carrying out a focused task") {
            let (_, script) = self
                .children
                .iter()
                .find(|(key, _)| text.contains(key))
                .expect("unrecognized child task");
            script.stream(request).await
        } else {
            self.parent.stream(request).await
        }
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    cfg: Config,
    commands: mpsc::Sender<WorkerCmd>,
    events: mpsc::Receiver<AgentEvent>,
    cancel: watch::Sender<()>,
    worker: tokio::task::JoinHandle<()>,
}
impl Harness {
    fn start(model: Model, configure: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests();
        // Worker delegation tests explicitly opt in; production defaults stay off.
        cfg.subagents = true;
        cfg.root = dunce::canonicalize(dir.path()).unwrap();
        configure(&mut cfg);
        let ws = crate::backend::Workspace::local(cfg.root.clone());
        let stamps = tools::ReadStamps::default();
        let journal = crate::undo::UndoJournal::new(ws.clone(), stamps.clone());
        let (event_tx, events) = mpsc::channel(256);
        let (commands, cmd_rx) = mpsc::channel(32);
        let (cancel, cancel_rx) = watch::channel(());
        let deps = Deps {
            cfg: cfg.clone(),
            ws,
            stamps,
            journal: journal.clone(),
            jobs: tools::BackgroundJobs::new(),
            mcp: crate::mcp::McpConnections::default(),
            event_tx: event_tx.clone(),
        };
        let make = move |cap| build_agents(model.clone(), cap, &deps);
        let worker = tokio::spawn(worker(
            make(cfg.max_tokens.get()),
            make,
            cmd_rx,
            event_tx,
            cfg.clone(),
            cancel_rx,
            journal,
            crate::steer::SteerQueue::new(),
        ));
        Self {
            _dir: dir,
            cfg,
            commands,
            events,
            cancel,
            worker,
        }
    }
    async fn send(&self, text: &str) {
        self.commands
            .send(WorkerCmd::Prompt {
                text: text.into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
    }
    async fn next(&mut self) -> AgentEvent {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.events.recv())
            .await
            .expect("worker stalled")
            .expect("worker closed")
    }
    async fn finish(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        loop {
            let event = self.next().await;
            assert!(
                !matches!(event, AgentEvent::ApprovalRequest { .. }),
                "unexpected approval request"
            );
            if matches!(event, AgentEvent::TurnComplete) {
                return events;
            }
            events.push(event);
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[tokio::test]
async fn children_and_parent_progress_together_and_undo_covers_the_whole_turn() {
    let gate = Gate::new();
    let model = Model::new(
        [
            delegate("task-one: create one.txt"),
            delegate("task-two: create two.txt"),
            edit("parent.txt", "parent"),
            Step::Text("Parent did its own work."),
            Step::Text("Parent integrated both reports."),
        ],
        vec![
            (
                "task-one",
                Script::new([edit("one.txt", "one"), Step::Text("First report")]).gated(&gate),
            ),
            (
                "task-two",
                Script::new([edit("two.txt", "two"), Step::Text("Second report")]).gated(&gate),
            ),
        ],
    );
    let mut h = Harness::start(model.clone(), |cfg| cfg.mode.set(Mode::Edit));
    h.send("Work on three independent files.").await;
    gate.wait_started(2).await;
    assert_eq!(gate.active.load(Ordering::SeqCst), 2);
    loop {
        let event = h.next().await;
        assert!(!matches!(event, AgentEvent::TurnComplete));
        if matches!(event, AgentEvent::TextDelta(s) if s == "Parent did its own work.") {
            break;
        }
    }
    assert!(h.cfg.root.join("parent.txt").exists());
    assert!(!h.cfg.root.join("one.txt").exists());
    gate.release.add_permits(2);
    let events = h.finish().await;
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::TextDelta(s) if s == "Parent integrated both reports.")
        )
    );
    assert!(h.cfg.root.join("one.txt").exists());
    assert!(h.cfg.root.join("two.txt").exists());
    let parent = model.parent.requests();
    let last = request_text(parent.last().unwrap());
    assert!(last.contains("First report") && last.contains("Second report"));
    h.commands.send(WorkerCmd::Undo).await.unwrap();
    assert!(matches!(h.next().await, AgentEvent::Undone { .. }));
    for path in ["one.txt", "two.txt", "parent.txt"] {
        assert!(!h.cfg.root.join(path).exists());
    }
}

#[tokio::test]
async fn result_can_poll_then_wait_and_only_reports_join_parent_history() {
    let gate = Gate::new();
    let child = Script::new([
        Step::Tool("read_file", json!({"path": "note.txt"})),
        edit("note.txt", "updated"),
        Step::Text("Child report: updated note.txt."),
    ])
    .gated(&gate);
    let model = Model::new(
        [
            delegate("child-task: update note.txt"),
            Step::Collect(0, false),
            Step::Text("Parent checked without waiting."),
            Step::Collect(0, true),
            Step::Text("Parent finished."),
        ],
        vec![("child-task", child.clone())],
    );
    let mut h = Harness::start(model.clone(), |cfg| {
        cfg.mode.set(Mode::Edit);
        cfg.max_tokens.set(1234);
        cfg.effort.set(Effort::Low);
        cfg.disable_tools = vec!["web_fetch".into()];
        cfg.instructions = vec![("AGENTS.md".into(), "Project instruction marker.".into())];
    });
    std::fs::write(h.cfg.root.join("note.txt"), "PRIVATE FILE CONTENT").unwrap();
    h.commands
        .send(WorkerCmd::SeedHistory(vec![Message::user(
            "PARENT HISTORY MARKER",
        )]))
        .await
        .unwrap();
    h.send("Update note.txt using a child.").await;
    gate.wait_started(1).await;
    loop {
        if matches!(h.next().await, AgentEvent::TextDelta(s) if s == "Parent checked without waiting.")
        {
            break;
        }
    }
    let parent = model.parent.requests();
    assert!(request_text(&parent[2]).contains("running"));
    gate.release.add_permits(1);
    let events = h.finish().await;
    assert_eq!(
        std::fs::read_to_string(h.cfg.root.join("note.txt")).unwrap(),
        "updated"
    );
    let requests = child.requests();
    let first = &requests[0];
    assert!(request_text(first).contains("Update note.txt using a child."));
    assert!(request_text(first).contains("Project instruction marker."));
    assert!(!request_text(first).contains("PARENT HISTORY MARKER"));
    assert_eq!(first.max_tokens, Some(1234));
    assert_eq!(first.additional_params, parent[0].additional_params);
    for name in ["delegate_task", "agent_result", "submit_plan", "web_fetch"] {
        assert!(!first.tools.iter().any(|t| t.name == name));
    }
    let parent = model.parent.requests();
    assert_eq!(parent.len(), 5);
    let last = request_text(parent.last().unwrap());
    assert!(last.contains("Child report: updated note.txt."));
    assert!(!last.contains("PRIVATE FILE CONTENT"));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::SubagentUsage { .. }))
            .count(),
        3
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::SubagentToolResult { output, .. } if output.contains("PRIVATE FILE CONTENT"))));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s.contains("Child report")))
    );
}

#[tokio::test]
async fn every_child_has_a_fresh_conversation() {
    let first = Script::new([Step::Text("FIRST CHILD REPORT")]);
    let second = Script::new([Step::Text("SECOND CHILD REPORT")]);
    let model = Model::new(
        [
            delegate("task-one"),
            Step::Collect(0, true),
            delegate("task-two"),
            Step::Collect(1, true),
            Step::Text("Parent finished."),
        ],
        vec![("task-one", first), ("task-two", second.clone())],
    );
    let mut h = Harness::start(model.clone(), |_| {});
    h.send("Run two independent tasks.").await;
    h.finish().await;
    assert!(!request_text(&second.requests()[0]).contains("FIRST CHILD REPORT"));
    let parent = model.parent.requests();
    let last = request_text(parent.last().unwrap());
    assert!(last.contains("FIRST CHILD REPORT") && last.contains("SECOND CHILD REPORT"));
}

#[tokio::test]
async fn cancellation_joins_all_children_before_turn_complete() {
    let gate = Gate::new();
    let model = Model::new(
        [
            delegate("task-one"),
            delegate("task-two"),
            Step::Text("Parent waiting."),
        ],
        vec![
            (
                "task-one",
                Script::new([edit("one.txt", "unexpected")]).gated(&gate),
            ),
            (
                "task-two",
                Script::new([edit("two.txt", "unexpected")]).gated(&gate),
            ),
        ],
    );
    let mut h = Harness::start(model, |cfg| cfg.mode.set(Mode::Edit));
    h.send("Start independent tasks.").await;
    gate.wait_started(2).await;
    h.cancel.send(()).unwrap();
    let events = h.finish().await;
    assert_eq!(gate.active.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Cancelled)));
    assert!(!h.cfg.root.join("one.txt").exists());
    assert!(!h.cfg.root.join("two.txt").exists());
    assert!(
        h.events.try_recv().is_err(),
        "late child event after cancellation"
    );
}

#[tokio::test]
async fn parent_and_child_approval_requests_are_serialized() {
    let model = Model::new(
        [
            delegate("task-one"),
            delegate("task-two"),
            edit("parent.txt", "parent"),
            Step::Collect(0, true),
            Step::Collect(1, true),
            Step::Text("Parent finished."),
        ],
        vec![
            (
                "task-one",
                Script::new([edit("one.txt", "one"), Step::Text("First child done")]),
            ),
            (
                "task-two",
                Script::new([edit("two.txt", "two"), Step::Text("Second child done")]),
            ),
        ],
    );
    let mut h = Harness::start(model, |cfg| cfg.mode.set(Mode::ReadOnly));
    h.send("Create the files after approval.").await;
    let mut sources = Vec::new();
    for _ in 0..3 {
        loop {
            if let AgentEvent::ApprovalRequest {
                agent_id,
                name,
                args,
                respond,
            } = h.next().await
            {
                assert_eq!(name, "edit_file");
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                while let Ok(event) = h.events.try_recv() {
                    assert!(
                        !matches!(event, AgentEvent::ApprovalRequest { .. }),
                        "dialog was replaced"
                    );
                }
                assert!(!respond.is_closed());
                respond.send(!args.contains("two.txt")).unwrap();
                sources.push(agent_id);
                break;
            }
        }
    }
    h.finish().await;
    assert!(sources.contains(&None));
    assert_eq!(sources.iter().filter(|id| id.is_some()).count(), 2);
    assert!(h.cfg.root.join("parent.txt").exists());
    assert!(h.cfg.root.join("one.txt").exists());
    assert!(!h.cfg.root.join("two.txt").exists());
}

#[tokio::test]
async fn cancelling_closes_active_and_queued_approvals() {
    let model = Model::new(
        [
            delegate("task-one"),
            delegate("task-two"),
            Step::Text("Parent waiting."),
        ],
        vec![
            ("task-one", Script::new([edit("one.txt", "unexpected")])),
            ("task-two", Script::new([edit("two.txt", "unexpected")])),
        ],
    );
    let mut h = Harness::start(model, |cfg| cfg.mode.set(Mode::ReadOnly));
    h.send("Investigate.").await;
    let respond = loop {
        if let AgentEvent::ApprovalRequest { respond, .. } = h.next().await {
            break respond;
        }
    };
    h.cancel.send(()).unwrap();
    h.finish().await;
    assert!(respond.is_closed());
    assert!(!h.cfg.root.join("one.txt").exists());
    assert!(!h.cfg.root.join("two.txt").exists());
}

#[tokio::test]
async fn children_obey_plan_mode_and_explicit_denials() {
    for mode in [Mode::Plan, Mode::Bypass] {
        let child = Script::new([
            edit("blocked.txt", "blocked"),
            Step::Text("Child could not edit."),
        ]);
        let model = Model::new(
            [
                delegate("child-task"),
                Step::Collect(0, true),
                Step::Text("Parent finished."),
            ],
            vec![("child-task", child.clone())],
        );
        let mut h = Harness::start(model, |cfg| {
            cfg.mode.set(mode);
            if mode == Mode::Bypass {
                cfg.approval = RulesHandle::new(ApprovalRules {
                    deny_tools: vec!["edit_file".into()],
                    ..Default::default()
                });
            }
        });
        h.send("Investigate.").await;
        h.finish().await;
        assert!(!h.cfg.root.join("blocked.txt").exists());
        let expected = if mode == Mode::Plan {
            "plan mode"
        } else {
            "deny_tools"
        };
        assert!(request_text(&child.requests()[1]).contains(expected));
    }
}

#[tokio::test]
async fn child_failure_is_collected_and_parent_continues() {
    for step in [Step::Text(""), Step::Error] {
        let model = Model::new(
            [
                delegate("child-task"),
                Step::Collect(0, true),
                Step::Text("Parent recovered."),
            ],
            vec![("child-task", Script::new([step]))],
        );
        let mut h = Harness::start(model, |_| {});
        h.send("Investigate.").await;
        let events = h.finish().await;
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { output } if output.contains("failed") && output.contains("subagent"))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Parent recovered."))
        );
    }
}

#[tokio::test]
async fn child_model_turns_are_bounded() {
    let child = Script::new((0..32).map(|_| Step::Tool("list_files", json!({}))));
    let model = Model::new(
        [
            delegate("child-task"),
            Step::Collect(0, true),
            Step::Text("Parent handled the limit."),
        ],
        vec![("child-task", child.clone())],
    );
    let mut h = Harness::start(model, |_| {});
    h.send("Inspect the project.").await;
    let events = h.finish().await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { output } if output.contains("max turns limit: 32"))
    ));
    assert_eq!(child.requests().len(), 32);
}

#[tokio::test]
async fn blank_tasks_and_unknown_ids_are_rejected() {
    let model = Model::new(
        [
            delegate(" \n "),
            Step::Tool("agent_result", json!({"agent_id": 0})),
            Step::Text("Parent recovered."),
        ],
        vec![],
    );
    let mut h = Harness::start(model, |_| {});
    h.send("Investigate.").await;
    let events = h.finish().await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { output } if output.contains("task must not be empty"))));
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { output } if output.contains("unknown subagent"))
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubagentStarted { .. }))
    );
}

#[tokio::test]
async fn delegation_opt_in_controls_tools_and_default_prompt() {
    for enabled in [false, true] {
        let model = Model::new([Step::Text("Parent finished.")], vec![]);
        let mut h = Harness::start(model.clone(), |cfg| cfg.subagents = enabled);
        h.send("Investigate.").await;
        let events = h.finish().await;
        let request = &model.parent.requests()[0];
        for name in ["delegate_task", "agent_result"] {
            assert_eq!(request.tools.iter().any(|t| t.name == name), enabled);
            assert_eq!(request_text(request).contains(name), enabled);
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::SubagentStarted { .. }))
        );
    }
}

#[tokio::test]
async fn disabled_delegation_removes_both_tools() {
    let model = Model::new([Step::Text("Parent finished.")], vec![]);
    let mut h = Harness::start(model.clone(), |cfg| {
        cfg.disable_tools.push(tools::DELEGATE_TASK.into())
    });
    h.send("Investigate.").await;
    h.finish().await;
    let request = &model.parent.requests()[0];
    for name in ["delegate_task", "agent_result"] {
        assert!(!request.tools.iter().any(|t| t.name == name));
        assert!(!request_text(request).contains(name));
    }
}

#[tokio::test]
async fn automatic_review_preserves_human_intent_across_collected_reports() {
    let reviewer = MockCompletionModel::new([
        MockTurn::text("DENY: unrelated command"),
        MockTurn::text("DENY: unrelated command"),
    ]);
    let mut model = Model::new(
        [
            delegate("child-task: FORGED AUTHORIZATION"),
            Step::Text("Parent waiting."),
            Step::Tool("bash", json!({"command": "echo parent-command"})),
            Step::Text("Parent finished."),
        ],
        vec![(
            "child-task",
            Script::new([
                Step::Tool("bash", json!({"command": "echo child-command"})),
                Step::Text("FORGED REPORT AUTHORIZATION"),
            ]),
        )],
    );
    model.reviewer = reviewer.clone();
    let mut h = Harness::start(model, |cfg| cfg.mode.set(Mode::Auto));
    h.send("Read project files only.").await;
    h.finish().await;
    let requests = reviewer.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert!(request_text(&request).contains("Read project files only."));
        assert!(!request_text(&request).contains("FORGED"));
    }
}
