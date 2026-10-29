//! Fake correctness tests for agent-testkit.
//!
//! These verify the in-memory fakes behave as their real implementations
//! would — seq assignment, expected-next-seq checks, idempotency, and
//! pass/fail results that are not normalized to success.

use agent_ports::doctor::DoctorCheck;
use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::diagnostics::{HealthCheckId, HealthStatus};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::*;
use agent_testkit::{FakeDoctorCheck, FakeEventStore};
use chrono::Utc;

fn draft(session_id: &str, event_type: &str) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: SessionId::from_string(session_id),
        run_id: Some(RunId::new()),
        turn_id: Some(TurnId::new()),
        step_id: Some(StepId::new()),
        actor: Actor::System,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

#[tokio::test]
async fn fake_event_store_appends_and_reads_back() {
    let store = FakeEventStore::new();
    let sid = SessionId::from_string("test-session");

    let receipt = store
        .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_STARTED)])
        .await
        .unwrap();
    assert_eq!(receipt.appended.len(), 1);
    assert_eq!(receipt.appended[0].seq, EventSeq::ZERO);

    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.session_id, sid);
    assert_eq!(events[0].event.event_type.as_str(), EventType::TURN_STARTED);
}

#[tokio::test]
async fn fake_event_store_assigns_contiguous_seq() {
    let store = FakeEventStore::new();
    let sid = SessionId::from_string("test-session");

    let r1 = store
        .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_STARTED)])
        .await
        .unwrap();
    let r2 = store
        .append(&sid, EventSeq::new(1), vec![draft("test-session", EventType::TURN_COMPLETED)])
        .await
        .unwrap();

    assert_eq!(r1.appended[0].seq, EventSeq::ZERO);
    assert_eq!(r2.appended[0].seq, EventSeq::new(1));
}

#[tokio::test]
async fn fake_event_store_rejects_stale_expected_seq() {
    let store = FakeEventStore::new();
    let sid = SessionId::from_string("test-session");

    store
        .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_STARTED)])
        .await
        .unwrap();

    // Expecting seq 0 again should conflict (actual next is now 1).
    let err = store
        .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_COMPLETED)])
        .await
        .unwrap_err();
    match err {
        EventStoreError::SequenceConflict { expected, actual } => {
            assert_eq!(expected, EventSeq::ZERO);
            assert_eq!(actual, EventSeq::new(1));
        }
        other => panic!("expected SequenceConflict, got {other:?}"),
    }
}

#[tokio::test]
async fn fake_event_store_is_idempotent_on_duplicate_event_id() {
    let store = FakeEventStore::new();
    let sid = SessionId::from_string("test-session");
    let d = draft("test-session", EventType::TURN_STARTED);

    let r1 = store.append(&sid, EventSeq::ZERO, vec![d.clone()]).await.unwrap();
    assert_eq!(r1.appended.len(), 1);

    // Re-append the same event id with the now-current next seq.
    let r2 = store.append(&sid, EventSeq::new(1), vec![d]).await.unwrap();
    assert_eq!(r2.appended.len(), 0);
    assert_eq!(r2.duplicates, 1);

    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1); // applied once, not twice
}

#[tokio::test]
async fn fake_event_store_subscribes_inclusively_and_keeps_watching() {
    let store = FakeEventStore::new();
    let sid = SessionId::from_string("test-session");

    store
        .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_STARTED)])
        .await
        .unwrap();
    store
        .append(&sid, EventSeq::new(1), vec![draft("test-session", EventType::TURN_COMPLETED)])
        .await
        .unwrap();

    let mut sub = store.subscribe(&sid, EventSeq::ZERO).await.unwrap();
    let first = sub.next().await.unwrap().expect("the event at seq 0");
    assert_eq!(first.seq, EventSeq::ZERO);
    let second = sub.next().await.unwrap().expect("the event at seq 1");
    assert_eq!(second.seq, EventSeq::new(1));

    // The fake has to wait like the real store, or every test built on it would
    // be written against semantics the real one does not have.
    store
        .append(&sid, EventSeq::new(2), vec![draft("test-session", EventType::RUN_COMPLETED)])
        .await
        .unwrap();
    let third = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("a caught-up subscription waits instead of ending")
        .unwrap()
        .expect("the event appended after the subscription began");
    assert_eq!(third.seq, EventSeq::new(2));
}

