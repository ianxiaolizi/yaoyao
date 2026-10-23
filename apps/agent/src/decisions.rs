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
use agent_ports::model_router::{ModelRoute, ModelRouter};

/// Why the shadow stack could not be assembled.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AssembleError {
    #[error("decision backend 'laya' cannot be built: {0}")]
    Laya(#[from] LayaConfigError),
}

/// The router a run executes with, and the advisor that observes its choices.
pub type DecisionStack = (Arc<dyn ModelRouter>, Arc<dyn DecisionAdvisor>);

/// Build the router and the shadow advisor for `route`.
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
    route: ModelRoute,
    laya: Option<ResolvedLaya>,
) -> Result<DecisionStack, AssembleError> {
    let mut backends: Vec<Arc<dyn FastDecisionProvider>> = Vec::new();

    if let Some(laya) = laya {
        let mut config = LayaConfig::new(laya.url);
        config.model = laya.model;
        config.threshold = laya.threshold;
        backends.push(Arc::new(LayaDecisionProvider::new(config)?));
    }

    Ok(agent_decision::shadow_for(route, backends))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::model_router::ModelTier;

    fn route() -> ModelRoute {
        ModelRoute::new(ModelTier::Main, "openai-compat", "m")
    }

    #[test]
    fn a_run_with_no_sidecar_configured_assembles_without_one() {
        let (_, advisor) = assemble(route(), None).expect("no sidecar is no problem");
        // Nothing to assert about the advisor's insides from here; what matters
        // is that the call is total and needs no configuration.
        drop(advisor);
    }

    #[test]
    fn a_sidecar_the_adapter_will_not_dial_stops_the_command() {
        // The refusal the config layer cannot make: it checks that a URL was
        // written, not what this build is willing to connect to.
        let error = assemble(
            route(),
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
        let (_, advisor) = assemble(
            route(),
            Some(ResolvedLaya {
                url: "http://127.0.0.1:8231".to_string(),
                model: Some("multilingual".to_string()),
                threshold: Some(0.6),
            }),
        )
        .expect("a loopback endpoint is what this build dials");
        drop(advisor);
    }
}
