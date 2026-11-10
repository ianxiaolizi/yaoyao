//! SecurityReviewer port — the seam for the independent judgement a high-risk
//! task needs before it may be called done.
//!
//! The completion gate (CONTRACT §7 #170) answers "does a check cover the work
//! on the table?". For a task that ran an L2/L3 action there is one question
//! that gate cannot answer by reading a pass: *was the action itself
//! acceptable?* A verifier runs a program the agent wrote; a reviewer looks at
//! what the agent did, from outside the agent, with nothing it could operate.
//!
//! The trait returns evidence and never an `Err`, for the reason the permission
//! classifier does: a timeout, a failure, an abstention and an unreadable answer
//! are one recorded fact each, and the kernel's answer to all four is the same —
//! block the delivery. An `Err` would invite a caller to retry, and a retry is a
//! second chance for a flaky reviewer to say yes.
//!
//! What a reviewer is *not*: an agent. It gets no tools, no file access, no
//! network and no approval authority, and the input type carries only
//! references, digests and a redacted summary — there is nowhere to put a page
//! body, a file's contents or a conversation, so there is nothing for an
//! injected instruction to ride in on.

pub use agent_protocol::evidence::Evidence;
pub use agent_protocol::security_review::{
    FindingSeverity, SecurityFinding, SecurityReviewInput, SecurityVerdict, SecurityVerdictKind,
};

/// What one review produced: the verdict (when there is one) and the record
/// that will be written beside it.
///
/// `evidence` is always present, including for a timeout: a review that could
/// not run is a fact the gate must be able to read, and an absent event is
/// indistinguishable from "nobody asked".
#[derive(Debug, Clone)]
pub struct SecurityReviewReport {
    /// The reviewer's verdict, when it formed one.
    pub verdict: Option<SecurityVerdict>,
    /// The evidence to record. Its `status` already reflects the verdict:
    /// `passed` only for an accepting pass, `failed` for a refusal, `error` for
    /// a review that could not run or could not be read.
    pub evidence: Evidence,
    /// Why there is no usable verdict, when the verdict is not a pass: `timeout`,
    /// `backend`, `unreadable`, `low_confidence`. Read by the CLI and the log,
    /// never by the gate, which reads `evidence.status`.
    pub failure_kind: Option<String>,
}

/// Port for the independent security reviewer.
#[async_trait::async_trait]
pub trait SecurityReviewer: Send + Sync {
    /// Review `input` and report what happened.
    ///
    /// One call. The implementation owns its deadline; the caller does not
    /// retry, because a second review of the same work is a second chance for a
    /// flaky backend to clear it.
    async fn review(&self, input: SecurityReviewInput) -> SecurityReviewReport;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::evidence::{EvidenceKind, EvidenceStatus};

    #[test]
    fn a_report_with_no_verdict_is_still_evidence_a_gate_can_read() {
        let mut evidence =
            Evidence::error("security_review/1".to_string(), 2, "security_reviewer".to_string());
        evidence.kind = EvidenceKind::SecurityReview;
        let report = SecurityReviewReport {
            verdict: None,
            evidence,
            failure_kind: Some("timeout".to_string()),
        };
        assert_eq!(report.evidence.status, EvidenceStatus::Error);
        assert_eq!(report.evidence.kind.as_str(), "security_review");
        assert_eq!(report.failure_kind.as_deref(), Some("timeout"));
    }

    #[test]
    fn the_port_names_the_contract_types_it_carries() {
        fn takes(input: &SecurityReviewInput) -> Option<SecurityVerdict> {
            let _ = input.input_hash();
            None
        }
        let _ = takes;
        assert_eq!(SecurityVerdictKind::Pass.as_str(), "pass");
        assert_eq!(FindingSeverity::High.as_str(), "high");
    }
}
