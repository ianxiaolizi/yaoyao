//! Scheduled turns through the product's assembly, end to end.
//!
//! The kernel's own tests drive `run_due` over a fake store and a fake model.
//! What they cannot show is that the time reaches the model through the wiring
//! the binary actually uses: the projection that turns an inbox payload into the
//! turn's user message, the resolved profile the scheduler stamps, the real JSONL
//! log. This suite calls `agent::stack::assemble` — the same function `apps/agent`
//! calls — and then presses the same button `serve` presses, `Scheduler::run_due`.
//!
//! Two claims are made here:
//!
//! | claim | how it is evidenced |
//! |---|---|
//! | a due item becomes a turn the model sees | the echo provider's answer quotes the scheduled text, and the session records `turn/completed` |
//! | the item is claimed inside the turn's terminal batch, once | one `inbox/claimed` for that item, sequenced between `turn/started` and `turn/completed`; nothing is left pending |
//!
//! A future item is also shown to stay untouched: `run_due` before its time
//! changes nothing, which is what makes `serve`'s poll loop safe.

use std::path::Path;
use std::sync::Arc;

use agent::providers::{EchoModelProvider, SelectedProvider};
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile};
use agent_kernel::inbox::scheduled_due_at;
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_protocol::events::{Actor, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use chrono::{Duration, Utc};
use tempfile::TempDir;

/// The session every test here runs under.
const SESSION: &str = "schedule-acceptance";

/// One assembled stack over a fresh log, with the echo model.
///
/// The model is handed in rather than selected from `AGENT_MODEL_PROVIDER`: the
/// environment belongs to the whole test process, and the echo provider is the
/// same value `select` would have built for it. Its answer begins `echo: ` and
/// quotes the text it was given, which is how the test knows the projection
/// carried the scheduled payload to the model.
async fn assembled(data: &Path) -> stack::Runtime {
    stack::assemble(stack::Inputs {
        session: SessionId::from_string(SESSION),
        workspace_root: data.to_path_buf(),
        data_dir: data.to_path_buf(),
        resolved: default_profile(),
        model: Some(SelectedProvider {
            provider: Arc::new(EchoModelProvider),
            route: ModelRoute::new(ModelTier::Main, "echo", "echo"),
        }),
        decisions: Decisions::Unobserved,
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: true,
    })
    .await
    .expect("the assembly the binary runs")
}

/// The configuration a run in a directory with no configuration has.
fn default_profile() -> ResolvedProfile {
    agent_config::resolve(Layers::default()).expect("the defaults resolve")
}

async fn log(runtime: &stack::Runtime, session: &SessionId) -> Vec<StoredEvent> {
    runtime.store.read_stream(session, EventSeq::ZERO, usize::MAX).await.expect("read the log")
}

#[tokio::test]
async fn a_due_item_becomes_a_turn_the_model_sees_and_is_claimed_in_its_batch() {
    let data = TempDir::new().unwrap();
    let runtime = assembled(data.path()).await;
    let session = SessionId::from_string(SESSION);

    let due = Utc::now() - Duration::seconds(1);
    let item = runtime
        .scheduler
        .inbox()
        .schedule(&session, due, "wake up and check the repo", 0, Actor::User)
        .await
        .expect("schedule the turn");
    assert_eq!(scheduled_due_at(&item).expect("a readable due time"), due);

    let report = runtime
        .scheduler
        .run_due(&session, Utc::now())
        .await
        .expect("run_due answers")
        .expect("a due item runs");

    // The model saw the scheduled text, through the ordinary projection and the
    // ordinary turn: this is the whole claim, and the answer is its evidence.
    assert_eq!(
        report.final_content.as_deref(),
        Some("echo: wake up and check the repo"),
        "the scheduled payload did not reach the model as the turn's input"
    );

    let events = log(&runtime, &session).await;
    let types: Vec<&str> = events.iter().map(|e| e.event.event_type.as_str()).collect();

    let claim = events
        .iter()
        .find(|e| {
            e.event.event_type.as_str() == EventType::INBOX_CLAIMED
                && e.event.payload["item_id"] == item.item_id.as_str()
        })
        .unwrap_or_else(|| panic!("the item was not claimed: {types:?}"));
    assert_eq!(
        claim.event.payload["run_id"],
        report.run_id.as_str(),
        "the claim belongs to the turn that took it"
    );

    // The terminal batch is one append, so it occupies one contiguous range of
    // sequence numbers: the claim is written in it (first, because the drafts
    // are built claim-first) and so is the turn's closing. The turn's *opening*
    // is a separate append that precedes it: the run is durable from the moment
    // it exists, while the claim is not written until the item has an answer.
    // A claim at the opening would leave the message consumed by a turn that
    // never finished — the consume-then-die shape this design avoids.
    let started = events
        .iter()
        .find(|e| e.event.event_type.as_str() == EventType::TURN_STARTED)
        .expect("the turn opened");
    let completed = events
        .iter()
        .find(|e| e.event.event_type.as_str() == EventType::TURN_COMPLETED)
        .expect("the turn closed");
    assert!(
        started.seq < claim.seq,
        "the turn's opening must precede its claim: started {}, claimed {}",
        started.seq,
        claim.seq
    );
    let batch: Vec<&StoredEvent> =
        events.iter().filter(|e| claim.seq <= e.seq && e.seq <= completed.seq).collect();
    assert_eq!(
        batch.len() as u64,
        completed.seq.as_u64() - claim.seq.as_u64() + 1,
        "the claim and the turn's close are not one contiguous append: {types:?}"
    );
    assert!(
        batch.iter().all(|e| e.event.event_type.as_str() != EventType::TURN_STARTED),
        "the turn's opening shares the append its claim belongs to: {types:?}"
    );
    assert_eq!(
        events.iter().filter(|e| e.event.event_type.as_str() == EventType::INBOX_CLAIMED).count(),
        1,
        "the item was claimed more than once: {types:?}"
    );

    // And nothing is left to run: the queue is what the log says it is.
    assert!(
        runtime.scheduler.inbox().scheduled(&session).await.expect("read the queue").is_empty(),
        "the claimed item is still listed as scheduled"
    );
    assert!(
        runtime.scheduler.inbox().pending(&session).await.expect("read the queue").is_empty(),
        "the claimed item is still pending"
    );
    assert!(
        runtime.scheduler.run_due(&session, Utc::now()).await.expect("second tick").is_none(),
        "the same now ran the item twice"
    );
}

#[tokio::test]
async fn an_item_not_yet_due_is_left_completely_alone() {
    let data = TempDir::new().unwrap();
    let runtime = assembled(data.path()).await;
    let session = SessionId::from_string(SESSION);

    runtime
        .scheduler
        .inbox()
        .schedule(&session, Utc::now() + Duration::hours(1), "later", 0, Actor::User)
        .await
        .expect("schedule the turn");

    assert!(
        runtime.scheduler.run_due(&session, Utc::now()).await.expect("run_due answers").is_none(),
        "a future item ran early"
    );

    // The queueing wrote the session's start and the item; `run_due` added
    // nothing to them, which is what "left alone" means here.
    let events = log(&runtime, &session).await;
    let types: Vec<&str> = events.iter().map(|e| e.event.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![EventType::SESSION_STARTED, EventType::INBOX_APPENDED],
        "a future item left a trace: {types:?}"
    );
    assert_eq!(
        runtime.scheduler.inbox().scheduled(&session).await.expect("read the queue").len(),
        1,
        "the future item is no longer queued"
    );
}
