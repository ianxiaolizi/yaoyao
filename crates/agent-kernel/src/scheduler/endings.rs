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
        // A limit ending is a *pause*, not an end: the turn did not answer, and
        // a `turn/completed` would make the delivery axis offer whatever text
        // the model happened to have written as the answer. `turn/yielded` is
        // the log's word for "stopped, waiting", and the `budget/parked` record
        // written beside it says what for (CONTRACT §7 #173).
        let (turn_event, run_event) = match &reason {
            // Spelled out rather than a guard on `is_limit`: a match arm with a
            // guard does not discharge exhaustiveness, and a new `StopReason`
            // variant must force a decision here rather than fall into `_`.
            StopReason::MaxSteps
            | StopReason::MaxToolCalls
            | StopReason::WallClockExceeded
            | StopReason::MaxOutputTokens
            | StopReason::TokenBudgetExceeded
            | StopReason::Stalled
            | StopReason::NeedsUserInput => (EventType::TURN_YIELDED, EventType::RUN_YIELDED),
            StopReason::FinalAnswer | StopReason::ContentFiltered | StopReason::Completed => {
                (EventType::TURN_COMPLETED, EventType::RUN_COMPLETED)
            }
            // The turn ended normally and the review afterwards withheld the
            // answer (B9). Named here so a loop that ever returned this reason
            // would be a decision rather than a fall into `_`; it is not a
            // reason the loop produces, and the scheduler, which writes it after
            // this method has run, rewrites the payload's `stop_reason` and sets
            // `incomplete` itself.
            StopReason::SecurityBlocked | StopReason::CapabilityBlocked => {
                (EventType::TURN_COMPLETED, EventType::RUN_COMPLETED)
            }
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
            // Same shape as a stall: text the model produced that is not a
            // delivered answer. Unreachable through `settle` for the reason the
            // terminal-event arm gives, and kept so the payload never falls
            // through to a shape that would read as an ordinary ending.
            Ending::Loop { reason: StopReason::SecurityBlocked, final_content } => {
                let mut payload = serde_json::json!({ "stop_reason": reason, "incomplete": true });
                if let Some(content) = final_content {
                    payload["final_content"] = serde_json::json!(content);
                }
                payload
            }
            // A capability stop is the loop's own decision, and its text is an
            // ask, not an answer: `incomplete` keeps it off the delivery axis
            // while the three choices ride in the payload for the page and the
            // CLI to show.
            Ending::Loop { reason: StopReason::CapabilityBlocked, final_content } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A capability stop is a completed turn that delivered nothing: the
    /// terminal pair is the completed one, and `incomplete` keeps the ask off
    /// the delivery axis. The three choices ride in the payload because the
    /// page and the CLI show them from there.
    #[test]
    fn a_capability_stop_settles_as_completed_but_incomplete() {
        let ending = Ending::Loop {
            reason: StopReason::CapabilityBlocked,
            final_content: Some("请选择 1、2 或 3".to_string()),
        };
        let (reason, content, turn_event, run_event, payload) = ending.settle();

        assert_eq!(reason, StopReason::CapabilityBlocked);
        assert_eq!(content.as_deref(), Some("请选择 1、2 或 3"));
        assert_eq!(turn_event, EventType::TURN_COMPLETED);
        assert_eq!(run_event, EventType::RUN_COMPLETED);
        assert_eq!(payload["stop_reason"], "capability_blocked");
        assert_eq!(payload["incomplete"], true);
        assert_eq!(payload["final_content"], "请选择 1、2 或 3");
    }

    /// A capability stop is not a limit: it does not carry the spend fields a
    /// parked record needs.
    #[test]
    fn a_capability_stop_is_not_a_limit() {
        assert!(!is_limit(&StopReason::CapabilityBlocked));
    }
}
