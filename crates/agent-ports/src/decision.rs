//! FastDecisionProvider port — the seam for decisions the main model does not
//! make.
//!
//! The value types live in `agent_protocol::decision` (protocol-first) and are
//! re-exported here so a caller needs one import path. Two traits sit on top:
//! [`FastDecisionProvider`] is what a backend implements; [`DecisionAdvisor`]
//! is the single method the agent loop sees. The loop needs to know that a
//! shadow decision happened and what it said — never how a question is phrased
//! or which backend answered it.

use crate::model_router::{ModelRoute, RouteRequest};

pub use agent_protocol::decision::{
    DecisionAnswer, DecisionBackendOutcome, DecisionBackendReport, DecisionFamily, DecisionKind,
    DecisionMode, DecisionOption, DecisionQuestion, DecisionResult, DecisionState,
    DecisionStateError, DecisionTrace, DecisionValue, MAX_FACTS_BYTES, QuestionPack,
};

/// Errors from a FastDecisionProvider implementation.
///
/// A timeout is deliberately absent: the caller measures the deadline, so a
/// backend cannot report "timed out" for a call that was never cut off. The
/// three kinds below are the ones only the backend can know about.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DecisionError {
    /// The backend is not reachable or has no usable revision loaded.
    #[error("decision backend unavailable: {0}")]
    Unavailable(String),
    /// The backend answered something that is not a valid result for the pack.
    #[error("decision protocol error: {0}")]
    Protocol(String),
    /// The backend could not be handed the question.
    #[error("decision payload error: {0}")]
    Payload(String),
}

impl DecisionError {
    /// A stable machine-readable kind, safe to persist in a trace — unlike the
    /// display message, which may embed backend text.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::Protocol(_) => "protocol",
            Self::Payload(_) => "payload",
        }
    }
}

/// Port for a fast-decision backend.
#[async_trait::async_trait]
pub trait FastDecisionProvider: Send + Sync {
    /// Answer one pack over one state.
    ///
    /// Declining is a normal outcome, not an error: return
    /// [`DecisionResult::abstained`] when the backend has no opinion. An
    /// abstention and a failure are recorded differently because they mean
    /// different things — "I don't know" is not "I was broken".
    async fn decide(
        &self,
        pack: &QuestionPack,
        state: &DecisionState,
    ) -> Result<DecisionResult, DecisionError>;

    /// A short stable id, used as this backend's key in a trace.
    fn provider_id(&self) -> String;

    /// The model revision or artifact hash this backend is pinned to, when it
    /// pins one. A decision that cannot be attributed to a revision
    /// cannot be replayed, so an unpinned backend says so with `None` rather
    /// than inventing a label.
    fn revision(&self) -> Option<String> {
        None
    }
}

/// The agent loop's whole view of fast decisions.
///
/// One method, and no error variant. A shadow decision is an observation the
/// loop records; it has no way to fail a turn, and — in [`DecisionMode::Shadow`]
/// — no way to change one. That is the type telling the truth about what the
/// acceptance criterion "FastDecision shadow does not affect the main chain"
/// means, rather than a comment promising it.
#[async_trait::async_trait]
pub trait DecisionAdvisor: Send + Sync {
    /// Run the turn-start model-route decision and return the trace to record.
    ///
    /// `chosen` is the route the [`crate::model_router::ModelRouter`] settled
    /// on. Whether the advice agrees with it changes nothing here: in shadow
    /// mode the trace is where a disagreement becomes visible, not in the turn.
    async fn advise_model_route(
        &self,
        request: &RouteRequest,
        chosen: &ModelRoute,
    ) -> DecisionTrace;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_error_kinds_are_stable_labels() {
        assert_eq!(DecisionError::Unavailable("x".into()).kind(), "unavailable");
        assert_eq!(DecisionError::Protocol("x".into()).kind(), "protocol");
        assert_eq!(DecisionError::Payload("x".into()).kind(), "payload");
    }

    #[test]
    fn the_family_constants_survive_the_re_export() {
        assert_eq!(DecisionFamily::MODEL_ROUTE, "model_route");
        assert_eq!(DecisionMode::Shadow.as_str(), "shadow");
        assert_eq!(MAX_FACTS_BYTES, 8192);
    }
}
