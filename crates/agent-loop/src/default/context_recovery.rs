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

use agent_ports::model_provider::ModelProviderError;

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
        let removed_ranges: Vec<(usize, usize)> =
            removed.iter().map(|group| (group.start, group.end + 1)).collect();
        let new_history_end = history_end_after_removal(
            turn.history_start,
            turn.history_end,
            &removed_ranges,
            Some(first_removed),
        );
        let new_history = new_history_end.saturating_sub(turn.history_start);
        turn.messages = candidate;
        turn.history_end = new_history_end;
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

/// The pieces one model attempt mutates when the provider refuses it.
pub(super) struct Attempt<'a> {
    pub step_id: &'a StepId,
    pub request: &'a mut ModelRequest,
    pub prepared: &'a mut serde_json::Value,
    pub estimated_request_tokens: &'a mut usize,
    pub attempt_id: &'a mut AttemptId,
}

/// What the first attempt produced.
pub(super) enum First {
    /// A response; the ordinary path continues from it.
    Response(ModelResponse),
    /// The step is over, and this is its outcome.
    Stopped(StepOutcome),
}

/// Send the request once — and, if the provider rejects it for its context
/// length, compress and send it once more.
///
/// Every attempt that fails reaches a terminal event before this returns, and a
/// second rejection is never retried: there is nothing left to remove, so a
/// third attempt would send the same rejected request.
pub(super) async fn first_attempt(
    model: &dyn ModelProvider,
    sink: &dyn ModelStreamSink,
    turn: &mut LoopTurn,
    attempt: Attempt<'_>,
    events: &mut Vec<EventDraft>,
) -> Result<First, AgentLoopError> {
    match model.stream(attempt.request, sink).await {
        Ok(response) => Ok(First::Response(response)),
        Err(error) if error.is_context_overflow() => {
            events.push(attempt_event(&turn.ctx, attempt.step_id, attempt.attempt_id, &error));
            let Some(context) = turn.budget.context else {
                return Ok(stopped(turn, attempt.step_id, events, StopReason::FatalError));
            };
            if !compress_for_overflow(turn, safe_target(&context), attempt.step_id, events)? {
                return Ok(stopped(turn, attempt.step_id, events, StopReason::FatalError));
            }
            // The compressed transcript is what the retry sends. It is a new
            // attempt: its own id, its own prepared record, exactly one send.
            attempt.request.messages = turn.messages.clone();
            *attempt.attempt_id = AttemptId::new();
            let (refs, hash) = message_references(&attempt.request.messages)?;
            let messages_tokens: usize =
                attempt.request.messages.iter().map(estimate_message).sum();
            let tool_tokens = budget::estimated_tool_tokens(&attempt.request.tool_manifest);
            let request_tokens = messages_tokens + tool_tokens;
            let mut retry = attempt.prepared.clone();
            retry["attempt_id"] = serde_json::json!(&*attempt.attempt_id);
            retry["message_count"] = serde_json::json!(attempt.request.messages.len());
            retry["context_message_count"] = serde_json::json!(turn.context_message_count);
            retry["estimated_messages_tokens"] = serde_json::json!(messages_tokens);
            retry["estimated_tool_tokens"] = serde_json::json!(tool_tokens);
            retry["estimated_request_tokens"] = serde_json::json!(request_tokens);
            retry["messages"] = serde_json::Value::Array(refs);
            retry["messages_hash"] = serde_json::json!(hash);
            retry["context_recovery"] = serde_json::json!("max_safe_compression");
            retry["original_error_kind"] = serde_json::json!("context_overflow");
            *attempt.prepared = retry.clone();
            *attempt.estimated_request_tokens = request_tokens;
            events.push(draft(
                &turn.ctx,
                attempt.step_id,
                EventType::MODEL_REQUEST_PREPARED,
                retry,
                Actor::Agent,
            ));
            events.push(draft(
                &turn.ctx,
                attempt.step_id,
                EventType::MODEL_ATTEMPT_STARTED,
                serde_json::json!({
                    "attempt_id": &*attempt.attempt_id,
                    "provider": model.provider_id(),
                    "route": serde_json::to_value(&attempt.request.model_route)?,
                }),
                Actor::Agent,
            ));
            match model.stream(attempt.request, sink).await {
                Ok(response) => Ok(First::Response(response)),
                Err(second) => {
                    let stop = if second.is_cancelled() {
                        StopReason::UserCancelled
                    } else {
                        StopReason::FatalError
                    };
                    events.push(attempt_event(
                        &turn.ctx,
                        attempt.step_id,
                        attempt.attempt_id,
                        &second,
                    ));
                    Ok(stopped(turn, attempt.step_id, events, stop))
                }
            }
        }
        Err(error) => {
            let stop = if error.is_cancelled() {
                StopReason::UserCancelled
            } else {
                StopReason::FatalError
            };
            events.push(attempt_event(&turn.ctx, attempt.step_id, attempt.attempt_id, &error));
            Ok(stopped(turn, attempt.step_id, events, stop))
        }
    }
}

