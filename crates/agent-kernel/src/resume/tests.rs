//! Tests for `resume`.
//!
//! Split out of the module when the approval work pushed that file past
//! its size ceiling; they are tests, and the module they test is unchanged.

use super::*;
use crate::approval::ApprovalAuthority;
use crate::policy::{PolicyEngine, PolicyRule};
use crate::recovery::Recovery;
use crate::resource_guard::ResourceGuard;
use crate::scheduler::{RunReport, Scheduler};
use agent_loop::DefaultAgentLoop;
use agent_ports::agent_loop::StopReason;
use agent_ports::event_store::{AppendReceipt, EventSubscription, FlushReceipt, SessionLease};
use agent_ports::tool_provider::{ToolDefinition, ToolProvider, ToolProviderError, ToolSideEffect};
use agent_protocol::model::{ContentBlock, FinishReason, ModelResponse};
use agent_protocol::permissions::{ApprovalScope, Capability, Workspace};
use agent_testkit::{
    FakeEventStore, FakeModelProvider, InMemoryArtifactStore, NullContextEngine, one_model_router,
};
use std::sync::Mutex;

/// A tool provider that records every invocation.
///
/// The point of the whole feature is that a side effect happens once, and
/// the only honest place to check that is where the effect happens — the
/// log's own account of what it did is the thing under suspicion.
struct CountingTools {
    defs: Vec<ToolDefinition>,
    calls: Mutex<Vec<(String, serde_json::Value)>>,
    /// Set to watch the log from inside a call. The one invariant that
    /// cannot be checked after the fact is *when* the dispatch reached the
    /// log — a run that appended it afterwards looks identical in a
    /// transcript read once the crash never happened — so the check has to
    /// happen here, while the call is in flight.
    observer: Mutex<Option<(Arc<dyn EventStore>, SessionId)>>,
    /// What the log held at each invocation: the payload of every
    /// `tool/started` on disk at that moment, in order.
    starts_seen: Mutex<Vec<Vec<serde_json::Value>>>,
    /// The write-operation log of a `FlushWatching` store, snapshotted from
    /// inside each invocation so a test can see what the *durability* state
    /// was at the moment the effect ran.
    ops_observer: Mutex<Option<Arc<Mutex<Vec<&'static str>>>>>,
    /// The operation log at each invocation, one entry per call.
    ops_seen: Mutex<Vec<Vec<&'static str>>>,
}

impl CountingTools {
    fn new() -> Self {
        Self {
            defs: ["deploy", "cleanup"]
                .into_iter()
                .map(|name| ToolDefinition {
                    name: name.to_string(),
                    description: format!("test tool {name}"),
                    source: agent_protocol::tool::ToolSource::Builtin,
                    side_effect: ToolSideEffect::Destructive,
                    capability_requirements: vec![Capability::new(Capability::PROCESS_SPAWN)],
                    idempotent: false,
                    timeout_ms: None,
                    input_schema: serde_json::json!({ "type": "object" }),
                })
                .collect(),
            calls: Mutex::new(Vec::new()),
            observer: Mutex::new(None),
            starts_seen: Mutex::new(Vec::new()),
            ops_observer: Mutex::new(None),
            ops_seen: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().unwrap().clone()
    }

    /// Snapshot the log from inside every later invocation.
    fn watch_log(&self, store: Arc<dyn EventStore>, session_id: SessionId) {
        *self.observer.lock().unwrap() = Some((store, session_id));
    }

    /// The dispatches on disk at each invocation, one entry per call.
    fn starts_seen_while_running(&self) -> Vec<Vec<serde_json::Value>> {
        self.starts_seen.lock().unwrap().clone()
    }

    /// Snapshot a store's write-operation log from inside every later
    /// invocation.
    fn watch_ops(&self, ops: Arc<Mutex<Vec<&'static str>>>) {
        *self.ops_observer.lock().unwrap() = Some(ops);
    }

    /// The write-operation log at each invocation, one entry per call.
    fn ops_seen_while_running(&self) -> Vec<Vec<&'static str>> {
        self.ops_seen.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ToolProvider for CountingTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.defs.clone()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        if !self.defs.iter().any(|d| d.name == invocation.tool_name) {
            return Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: self.defs.iter().map(|d| d.name.clone()).collect(),
            });
        }
        self.calls
            .lock()
            .unwrap()
            .push((invocation.tool_name.clone(), invocation.arguments.clone()));

        // What the log says at the moment the effect happens. Reading it
        // here rather than afterwards is the whole point: a run that wrote
        // the dispatch after the call would leave the same transcript.
        let watching = self.observer.lock().unwrap().clone();
        if let Some((store, session_id)) = watching {
            let events = store.read_stream(&session_id, EventSeq::ZERO, usize::MAX).await.unwrap();
            let starts = events
                .iter()
                .filter(|e| e.event.event_type.as_str() == EventType::TOOL_STARTED)
                .map(|e| e.event.payload.clone())
                .collect();
            self.starts_seen.lock().unwrap().push(starts);
        }

        // And what the *durability* state was, read the same way: a flush
        // that has not happened yet at this point is a dispatch a power cut
        // could still erase.
        let ops = self.ops_observer.lock().unwrap().clone();
        if let Some(ops) = ops {
            self.ops_seen.lock().unwrap().push(ops.lock().unwrap().clone());
        }

        Ok(ToolResult::completed("done"))
    }
}

