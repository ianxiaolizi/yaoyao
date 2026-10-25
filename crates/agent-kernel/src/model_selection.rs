//! The model a session is set to use, recorded in the session's own log.
//!
//! A choice is a decision about the next turn, not a fact about a past one — but
//! it belongs in the log all the same. A fork copies the log and so inherits the
//! choice without a second copy; every entry point reads the same answer; and
//! the run that follows still records the route it actually used in
//! `profile/resolved`, so choosing a model never rewrites what an earlier run
//! said.
//!
//! What is stored is the two knobs the host builds a provider from — its name
//! and, optionally, a model — not the route that came out. A route carries the
//! adapter's own provider id (`openai-compat`), which is not something the host
//! can build from; storing it would make the event look like a decision while
//! being unusable as one.
//!
//! The kernel cannot depend on the projection, so the fold lives here next to
//! the writer, the way the plan ledger's does.

use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId};
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Which provider to build, and which model to ask it for.
///
/// `model` is optional because naming a provider is a complete choice: the
/// adapter's configured default is the answer to "which model" when nobody says
/// otherwise. For the echo provider the name is still recorded, because it is
/// what the page showed and a log that dropped it would make the choice
/// unreadable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ModelChoice {
    pub fn new(provider: impl Into<String>, model: Option<String>) -> Self {
        Self { provider: provider.into(), model }
    }
}

/// Why a selection could not be written.
#[derive(Debug, thiserror::Error)]
pub enum ModelSelectionError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("model selection payload error: {0}")]
    Payload(#[from] serde_json::Error),
}

/// The model this session is set to use, if it ever named one.
///
/// A malformed payload is an error rather than "no choice": this build wrote
/// the event, so a payload that will not read back is a log anomaly, and
/// quietly running the next turn on the default model would hide it.
pub fn current(events: &[StoredEvent]) -> Result<Option<ModelChoice>, serde_json::Error> {
    for stored in events.iter().rev() {
        if stored.event.event_type.as_str() != EventType::SESSION_MODEL_SELECTED {
            continue;
        }
        let choice = serde_json::from_value(stored.event.payload.clone())?;
        return Ok(Some(choice));
    }
    Ok(None)
}

/// Record the choice this session's next turns should use.
///
/// The log's end is read and appended to under the session's lease, so a write
/// from another process (a CLI turn, a second page) cannot move the sequence in
/// between and make this append land on a stale number.
pub async fn select(
    store: Arc<dyn EventStore>,
    session: &SessionId,
    choice: ModelChoice,
    actor: Actor,
) -> Result<(), ModelSelectionError> {
    let _lease = store.lease(session).await?;
    let events = store.read_stream(session, EventSeq::ZERO, usize::MAX).await?;
    let next = events.last().map(|event| event.seq.next()).unwrap_or(EventSeq::ZERO);
    let draft = EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(EventType::SESSION_MODEL_SELECTED),
        schema_version: SchemaVersion::current(),
        session_id: session.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::to_value(&choice)?,
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

    fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
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
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    #[test]
    fn the_latest_selection_wins() {
        let events = vec![
            stored(0, EventType::SESSION_MODEL_SELECTED, serde_json::json!({ "provider": "echo" })),
            stored(1, EventType::PROFILE_RESOLVED, serde_json::json!({})),
            stored(
                2,
                EventType::SESSION_MODEL_SELECTED,
                serde_json::json!({ "provider": "openai", "model": "gpt-x" }),
            ),
        ];
        let current = current(&events).expect("folds").expect("a selection");
        assert_eq!(current, ModelChoice::new("openai", Some("gpt-x".to_string())));
    }

    #[test]
    fn a_session_that_never_chose_reports_none() {
        let events = vec![stored(0, EventType::PROFILE_RESOLVED, serde_json::json!({}))];
        assert!(current(&events).expect("folds").is_none());
    }

    #[test]
    fn a_malformed_payload_is_an_error_not_a_default() {
        let events = vec![stored(0, EventType::SESSION_MODEL_SELECTED, serde_json::json!({}))];
        assert!(current(&events).is_err(), "an empty payload names no provider");
    }

    #[tokio::test]
    async fn a_selection_is_written_to_the_session_log() {
        // The store refuses an append that arrives without a lease, so this is
        // also the check that the read and the append are one step. (YA-36)
        let store: Arc<dyn EventStore> = Arc::new(FakeEventStore::new().requiring_lease());
        let session = SessionId::from_string("s");

        select(
            store.clone(),
            &session,
            ModelChoice::new("openai", Some("gpt-x".to_string())),
            Actor::User,
        )
        .await
        .expect("writes");

        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.expect("reads");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.event_type.as_str(), EventType::SESSION_MODEL_SELECTED);
        let choice = current(&events).expect("folds").expect("a selection");
        assert_eq!(choice.model.as_deref(), Some("gpt-x"));
    }
}
