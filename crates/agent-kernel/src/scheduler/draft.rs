//! The shape of one event the Scheduler writes.
//!
//! It lives beside the journal and the endings because all three are about the
//! same act — a turn writing a fact — and because the module that builds the
//! events had grown past what one file can hold. Nothing here decides
//! anything: every field comes from the caller.

use chrono::Utc;

use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, RunId, SchemaVersion, SessionId, StepId, TurnId};

pub(crate) fn draft(
    session_id: &SessionId,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
    run_id: Option<&RunId>,
    turn_id: Option<&TurnId>,
    step_id: Option<&StepId>,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session_id.clone(),
        run_id: run_id.cloned(),
        turn_id: turn_id.cloned(),
        step_id: step_id.cloned(),
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}
