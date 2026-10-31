//! The block that tells a long run where it stands, placed at the tail of
//! every request.
//!
//! Two halves, kept apart on purpose. The objective half is the loop's: which
//! step this is, what has been spent, how many calls were made, what last
//! failed. The subjective half is the model's own `state/1` account, written
//! through `state.update`. A reader can tell which is which, and the loop never
//! takes the model's word for a fact it can count.
//!
//! It goes last — after this turn's tool traffic, before the budget notice —
//! because a state block above forty tool results is history, not a statement
//! of where the work stands. Like the budget notice it is replaced each step
//! rather than repeated, so the request carries one block and the cached
//! prefix above the traffic is untouched.

use super::loop_call::{LoopCallAnswer, answer_loop_call};
use super::*;
use agent_protocol::state;

/// Place this step's work-state block at the tail, and return its text for the
/// prepared-request record.
///
/// `None` when this turn has no work-state ledger at all (`TurnContext.work`
/// is `None`): a harness that keeps no record gets the request it had before
/// the block existed, rather than an objective line it never asked for.
pub(super) fn place_work_state(turn: &mut LoopTurn) -> Option<String> {
    let work = turn.work.clone()?;
    let text = block(turn, &work);
    let uid = format!("msg/work-state/{}", turn.ctx.turn_id);
    turn.messages.retain(|message| message.message_uid.as_str() != uid);
    turn.messages.push(ModelMessage::work_state_for_turn(&turn.ctx.turn_id, text.clone()));
    Some(text)
}

/// What the block says, from the numbers this step already holds.
pub(super) fn block(turn: &LoopTurn, work: &agent_protocol::state::WorkState) -> String {
    let mut objective = Vec::new();
    if let Some(facts) = turn.ctx.state.as_ref() {
        objective.push(format!("step {} of {}", turn.step_index, facts.max_steps));
    }
    objective.push(format!("{} tool call(s)", turn.tool_calls_made));
    if let Some(max) = turn.budget.max_total_tokens {
        objective.push(format!("{} of {max} tokens", turn.spent.total_tokens()));
    }
    if let Some(deadline) = turn.deadline {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        objective.push(format!("{}m left", left.as_secs() / 60));
    }
    let mut text = format!("Objective: {}.", objective.join("; "));
    if let Some(failure) = &turn.pending_failure {
        text.push_str(&format!(
            "\nLast failure: `{}` ({}).",
            failure.tool,
            truncate(&failure.detail, 120)
        ));
    }
    text.push('\n');
    if work.is_empty() {
        text.push_str(
            "Work state: the model has not recorded one yet. Record it with `state.update` — \
             the goal, the next action, what is done with its evidence, and what is blocked.",
        );
    } else {
        // The standalone header says "Work state"; under the objective line it
        // would repeat the first word of the block, so the body is used as it
        // renders minus that header.
        text.push_str(&work.render().replacen(
            "Work state (the model's own account):",
            "Account:",
            1,
        ));
    }
    text
}

/// Record one accepted `state.update`: the event payload, and the refresh of
/// the block the next step reads.
///
/// The render is refreshed here rather than next step so a model that writes
/// its state mid-turn reads the recorded version back before it acts again —
/// what it sees and what the log holds stay the same words.
pub(super) fn record(turn: &mut LoopTurn, state: agent_protocol::state::WorkState) -> usize {
    turn.work = Some(state.clone());
    place_work_state(turn);
    state.done.len()
}

/// Answer one `state.update` call: record it when it reads, refuse it when it
/// does not.
///
/// A refused state records no event — the model gets the reason and may try
/// again, and a ledger that never held the bad version leaves nothing for the
/// next turn to fold. An accepted one is stored scrubbed, because it is
/// model-authored text that outlives the run; the model is handed the same
/// scrubbed state, so the block it reads through `place_work_state` is the one
/// the log holds.
#[allow(clippy::too_many_arguments)]
pub(super) fn answer_update(
    turn: &mut LoopTurn,
    ctx: &TurnContext,
    step_id: &StepId,
    step_index: u32,
    call: &ToolCall,
    action_id: &str,
    redactor: &Redactor,
    events: &mut Vec<EventDraft>,
) -> Result<(), AgentLoopError> {
    let (status, event_type, content) = match state::WorkState::from_arguments(&call.arguments) {
        Ok(work) => {
            let (value, redaction) = redactor.redact_value(&serde_json::to_value(&work)?);
            let work: state::WorkState = serde_json::from_value(value)?;
            let mut payload = serde_json::json!({
                "provider_tool_call_id": call.tool_call_id,
                "schema": state::SCHEMA,
                "state": &work,
            });
            if redaction.redacted || !redaction.warnings.is_empty() {
                payload["redaction"] = serde_json::json!({
                    "redacted": redaction.redacted,
                    "patterns": redaction.patterns,
                    "warnings": redaction.warnings,
                });
            }
            events.push(draft(ctx, step_id, EventType::STATE_UPDATED, payload, Actor::Agent));
            let done = work.done.len();
            let blocked = work.blocked.is_some();
            record(turn, work);
            let content = format!(
                "state recorded: {done} item(s) done{}",
                if blocked { ", blocked recorded" } else { "" }
            );
            (ToolStatus::Completed, EventType::TOOL_COMPLETED, content)
        }
        Err(error) => {
            (ToolStatus::Failed, EventType::TOOL_FAILED, format!("state not recorded: {error}"))
        }
    };
    answer_loop_call(
        turn,
        ctx,
        step_id,
        step_index,
        LoopCallAnswer { call, action_id, status, event_type, content },
        events,
    );
    Ok(())
}