/// A store decorator that records its own writes, so a test can ask what
/// the durability state was at the moment the approved call ran. It exists
/// because "the dispatch was appended" and "the dispatch was flushed" are
/// two different facts, and only the second survives a power cut — a store
/// that never flushes looks identical from every other angle.
struct FlushWatching {
    inner: Arc<dyn EventStore>,
    ops: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait::async_trait]
impl EventStore for FlushWatching {
    async fn append(
        &self,
        session_id: &SessionId,
        expected_next_seq: EventSeq,
        drafts: Vec<EventDraft>,
    ) -> Result<AppendReceipt, EventStoreError> {
        self.ops.lock().unwrap().push("append");
        self.inner.append(session_id, expected_next_seq, drafts).await
    }

    async fn lease(
        &self,
        session_id: &SessionId,
    ) -> Result<Box<dyn SessionLease>, EventStoreError> {
        self.inner.lease(session_id).await
    }

    async fn flush(&self, session_id: &SessionId) -> Result<FlushReceipt, EventStoreError> {
        self.ops.lock().unwrap().push("flush");
        self.inner.flush(session_id).await
    }

    async fn driver_lease(
        &self,
        session_id: &SessionId,
    ) -> Result<Box<dyn SessionLease>, EventStoreError> {
        self.inner.driver_lease(session_id).await
    }

    async fn read_stream(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, EventStoreError> {
        self.inner.read_stream(session_id, from_seq, limit).await
    }

    async fn subscribe(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
    ) -> Result<Box<dyn EventSubscription>, EventStoreError> {
        self.inner.subscribe(session_id, from_seq).await
    }
}

/// A session where every deployment needs approval, wired the way the app
/// wires it — so the log resume reads is the log the real loop writes.
struct Harness {
    /// The type is the trait, not `FakeEventStore`: one test supplies a
    /// store that records its own writes (see `FlushWatching`), and every
    /// other user of the store here goes through the trait anyway.
    store: Arc<dyn EventStore>,
    approvals: Arc<ApprovalService>,
    tools: Arc<CountingTools>,
    scheduler: Scheduler,
    resume: Resume,
    session_id: SessionId,
}

fn harness(model: Vec<ModelResponse>) -> Harness {
    harness_with(Arc::new(FakeEventStore::new()), model)
}

fn harness_with(store: Arc<dyn EventStore>, model: Vec<ModelResponse>) -> Harness {
    let approvals = Arc::new(ApprovalService::new(store.clone()));
    let tools = Arc::new(CountingTools::new());
    let gateway = Arc::new(GuardedToolProvider::new(
        tools.clone(),
        PolicyEngine::new()
            .with_rules([PolicyRule::ask(Capability::new(Capability::PROCESS_SPAWN))])
            .with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]),
        approvals.clone(),
        ResourceGuard::new(Workspace {
            workspace_root: std::env::current_dir().unwrap().display().to_string(),
            writable_roots: vec![],
            external_directories: vec![],
        }),
        Arc::new(InMemoryArtifactStore::default()),
    ));
    let suite: Arc<dyn agent_ports::tool_gateway::ToolGateway> = gateway.clone();
    // These tests are about the approval/resume plumbing, not about the
    // thinking pipeline: with it on, the first irreversible call would be
    // asked for a branch before it ever reaches the gateway, and the run
    // would need a model answer these fixtures do not have.
    let loop_ = Arc::new(
        DefaultAgentLoop::new(
            Arc::new(FakeModelProvider::new(model)),
            suite,
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        )
        .without_judgment(),
    );
    let session_id = SessionId::from_string("resume-session");
    Harness {
        scheduler: Scheduler::new(store.clone(), loop_, 8),
        resume: Resume::new(store.clone(), approvals.clone(), gateway),
        store,
        approvals,
        tools,
        session_id,
    }
}

