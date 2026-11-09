//! The capability stop: when the backend refuses the same missing capability
//! twice, the turn ends and hands a person the choice.
//!
//! It lives beside the loop rather than inside it for the same reason `budget`
//! does: the dispatch loop is long, and the rule that stops it is one small
//! fact with a lot of wording around it. The rule reads the refusal layer the
//! gateway already recorded — `sandbox_unavailable` — so a policy decision is
//! never mistaken for a capability the build cannot provide.

use agent_ports::agent_loop::{StepOutcome, StopReason, TurnContext};
use agent_ports::tool_gateway::ToolGatewayError;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::StepId;
use agent_protocol::model::ToolCall;
use agent_protocol::permissions::RefusalLayer;

use super::LoopTurn;
use super::event_drafts::draft;
use super::refusals::refuse_undispatched;

/// The capability-gap counters a turn carries between its steps.
#[derive(Debug, Default)]
pub(super) struct CapabilityGaps {
    /// `sandbox_unavailable` refusals this turn has seen.
    denials: u32,
    /// The last refusal that named a gap: (tool, capability).
    last: Option<(String, String)>,
}

impl LoopTurn {
    /// Count a refusal whose layer is the sandbox, and say whether the turn has
    /// now hit the same gap twice.
    ///
    /// One may be a call the model can replace; the second means the task needs
    /// a capability this run cannot get, so the turn stops instead of detouring.
    /// A rule or managed-policy denial is a decision about a call, not a gap,
    /// and is deliberately not counted.
    pub(super) fn note_capability_gap(&mut self, error: &ToolGatewayError, tool: &str) -> bool {
        let ToolGatewayError::PolicyDenied { policy, .. } = error else {
            return false;
        };
        let Some((capability, _)) =
            policy.refusals.iter().find(|(_, layer)| **layer == RefusalLayer::SandboxUnavailable)
        else {
            return false;
        };
        self.capability_gaps.denials = self.capability_gaps.denials.saturating_add(1);
        self.capability_gaps.last = Some((tool.to_string(), capability.as_str().to_string()));
        self.capability_gaps.denials >= 2
    }
}

/// End a turn that has hit the same capability gap twice.
///
/// The refused call is already in `events`; this answers the calls that were
/// never dispatched, closes the step, and returns the ending. The text is the
/// three choices a person can act on, so the ask is the turn's final content
/// rather than the model's next guess.
pub(super) fn stop(
    ctx: &TurnContext,
    step_id: &StepId,
    turn: &mut LoopTurn,
    events: &mut Vec<EventDraft>,
    remaining: &[ToolCall],
) -> StepOutcome {
    let text = gap_text(turn.capability_gaps.last.as_ref());
    // The calls after the one that crossed the rule never ran; each still
    // needs an answer, or the next request's history would be invalid.
    refuse_undispatched(
        ctx,
        step_id,
        turn,
        events,
        remaining,
        &StopReason::CapabilityBlocked,
        turn.budget.max_output_tokens,
    );
    events.push(draft(
        ctx,
        step_id,
        EventType::STEP_COMPLETED,
        serde_json::json!({}),
        Actor::Agent,
    ));
    StepOutcome {
        step_id: step_id.clone(),
        events: std::mem::take(events),
        stop: Some(StopReason::CapabilityBlocked),
        final_content: Some(text),
        spent: turn.spent,
    }
}

/// The ask a capability stop hands to a person.
///
/// It is kernel-authored, not model-authored: the stop must name what is
/// missing and offer the same three choices every time, rather than depend on
/// what the model would have written next. It names the refused tool and
/// capability when the refusal did, and stays neutral otherwise.
fn gap_text(gap: Option<&(String, String)>) -> String {
    let missing = match gap {
        Some((tool, capability)) => format!("`{tool}` 需要 `{capability}`"),
        None => String::from("这次任务需要的能力"),
    };
    format!(
        "能力缺口：{missing}，当前后端不提供这个能力。已停止，不再用静态文件或页面假装完成。\n\
         请选择下一步：\n\
         1. 用 app-container 后端重启后重试；\n\
         2. 接受降级交付（只交付静态内容，并明确它不是原目标）；\n\
         3. 取消这个任务。\n\
         回复 1、2 或 3。"
    )
}
