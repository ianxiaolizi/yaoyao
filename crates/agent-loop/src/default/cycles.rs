//! The record of a call that repeated itself.
//!
//! A third byte-identical call is a cycle, not a retry: after two runs the world
//! has already answered. Two places write the record — the repeat gate, when it
//! holds the call before it runs, and the dispatch path, when the run happens —
//! because what matters is that the run went in circles, not whether the third
//! call was executed. Exactly one is written per streak: the counter keeps
//! growing, so only the step that reached three sees the condition, and the gate
//! marks the streak it already recorded.
//!
//! Kept out of `default.rs`: the loop is at its line ratchet, and a signature
//! comparison is not step machinery.

use agent_ports::agent_loop::TurnContext;
use agent_protocol::events::EventDraft;
use agent_protocol::ids::StepId;

use super::*;

/// Fold one executed call into the streak, and record the cycle when this run
/// is the third byte-identical one.
///
/// A call that ran is the one a fourth would repeat; a different signature
/// starts a new streak and re-arms the gate's one refusal.
pub(super) fn note_run(
    turn: &mut LoopTurn,
    ctx: &TurnContext,
    step_id: &StepId,
    call: &ToolCall,
) -> Option<EventDraft> {
    let signature = call_signature(&call.tool_name, &call.arguments);
    if turn.last_signature.as_deref() == Some(signature.as_str()) {
        turn.identical_streak += 1;
    } else {
        turn.last_signature = Some(signature);
        turn.identical_streak = 1;
        turn.repeat_refused = false;
    }
    (turn.identical_streak == 3 && !turn.repeat_refused)
        .then(|| record(ctx, step_id, call, turn.identical_streak, false))
}

/// The record for a streak the repeat gate held before the third call ran.
pub(super) fn held(
    ctx: &TurnContext,
    step_id: &StepId,
    call: &ToolCall,
    repeats: u32,
) -> EventDraft {
    record(ctx, step_id, call, repeats, true)
}

fn record(
    ctx: &TurnContext,
    step_id: &StepId,
    call: &ToolCall,
    repeats: u32,
    held: bool,
) -> EventDraft {
    draft(
        ctx,
        step_id,
        EventType::TOOL_DUPLICATE_CYCLE,
        serde_json::json!({
            "tool_name": call.tool_name,
            "provider_tool_call_id": call.tool_call_id,
            "repeats": repeats,
            "held": held,
        }),
        Actor::Agent,
    )
}
