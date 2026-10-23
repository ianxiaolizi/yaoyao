//! DecisionBroker: backend selection, degradation, shadow comparison, audit.
//!
//! One call runs every configured backend **and** the rule baseline, then
//! settles on the first backend that answered, falling back to the rule. The
//! losers are kept in the trace on purpose: a comparison that discarded the
//! comparison side would not be a comparison, and the whole reason to run a
//! fast-decision model in shadow is to find out where it disagrees.
//!
//! Nothing here fails a caller. A backend that times out, errors, abstains, or
//! returns something malformed is one recorded fact among several — the broker
//! keeps going and the trace says what happened. Only misconfiguration (asking
//! for a mode this build cannot honor) is an error, and it is caught when the
//! broker is built rather than when a turn is running.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_ports::decision::{
    DecisionAdvisor, DecisionBackendOutcome, DecisionBackendReport, DecisionMode, DecisionResult,
    DecisionState, DecisionTrace, FastDecisionProvider, QuestionPack,
};
use agent_ports::model_router::{ModelRoute, RouteRequest};

use crate::model_route;

/// How long one backend gets before it is treated as failed.
///
/// A fast decision is on the critical path of a turn even when its answer is
/// only recorded, so its budget is milliseconds. A backend that cannot answer
/// in that window is not fast, and the trace will say so every time rather than
/// once per incident.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(250);

/// Why a broker could not be built.
#[derive(Debug, thiserror::Error)]
pub enum DecisionBrokerError {
    /// Promoting a family out of shadow is gated on pinned revisions, held-out
    /// evaluation, confidence calibration, latency caps, canary, and a rollback
    /// drill. None of that exists yet, so the honest answer is to refuse
    /// the mode rather than to accept a label nothing enforces.
    #[error("decision mode '{0}' is not available: leaving shadow requires the promotion gates")]
    ModeNotAvailable(DecisionMode),
}

/// Runs backends and the rule baseline over one question pack.
pub struct DecisionBroker {
    mode: DecisionMode,
    /// Backends tried in order. The rule baseline is deliberately not among
    /// them: "the rule backend always exists" is a property of the type, not of
    /// however a caller happened to assemble a list.
    backends: Vec<Arc<dyn FastDecisionProvider>>,
    rule: Arc<dyn FastDecisionProvider>,
    timeout: Duration,
}

impl DecisionBroker {
    /// A broker in `mode` with the given rule baseline.
    pub fn new(
        mode: DecisionMode,
        rule: Arc<dyn FastDecisionProvider>,
    ) -> Result<Self, DecisionBrokerError> {
        if mode != DecisionMode::Shadow {
            return Err(DecisionBrokerError::ModeNotAvailable(mode));
        }
        Ok(Self { mode, backends: Vec::new(), rule, timeout: DEFAULT_TIMEOUT })
    }

    /// Add a backend, tried in the order added.
    pub fn with_backend(mut self, backend: Arc<dyn FastDecisionProvider>) -> Self {
        self.backends.push(backend);
        self
    }

    /// Override the per-backend deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The mode this broker runs in.
    pub fn mode(&self) -> DecisionMode {
        self.mode
    }

    /// Run every backend and the rule baseline over one pack.
    ///
    /// Public because it is family-agnostic: a new family supplies its own pack
    /// and state and gets the same selection, degradation, and comparison
    /// without touching this code.
    pub async fn decide(&self, pack: &QuestionPack, state: &DecisionState) -> DecisionTrace {
        let started = Instant::now();
        let mut trace = DecisionTrace::new(pack, self.mode);

        for backend in &self.backends {
            let report = run_backend(backend.as_ref(), pack, state, self.timeout).await;
            settle(&mut trace, &report);
            trace.backends.push(report);
        }

        // Always, not only after a failure: the baseline answer is what makes
        // the comparison in this trace meaningful, and it is a pure function of
        // the state.
        let baseline = run_backend(self.rule.as_ref(), pack, state, self.timeout).await;
        settle(&mut trace, &baseline);
        trace.backends.push(baseline);

        trace.latency_ms = started.elapsed().as_millis() as u64;
        trace
    }
}

