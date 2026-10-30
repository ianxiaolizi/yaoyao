//! The resolved configuration a run executes under.
//!
//! A profile is a scenario composition — model, tools, policy, budget, plugins
//! — resolved across configuration layers and then frozen. The frozen form is
//! what a reader gets: the config files may have moved on since the run, so
//! "what was this history running under?" can only be answered by a value that
//! was written down at the time.
//!
//! It lives in the protocol because it is a durable payload, not because the
//! kernel resolves it: resolution is `agent-config`'s job, and this module is
//! the shape both sides agree on.

use std::collections::BTreeMap;

use crate::digest;
use crate::model::ModelRoute;
use crate::permissions::CapabilitySet;
use serde::{Deserialize, Serialize};

/// The step and spend limits a run starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBudget {
    /// How many steps a turn may take before it stops as `MaxSteps`.
    pub max_steps: u32,
    /// How many tool calls the whole run may make. `None` means unbounded.
    ///
    /// Enforced by the loop, which is what sees a call before dispatching it: a
    /// batch that would cross the limit stops at the call that crosses it, and
    /// the rest are recorded as `tool/not_started`. The count covers one run,
    /// which is one turn here; the run a resume opens is a new run with its own
    /// budget.
    pub max_tool_calls: Option<u32>,
    /// How long the run may take, in milliseconds. `None` means unbounded.
    ///
    /// Enforced by the loop at the step boundary and before each tool dispatch,
    /// on the monotonic clock the turn opened with. A request already in flight
    /// is not aborted, so the overshoot is bounded by one request.
    pub wall_clock_ms: Option<u64>,
    /// The most estimated input tokens one request may carry. `None` means the
    /// history is sent whole — the behaviour before this limit existed.
    ///
    /// Skipped when unset so that a `profile/resolved` written by a build that
    /// did not know the field, and one written by this build with no limit,
    /// produce the same JSON and therefore the same `config_hash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
    /// How much of `max_input_tokens` is reserved for the model's answer.
    /// Only meaningful with `max_input_tokens`; `None` is treated as zero.
    ///
    /// This is window accounting and nothing else: the trimmer leaves this much
    /// room so a request plus its answer fits the working set. It is no longer
    /// the provider's `max_tokens` — see [`Self::max_output_tokens`], which
    /// exists because one number for both jobs made a 2,000-token reservation
    /// into a hard completion cap that reasoning models exhausted before
    /// writing anything (CONTRACT §7 #131).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_output_tokens: Option<u32>,
    /// The most completion tokens one request may ask the provider for, sent
    /// as the request's `max_tokens`. `None` sends no cap and lets the
    /// provider's own default apply.
    ///
    /// Independent of the window: `reserved_output_tokens` is how much room the
    /// trimmer keeps for the answer, this is how long an answer may be. A
    /// reasoning model spends part of this budget on its reasoning, so a cap
    /// sized for the prose is a cap that can end the response mid-thought.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// When the estimated settled history reaches this share of the usable
    /// window, the loop asks the model for a handover summary before its
    /// normal request. `None` means compaction is off and the trimmer is the
    /// only protection. Only meaningful with `max_input_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_at_percent: Option<u32>,
}

impl RunBudget {
    /// How much of one tool result may stay inline before it is offloaded.
    ///
    /// Compaction keeps the newest group whole, so an output that alone
    /// exceeds the working set cannot be rescued by summarizing history: the
    /// only place to bound it is where it is appended. The limit is therefore
    /// a quarter of the usable window, converted at the estimator's usual ~4
    /// bytes per token, floored at 8 KiB so an ordinary file still arrives
    /// whole and capped at the historical 50 KiB. A run with no configured
    /// window keeps the historical limit.
    pub fn tool_output_limit_bytes(&self) -> usize {
        const FLOOR_BYTES: usize = 8 * 1024;
        const CEILING_BYTES: usize = 50 * 1024;
        const BYTES_PER_TOKEN: usize = 4;
        let Some(max_input_tokens) = self.max_input_tokens else {
            return CEILING_BYTES;
        };
        let usable =
            max_input_tokens.saturating_sub(self.reserved_output_tokens.unwrap_or(0)) as usize;
        (usable / 4 * BYTES_PER_TOKEN).clamp(FLOOR_BYTES, CEILING_BYTES)
    }
}

