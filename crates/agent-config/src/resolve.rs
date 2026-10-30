//! Resolving the layers into one configuration.
//!
//! The order is the design's:
//!
//! ```text
//! built-in defaults -> managed policy -> user -> workspace -> profile -> CLI
//! ```
//!
//! with two rules deciding what "later" means. A normal setting — a model
//! name, a budget — is whatever the closest layer that named it said. A
//! security setting — a capability deny — is the strictest thing any layer
//! said, so a project can refuse what a profile permits but never the reverse.
//!
//! Two questions get two answers here, and they are not the same question:
//!
//! - *May this capability be used at all in this run?* — the capability set,
//!   which is what a plugin's `requires` is checked against at registration.
//! - *What happens when a tool asks for it?* — the policy rules, which the
//!   gateway consults per call, with the arguments in hand.
//!
//! A configuration that answered only one of them would either register
//! plugins it can never use or authorize calls it never meant to make.

use std::collections::{BTreeMap, BTreeSet};

use agent_protocol::digest;
use agent_protocol::model::{ModelRoute, ModelTier};
use agent_protocol::permissions::{Capability, CapabilitySet, PolicyDecision};
use agent_protocol::profile::{ResolvedProfileSnapshot, RunBudget};

use crate::layers::{
    BudgetSpec, Disposition, LayaSpec, ManagedPolicy, McpServer, PluginsSpec, PolicySpec, Profile,
    SubagentSpec, ToolsSpec, UserConfig, WorkspaceConfig, parse_duration_ms,
};
use crate::{ConfigError, Result};

/// The layers as the app read them, plus whatever the command line said.
#[derive(Debug, Clone, Default)]
pub struct Layers {
    pub managed: ManagedPolicy,
    pub user: UserConfig,
    pub workspace: WorkspaceConfig,
    /// `--profile`: selects a profile without editing a file.
    pub profile_override: Option<String>,
    /// `--model`: names the model for this run.
    pub model_override: Option<String>,
}

/// A configuration resolved to the point where only the model route is open.
///
/// The route is the app's to fill: it depends on which provider the process
/// actually selected, which depends on the model name this resolution produced.
/// Keeping that one step outside is what stops this crate from knowing anything
/// about providers.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProfile {
    pub profile_id: String,
    /// The model the configuration names, if any.
    pub model: Option<String>,
    /// Every tier the configuration named a model for, `main` included.
    ///
    /// `model` above is what the *run's* provider is switched to; this map is
    /// what a task class asks for. They overlap on `main` by construction, and
    /// they are kept separate because only one of them is a single value:
    /// `--model` overrides `main` and nothing else, so a layer that names
    /// `fast` keeps it.
    pub model_tiers: BTreeMap<ModelTier, String>,
    /// Whether this run offers the model the long-term memory tools.
    ///
    /// Read from the user layer's `[memory] enabled` and nowhere else. The
    /// tools' *calls* are still gated by `memory.read` / `memory.write` — this
    /// only decides whether they are on the surface at all — so a run with the
    /// store off has no way to reach it even by naming a tool.
    pub memory_enabled: bool,
    pub capability_set: CapabilitySet,
    pub plugin_versions: BTreeMap<String, String>,
    /// The MCP servers this run starts, one per enabled `mcp.<server>` plugin,
    /// in name order.
    ///
    /// Defined-but-not-enabled servers are not here: a definition says how to
    /// reach a server, and a profile's `plugins.enable` says whether this run
    /// reaches it. Both have to be true for an entry to appear, which is what
    /// makes turning a server off a matter of editing the profile rather than
    /// deleting the definition.
    pub mcp_servers: Vec<McpServerConfig>,
    /// The subagent providers this run may delegate through, one per enabled
    /// `subagent.<provider>` plugin, in name order.
    ///
    /// The same two facts decide an entry here as decide a server there: a layer
    /// defines how to reach the provider, and the selected profile says whether
    /// this run reaches it. A definition nobody enabled is not a provider this
    /// run has.
    pub subagents: Vec<SubagentConfig>,
    /// The fast-decision sidecar this run attaches, when one is defined and the
    /// selected profile has not turned it off.
    pub laya: Option<ResolvedLaya>,
    /// The decision backends this run will attach, as compact labels for the
    /// snapshot. Derived from [`ResolvedProfile::laya`], so a reader comparing
    /// two runs sees which advisors each one had without reading the config
    /// files back.
    pub decision_backends: Vec<String>,
    pub budget: RunBudget,
    pub policy: ResolvedPolicy,
    /// Digest of the resolved policy; see `ResolvedProfileSnapshot::policy_version`.
    pub policy_version: String,
    /// Narrowing applied by a higher authority than the layer that asked for
    /// something. Reported rather than swallowed: a grant that managed policy
    /// removed is a user's intent being overruled, and the person who wrote it
    /// is the one who can do something about it.
    pub notes: Vec<String>,
}

impl ResolvedProfile {
    /// Freeze it, now that the app knows which model it selected.
    ///
    /// `model_routes` is the table the app built from [`Self::model_tiers`],
    /// complete with provider ids. It arrives here rather than being derived
    /// here because which provider serves a tier is the app's to know — this
    /// crate never learns a vendor's name.
    pub fn into_snapshot(
        self,
        model_route: ModelRoute,
        model_routes: BTreeMap<String, ModelRoute>,
    ) -> ResolvedProfileSnapshot {
        ResolvedProfileSnapshot {
            profile_id: self.profile_id,
            config_hash: String::new(),
            plugin_versions: self.plugin_versions,
            decision_backends: self.decision_backends,
            policy_version: self.policy_version,
            model_route,
            model_routes,
            memory_enabled: self.memory_enabled,
            capability_set: self.capability_set,
            budget: self.budget,
        }
        .sealed()
    }
}

