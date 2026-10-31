//! One-shot recovery from a provider context-length rejection.
//!
//! A provider that answers `413` or a context-length `400` has told us exactly
//! one thing: this request was too large. Repeating it is pointless, and asking
//! a summarizer to shrink it can overflow for the same reason. The recovery is
//! therefore local and deterministic: settle this turn's traffic without a model
//! call if that is enough, otherwise drop the oldest complete message groups
//! until the request fits a conservative target, record the removal as one
//! `context/compacted` event, and let the caller retry exactly once.
//!
//! Nothing here removes the current user input, a mid-turn user message, or a
//! tool call separately from its result.

use std::collections::HashSet;

use super::*;

/// The estimate a compressed request must come in under.
///
/// The compaction trigger is already the point at which the run considers the
/// request too large; the input working set is the hard ceiling. Taking the
/// smaller of the two leaves room for the answer and is still below whatever
/// measure the provider rejected.
pub(super) fn safe_target(context: &ResolvedContextBudget) -> usize {
    usize::try_from(context.input_working_set_tokens.min(context.compaction_trigger_tokens))
        .unwrap_or(usize::MAX)
}

/// Compress `turn` in place until its request fits `target`.
///
/// Returns `true` when the turn's messages now fit, in which case the caller may
/// rebuild and retry the request once. Returns `false` without touching the
/// transcript when there was nothing removable that could make it fit — the
/// caller then reports the failure it already recorded.
pub(super) fn compress_for_overflow(
    turn: &mut LoopTurn,
    target: usize,
    step_id: &StepId,
    events: &mut Vec<EventDraft>,
) -> Result<bool, AgentLoopError> {
    // 1. Settlement costs no model call and is the cheapest way to shrink the
    // request, so it is tried first. Its own event is the record.
    let settled = if turn.budget.context.is_some() {
        settlement::settle(turn, step_id, "provider_context_overflow", events)?
    } else {
        false
    };
    // A request that already fits under our own estimate cannot be made smaller,
    // and re-sending the same bytes to a provider that just rejected them would
    // spend an attempt on nothing. Only a transcript that actually changed is
    // worth retrying.
    if settled && request_fits(turn, target) {
        return Ok(true);
    }

    let attempt_id = AttemptId::new();
    let input_uid = format!("msg/user/{}", turn.ctx.turn_id);
    let in_flight_start = turn
        .messages
        .iter()
        .position(|message| message.message_uid.as_str() == input_uid)
        .map(|index| index + 1);

    // 2. The removable groups, oldest first: settled history, then this turn's
    // traffic after the current input.
    let groups = agent_protocol::compaction::groups(&turn.messages);
    let mut removable: Vec<(usize, agent_protocol::compaction::Group)> = Vec::new();
    let mut newest_in_flight: Option<usize> = None;
    for (index, group) in groups.iter().enumerate() {
        if !group.keep {
            continue;
        }
        let settled = group.start >= turn.history_start && group.end < turn.history_end;
        let in_flight = in_flight_start.is_some_and(|start| group.start >= start);
        if !settled && !in_flight {
            continue;
        }
        // The user's own words are never removed, and a group that carries
        // one is kept whole rather than split.
        if turn.messages[group.start..=group.end]
            .iter()
            .any(|message| message.role == MessageRole::User)
        {
            continue;
        }
        if in_flight {
            newest_in_flight = Some(index);
        }
        removable.push((index, *group));
    }
    // The newest tool cycle is what the model is in the middle of acting on.
    if let Some(newest) = newest_in_flight {
        removable.retain(|(index, _)| *index != newest);
    }

    // 3. Remove oldest first until the request with its replacement record fits.
    let mut removed: Vec<agent_protocol::compaction::Group> = Vec::new();
    for (_, group) in removable {
        removed.push(group);
        let removed_uids: HashSet<String> = removed
            .iter()
            .flat_map(|group| {
                turn.messages[group.start..=group.end]
                    .iter()
                    .map(|message| message.message_uid.as_str().to_string())
            })
            .collect();
        let first_removed = removed
            .iter()
            .map(|group| group.start)
            .min()
            .expect("a removal has at least one group");
        let replacement = replacement_message(&attempt_id, removed_uids.len());
        let candidate =
            with_replacement(&turn.messages, first_removed, &removed_uids, &replacement);
        if budget::estimated_request_tokens(&candidate, &turn.manifest) > target {
            continue;
        }

        // 4. Commit: the transcript loses exactly the selected messages and
        // gains the record in the earliest one's place.
        let estimated_before = budget::estimated_request_tokens(&turn.messages, &turn.manifest);
        let refs: Vec<serde_json::Value> = turn
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| removed_uids.contains(message.message_uid.as_str()))
            .map(|(_, message)| {
                serde_json::json!({
                    "message_uid": message.message_uid.as_str(),
                    "role": message.role.as_str(),
                    "hash": digest::json(&serde_json::to_value(message).unwrap_or_default()),
                })
            })
            .collect();
        let previous_history = turn.history_end.saturating_sub(turn.history_start);
        let removed_from_history = removed
            .iter()
            .map(|group| {
                turn.messages[group.start..=group.end]
                    .iter()
                    .filter(|message| {
                        let uid = message.message_uid.as_str();
                        removed_uids.contains(uid)
                    })
                    .count()
            })
            .sum::<usize>();
        let replacement_in_history =
            first_removed >= turn.history_start && first_removed < turn.history_end;
        turn.messages = candidate;
        let retained_history = previous_history.saturating_sub(removed_from_history);
        let new_history = retained_history + usize::from(replacement_in_history);
        turn.history_end = turn.history_start + new_history;
        turn.context_message_count =
            turn.context_message_count.saturating_sub(previous_history).saturating_add(new_history);

        let estimated_after = budget::estimated_request_tokens(&turn.messages, &turn.manifest);
        let text = replacement.text_content();
        events.push(draft(
            &turn.ctx,
            step_id,
            EventType::CONTEXT_COMPACTED,
            serde_json::json!({
                "strategy": "overflow-trim",
                "fallback": "provider_context_overflow",
                "scope": "overflow",
                "attempt_id": attempt_id,
                "summary_message_uid": format!("msg/summary/{attempt_id}"),
                "summary": text,
                "summarized": refs,
                "summarized_hash": digest::json(&serde_json::Value::Array(refs.clone())),
                "context_revision": turn.context_revision.as_u64(),
                "message_count": removed_uids.len(),
                "removed_messages": removed_uids.len(),
                "dropped_messages": removed_uids.len(),
                "budget_tokens": target,
                "estimated_before": estimated_before,
                "estimated_after": estimated_after,
                "estimated_tokens": estimated_before,
                "retained_tokens": estimated_after,
                "observed_input_tokens": turn.observed_input_tokens,
                "truncated": false,
            }),
            Actor::Agent,
        ));
        return Ok(true);
    }
    Ok(false)
}

fn request_fits(turn: &LoopTurn, target: usize) -> bool {
    budget::estimated_request_tokens(&turn.messages, &turn.manifest) <= target
}

/// The message that stands in for the removed history.
fn replacement_message(attempt_id: &AttemptId, omitted: usize) -> ModelMessage {
    ModelMessage::summary_for(
        attempt_id,
        format!(
            "Overflow recovery: {omitted} older messages were omitted because the provider \
             rejected the request for its context length. The run instructions, current user \
             input, newest tool cycle, and this record remain. The omitted messages are still \
             present in the event log."
        ),
    )
}

/// The transcript with `removed` dropped and `replacement` placed at
/// `insert_at`.
fn with_replacement(
    messages: &[ModelMessage],
    insert_at: usize,
    removed: &HashSet<String>,
    replacement: &ModelMessage,
) -> Vec<ModelMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if index == insert_at {
            out.push(replacement.clone());
        }
        if removed.contains(message.message_uid.as_str()) {
            continue;
        }
        out.push(message.clone());
    }
    out
}
