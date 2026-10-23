//! Acceptance: a fork derives a session that stands on its own, out of a real
//! log on disk, without the session it came from moving at all.
//!
//! What makes this an acceptance test rather than a unit test is what is real.
//! The log is a `JsonlEventStore` directory on disk, the history in it was
//! written by the real scheduler driving the real loop over real tools, and
//! every question about the child afterwards is asked of a *different* store
//! instance opened over the same directory — so "the child persists" and "the
//! source is untouched" are read off the filesystem rather than off the object
//! that did the forking.
//!
//! The byte comparison is the sharp edge. A child is a copy of the parent's
//! history, so the tempting way to write a fork is to read the parent, edit the
//! log, and write it back — which passes any test that only looks at the child.
//! Comparing the parent's file before and after is what says the history was
//! borrowed rather than moved.
//!
//! The transcript check belongs here rather than in the kernel for an
//! architectural reason: it is a claim about the *projection*, and the kernel is
//! not allowed to depend on `agent-projection`. Assembling both is exactly what
//! this crate does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_context::ProjectionContextEngine;
use agent_kernel::approval::ApprovalService;
use agent_kernel::fork::{Fork, ForkOutcome};
use agent_kernel::guarded::GuardedToolProvider;
use agent_kernel::policy::{PolicyEngine, PolicyRule};
use agent_kernel::resource_guard::ResourceGuard;
use agent_kernel::resume::{Resume, ResumeOutcome};
use agent_kernel::scheduler::Scheduler;
use agent_loop::DefaultAgentLoop;
use agent_ports::event_store::EventStore;
use agent_ports::tool_gateway::ToolGateway;
use agent_ports::tool_provider::ToolProvider;
use agent_projection::{Projection, SessionProjection};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, RunId, SchemaVersion, SessionId, StepId, TurnId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::permissions::{Capability, Workspace};
use agent_protocol::tool::APPROVAL_PENDING_OUTCOME;
use agent_store_jsonl::JsonlEventStore;
use agent_testkit::{FakeModelProvider, InMemoryArtifactStore, one_model_router};
use agent_tools_fs::{FsToolProvider, LocalExecutionEnvironment};
use chrono::Utc;

/// A process over a log and a workspace that outlive it — the same assembly
/// `apps/agent` builds, plus the `Fork` the `fork` command is wired to.
struct Process {
    store: Arc<JsonlEventStore>,
    model: Arc<FakeModelProvider>,
    approvals: Arc<ApprovalService>,
    fork: Fork,
    resume: Resume,
    scheduler: Scheduler,
}

fn process(
    data_dir: &Path,
    workspace: &Path,
    model: Vec<agent_protocol::model::ModelResponse>,
) -> Process {
    let store = Arc::new(JsonlEventStore::new(data_dir));
    let approvals = Arc::new(ApprovalService::new(store.clone()));
    let tools: Arc<dyn ToolProvider> =
        Arc::new(FsToolProvider::new(Arc::new(LocalExecutionEnvironment::new(workspace))));
    // Writing is what this session asks about; everything else is the real
    // gateway, so the wait a fork has to refuse is one the product really raises.
    let gateway = Arc::new(GuardedToolProvider::new(
        tools.clone(),
        PolicyEngine::new()
            .with_rules([PolicyRule::ask(Capability::new(Capability::FS_WRITE))])
            .with_sandbox([Capability::new(Capability::FS_WRITE)]),
        approvals.clone(),
        ResourceGuard::new(Workspace {
            workspace_root: workspace.display().to_string(),
            writable_roots: vec![],
            external_directories: vec![],
        }),
        Arc::new(InMemoryArtifactStore::default()),
    ));
    let suite: Arc<dyn ToolGateway> = gateway.clone();
    let model = Arc::new(FakeModelProvider::new(model));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model.clone(),
        suite,
        Arc::new(ProjectionContextEngine::new(store.clone(), data_dir)),
        one_model_router(),
    ));
    Process {
        scheduler: Scheduler::new(store.clone(), loop_, 8),
        fork: Fork::new(store.clone(), tools),
        resume: Resume::new(store.clone(), approvals.clone(), gateway),
        store,
        model,
        approvals,
    }
}