impl Harness {
    /// Submit `text` and drive a turn, if one can be driven.
    async fn ask(&self, text: &str) -> Option<RunReport> {
        self.scheduler
            .inbox()
            .append(
                &self.session_id,
                InboxKind::NextTurn,
                serde_json::json!({ "text": text }),
                0,
                Actor::User,
            )
            .await
            .unwrap();
        self.scheduler.run_pending(&self.session_id, Utc::now()).await.unwrap()
    }

    async fn run(&self) -> Result<Option<RunReport>, crate::scheduler::SchedulerError> {
        self.scheduler.run_pending(&self.session_id, Utc::now()).await
    }

    async fn resume(&self, text: &str) -> ResumeOutcome {
        self.resume.resume(&self.session_id, text).await.unwrap()
    }

    async fn log(&self) -> Vec<StoredEvent> {
        self.store.read_stream(&self.session_id, EventSeq::ZERO, usize::MAX).await.unwrap()
    }

    async fn types(&self) -> Vec<String> {
        self.log().await.into_iter().map(|e| e.event.event_type.to_string()).collect()
    }

    async fn count(&self, event_type: &str) -> usize {
        self.types().await.into_iter().filter(|t| t == event_type).count()
    }

    /// The one approval this session is waiting on.
    async fn pending(&self) -> ApprovalId {
        let pending = self.approvals.pending(&self.session_id).await.unwrap();
        assert_eq!(pending.len(), 1, "the premise: one call is waiting");
        pending[0].approval_id.clone()
    }

    /// The wakeup `approve`/`deny` queues next to a decision, written the
    /// way the command does.
    async fn wakeup(&self, approval_id: &ApprovalId) {
        self.scheduler
            .inbox()
            .append(
                &self.session_id,
                InboxKind::Wakeup,
                serde_json::json!({
                    "approval_id": approval_id.as_str(),
                    "decision": "grant",
                    "scope": "allow_once",
                }),
                0,
                Actor::User,
            )
            .await
            .unwrap();
    }

    async fn pending_kinds(&self) -> Vec<InboxKind> {
        self.scheduler
            .inbox()
            .pending(&self.session_id)
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.kind)
            .collect()
    }

    async fn grant(&self, approval_id: &ApprovalId) {
        self.approvals
            .grant(
                &self.session_id,
                approval_id,
                ApprovalScope::AllowOnce,
                "user",
                &ApprovalAuthority::test(),
            )
            .await
            .unwrap();
    }

    /// A tool event's payload field, from the log.
    async fn payload_field(&self, event_type: &str, field: &str) -> Vec<serde_json::Value> {
        self.log()
            .await
            .into_iter()
            .filter(|e| e.event.event_type.as_str() == event_type)
            .filter_map(|e| e.event.payload.get(field).cloned())
            .collect()
    }
}

/// A model response proposing several calls in one step.
fn proposing(calls: &[(&str, &str, serde_json::Value)]) -> ModelResponse {
    ModelResponse {
        content_blocks: calls
            .iter()
            .map(|(id, name, args)| ContentBlock::tool_use(*id, *name, args.clone()))
            .collect(),
        usage: None,
        finish_reason: FinishReason::new(FinishReason::TOOL_CALLS),
        provider_metadata: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn a_session_with_nothing_waiting_has_nothing_to_resume() {
    let h = harness(vec![FakeModelProvider::answer("hello")]);
    h.ask("hi").await;

    assert_eq!(h.resume("anything").await, ResumeOutcome::NothingToResume);
    // And nothing about a finished turn looks like a wait.
    assert_eq!(h.count(EventType::TURN_RESUMED).await, 0);
}

#[tokio::test]
async fn a_pending_approval_writes_nothing_and_runs_nothing() {
    let h = harness(vec![FakeModelProvider::tool_call("deploy", serde_json::json!({}))]);
    let held = h.ask("deploy it").await.expect("a run");
    assert_eq!(held.stop_reason, StopReason::NeedsUserInput);
    let approval_id = h.pending().await;

    let before = h.types().await;
    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Waiting { approval_id, tool_name: "deploy".to_string() }
    );

    // Idempotent in the state that matters: asking twice, or asking while
    // the decision is out, changes nothing and runs nothing.
    assert_eq!(h.types().await, before);
    assert!(h.tools.calls().is_empty());
}

/// The claim, not the write lease, is what a resume can lose to.
///
/// A driver in another terminal holds the claim for its whole turn and
/// holds nothing in the write lease until it settles. A resume that only
/// waited on the write lease would block, then answer a wait that driver is
/// about to settle itself. This one is told no, before anything is read.
#[tokio::test]
async fn a_resume_is_refused_while_another_caller_drives_the_session() {
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("carried on"),
    ]);
    h.ask("deploy it").await.expect("a run");
    h.grant(&h.pending().await).await;

    let elsewhere = h.store.driver_lease(&h.session_id).await.expect("the claim");
    let before = h.types().await;

    let refused = h.resume.resume(&h.session_id, "").await;
    assert!(
        matches!(refused, Err(ResumeError::SessionBusy { .. })),
        "expected a busy refusal, got {:?}",
        refused.err().map(|error| error.to_string())
    );
    assert_eq!(h.types().await, before, "a refused resume writes nothing");
    assert!(h.tools.calls().is_empty(), "and runs nothing");

    drop(elsewhere);
    assert!(
        matches!(h.resume("").await, ResumeOutcome::Settled { .. }),
        "the claim is free again once the other driver lets go"
    );
}