#[tokio::test]
async fn a_lease_requiring_fake_refuses_an_unleased_append() {
    let store = FakeEventStore::new().requiring_lease();
    let sid = SessionId::from_string("test-session");

    // No lease held: the write a caller could be overtaken on is refused.
    let err = store
        .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_STARTED)])
        .await
        .unwrap_err();
    assert!(matches!(err, EventStoreError::ConcurrentWriter(ref s) if s == &sid));

    // Nothing landed.
    assert!(store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_lease_requiring_fake_admits_a_leased_append_and_releases_on_drop() {
    let store = FakeEventStore::new().requiring_lease();
    let sid = SessionId::from_string("test-session");

    {
        let _lease = store.lease(&sid).await.unwrap();
        store
            .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_STARTED)])
            .await
            .unwrap();
    }

    // The lease went with the guard: the same write is refused again.
    let err = store
        .append(&sid, EventSeq::new(1), vec![draft("test-session", EventType::TURN_COMPLETED)])
        .await
        .unwrap_err();
    assert!(matches!(err, EventStoreError::ConcurrentWriter(_)));
}

/// The fake's driver claim has to refuse the way the real one does: one driver
/// per session, no waiting, no nesting — the real store takes a fresh file lock
/// per caller for exactly this reason.
#[tokio::test]
async fn the_fake_admits_one_driver_per_session_and_no_nesting() {
    let store = FakeEventStore::new();
    let sid = SessionId::from_string("test-session");

    let driver = store.driver_lease(&sid).await.unwrap();

    // Same store, second caller: still a second driver.
    let nested = store.driver_lease(&sid).await;
    assert!(
        matches!(nested, Err(EventStoreError::ConcurrentDriver { ref session_id, .. }) if session_id == &sid),
        "a driver claim must not nest, got {:?}",
        nested.err().map(|error| error.to_string())
    );
    // No holder record: nothing in this process has a pid to report.
    match store.driver_lease(&sid).await {
        Err(EventStoreError::ConcurrentDriver { holder, .. }) => assert_eq!(holder, None),
        Ok(_) => panic!("expected a refusal, got the claim"),
        Err(error) => panic!("expected a driver refusal, got {error}"),
    }

    // A different session is a different claim.
    let other = store.driver_lease(&SessionId::from_string("elsewhere")).await.unwrap();

    // And the claim is a guard: dropping it is the release.
    drop(driver);
    let again = store.driver_lease(&sid).await.expect("the claim is free after the drop");
    drop(again);
    drop(other);
}

#[tokio::test]
async fn a_default_fake_still_permits_a_direct_append() {
    let store = FakeEventStore::new();
    let sid = SessionId::from_string("test-session");

    // The enforcement is opt-in: the real store does not require a caller lease
    // for a bare append either, so the default fake must not either.
    store
        .append(&sid, EventSeq::ZERO, vec![draft("test-session", EventType::TURN_STARTED)])
        .await
        .unwrap();
}

#[tokio::test]
async fn a_lease_refuses_the_session_names_the_real_store_cannot_open() {
    let store = FakeEventStore::new();
    // Names no layout can accept: the two directory self-names, an empty
    // spelling, characters that are not a single path segment, and one longer
    // than a filesystem allows.
    let too_long = "x".repeat(129);
    for name in [".", "..", "", "has space", "nested/dir", too_long.as_str()] {
        let sid = SessionId::from_string(name);
        let err = match store.lease(&sid).await {
            Ok(_) => panic!("`{name:?}` was given a lease"),
            Err(error) => error,
        };
        assert!(
            matches!(&err, EventStoreError::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput),
            "`{name:?}` should be refused as invalid input, got {err:?}"
        );
        assert!(
            err.to_string().contains("not a safe directory name"),
            "the refusal should be the store's own: {err}"
        );
    }

    // The names Windows renames or routes to a device (YA-71). They are legal
    // on other platforms, so this half is Windows-only for the same reason the
    // store's check is.
    #[cfg(windows)]
    for name in ["NUL", "con.txt", "foo."] {
        let sid = SessionId::from_string(name);
        let err = match store.lease(&sid).await {
            Ok(_) => panic!("`{name:?}` was given a lease"),
            Err(error) => error,
        };
        assert!(
            matches!(&err, EventStoreError::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput),
            "`{name:?}` should be refused as invalid input, got {err:?}"
        );
    }

    // An ordinary name still gets its lease.
    let sid = SessionId::from_string("test-session");
    assert!(store.lease(&sid).await.is_ok());
}

#[tokio::test]
async fn fake_doctor_check_passing() {
    let check = FakeDoctorCheck::passing("test-check");
    assert_eq!(check.id(), HealthCheckId::new("test-check"));

    let results = check.run().await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, HealthStatus::Passed);
}

#[tokio::test]
async fn fake_doctor_check_failing() {
    let check = FakeDoctorCheck::failing("bad-check", "something broke");
    let results = check.run().await;
    assert_eq!(results[0].status, HealthStatus::Failed);
    assert_eq!(results[0].message, "something broke");
}