/// The policy a configuration resolved to, in the protocol's vocabulary.
///
/// Not `PolicyRule` — that is the kernel's, and this crate sits below it. The
/// app turns these into rules; what a rule *is* stays the kernel's business.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPolicy {
    /// Capabilities no layer, and no approval, can enable.
    pub managed_deny: Vec<Capability>,
    pub rules: Vec<Directive>,
    /// Dispositions named by a profile's `[policy]` section.
    ///
    /// Kept apart from `rules` because they mean something stronger. Each names
    /// a capability the built-in rules also rule on, and on a specificity tie
    /// the strictest rule wins. Appending `allow` beside the built-in `ask` for
    /// `process.spawn` would leave the knob reading `allow` while every run
    /// kept asking. An override replaces the built-in disposition for its
    /// capability instead of competing with it.
    pub overrides: Vec<Directive>,
    /// Whether `rules` are the whole set, replacing the built-in defaults.
    ///
    /// True when a profile wrote a `[tools]` section that **lists what it
    /// permits**. A profile that lists capabilities gets exactly those, because
    /// an unlisted capability then falls to the engine's fail-closed deny — which
    /// is what makes `allow` a whitelist rather than a longer allow list.
    ///
    /// A `[tools]` section that writes only `deny` is not that: it is a
    /// narrowing of the defaults, and `authorized` already reads it the same
    /// way. Before YA-10 the two halves disagreed, so the narrowest-looking
    /// section produced the widest capability set and the fewest rules.
    ///
    /// A `[policy]` section is never that either (YA-10's sibling, 2026-10-11).
    /// Its three knobs are per-capability dispositions and cannot name a whole
    /// set — they say nothing about `fs.read` or `git.status`, which the defaults
    /// do rule on — so reading the section as a replacement only dropped rules
    /// the user never meant to drop. `shell = "ask"` is what the default already
    /// says about `process.spawn`, yet under the old reading it also removed the
    /// default's allow for `fs.read`.
    pub replaces_defaults: bool,
}

/// One MCP server a run will start.
///
/// The transport is a value, not a section of TOML: by the time a server is
/// here, which of the two shapes it is has been decided, and the fields that
/// belonged to the other one are gone rather than present-and-ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    /// The server name — the half of the plugin id after `mcp.`, and the middle
    /// of every tool name it contributes.
    pub name: String,
    pub transport: McpTransport,
    /// The capabilities every tool this server offers is stamped with.
    pub requires: Vec<Capability>,
    /// How long one call may take, as a duration in milliseconds.
    pub timeout_ms: Option<u64>,
    /// What to do with a tool whose declaration this build cannot enforce.
    pub schema: SchemaPolicy,
}

/// What a run does with a tool whose `input_schema` it cannot enforce.
///
/// A declaration belongs to whoever wrote the tool, so which answer is right is
/// the operator's to give rather than this build's to guess. Refusing the
/// server is the default because a surface is a promise: a tool on it is a tool
/// whose arguments can be checked. `warn` is for a person who would rather keep
/// the other tools behind a server that has one bad declaration.
///
/// Neither value means "run it anyway". A tool this build cannot check is
/// refused when it is called, whichever policy is set — the difference is
/// whether the run says so at assembly, with the rest of the server still
/// registered, or leaves it to the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SchemaPolicy {
    /// An unenforceable declaration keeps the whole server off the surface.
    #[default]
    Strict,
    /// The server is registered and the tool is named in a note.
    Warn,
}

/// One subagent provider a run may delegate through.
///
/// A value rather than a section of TOML, and — unlike [`McpServerConfig`] —
/// without a transport to go with it, because a child is reached one way.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentConfig {
    /// The provider name — the half of the plugin id after `subagent.`, the
    /// name a delegation records as its `provider`, and the name a refusal
    /// about this provider uses.
    pub provider: String,
    /// The program to run.
    pub command: String,
    pub args: Vec<String>,
    /// In name order, because a process environment is a set of pairs and two
    /// equal configurations must produce equal values.
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    /// The capabilities a delegation through this provider requires.
    pub requires: Vec<Capability>,
    /// The longest a child may live, as a duration in milliseconds.
    pub ceiling_ms: Option<u64>,
}

/// The decision sidecar this run attaches.
///
/// A value rather than a section of TOML: by the time it is here, the layers
/// that defined it and the profile that selected it have both had their say,
/// and what is left is the endpoint to dial.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLaya {
    pub url: String,
    pub model: Option<String>,
    pub threshold: Option<f64>,
}

impl ResolvedLaya {
    /// The label a snapshot records for this backend.
    fn label(&self) -> String {
        match &self.model {
            Some(model) => format!("laya:{}@{model}", self.url),
            None => format!("laya:{}", self.url),
        }
    }
}

/// How the run reaches a server.
#[derive(Debug, Clone, PartialEq)]
pub enum McpTransport {
    /// A program the agent starts and speaks to over its stdio.
    Stdio {
        command: String,
        args: Vec<String>,
        /// In name order, because a process environment is a set of pairs and
        /// two equal configurations must produce equal values.
        env: Vec<(String, String)>,
        cwd: Option<String>,
    },
    /// A Streamable HTTP endpoint.
    Http { url: String, headers: Vec<(String, String)> },
}

/// One rule, as configuration expressed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    pub decision: PolicyDecision,
    pub capability: Capability,
}

