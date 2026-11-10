//! Session status: the three axes a turned-looking session is read along.
//!
//! A turn ending well says one thing — the run stopped without an error — and
//! readers used to hear it as "the task is done". That is the reading the
//! 2026-11-01 incident exposed: an answer that had never been run, or a run
//! that was still exploring, both looked finished because "the run completed".
//! Three orthogonal facts are needed instead:
//!
//! * [`RunStatus`] — what the execution itself is doing (running, ended,
//!   interrupted, failed). This is about the process, not the task.
//! * [`DeliveryStatus`] — whether the person actually received an answer to
//!   the request. A run can end without one (a limit, a failure, a machine
//!   check that passes with nothing to say).
//! * [`VerificationStatus`] — whether the result has been checked against a
//!   criterion, and whether that check still covers the work on the table.
//!
//! [`CompletionStatus`] is **derived** from those three and is the only word
//! that may be read as "this task is complete": [`CompletionStatus::VerifiedComplete`].
//! There is deliberately no `unverified_complete`: a finished-looking run with
//! no fresh verification is [`CompletionStatus::BlockedOrParked`], not a
//! completion with an asterisk. A task that was only ever answered with
//! information has no completion claim at all — its `NotApplicable`
//! verification means "结果已交付，无机器验收标准", which is not counted as a
//! completion and cannot authorize a later side effect.
//!
//! These are pure vocabulary. The derivation lives in the kernel
//! (`agent_kernel::status`), which is why this module has no fold of its own:
//! the protocol crate states what the words mean, and the reader that owns the
//! event stream states which one is true.

use serde::{Deserialize, Serialize};

/// What the session's latest run is doing.
///
/// Derived from the latest `run/started` and the terminal event that closes it.
/// A run that yielded for an approval or an input is still [`Self::InProgress`]:
/// it can resume, so it is neither finished nor interrupted — the parking is a
/// property of the completion axis, not of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Started and not closed; includes a run waiting on a decision.
    InProgress,
    /// Ended because the loop said it was over, or because the operator's
    /// completion criterion passed.
    Completed,
    /// The person stopped it, or a recovery closed what a dead process left.
    Interrupted,
    /// Ended on an error the loop could not continue past.
    Failed,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }
}

/// Whether the person received an answer to the request.
///
/// Read from the latest settled turn's terminal payload: an answer is a
/// `final_content` the turn ended with, and text that was cut off (max output,
/// content filter, token budget, a stall) is the beginning of an answer rather
/// than one, so it does not count as delivered (CONTRACT §7 #131, #132).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// The last settled turn ended with an answer the user can read.
    Delivered,
    /// No answer was handed over — still running, stopped, failed, or ended
    /// with only partial text.
    Unanswered,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Unanswered => "unanswered",
        }
    }
}

/// Whether the work on the table has been verified, and how fresh that is.
///
/// Before the evidence ledger (slice A5) the only deterministic verification
/// fact this build records is `completion/checked` — the operator's criterion,
/// run at a step boundary. "A persisted change happened after the check" is
/// therefore approximated as "a tool call happened after the check": a read
/// that changed nothing still marks the older check stale. That errs toward
/// `stale`, which is the honest direction — reporting a stale check as fresh
/// is the failure this axis exists to prevent. Slice A5 replaces the
/// approximation with evidence keyed to a task revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// A completion criterion passed and nothing has run since.
    Verified,
    /// Work happened and no criterion says it met its goal.
    Unverified,
    /// A criterion passed, then work continued; the pass no longer covers the
    /// result on the table.
    Stale,
    /// A completed run did no tool work at all: there is nothing a machine
    /// criterion could check, and the answer is information rather than a
    /// change to the world.
    NotApplicable,
    /// Verification could not stand: the run failed or was interrupted, or the
    /// criterion itself could not run.
    Blocked,
}

impl VerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverified => "unverified",
            Self::Stale => "stale",
            Self::NotApplicable => "not_applicable",
            Self::Blocked => "blocked",
        }
    }
}

/// The derived completion claim — the only two things a reader may be told.
///
/// [`Self::VerifiedComplete`] requires both a completed run and fresh
/// verification. Everything else that claims to be finished is
/// [`Self::BlockedOrParked`]: failed, interrupted, parked on a decision, or
/// simply done without anything checking it. A task with no machine criterion
/// (information delivered) has *no* completion claim — a caller reports it as
/// `None`, and the user-facing sentence is "结果已交付，无机器验收标准".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    /// Completed and verified against the current work.
    VerifiedComplete,
    /// Not complete, and not going to be without something changing: failure,
    /// interruption, a parked run, or missing/stale verification.
    BlockedOrParked,
}

impl CompletionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedComplete => "verified_complete",
            Self::BlockedOrParked => "blocked_or_parked",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_names_are_snake_case_and_stable() {
        assert_eq!(serde_json::to_string(&RunStatus::InProgress).unwrap(), "\"in_progress\"");
        assert_eq!(serde_json::to_string(&DeliveryStatus::Unanswered).unwrap(), "\"unanswered\"");
        assert_eq!(
            serde_json::to_string(&VerificationStatus::NotApplicable).unwrap(),
            "\"not_applicable\""
        );
        assert_eq!(
            serde_json::to_string(&CompletionStatus::VerifiedComplete).unwrap(),
            "\"verified_complete\""
        );
    }

    #[test]
    fn every_status_name_matches_its_wire_form() {
        // The text and JSON views have to agree: `status` prints `as_str` and
        // `status --json` serializes the value, and two spellings of one fact
        // are how a consumer starts parsing the wrong one.
        for (value, wire) in [
            (RunStatus::InProgress, "in_progress"),
            (RunStatus::Completed, "completed"),
            (RunStatus::Interrupted, "interrupted"),
            (RunStatus::Failed, "failed"),
        ] {
            assert_eq!(value.as_str(), wire);
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        for (value, wire) in
            [(DeliveryStatus::Delivered, "delivered"), (DeliveryStatus::Unanswered, "unanswered")]
        {
            assert_eq!(value.as_str(), wire);
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        for (value, wire) in [
            (VerificationStatus::Verified, "verified"),
            (VerificationStatus::Unverified, "unverified"),
            (VerificationStatus::Stale, "stale"),
            (VerificationStatus::NotApplicable, "not_applicable"),
            (VerificationStatus::Blocked, "blocked"),
        ] {
            assert_eq!(value.as_str(), wire);
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        for (value, wire) in [
            (CompletionStatus::VerifiedComplete, "verified_complete"),
            (CompletionStatus::BlockedOrParked, "blocked_or_parked"),
        ] {
            assert_eq!(value.as_str(), wire);
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
    }
}
