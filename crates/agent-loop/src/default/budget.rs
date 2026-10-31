//! What a turn has spent of its run budget, and what it is told about that.
//!
//! The accounting is kept beside the step machinery rather than inside it: it
//! is arithmetic over numbers the step already holds, and a reader looking for
//! "what did this cost" should not have to read the request's construction to
//! find it. Nothing here decides to stop a turn. A spent budget is reported to
//! the caller, whose step boundary ends the run (`default.rs`'s module doc).

use agent_protocol::ids::TurnId;
use agent_protocol::model::estimate_block;

use crate::context_layers::BudgetNotice;

use super::*;

impl LoopTurn {
    /// Whether the run has spent the token budget it was configured with.
    ///
    /// Raw `input + output`, compared against `max_total_tokens`. Cache hits
    /// count at face value: the enforced limit is the operator's number, not a
    /// discounted view of it (see [`Spent::billable_tokens`]).
    pub(super) fn token_budget_spent(&self) -> bool {
        self.budget.max_total_tokens.is_some_and(|max| self.spent.total_tokens() >= max)
    }

    /// The largest share of any primary limit (tokens or wall clock) used, as a
    /// percentage. Zero when neither is configured.
    pub(super) fn budget_used_percent(&self) -> u32 {
        let mut used = 0u32;
        if let Some(max) = self.budget.max_total_tokens.filter(|max| *max > 0) {
            let percent = self.spent.total_tokens().saturating_mul(100) / max;
            used = used.max(u32::try_from(percent.min(100)).unwrap_or(100));
        }
        if let (Some(deadline), Some(total)) =
            (self.deadline, self.budget.wall_clock_ms.filter(|total| *total > 0))
        {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let elapsed = total.saturating_sub(left.as_millis() as u64);
            let percent = elapsed.saturating_mul(100) / total;
            used = used.max(u32::try_from(percent.min(100)).unwrap_or(100));
        }
        used
    }

    /// Which phase this run is in, by budget alone.
    ///
    /// `finalize` means the next request should ask the model to stop opening
    /// new lines of work and report what it has. It is advice, not a stop: the
    /// turn ends only when a limit is actually reached.
    pub(super) fn budget_phase(&self) -> &'static str {
        let threshold = self.budget.finalize_at_percent.unwrap_or(85);
        if self.budget_used_percent() >= threshold { "finalize" } else { "normal" }
    }
}

/// What a response with no reported usage plausibly cost in output tokens.
///
/// The same estimator the context engine uses, over every block the response
/// carries — prose, tool names, and arguments. It is a fallback only; a
/// response whose provider reported a count is never estimated.
pub(super) fn estimate_response_tokens(response: &ModelResponse) -> u64 {
    u64::try_from(response.content_blocks.iter().map(estimate_block).sum::<usize>())
        .unwrap_or(u64::MAX)
}

/// Add what one response cost to the turn's running total.
///
/// A provider that reports usage is believed. One that does not is estimated
/// from the request that produced the response and the response's own blocks,
/// and the total is flagged: an unreported count must not read as free, or a
/// run would outlive the budget its operator configured.
pub(super) fn record_spend(
    turn: &mut LoopTurn,
    response: &ModelResponse,
    estimated_request_tokens: u64,
) {
    match &response.usage {
        Some(usage) if usage.input_tokens.is_some() && usage.output_tokens.is_some() => {
            turn.spent.add(
                usage.input_tokens.unwrap_or(0),
                usage.output_tokens.unwrap_or(0),
                usage.cache_hit_tokens.unwrap_or(0),
            );
        }
        usage => {
            let reported_input = usage
                .as_ref()
                .and_then(|usage| usage.input_tokens)
                .unwrap_or(estimated_request_tokens);
            let reported_output = usage
                .as_ref()
                .and_then(|usage| usage.output_tokens)
                .unwrap_or_else(|| estimate_response_tokens(response));
            let cache_hit = usage.as_ref().and_then(|usage| usage.cache_hit_tokens).unwrap_or(0);
            turn.spent.add(reported_input, reported_output, cache_hit);
            turn.spent.estimated = true;
        }
    }
}

/// Place the wrap-up reminder this request carries, if the run has reached the
/// finalize phase, and return it for the prepared-request record.
///
/// It goes last, after any compaction and after this turn's own traffic: it is
/// a wrap-up instruction, and an instruction buried above a large tool result
/// reads as history. The uid is the turn's, so replacing it each step keeps one
/// reminder in the request rather than a growing pile of them.
pub(super) fn place_finalize_reminder(
    turn: &mut LoopTurn,
    turn_id: &TurnId,
) -> Option<BudgetNotice> {
    let phase = turn.budget_phase();
    if phase != "finalize" {
        return None;
    }
    let notice = BudgetNotice::new(phase, turn.budget_used_percent(), turn.budget.max_total_tokens);
    let uid = format!("msg/budget/{turn_id}");
    let message = ModelMessage::budget_notice_for_turn(turn_id, notice.text.clone());
    match turn.messages.iter_mut().find(|message| message.message_uid.as_str() == uid) {
        Some(existing) => *existing = message,
        None => turn.messages.push(message),
    }
    Some(notice)
}
