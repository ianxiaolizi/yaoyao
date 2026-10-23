//! The deadline the *loop* pays: a bound on the whole advisory call.
//!
//! [`DecisionBroker`](crate::DecisionBroker) bounds each backend separately
//! ([`DEFAULT_TIMEOUT`](crate::DEFAULT_TIMEOUT) per backend), so the time a turn
//! can lose to shadow is the sum over the backends plus the baseline — a number
//! that grows with every backend `with_backend` adds, and one no single backend
//! can see. This wrapper bounds that sum at the seam the loop actually calls.
//!
//! It is a decorator rather than a loop change on purpose: `agent-loop` has no
//! runtime dependency (a turn is driven by whatever executor the caller runs it
//! on), and putting a timer there would drag one in. The bound belongs to the
//! assembly that decided which backends to consult — the same place that knows
//! whether any of them can block.
//!
//! What a cut-off call leaves behind is a trace with no backend reports and
//! `aborted: "deadline"`: "we stopped waiting" is recorded as the absence of
//! evidence, never as an abstention, and the loop records it like any other
//! trace without reading it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_ports::decision::{DecisionAdvisor, DecisionMode};
use agent_ports::model_router::{ModelRoute, RouteRequest};
use agent_protocol::decision::DecisionTrace;

use crate::model_route;

/// How long the whole advisory call may take before the turn stops waiting.
///
/// Twice the per-backend timeout: enough for the baseline plus one real backend
/// to answer under their own deadlines, short enough that a shadow decision
/// cannot become the slowest thing in a turn.
pub const DEFAULT_ADVISOR_DEADLINE: Duration = Duration::from_millis(500);

/// An advisor that cannot hold the call open longer than `deadline`.
///
/// It is deliberately not generic over the decision family: the trace it
/// synthesizes when it cuts a call off has to be about the same questions the
/// inner advisor would have been asked, and today exactly one family has an
/// advisor at all. A second family brings its own pack and its own wrapper
/// rather than teaching this one to guess.
///
/// The synthesized trace is comparable to a settled one by `pack_version` and
/// the question ids — the family revision — not by `pack_id`, which
/// [`QuestionPack::new`](agent_protocol::decision::QuestionPack::new) mints per
/// instance. A cut-off trace therefore carries a pack id of its own rather than
/// a fabricated reference to someone else's.
pub struct DeadlineAdvisor {
    inner: Arc<dyn DecisionAdvisor>,
    mode: DecisionMode,
    deadline: Duration,
}

impl DeadlineAdvisor {
    /// Wrap `inner`, giving it [`DEFAULT_ADVISOR_DEADLINE`] to answer.
    pub fn new(inner: Arc<dyn DecisionAdvisor>, mode: DecisionMode) -> Self {
        Self { inner, mode, deadline: DEFAULT_ADVISOR_DEADLINE }
    }

    /// Use a different deadline. Tests use this; production wiring does not.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

#[async_trait::async_trait]
impl DecisionAdvisor for DeadlineAdvisor {
    async fn advise_model_route(
        &self,
        request: &RouteRequest,
        chosen: &ModelRoute,
    ) -> DecisionTrace {
        let started = Instant::now();
        match tokio::time::timeout(self.deadline, self.inner.advise_model_route(request, chosen))
            .await
        {
            // A trace that arrived is returned untouched, including its identity:
            // wrapping is not a rewrite, and a caller comparing traces must not
            // see the wrapper in them.
            Ok(trace) => trace,
            Err(_elapsed) => {
                let mut trace = DecisionTrace::new(&model_route::pack(), self.mode);
                trace.aborted = Some("deadline".to_string());
                trace.latency_ms = started.elapsed().as_millis() as u64;
                trace
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use agent_ports::model_router::{ModelBudget, TaskClass};
    use agent_protocol::ids::{EventSeq, SessionId, TurnId};
    use agent_testkit::FakeDecisionAdvisor;

    fn request() -> RouteRequest {
        RouteRequest {
            session_id: SessionId::from_string("deadline-session"),
            turn_id: TurnId::new(),
            task: TaskClass::new(TaskClass::INTERACTIVE),
            context_revision: EventSeq::ZERO,
            message_count: 1,
            required_capabilities: Vec::new(),
            budget: ModelBudget::default(),
        }
    }

    fn route() -> ModelRoute {
        ModelRoute {
            model: "echo".to_string(),
            provider: "echo".to_string(),
            tier: agent_ports::model_router::ModelTier::Main,
        }
    }

    #[tokio::test]
    async fn a_fast_advisor_passes_its_trace_through_untouched() {
        let inner = Arc::new(FakeDecisionAdvisor::returning(DecisionTrace::new(
            &model_route::pack(),
            DecisionMode::Shadow,
        )));
        let expected = inner.advise_model_route(&request(), &route()).await;

        let advisor = DeadlineAdvisor::new(inner, DecisionMode::Shadow);
        let trace = advisor.advise_model_route(&request(), &route()).await;

        assert_eq!(trace, expected, "wrapping must not alter the trace it received");
    }

    #[tokio::test]
    async fn an_advisor_that_never_answers_is_cut_off_at_the_deadline() {
        let advisor =
            DeadlineAdvisor::new(Arc::new(FakeDecisionAdvisor::hanging()), DecisionMode::Shadow)
                .with_deadline(Duration::from_millis(20));

        let started = Instant::now();
        let trace = advisor.advise_model_route(&request(), &route()).await;
        let elapsed = started.elapsed();

        assert!(elapsed < Duration::from_secs(5), "the call was cut off, not waited out");
        assert_eq!(trace.aborted.as_deref(), Some("deadline"));
        assert!(trace.chosen.is_none());
        assert!(trace.backends.is_empty(), "no backend report was obtained");
    }

    #[tokio::test]
    async fn a_cut_off_trace_is_about_the_same_family_revision_as_a_settled_one() {
        let advisor =
            DeadlineAdvisor::new(Arc::new(FakeDecisionAdvisor::hanging()), DecisionMode::Shadow)
                .with_deadline(Duration::from_millis(20));

        let trace = advisor.advise_model_route(&request(), &route()).await;
        let pack = model_route::pack();

        // Comparability is the family revision — version plus the questions asked
        // — not `pack_id`, which a pack mints per instance and which therefore
        // differs between any two packs, settled or not.
        assert_eq!(trace.family, pack.family);
        assert_eq!(trace.pack_version, pack.version);
        assert_eq!(trace.question_ids, pack.question_ids());
        assert_eq!(trace.mode, DecisionMode::Shadow);
    }
}
