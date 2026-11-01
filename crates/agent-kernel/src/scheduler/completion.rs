//! The completion gate's verdict, written beside the turn that earned it.
//!
//! Two facts are recorded here rather than inferred later by a reader:
//!
//! * a turn that stopped because the criterion passed is a *claim*, and the
//!   gate's refusal is written beside it, so the log says why no completion
//!   stands;
//! * an answer handed over after tool work with nothing current behind it is
//!   the ordinary case the status axes exist for, and recording it here means a
//!   reader does not have to re-fold to discover it.
//!
//! Neither event changes the turn's ending or the run's terminal word: the turn
//! really ended, and the transcript must not pretend otherwise. The decisions
//! themselves are `crate::completion`'s; this module only turns a verdict into
//! the events the terminal batch carries (CONTRACT §7 #170, #173).

use agent_ports::agent_loop::StopReason;
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::evidence::ClaimVerdict;
use agent_protocol::ids::{RunId, SessionId, TurnId};

use super::draft;

/// Append the gate's verdicts for this turn, if it has any to record.
pub(crate) fn note_claims(
    history: &[StoredEvent],
    so_far: &[StoredEvent],
    reason: &StopReason,
    final_content: Option<&str>,
    (session_id, run_id, turn_id): (&SessionId, &RunId, &TurnId),
    pending: &mut Vec<EventDraft>,
) {
    let claim = crate::completion::gate(so_far);
    if *reason == StopReason::Completed && !claim.is_accepted() {
        let mut payload = serde_json::json!({
            "verdict": claim.as_str(),
            "task_revision": crate::completion::task_revision(so_far),
        });
        if let Some(evidence) = claim.evidence() {
            payload["check_id"] = serde_json::json!(evidence.check_id);
            if let ClaimVerdict::ExitMasked { operator, .. } = &claim {
                payload["operator"] = serde_json::json!(operator);
            }
        }
        pending.push(draft(
            session_id,
            EventType::COMPLETION_CLAIM_REJECTED,
            payload,
            Actor::Scheduler,
            Some(run_id),
            Some(turn_id),
            None,
        ));
    }
    let delivered = *reason == StopReason::FinalAnswer
        && final_content.is_some_and(|text| !text.trim().is_empty());
    let did_work =
        crate::completion::task_revision(so_far) > crate::completion::task_revision(history);
    if delivered && did_work && !claim.is_accepted() {
        pending.push(draft(
            session_id,
            EventType::COMPLETION_UNVERIFIED_RESULT_DELIVERED,
            serde_json::json!({
                "verdict": claim.as_str(),
                "task_revision": crate::completion::task_revision(so_far),
            }),
            Actor::Scheduler,
            Some(run_id),
            Some(turn_id),
            None,
        ));
    }
}
