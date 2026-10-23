//! Minimal plan fold for the session's ledger.
//!
//! The kernel cannot depend on agent-projection, so the Scheduler reads the
//! plan straight from the event stream here, the way it reads the lifecycle.
//! The rule is the ledger's whole semantics: each `plan/updated` is a full
//! replacement, so the latest one is the current plan and the earlier ones are
//! history.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::plan::Plan;

/// The plan the session currently carries, if any.
///
/// A malformed payload is an error rather than "no plan": the event was
/// written by this build, so a payload that will not read back is a log
/// anomaly, and silently opening the next turn with no plan would hide it.
pub fn current(events: &[StoredEvent]) -> Result<Option<Plan>, serde_json::Error> {
    for stored in events.iter().rev() {
        if stored.event.event_type.as_str() != EventType::PLAN_UPDATED {
            continue;
        }
        let plan =
            serde_json::from_value(stored.event.payload.get("plan").cloned().unwrap_or_default())?;
        return Ok(Some(plan));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft, EventType};
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

    fn plan_event(seq: u64, goal: &str) -> StoredEvent {
        stored(
            seq,
            EventType::PLAN_UPDATED,
            serde_json::json!({ "provider_tool_call_id": "call-1", "plan": { "goal": goal } }),
        )
    }

    #[test]
    fn no_plan_event_means_no_plan() {
        let events = vec![stored(1, EventType::TURN_STARTED, serde_json::json!({}))];
        assert_eq!(current(&events).expect("readable"), None);
    }

    #[test]
    fn the_latest_plan_replaces_the_previous_one() {
        let events = vec![plan_event(1, "first"), plan_event(2, "second")];
        let plan = current(&events).expect("readable").expect("a plan");
        assert_eq!(plan.goal, "second");
        assert!(plan.items.is_empty(), "no items recorded");
    }

    #[test]
    fn a_plan_event_without_a_readable_plan_is_an_error() {
        let broken = stored(1, EventType::PLAN_UPDATED, serde_json::json!({}));
        assert!(current(&[broken]).is_err());
    }
}