/// Resolve the layers into one configuration.
pub fn resolve(layers: Layers) -> Result<ResolvedProfile> {
    let selected = layers.profile_override.clone().or_else(|| layers.workspace.profile.clone());
    let profile_id = selected.clone().unwrap_or_else(|| "default".to_string());

    let composed = match &selected {
        Some(name) => {
            if !layers.user.defines(name) {
                return Err(ConfigError::UnknownProfile(name.clone()));
            }
            compose(&layers.user.profiles, name)?
        }
        // No profile selected is a configuration, not a gap: it is the
        // built-in defaults, which is what every command did before profiles
        // existed.
        None => Profile::default(),
    };

    let mut notes = Vec::new();
    let granted = authorized(&layers, &composed, &profile_id)?;
    let managed_deny = managed_deny(&layers.managed);
    let capability_set = without_managed_deny(granted, &managed_deny, &mut notes);

    let policy = with_rules(
        ResolvedPolicy {
            managed_deny,
            rules: Vec::new(),
            overrides: Vec::new(),
            replaces_defaults: false,
        },
        &composed,
        &layers.workspace,
    );
    // Computed here because it is a fact about the policy, not about the route
    // the app is about to fill in — and the snapshot records it before the
    // snapshot can be sealed.
    let policy_version = policy_fingerprint(&policy);

    // Which plugins are on, then — from that answer and the definitions the
    // layers wrote — which servers this run reaches. The second question is
    // asked of the first one's answer rather than of the definitions alone,
    // because a server nobody enabled is a definition, not a connection.
    let plugin_versions =
        enabled_plugins(&layers.user, composed.plugins.as_ref(), &selected_id(&layers))?;
    let mcp_servers = mcp_servers(&layers, &plugin_versions, &capability_set, &mut notes)?;
    let subagents = subagent_providers(&layers, &plugin_versions, &capability_set, &mut notes)?;
    // Asked once, of both answers, because it is one question about the plugin
    // map rather than a remark either family would make about the other.
    note_unstarted_plugins(&plugin_versions, &mut notes);
    let laya = laya_backend(&layers, &composed, &profile_id, &mut notes)?;
    let decision_backends = laya.iter().map(ResolvedLaya::label).collect();

    Ok(ResolvedProfile {
        profile_id,
        model: layers
            .model_override
            .clone()
            .or_else(|| composed.model.as_ref().and_then(|model| model.main.clone())),
        model_tiers: model_tiers(&layers, composed.model.as_ref()),
        memory_enabled: layers.user.memory.as_ref().is_some_and(|memory| memory.enabled),
        capability_set,
        plugin_versions,
        mcp_servers,
        subagents,
        laya,
        decision_backends,
        budget: budget(&layers.workspace, &composed)?,
        policy,
        policy_version,
        notes,
    })
}

/// The name a refusal should use: the profile the user asked for, whichever
/// layer asked for it.
fn selected_id(layers: &Layers) -> String {
    layers
        .profile_override
        .clone()
        .or_else(|| layers.workspace.profile.clone())
        .unwrap_or_else(|| "default".to_string())
}

/// The models the layers named for each tier.
///
/// `--model` is a statement about *this run*, not about a tier: it replaces
/// `main` and leaves `fast`/`thinking` exactly as the layers wrote them. A tier
/// that names no model is absent rather than defaulted to `main` here — the
/// table says only what was asked for, and the router's own default supplies
/// the rest, so "not configured" and "configured to the same model as main"
/// stay distinguishable in `profile/resolved`.
fn model_tiers(
    layers: &Layers,
    spec: Option<&crate::layers::ModelSpec>,
) -> BTreeMap<ModelTier, String> {
    let mut tiers = BTreeMap::new();
    if let Some(spec) = spec {
        if let Some(model) = &spec.fast {
            tiers.insert(ModelTier::Fast, model.clone());
        }
        if let Some(model) = &spec.thinking {
            tiers.insert(ModelTier::Thinking, model.clone());
        }
    }
    let main = layers.model_override.clone().or_else(|| spec.and_then(|spec| spec.main.clone()));
    if let Some(model) = main {
        tiers.insert(ModelTier::Main, model);
    }
    tiers
}

/// Compose a profile and everything it extends, parent first.
///
/// A `deny` a parent set survives into the child: a composition may add
/// refusals, and the one thing it must not do is quietly drop one. That falls
/// out of merging field by field rather than replacing whole sections.
fn compose(profiles: &BTreeMap<String, Profile>, name: &str) -> Result<Profile> {
    let mut lineage: Vec<String> = Vec::new();
    let mut next = Some(name.to_string());
    while let Some(step) = next {
        if lineage.iter().any(|seen| seen == &step) {
            lineage.push(step);
            return Err(ConfigError::CircularExtends { lineage: lineage.join(" -> ") });
        }
        let profile =
            profiles.get(&step).ok_or_else(|| ConfigError::UnknownProfile(step.clone()))?;
        next = profile.extends.clone();
        lineage.push(step);
    }

    let mut merged = Profile::default();
    for step in lineage.iter().rev() {
        let profile = profiles.get(step).expect("the lineage was just walked");
        merged = merge(merged, profile.clone());
    }
    Ok(merged)
}