#[tokio::test]
async fn a_new_turn_is_refused_while_a_call_is_unanswered() {
    // The refused reason is the same one resume exists to resolve, so this
    // test is the reason the guard is not merely a nicety: without it, the
    // held call would be left unanswered for good.
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("carried on"),
    ]);
    h.ask("deploy it").await.expect("a run");

    let refused = h.run().await;
    assert!(
        matches!(refused, Err(crate::scheduler::SchedulerError::AwaitingDecision { .. })),
        "a turn must not open on top of an unanswered call: {refused:?}"
    );
}

#[tokio::test]
async fn the_default_continuation_names_the_call_and_the_request() {
    // The continuation is the whole of what the next turn is told. A text
    // that only says "continue" names neither the call nor the request, and
    // a small model fills that gap by repeating the state layer.
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({ "target": "staging" })),
        FakeModelProvider::answer("carried on"),
    ]);
    h.ask("deploy the build to staging").await.expect("a run");
    h.grant(&h.pending().await).await;

    assert!(matches!(h.resume("").await, ResumeOutcome::Settled { .. }));

    let text = h
        .log()
        .await
        .into_iter()
        .rfind(|stored| {
            stored.event.event_type.as_str() == EventType::INBOX_APPENDED
                && stored.event.payload["payload"]["text"].is_string()
        })
        .and_then(|stored| stored.event.payload["payload"]["text"].as_str().map(str::to_string))
        .expect("a queued continuation");
    assert!(text.contains("deploy"), "the call is not named: {text}");
    assert!(text.contains("staging"), "the arguments are not named: {text}");
    assert!(text.contains("deploy the build to staging"), "the request is not named: {text}");
    assert!(!text.contains("continue the task"), "the old text is still in play: {text}");
}

#[tokio::test]
async fn a_granted_approval_runs_the_call_once_and_carries_the_turn_forward() {
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({ "target": "prod" })),
        FakeModelProvider::answer("carried on"),
    ]);
    let held = h.ask("deploy it").await.expect("a run");
    assert_eq!(held.stop_reason, StopReason::NeedsUserInput);
    assert!(h.tools.calls().is_empty(), "waiting is not running");

    let approval_id = h.pending().await;
    h.grant(&approval_id).await;

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::Ran { status: ToolStatus::Completed },
        }
    );

    // The call ran, once, with the arguments the model proposed — not the
    // ones resume might have invented, and not a second time.
    assert_eq!(
        h.tools.calls(),
        vec![("deploy".to_string(), serde_json::json!({ "target": "prod" }))]
    );
    assert_eq!(h.count(EventType::TOOL_COMPLETED).await, 1);
    assert_eq!(h.count(EventType::TURN_RESUMED).await, 1);
    assert_eq!(h.count(EventType::TOOL_NOT_STARTED).await, 0);
    assert!(h.approvals.pending(&h.session_id).await.unwrap().is_empty());

    // The wait is over, so the continuation turn runs — with the settled
    // result in the transcript it reads.
    let carried = h.run().await.unwrap().expect("the continuation runs");
    assert_eq!(carried.stop_reason, StopReason::FinalAnswer);
    assert_eq!(carried.final_content.as_deref(), Some("carried on"));
    assert_eq!(h.tools.calls().len(), 1, "and the call still ran exactly once");
}

