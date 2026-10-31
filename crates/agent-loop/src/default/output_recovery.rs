//! The one recovery from a reasoning-only truncation.
//!
//! A reasoning model can spend the whole completion cap on tokens that never
//! become content: `finish_reason: length`, no text block, no tool call, and
//! `reasoning_tokens == output_tokens`. The turn has paid for a request that
//! answered nothing, but it also learned something — the cap, not the model's
//! willingness, is what ended the answer — so it asks once more with more room.
//!
//! At the model's own ceiling there is no more room to give, and the only
//! remaining lever is depth: the request is re-asked with the next lower
//! reasoning depth the model declared (`max -> high -> low`), never one it did
//! not. Recovery happens at most once per step: a second truncation means this
//! request cannot be answered within the model's limits, and re-asking again
//! would spend the run's budget on the same wall.

use agent_ports::agent_loop::{AgentLoopError, StopReason, TurnContext};
use agent_ports::model_provider::{
    ContentBlock, FinishReason, ModelRequest, ModelResponse, ReasoningEffort,
};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{AttemptId, StepId};
use agent_protocol::profile::{ResolvedContextBudget, RunBudget};

use crate::context_layers::truncate;

use super::{DefaultAgentLoop, LoopTurn, MAX_ERROR_CHARS, NullSink, budget, draft, step_failed};

/// What the loop should do with a response the output limit ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    /// Ask again once, with this completion cap and (when set) this depth.
    ///
    /// `reasoning_effort: None` keeps the depth the first request carried:
    /// doubling the cap is the whole change.
    Retry { max_tokens: u32, reasoning_effort: Option<ReasoningEffort> },
    /// Treat the truncated response as the step's settled outcome.
    Stop,
}

/// Decide whether a truncated response earns the one retry.
///
/// `current_effort` is the depth the first request carried, needed only when
/// the cap is already at its ceiling and the next lever is depth.
pub(crate) fn decision(
    response: &ModelResponse,
    budget: &ResolvedContextBudget,
    current_effort: Option<ReasoningEffort>,
    supported_efforts: &[ReasoningEffort],
    retry_used: bool,
) -> RecoveryDecision {
    if retry_used || !is_reasoning_only(response) {
        return RecoveryDecision::Stop;
    }
    let cap = budget.completion_cap_tokens;
    // A ceiling below the cap would be a configuration the budget resolver
    // rejects; reading it as "no room to grow" is still the safe answer.
    let ceiling = budget.completion_cap_ceiling_tokens.max(cap);
    if cap < ceiling {
        let doubled = cap.saturating_mul(2).min(ceiling);
        return RecoveryDecision::Retry { max_tokens: doubled, reasoning_effort: None };
    }
    match next_lower_effort(current_effort, supported_efforts) {
        Some(effort) => RecoveryDecision::Retry { max_tokens: cap, reasoning_effort: Some(effort) },
        None => RecoveryDecision::Stop,
    }
}

/// Whether the provider ended the response with nothing but reasoning.
///
/// All three conditions are required:
///
/// - the ending is `length`, not a stop the model chose;
/// - there is no content the turn could keep — a blank text block is not
///   content, and a proposed call is content (indeed a plan the halt forbids);
/// - the reported usage proves the cap was spent thinking: reasoning tokens
///   account for the whole output. A provider that reports no usage cannot
///   prove it, so it never earns a retry.
pub(crate) fn is_reasoning_only(response: &ModelResponse) -> bool {
    if response.finish_reason.as_str() != FinishReason::LENGTH {
        return false;
    }
    let has_content = response.content_blocks.iter().any(|block| match block {
        ContentBlock::Text { text } => !text.trim().is_empty(),
        // A resolved response carries proposals, not outcomes; a tool result
        // here would be a provider echoing its own history, and either way it
        // is content the turn must not discard.
        ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => true,
    });
    if has_content {
        return false;
    }
    let Some(usage) = response.usage else {
        return false;
    };
    match (usage.reasoning_tokens, usage.output_tokens) {
        (Some(reasoning), Some(output)) => reasoning > 0 && reasoning == output,
        _ => false,
    }
}

/// The greatest depth the model declared that is strictly below `current`.
fn next_lower_effort(
    current: Option<ReasoningEffort>,
    supported: &[ReasoningEffort],
) -> Option<ReasoningEffort> {
    let current = current?;
    supported.iter().copied().filter(|effort| *effort < current).max()
}