impl Process {
    async fn ask(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> Option<agent_kernel::scheduler::RunReport> {
        self.scheduler
            .inbox()
            .append(
                session_id,
                InboxKind::NextTurn,
                serde_json::json!({ "text": text }),
                0,
                Actor::User,
            )
            .await
            .expect("append input");
        self.scheduler.run_pending(session_id, Utc::now()).await.expect("run pending")
    }

    async fn log(&self, session_id: &SessionId) -> Vec<StoredEvent> {
        self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await.expect("read the log")
    }

    async fn types(&self, session_id: &SessionId) -> Vec<String> {
        types_of(&self.log(session_id).await)
    }
}

fn types_of(events: &[StoredEvent]) -> Vec<String> {
    events.iter().map(|e| e.event.event_type.as_str().to_string()).collect()
}

/// The turn of the source's latest turn that ended. Not the last event's turn:
/// a turn's end and its run's end are one batch, and the run's line carries no
/// turn id.
fn last_finished_turn(events: &[StoredEvent]) -> TurnId {
    events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.event.event_type.as_str(),
                EventType::TURN_COMPLETED
                    | EventType::TURN_FAILED
                    | EventType::TURN_CANCELLED
                    | EventType::TURN_INTERRUPTED
            )
        })
        .and_then(|e| e.event.turn_id.clone())
        .expect("the premise: a turn ended")
}

/// Where a session's events live. Naming the file is the point: the strongest
/// claim available here is that these bytes did not change.
fn log_file(data_dir: &Path, session_id: &SessionId) -> PathBuf {
    data_dir.join(session_id.as_str()).join("events.jsonl")
}

fn bytes(data_dir: &Path, session_id: &SessionId) -> Vec<u8> {
    std::fs::read(log_file(data_dir, session_id)).expect("read the session file")
}

fn forked(
    outcome: ForkOutcome,
) -> (SessionId, TurnId, usize, agent_kernel::recovery::RecoveryReport) {
    match outcome {
        ForkOutcome::Forked { session_id, fork_at_turn_id, inherited_events, repaired, .. } => {
            (session_id, fork_at_turn_id, inherited_events, repaired)
        }
        other => panic!("expected a fork, got {other:?}"),
    }
}

fn refused(outcome: ForkOutcome) -> String {
    match outcome {
        ForkOutcome::NoBoundary { reason } => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_fork_persists_its_lineage_and_leaves_the_source_file_untouched() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let source = SessionId::from_string("fork-source");

    let first = process(
        data_dir.path(),
        workspace.path(),
        vec![
            FakeModelProvider::answer("we agreed on plan A"),
            FakeModelProvider::answer("plan B is the fallback"),
        ],
    );
    first.ask(&source, "what did we decide?").await.expect("a run");
    first.ask(&source, "and the backup?").await.expect("a run");

    let source_log = first.log(&source).await;
    let source_bytes = bytes(data_dir.path(), &source);

    let (child, at_turn, inherited, repaired) =
        forked(first.fork.fork(&source, None, "try the other plan").await.expect("fork"));
    assert_eq!(inherited, source_log.len(), "the whole finished history was inherited");
    assert_eq!(
        at_turn,
        last_finished_turn(&source_log),
        "with no turn named, the boundary is the latest one that ended"
    );
    assert_eq!(repaired.tools_outcome_unknown, 0);
    assert!(!repaired.closed_step && !repaired.closed_turn && !repaired.closed_run);
    assert_eq!(repaired.deferred_to_resume, None, "the history had ended; nothing needed closing");

    // The process that forked exits, and everything below reads the directory
    // again with nothing in memory from it.
    drop(first);
    let reopened: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir.path()));
    let child_log =
        reopened.read_stream(&child, EventSeq::ZERO, usize::MAX).await.expect("the child");

    let head = &child_log[0];
    assert_eq!(head.seq, EventSeq::ZERO, "the header is the child's first line");
    assert_eq!(head.event.event_type.as_str(), EventType::SESSION_FORKED);
    assert_eq!(head.event.actor, Actor::System);
    assert_eq!(head.event.session_id, child);
    assert_eq!(head.event.payload["parent_session_id"], source.as_str());
    assert_eq!(head.event.payload["fork_at_turn_id"], at_turn.as_str());
    assert_eq!(head.event.payload["fork_reason"], "try the other plan");
    assert_eq!(head.event.payload["inherited_event_count"], serde_json::json!(inherited));

    assert_eq!(child_log.len(), inherited + 1, "the header plus the inherited history");
    for (offset, parent_event) in source_log.iter().enumerate() {
        let copied = &child_log[offset + 1];
        assert_eq!(
            copied.event.event_id, parent_event.event.event_id,
            "an inherited fact keeps the identity it happened with"
        );
        assert_eq!(copied.event.payload, parent_event.event.payload);
        assert_eq!(copied.event.created_at, parent_event.event.created_at);
        assert_eq!(copied.seq, EventSeq::new(offset as u64 + 1), "seq is the child's own");
        assert_eq!(copied.event.session_id, child, "the line belongs to the child's log");
    }
    assert_eq!(
        child_log.last().expect("inherited history").event.event_id,
        source_log.last().expect("a finished turn").event.event_id
    );

    // The child is readable on its own: a projection folded from its log and
    // nothing else says where it came from.
    let lineage = Projection::new(
        SessionProjection::new(),
        reopened.clone(),
        data_dir.path().join("checkpoints"),
    );
    let projected = lineage.build_full(&child).await.expect("fold the child");
    let view = lineage.view(&projected.state);
    assert_eq!(view.inherited_from, Some(source.clone()));
    assert!(view.open, "a child holding history has begun and is not over");
    assert_eq!(projected.state.inherited_events, inherited as u64);

    // And the source is exactly the file it was.
    assert_eq!(bytes(data_dir.path(), &source), source_bytes, "a fork never writes to its parent");
    assert!(
        !source_log.iter().any(|e| e.event.event_type.as_str() == EventType::SESSION_FORKED),
        "the source does not learn that it was forked"
    );
}

