//! Whether a session is put away, recorded in the session's own log.
//!
//! Archiving is a decision about how the session is listed, not a change to the
//! session: the log keeps every event it ever had, the session can be opened and
//! even continued, and taking it back out is one more event. Deleting is the
//! other thing — a session that should leave the disk goes through the store,
//! not through this module — and the two are deliberately separate so that
//! "hidden" and "gone" cannot be confused in a menu or in a log.
//!
//! It lives in the log for the same reasons the model choice does: every entry
//! point (page, CLI, `tick`) reads the same answer, and a fork copies the log
//! and so inherits it. A side file would be a second source of truth that a
//! copy, a backup or a different machine would lose.

use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId};
use chrono::Utc;

/// Why an archive state could not be written.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
}

/// Whether the session is currently archived.
///
/// The latest of the two events decides; a session that never recorded one is
/// not archived. Unarchiving is its own event rather than the absence of an
/// archive event, so the log still says the session was put away once — which
/// is what "this was archived for a while" means.
pub fn current(events: &[StoredEvent]) -> bool {
    for stored in events.iter().rev() {
        match stored.event.event_type.as_str() {
            EventType::SESSION_ARCHIVED => return true,
            EventType::SESSION_UNARCHIVED => return false,
            _ => {}
        }
    }
    false
}

/// Put the session away, or take it back out.
///
/// The log's end is read and appended to under the session's lease, so a write
/// from another process (a CLI turn, a second page) cannot move the sequence in
/// between and make this append land on a stale number.
pub async fn set(
    store: Arc<dyn EventStore>,
    session: &SessionId,
    archived: bool,
    actor: Actor,
) -> Result<(), ArchiveError> {
    let _lease = store.lease(session).await?;
    let events = store.read_stream(session, EventSeq::ZERO, usize::MAX).await?;
    let next = events.last().map(|event| event.seq.next()).unwrap_or(EventSeq::ZERO);
    let event_type =
        if archived { EventType::SESSION_ARCHIVED } else { EventType::SESSION_UNARCHIVED };
    let draft = EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({ "archived": archived }),
        metadata: serde_json::json!({}),
        ignorable: false,
    };
    store.append(session, next, vec![draft]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_testkit::FakeEventStore;

    fn stored(seq: u64, event_type: &str) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::current(),
                session_id: SessionId::from_string("s"),
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
            },
        }
    }

    #[test]
    fn a_session_that_was_never_archived_is_listed() {
        let events = vec![stored(0, EventType::SESSION_STARTED)];
        assert!(!current(&events));
    }

    #[test]
    fn the_latest_of_the_two_events_decides() {
        let archived = vec![
            stored(0, EventType::SESSION_STARTED),
            stored(1, EventType::SESSION_ARCHIVED),
            stored(2, EventType::SESSION_UNARCHIVED),
            stored(3, EventType::SESSION_ARCHIVED),
        ];
        assert!(current(&archived));
        let restored = vec![
            stored(0, EventType::SESSION_ARCHIVED),
            stored(1, EventType::SESSION_STARTED),
            stored(2, EventType::SESSION_UNARCHIVED),
        ];
        assert!(!current(&restored));
    }

    #[tokio::test]
    async fn archiving_is_written_to_the_session_log() {
        // The store refuses an append that arrives without a lease, so this is
        // also the check that the read and the append are one step. (YA-36)
        let store: Arc<dyn EventStore> = Arc::new(FakeEventStore::new().requiring_lease());
        let session = SessionId::from_string("s");

        set(store.clone(), &session, true, Actor::User).await.expect("writes");
        set(store.clone(), &session, false, Actor::User).await.expect("writes");

        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.expect("reads");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.event_type.as_str(), EventType::SESSION_ARCHIVED);
        assert_eq!(events[1].event.event_type.as_str(), EventType::SESSION_UNARCHIVED);
        assert!(!current(&events));
    }
}
