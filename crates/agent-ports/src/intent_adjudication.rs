//! IntentAdjudicator port — the seam for the one call that answers what a
//! message means for the work in flight.
//!
//! The kernel's rule layer handles control words and nothing else. A message it
//! cannot route is queued untouched and marked `unclear`; this port is what that
//! marker is resolved by, once per message batch, over a bounded input.
//!
//! The trait has one method and no error variant, for the reason
//! [`DecisionAdvisor`](crate::decision::DecisionAdvisor) has none: a backend
//! that times out, fails, abstains, or answers something unreadable is one
//! recorded fact, and the kernel's answer to all of them is the same — no side
//! effect, and a clarification question for the person. Making those an `Err`
//! would invite a caller to retry or to guess, and both are the failure this
//! family exists to prevent.
//!
//! The adjudicator has no tool access, no file access, and no approval
//! authority, and this type gives it nowhere to acquire any: the only thing a
//! caller can ask it for is a suggestion, which the kernel then decides about.

use agent_protocol::intent_adjudication::AdjudicationSuggestion;

pub use agent_protocol::intent_adjudication::{
    AdjudicationInput, AdjudicationRelation, RecentLine,
};

/// What one adjudication call produced.
#[derive(Debug, Clone, PartialEq)]
pub enum AdjudicationOutcome {
    /// A suggestion that survived the pack's validation and the field contract.
    Suggested(AdjudicationSuggestion),
    /// The backend declined to answer. Not a failure: it was reached and said
    /// it had no opinion.
    Abstained,
    /// The backend could not be asked or answered something that is not the
    /// contract. `kind` is a stable label (`timeout`, `unavailable`,
    /// `protocol`, `payload`, `state_too_large`), never the backend's message.
    Failed { kind: String },
}

impl AdjudicationOutcome {
    /// A stable label for the outcome as a whole, for a status line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Suggested(_) => "suggested",
            Self::Abstained => "abstained",
            Self::Failed { .. } => "failed",
        }
    }
}

/// One call's result, with everything the kernel records about it.
///
/// The audit fields are here rather than looked up later because they describe
/// *this* call: which route answered, which revision, what question was asked
/// (by digest), what it was asked over (by digest), and how long it took. A
/// reader of the log can check a decision against the inputs it was derived
/// from without the process that made it.
#[derive(Debug, Clone, PartialEq)]
pub struct AdjudicationReport {
    /// The routing identity of the backend that answered — the same string a
    /// decision trace uses as its backend id.
    pub route_id: String,
    /// The model revision or artifact hash that backend is pinned to, when it
    /// pins one. `None` is the honest answer for a backend that does not.
    pub revision: Option<String>,
    /// The question pack revision the call was made against.
    pub pack_version: String,
    /// A digest of the question as asked. Stable across calls within a version.
    pub prompt_hash: String,
    /// A digest of the (already truncated) facts the call was made over.
    pub input_hash: String,
    /// Wall time of the call, measured by the caller.
    pub latency_ms: u64,
    pub outcome: AdjudicationOutcome,
}

impl AdjudicationReport {
    /// The report for a call that could not be made at all.
    ///
    /// No backend was asked, so there is no route to name; the report says that
    /// rather than borrowing a backend id and looking like a call where
    /// everybody abstained.
    pub fn unasked(kind: impl Into<String>) -> Self {
        Self {
            route_id: String::new(),
            revision: None,
            pack_version: String::new(),
            prompt_hash: String::new(),
            input_hash: String::new(),
            latency_ms: 0,
            outcome: AdjudicationOutcome::Failed { kind: kind.into() },
        }
    }
}

/// Port for the intent adjudicator.
#[async_trait::async_trait]
pub trait IntentAdjudicator: Send + Sync {
    /// Decide what `input`'s message asks for, and report what happened.
    ///
    /// One call. The implementation is responsible for its own deadline; the
    /// caller does not retry, because a second call is a second opinion nobody
    /// asked for.
    async fn adjudicate(&self, input: &AdjudicationInput) -> AdjudicationReport;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unasked_report_names_no_backend() {
        let report = AdjudicationReport::unasked("state_too_large");
        assert!(report.route_id.is_empty());
        assert_eq!(
            report.outcome,
            AdjudicationOutcome::Failed { kind: "state_too_large".to_string() }
        );
        assert_eq!(report.outcome.as_str(), "failed");
    }

    #[test]
    fn the_outcome_labels_are_stable() {
        assert_eq!(AdjudicationOutcome::Abstained.as_str(), "abstained");
        assert_eq!(AdjudicationRelation::NewTask.as_str(), "new_task");
        assert!(AdjudicationRelation::ReviseTask.touches_queue());
        assert!(!AdjudicationRelation::Continue.touches_queue());
    }
}
