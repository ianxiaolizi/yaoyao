//! One driver per session, across stores that share nothing but the files.
//!
//! The claim is a separate file from the write lease, and this file is about
//! why: a driver holds it for a whole turn while steering and approval answers
//! are single appends that must not queue behind it. What is exercised here is
//! two `JsonlEventStore` instances in one process — each with its own lease
//! table, sharing exactly the files and the OS advisory locks that two
//! processes would share — which is the same arrangement the rest of the
//! cross-writer story is tested with (see `cross_process_writers.rs`).

use std::time::Duration;

use agent_ports::event_store::{DRIVER_HEARTBEAT, EventStore, EventStoreError, SessionLease};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId};
use agent_store_jsonl::JsonlEventStore;
use chrono::Utc;

fn draft(session_id: &SessionId, event_type: &str) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: session_id.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor: Actor::User,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

/// The claim, or a panic naming the refusal.
///
/// `Result::unwrap` is not available here: its `Ok` type is a
/// `Box<dyn SessionLease>`, which is not `Debug`, and giving every lease a
/// `Debug` impl so a test can unwrap one would be the tail wagging the dog.
fn took(result: Result<Box<dyn SessionLease>, EventStoreError>) -> Box<dyn SessionLease> {
    match result {
        Ok(lease) => lease,
        Err(error) => panic!("expected the driver claim, got {error}"),
    }
}

fn two_drivers(root: &std::path::Path) -> (JsonlEventStore, JsonlEventStore) {
    let open = || JsonlEventStore::new(root.to_path_buf());
    (open(), open())
}

#[tokio::test]
async fn a_second_driver_of_the_same_session_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("one-driver");
    let (a, b) = two_drivers(dir.path());

    let driver = took(a.driver_lease(&sid).await);
    assert_eq!(driver.session_id(), &sid);

    let refused = b.driver_lease(&sid).await;
    assert!(
        matches!(refused, Err(EventStoreError::ConcurrentDriver { ref session_id, .. }) if session_id == &sid),
        "another store on the same session must be refused, got {:?}",
        refused.err().map(|error| error.to_string())
    );

    // Not re-entrant, and the same store is not an exception: the second
    // caller in one process is as much a second driver as one in another.
    let nested = a.driver_lease(&sid).await;
    assert!(
        matches!(nested, Err(EventStoreError::ConcurrentDriver { .. })),
        "a driver must not be able to nest its own claim, got {:?}",
        nested.err().map(|error| error.to_string())
    );

    // The claim is per session, not per store: a different session is free.
    let other = SessionId::from_string("someone-else");
    let elsewhere = took(b.driver_lease(&other).await);
    drop(elsewhere);
    drop(driver);
}

#[tokio::test]
async fn dropping_the_claim_lets_the_next_driver_in() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("handover");
    let (a, b) = two_drivers(dir.path());

    let first = took(a.driver_lease(&sid).await);
    assert!(b.driver_lease(&sid).await.is_err());
    drop(first);

    // The release is the drop: no explicit unlock to forget, and the next
    // caller takes the claim while the first one is still alive.
    let second = took(b.driver_lease(&sid).await);
    assert!(a.driver_lease(&sid).await.is_err(), "the second holder excludes the first");
    drop(second);

    let third = took(a.driver_lease(&sid).await);
    drop(third);
}

#[tokio::test]
async fn a_refusal_says_who_is_driving() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("who-goes-there");
    let (a, b) = two_drivers(dir.path());

    let before = Utc::now();
    let driver = took(a.driver_lease(&sid).await);

    let holder = match b.driver_lease(&sid).await {
        Err(EventStoreError::ConcurrentDriver { holder, .. }) => {
            holder.expect("the holder records itself, and the record is readable across stores")
        }
        Ok(_) => panic!("expected a refusal, got the claim"),
        Err(error) => panic!("expected a driver refusal, got {error}"),
    };
    // The record is the *other* caller's account of itself: the pid is this
    // process's, and the timestamps are from around when it claimed.
    assert_eq!(holder.pid, std::process::id());
    assert!(holder.started_at >= before, "the record says when the claim started");
    assert!(
        holder.heartbeat_at >= holder.started_at,
        "the first heartbeat is taken at the same moment as the claim"
    );

    // A refusal's message is what a person reads, so it has to carry the pid.
    let message = match b.driver_lease(&sid).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("expected a refusal, got the claim"),
    };
    assert!(message.contains(&holder.pid.to_string()), "refusal should name the pid: {message}");
    drop(driver);
}

#[tokio::test]
async fn a_driver_does_not_block_the_writes_a_second_process_needs() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("steering-while-driving");
    let (a, b) = two_drivers(dir.path());

    // A turn is running in A and holds the claim for its whole length.
    let driver = took(a.driver_lease(&sid).await);

    // Meanwhile steering and approval answers arrive in B. They are what the
    // separate lock file is for: if the driver claim were the write lease, a
    // long turn would make them fail or queue for minutes.
    b.append(&sid, EventSeq::ZERO, vec![draft(&sid, "inbox/steered")])
        .await
        .expect("steering must be writable while another caller drives");
    let write_lease = match b.lease(&sid).await {
        Ok(lease) => lease,
        Err(error) => panic!("the write lease is not what the driver holds: {error}"),
    };
    drop(write_lease);

    assert_eq!(a.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().len(), 1);
    drop(driver);
}

#[tokio::test]
async fn the_holder_keeps_its_record_fresh_while_it_drives() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("heartbeat");
    // Seconds would make this a slow test for a number that is only a
    // diagnostic; what is under test is that the beat happens at all.
    let tick = Duration::from_millis(40);
    let a = JsonlEventStore::new(dir.path().to_path_buf()).with_driver_heartbeat(tick);
    let b = JsonlEventStore::new(dir.path().to_path_buf());

    let driver = took(a.driver_lease(&sid).await);
    let first = holder_of(&b, &sid).await;
    // Two beats later. The wait is generous next to the interval because the
    // thread is the point: a beat is a real wake-up, not a computed value.
    std::thread::sleep(tick * 3);
    let later = holder_of(&b, &sid).await;
    drop(driver);

    assert!(later.heartbeat_at > first.heartbeat_at, "the held claim's heartbeat must advance");
    assert_eq!(later.started_at, first.started_at, "the claim's start is a fact and does not move");

    // The default interval is the audit's five seconds, and it is what a build
    // with no test override runs at.
    assert_eq!(DRIVER_HEARTBEAT, Duration::from_secs(5));
}

/// The holder record as a second store sees it, via a refusal.
async fn holder_of(
    store: &JsonlEventStore,
    session: &SessionId,
) -> agent_ports::event_store::DriverRecord {
    match store.driver_lease(session).await {
        Err(EventStoreError::ConcurrentDriver { holder, .. }) => {
            holder.expect("a held claim records its holder")
        }
        Ok(_) => panic!("expected the claim to be held, got it"),
        Err(error) => panic!("expected a driver refusal, got {error}"),
    }
}

#[tokio::test]
async fn a_released_claim_leaves_no_record_behind() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("clean-exit");
    let (a, b) = two_drivers(dir.path());

    let driver = took(a.driver_lease(&sid).await);
    drop(driver);

    // Cleared while the claim was still held, so a record that exists means a
    // driver that exists — the next holder writes its own.
    let record = dir.path().join(sid.as_str()).join("driver.holder");
    assert_eq!(std::fs::read(&record).unwrap(), Vec::<u8>::new());

    let next = took(b.driver_lease(&sid).await);
    let seen = holder_of(&a, &sid).await;
    assert_eq!(seen.pid, std::process::id());
    drop(next);
}
