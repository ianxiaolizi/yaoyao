//! Building the drafts the loop writes, with the right scope on each one.
//!
//! Kept beside `default.rs` rather than in it: the loop is already at its line
//! ratchet, and an event's scope is a fact about the event, not about the step
//! that happens to be writing it.

use agent_ports::agent_loop::TurnContext;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, SchemaVersion, StepId};
use agent_protocol::reason::ReasonNode;
use chrono::Utc;

pub(super) fn draft(
    ctx: &TurnContext,
    step_id: &StepId,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    draft_scoped(ctx, Some(step_id), event_type, payload, actor)
}

/// A turn-level event: it belongs to the turn and to no step.
///
/// Which step was running is a fact worth recording for a step's own events and
/// a lie for a turn's, so the route decision — taken before any step exists —
/// carries no `step_id` rather than borrowing the first one's.
pub(super) fn draft_turn(
    ctx: &TurnContext,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    draft_scoped(ctx, None, event_type, payload, actor)
}

fn draft_scoped(
    ctx: &TurnContext,
    step_id: Option<&StepId>,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: ctx.session_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        turn_id: Some(ctx.turn_id.clone()),
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

/// One node of the turn's reasoning chain.
///
/// Turn-level nodes carry no `step_id`; a step's nodes carry the step they were
/// decided in. The actor is the agent in both cases: the node was written by
/// this loop, whatever the source it records.
pub(super) fn reason_event(
    ctx: &TurnContext,
    step_id: Option<&StepId>,
    node: &ReasonNode,
) -> EventDraft {
    draft_scoped(
        ctx,
        step_id,
        EventType::REASON_RECORDED,
        serde_json::json!({ "node": node }),
        Actor::Agent,
    )
}