#[tokio::test]
async fn a_denied_approval_records_the_refusal_and_never_runs_the_call() {
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("understood"),
    ]);
    let held = h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.approvals.deny(&h.session_id, &approval_id, "user").await.unwrap();

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::Denied { status: ApprovalStatus::Denied },
        }
    );

    assert!(h.tools.calls().is_empty(), "a refusal runs nothing");
    // The call still gets an answer, or the history has a proposed call
    // that nothing replies to.
    let recorded = h.payload_field(EventType::TOOL_FAILED, "content").await;
    assert!(
        recorded.iter().any(|c| c.as_str().is_some_and(|c| c.contains("denied"))),
        "the refusal is recorded as the model will read it: {recorded:?}"
    );
    // And the turn carries on, so the refusal is not a dead end.
    assert_eq!(h.run().await.unwrap().expect("a run").stop_reason, StopReason::FinalAnswer);
}

#[tokio::test]
async fn a_question_that_timed_out_settles_as_a_call_that_never_ran() {
    // `sweep_expired` writes this event when a request has stood unanswered
    // past `PENDING_TTL`. A day is not something a test can wait for, so it
    // writes the same event the sweep does and checks the half that belongs
    // to `resume`.
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("understood"),
    ]);
    let held = h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    seed(&*h.store, &h.session_id, vec![expired_draft(&h.session_id, &approval_id)]).await;

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::Denied { status: ApprovalStatus::Expired },
        }
    );

    assert!(h.tools.calls().is_empty(), "a question that timed out is not a quiet yes");
    let recorded = h.payload_field(EventType::TOOL_FAILED, "content").await;
    assert!(
        recorded.iter().any(|c| c.as_str().is_some_and(|c| c.contains("expired"))),
        "the model is told which way the wait ended: {recorded:?}"
    );
    // And the turn carries on, so a timeout is not a dead end either.
    assert_eq!(h.run().await.unwrap().expect("a run").stop_reason, StopReason::FinalAnswer);
}

#[tokio::test]
async fn a_stopped_session_settles_as_a_call_that_never_ran() {
    // The other way a request is taken back without being answered: the
    // person asks the run to stop. `run_pending` is what turns that into a
    // cancellation, because on a held session nothing else is reading the
    // interrupt.
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("understood"),
    ]);
    let held = h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.scheduler
        .inbox()
        .append(&h.session_id, InboxKind::Interrupt, serde_json::json!({}), 0, Actor::User)
        .await
        .unwrap();

    assert!(
        matches!(h.run().await, Err(crate::scheduler::SchedulerError::AwaitingDecision { .. })),
        "the turn is still not carried forward until `resume` runs"
    );
    assert_eq!(
        h.approvals.status(&h.session_id, &approval_id).await.unwrap(),
        ApprovalStatus::Cancelled,
    );

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::Denied { status: ApprovalStatus::Cancelled },
        }
    );
    assert!(h.tools.calls().is_empty(), "a call the person stopped does not run");
}

#[tokio::test]
async fn calls_the_turn_never_reached_are_recorded_as_never_started() {
    // The loop stops at the first call that needs a decision, so the second
    // one was proposed and never dispatched. It needs an answer too: a
    // history with a proposed call nothing replies to is not one a provider
    // will accept back.
    let h = harness(vec![
        proposing(&[
            ("call_a", "deploy", serde_json::json!({})),
            ("call_b", "cleanup", serde_json::json!({})),
        ]),
        FakeModelProvider::answer("carried on"),
    ]);
    h.ask("do both").await.expect("a run");
    assert_eq!(h.count(EventType::TOOL_STARTED).await, 1, "the loop stopped at the first call");

    h.grant(&h.pending().await).await;
    h.resume("carry on").await;

    assert_eq!(
        h.payload_field(EventType::TOOL_NOT_STARTED, "tool_name").await,
        vec![serde_json::json!("cleanup")]
    );
    assert_eq!(
        h.payload_field(EventType::TOOL_NOT_STARTED, "provider_tool_call_id").await,
        vec![serde_json::json!("call_b")]
    );
    assert_eq!(
        h.tools.calls().iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
        vec!["deploy"],
        "the call that was never reached did not run"
    );
}

