//! Helpers for scheduler tests that need a profile snapshot.

use agent_protocol::profile::ResolvedProfileSnapshot;

/// A profile snapshot that carries only the budget a scheduler test is about.
pub(crate) fn snapshot_with_budget(
    budget: agent_protocol::profile::RunBudget,
) -> ResolvedProfileSnapshot {
    ResolvedProfileSnapshot {
        profile_id: "coding".to_string(),
        config_hash: String::new(),
        plugin_versions: std::collections::BTreeMap::new(),
        policy_version: "deadbeef".to_string(),
        decision_backends: Vec::new(),
        model_route: agent_protocol::model::ModelRoute::new(
            agent_protocol::model::ModelTier::Main,
            "echo",
            "echo",
        ),
        model_routes: std::collections::BTreeMap::new(),
        memory_enabled: false,
        capability_set: agent_protocol::permissions::CapabilitySet::new(),
        budget,
        model_capabilities: std::collections::BTreeMap::new(),
        budget_sources: std::collections::BTreeMap::new(),
        completion: None,
    }
    .sealed()
}
