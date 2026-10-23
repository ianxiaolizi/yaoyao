//! The one place the shadow stack is built.
//!
//! It lives here rather than in the binary so that the assembly an acceptance
//! test exercises is the same assembly the binary runs. A test that rebuilds
//! "the same thing" from the same parts is a test of the parts, not of the
//! wiring.
//!
//! Three properties are deliberate:
//!
//! - **The route comes from the caller.** This function does not decide which
//!   model a process talks to; it takes a route someone else chose and builds a
//!   router that always returns it. That keeps "which model" a single decision
//!   made at the app layer.
//! - **The only baseline is the rule.** The spec requires that a rule baseline
//!   always exists and is always asked, so it is not a backend choice a caller
//!   gets to make. Every other backend is: they arrive as a list, built by
//!   whoever knows which vendors this build has (a Laya sidecar is
//!   the first). This crate stays vendor-agnostic — it holds
//!   `Arc<dyn FastDecisionProvider>` and never asks which one it is holding.
//! - **The advisor is deadline-wrapped.** Whoever adds a backend that can block
//!   is the one who pays for it, here, rather than in the loop: see
//!   [`DeadlineAdvisor`](crate::deadline::DeadlineAdvisor).

use std::sync::Arc;

use agent_ports::decision::{DecisionAdvisor, DecisionMode, FastDecisionProvider};
use agent_ports::model_router::{ModelRoute, ModelRouter};

use crate::broker::DecisionBroker;
use crate::deadline::DeadlineAdvisor;
use crate::model_route;
use crate::router::TableModelRouter;
use crate::rule::{RuleDecision, RuleDecisionProvider};

/// Build the deterministic router and the shadow advisor for `route`.
///
/// The baseline answers "which tier?" with the tier the router already chose.
/// That is the comparison the trace exists to hold: a backend that disagrees is
/// visible next to the deterministic answer it disagreed with.
///
/// `backends` are asked before it, in order, and each one is bounded by the
/// broker's per-backend deadline. An empty list is the honest assembly for a run
/// with no sidecar configured: the trace then says the baseline was the only
/// participant, which is a different statement from a sidecar that abstained.
pub fn shadow_for(
    route: ModelRoute,
    backends: Vec<Arc<dyn FastDecisionProvider>>,
) -> (Arc<dyn ModelRouter>, Arc<dyn DecisionAdvisor>) {
    let router: Arc<dyn ModelRouter> = Arc::new(TableModelRouter::new(route));

    let baseline = Arc::new(RuleDecisionProvider::new(vec![RuleDecision::from_fact(
        agent_protocol::decision::DecisionFamily::MODEL_ROUTE,
        model_route::TIER_QUESTION,
        model_route::CHOSEN_TIER_FACT,
    )]));

    // Shadow is the one mode this build implements, so this cannot fail; the
    // gate exists so that a future mode has to be granted deliberately.
    let mut broker = DecisionBroker::new(DecisionMode::Shadow, baseline)
        .expect("shadow is the one mode this build implements");
    for backend in backends {
        broker = broker.with_backend(backend);
    }

    // The broker's deadline is per backend; this one bounds the call the loop
    // waits on. A shadow decision that could outlast its own backends would be
    // the one thing shadow must never be: a reason a turn is slow.
    let advisor: Arc<dyn DecisionAdvisor> =
        Arc::new(DeadlineAdvisor::new(Arc::new(broker), DecisionMode::Shadow));

    (router, advisor)
}