impl Default for RunBudget {
    /// What a run has when no configuration says otherwise.
    ///
    /// The step limit is the one the assembled binary has always used, so a
    /// run with no config files behaves exactly as it did before profiles
    /// existed. `max_steps` is enforced by the Scheduler, which owns when a
    /// turn advances; `max_tool_calls` and `wall_clock_ms` are enforced by the
    /// loop, which is what sees each call before dispatching it.
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_tool_calls: None,
            wall_clock_ms: None,
            max_input_tokens: None,
            reserved_output_tokens: None,
            max_output_tokens: None,
            compact_at_percent: None,
        }
    }
}

/// The configuration one run executes under, frozen.
///
/// Recorded once per run as `profile/resolved`, and copied into a fork's
/// header, so that the lineage of a session carries the configuration its
/// history was produced under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedProfileSnapshot {
    /// The profile that was composed, or `default` when none was selected.
    pub profile_id: String,
    /// Digest of every other field; see [`ResolvedProfileSnapshot::sealed`].
    pub config_hash: String,
    /// The plugins this run enabled, mapped to the version each was trusted at.
    ///
    /// Separate from `config_hash` because a config file names a plugin, not a
    /// version: upgrading the plugin behind an unchanged configuration moves
    /// this map and leaves the hash alone, which is exactly the distinction
    /// worth recording. `*` means the entry was trusted at any version.
    pub plugin_versions: BTreeMap<String, String>,
    /// Digest of the resolved policy — its rules, managed denies, and the
    /// capabilities the sandbox advertises.
    ///
    /// Separate from `config_hash` because part of the policy is code, not
    /// configuration: a built-in default that changes moves this digest with no
    /// config file touched.
    pub policy_version: String,
    /// The decision backends the run attached, as labels (`laya:<url>@<model>`).
    ///
    /// Here rather than only in the config hash because a shadow decision is
    /// read against "who could have answered": a trace whose `chosen_backend`
    /// is the rule baseline means something different when a sidecar was
    /// attached and abstained than when none was configured at all.
    pub decision_backends: Vec<String>,
    /// The model this run's requests are routed to.
    pub model_route: ModelRoute,
    /// The routes for tiers besides `main`, keyed by tier name.
    ///
    /// Absent unless the configuration named a tier besides `main`: a run with
    /// one model has nothing a table would add, and emitting an empty one would
    /// move `config_hash` for every configuration that never asked for routing.
    /// `main` is not repeated here — [`Self::model_route`] is already it, and is
    /// also what every unnamed task class falls back to. The map is ordered, so
    /// the same configuration serializes to the same bytes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_routes: BTreeMap<String, ModelRoute>,
    /// Whether this run offered the model the long-term memory tools.
    ///
    /// Absent when off, for the reason `model_routes` is: memory is off by
    /// default, and a run that never turned it on must compare equal to the runs
    /// that came before this field existed. When it is on, the tools were on the
    /// surface, and that is a difference between two runs worth recording.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub memory_enabled: bool,
    /// The capabilities this run is authorized to use.
    pub capability_set: CapabilitySet,
    pub budget: RunBudget,
}

