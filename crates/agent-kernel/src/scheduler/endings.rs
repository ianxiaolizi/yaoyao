//! How a turn stopped, and what the session's lifecycle says about starting
//! one.

use agent_ports::agent_loop::StopReason;
use agent_protocol::events::EventType;
use agent_protocol::ids::InboxItemId;

/// What the session's lifecycle says about starting a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// No run is open and nothing is held.
    Ready,
    /// A run is already open; the caller should report nothing and consume
    /// nothing.
    Busy,
}

/// Whether the turn stopped because a limit was reached, rather than because an
/// answer, a person or a failure ended it.
///
/// These endings carry what the run spent, because a limit hit tells a reader
/// nothing without how far the turn got before it.
pub(crate) fn is_limit(reason: &StopReason) -> bool {
    matches!(
        reason,
        StopReason::MaxSteps
            | StopReason::MaxToolCalls
            | StopReason::WallClockExceeded
            | StopReason::MaxOutputTokens
            | StopReason::TokenBudgetExceeded
            | StopReason::Stalled
    )
}

/// How a turn stopped.
///
/// Three endings, because two of them are decisions the Scheduler makes rather
/// than facts the loop reports, and collapsing them into a `StopReason` would
/// lose that: an exhausted step budget and a user interrupt are not things the
/// loop decided, they are things that were done to the turn.
pub(crate) enum Ending {
    /// The loop said the turn was over.
    Loop { reason: StopReason, final_content: Option<String> },
    /// The step budget ran out. The budget is the Scheduler's policy, so only
    /// the Scheduler can report this ending.
    Budget,
    /// The user interrupted the turn at a step boundary.
    Interrupted { item_id: InboxItemId },
}

impl Ending {
    /// The stop reason, the final content, the two terminal event types, and
    /// the terminal payload.
    #[allow(clippy::type_complexity)]
    pub(crate) fn settle(
        &self,
    ) -> (StopReason, Option<String>, &'static str, &'static str, serde_json::Value) {
        let reason = match self {
            Ending::Loop { reason, .. } => reason.clone(),
            Ending::Budget => StopReason::MaxSteps,
            Ending::Interrupted { .. } => StopReason::UserCancelled,
        };
        let final_content = match self {
            Ending::Loop { final_content, .. } => final_content.clone(),
            _ => None,
        };
        let (turn_event, run_event) = match &reason {
            StopReason::FinalAnswer
            | StopReason::MaxSteps
            | StopReason::MaxToolCalls
            | StopReason::WallClockExceeded
            | StopReason::MaxOutputTokens
            | StopReason::ContentFiltered
            | StopReason::TokenBudgetExceeded
            | StopReason::Stalled
            | StopReason::Completed => (EventType::TURN_COMPLETED, EventType::RUN_COMPLETED),
            StopReason::NeedsUserInput => (EventType::TURN_YIELDED, EventType::RUN_YIELDED),
            StopReason::FatalError => (EventType::TURN_FAILED, EventType::RUN_FAILED),
            StopReason::UserCancelled => (EventType::TURN_CANCELLED, EventType::RUN_CANCELLED),
        };
        // An interrupt names its cause, because `turn/cancelled` alone cannot
        // say whether the user stopped the turn or an adapter abandoned an
        // attempt. The paths that predate the interrupt keep their exact
        // payload — the cancelled turn's shape is not this change's to alter.
        let payload = match self {
            Ending::Interrupted { item_id } => serde_json::json!({
                "cause": "user_interrupt",
                "inbox_item": item_id,
            }),
            // The one terminal payload that would otherwise be empty. A budget
            // stop writes no final content, and `turn/completed {}` cannot be
            // told from a turn that ended with nothing to say: on 2026-10-24's
            // real use the log gave no reason and only stdout did.
            Ending::Budget => serde_json::json!({ "stop_reason": &reason }),
            // A limit the loop enforced inside a step ends the turn the same
            // way the step limit does, and for the same reason: `turn/completed`
            // with an empty payload cannot be told from a turn that ended with
            // nothing to say.
            Ending::Loop { reason, .. }
                if matches!(reason, StopReason::MaxToolCalls | StopReason::WallClockExceeded) =>
            {
                serde_json::json!({ "stop_reason": reason })
            }
            // A budget or stall ending may still carry text the model wrote
            // before it was stopped, but that text is not an answer to the
            // whole task. `incomplete` says so in the log beside it, so a
            // reader does not have to infer it from the stop reason.
            Ending::Loop {
                reason: reason @ (StopReason::TokenBudgetExceeded | StopReason::Stalled),
                final_content,
            } => {
                let mut payload = serde_json::json!({ "stop_reason": reason, "incomplete": true });
                if let Some(content) = final_content {
                    payload["final_content"] = serde_json::json!(content);
                }
                payload
            }
            // The operator's completion check ended the turn. Unlike a model
            // final answer this reason is a claim about the world, so the log
            // names it even when a final content is absent.
            Ending::Loop { reason: StopReason::Completed, final_content } => {
                let mut payload = serde_json::json!({ "stop_reason": reason });
                if let Some(content) = final_content {
                    payload["final_content"] = serde_json::json!(content);
                }
                payload
            }
            // A response the provider ended before the model finished is not an
            // answer, and it may still have text: the beginning of one. The
            // terminal says both, so `metrics` counts it under the provider's
            // ending rather than under `final_answer`, and a reader is not
            // handed half an answer dressed as a whole one (CONTRACT §7 #131,
            // #132).
            Ending::Loop {
                reason: reason @ (StopReason::MaxOutputTokens | StopReason::ContentFiltered),
                final_content,
            } => {
                let mut payload = serde_json::json!({ "stop_reason": reason });
                if let Some(content) = final_content {
                    payload["final_content"] = serde_json::json!(content);
                }
                payload
            }
            _ => match &final_content {
                Some(content) => serde_json::json!({ "final_content": content }),
                None => serde_json::json!({}),
            },
        };
        (reason, final_content, turn_event, run_event, payload)
    }
}