/// `above` overrides `base`, one field at a time.
fn merge(base: Profile, above: Profile) -> Profile {
    Profile {
        // The chain has been walked; `extends` is not a property of the result.
        extends: None,
        model: merge_with(base.model, above.model, |base, above| crate::layers::ModelSpec {
            main: above.main.or(base.main),
            fast: above.fast.or(base.fast),
            thinking: above.thinking.or(base.thinking),
        }),
        tools: merge_with(base.tools, above.tools, |base, above| ToolsSpec {
            allow: pick(base.allow, above.allow),
            deny: union(base.deny, above.deny),
        }),
        policy: merge_with(base.policy, above.policy, |base, above| PolicySpec {
            shell: above.shell.or(base.shell),
            network: above.network.or(base.network),
            workspace_write: above.workspace_write.or(base.workspace_write),
        }),
        budget: merge_with(base.budget, above.budget, |base, above| BudgetSpec {
            max_steps: above.max_steps.or(base.max_steps),
            max_tool_calls: above.max_tool_calls.or(base.max_tool_calls),
            wall_clock_timeout: above.wall_clock_timeout.or(base.wall_clock_timeout),
            max_input_tokens: above.max_input_tokens.or(base.max_input_tokens),
            reserved_output_tokens: above.reserved_output_tokens.or(base.reserved_output_tokens),
            compact_at_percent: above.compact_at_percent.or(base.compact_at_percent),
        }),
        plugins: merge_with(base.plugins, above.plugins, |base, above| PluginsSpec {
            enable: union(base.enable, above.enable),
            disable: union(base.disable, above.disable),
        }),
        // A switch, not a list: the child's setting wins, and a child that says
        // nothing leaves the parent's. Merging an "on" with an "off" has no
        // union — one of them has to be the answer, and the closer layer is it.
        decisions: merge_with(base.decisions, above.decisions, |base, above| {
            crate::layers::ProfileDecisions { laya: above.laya.or(base.laya) }
        }),
    }
}

fn merge_with<T>(base: Option<T>, above: Option<T>, merge: impl FnOnce(T, T) -> T) -> Option<T> {
    match (base, above) {
        (Some(base), Some(above)) => Some(merge(base, above)),
        (None, Some(above)) => Some(above),
        (base, None) => base,
    }
}

/// A child's list replaces its parent's; an absent one leaves the parent's.
fn pick(base: Vec<String>, above: Vec<String>) -> Vec<String> {
    if above.is_empty() { base } else { above }
}

/// A child's list extends its parent's, without duplicates and in a stable
/// order — the same list twice is the same declaration.
fn union(base: Vec<String>, above: Vec<String>) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    base.into_iter().chain(above).filter(|item| seen.insert(item.clone())).collect()
}

/// The capabilities this run is authorized to use.
///
/// The user layer is the ceiling; a profile selects inside it or says nothing.
/// A profile that names something the user never authorized is a refusal
/// rather than a narrowing, because the alternative — silently dropping it —
/// leaves a config file that says one thing and a run that does another.
fn authorized(layers: &Layers, composed: &Profile, profile_id: &str) -> Result<CapabilitySet> {
    let allowed = composed.tools.as_ref().map(|tools| tools.allow.clone()).unwrap_or_default();
    if allowed.is_empty() {
        return Ok(layers.user.capabilities.iter().map(Capability::new).collect());
    }

    let unlisted: Vec<String> =
        allowed.iter().filter(|name| !layers.user.capabilities.contains(name)).cloned().collect();
    if !unlisted.is_empty() {
        return Err(ConfigError::UnauthorizedCapability {
            profile: profile_id.to_string(),
            capabilities: unlisted,
        });
    }
    Ok(allowed.into_iter().map(Capability::new).collect())
}

fn managed_deny(managed: &ManagedPolicy) -> Vec<Capability> {
    let mut denied: Vec<Capability> = managed.deny.iter().map(Capability::new).collect();
    denied.sort();
    denied.dedup();
    denied
}

/// Subtract the absolute refusals, saying so when one of them lands on a grant.
fn without_managed_deny(
    authorized: CapabilitySet,
    managed_deny: &[Capability],
    notes: &mut Vec<String>,
) -> CapabilitySet {
    let mut effective = CapabilitySet::new();
    for capability in authorized.iter() {
        if managed_deny.contains(capability) {
            notes.push(format!(
                "capability '{capability}' is denied by managed policy and was not granted"
            ));
            continue;
        }
        effective.insert(capability.clone());
    }
    effective
}

/// Attach the rules, profile first and the project's refusals last.
///
/// The project's denies go on after the profile's rules on purpose: a deny is a
/// security setting, and the strictest value across every layer is
/// the one in effect. Applied last, a project's refusal survives whatever the
/// profile it selected permits.
fn with_rules(
    mut policy: ResolvedPolicy,
    composed: &Profile,
    workspace: &WorkspaceConfig,
) -> ResolvedPolicy {
    if let Some(tools) = &composed.tools {
        // Only an `allow` list makes the rules the whole set. A section that
        // writes nothing but `deny` is narrowing, and `authorized` reads it the
        // same way — an empty `allow` leaves the user's authorization intact.
        // Reading it as a whitelist here would turn "deny one thing" into "allow
        // only that one thing", silently, in the one place the user was trying
        // to be careful (CONTRACT.md §7 row 3, YA-10).
        if !tools.allow.is_empty() {
            policy.replaces_defaults = true;
        }
        for name in &tools.allow {
            policy.rules.push(Directive {
                decision: PolicyDecision::Allow,
                capability: Capability::new(name),
            });
        }
        for name in &tools.deny {
            policy.rules.push(Directive {
                decision: PolicyDecision::Deny,
                capability: Capability::new(name),
            });
        }
    }

    if let Some(spec) = &composed.policy {
        // Not a replacement of the whole set (CONTRACT.md §7 裁决 #14): the
        // section cannot name `fs.read` or `git.status`, so reading it as one
        // dropped rules the user never meant to drop. It *does* replace the
        // built-in disposition for the one capability each knob names — an
        // appended rule would tie with the default and lose to the stricter of
        // the two, which turned `shell = "allow"` into a knob that quietly kept
        // asking (2026-10-19, found by running the agent without approvals).
        if let Some(disposition) = spec.shell {
            policy.overrides.push(Directive {
                decision: disposition.into(),
                capability: Capability::new(Capability::PROCESS_SPAWN),
            });
        }
        if let Some(disposition) = spec.network {
            policy.overrides.push(Directive {
                decision: disposition.into(),
                capability: Capability::new(Capability::NETWORK_REQUEST),
            });
        }
        if let Some(writable) = spec.workspace_write {
            policy.overrides.push(Directive {
                decision: if writable { PolicyDecision::Allow } else { PolicyDecision::Deny },
                capability: Capability::new(Capability::FS_WRITE),
            });
        }
    }

    if let Some(tools) = &workspace.tools {
        for name in &tools.deny {
            policy.rules.push(Directive {
                decision: PolicyDecision::Deny,
                capability: Capability::new(name),
            });
        }
    }

    policy
}

