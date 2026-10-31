//! The shared ending of every loop-owned call.
//!
//! The loop answers a few tools itself — `plan.update`, `state.update`, the
//! judgment record. Whatever the decision, the record it leaves is the same
//! shape: a result event, an observation in the chain that cites it, and the
//! tool message the next step reads. A ledger write with no recorded result
//! would be an action a reader could not follow to its outcome, so every path
//! through these branches writes all three, here rather than three times over.

use super::*;

/// The answer one loop-owned call gets, ready to be recorded.
pub(super) struct LoopCallAnswer<'a> {
    pub(super) call: &'a ToolCall,
    pub(super) action_id: &'a str,
    pub(super) status: ToolStatus,
    pub(super) event_type: &'static str,
    pub(super) content: String,
}

/// Record the outcome of a loop-owned call: its result event, the observation
/// that cites it, and the message the next step reads.
pub(super) fn answer_loop_call(
    turn: &mut LoopTurn,
    ctx: &TurnContext,
    step_id: &StepId,
    step_index: u32,
    answer: LoopCallAnswer<'_>,
    events: &mut Vec<EventDraft>,
) {
    let result_event = draft(
        ctx,
        step_id,
        answer.event_type,
        serde_json::json!({
            "tool_name": answer.call.tool_name,
            "provider_tool_call_id": answer.call.tool_call_id,
            "status": answer.status,
            "content": &answer.content,
        }),
        Actor::Tool,
    );
    let result_event_id = result_event.event_id.as_str().to_string();
    events.push(result_event);
    let observation = turn.reason.node(
        ReasonKind::Observation,
        step_index,
        format!("{} {}: {}", answer.call.tool_name, status_word(answer.status), answer.content),
        vec![Basis::node(answer.action_id.to_string()), Basis::event(result_event_id)],
        "observation/result",
        ReasonSource::World,
    );
    turn.reason_last_observation = Some(observation.id.clone());
    events.push(reason_event(ctx, Some(step_id), &observation));
    turn.messages.push(ModelMessage::tool_for_call(
        step_id,
        answer.call.tool_call_id.clone(),
        answer.call.tool_name.clone(),
        answer.content,
        answer.status != ToolStatus::Completed,
    ));
}
