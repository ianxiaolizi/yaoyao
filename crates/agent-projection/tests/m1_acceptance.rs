//! M1 acceptance suite: one test per acceptance criterion, running the real
//! JSONL store and projection together where they interact.
//!
//! The M1 acceptance list has 13 criteria. Ten are testable here; the remaining
//! three need M2 semantics (session lifecycle / recovery / fork) and are
//! documented below, not faked:
//!
//! - "open run/turn/step repaired as interrupted" — needs run/step lifecycle
//!   tracking plus a recovery write path (M2 Scheduler).
//! - "side-effecting tool without terminal outcome is not auto-replayed" —
//!   needs side-effect classification and replay policy (M2).
//! - "fork only at a closed Turn boundary" — needs fork lineage and open-turn
//!   detection (M2).

use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_projection::{Projection, ProjectionError, SessionProjection};
use agent_protocol::events::{Actor, EventDraft, EventType, LiveEvent, StoredEvent};
use agent_protocol::ids::*;
use agent_store_jsonl::JsonlEventStore;
use chrono::Utc;

fn draft(session_id: &str, event_type: &str, ignorable: bool) -> EventDraft {
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
        ignorable,
    }
}

#[tokio::test]
async fn acceptance_append_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("a1");

    let d0 = draft("a1", EventType::TURN_STARTED, false);
    let d1 = draft("a1", EventType::TURN_COMPLETED, false);
    store.append(&sid, EventSeq::ZERO, vec![d0.clone(), d1.clone()]).await.unwrap();

    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event.event_id, d0.event_id);
    assert_eq!(events[0].seq, EventSeq::ZERO);
    assert_eq!(events[1].event.event_id, d1.event_id);
    assert_eq!(events[1].seq, EventSeq::new(1));

    // The log is one JSON line per event, in seq order.
    let raw = std::fs::read_to_string(dir.path().join("a1").join("events.jsonl")).unwrap();
    assert_eq!(raw.lines().count(), 2);
}

#[tokio::test]
async fn acceptance_seq_contiguous_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("a2");

    for i in 0..4u64 {
        store
            .append(&sid, EventSeq::new(i), vec![draft("a2", EventType::TURN_STARTED, false)])
            .await
            .unwrap();
    }

    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    let seqs: Vec<u64> = events.iter().map(|e| e.seq.as_u64()).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3]);
}

#[tokio::test]
async fn acceptance_expected_next_seq() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("a3");

    store
        .append(&sid, EventSeq::ZERO, vec![draft("a3", EventType::TURN_STARTED, false)])
        .await
        .unwrap();

    let err = store
        .append(&sid, EventSeq::ZERO, vec![draft("a3", EventType::TURN_COMPLETED, false)])
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
async fn acceptance_event_id_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("a4");
    let d = draft("a4", EventType::TURN_STARTED, false);

    store.append(&sid, EventSeq::ZERO, vec![d.clone()]).await.unwrap();
    let r2 = store.append(&sid, EventSeq::new(1), vec![d]).await.unwrap();
    assert_eq!(r2.appended.len(), 0);
    assert_eq!(r2.duplicates, 1);

    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn acceptance_flush_crash_durable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sid = SessionId::from_string("a5");

    {
        let store = JsonlEventStore::new(root.clone());
        store
            .append(&sid, EventSeq::ZERO, vec![draft("a5", EventType::TURN_STARTED, false)])
            .await
            .unwrap();
        store.flush(&sid).await.unwrap();
    }

    let reopened = JsonlEventStore::new(root.clone());
    let events = reopened.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, EventSeq::ZERO);
}

#[tokio::test]
async fn acceptance_corrupt_checkpoint_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = Arc::new(JsonlEventStore::new(root.clone()));
    let projection = Projection::new(SessionProjection::new(), store.clone(), root.clone());
    let sid = SessionId::from_string("a6");

    store
        .append(&sid, EventSeq::ZERO, vec![draft("a6", EventType::SESSION_STARTED, false)])
        .await
        .unwrap();

    let initial = projection.build_full(&sid).await.unwrap();
    projection.checkpoint(&sid, &initial).await.unwrap();

    std::fs::write(root.join("a6").join("checkpoints").join("session.json"), b"{ not json")
        .unwrap();

    let full = projection.build_full(&sid).await.unwrap();
    let rebuilt = projection.build(&sid).await.unwrap();
    assert_eq!(rebuilt.state, full.state);
}