/// The working set a run gets when no layer names one.
///
/// A run that configured no budget used to send its history whole. That is
/// not a safe product default: the history grows with every turn, every step
/// resends it, and nothing bounds a single tool result. The values are the
/// regular profile from CONTRACT #51, checked by the four-tier comparison in
/// `项目工作区/真实使用_2026-10-19/four-tier-2026-10-20/` and recorded in
/// CONTRACT #52. A layer that names any window field opts out entirely, so a
/// small explicit window keeps its own reservation and trigger — including
/// the refusal of a trigger with no window.
const DEFAULT_MAX_INPUT_TOKENS: u32 = 8_000;
const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 2_000;
const DEFAULT_COMPACT_AT_PERCENT: u32 = 75;

/// Whether a layer said anything about the window.
///
/// Step and tool-call limits do not count: a profile that only tightens how
/// many steps a turn may take still gets the shipped working set.
fn names_a_window(spec: Option<&BudgetSpec>) -> bool {
    spec.is_some_and(|spec| {
        spec.max_input_tokens.is_some()
            || spec.reserved_output_tokens.is_some()
            || spec.compact_at_percent.is_some()
    })
}

/// The run's limits: the project's layer first, then the profile's on top.
fn budget(workspace: &WorkspaceConfig, composed: &Profile) -> Result<RunBudget> {
    let mut budget = RunBudget::default();
    apply_budget(&mut budget, workspace.budget.as_ref())?;
    apply_budget(&mut budget, composed.budget.as_ref())?;
    if !names_a_window(workspace.budget.as_ref()) && !names_a_window(composed.budget.as_ref()) {
        budget.max_input_tokens = Some(DEFAULT_MAX_INPUT_TOKENS);
        budget.reserved_output_tokens = Some(DEFAULT_RESERVED_OUTPUT_TOKENS);
        budget.compact_at_percent = Some(DEFAULT_COMPACT_AT_PERCENT);
    }
    // The pair is checked once both layers have had their say: one layer may
    // name the window and another the reservation, and each is unremarkable
    // until it meets the other.
    if let (Some(max), Some(reserved)) = (budget.max_input_tokens, budget.reserved_output_tokens)
        && reserved >= max
    {
        return Err(ConfigError::InvalidBudget {
            layer: "budget".to_string(),
            detail: format!(
                "`reserved_output_tokens` ({reserved}) is not smaller than `max_input_tokens` \
                 ({max}); the whole window would be reserved for the answer"
            ),
        });
    }
    // A trigger with no window would fire on zero tokens: there is nothing to
    // measure the share against, and a silently ignored setting is worse than
    // a refusal that says why.
    if budget.compact_at_percent.is_some() && budget.max_input_tokens.is_none() {
        return Err(ConfigError::InvalidBudget {
            layer: "budget".to_string(),
            detail: "`compact_at_percent` needs `max_input_tokens`: without a window there is \
                     no budget to take a share of"
                .to_string(),
        });
    }
    Ok(budget)
}

/// A limit of zero is refused wherever it is written.
///
/// "No limit" is the field being absent, not zero: a zero step or tool-call
/// ceiling stops the run before it starts, and a zero wall clock does the same
/// one instant later. Accepting it would make a typo indistinguishable from an
/// intentionally tight budget — the same rule the MCP and subagent timeouts
/// follow (CFG-4).
fn apply_budget(budget: &mut RunBudget, spec: Option<&BudgetSpec>) -> Result<()> {
    let Some(spec) = spec else {
        return Ok(());
    };
    if let Some(steps) = spec.max_steps {
        if steps == 0 {
            return Err(ConfigError::InvalidBudget {
                layer: "budget".to_string(),
                detail: "`max_steps` is zero, which stops every turn before its first step"
                    .to_string(),
            });
        }
        budget.max_steps = steps;
    }
    if let Some(calls) = spec.max_tool_calls {
        if calls == 0 {
            return Err(ConfigError::InvalidBudget {
                layer: "budget".to_string(),
                detail: "`max_tool_calls` is zero, which refuses every tool call; omit it to \
                         leave tool calls unbounded, or deny the capability instead"
                    .to_string(),
            });
        }
        budget.max_tool_calls = Some(calls);
    }
    if let Some(timeout) = &spec.wall_clock_timeout {
        let ms = parse_duration_ms("budget", timeout)?;
        if ms == 0 {
            return Err(ConfigError::InvalidBudget {
                layer: "budget".to_string(),
                detail: format!(
                    "`wall_clock_timeout` is zero ('{timeout}'), which ends the run before it \
                     starts; omit it to leave the run unbounded"
                ),
            });
        }
        budget.wall_clock_ms = Some(ms);
    }
    if let Some(max) = spec.max_input_tokens {
        if max == 0 {
            return Err(ConfigError::InvalidBudget {
                layer: "budget".to_string(),
                detail: "`max_input_tokens` is zero, which leaves no room for even the turn's \
                         own input; omit it to send the history untrimmed"
                    .to_string(),
            });
        }
        budget.max_input_tokens = Some(max);
    }
    if let Some(reserved) = spec.reserved_output_tokens {
        if reserved == 0 {
            return Err(ConfigError::InvalidBudget {
                layer: "budget".to_string(),
                detail: "`reserved_output_tokens` is zero; omit it to reserve nothing, rather \
                         than writing a reservation that reserves nothing"
                    .to_string(),
            });
        }
        budget.reserved_output_tokens = Some(reserved);
    }
    if let Some(percent) = spec.compact_at_percent {
        if !(1..=99).contains(&percent) {
            return Err(ConfigError::InvalidBudget {
                layer: "budget".to_string(),
                detail: format!(
                    "`compact_at_percent` is {percent}, which is not a share of the window; \
                     use 1..=99, or omit it to leave compaction off"
                ),
            });
        }
        budget.compact_at_percent = Some(percent);
    }
    Ok(())
}