impl ResolvedProfileSnapshot {
    /// Fill in `config_hash` from every other field.
    ///
    /// The hash is never a caller's to pass in: one that disagreed with the
    /// payload it sits on would let two runs compare equal when they are not,
    /// which is the only thing this field exists to prevent.
    pub fn sealed(mut self) -> Self {
        self.config_hash = String::new();
        // Every field is plain data, so this cannot fail. Silently hashing
        // empty bytes on failure would make every profile compare equal — the
        // exact outcome this field exists to prevent.
        let bytes = serde_json::to_vec(&self)
            .expect("a resolved profile snapshot is plain data and always serializes");
        self.config_hash = digest::fnv1a_hex(&bytes);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelTier;
    use crate::permissions::Capability;

    fn snapshot(profile_id: &str, model: &str) -> ResolvedProfileSnapshot {
        ResolvedProfileSnapshot {
            profile_id: profile_id.to_string(),
            config_hash: String::new(),
            plugin_versions: BTreeMap::new(),
            policy_version: "p".to_string(),
            decision_backends: Vec::new(),
            model_route: ModelRoute::new(ModelTier::Main, "echo", model),
            model_routes: BTreeMap::new(),
            memory_enabled: false,
            capability_set: [Capability::new(Capability::FS_READ)].into_iter().collect(),
            budget: RunBudget::default(),
        }
    }

    #[test]
    fn the_default_budget_is_the_step_limit_the_binary_already_used() {
        // A run with no config must not change shape because profiles now
        // exist, so the fallback is the number that was hard-coded before.
        assert_eq!(RunBudget::default().max_steps, 8);
        assert_eq!(RunBudget::default().max_tool_calls, None);
    }

    #[test]
    fn the_tool_output_limit_follows_the_working_set() {
        // A quarter of the usable window, at four bytes per token: 32k in,
        // 8k tokens of output, 32,000 bytes.
        let budget = RunBudget {
            max_input_tokens: Some(34_000),
            reserved_output_tokens: Some(2_000),
            ..RunBudget::default()
        };
        assert_eq!(budget.tool_output_limit_bytes(), 32_000);
        // Nothing configured keeps the limit the gateway has always used.
        assert_eq!(RunBudget::default().tool_output_limit_bytes(), 50 * 1024);
        // A window too small to quarter meaningfully still admits a file.
        let tiny = RunBudget { max_input_tokens: Some(3_200), ..RunBudget::default() };
        assert_eq!(tiny.tool_output_limit_bytes(), 8 * 1024);
        // A huge window does not raise the ceiling.
        let huge = RunBudget { max_input_tokens: Some(1_048_576), ..RunBudget::default() };
        assert_eq!(huge.tool_output_limit_bytes(), 50 * 1024);
    }

    #[test]
    fn a_sealed_snapshot_hashes_itself_and_nothing_else() {
        let sealed = snapshot("coding", "deepseek-chat").sealed();
        assert!(!sealed.config_hash.is_empty());
        assert_eq!(sealed.config_hash.len(), 16);

        // Sealing is a function of the payload, not of when it was sealed.
        let again = snapshot("coding", "deepseek-chat").sealed();
        assert_eq!(sealed, again);
        assert_eq!(sealed.config_hash, again.config_hash);

        // A different configuration is a different hash — including one that
        // differs only in a field a reader might not think to compare.
        assert_ne!(sealed.config_hash, snapshot("review", "deepseek-chat").sealed().config_hash);
        assert_ne!(sealed.config_hash, snapshot("coding", "other-model").sealed().config_hash);
    }

    #[test]
    fn turning_memory_on_moves_the_hash_and_off_leaves_the_shape_alone() {
        // Off is the default and must stay byte-identical to the snapshots
        // written before this field existed, or every stored run would compare
        // unequal to every new one for a reason nobody asked about.
        let off = snapshot("coding", "m").sealed();
        let json = serde_json::to_value(&off).unwrap();
        assert!(json.get("memory_enabled").is_none(), "off is absent, not false: {json}");

        let mut on = snapshot("coding", "m");
        on.memory_enabled = true;
        let on = on.sealed();
        assert!(serde_json::to_value(&on).unwrap().get("memory_enabled").is_some());
        assert_ne!(off.config_hash, on.config_hash, "the tools a run offered are part of it");
    }

    #[test]
    fn a_snapshot_round_trips_through_serde() {
        let sealed = snapshot("coding", "deepseek-chat").sealed();
        let json = serde_json::to_string(&sealed).unwrap();
        let decoded: ResolvedProfileSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, sealed);
        assert_eq!(decoded.model_route.model, "deepseek-chat");
        assert!(decoded.capability_set.contains(&Capability::new(Capability::FS_READ)));
    }
}
