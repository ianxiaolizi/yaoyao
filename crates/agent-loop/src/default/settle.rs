//! When a turn's own traffic is settled, and what is proposed for it.
//!
//! The engine sees settled history and the trimmer sees the settled
//! derivation; neither sees what a turn accumulates through its own tool
//! results. Settlement is the loop's answer to that, and this module holds the
//! trigger: it is fired by the **whole request's** estimated cost, because a
//! request that is about to spend the window is the thing to prevent, not the
//! traffic inside it.

use super::*;

/// The share of the usable window at which this turn's traffic is settled,
/// when the profile did not state one.
///
/// The same default the configuration layer fills in; stated here so a caller
/// that builds a budget by hand (a test, a harness) gets the same shape as a
/// configuration that omitted the field.
const DEFAULT_SETTLE_AT_PERCENT: u32 = 70;

/// This turn's own step traffic, proposed for one of its mid-turn compactions.
///
/// The range starts after this turn's input and covers every assistant/tool
/// message since; user text (the input, any steering note) is never proposed
/// and stays verbatim.
///
/// `estimated_request_tokens` is the whole request this step is about to send,
/// tool definitions included. Triggering on it rather than on the traffic
/// alone is what makes the compaction timely: the old rule required this
/// turn's traffic to reach twice the usable window, and since the request
/// already carries that traffic, the request had always exceeded the window
/// before the rule could fire — at any window size (2026-10-31).
///
/// `None` means the request is still under the settle share, there is nothing
/// older than the newest group to summarize, or no window is configured — in
/// which case the turn keeps the behavior it had before in-flight compaction
/// existed.
pub(super) fn in_flight_candidate(
    turn: &LoopTurn,
    estimated_request_tokens: usize,
) -> Option<CompactionCandidate> {
    let max_input = turn.context_usage.max_input_tokens?;
    let budget =
        max_input.saturating_sub(turn.context_usage.reserved_output_tokens.unwrap_or(0)) as usize;
    if budget == 0 {
        return None;
    }
    let percent = turn.budget.settle_at_percent.unwrap_or(DEFAULT_SETTLE_AT_PERCENT) as usize;
    let threshold = budget.saturating_mul(percent) / 100;
    if estimated_request_tokens < threshold {
        return None;
    }

    let input_uid = format!("msg/user/{}", turn.ctx.turn_id);
    let input =
        turn.messages.iter().position(|message| message.message_uid.as_str() == input_uid)?;
    let traffic = &turn.messages[input + 1..];
    let groups = agent_protocol::compaction::groups(traffic);
    let keep_target =
        budget.saturating_mul(agent_protocol::compaction::COMPACTION_KEEP_PERCENT as usize) / 100;
    let (keep_from, retained) =
        agent_protocol::compaction::keep_from(traffic, &groups, 0, keep_target)?;

    let candidate: Vec<ModelMessage> = groups[..keep_from]
        .iter()
        .filter(|group| {
            group.keep
                && !traffic[group.start..=group.end]
                    .iter()
                    .any(|message| message.role == MessageRole::User)
        })
        .flat_map(|group| traffic[group.start..=group.end].iter().cloned())
        .collect();
    if candidate.is_empty() {
        return None;
    }
    Some(CompactionCandidate {
        estimated_tokens: candidate.iter().map(estimate_message).sum(),
        messages: candidate,
        retained_tokens: retained,
        budget_tokens: budget,
        observed_input_tokens: None,
        scope: CompactionScope::InFlight,
    })
}