#[tokio::test]
async fn a_call_settled_by_an_earlier_resume_is_not_run_a_second_time() {
    // The state a crash in the middle of resume leaves: the call has its
    // outcome, the turn was never marked resumed. Retrying the resume must
    // finish the job without running anything again — the log cannot say
    // whether that recorded outcome came from a call that finished, and
    // guessing is how side effects get duplicated.
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({ "target": "prod" })),
        FakeModelProvider::answer("carried on"),
    ]);
    let held = h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.grant(&approval_id).await;

    // What the first resume wrote before it died: the settlement, without
    // the carrying-forward.
    let events = h.log().await;
    let call = classify(&events, &held.turn_id).expect("a held call");
    let settlement = vec![
        tool_draft(
            &h.session_id,
            &call,
            EventType::TOOL_STARTED,
            serde_json::json!({
                "tool_name": call.tool_name,
                "provider_tool_call_id": call.tool_call_id,
                "arguments": serde_json::json!({ "target": "prod" }),
            }),
        ),
        tool_draft(
            &h.session_id,
            &call,
            EventType::TOOL_COMPLETED,
            serde_json::json!({
                "tool_name": call.tool_name,
                "provider_tool_call_id": call.tool_call_id,
                "status": ToolStatus::Completed,
                "content": "done",
            }),
        ),
    ];
    let store: Arc<dyn EventStore> = h.store.clone();
    let next = next_seq(&store, &h.session_id).await.unwrap();
    h.store.append(&h.session_id, next, settlement).await.unwrap();

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::AlreadySettled,
        }
    );
    assert!(h.tools.calls().is_empty(), "nothing ran again");
    assert_eq!(h.count(EventType::TOOL_COMPLETED).await, 1);
    assert_eq!(h.count(EventType::TURN_RESUMED).await, 1, "and the turn is carried forward");
    assert_eq!(h.run().await.unwrap().expect("a run").stop_reason, StopReason::FinalAnswer);
}

#[tokio::test]
async fn the_dispatch_reaches_the_log_before_the_approved_call_runs() {
    // The property the feature rests on, checked at the one moment it can
    // be. A side effect that ran before its dispatch was durable is one a
    // crash can repeat; reading the log afterwards cannot tell that apart
    // from a correct run, because the correct run leaves the same
    // transcript. Only reading it from inside the call can.
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({ "target": "prod" })),
        FakeModelProvider::answer("carried on"),
    ]);
    let held = h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    let store: Arc<dyn EventStore> = h.store.clone();
    h.tools.watch_log(store, h.session_id.clone());
    h.grant(&approval_id).await;

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::Ran { status: ToolStatus::Completed },
        }
    );

    let seen = h.tools.starts_seen_while_running();
    assert_eq!(seen.len(), 1, "the approved call ran once");
    let dispatches = &seen[0];
    assert_eq!(
        dispatches.len(),
        2,
        "the model's proposal and the resume's dispatch are both on disk before the call runs"
    );
    assert_eq!(
        dispatches[1].get("resumed").and_then(|v| v.as_bool()),
        Some(true),
        "the second one is the resume's dispatch, not a second copy of the proposal"
    );
}

#[tokio::test]
async fn the_dispatch_is_flushed_before_the_approved_call_runs() {
    // The ordering test above is about the dispatch reaching the *log*;
    // this one is about it reaching the *disk*. An appended line sits in
    // the page cache and is lost to a power cut, and this is the only place
    // in the product where a real side effect follows an append — so it is
    // the one place a lost append means a repeated real effect. That window
    // is what C4.3 closes; flushing before the call is what makes the close
    // survive the machine, not just the process (C4.4). Read from inside
    // the call, because a transcript read afterwards cannot tell.
    let ops: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let store =
        Arc::new(FlushWatching { inner: Arc::new(FakeEventStore::new()), ops: ops.clone() });
    let h = harness_with(
        store,
        vec![
            FakeModelProvider::tool_call("deploy", serde_json::json!({ "target": "prod" })),
            FakeModelProvider::answer("carried on"),
        ],
    );
    let held = h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.tools.watch_ops(ops);
    h.grant(&approval_id).await;

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::Ran { status: ToolStatus::Completed },
        }
    );

    let seen = h.tools.ops_seen_while_running();
    assert_eq!(seen.len(), 1, "the approved call ran once");
    let at_call = &seen[0];
    assert!(
        at_call.contains(&"append"),
        "premise: writes happened before the call, got {at_call:?}"
    );
    assert_eq!(
        at_call.last(),
        Some(&"flush"),
        "the dispatch must be flushed before the call runs, got {at_call:?}"
    );
}