/// Let a backend's answer become the chosen one, unless one already has.
fn settle(trace: &mut DecisionTrace, report: &DecisionBackendReport) {
    settle_into(&mut trace.chosen, &mut trace.chosen_backend, report);
}

/// The selection rule, over a pair of slots rather than over a trace.
///
/// Split out so that it has exactly one implementation: [`crate::replay`]
/// re-derives a trace's conclusion by running this same function over the same
/// reports, and a second copy of the rule would make that check a comparison of
/// two guesses.
pub(crate) fn settle_into(
    chosen: &mut Option<DecisionResult>,
    chosen_backend: &mut Option<String>,
    report: &DecisionBackendReport,
) {
    if chosen.is_some() {
        return;
    }
    if let DecisionBackendOutcome::Answered(result) = &report.outcome {
        *chosen = Some(result.clone());
        *chosen_backend = Some(report.backend_id.clone());
    }
}

/// Run one backend under the deadline and report what it did.
///
/// The reported `error_kind` is a stable label, never the backend's message: a
/// trace is an audit record, not a copy of whatever a backend chose to say.
/// A backend that fails validation is `protocol` — it was reached, it answered
/// something that is not a valid answer to this pack.
async fn run_backend(
    backend: &dyn FastDecisionProvider,
    pack: &QuestionPack,
    state: &DecisionState,
    timeout: Duration,
) -> DecisionBackendReport {
    let started = Instant::now();
    let outcome = match tokio::time::timeout(timeout, backend.decide(pack, state)).await {
        Err(_elapsed) => DecisionBackendOutcome::Failed { error_kind: "timeout".to_string() },
        Ok(Err(error)) => DecisionBackendOutcome::Failed { error_kind: error.kind().to_string() },
        Ok(Ok(result)) => match pack.validate(&result) {
            Err(_) => DecisionBackendOutcome::Failed { error_kind: "protocol".to_string() },
            Ok(()) if result.abstained => DecisionBackendOutcome::Abstained,
            Ok(()) => DecisionBackendOutcome::Answered(result),
        },
    };

    DecisionBackendReport {
        backend_id: backend.provider_id(),
        revision: backend.revision(),
        outcome,
        latency_ms: started.elapsed().as_millis() as u64,
    }
}