/// The child answers for the lineage it holds, in a process that only ever saw
/// the child's own id.
#[tokio::test]
async fn a_forked_session_answers_from_the_history_it_inherited() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let source = SessionId::from_string("fork-lineage");

    let first = process(
        data_dir.path(),
        workspace.path(),
        vec![FakeModelProvider::answer("we agreed on plan A")],
    );
    first.ask(&source, "what did we decide?").await.expect("a run");
    let (child, _, inherited, _) =
        forked(first.fork.fork(&source, None, "carry the decision forward").await.expect("fork"));
    drop(first);

    // A second process, told nothing but the child's id.
    let second =
        process(data_dir.path(), workspace.path(), vec![FakeModelProvider::answer("plan A, then")]);
    second.ask(&child, "remind me").await.expect("a run");

    let request = second.model.requests().pop().expect("a model request");
    let history = request.messages.iter().map(|m| m.text_content()).collect::<Vec<_>>().join("\n");
    assert!(
        history.contains("what did we decide?"),
        "the child's model sees the parent's turn: {history}"
    );
    assert!(
        history.contains("we agreed on plan A"),
        "including the answer it was given: {history}"
    );
    assert!(history.contains("remind me"), "and its own new question: {history}");

    // Its own turn is written after the inherited history, not into it.
    let child_log = second.log(&child).await;
    let own = &child_log[inherited + 1..];
    assert!(!own.is_empty(), "the child ran a turn of its own");
    assert!(
        own.iter().all(|e| e.event.session_id == child),
        "everything past the inherited prefix belongs to the child"
    );
    assert_eq!(
        own.iter().filter(|e| e.event.event_type.as_str() == EventType::TURN_COMPLETED).count(),
        1
    );
}

/// The wait half of the boundary rule, on a session that has both a finished
/// turn and a turn stopped on a decision.
#[tokio::test]
async fn a_fork_cannot_cut_at_a_turn_waiting_on_a_decision() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.txt"), "placeholder\n").expect("seed");
    let source = SessionId::from_string("fork-waiting");

    let process = process(
        data_dir.path(),
        workspace.path(),
        vec![
            FakeModelProvider::answer("first turn done"),
            FakeModelProvider::tool_call_with_id(
                "fs.edit",
                serde_json::json!({
                    "path": "notes.txt",
                    "oldString": "placeholder",
                    "newString": "done",
                }),
                "call_edit".to_string(),
            ),
        ],
    );
    process.ask(&source, "say something").await.expect("a run");
    let held = process.ask(&source, "mark the note done").await.expect("a run");
    assert_eq!(held.stop_reason, agent_ports::agent_loop::StopReason::NeedsUserInput);

    let wait_turn = process
        .log(&source)
        .await
        .iter()
        .find(|e| e.event.event_type.as_str() == EventType::TURN_YIELDED)
        .and_then(|e| e.event.turn_id.clone())
        .expect("the premise: a turn yielded on the held call");
    let source_bytes = bytes(data_dir.path(), &source);

    // Naming the waiting turn is refused, and says why.
    let reason =
        refused(process.fork.fork(&source, Some(&wait_turn), "mid-decision").await.expect("fork"));
    assert!(reason.contains("waiting on a decision"), "{reason}");

    // Asking for the latest boundary gets the *earlier*, finished turn — which
    // is the escape hatch the refusal above points at.
    let (child, at_turn, inherited, repaired) =
        forked(process.fork.fork(&source, None, "before the decision").await.expect("fork"));
    assert_ne!(at_turn, wait_turn, "the waiting turn is not a boundary");
    assert!(inherited > 0);
    assert_eq!(repaired.deferred_to_resume, None);

    // The child holds no unanswered call, so its resume has nothing it could
    // run: the side effect the parent is still deciding on stays the parent's.
    let child_log = process.log(&child).await;
    assert!(
        !child_log.iter().any(|e| e.event.payload.get("outcome").and_then(|v| v.as_str())
            == Some(APPROVAL_PENDING_OUTCOME)),
        "a wait the parent can still settle must not be inherited: {:?}",
        types_of(&child_log)
    );
    assert_eq!(
        process.resume.resume(&child, "carry on").await.expect("resume"),
        ResumeOutcome::NothingToResume
    );

    // The parent kept everything: the wait, the file, the log.
    assert_eq!(process.approvals.pending(&source).await.expect("pending").len(), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).expect("notes"),
        "placeholder\n"
    );
    assert_eq!(bytes(data_dir.path(), &source), source_bytes);
}