/// The plugins this run turns on, with the version each was trusted at.
fn enabled_plugins(
    user: &UserConfig,
    spec: Option<&PluginsSpec>,
    profile_id: &str,
) -> Result<BTreeMap<String, String>> {
    let trusted = user.trusted_plugins();
    let mut enabled = BTreeMap::new();
    let Some(spec) = spec else {
        return Ok(enabled);
    };

    for plugin in &spec.enable {
        let Some((_, version)) = trusted.iter().find(|(id, _)| id == plugin) else {
            return Err(ConfigError::UntrustedPlugin {
                profile: profile_id.to_string(),
                plugin: plugin.clone(),
            });
        };
        if spec.disable.contains(plugin) {
            return Err(ConfigError::PluginEnabledAndDisabled {
                profile: profile_id.to_string(),
                plugin: plugin.clone(),
            });
        }
        enabled.insert(plugin.clone(), version.clone());
    }
    Ok(enabled)
}

/// The prefix an MCP server's plugin id carries.
const MCP_PREFIX: &str = "mcp.";

/// The prefix a subagent provider's plugin id carries.
const SUBAGENT_PREFIX: &str = "subagent.";

/// The MCP servers this run starts.
///
/// One entry per enabled `mcp.<server>` plugin, in the plugin map's order — a
/// server nobody enabled is a definition this run does not connect to, and a
/// server that is enabled but undefined is a refusal rather than a server that
/// silently fails to appear.
///
/// An enabled plugin that is neither family is passed over here and reported by
/// [`note_unstarted_plugins`], once, for both families.
fn mcp_servers(
    layers: &Layers,
    enabled: &BTreeMap<String, String>,
    capability_set: &CapabilitySet,
    notes: &mut Vec<String>,
) -> Result<Vec<McpServerConfig>> {
    let defined = defined_servers(layers, notes);
    let mut servers = Vec::new();

    for plugin in enabled.keys() {
        let Some(name) = plugin.strip_prefix(MCP_PREFIX) else {
            continue;
        };
        let Some((layer, server)) = defined.get(name) else {
            return Err(ConfigError::UndefinedMcpServer {
                plugin: plugin.clone(),
                server: name.to_string(),
            });
        };
        servers.push(server_config(layer, name, server, capability_set)?);
    }

    Ok(servers)
}

/// The subagent providers this run may delegate through.
///
/// The same three answers as the servers, for the same reasons: an enabled
/// `subagent.<provider>` is one this run has if a layer defined it, a refusal if
/// none did, and an enabled plugin of any other name is not this family's to do
/// anything about.
fn subagent_providers(
    layers: &Layers,
    enabled: &BTreeMap<String, String>,
    capability_set: &CapabilitySet,
    notes: &mut Vec<String>,
) -> Result<Vec<SubagentConfig>> {
    let defined = defined_subagents(layers, notes);
    let mut providers = Vec::new();

    for plugin in enabled.keys() {
        let Some(name) = plugin.strip_prefix(SUBAGENT_PREFIX) else {
            continue;
        };
        let Some((layer, spec)) = defined.get(name) else {
            return Err(ConfigError::UndefinedSubagent {
                plugin: plugin.clone(),
                provider: name.to_string(),
            });
        };
        providers.push(subagent_config(layer, name, spec, capability_set)?);
    }

    Ok(providers)
}

/// Say, once each, which enabled plugins this build cannot start.
///
/// Both families have to be known here for the answer to be right: `vendor.pack`
/// is not startable, `mcp.files` is, and a check written inside one family would
/// have to call the other one unstartable to be able to say so. It is a note
/// rather than a refusal because the plugin *is* trusted and *is* enabled — this
/// build simply has no way to load it, and a run that says so is a run that did
/// what it promised minus one thing it named.
fn note_unstarted_plugins(enabled: &BTreeMap<String, String>, notes: &mut Vec<String>) {
    for plugin in enabled.keys() {
        if plugin.starts_with(MCP_PREFIX) || plugin.starts_with(SUBAGENT_PREFIX) {
            continue;
        }
        notes.push(format!(
            "plugin '{plugin}' is enabled, but this build starts only MCP servers \
             (`{MCP_PREFIX}<server>`) and subagent providers (`{SUBAGENT_PREFIX}<provider>`); \
             it was not loaded"
        ));
    }
}