#[tokio::test]
async fn acceptance_checkpoint_plus_tail_equals_full() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = Arc::new(JsonlEventStore::new(root.clone()));
    let projection = Projection::new(SessionProjection::new(), store.clone(), root.clone());
    let sid = SessionId::from_string("a7");

    store
        .append(
            &sid,
            EventSeq::ZERO,
            vec![
                draft("a7", EventType::SESSION_STARTED, false),
                draft("a7", EventType::TURN_STARTED, false),
                draft("a7", EventType::TURN_COMPLETED, false),
            ],
        )
        .await
        .unwrap();

    let initial = projection.build_full(&sid).await.unwrap();
    assert_eq!(initial.as_of_seq, Some(EventSeq::new(2)));
    projection.checkpoint(&sid, &initial).await.unwrap();

    store
        .append(
            &sid,
            EventSeq::new(3),
            vec![
                draft("a7", EventType::TURN_STARTED, false),
                draft("a7", EventType::TURN_FAILED, false),
            ],
        )
        .await
        .unwrap();

    let with_checkpoint = projection.build(&sid).await.unwrap();
    let full = projection.build_full(&sid).await.unwrap();
    assert_eq!(with_checkpoint.state, full.state);
    assert_eq!(with_checkpoint.as_of_seq, Some(EventSeq::new(4)));
}

#[tokio::test]
async fn acceptance_unknown_ignorable_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = Arc::new(JsonlEventStore::new(root.clone()));
    let projection = Projection::new(SessionProjection::new(), store.clone(), root.clone());
    let sid = SessionId::from_string("a8");

    store
        .append(
            &sid,
            EventSeq::ZERO,
            vec![
                draft("a8", EventType::SESSION_STARTED, false),
                draft("a8", "plugin/custom", true),
            ],
        )
        .await
        .unwrap();

    let projected = projection.build_full(&sid).await.unwrap();
    assert!(projected.state.started);
    assert_eq!(projected.as_of_seq, Some(EventSeq::new(1)));
}

#[tokio::test]
async fn acceptance_unknown_required_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = Arc::new(JsonlEventStore::new(root.clone()));
    let projection = Projection::new(SessionProjection::new(), store.clone(), root.clone());
    let sid = SessionId::from_string("a9");

    store
        .append(
            &sid,
            EventSeq::ZERO,
            vec![
                draft("a9", EventType::SESSION_STARTED, false),
                draft("a9", "plugin/custom", false),
            ],
        )
        .await
        .unwrap();

    let err = projection.build_full(&sid).await.unwrap_err();
    match err {
        ProjectionError::UnknownRequiredEvent { event_type, seq } => {
            assert_eq!(event_type.as_str(), "plugin/custom");
            assert_eq!(seq, EventSeq::new(1));
        }
        other => panic!("expected UnknownRequiredEvent, got {other:?}"),
    }
}

#[tokio::test]
async fn acceptance_live_delta_not_durable() {
    // Live events carry their own live_seq and no durable seq.
    let live = LiveEvent {
        live_seq: 7,
        event_type: EventType::new("ui/model.delta"),
        session_id: SessionId::from_string("a10"),
        created_at: Utc::now(),
        payload: serde_json::json!({"delta": "x"}),
        metadata: serde_json::json!({}),
    };
    let json = serde_json::to_value(&live).unwrap();
    assert!(json.get("seq").is_none());
    assert_eq!(json.get("live_seq").and_then(|v| v.as_u64()), Some(7));

    // Durable stored events always carry a seq.
    let stored = StoredEvent {
        seq: EventSeq::new(3),
        recorded_at: Utc::now(),
        event: draft("a10", EventType::TURN_STARTED, false),
    };
    let json = serde_json::to_value(&stored).unwrap();
    assert!(json.get("seq").is_some());
}