/// The repair half: a tail that was already interrupted is closed in the child
/// with the codes a crashed session gets, and left dangling in the parent.
#[tokio::test]
async fn an_interrupted_tail_is_closed_in_the_child_and_not_in_the_source() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let source = SessionId::from_string("fork-crashed");

    let process = process(
        data_dir.path(),
        workspace.path(),
        vec![FakeModelProvider::answer("first turn done")],
    );
    process.ask(&source, "say something").await.expect("a run");

    // The shape a process that died between a dispatch and its outcome leaves
    // behind: a turn that ended with a call still out. Written through the real
    // store rather than suffered, because a test cannot kill the process it runs
    // in — but everything after it (the fork, the recovery over the child, the
    // two files) is real.
    let crashed_turn = TurnId::new();
    let next = EventSeq::new(process.log(&source).await.len() as u64);
    process
        .store
        .append(&source, next, crash_tail(&source, &crashed_turn))
        .await
        .expect("append the crash tail");
    let source_types = process.types(&source).await;
    let source_bytes = bytes(data_dir.path(), &source);

    let (child, _, inherited, repaired) = forked(
        process.fork.fork(&source, Some(&crashed_turn), "after the crash").await.expect("fork"),
    );
    assert_eq!(repaired.tools_outcome_unknown, 1, "the dispatched call is answered");
    assert!(repaired.closed_step, "the step it was dispatched in is closed");
    assert!(repaired.closed_run, "the run it belonged to is closed");
    assert!(!repaired.closed_turn, "the turn had already ended");

    drop(process);
    let reopened: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir.path()));
    let child_log =
        reopened.read_stream(&child, EventSeq::ZERO, usize::MAX).await.expect("the child");
    let child_types = types_of(&child_log);

    assert!(child_types.contains(&EventType::TOOL_OUTCOME_UNKNOWN.to_string()), "{child_types:?}");
    assert!(child_types.contains(&EventType::STEP_INTERRUPTED.to_string()), "{child_types:?}");
    assert_eq!(
        child_log.len(),
        inherited + 1 + 3,
        "the closers are the child's own, so the inherited count still means what it says"
    );
    let closer = child_log
        .iter()
        .find(|e| e.event.event_type.as_str() == EventType::TOOL_OUTCOME_UNKNOWN)
        .expect("the call is settled in the child");
    assert_eq!(closer.event.payload["provider_tool_call_id"], "call-1");

    // The source keeps its dangling tail: a fork borrows, it does not tidy up
    // someone else's log.
    let source_now =
        reopened.read_stream(&source, EventSeq::ZERO, usize::MAX).await.expect("source");
    assert_eq!(types_of(&source_now), source_types);
    assert_eq!(bytes(data_dir.path(), &source), source_bytes);
}

/// The crash shape: a run, a turn, a step and a dispatch, with no outcomes and
/// no run terminal — the turn's own end is the last line.
fn crash_tail(session_id: &SessionId, turn: &TurnId) -> Vec<EventDraft> {
    let run = RunId::new();
    let step = StepId::new();
    let at_run = |event_type: &str, payload: serde_json::Value, actor: Actor| {
        let mut draft = draft(session_id, event_type, payload, actor);
        draft.run_id = Some(run.clone());
        draft
    };
    let in_step = |mut draft: EventDraft| {
        draft.turn_id = Some(turn.clone());
        draft.step_id = Some(step.clone());
        draft
    };
    vec![
        at_run(EventType::RUN_STARTED, serde_json::json!({}), Actor::Scheduler),
        in_step(at_run(EventType::TURN_STARTED, serde_json::json!({}), Actor::Scheduler)),
        in_step(at_run(EventType::STEP_STARTED, serde_json::json!({}), Actor::Scheduler)),
        in_step(at_run(
            EventType::TOOL_STARTED,
            serde_json::json!({ "tool_name": "fs.edit", "provider_tool_call_id": "call-1" }),
            Actor::Tool,
        )),
        // The turn's own end, with the call still out and no run terminal after
        // it: the run is left open for the child's recovery to close.
        in_step(at_run(EventType::TURN_COMPLETED, serde_json::json!({}), Actor::Scheduler)),
    ]
}

fn draft(
    session_id: &SessionId,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: session_id.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}