/// Every server any layer defines, by name, with the layer that defined it.
///
/// The environment first, then the operator's: a managed definition replaces a
/// user's of the same name, and says so, because the one thing a user
/// configuration must not be able to do is point a server the operator pinned
/// at a program of its own.
fn defined_servers<'a>(
    layers: &'a Layers,
    notes: &mut Vec<String>,
) -> BTreeMap<&'a str, (&'static str, &'a McpServer)> {
    let mut defined: BTreeMap<&str, (&'static str, &McpServer)> = BTreeMap::new();
    if let Some(spec) = &layers.user.mcp {
        for (name, server) in &spec.servers {
            defined.insert(name.as_str(), ("user config", server));
        }
    }
    if let Some(spec) = &layers.managed.mcp {
        for (name, server) in &spec.servers {
            let replaced = defined.insert(name.as_str(), ("managed policy", server)).is_some();
            if replaced {
                notes.push(format!(
                    "mcp server '{name}' is defined by managed policy; the user \
                     configuration's definition was not used"
                ));
            }
        }
    }
    defined
}

/// One server, its shape checked and its capability claim answered.
///
/// The claim is checked here, against the same set the registry will check it
/// against at registration, so that a server asking for a capability this run
/// was never given is a message about the file that asked rather than a
/// registration failure after a process has already been started.
fn server_config(
    layer: &str,
    name: &str,
    server: &McpServer,
    capability_set: &CapabilitySet,
) -> Result<McpServerConfig> {
    let transport = transport_of(layer, name, server)?;

    let timeout_ms = match &server.timeout {
        Some(text) => {
            let ms = parse_duration_ms(layer, text).map_err(|_| ConfigError::InvalidMcpServer {
                layer: layer.to_string(),
                server: name.to_string(),
                detail: format!("'{text}' is not a duration; use ms, s, m, or h"),
            })?;
            // Zero is a timeout that fires before the call is sent. It would
            // make every tool under this server fail in a way that looks like
            // the server is down, so it is refused where it is written.
            if ms == 0 {
                return Err(ConfigError::InvalidMcpServer {
                    layer: layer.to_string(),
                    server: name.to_string(),
                    detail: "`timeout` is zero, which would cut every call off before it starts"
                        .to_string(),
                });
            }
            Some(ms)
        }
        None => None,
    };

    let requires: Vec<Capability> = server.requires.iter().map(Capability::new).collect();
    let missing: Vec<String> = capability_set
        .missing_from(&requires)
        .into_iter()
        .map(|c| c.as_str().to_string())
        .collect();
    if !missing.is_empty() {
        return Err(ConfigError::UnauthorizedMcpCapability {
            server: name.to_string(),
            capabilities: missing,
        });
    }

    let schema = match server.schema.as_deref() {
        None | Some("strict") => SchemaPolicy::Strict,
        Some("warn") => SchemaPolicy::Warn,
        Some(other) => {
            return Err(ConfigError::InvalidMcpServer {
                layer: layer.to_string(),
                server: name.to_string(),
                detail: format!(
                    "`schema` is '{other}'; it is either \"strict\" (the server is refused when a \
                     tool cannot be checked) or \"warn\" (it is registered, and the tool is named)"
                ),
            });
        }
    };

    Ok(McpServerConfig { name: name.to_string(), transport, requires, timeout_ms, schema })
}

/// Which of the two shapes a server entry is, refusing anything in between.
///
/// The fields of both transports live on one type, so "a stdio server that also
/// has headers" is expressible and meaningless. It is refused rather than
/// ignored: a file that says two things at once is a file whose author believed
/// something that is not so.
fn transport_of(layer: &str, name: &str, server: &McpServer) -> Result<McpTransport> {
    let refuse = |detail: String| ConfigError::InvalidMcpServer {
        layer: layer.to_string(),
        server: name.to_string(),
        detail,
    };

    match (&server.command, &server.url) {
        (Some(_), Some(_)) => {
            Err(refuse("it names both a `command` and a `url`; a server is reached one way".into()))
        }
        (None, None) => {
            Err(refuse("it names neither a `command` (stdio) nor a `url` (streamable http)".into()))
        }
        (Some(command), None) => {
            if !server.headers.is_empty() {
                return Err(refuse(
                    "`headers` belong to a `url` server, not one reached by `command`".into(),
                ));
            }
            Ok(McpTransport::Stdio {
                command: command.clone(),
                args: server.args.clone(),
                env: server.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                cwd: server.cwd.clone(),
            })
        }
        (None, Some(url)) => {
            if !server.args.is_empty() || !server.env.is_empty() || server.cwd.is_some() {
                return Err(refuse(
                    "`args`, `env`, and `cwd` belong to a server reached by `command`, not by `url`"
                        .into(),
                ));
            }
            Ok(McpTransport::Http {
                url: url.clone(),
                headers: server.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            })
        }
    }
}

/// Every subagent provider any layer defines, by name, with the layer that
/// defined it.
///
/// Managed last, for the reason the servers are: a delegation starts a program,
/// and the one thing a user configuration must not be able to do is point a
/// provider the operator pinned at a program of its own.
fn defined_subagents<'a>(
    layers: &'a Layers,
    notes: &mut Vec<String>,
) -> BTreeMap<&'a str, (&'static str, &'a SubagentSpec)> {
    let mut defined: BTreeMap<&str, (&'static str, &SubagentSpec)> = BTreeMap::new();
    if let Some(specs) = &layers.user.subagents {
        for (name, spec) in specs {
            defined.insert(name.as_str(), ("user config", spec));
        }
    }
    if let Some(specs) = &layers.managed.subagents {
        for (name, spec) in specs {
            let replaced = defined.insert(name.as_str(), ("managed policy", spec)).is_some();
            if replaced {
                notes.push(format!(
                    "subagent provider '{name}' is defined by managed policy; the user \
                     configuration's definition was not used"
                ));
            }
        }
    }
    defined
}

