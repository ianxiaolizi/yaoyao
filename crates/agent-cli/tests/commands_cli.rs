//! The typed command seam: what the web layer will call instead of the
//! printing `run_*` functions.

use std::sync::Arc;

use agent_cli::commands::{self, CommandError, Grant, InterruptOutcome, TurnStart};
use agent_kernel::approval::ApprovalService;
use agent_kernel::inbox::Inbox;
use agent_kernel::scheduler::Scheduler;
use agent_loop::DefaultAgentLoop;
use agent_ports::agent_loop::StopReason;
use agent_ports::event_store::EventStore;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, RunId, SchemaVersion, SessionId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::permissions::{ApprovalDecision, Capability};
use agent_testkit::{
    FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine,
    one_model_router,
};
use chrono::Utc;

/// A scheduler over a fake store, with the store handed back: seeding an event
/// needs it, and so does the approval service.
fn harness() -> (Scheduler, Arc<FakeEventStore>, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("hello")]));
    let tools = Arc::new(FakeToolProvider::new());
    let gateway = Arc::new(FakeToolGateway::new(tools));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model,
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    ));
    let scheduler = Scheduler::new(store.clone(), loop_, 8);
    let sid = SessionId::from_string("commands-session");
    (scheduler, store, sid)
}

/// Open a run the way the scheduler opens one: a `run/started` with nothing
/// after it. Then the next `start_turn` must find the session busy.
async fn seed_open_run(store: &FakeEventStore, sid: &SessionId) {
    let draft = EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(EventType::RUN_STARTED),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: sid.clone(),
        run_id: Some(RunId::new()),
        turn_id: None,
        step_id: None,
        actor: Actor::Scheduler,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    };
    store.append(sid, EventSeq::ZERO, vec![draft]).await.unwrap();
}

#[tokio::test]
async fn start_turn_returns_the_run_it_drove() {
    let (scheduler, _, sid) = harness();

    let started = commands::start_turn(&scheduler, &sid, "hi", Utc::now()).await.unwrap();

    match started {
        TurnStart::Ran(report) => {
            assert_eq!(report.final_content.as_deref(), Some("hello"));
            assert_eq!(report.stop_reason, StopReason::FinalAnswer);
        }
        TurnStart::NotStarted => panic!("a free session must start the turn"),
    }
}

#[tokio::test]
async fn start_turn_leaves_the_text_queued_when_a_run_is_open() {
    let (scheduler, store, sid) = harness();
    seed_open_run(&store, &sid).await;

    let started = commands::start_turn(&scheduler, &sid, "hi", Utc::now()).await.unwrap();

    assert_eq!(started, TurnStart::NotStarted);
    let pending = scheduler.inbox().pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1, "the text was queued and nothing consumed it");
    assert_eq!(pending[0].kind, InboxKind::NextTurn);
}

#[tokio::test]
async fn status_counts_what_is_pending() {
    let (scheduler, _, sid) = harness();
    scheduler
        .inbox()
        .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "later"}), 0, Actor::User)
        .await
        .unwrap();

    let report = commands::status(&scheduler, &sid).await.unwrap();

    assert_eq!(report.pending, 1);
    assert!(report.open_turn.is_none());
}

#[tokio::test]
async fn a_grant_reports_the_reply_and_queues_a_wakeup() {
    let (_, store, sid) = harness();
    let approvals = ApprovalService::new(store.clone());
    let inbox = Inbox::new(store);
    let request = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "src/main.rs")
        .await
        .unwrap();

    let receipt = commands::grant_approval(
        &approvals,
        &inbox,
        &sid,
        request.approval_id.as_str(),
        Grant::Once,
        "user",
    )
    .await
    .unwrap();

    assert_eq!(receipt.replies.len(), 1);
    assert_eq!(receipt.replies[0].decision, ApprovalDecision::Grant);
    let pending = inbox.pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1, "a wakeup was queued");
    assert_eq!(pending[0].kind, InboxKind::Wakeup);
}

#[tokio::test]
async fn interrupt_with_nothing_in_flight_is_not_a_queue() {
    let (scheduler, _, sid) = harness();

    let outcome = commands::interrupt(&scheduler, &sid).await.unwrap();

    assert_eq!(outcome, InterruptOutcome::NothingToInterrupt);
    assert!(scheduler.inbox().pending(&sid).await.unwrap().is_empty());
}

#[tokio::test]
async fn steer_refuses_empty_text_before_writing_anything() {
    let (scheduler, _, sid) = harness();

    let error = commands::steer(&scheduler, &sid, "   ").await.unwrap_err();

    assert!(matches!(error, CommandError::Invalid(_)), "unexpected: {error:?}");
    assert!(scheduler.inbox().pending(&sid).await.unwrap().is_empty());
}
