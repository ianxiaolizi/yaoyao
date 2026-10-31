//! What a turn has spent of its run budget, and what it is told about that.
//!
//! The accounting is kept beside the step machinery rather than inside it: it
//! is arithmetic over numbers the step already holds, and a reader looking for
//! "what did this cost" should not have to read the request's construction to
//! find it. Nothing here decides to stop a turn. A spent budget is reported to
//! the caller, whose step boundary ends the run (`default.rs`'s module doc).

use agent_protocol::ids::TurnId;
use agent_protocol::model::{estimate_block, estimate_message, estimate_text};

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

/// What this request will cost the window, on the same estimator the context
/// engine uses.
///
/// It covers the whole request rather than the settled derivation: the stable
/// head, every layer, this turn's input, any in-flight tool traffic, and the
/// tool definitions sent beside the messages. That is the number settlement is
/// triggered on — the settled history alone was measured at 240 tokens while
/// the real request carried 69,575 (2026-10-31).
pub(super) fn estimated_request_tokens(
    messages: &[ModelMessage],
    manifest: &ToolManifest,
) -> usize {
    let messages: usize = messages.iter().map(estimate_message).sum();
    messages + estimated_tool_tokens(manifest)
}

/// What the tool definitions beside a request cost, on the same estimator.
///
/// The wire shape, not the manifest entry: `registration_version`, `source`,
/// and `schema_hash` never reach the provider, and counting them inflated this
/// estimate by roughly 12% (2026-10-19).
pub(super) fn estimated_tool_tokens(manifest: &ToolManifest) -> usize {
    manifest
        .entries
        .iter()
        .map(|entry| {
            let wire = serde_json::json!({
                "type": "function",
                "function": {
                    "name": entry.tool_id,
                    "description": entry.description.trim(),
                    "parameters": entry.input_schema,
                },
            });
            estimate_text(&wire.to_string())
        })
        .sum()
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

/// Place this step's budget reminder at the tail of the request, and return it
/// for the prepared-request record.
///
/// Every request carries one while the run has a limit to report: the numbers
/// are what let the model decide where to spend what is left. A turn with no
/// token budget and no wall clock has nothing to report, and gets none — the
/// request is then exactly what it was before reminders existed.
///
/// It goes last, after any compaction and after this turn's own traffic: a
/// wrap-up instruction buried above a large tool result reads as history. The
/// uid is the turn's, so replacing it each step keeps one reminder in the
/// request rather than a growing pile of them, and the stable prefix the
/// provider's cache hits stays untouched.
pub(super) fn place_budget_notice(turn: &mut LoopTurn, turn_id: &TurnId) -> Option<BudgetNotice> {
    if turn.budget.max_total_tokens.is_none() && turn.budget.wall_clock_ms.is_none() {
        return None;
    }
    let notice = BudgetNotice::new(
        turn.budget_phase(),
        turn.budget_used_percent(),
        turn.spent.total_tokens(),
        turn.budget.max_total_tokens,
        turn.deadline.map(|deadline| {
            deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as u64
        }),
    );
    let uid = format!("msg/budget/{turn_id}");
    let message = ModelMessage::budget_notice_for_turn(turn_id, notice.text.clone());
    // Replaced and re-appended rather than overwritten in place: the reminder
    // from the previous step sat above the traffic that step produced, and a
    // stale position would bury it in exactly the tool results it should be
    // read after.
    turn.messages.retain(|message| message.message_uid.as_str() != uid);
    turn.messages.push(message);
    Some(notice)
}