/// The completion budget output recovery is measured against.
///
/// Assembly freezes one derived from the model's capabilities. A run that never
/// reached discovery — a harness, or a build older than the derivation — still
/// has the explicit `max_output_tokens` it asked for; its ceiling is that same
/// number, because nothing has shown the model has more room than the operator
/// granted. A run with neither is a run that sent no completion cap, and there
/// is nothing to double or lower.
pub(super) fn recovery_budget(budget: &RunBudget) -> Option<ResolvedContextBudget> {
    match budget.context {
        Some(context) => Some(context),
        None => budget.max_output_tokens.map(|cap| ResolvedContextBudget {
            completion_cap_tokens: cap,
            completion_cap_ceiling_tokens: cap,
            ..ResolvedContextBudget::default()
        }),
    }
}

/// What the step does with the response the provider settled.
pub(super) enum RecoveryOutcome {
    /// The response the step keeps — the first one when no retry was earned.
    Settled(ModelResponse),
    /// The recovery request itself failed; the step ends on this stop.
    Failed(StopReason),
}

impl DefaultAgentLoop {
    /// Run the one recovery a reasoning-only truncation earns.
    ///
    /// Exactly one request: `decision` is asked once, with `retry_used: false`,
    /// and the caller asks this once per step, so a second truncation is the
    /// step's outcome rather than another ask. The attempt events live here
    /// (rather than in the loop) because everything they carry is about the
    /// recovery; a failed retry ends the step the way a failed first attempt
    /// does, minus the response the loop would otherwise record.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn recover_output(
        &self,
        ctx: &TurnContext,
        step_id: &StepId,
        turn: &mut LoopTurn,
        request: &ModelRequest,
        prepared: &serde_json::Value,
        attempt_id: &mut AttemptId,
        first: ModelResponse,
        estimated_request_tokens: usize,
        events: &mut Vec<EventDraft>,
    ) -> Result<RecoveryOutcome, AgentLoopError> {
        let response = first;
        let Some(recovery_budget) = recovery_budget(&turn.budget) else {
            return Ok(RecoveryOutcome::Settled(response));
        };
        let decision = decision(
            &response,
            &recovery_budget,
            turn.budget.reasoning_effort,
            &turn.budget.supported_reasoning_efforts,
            false,
        );
        let RecoveryDecision::Retry { max_tokens, reasoning_effort } = decision else {
            return Ok(RecoveryOutcome::Settled(response));
        };
        // The first response's tokens were really spent: the budget reads cost,
        // not value, so they are recorded even though the response itself is
        // discarded.
        budget::record_spend(turn, &response, estimated_request_tokens as u64);
        let mut retried_request = request.clone();
        retried_request.budget.max_tokens = Some(max_tokens);
        if let Some(effort) = reasoning_effort {
            retried_request.budget.reasoning_effort = Some(effort);
        }
        *attempt_id = AttemptId::new();
        let mut retried = prepared.clone();
        retried["attempt_id"] = serde_json::json!(*attempt_id);
        retried["output_recovery"] = serde_json::json!("reasoning_only");
        events.push(draft(ctx, step_id, EventType::MODEL_REQUEST_PREPARED, retried, Actor::Agent));
        events.push(draft(
            ctx,
            step_id,
            EventType::MODEL_ATTEMPT_STARTED,
            serde_json::json!({
                "attempt_id": *attempt_id,
                "provider": self.model.provider_id(),
                "route": serde_json::to_value(&retried_request.model_route)?,
                "output_recovery": "reasoning_only",
            }),
            Actor::Agent,
        ));
        // `NullSink`: the reasoning this retry produces is not shown twice —
        // the live turn already had its one stream of thinking from the
        // attempt that ran out of room.
        match self.model.stream(&retried_request, &NullSink).await {
            Ok(second) => Ok(RecoveryOutcome::Settled(second)),
            Err(error) => {
                let cancelled = error.is_cancelled();
                events.push(draft(
                    ctx,
                    step_id,
                    if cancelled {
                        EventType::MODEL_ATTEMPT_CANCELLED
                    } else {
                        EventType::MODEL_ATTEMPT_FAILED
                    },
                    serde_json::json!({
                        "attempt_id": *attempt_id,
                        "error_kind": error.kind(),
                        "message": truncate(&error.to_string(), MAX_ERROR_CHARS),
                        "output_recovery": "reasoning_only",
                    }),
                    Actor::Agent,
                ));
                events.push(step_failed(ctx, step_id));
                Ok(RecoveryOutcome::Failed(if cancelled {
                    StopReason::UserCancelled
                } else {
                    StopReason::FatalError
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::model::Usage;

    /// A response that spent `tokens` entirely on reasoning.
    fn reasoning_only(tokens: u64) -> ModelResponse {
        ModelResponse {
            content_blocks: Vec::new(),
            usage: Some(Usage {
                input_tokens: Some(5_010),
                output_tokens: Some(tokens),
                total_tokens: Some(5_010 + tokens),
                reasoning_tokens: Some(tokens),
                ..Usage::default()
            }),
            finish_reason: FinishReason::new(FinishReason::LENGTH),
            provider_metadata: serde_json::Value::Null,
        }
    }

    fn budget(cap: u32, ceiling: u32) -> ResolvedContextBudget {
        ResolvedContextBudget {
            completion_cap_tokens: cap,
            completion_cap_ceiling_tokens: ceiling,
            ..ResolvedContextBudget::default()
        }
    }

    const ALL_EFFORTS: [ReasoningEffort; 3] =
        [ReasoningEffort::Low, ReasoningEffort::High, ReasoningEffort::Max];

    #[test]
    fn a_reasoning_only_length_response_requests_a_larger_cap() {
        let decision = decision(
            &reasoning_only(8_000),
            &budget(262_144, 393_216),
            Some(ReasoningEffort::High),
            &ALL_EFFORTS,
            false,
        );
        assert_eq!(
            decision,
            RecoveryDecision::Retry { max_tokens: 393_216, reasoning_effort: None }
        );
    }

    #[test]
    fn a_cap_at_the_ceiling_steps_the_depth_down_instead() {
        let decision = decision(
            &reasoning_only(393_216),
            &budget(393_216, 393_216),
            Some(ReasoningEffort::Max),
            &ALL_EFFORTS,
            false,
        );
        assert_eq!(
            decision,
            RecoveryDecision::Retry {
                max_tokens: 393_216,
                reasoning_effort: Some(ReasoningEffort::High)
            }
        );
    }

    #[test]
    fn depth_steps_through_only_the_declared_levels_and_stops_at_the_bottom() {
        // high -> low when both are declared; low has nothing below it.
        assert_eq!(
            decision(
                &reasoning_only(1),
                &budget(1_000, 1_000),
                Some(ReasoningEffort::High),
                &ALL_EFFORTS,
                false,
            ),
            RecoveryDecision::Retry {
                max_tokens: 1_000,
                reasoning_effort: Some(ReasoningEffort::Low)
            }
        );
        assert_eq!(
            decision(
                &reasoning_only(1),
                &budget(1_000, 1_000),
                Some(ReasoningEffort::Low),
                &ALL_EFFORTS,
                false,
            ),
            RecoveryDecision::Stop
        );
        // A model that declared only max gets no retry: the depth it would be
        // asked for was never offered.
        assert_eq!(
            decision(
                &reasoning_only(1),
                &budget(1_000, 1_000),
                Some(ReasoningEffort::Max),
                &[ReasoningEffort::Max],
                false,
            ),
            RecoveryDecision::Stop
        );
        // No depth was sent, so there is nothing to lower.
        assert_eq!(
            decision(&reasoning_only(1), &budget(1_000, 1_000), None, &ALL_EFFORTS, false),
            RecoveryDecision::Stop
        );
    }

    #[test]
    fn the_retry_happens_once_and_only_for_a_proven_reasoning_truncation() {
        assert_eq!(
            decision(&reasoning_only(1), &budget(1_000, 2_000), None, &ALL_EFFORTS, true),
            RecoveryDecision::Stop,
            "a second truncation is final"
        );

        // Text survived the truncation: there is content to keep, so the cap
        // was not spent entirely on thinking.
        let mut with_text = reasoning_only(1);
        with_text.content_blocks = vec![ContentBlock::text("half a sentence")];
        assert_eq!(
            decision(&with_text, &budget(1_000, 2_000), None, &ALL_EFFORTS, false),
            RecoveryDecision::Stop
        );

        // A proposed call is content too — the halt already forbids dispatching
        // it, and a retry would only repeat the same beginning.
        let mut with_call = reasoning_only(1);
        with_call.content_blocks =
            vec![ContentBlock::tool_use("c1", "echo", serde_json::json!({}))];
        assert_eq!(
            decision(&with_call, &budget(1_000, 2_000), None, &ALL_EFFORTS, false),
            RecoveryDecision::Stop
        );

        // Usage that does not prove the whole output was reasoning is not
        // enough evidence to spend a second request.
        let mut unequal = reasoning_only(1);
        unequal.usage = Some(Usage {
            output_tokens: Some(2_000),
            reasoning_tokens: Some(1_000),
            ..Usage::default()
        });
        assert_eq!(
            decision(&unequal, &budget(1_000, 2_000), None, &ALL_EFFORTS, false),
            RecoveryDecision::Stop
        );

        let mut no_usage = reasoning_only(1);
        no_usage.usage = None;
        assert_eq!(
            decision(&no_usage, &budget(1_000, 2_000), None, &ALL_EFFORTS, false),
            RecoveryDecision::Stop
        );

        // A model-chosen stop is not a truncation at all.
        let mut stopped = reasoning_only(1);
        stopped.finish_reason = FinishReason::new(FinishReason::STOP);
        assert_eq!(
            decision(&stopped, &budget(1_000, 2_000), None, &ALL_EFFORTS, false),
            RecoveryDecision::Stop
        );
    }
}
