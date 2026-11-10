//! PermissionClassifier port — the seam for the one call that judges a boundary
//! action.
//!
//! Policy answers "does a rule allow this capability"; the workspace boundary
//! answers "is this path inside the tree"; neither answers "should *this*
//! command, against *this* origin, with *this* path, run". That question is what
//! this port exists for, and it is asked only for an action the kernel already
//! rates `L2` and above — the deterministic layers have had their say before a
//! backend is reached.
//!
//! The trait returns a [`ClassifierTrace`] and never an `Err`, for the reason
//! [`IntentAdjudicator`](crate::intent_adjudication::IntentAdjudicator) does:
//! a timeout, a failure, an abstention and an unreadable answer are one recorded
//! fact each, and the kernel's answer to all four is the same — stop and ask a
//! person. An `Err` would invite a caller to retry or to treat "no answer" as
//! "no objection", and the second is the failure this family exists to prevent.
//!
//! The trace carries the answer *and* the audit of the call: which route, which
//! revision, which pack, a digest of the question and of the (already truncated)
//! input, and how long it took. The classifier has no tool access, no file
//! access and no approval authority, and this type gives it nowhere to acquire
//! any: the only thing a caller can ask for is an opinion.

pub use agent_protocol::permission_classifier::{
    ClassifierOutcome, ClassifierTrace, PermissionClassifierInput, PermissionSuggestion,
    SuggestionDecision,
};

/// Port for the permission classifier.
#[async_trait::async_trait]
pub trait PermissionClassifier: Send + Sync {
    /// Judge `input`'s action and report what happened.
    ///
    /// One call. The implementation is responsible for its own deadline; the
    /// caller does not retry, because a second opinion about the same action is
    /// a second chance for a flaky backend to say yes.
    async fn classify(&self, input: &PermissionClassifierInput) -> ClassifierTrace;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_trace_is_the_one_that_asks() {
        let trace = ClassifierTrace::unavailable("");
        assert_eq!(trace.outcome, ClassifierOutcome::Unavailable);
        assert!(trace.outcome.asks());
        assert!(trace.suggestion.is_none());
    }

    #[test]
    fn the_port_names_the_contract_types_it_carries() {
        // A compile-time statement: the types a caller of this port sees are the
        // protocol's, not a second copy beside them.
        fn takes(_: &PermissionClassifierInput) -> Option<PermissionSuggestion> {
            None
        }
        let _ = takes;
        assert_eq!(SuggestionDecision::Allow.as_str(), "allow");
    }
}
