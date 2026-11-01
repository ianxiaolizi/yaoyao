//! Decision assembly: who routes, and who is asked for a shadow opinion.
//!
//! The construction itself lives in `agent_decision::shadow_for`, so the
//! acceptance tests exercise the same assembly this binary runs. What belongs
//! here is the decision that function deliberately does not make: which
//! vendors this build has. It is the app layer that knows `agent-decision-laya`
//! exists, and `agent-decision` that keeps not knowing.

use std::sync::Arc;

use agent_config::ResolvedLaya;
use agent_decision_laya::{LayaConfig, LayaConfigError, LayaDecisionProvider};
use agent_ports::decision::{DecisionAdvisor, FastDecisionProvider};
use agent_ports::intent_adjudication::IntentAdjudicator;
use agent_ports::model_router::ModelRouter;

/// Why the shadow stack could not be assembled.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AssembleError {
    #[error("decision backend 'laya' cannot be built: {0}")]
    Laya(#[from] LayaConfigError),
}

/// The router a run executes with, the advisor that observes its choices, and
/// the adjudicator that resolves messages the rules could not.
///
/// The adjudicator is `None` when no sidecar was configured. Every other field
/// is always present: the loop is always given a router, and a configured run
/// is always given an advisor.
pub struct DecisionStack {
    pub router: Arc<dyn ModelRouter>,
    pub advisor: Arc<dyn DecisionAdvisor>,
    pub adjudicator: Option<Arc<dyn IntentAdjudicator>>,
}

/// Wrap `router` with the shadow advisor.
///
/// The sidecar is built here, once, at startup — not lazily on the first turn.
/// A configuration this build will not honor (an endpoint that is not loopback,
/// a threshold outside the unit interval) is therefore a refusal to start, with
/// the offending value in the message, rather than a backend that fails
/// invisibly on every turn of a session that has already begun.
///
/// What this does **not** do is check that the sidecar is up. Reachability is
/// not the adapter's to decide, and asking once at startup would answer the
/// wrong question: a sidecar that is down now and up at the first turn is a
/// backend that will be asked and will answer. A turn where it is unreachable
/// records `unavailable` next to the rule baseline's answer, which is exactly
/// what the trace is for — and the run continues.
pub fn assemble(
    router: Arc<dyn ModelRouter>,
    laya: Option<ResolvedLaya>,
) -> Result<DecisionStack, AssembleError> {
    let mut backends: Vec<Arc<dyn FastDecisionProvider>> = Vec::new();

    if let Some(laya) = laya {
        let mut config = LayaConfig::new(laya.url);
        config.model = laya.model;
        config.threshold = laya.threshold;
        backends.push(Arc::new(LayaDecisionProvider::new(config)?));
    }

    // The adjudicator is built from the same backend the shadow trace is, and
    // before `shadow_for` takes ownership of the list. It is the same sidecar,
    // not the same call: each adjudication is a separate request with its own
    // bounded input, and it never sees the executing session's history.
    let adjudicator =
        agent_decision::intent_adjudication::adjudicator_for(backends.first().cloned());
    let (router, advisor) = agent_decision::shadow_for(router, backends);
    Ok(DecisionStack { router, advisor, adjudicator })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_decision::TableModelRouter;
    use agent_ports::model_router::{ModelRoute, ModelTier};

    fn route() -> ModelRoute {
        ModelRoute::new(ModelTier::Main, "openai-compat", "m")
    }

    /// One route, for the tests that are not about routing.
    fn one_route() -> Arc<dyn ModelRouter> {
        Arc::new(TableModelRouter::new(route()))
    }

    #[test]
    fn a_run_with_no_sidecar_configured_assembles_without_one() {
        let stack = assemble(one_route(), None).expect("no sidecar is no problem");
        // Nothing to assert about the advisor's insides from here; what matters
        // is that the call is total and needs no configuration — and that no
        // sidecar means no adjudicator rather than a broken one.
        assert!(stack.adjudicator.is_none());
    }

    #[test]
    fn a_sidecar_the_adapter_will_not_dial_stops_the_command() {
        // The refusal the config layer cannot make: it checks that a URL was
        // written, not what this build is willing to connect to.
        let error = assemble(
            one_route(),
            Some(ResolvedLaya {
                url: "http://laya.internal:8000".to_string(),
                model: None,
                threshold: None,
            }),
        )
        .err()
        .expect("a remote endpoint is refused");

        assert!(matches!(error, AssembleError::Laya(_)), "{error}");
        assert!(error.to_string().contains("decision backend 'laya'"), "{error}");
    }

    #[test]
    fn a_configured_sidecar_assembles_with_its_parameters() {
        let stack = assemble(
            one_route(),
            Some(ResolvedLaya {
                url: "http://127.0.0.1:8231".to_string(),
                model: Some("multilingual".to_string()),
                threshold: Some(0.6),
            }),
        )
        .expect("a loopback endpoint is what this build dials");
        // The configured sidecar is what answers unclear messages; a stack
        // without one would leave every ambiguity to the person.
        assert!(stack.adjudicator.is_some());
    }
}