#[tokio::test]
async fn a_dispatch_that_outlived_its_caller_is_not_run_a_second_time() {
    // The crash the ordering makes survivable: the dispatch reached the log,
    // the call had not run, and the process died. On disk that is a
    // `tool/started` with no outcome — which is the state `Recovery` exists
    // to close, and whose marker is what stops the older `approval_pending`
    // from reading as a wait that is still open.
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({ "target": "prod" })),
        FakeModelProvider::answer("carried on"),
    ]);
    let held = h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.grant(&approval_id).await;

    // What the resume wrote before it died: the dispatch, alone.
    let events = h.log().await;
    let call = classify(&events, &held.turn_id).expect("a held call");
    let wait = call.wait.as_ref().expect("the call is still unanswered");
    let store: Arc<dyn EventStore> = h.store.clone();
    let next = next_seq(&store, &h.session_id).await.unwrap();
    h.store
        .append(&h.session_id, next, vec![dispatch_draft(&h.session_id, &call, wait)])
        .await
        .unwrap();

    let report = Recovery::new(h.store.clone(), h.tools.clone())
        .recover(&h.session_id)
        .await
        .expect("recovery");
    assert_eq!(report.tools_outcome_unknown, 1, "the dangling dispatch is closed");
    assert_eq!(h.count(EventType::TOOL_OUTCOME_UNKNOWN).await, 1);

    assert_eq!(
        h.resume("carry on").await,
        ResumeOutcome::Settled {
            turn_id: held.turn_id.clone(),
            tool_name: "deploy".to_string(),
            disposition: Disposition::AlreadySettled,
        }
    );
    assert!(h.tools.calls().is_empty(), "the call the dispatch named is not run again");
    assert_eq!(h.count(EventType::TOOL_COMPLETED).await, 0);
}

#[tokio::test]
async fn a_wait_with_no_call_to_settle_is_reported_and_left_alone() {
    // A yield this build cannot explain — no call, no ids. Fail closed: the
    // report says so and the log is untouched, rather than inventing a
    // settlement for a turn whose terms are unknown.
    let h = harness(vec![FakeModelProvider::answer("unused")]);
    let turn = TurnId::new();
    let step = StepId::new();
    seed(
        h.store.as_ref(),
        &h.session_id,
        vec![
            draft(
                &h.session_id,
                EventType::TURN_STARTED,
                serde_json::json!({}),
                Actor::Scheduler,
                None,
                Some(&turn),
                None,
            ),
            draft(
                &h.session_id,
                EventType::STEP_STARTED,
                serde_json::json!({}),
                Actor::Scheduler,
                None,
                Some(&turn),
                Some(&step),
            ),
            draft(
                &h.session_id,
                EventType::TURN_YIELDED,
                serde_json::json!({}),
                Actor::Scheduler,
                None,
                Some(&turn),
                None,
            ),
        ],
    )
    .await;
    let before = h.types().await;

    match h.resume("carry on").await {
        ResumeOutcome::Unresumable { turn_id, reason } => {
            assert_eq!(turn_id, turn);
            assert!(!reason.is_empty());
        }
        other => panic!("expected an unresumable report, got {other:?}"),
    }
    assert_eq!(h.types().await, before, "nothing was written");
}

/// The wakeup a decision was delivered with is spent once the decision has
/// been carried out.
///
/// `approve` writes the grant *and* queues a wakeup so the session gets
/// picked back up. Nothing selects a wakeup — it is a signal, not work — so
/// without being spent it stays pending for the life of the session, and
/// `status` reports an item no run will ever take.
#[tokio::test]
async fn a_resume_spends_the_wakeup_that_delivered_the_decision() {
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("carried on"),
    ]);
    h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.grant(&approval_id).await;
    h.wakeup(&approval_id).await;
    assert!(
        h.pending_kinds().await.contains(&InboxKind::Wakeup),
        "the premise: the decision was delivered as a wakeup"
    );

    assert!(matches!(h.resume("carry on").await, ResumeOutcome::Settled { .. }));

    let left = h.pending_kinds().await;
    assert!(
        !left.contains(&InboxKind::Wakeup),
        "the wakeup was acted on, so it is spent: {left:?}"
    );
    assert_eq!(h.count(EventType::INBOX_DISCARDED).await, 1);
    let spent = h
        .log()
        .await
        .into_iter()
        .find(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
        .expect("the spent wakeup is discarded");
    assert_eq!(spent.event.payload["cause"], serde_json::json!("spent"));
    // The continuation is still queued: spending the signal must not
    // consume the work it pointed at.
    assert_eq!(left, vec![InboxKind::NextTurn]);
}

