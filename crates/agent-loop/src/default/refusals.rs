//! Answering the calls a stopped step never dispatched.
//!
//! A provider response's tool calls each need a tool result in the history that
//! follows; dropping one makes the next request invalid. These functions write
//! the answer a call gets when a gate, a limit, or a capability stop kept it
//! from running.

use agent_ports::agent_loop::{StopReason, TurnContext};
use agent_ports::model_provider::ModelMessage;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::StepId;
use agent_protocol::model::ToolCall;

use super::LoopTurn;
use super::event_drafts::draft;

/// Answer a step's calls as never run.
///
/// `tool/not_started` is the existing word for exactly this: the call provably
/// did not run, so proposing it again is not a retry. The text names the limit,
/// so a reader of the log does not have to infer it from the terminal payload.
pub(super) fn refuse_undispatched(
    ctx: &TurnContext,
    step_id: &StepId,
    turn: &mut LoopTurn,
    events: &mut Vec<EventDraft>,
    calls: &[ToolCall],
    reason: &StopReason,
    output_limit: Option<u32>,
) {
    let content = match reason {
        StopReason::MaxToolCalls => format!(
            "This tool call was never dispatched: the run's tool-call limit ({}) was reached. \
             Nothing ran; propose it again only if the next run still needs it.",
            turn.budget.max_tool_calls.unwrap_or_default()
        ),
        // The response itself was cut, so the call may be incomplete as well as
        // unrun: the answer says both, because retrying a half-written call
        // without changing anything is exactly what the limit would reproduce.
        StopReason::MaxOutputTokens => format!(
            "This tool call was never dispatched: the provider stopped the response at the \
             run's output-token limit ({}), so the calls in it may be incomplete. Nothing ran; \
             raise `max_output_tokens` and propose them again if the work still needs doing.",
            output_limit.unwrap_or_default()
        ),
        StopReason::ContentFiltered => String::from(
            "This tool call was never dispatched: the provider withheld the response under its \
             content filter. Nothing ran; propose it again only if the next run still needs it.",
        ),
        // The calls after a capability stop were never dispatched. The text
        // says why, so a resumed turn does not read "not started" as a
        // transient failure worth retrying before the person has chosen.
        StopReason::CapabilityBlocked => String::from(
            "This tool call was never dispatched: the turn stopped because the backend refused \
             the same missing capability twice. Nothing ran; wait for the user's choice \
             (switch the backend, accept a degraded result, or cancel) before proposing it again.",
        ),
        _ => String::from(
            "This tool call was never dispatched: the run's wall-clock limit was reached. \
             Nothing ran; propose it again only if the next run still needs it.",
        ),
    };
    for call in calls {
        events.push(draft(
            ctx,
            step_id,
            EventType::TOOL_NOT_STARTED,
            serde_json::json!({
                "tool_name": call.tool_name,
                "provider_tool_call_id": call.tool_call_id,
                "content": content,
            }),
            Actor::Tool,
        ));
        turn.messages.push(ModelMessage::tool_for_call(
            step_id,
            call.tool_call_id.clone(),
            call.tool_name.clone(),
            content.clone(),
            true,
        ));
    }
}
