//! The independent security review, taken at the end of a turn.
//!
//! A high-risk task — one that ran an L2/L3 action — may not report its answer
//! until a reviewer that did not choose the action has passed it (B9, CONTRACT
//! §7 #183). This module is where the scheduler asks: it runs before the claim
//! and before the terminal events are written, so a turn whose review refused
//! cannot be read out of order as a delivery with a refusal bolted on after.
//!
//! It lives beside the turn logic rather than inside it for the reason
//! [`super::rebuilt`] does: the ordering that matters is three lines long, and
//! the reasoning that makes it correct is not. Keeping the reasoning here also
//! keeps `scheduler.rs` inside its size ratchet.

use std::sync::Arc;

use agent_ports::agent_loop::{EventSink, StopReason};
use agent_ports::security_reviewer::SecurityReviewer;
use agent_protocol::events::{EventDraft, StoredEvent};
use agent_protocol::ids::{EventSeq, RunId, SessionId, TurnId};
use serde_json::Value;

use super::journal::SessionJournal;
use super::{Scheduler, SchedulerError};

impl Scheduler {
    /// Ask `reviewer` about the L2/L3 actions a session has run, before a turn
    /// reports an answer (B9).
    ///
    /// Set by the app layer, which is the only place that knows whether this
    /// build has a sidecar at all. The scheduler does not decide when a review
    /// is needed — [`crate::security::review_input`] reads that from the log —
    /// and a scheduler with no reviewer records nothing, which the completion
    /// gate reads as "no review" rather than "a review that passed".
    pub fn with_security_reviewer(mut self, reviewer: Arc<dyn SecurityReviewer>) -> Self {
        self.security = Some(reviewer);
        self
    }

    /// Review the work behind `stop_reason`, and say how the turn ends.
    ///
    /// Returns the reason unchanged unless a review refused the answer, in which
    /// case it returns [`StopReason::SecurityBlocked`] and marks `payload`
    /// incomplete: the model's text is still there, and nothing that reads the
    /// terminal event will take it as a delivered result.
    ///
    /// Only a turn that is about to report an answer is reviewed. A turn that
    /// yielded to a limit or failed makes no claim for a review to stand behind.
    ///
    /// The log is read here and again after the review is written: the claim the
    /// gate refuses is written in the same batch as the terminal events, and a
    /// claim decided from a log that has not seen the review would be a verdict
    /// about a record that does not exist. The second read is the snapshot the
    /// caller folds for the claim and the parked record.
    pub(super) async fn review_before_claim(
        &self,
        (session_id, run_id, turn_id, stop_reason): (&SessionId, &RunId, &TurnId, StopReason),
        (pending, journal, payload): (&mut Vec<EventDraft>, &Arc<SessionJournal>, &mut Value),
    ) -> Result<(StopReason, Vec<StoredEvent>), SchedulerError> {
        let mut so_far = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        if !matches!(stop_reason, StopReason::FinalAnswer | StopReason::Completed) {
            return Ok((stop_reason, so_far));
        }
        let Some(reviewer) = self.security.as_deref() else {
            return Ok((stop_reason, so_far));
        };
        let Some(input) = crate::security::review_input(session_id, &so_far) else {
            return Ok((stop_reason, so_far));
        };
        // One call, no retry: a second review of the same work is a second
        // chance for a flaky backend to clear it.
        let report = reviewer.review(input.clone()).await;
        let recorded = crate::security::record(session_id, run_id, turn_id, &input, report)?;
        pending.extend(recorded.drafts);
        EventSink::commit(journal.as_ref(), std::mem::take(pending)).await?;
        so_far = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;

        let Some(reason) = recorded.blocked else {
            return Ok((stop_reason, so_far));
        };
        block_delivery(payload, reason, &recorded.check_id);
        Ok((StopReason::SecurityBlocked, so_far))
    }
}

/// Write the refusal onto the terminal payload.
///
/// Three facts, and each is read by a different layer: `stop_reason` by the CLI
/// and the report, `incomplete` by the delivery axis (the same marker a cut-off
/// turn carries), and `security_review` by whoever needs to join the refusal to
/// the record it came from.
fn block_delivery(payload: &mut Value, reason: String, check_id: &str) {
    payload["stop_reason"] = serde_json::json!(StopReason::SecurityBlocked);
    payload["incomplete"] = serde_json::json!(true);
    payload["security_review"] = serde_json::json!({ "blocked": reason, "check_id": check_id });
}