/// A wakeup naming a decision that has not been made yet is left alone.
///
/// The rule is "spent once acted on", not "spent whenever a resume
/// happens" — and a session can hold a decision that this turn never asked
/// for.
#[tokio::test]
async fn a_wakeup_for_an_undecided_approval_is_left_pending() {
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("carried on"),
    ]);
    h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.grant(&approval_id).await;
    h.wakeup(&approval_id).await;

    // A second question, still open, with a wakeup that arrived early.
    let other = h
        .approvals
        .request(&h.session_id, Capability::new(Capability::PROCESS_SPAWN), "deploy", "elsewhere")
        .await
        .unwrap();
    h.wakeup(&other.approval_id).await;

    assert!(matches!(h.resume("carry on").await, ResumeOutcome::Settled { .. }));

    let left = h.pending_kinds().await;
    assert_eq!(
        left.iter().filter(|kind| **kind == InboxKind::Wakeup).count(),
        1,
        "the undecided one stays; only the spent one went: {left:?}"
    );
    let still_pending = h.approvals.pending(&h.session_id).await.unwrap();
    assert_eq!(still_pending[0].approval_id, other.approval_id);
}

/// A wakeup with no readable approval id cannot be matched to a decision,
/// so it is discarded with a reason rather than left pending for the life
/// of the session.
#[tokio::test]
async fn a_wakeup_without_an_approval_id_is_discarded_with_a_reason() {
    let h = harness(vec![
        FakeModelProvider::tool_call("deploy", serde_json::json!({})),
        FakeModelProvider::answer("carried on"),
    ]);
    h.ask("deploy it").await.expect("a run");
    let approval_id = h.pending().await;
    h.grant(&approval_id).await;
    h.wakeup(&approval_id).await;
    let malformed = h
        .scheduler
        .inbox()
        .append(
            &h.session_id,
            InboxKind::Wakeup,
            serde_json::json!({ "decision": "grant" }),
            0,
            Actor::User,
        )
        .await
        .unwrap();

    assert!(matches!(h.resume("carry on").await, ResumeOutcome::Settled { .. }));

    let left = h.pending_kinds().await;
    assert!(
        !left.contains(&InboxKind::Wakeup),
        "no wakeup survives, including the bad one: {left:?}"
    );

    let discarded = h
        .log()
        .await
        .into_iter()
        .find(|e| {
            e.event.event_type.as_str() == EventType::INBOX_DISCARDED
                && e.event.payload["item_id"] == serde_json::json!(malformed.item_id)
        })
        .expect("the malformed wakeup is discarded, not silently left behind");
    let reason = discarded.event.payload["reason"].as_str().expect("a readable reason");
    assert!(reason.contains("approval_id"), "{reason}");
    // The cause is the half a reader can branch on without parsing prose.
    assert_eq!(discarded.event.payload["cause"], serde_json::json!("unreadable"));
}

/// The `approval/expired` event `sweep_expired` writes, in the shape it
/// writes it.
fn expired_draft(session_id: &SessionId, approval_id: &ApprovalId) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(EventType::APPROVAL_EXPIRED),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: session_id.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor: Actor::Scheduler,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({ "approval_id": approval_id, "cause": "pending_ttl" }),
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

async fn seed(store: &dyn EventStore, session_id: &SessionId, drafts: Vec<EventDraft>) {
    let next = store
        .read_stream(session_id, EventSeq::ZERO, usize::MAX)
        .await
        .unwrap()
        .last()
        .map(|e| e.seq.next())
        .unwrap_or(EventSeq::ZERO);
    store.append(session_id, next, drafts).await.unwrap();
}

#[test]
fn a_resumed_dispatch_writes_scrubbed_arguments() {
    // A call held for approval is re-dispatched from the arguments kept in
    // the log (`Wait::arguments`). A log written before argument redaction
    // existed still holds them raw, and this second writer must not carry
    // that secret forward into the new record it writes (§7 row 4).
    let held = HeldCall {
        tool_name: "deploy".to_string(),
        tool_call_id: "call-1".to_string(),
        step_id: StepId::new(),
        run_id: Some(RunId::new()),
        turn_id: TurnId::new(),
        wait: None,
    };
    let wait = Wait {
        approval_id: ApprovalId::new(),
        arguments: serde_json::json!({
            "token": "ghp_SECRETVALUE0123456789",
            "target": "prod",
        }),
    };

    let draft = dispatch_draft(&SessionId::new(), &held, &wait);

    assert_eq!(draft.payload["arguments"]["token"], serde_json::json!("***"));
    assert_eq!(draft.payload["arguments"]["target"], serde_json::json!("prod"));
    assert_eq!(draft.payload["redaction"]["redacted"], true);
    let text = serde_json::to_string(&draft.payload).unwrap();
    assert!(!text.contains("SECRETVALUE"), "secret leaked into the dispatch: {text}");
}
