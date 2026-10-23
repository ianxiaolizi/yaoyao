//! The model-route decision family (the first candidate scenario).
//!
//! The question is narrow and the state is small, which is the point: a router
//! already picked a route deterministically, and this family asks a fast backend
//! whether it agrees. The router keeps the final say — in shadow mode the
//! answer is recorded and nothing else.

use agent_ports::decision::{
    DecisionFamily, DecisionOption, DecisionQuestion, DecisionState, DecisionStateError,
    QuestionPack,
};
use agent_ports::model_router::{ModelRoute, ModelTier, RouteRequest};

/// The question: which tier should serve this turn?
pub const TIER_QUESTION: &str = "tier";

/// The state fact carrying the router's own choice.
pub const CHOSEN_TIER_FACT: &str = "chosen_tier";

/// The pack revision. Bump it whenever a question, its kind, or its options
/// change — a trace's pack version is what makes two decisions comparable.
pub const PACK_VERSION: &str = "v1";

const PROMPT: &str = "Which model tier should serve this turn? Answer with one tier.";

/// Every tier the question offers.
///
/// Option ids come from [`ModelTier::as_str`], so a category chosen here is the
/// same string the route and the trace carry.
pub fn tiers() -> Vec<DecisionOption> {
    [ModelTier::Main, ModelTier::Fast, ModelTier::Thinking]
        .into_iter()
        .map(|tier| DecisionOption::new(tier.as_str(), tier.as_str()))
        .collect()
}

/// The pack for one turn's routing decision.
pub fn pack() -> QuestionPack {
    QuestionPack::new(
        DecisionFamily::new(DecisionFamily::MODEL_ROUTE),
        PACK_VERSION,
        vec![DecisionQuestion::categorical(TIER_QUESTION, PROMPT, tiers())],
    )
}

/// The bounded state for one turn's routing decision.
///
/// Everything here is either configuration (the chosen model) or derived from
/// durable state (the context revision, the message count). No message content
/// is copied in: a router's question is about the shape of the turn, and a
/// backend that needs to read the conversation is asking for something this
/// family deliberately does not hand out.
pub fn state(
    request: &RouteRequest,
    chosen: &ModelRoute,
) -> Result<DecisionState, DecisionStateError> {
    let mut facts = serde_json::Map::new();
    facts.insert(
        CHOSEN_TIER_FACT.to_string(),
        serde_json::Value::String(chosen.tier.as_str().to_string()),
    );
    facts.insert("task".to_string(), serde_json::Value::String(request.task.as_str().to_string()));
    facts.insert("context_revision".to_string(), serde_json::json!(request.context_revision));
    facts.insert("message_count".to_string(), serde_json::json!(request.message_count));
    facts.insert("model".to_string(), serde_json::Value::String(chosen.model.clone()));

    DecisionState::new(
        request.session_id.clone(),
        Some(request.turn_id.clone()),
        // The decision state predates the `EventSeq` unification and still
        // takes the raw number; the conversion is stated here rather than
        // hidden in the type.
        request.context_revision.as_u64(),
        facts,
    )
}