#[async_trait::async_trait]
impl DecisionAdvisor for DecisionBroker {
    async fn advise_model_route(
        &self,
        request: &RouteRequest,
        chosen: &ModelRoute,
    ) -> DecisionTrace {
        let pack = model_route::pack();
        match model_route::state(request, chosen) {
            Ok(state) => self.decide(&pack, &state).await,
            Err(error) => {
                // No backend was asked, and the trace has to say that rather
                // than look like a call where everybody abstained.
                let mut trace = DecisionTrace::new(&pack, self.mode);
                trace.aborted = Some(error.kind().to_string());
                trace
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::{RuleDecision, RuleDecisionProvider};
    use agent_ports::decision::{
        DecisionError, DecisionFamily, DecisionValue, FastDecisionProvider,
    };
    use agent_ports::model_router::{ModelBudget, ModelCapability, ModelTier, TaskClass};
    use agent_protocol::ids::{EventSeq, SessionId, TurnId};
    use agent_testkit::{FakeDecisionBehaviour, FakeDecisionProvider};
    use std::time::Duration;

    const TIER: &str = model_route::TIER_QUESTION;

    fn fast() -> DecisionValue {
        DecisionValue::Categorical { choice: ModelTier::Fast.as_str().to_string() }
    }

    fn main() -> DecisionValue {
        DecisionValue::Categorical { choice: ModelTier::Main.as_str().to_string() }
    }

    /// A baseline that answers with whatever the router chose.
    fn router_baseline() -> Arc<dyn FastDecisionProvider> {
        Arc::new(RuleDecisionProvider::new(vec![RuleDecision::from_fact(
            DecisionFamily::MODEL_ROUTE,
            TIER,
            model_route::CHOSEN_TIER_FACT,
        )]))
    }

    fn request() -> RouteRequest {
        RouteRequest {
            session_id: SessionId::from_string("s"),
            turn_id: TurnId::from_string("t"),
            task: TaskClass::new(TaskClass::INTERACTIVE),
            context_revision: EventSeq::new(11),
            message_count: 3,
            required_capabilities: vec![ModelCapability::new(ModelCapability::STREAMING)],
            budget: ModelBudget::default(),
        }
    }

    fn chosen() -> ModelRoute {
        ModelRoute::new(ModelTier::Main, "openai-compat", "deepseek-flash")
    }

    fn broker(backends: Vec<Arc<dyn FastDecisionProvider>>) -> DecisionBroker {
        let mut broker = DecisionBroker::new(DecisionMode::Shadow, router_baseline())
            .expect("shadow is available");
        for backend in backends {
            broker = broker.with_backend(backend);
        }
        broker
    }

    async fn decide(broker: &DecisionBroker) -> DecisionTrace {
        let pack = model_route::pack();
        let state = model_route::state(&request(), &chosen()).expect("state is small");
        broker.decide(&pack, &state).await
    }

    fn chosen_choice(trace: &DecisionTrace) -> Option<String> {
        match trace.chosen.as_ref()?.answer_for(TIER)?.value.clone() {
            DecisionValue::Categorical { choice } => Some(choice),
            _ => None,
        }
    }

    #[tokio::test]
    async fn a_backend_answer_wins_and_the_baseline_is_still_recorded() {
        let backend = Arc::new(FakeDecisionProvider::answering("local", TIER, fast()));
        let trace = decide(&broker(vec![backend])).await;

        assert_eq!(trace.chosen_backend.as_deref(), Some("local"));
        assert_eq!(chosen_choice(&trace).as_deref(), Some("fast"));
        assert!(trace.aborted.is_none());
        // The comparison side: the baseline ran even though nobody needed it.
        let baseline = trace.report_for("rule").expect("the baseline always runs");
        assert!(baseline.outcome.is_answered());
        let DecisionBackendOutcome::Answered(result) = &baseline.outcome else {
            panic!("the baseline answered");
        };
        assert_eq!(
            result.answer_for(TIER).map(|a| a.value.clone()),
            Some(main()),
            "the baseline repeats the deterministic router's choice"
        );
    }

    #[tokio::test]
    async fn the_trace_records_every_backend_that_ran_in_order() {
        let first =
            Arc::new(FakeDecisionProvider::answering("a", TIER, fast()).with_revision("rev-a"));
        let second =
            Arc::new(FakeDecisionProvider::answering("b", TIER, main()).with_revision("rev-b"));
        let trace = decide(&broker(vec![first, second])).await;

        let ids: Vec<&str> = trace.backends.iter().map(|r| r.backend_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "rule"]);
        assert_eq!(trace.backends[0].revision.as_deref(), Some("rev-a"));
        assert_eq!(trace.backends[1].revision.as_deref(), Some("rev-b"));
        assert!(trace.backends[2].revision.is_none());
    }

    #[tokio::test]
    async fn the_first_answering_backend_wins() {
        let first = Arc::new(FakeDecisionProvider::answering("a", TIER, fast()));
        let second = Arc::new(FakeDecisionProvider::answering("b", TIER, main()));
        let trace = decide(&broker(vec![first, second])).await;

        assert_eq!(trace.chosen_backend.as_deref(), Some("a"));
        assert_eq!(chosen_choice(&trace).as_deref(), Some("fast"));
    }

    #[tokio::test]
    async fn a_failed_backend_degrades_to_the_baseline() {
        let backend = Arc::new(FakeDecisionProvider::new(
            "local",
            FakeDecisionBehaviour::Fail(DecisionError::Unavailable("sidecar down".to_string())),
        ));
        let trace = decide(&broker(vec![backend])).await;

        assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
        assert_eq!(chosen_choice(&trace).as_deref(), Some("main"));
        assert_eq!(
            trace.report_for("local").map(|r| r.outcome.clone()),
            Some(DecisionBackendOutcome::Failed { error_kind: "unavailable".to_string() })
        );
    }

    #[tokio::test]
    async fn an_abstaining_backend_degrades_to_the_baseline() {
        let backend = Arc::new(FakeDecisionProvider::new("local", FakeDecisionBehaviour::Abstain));
        let trace = decide(&broker(vec![backend])).await;

        assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
        assert_eq!(
            trace.report_for("local").map(|r| r.outcome.clone()),
            Some(DecisionBackendOutcome::Abstained)
        );
    }

    #[tokio::test]
    async fn a_hanging_backend_times_out_and_degrades_to_the_baseline() {
        let backend = Arc::new(FakeDecisionProvider::new(
            "slow",
            FakeDecisionBehaviour::Hang(Duration::from_secs(30)),
        ));
        let broker = broker(vec![backend]).with_timeout(Duration::from_millis(20));
        let trace = decide(&broker).await;

        assert_eq!(
            trace.report_for("slow").map(|r| r.outcome.clone()),
            Some(DecisionBackendOutcome::Failed { error_kind: "timeout".to_string() })
        );
        assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
    }

    #[tokio::test]
    async fn an_answer_for_another_pack_is_a_protocol_failure_and_does_not_win() {
        let backend = Arc::new(FakeDecisionProvider::new(
            "stale",
            FakeDecisionBehaviour::AnswerForAnotherPack(vec![(TIER.to_string(), fast())]),
        ));
        let trace = decide(&broker(vec![backend])).await;

        assert_eq!(
            trace.report_for("stale").map(|r| r.outcome.clone()),
            Some(DecisionBackendOutcome::Failed { error_kind: "protocol".to_string() })
        );
        assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
    }

    #[tokio::test]
    async fn nobody_answering_is_recorded_as_no_choice_not_as_an_abort() {
        let broker =
            DecisionBroker::new(DecisionMode::Shadow, Arc::new(RuleDecisionProvider::empty()))
                .expect("shadow is available");
        let trace = decide(&broker).await;

        assert!(trace.chosen.is_none());
        assert!(trace.chosen_backend.is_none());
        assert!(trace.aborted.is_none(), "the backends were asked; nobody had an opinion");
        assert_eq!(trace.backends.len(), 1);
    }

    #[test]
    fn leaving_shadow_is_refused_when_the_broker_is_built() {
        for mode in [DecisionMode::Advisory, DecisionMode::Bounded] {
            let error = DecisionBroker::new(mode, router_baseline())
                .err()
                .expect("a mode this build cannot honor is refused");
            assert!(matches!(error, DecisionBrokerError::ModeNotAvailable(m) if m == mode));
        }
    }

    #[tokio::test]
    async fn advising_asks_the_route_family_and_keeps_the_routers_choice() {
        let decision_broker = broker(Vec::new());
        let trace = decision_broker.advise_model_route(&request(), &chosen()).await;

        assert_eq!(trace.family.as_str(), DecisionFamily::MODEL_ROUTE);
        assert_eq!(trace.pack_version, model_route::PACK_VERSION);
        assert_eq!(trace.question_ids, vec![TIER.to_string()]);
        assert_eq!(trace.mode, DecisionMode::Shadow);
        // The baseline answered from the router's own choice, so the comparison
        // is between the router and whoever else was asked — and the caller
        // still holds the route it made.
        assert_eq!(chosen_choice(&trace).as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn a_state_that_cannot_be_built_is_recorded_as_an_abort() {
        let decision_broker = broker(Vec::new());
        // The chosen model name is configuration, and configuration is what
        // goes into the state; a name large enough to blow the cap makes the
        // state unbuildable. No backend is asked, and the trace says why.
        let oversized = ModelRoute::new(ModelTier::Main, "openai-compat", "m".repeat(9000));

        let trace = decision_broker.advise_model_route(&request(), &oversized).await;

        assert_eq!(trace.aborted.as_deref(), Some("state_too_large"));
        assert!(trace.backends.is_empty());
        assert!(trace.chosen.is_none());
    }

    #[tokio::test]
    async fn the_state_a_backend_sees_carries_no_conversation() {
        let backend = Arc::new(FakeDecisionProvider::new("local", FakeDecisionBehaviour::Abstain));
        decide(&broker(vec![backend.clone()])).await;

        let calls = backend.calls();
        assert_eq!(calls.len(), 1);
        let (_, state) = &calls[0];
        assert_eq!(state.context_revision, 11);
        let keys: Vec<&str> = state.facts.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["chosen_tier", "context_revision", "message_count", "model", "task"]);
    }
}
