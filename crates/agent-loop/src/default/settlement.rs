//! Settlement: replacing a turn's older steps with the state that stands for
//! them, without asking the model to retell them.
//!
//! A summarizer is a second model call that can drop the goal, misquote the
//! user, or repeat a failure it was never told about — the ledger, the user's
//! words and the failures record exist because live runs showed exactly that.
//! When the run has kept a work state (the model's own `state/1` account plus
//! the loop's objective facts), that record already says where the work stands.
//! Settlement is the cheaper answer: the older steps leave the window, one
//! record of them takes their place, and no provider is asked anything.
//!
//! The transcript keeps its shape. The event is still `context/compacted`, the
//! message that stands in for the removed steps is still the one the fold
//! rebuilds from `attempt_id`, and a failure to settle falls back to the
//! summarizer rather than to nothing.

use super::settle::DEFAULT_SETTLE_PERCENT;
use super::*;
use agent_protocol::state::WorkState;

/// The schema name this strategy writes.
const SCOPE: &str = "state/1";

/// The share of the usable window a settlement keeps as recent traffic.
///
/// Lower than the summarizer's keep share: a settlement is written from the
/// state rather than read from the removed text, so it needs less carried
/// history to be safe, and the point of settling is to leave the next request
/// well under the window.
const SETTLE_KEEP_PERCENT: usize = 40;

/// Whether this step's request has reached the point where its traffic is
/// settled, and which measure said so.
///
/// The estimate fires first because it is available before the request leaves;
/// the provider's own count for the previous request is the backstop, since it
/// is the only measure that cannot be wrong about what the window really held.
pub(super) fn trigger(turn: &LoopTurn, estimated_request_tokens: usize) -> Option<&'static str> {
    let working_set = u64::from(super::settle::working_set(turn)?);
    if turn.observed_input_tokens.is_some_and(|observed| observed > working_set) {
        return Some("observed_input");
    }
    let budget = working_set;
    if budget == 0 {
        return None;
    }
    let percent = turn.budget.settle_at_percent.unwrap_or(DEFAULT_SETTLE_PERCENT) as u64;
    (estimated_request_tokens as u64 >= budget * percent / 100).then_some("estimated_request")
}

/// Replace this turn's older traffic with a settlement record.
///
/// Returns `false` when there was nothing to settle — no window, no state to
/// write it from, or only the newest group of traffic — and the caller falls
/// back to the summarizer.
///
/// The user's input stays verbatim, and so does every user message: a whole
/// group that carries one is kept rather than split, the same rule
/// `in_flight_candidate` applies. A call and its answer share one group, so
/// dropping none of one and all of the other is a property of the shape.
pub(super) fn settle(
    turn: &mut LoopTurn,
    step_id: &StepId,
    trigger: &'static str,
    events: &mut Vec<EventDraft>,
) -> Result<bool, AgentLoopError> {
    let Some(work) = turn.work.clone() else {
        return Ok(false);
    };
    let Some(budget) = super::settle::working_set(turn) else {
        return Ok(false);
    };
    let budget = budget as usize;
    if budget == 0 {
        return Ok(false);
    }
    let input_uid = format!("msg/user/{}", turn.ctx.turn_id);
    let Some(input) = turn.messages.iter().position(|m| m.message_uid.as_str() == input_uid) else {
        return Ok(false);
    };
    let traffic = turn.messages[input + 1..].to_vec();
    let groups = agent_protocol::compaction::groups(&traffic);
    let keep_target = budget.saturating_mul(SETTLE_KEEP_PERCENT) / 100;
    let Some((keep_from, _)) =
        agent_protocol::compaction::keep_from(&traffic, &groups, 0, keep_target)
    else {
        return Ok(false);
    };

    // A group that carries user text is kept whole: the input beside it stays
    // verbatim, and so does anything the user said mid-turn.
    let mut dropped: Vec<String> = Vec::new();
    for group in &groups[..keep_from] {
        let slice = &traffic[group.start..=group.end];
        if slice.iter().any(|message| message.role == MessageRole::User) {
            continue;
        }
        dropped.extend(slice.iter().map(|message| message.message_uid.as_str().to_string()));
    }
    if dropped.is_empty() {
        return Ok(false);
    }

    let estimated_before = budget::estimated_request_tokens(&turn.messages, &turn.manifest) as u64;
    let refs: Vec<serde_json::Value> = traffic
        .iter()
        .filter(|message| dropped.iter().any(|uid| uid == message.message_uid.as_str()))
        .map(|message| {
            serde_json::json!({
                "message_uid": message.message_uid.as_str(),
                "role": message.role.as_str(),
                "hash": digest::json(&serde_json::to_value(message).unwrap_or_default()),
            })
        })
        .collect();

    let ledger = Ledger::build(&traffic, &turn.write_tools);
    let subjective = !work.is_empty();
    let text = record_text(turn, &work, &ledger, dropped.len());
    let attempt_id = AttemptId::new();
    let insert_at = turn
        .messages
        .iter()
        .position(|message| dropped.iter().any(|uid| uid == message.message_uid.as_str()))
        .unwrap_or(input + 1);
    turn.messages.retain(|message| !dropped.iter().any(|uid| uid == message.message_uid.as_str()));
    let insert_at = insert_at.min(turn.messages.len());
    turn.messages.insert(insert_at, ModelMessage::summary_for(&attempt_id, text.clone()));

    let estimated_after = budget::estimated_request_tokens(&turn.messages, &turn.manifest) as u64;
    let state_hash = digest::json(&serde_json::to_value(&work)?);
    events.push(draft(
        &turn.ctx,
        step_id,
        EventType::CONTEXT_COMPACTED,
        serde_json::json!({
            "strategy": "state/1",
            "trigger": trigger,
            "attempt_id": attempt_id,
            "summary_message_uid": format!("msg/summary/{attempt_id}"),
            "summary": text,
            "summarized": refs,
            "summarized_hash": digest::json(&serde_json::Value::Array(refs.clone())),
            "context_revision": turn.context_revision.as_u64(),
            "message_count": dropped.len(),
            "removed_messages": dropped.len(),
            "dropped_messages": dropped.len(),
            "kept_groups": groups.len().saturating_sub(keep_from),
            "estimated_before": estimated_before,
            "estimated_after": estimated_after,
            "estimated_tokens": estimated_before,
            "retained_tokens": estimated_after,
            "budget_tokens": budget,
            "observed_input_tokens": turn.observed_input_tokens,
            "scope": "in-flight",
            "state_hash": state_hash,
            "subjective": if subjective { "present" } else { "none" },
            "fallback": "none",
            "truncated": false,
        }),
        Actor::Agent,
    ));
    Ok(true)
}

/// What the settlement message says.
///
/// The objective half is the same one the tail block shows, so the record and
/// the block cannot disagree about the numbers. The ledger beside it is the
/// deterministic account of what was removed, and the closing line says what
/// the state cannot: the traffic is still in the log and can be read again.
fn record_text(turn: &LoopTurn, work: &WorkState, ledger: &Ledger, dropped: usize) -> String {
    let mut text = format!(
        "Settlement ({SCOPE}): {dropped} message(s) of this turn's older tool traffic were \
         replaced by this record. It was written from the loop's own facts and the work state \
         the model recorded — no summarizer ran. The user's input above is verbatim.\n",
    );
    text.push_str(&work_state::block(turn, work));
    let ledger_text = ledger.render();
    if !ledger.is_empty() {
        text.push('\n');
        text.push_str(&ledger_text);
    }
    text.push_str(
        "\nAnything this record does not carry can be read again with the tools; the removed \
         messages are still in the event log.",
    );
    text
}