/// The durable fact that one attempt did not settle.
fn attempt_event(
    ctx: &TurnContext,
    step_id: &StepId,
    attempt_id: &AttemptId,
    error: &ModelProviderError,
) -> EventDraft {
    draft(
        ctx,
        step_id,
        if error.is_cancelled() {
            EventType::MODEL_ATTEMPT_CANCELLED
        } else {
            EventType::MODEL_ATTEMPT_FAILED
        },
        serde_json::json!({
            "attempt_id": attempt_id,
            "error_kind": error.kind(),
            "message": truncate(&error.to_string(), MAX_ERROR_CHARS),
        }),
        Actor::Agent,
    )
}

/// End the step with `stop`, carrying what the attempt recorded.
fn stopped(
    turn: &LoopTurn,
    step_id: &StepId,
    events: &mut Vec<EventDraft>,
    stop: StopReason,
) -> First {
    events.push(step_failed(&turn.ctx, step_id));
    First::Stopped(StepOutcome {
        step_id: step_id.clone(),
        events: std::mem::take(events),
        stop: Some(stop),
        final_content: None,
        spent: turn.spent,
    })
}

/// The history boundary after removing message ranges expressed in original
/// transcript indices.
///
/// In-flight traffic shares the transcript with settled history but not the
/// history range. Counting an in-flight removal as a history removal would
/// shrink `history_end` past messages that are still present, so only overlap
/// with `history_start..history_end` is charged here.
fn history_end_after_removal(
    history_start: usize,
    history_end: usize,
    removed_ranges: &[(usize, usize)],
    replacement_at: Option<usize>,
) -> usize {
    let previous_history = history_end.saturating_sub(history_start);
    let removed_from_history = removed_ranges
        .iter()
        .map(|(start, end)| {
            let overlap_start = (*start).max(history_start);
            let overlap_end = (*end).min(history_end);
            overlap_end.saturating_sub(overlap_start)
        })
        .sum::<usize>();
    let retained_history = previous_history.saturating_sub(removed_from_history);
    let replacement_in_history =
        replacement_at.is_some_and(|at| at >= history_start && at < history_end);
    history_start
        .saturating_add(retained_history)
        .saturating_add(usize::from(replacement_in_history))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mixed_removal_counts_only_messages_that_were_in_history() {
        assert_eq!(
            history_end_after_removal(2, 7, &[(2, 3), (8, 10)], Some(2)),
            7,
            "only the settled-history group may reduce the history range"
        );
    }

    #[test]
    fn a_replacement_outside_history_does_not_change_history_end() {
        assert_eq!(
            history_end_after_removal(2, 7, &[(8, 10)], Some(8)),
            7,
            "an in-flight-only removal keeps every historical message"
        );
    }
}
