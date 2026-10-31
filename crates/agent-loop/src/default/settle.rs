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
pub(super) const DEFAULT_SETTLE_PERCENT: u32 = 70;

/// The input working set this run was resolved with.
///
/// The run budget is the authority: assembly froze the resolved numbers beside
/// the route. A harness, or a run from before the derivation existed, may only
/// have the engine's own report of the window it trimmed against, so that is
/// the fallback. A run with neither has no window, and nothing is bounded.
pub(super) fn working_set(turn: &LoopTurn) -> Option<u32> {
    turn.budget
        .context
        .as_ref()
        .map(|budget| budget.input_working_set_tokens)
        .or(turn.context_usage.input_working_set_tokens)
}

/// How many of the newest tokens a compaction keeps whole.
///
/// The resolved budget states it. A run that only has the engine's report falls
/// back to the share the trimmer has always used, so an old snapshot compacts
/// rather than not at all.
pub(super) fn retain_recent(turn: &LoopTurn, budget: usize) -> usize {
    match turn.budget.context.as_ref() {
        Some(resolved) => resolved.retain_recent_tokens as usize,
        None => {
            budget.saturating_mul(agent_protocol::compaction::COMPACTION_KEEP_PERCENT as usize)
                / 100
        }
    }
}

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
    let budget = working_set(turn)? as usize;
    if budget == 0 {
        return None;
    }
    let percent = turn.budget.settle_at_percent.unwrap_or(DEFAULT_SETTLE_PERCENT) as usize;
    let threshold = budget.saturating_mul(percent) / 100;
    if estimated_request_tokens < threshold {
        return None;
    }

    let input_uid = format!("msg/user/{}", turn.ctx.turn_id);
    let input =
        turn.messages.iter().position(|message| message.message_uid.as_str() == input_uid)?;
    let traffic = &turn.messages[input + 1..];
    let groups = agent_protocol::compaction::groups(traffic);
    let keep_target = retain_recent(turn, budget);
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
        // The settle share, not the settled-history trigger: this proposal is
        // this turn's own traffic and fires on the run's `settle_at_percent`.
        trigger_tokens: u32::try_from(threshold).unwrap_or(u32::MAX),
        observed_input_tokens: None,
        scope: CompactionScope::InFlight,
    })
}