/// One provider, its ceiling read and its capability claim answered.
///
/// The claim is checked before a run starts rather than when a delegation asks,
/// so that a provider asking for a capability this run was never given is a
/// message about the file that asked instead of a refusal in the middle of a
/// turn that has already forked a child.
fn subagent_config(
    layer: &str,
    provider: &str,
    spec: &SubagentSpec,
    capability_set: &CapabilitySet,
) -> Result<SubagentConfig> {
    let refuse = |detail: String| ConfigError::InvalidSubagent {
        layer: layer.to_string(),
        provider: provider.to_string(),
        detail,
    };

    if spec.command.trim().is_empty() {
        return Err(refuse("`command` is empty, so there is no program to run".to_string()));
    }

    let ceiling_ms = match &spec.timeout {
        Some(text) => Some(
            parse_duration_ms(layer, text)
                .map_err(|_| refuse(format!("'{text}' is not a duration; use ms, s, m, or h")))?,
        ),
        None => None,
    };

    if ceiling_ms == Some(0) {
        return Err(refuse(
            "`timeout` is zero, which would stop every child before it could answer".to_string(),
        ));
    }

    let requires: Vec<Capability> = spec.requires.iter().map(Capability::new).collect();
    let missing: Vec<String> = capability_set
        .missing_from(&requires)
        .into_iter()
        .map(|c| c.as_str().to_string())
        .collect();
    if !missing.is_empty() {
        return Err(ConfigError::UnauthorizedSubagentCapability {
            provider: provider.to_string(),
            capabilities: missing,
        });
    }

    Ok(SubagentConfig {
        provider: provider.to_string(),
        command: spec.command.clone(),
        args: spec.args.clone(),
        env: spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        cwd: spec.cwd.clone(),
        requires,
        ceiling_ms,
    })
}

/// The decision sidecar this run attaches.
///
/// Three answers, in order: a profile that turned it off wins outright; a run
/// that turned it on without any layer defining it is a refusal; and a
/// definition nobody turned off is the answer. The middle one is the refusal
/// that matters — a configuration that asks for a backend no file describes is
/// a file that says one thing while the run does another.
fn laya_backend(
    layers: &Layers,
    composed: &Profile,
    profile_id: &str,
    notes: &mut Vec<String>,
) -> Result<Option<ResolvedLaya>> {
    let toggle = composed.decisions.as_ref().and_then(|decisions| decisions.laya);
    if toggle == Some(false) {
        return Ok(None);
    }

    let Some((layer, spec)) = defined_laya(layers, notes) else {
        return if toggle == Some(true) {
            Err(ConfigError::UndefinedLaya { profile: profile_id.to_string() })
        } else {
            Ok(None)
        };
    };

    if let Some(threshold) = spec.threshold
        && !(0.0..=1.0).contains(&threshold)
    {
        return Err(ConfigError::InvalidLaya {
            layer: layer.to_string(),
            detail: format!("threshold {threshold} is outside 0..=1"),
        });
    }
    if spec.url.trim().is_empty() {
        return Err(ConfigError::InvalidLaya {
            layer: layer.to_string(),
            detail: "`url` is empty".to_string(),
        });
    }

    Ok(Some(ResolvedLaya {
        url: spec.url.clone(),
        model: spec.model.clone(),
        threshold: spec.threshold,
    }))
}

/// The sidecar any layer defines, with the layer that defined it.
///
/// Managed last, so an operator's definition replaces a user's of the same
/// name and says so — the one thing a user configuration must not be able to do
/// is point the operator's sidecar somewhere of its own.
fn defined_laya<'a>(
    layers: &'a Layers,
    notes: &mut Vec<String>,
) -> Option<(&'static str, &'a LayaSpec)> {
    let mut defined = layers
        .user
        .decisions
        .as_ref()
        .and_then(|spec| spec.laya.as_ref())
        .map(|laya| ("user config", laya));

    if let Some(laya) = layers.managed.decisions.as_ref().and_then(|spec| spec.laya.as_ref())
        && defined.replace(("managed policy", laya)).is_some()
    {
        notes.push(
            "decision backend 'laya' is defined by managed policy; the user configuration's \
             definition was not used"
                .to_string(),
        );
    }

    defined
}

/// A digest of the resolved policy, for the snapshot's `policy_version`.
fn policy_fingerprint(policy: &ResolvedPolicy) -> String {
    let describe = |directives: &[Directive]| {
        let mut rules: Vec<String> = directives
            .iter()
            .map(|directive| {
                format!("{}:{}", disposition_name(directive.decision), directive.capability)
            })
            .collect();
        rules.sort();
        rules
    };
    digest::json(&serde_json::json!({
        "builtin_defaults": !policy.replaces_defaults,
        "managed_deny": policy.managed_deny.iter().map(Capability::as_str).collect::<Vec<_>>(),
        "rules": describe(&policy.rules),
        "overrides": describe(&policy.overrides),
    }))
}

fn disposition_name(decision: PolicyDecision) -> &'static str {
    match decision {
        PolicyDecision::Allow => "allow",
        PolicyDecision::Ask => "ask",
        PolicyDecision::Deny => "deny",
    }
}

impl From<Disposition> for PolicyDecision {
    fn from(disposition: Disposition) -> Self {
        match disposition {
            Disposition::Allow => Self::Allow,
            Disposition::Ask => Self::Ask,
            Disposition::Deny => Self::Deny,
        }
    }
}
