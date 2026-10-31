//! Minimal work-state fold for the run's own account of where it stands.
//!
//! The kernel cannot depend on agent-projection, so the Scheduler reads the
//! state straight from the event stream here, the way it reads the plan. Each
//! `state/updated` is a full replacement — the loop writes the whole state the
//! model sent — so the latest one is the current state and the earlier ones
//! are history.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::state::WorkState;

/// The work state the session currently carries, if the model ever recorded
/// one.
///
/// A malformed payload is an error rather than "no state", for the same reason
/// the plan fold is: the event was written by this build, and a payload that
/// will not read back is a log anomaly — opening the next turn as if the model
/// had said nothing would hide it.
pub fn current(events: &[StoredEvent]) -> Result<Option<WorkState>, serde_json::Error> {
    for stored in events.iter().rev() {
        if stored.event.event_type.as_str() != EventType::STATE_UPDATED {
            continue;
        }
        let state =
            serde_json::from_value(stored.event.payload.get("state").cloned().unwrap_or_default())?;
        return Ok(Some(state));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId};
    use chrono::Utc;

    fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: agent_protocol::ids::EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("s"),
                run_id: None,
                turn_id: None,
                step_id: None,
                actor: Actor::Agent,
                created_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    fn recorded(seq: u64, goal: &str) -> StoredEvent {
        stored(
            seq,
            EventType::STATE_UPDATED,
            serde_json::json!({
                "state": { "goal": goal, "next": "keep going" },
            }),
        )
    }

    #[test]
    fn the_latest_state_is_what_a_session_carries() {
        let events = [recorded(1, "first"), recorded(2, "second")];

        let state = current(&events).expect("a readable log").expect("a state");
        assert_eq!(state.goal, "second");
    }

    #[test]
    fn a_session_that_never_recorded_a_state_carries_none() {
        let events = [stored(1, EventType::PLAN_UPDATED, serde_json::json!({"plan": {}}))];
        assert_eq!(current(&events).expect("a readable log"), None);
    }

    #[test]
    fn a_payload_that_will_not_read_back_is_an_error() {
        let events =
            [stored(1, EventType::STATE_UPDATED, serde_json::json!({ "state": { "goal": 7 } }))];
        assert!(current(&events).is_err());
    }
}
