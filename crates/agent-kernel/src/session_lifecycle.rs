//! Starting and deleting a session — the lifecycle changes that are neither
//! "hide it" ([`crate::session_archive`]) nor "continue it" (the Scheduler).
//!
//! Both are facts about the log. A session exists because its log does, and it
//! has started because its log says so; neither is recorded anywhere else, so a
//! copy, a backup or a second machine reconstructs the same answer. Deleting is
//! the one operation that destroys facts, and it refuses while somebody is
//! driving the session rather than removing the log under a running turn.
//!
//! A forked session needs neither: the child inherits its parent's prefix,
//! which already carries a `session/started`, and its own first event says what
//! it is ([`crate::fork`]).

use std::sync::Arc;

use agent_ports::event_store::{DriverRecord, EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId};
use chrono::Utc;

/// Why a lifecycle change could not be made.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    /// Another process holds the session's driver claim and is running a turn.
    /// Not folded into `Store` because it has its own remedy — wait for the turn
    /// or stop it — and because a caller deciding between "retry" and "fix the
    /// store" cannot read that out of an IO error.
    #[error("session {session_id} is already being driven ({})", describe(holder))]
    Busy { session_id: SessionId, holder: Option<DriverRecord> },
}

/// The parenthetical in `Busy`, in the same words the scheduler's refusal uses.
fn describe(holder: &Option<DriverRecord>) -> String {
    match holder {
        Some(record) => record.to_string(),
        None => "holder did not record itself".to_string(),
    }
}

/// What deleting a session did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Its log was there and is gone.
    Removed,
    /// There was nothing to delete. Not an error: the caller asked for it to be
    /// gone, and it is.
    Absent,
}

/// The `session/started` draft, so every writer of it says the same thing.
pub fn started_draft(session_id: &SessionId, actor: Actor) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(EventType::SESSION_STARTED),
        schema_version: SchemaVersion::current(),
        session_id: session_id.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

/// Record that the session has started, unless the log already says so.
///
/// Returns whether this call wrote the event. The decision is made by reading
/// the log under the session's lease and appending at its end, so two callers
/// racing here produce one `session/started`; a session that already has one —
/// including one created before this build wrote them — is left alone.
pub async fn ensure_started(
    store: &Arc<dyn EventStore>,
    session_id: &SessionId,
    actor: Actor,
) -> Result<bool, LifecycleError> {
    let _lease = store.lease(session_id).await?;
    let events = store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
    if has_started(&events) {
        return Ok(false);
    }
    let next = events.last().map(|event| event.seq.next()).unwrap_or(EventSeq::ZERO);
    store.append(session_id, next, vec![started_draft(session_id, actor)]).await?;
    Ok(true)
}

/// Whether any event in `events` says the session started.
pub fn has_started(events: &[agent_protocol::events::StoredEvent]) -> bool {
    events.iter().any(|stored| stored.event.event_type.as_str() == EventType::SESSION_STARTED)
}

/// Delete the session's log, refusing while another process drives it.
///
/// The refusal is the whole point of this function existing next to
/// [`EventStore::delete_session`]: that call cannot see a turn running in
/// another process, and removing the log under one leaves the turn appending to
/// a session nobody can see. The claim is asked for and released before the
/// removal — the store's own claim files live inside the directory being
/// removed, and an open handle would make the removal fail on Windows — so what
/// is closed here is the window a whole turn would occupy, not the microseconds
/// between the check and the removal.
pub async fn delete(
    store: &Arc<dyn EventStore>,
    session_id: &SessionId,
) -> Result<DeleteOutcome, LifecycleError> {
    let claim = match store.driver_lease(session_id).await {
        Ok(claim) => claim,
        Err(EventStoreError::ConcurrentDriver { session_id, holder }) => {
            return Err(LifecycleError::Busy { session_id, holder });
        }
        Err(error) => return Err(error.into()),
    };
    let events = store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
    let outcome = if events.is_empty() { DeleteOutcome::Absent } else { DeleteOutcome::Removed };
    // Released before the directory goes: see this function's own note.
    drop(claim);
    store.delete_session(session_id).await?;
    Ok(outcome)
}
