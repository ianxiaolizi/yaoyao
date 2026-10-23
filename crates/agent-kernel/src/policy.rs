//! PolicyEngine — fail-closed policy evaluation over capability requests.
//!
//! A tool request lists the capabilities it needs; the engine folds them into a
//! single decision. Precedence, highest to lowest:
//!
//! ```text
//! managed deny > policy deny > sandbox capability unavailable
//!   > policy ask > user approval (lease) > policy allow
//! ```
//!
//! Anything not covered by an explicit rule fails closed to `Deny`. A grant — a
//! capability an approval has already put in effect — can satisfy a `policy ask`
//! but can never override a managed deny, a policy deny, or a missing sandbox
//! capability.
//!
//! A caller hands grants over as capabilities, not as `LeaseSpec`s. Whether a
//! lease is *still* in effect is a question about the session's log — which turn
//! it was granted in, how long ago — and about the clock, and the engine has
//! neither. A second place that judged liveness could disagree with the one that
//! handed the grant over, which is how a policy engine comes to say "allowed"
//! about a lease nobody is standing behind (YA-16).
//!
//! Rules may be narrowed to a tool/path/command prefix (Codex ExecPolicy style).
//! A matcher is best-effort routing, not the security boundary: the hard
//! boundary remains the ExecutionEnvironment and ResourceGuard.

use std::collections::HashSet;

use agent_ports::execution_environment::SandboxMode;
use agent_protocol::permissions::{Capability, PolicyDecision};

/// A rule's optional match context. `None` fields match anything; a set field
/// only matches when the context carries a value with that prefix.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleMatcher {
    pub tool_name: Option<String>,
    pub path: Option<String>,
    pub command: Option<String>,
}

/// The context a policy decision is made in, derived from a tool invocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyContext<'a> {
    pub tool_name: &'a str,
    pub path: Option<&'a str>,
    pub command: Option<&'a str>,
}

/// Which layer refused a capability.
///
/// This is the precedence above, made sayable: the layers are tried in order and
/// the first to say no is the one that refused. The engine computed this all
/// along and threw it away, which is why a refusal could only ever be reported
/// as "denied by policy" whatever had actually said no — and two of these four
/// are not policy decisions at all. A capability the sandbox does not provide is
/// a fact about the build; one nothing granted is a rule somebody has not
/// written yet. Both are fixed in a different place from a rule that forbids it,
/// and a refusal that does not name the layer leaves the reader to guess which
/// of them they are looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// An administrator's managed policy denies it. Nothing below this layer can
    /// override the decision.
    ManagedPolicy,
    /// A profile or configuration rule says deny.
    Rule,
    /// The bound execution environment does not provide the capability, so
    /// nothing here could grant it.
    SandboxUnavailable,
    /// This build provides the capability and no rule grants it.
    Ungranted,
}

impl Refusal {
    /// How the layer reads inside a refusal a person is shown.
    pub fn described(self) -> &'static str {
        match self {
            Refusal::ManagedPolicy => "is denied by managed policy",
            Refusal::Rule => "is denied by a policy rule",
            Refusal::SandboxUnavailable => "is not provided by this build's execution sandbox",
            Refusal::Ungranted => "is not granted by any policy rule",
        }
    }
}

/// A profile/config rule prescribing a disposition for a capability, optionally
/// narrowed to a tool/path/command prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRule {
    disposition: PolicyDecision,
    capability: Capability,
    matcher: RuleMatcher,
}

impl PolicyRule {
    pub fn allow(capability: Capability) -> Self {
        Self::new(PolicyDecision::Allow, capability)
    }

    pub fn ask(capability: Capability) -> Self {
        Self::new(PolicyDecision::Ask, capability)
    }

    pub fn deny(capability: Capability) -> Self {
        Self::new(PolicyDecision::Deny, capability)
    }

    fn new(disposition: PolicyDecision, capability: Capability) -> Self {
        Self { disposition, capability, matcher: RuleMatcher::default() }
    }

    /// Narrow this rule to invocations matching `matcher`.
    pub fn matching(mut self, matcher: RuleMatcher) -> Self {
        self.matcher = matcher;
        self
    }

    fn disposition(&self) -> PolicyDecision {
        self.disposition
    }
}

impl RuleMatcher {
    /// Whether this rule is about the capability alone, with no tool, path or
    /// command narrowing.
    fn is_match_all(&self) -> bool {
        self.tool_name.is_none() && self.path.is_none() && self.command.is_none()
    }

    fn matches(&self, ctx: Option<&PolicyContext>) -> bool {
        let Some(ctx) = ctx else {
            // No context: only a match-all matcher (all fields None) applies.
            return self.is_match_all();
        };
        let tool_ok = self.tool_name.as_ref().is_none_or(|m| ctx.tool_name.starts_with(m.as_str()));
        let path_ok = match (&self.path, ctx.path) {
            (Some(m), Some(p)) => p.starts_with(m.as_str()),
            (Some(_), None) => false,
            (None, _) => true,
        };
        let command_ok = match (&self.command, ctx.command) {
            (Some(m), Some(c)) => c.starts_with(m.as_str()),
            (Some(_), None) => false,
            (None, _) => true,
        };
        tool_ok && path_ok && command_ok
    }

    /// A specificity key for most-specific-rule precedence: more constrained
    /// fields first, then longer prefixes. Higher is more specific.
    fn specificity(&self) -> (usize, usize, usize, usize) {
        let non_none = usize::from(self.tool_name.is_some())
            + usize::from(self.path.is_some())
            + usize::from(self.command.is_some());
        let tool_len = self.tool_name.as_ref().map_or(0, |s| s.len());
        let path_len = self.path.as_ref().map_or(0, |s| s.len());
        let command_len = self.command.as_ref().map_or(0, |s| s.len());
        (non_none, tool_len, path_len, command_len)
    }
}

/// The engine, assembled from injected rules and sandbox capabilities.
#[derive(Debug, Clone, Default)]
pub struct PolicyEngine {
    managed_deny: HashSet<Capability>,
    rules: Vec<PolicyRule>,
    sandbox: HashSet<Capability>,
}

impl PolicyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Capabilities that are always denied, no matter what profile, project
    /// config, or user approval says.
    pub fn with_managed_deny(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.managed_deny.extend(caps);
        self
    }

    /// Profile/config rules, in no particular order. Multiple rules may target
    /// the same capability with different matchers; the most specific match
    /// wins, and ties break toward the strictest disposition.
    pub fn with_rules(mut self, rules: impl IntoIterator<Item = PolicyRule>) -> Self {
        self.rules.extend(rules);
        self
    }

    /// Rules that **replace** the existing disposition for their capability.
    ///
    /// For configuration knobs that name one capability's disposition (a
    /// profile's `[policy] shell = "allow"`). An appended rule would tie with
    /// the built-in rule for the same capability, and a tie goes to the
    /// strictest — so the knob would read `allow` while the run kept asking.
    /// Installing an override removes the existing match-all rule for that
    /// capability first; rules narrowed to a tool, path or command are left
    /// alone, because they say something narrower and still win on specificity.
    pub fn with_override_rules(mut self, rules: impl IntoIterator<Item = PolicyRule>) -> Self {
        for rule in rules {
            let capability = rule.capability.clone();
            self.rules.retain(|existing| {
                existing.capability != capability || !existing.matcher.is_match_all()
            });
            self.rules.push(rule);
        }
        self
    }

    /// Capabilities the bound execution environment actually provides.
    pub fn with_sandbox(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.sandbox.extend(caps);
        self
    }

    /// Fold the requested capabilities into one decision, without a match
    /// context. `grants` are the capabilities an in-effect approval covers.
    pub fn evaluate(&self, capabilities: &[Capability], grants: &[Capability]) -> PolicyDecision {
        self.evaluate_with_ctx(capabilities, None, grants)
    }

    /// Whether a tool needing `capabilities` could ever run in this run, at
    /// some tool, path or command.
    ///
    /// This is a question about the surface, not about a call: it asks
    /// whether advertising the tool would offer the model something any
    /// invocation could use. A capability narrowed by a rule counts as open —
    /// the policy refuses particular paths, and only the call's own context
    /// can answer that. An undeclared capability list counts as open for the
    /// same kind of reason: the call-time decision, not the surface, is what
    /// refuses it.
    pub fn could_allow(&self, capabilities: &[Capability]) -> bool {
        if capabilities.is_empty() {
            return true;
        }
        if capabilities.iter().any(|cap| self.managed_deny.contains(cap)) {
            return false;
        }
        if capabilities.iter().any(|cap| !self.sandbox.contains(cap)) {
            return false;
        }
        if capabilities.iter().any(|cap| {
            self.rules.iter().any(|rule| &rule.capability == cap && !rule.matcher.is_match_all())
        }) {
            return true;
        }
        !matches!(self.evaluate(capabilities, &[]), PolicyDecision::Deny)
    }

    /// As [`PolicyEngine::evaluate`], but with a tool/path/command context for
    /// rule matchers.
    ///
    /// An empty request is `Deny`. A tool that declares no capabilities is not a
    /// tool that needs nothing — it is one nothing has been said about, and the
    /// rule for "no rule covers this" applies to the request as a whole exactly
    /// as it does to one capability in it. Letting an empty list through would
    /// make "forgot to declare" the one way to skip the engine entirely.
    pub fn evaluate_with_ctx(
        &self,
        capabilities: &[Capability],
        ctx: Option<&PolicyContext>,
        grants: &[Capability],
    ) -> PolicyDecision {
        if capabilities.is_empty() {
            return PolicyDecision::Deny;
        }
        let mut result = PolicyDecision::Allow;
        for cap in capabilities {
            result = combine(result, self.decide_one(cap, ctx, grants).decision());
        }
        result
    }

    /// The capabilities this evaluation refused, each with the layer that
    /// refused it, in the order they were requested.
    ///
    /// Empty when the decision is not `Deny`, and — the one exception — when the
    /// request carried no capabilities: that `Deny` has no capability to name,
    /// so there is no layer to report for it and wording it is the caller's job.
    /// Everywhere else the same walk that produced the decision produced this,
    /// and the two cannot disagree about what happened. A caller that has seen a
    /// `Deny` asks this rather than re-deriving the precedence: the precedence is
    /// the engine's, and a second copy of it is a second copy to keep in step.
    pub fn refusals(
        &self,
        capabilities: &[Capability],
        ctx: Option<&PolicyContext>,
        grants: &[Capability],
    ) -> Vec<(Capability, Refusal)> {
        capabilities
            .iter()
            .filter_map(|cap| match self.decide_one(cap, ctx, grants) {
                Verdict::Deny(why) => Some((cap.clone(), why)),
                Verdict::Allow | Verdict::Ask => None,
            })
            .collect()
    }

    fn decide_one(
        &self,
        cap: &Capability,
        ctx: Option<&PolicyContext>,
        grants: &[Capability],
    ) -> Verdict {
        // 1. managed deny is absolute.
        if self.managed_deny.contains(cap) {
            return Verdict::Deny(Refusal::ManagedPolicy);
        }
        let disposition = self.rule_disposition(cap, ctx);
        // 2. policy deny.
        if disposition == Some(PolicyDecision::Deny) {
            return Verdict::Deny(Refusal::Rule);
        }
        // 3. sandbox capability unavailable -> fail closed, never downgrade.
        if !self.sandbox.contains(cap) {
            return Verdict::Deny(Refusal::SandboxUnavailable);
        }
        // 4. policy ask -> satisfied by a capability an approval covers, else ask.
        if disposition == Some(PolicyDecision::Ask) {
            return if grants.contains(cap) { Verdict::Allow } else { Verdict::Ask };
        }
        // 5. policy allow (a grant is a no-op upgrade here).
        if disposition == Some(PolicyDecision::Allow) {
            return Verdict::Allow;
        }
        // 6. nothing granted it: this build provides the capability and no rule
        // says to use it, so the fail-closed answer is a decision waiting to be
        // made rather than one that was made.
        Verdict::Deny(Refusal::Ungranted)
    }

    /// The disposition of the most specific rule for `cap` whose matcher matches
    /// `ctx`. On a specificity tie, the strictest disposition wins (fail
    /// closed). `None` when no rule matches (treated as fail-closed deny above).
    fn rule_disposition(
        &self,
        cap: &Capability,
        ctx: Option<&PolicyContext>,
    ) -> Option<PolicyDecision> {
        let mut best: Option<((usize, usize, usize, usize), PolicyDecision)> = None;
        for rule in &self.rules {
            if rule.capability != *cap || !rule.matcher.matches(ctx) {
                continue;
            }
            let specificity = rule.matcher.specificity();
            let decision = rule.disposition();
            best = Some(match best {
                None => (specificity, decision),
                Some((best_spec, _)) if specificity > best_spec => (specificity, decision),
                Some((best_spec, best_dec)) if specificity == best_spec => {
                    (best_spec, combine(best_dec, decision))
                }
                Some(keep) => keep,
            });
        }
        best.map(|(_, decision)| decision)
    }
}

/// One capability's outcome, carrying the refusing layer when there is one.
///
/// Private, and the only shape `decide_one` has: a decision and its reason are
/// produced together or the reason is lost, and losing it is exactly the bug
/// this exists to close. [`PolicyDecision`] stays the engine's public answer.
enum Verdict {
    Allow,
    Ask,
    Deny(Refusal),
}

impl Verdict {
    fn decision(self) -> PolicyDecision {
        match self {
            Verdict::Allow => PolicyDecision::Allow,
            Verdict::Ask => PolicyDecision::Ask,
            Verdict::Deny(_) => PolicyDecision::Deny,
        }
    }
}

/// The framework's default policy: read and write the workspace freely, inspect
/// its history freely, but ask the user before spawning processes. The bound
/// sandbox advertises what this build provides, which is a superset of what the
/// default rules grant — so a rule for a capability nothing advertised still
/// fails closed, and an advertised capability nobody ruled on is still a no.
///
/// `git.status` is allowed because it is a read of the workspace the agent is
/// already trusted to read — it is not a weaker gate than `fs.read`, which is
/// already allow. Anything that *writes* through git (`git.commit`, `git.push`)
/// is deliberately absent from both lists: unadvertised, and therefore denied by
/// default, until a milestone owns that decision. `subagent.spawn` and
/// `network.request` are the other way round — advertised by the sandbox, ruled
/// on by nobody — and [`default_sandbox_capabilities`] says why.
///
/// `memory.read` is allowed for the same reason `fs.read` is: it reads material
/// the run already owns. The log this searches is the session's own history, so
/// granting it adds no reach the run did not have when it wrote that history.
pub fn default_policy() -> PolicyEngine {
    default_policy_with(&default_sandbox_capabilities())
}

/// The built-in rule set, over what this run's backend actually advertises.
///
/// Same rules as [`default_policy`]; the sandbox set is the caller's because a
/// read-only run or a confinement backend provides less than a local process
/// does, and a rule that names a capability the backend lacks is refused
/// before it is consulted. A run must not be able to allow its way into
/// something the backend cannot enforce.
pub fn default_policy_with(advertised: &[Capability]) -> PolicyEngine {
    PolicyEngine::new()
        .with_rules([
            PolicyRule::allow(Capability::new(Capability::FS_READ)),
            PolicyRule::allow(Capability::new(Capability::FS_WRITE)),
            PolicyRule::allow(Capability::new(Capability::GIT_STATUS)),
            PolicyRule::allow(Capability::new(Capability::MEMORY_READ)),
            PolicyRule::ask(Capability::new(Capability::PROCESS_SPAWN)),
        ])
        .with_sandbox(advertised.iter().cloned())
}

/// What the bound execution environment actually provides.
///
/// Separate from the rules because the two answer different questions: rules
/// say what a configuration *wants* to permit, and this says what the backend
/// can do at all. A profile that grants a capability absent here is not an
/// error — it is an unadvertised capability, and the engine fails it closed.
///
/// `subagent.spawn` and `network.request` are here and in no default rule,
/// which makes them the capabilities advertised and ungranted: unlike
/// `git.commit` — absent from both lists because nothing owns that decision —
/// these are things a profile is meant to opt into, and an opt-in that could not
/// be expressed would be a capability this build ships without. So the
/// environment says it can, the default rules say nothing, and a profile that
/// wants it has to ask.
///
/// `network.request` is here because this build reaches the network: an MCP
/// server may be configured by `url` rather than by `command`, and the adapter
/// speaks Streamable HTTP to it. Leaving it unadvertised did not make the build
/// safe — it made `policy.network` a setting that could not take effect, since
/// an unadvertised capability is refused before any rule is consulted. A
/// configuration surface that cannot be honoured is worse than one that is
/// absent: it reads as a promise. Advertising is still not granting — nothing in
/// [`default_policy`] allows this, and a run that wants an HTTP MCP server has
/// to say so.
pub fn default_sandbox_capabilities() -> Vec<Capability> {
    SandboxMode::LocalProcess.advertised_capabilities()
}

/// Deny dominates, then Ask, then Allow.
fn combine(a: PolicyDecision, b: PolicyDecision) -> PolicyDecision {
    match (a, b) {
        (PolicyDecision::Deny, _) | (_, PolicyDecision::Deny) => PolicyDecision::Deny,
        (PolicyDecision::Ask, _) | (_, PolicyDecision::Ask) => PolicyDecision::Ask,
        _ => PolicyDecision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(s: &str) -> Capability {
        Capability::new(s)
    }

    fn ctx(tool: &str) -> PolicyContext<'_> {
        PolicyContext { tool_name: tool, path: None, command: None }
    }

    #[test]
    fn empty_capability_set_is_denied() {
        // This test used to assert the opposite ("…_is_allowed"), and the
        // reversal is the point of YA-07: an empty declaration is a tool nobody
        // has said anything about, not one that needs nothing. A permissive
        // engine still says no, which is what makes this a rule about the
        // request rather than about the rules.
        let engine = PolicyEngine::new();
        assert_eq!(engine.evaluate(&[], &[]), PolicyDecision::Deny);
        // The one case where a `Deny` has no layer to name — the caller words it.
        assert!(engine.refusals(&[], None, &[]).is_empty());
    }

    #[test]
    fn unknown_capability_fails_closed() {
        let engine = PolicyEngine::new().with_sandbox([cap(Capability::FS_READ)]);
        assert_eq!(engine.evaluate(&[cap("mystery.capability")], &[]), PolicyDecision::Deny);
    }

    #[test]
    fn an_override_replaces_the_disposition_it_names() {
        // The shape `[policy] shell = "allow"` produces. Without an override
        // the two match-all rules tie and the stricter one wins, so the knob
        // would read `allow` while every call still waited on a human.
        let engine = PolicyEngine::new()
            .with_sandbox([cap(Capability::PROCESS_SPAWN)])
            .with_rules([PolicyRule::ask(cap(Capability::PROCESS_SPAWN))])
            .with_override_rules([PolicyRule::allow(cap(Capability::PROCESS_SPAWN))]);
        assert_eq!(engine.evaluate(&[cap(Capability::PROCESS_SPAWN)], &[]), PolicyDecision::Allow);
    }

    #[test]
    fn managed_deny_cannot_be_overridden_by_policy_allow() {
        let engine = PolicyEngine::new()
            .with_managed_deny([cap(Capability::NETWORK_REQUEST)])
            .with_rules([PolicyRule::allow(cap(Capability::NETWORK_REQUEST))])
            .with_sandbox([cap(Capability::NETWORK_REQUEST)]);
        assert_eq!(engine.evaluate(&[cap(Capability::NETWORK_REQUEST)], &[]), PolicyDecision::Deny);
    }

    #[test]
    fn managed_deny_cannot_be_overridden_by_user_approval() {
        let engine = PolicyEngine::new()
            .with_managed_deny([cap(Capability::NETWORK_REQUEST)])
            .with_rules([PolicyRule::ask(cap(Capability::NETWORK_REQUEST))])
            .with_sandbox([cap(Capability::NETWORK_REQUEST)]);
        assert_eq!(
            engine
                .evaluate(&[cap(Capability::NETWORK_REQUEST)], &[cap(Capability::NETWORK_REQUEST)]),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn policy_deny_cannot_be_overridden_by_user_approval() {
        let engine = PolicyEngine::new()
            .with_rules([PolicyRule::deny(cap(Capability::FS_WRITE))])
            .with_sandbox([cap(Capability::FS_WRITE)]);
        assert_eq!(
            engine.evaluate(&[cap(Capability::FS_WRITE)], &[cap(Capability::FS_WRITE)]),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn sandbox_unavailable_fails_closed() {
        let engine = PolicyEngine::new().with_rules([PolicyRule::allow(cap(Capability::FS_READ))]);
        assert_eq!(engine.evaluate(&[cap(Capability::FS_READ)], &[]), PolicyDecision::Deny);
    }

    #[test]
    fn the_surface_keeps_only_capabilities_that_could_run() {
        let engine = PolicyEngine::new()
            .with_rules([PolicyRule::allow(cap(Capability::FS_READ))])
            .with_sandbox([
                cap(Capability::FS_READ),
                cap(Capability::FS_WRITE),
                cap(Capability::PROCESS_SPAWN),
            ]);
        assert!(engine.could_allow(&[cap(Capability::FS_READ)]));
        assert!(!engine.could_allow(&[cap(Capability::FS_WRITE)]), "no rule grants it");
        assert!(!engine.could_allow(&[cap(Capability::PROCESS_SPAWN)]));

        // An ask is how a tool runs — with an approval — so it stays offered.
        let asking = PolicyEngine::new()
            .with_rules([PolicyRule::ask(cap(Capability::PROCESS_SPAWN))])
            .with_sandbox([cap(Capability::PROCESS_SPAWN)]);
        assert!(asking.could_allow(&[cap(Capability::PROCESS_SPAWN)]));

        // A narrowed rule says which invocations, not whether the capability
        // exists: the tool stays, and the call decides.
        let narrowed = PolicyEngine::new()
            .with_rules([PolicyRule::allow(cap(Capability::FS_READ)).matching(RuleMatcher {
                path: Some("notes/".to_string()),
                ..RuleMatcher::default()
            })])
            .with_sandbox([cap(Capability::FS_READ)]);
        assert!(narrowed.could_allow(&[cap(Capability::FS_READ)]));

        // Nothing the backend provides: no rule can make the call run.
        let no_backend =
            PolicyEngine::new().with_rules([PolicyRule::allow(cap(Capability::FS_READ))]);
        assert!(!no_backend.could_allow(&[cap(Capability::FS_READ)]));
    }

    #[test]
    fn policy_ask_without_a_grant_returns_ask() {
        let engine = PolicyEngine::new()
            .with_rules([PolicyRule::ask(cap(Capability::PROCESS_SPAWN))])
            .with_sandbox([cap(Capability::PROCESS_SPAWN)]);
        assert_eq!(engine.evaluate(&[cap(Capability::PROCESS_SPAWN)], &[]), PolicyDecision::Ask);
    }

    #[test]
    fn policy_ask_with_a_matching_grant_returns_allow() {
        let engine = PolicyEngine::new()
            .with_rules([PolicyRule::ask(cap(Capability::PROCESS_SPAWN))])
            .with_sandbox([cap(Capability::PROCESS_SPAWN)]);
        assert_eq!(
            engine.evaluate(&[cap(Capability::PROCESS_SPAWN)], &[cap(Capability::PROCESS_SPAWN)]),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn a_grant_for_a_different_capability_does_not_grant() {
        let engine = PolicyEngine::new()
            .with_rules([PolicyRule::ask(cap(Capability::PROCESS_SPAWN))])
            .with_sandbox([cap(Capability::PROCESS_SPAWN), cap(Capability::FS_READ)]);
        assert_eq!(
            engine.evaluate(&[cap(Capability::PROCESS_SPAWN)], &[cap(Capability::FS_READ)]),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn a_grant_covers_only_the_capability_that_was_granted() {
        // One request, two capabilities, one of them granted: the request is
        // still an ask. The aggregate is the strictest answer over the whole
        // request, so a grant for `fs.read` cannot carry `process.spawn` along
        // with it — which is the shape a blanket "the user said yes once" would
        // take.
        let engine = PolicyEngine::new()
            .with_rules([
                PolicyRule::ask(cap(Capability::FS_READ)),
                PolicyRule::ask(cap(Capability::PROCESS_SPAWN)),
            ])
            .with_sandbox([cap(Capability::FS_READ), cap(Capability::PROCESS_SPAWN)]);
        assert_eq!(
            engine.evaluate(
                &[cap(Capability::FS_READ), cap(Capability::PROCESS_SPAWN)],
                &[cap(Capability::FS_READ)]
            ),
            PolicyDecision::Ask
        );
        assert_eq!(
            engine.evaluate(
                &[cap(Capability::FS_READ), cap(Capability::PROCESS_SPAWN)],
                &[cap(Capability::FS_READ), cap(Capability::PROCESS_SPAWN)]
            ),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn policy_allow_returns_allow() {
        let engine = PolicyEngine::new()
            .with_rules([PolicyRule::allow(cap(Capability::FS_READ))])
            .with_sandbox([cap(Capability::FS_READ)]);
        assert_eq!(engine.evaluate(&[cap(Capability::FS_READ)], &[]), PolicyDecision::Allow);
    }

    #[test]
    fn multiple_capabilities_aggregate_to_strictest() {
        let engine = PolicyEngine::new()
            .with_rules([
                PolicyRule::allow(cap(Capability::FS_READ)),
                PolicyRule::ask(cap(Capability::PROCESS_SPAWN)),
            ])
            .with_sandbox([cap(Capability::FS_READ), cap(Capability::PROCESS_SPAWN)]);
        assert_eq!(
            engine.evaluate(&[cap(Capability::FS_READ), cap(Capability::PROCESS_SPAWN)], &[]),
            PolicyDecision::Ask
        );

        let engine = PolicyEngine::new()
            .with_rules([
                PolicyRule::allow(cap(Capability::FS_READ)),
                PolicyRule::deny(cap(Capability::FS_WRITE)),
            ])
            .with_sandbox([cap(Capability::FS_READ), cap(Capability::FS_WRITE)]);
        assert_eq!(
            engine.evaluate(&[cap(Capability::FS_READ), cap(Capability::FS_WRITE)], &[]),
            PolicyDecision::Deny
        );
    }

    // ExecPolicy-style matcher tests: rules narrowed by tool/path/command.

    fn path_ctx<'a>(tool: &'a str, path: &'a str) -> PolicyContext<'a> {
        PolicyContext { tool_name: tool, path: Some(path), command: None }
    }

    #[test]
    fn path_matcher_narrows_a_rule() {
        // fs.read allowed generally, but reads under "secrets/" ask.
        let engine = PolicyEngine::new()
            .with_rules([
                PolicyRule::allow(cap(Capability::FS_READ)),
                PolicyRule::ask(cap(Capability::FS_READ)).matching(RuleMatcher {
                    path: Some("secrets/".to_string()),
                    ..RuleMatcher::default()
                }),
            ])
            .with_sandbox([cap(Capability::FS_READ)]);

        assert_eq!(
            engine.evaluate_with_ctx(
                &[cap(Capability::FS_READ)],
                Some(&path_ctx("fs.read", "src/main.rs")),
                &[]
            ),
            PolicyDecision::Allow
        );
        assert_eq!(
            engine.evaluate_with_ctx(
                &[cap(Capability::FS_READ)],
                Some(&path_ctx("fs.read", "secrets/.env")),
                &[]
            ),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn tool_matcher_narrows_a_rule() {
        let engine = PolicyEngine::new()
            .with_rules([
                PolicyRule::ask(cap(Capability::PROCESS_SPAWN)),
                PolicyRule::allow(cap(Capability::PROCESS_SPAWN)).matching(RuleMatcher {
                    tool_name: Some("shell.".to_string()),
                    ..RuleMatcher::default()
                }),
            ])
            .with_sandbox([cap(Capability::PROCESS_SPAWN)]);

        // "shell.run" is allow, but an unrelated process.spawn tool still asks.
        assert_eq!(
            engine.evaluate_with_ctx(
                &[cap(Capability::PROCESS_SPAWN)],
                Some(&ctx("shell.run")),
                &[]
            ),
            PolicyDecision::Allow
        );
        assert_eq!(
            engine.evaluate_with_ctx(
                &[cap(Capability::PROCESS_SPAWN)],
                Some(&ctx("build.run")),
                &[]
            ),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn matcher_that_does_not_match_falls_through_to_general_rule() {
        let engine = PolicyEngine::new()
            .with_rules([
                PolicyRule::allow(cap(Capability::FS_WRITE)),
                PolicyRule::deny(cap(Capability::FS_WRITE)).matching(RuleMatcher {
                    path: Some("node_modules/".to_string()),
                    ..RuleMatcher::default()
                }),
            ])
            .with_sandbox([cap(Capability::FS_WRITE)]);

        assert_eq!(
            engine.evaluate_with_ctx(
                &[cap(Capability::FS_WRITE)],
                Some(&path_ctx("fs.write", "src/lib.rs")),
                &[]
            ),
            PolicyDecision::Allow
        );
        assert_eq!(
            engine.evaluate_with_ctx(
                &[cap(Capability::FS_WRITE)],
                Some(&path_ctx("fs.write", "node_modules/x")),
                &[]
            ),
            PolicyDecision::Deny
        );
    }

    // The default policy is the one assembly actually installs, so what it grants
    // is a product decision, not a detail of the engine.
    #[test]
    fn the_default_policy_lets_the_shipped_tools_run() {
        let engine = default_policy();

        // The tools M5 wires into the binary must resolve, or the assembly ships
        // tools nobody can call.
        assert_eq!(engine.evaluate(&[cap(Capability::FS_READ)], &[]), PolicyDecision::Allow);
        assert_eq!(engine.evaluate(&[cap(Capability::FS_WRITE)], &[]), PolicyDecision::Allow);
        assert_eq!(engine.evaluate(&[cap(Capability::GIT_STATUS)], &[]), PolicyDecision::Allow);
        // Searching the session's own history is a read of what the run already
        // wrote, not a new reach.
        assert_eq!(engine.evaluate(&[cap(Capability::MEMORY_READ)], &[]), PolicyDecision::Allow);
        // Spawning still asks; the sandbox advertises it so an approval *can* be
        // granted, which is different from granting it.
        assert_eq!(engine.evaluate(&[cap(Capability::PROCESS_SPAWN)], &[]), PolicyDecision::Ask);
        // …and once one has been, the same call is answered without asking
        // again. This is the whole point of the grant path: `approve` is a
        // thing that changes what the next call does, not just what this one
        // did.
        assert_eq!(
            engine.evaluate(&[cap(Capability::PROCESS_SPAWN)], &[cap(Capability::PROCESS_SPAWN)]),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn the_default_policy_denies_git_that_writes() {
        let engine = default_policy();
        // No rule, no sandbox entry: fail closed. Adding a git tool that commits
        // must be a deliberate edit here, not an accident of wiring.
        assert_eq!(engine.evaluate(&[cap(Capability::GIT_COMMIT)], &[]), PolicyDecision::Deny);
        assert_eq!(engine.evaluate(&[cap(Capability::GIT_PUSH)], &[]), PolicyDecision::Deny);
    }

    #[test]
    fn the_default_policy_denies_delegation_but_a_profile_can_grant_it() {
        // The advertised-but-ungranted capability: nobody delegates under the
        // default rules, and the difference this makes to a profile is the
        // difference between a rule that works and one that fails closed.
        let engine = default_policy();
        assert_eq!(engine.evaluate(&[cap(Capability::SUBAGENT_SPAWN)], &[]), PolicyDecision::Deny);

        let asked = engine.clone().with_rules([PolicyRule::allow(cap(Capability::SUBAGENT_SPAWN))]);
        assert_eq!(
            asked.evaluate(&[cap(Capability::SUBAGENT_SPAWN)], &[]),
            PolicyDecision::Allow,
            "a profile that asks for it gets it, because the sandbox advertises it"
        );

        // And the same rule against an engine built with an empty sandbox — a
        // configuration whose rules replace the defaults — still fails closed,
        // which is what keeps the advertisement a fact about this machine rather
        // than a grant.
        let unadvertised =
            PolicyEngine::new().with_rules([PolicyRule::allow(cap(Capability::SUBAGENT_SPAWN))]);
        assert_eq!(
            unadvertised.evaluate(&[cap(Capability::SUBAGENT_SPAWN)], &[]),
            PolicyDecision::Deny
        );
    }

    // The engine used to answer "denied" and nothing else, so a refusal could
    // not say whether a rule forbade it, an administrator had, or the build
    // simply could not do it — three different problems, fixed in three
    // different places. These pin that the layer survives the decision.

    #[test]
    fn a_refusal_names_the_layer_that_refused() {
        let rule = PolicyEngine::new()
            .with_rules([PolicyRule::deny(cap(Capability::FS_WRITE))])
            .with_sandbox([cap(Capability::FS_WRITE)]);
        assert_eq!(
            rule.refusals(&[cap(Capability::FS_WRITE)], None, &[]),
            vec![(cap(Capability::FS_WRITE), Refusal::Rule)]
        );

        let managed = PolicyEngine::new()
            .with_managed_deny([cap(Capability::NETWORK_REQUEST)])
            .with_rules([PolicyRule::allow(cap(Capability::NETWORK_REQUEST))])
            .with_sandbox([cap(Capability::NETWORK_REQUEST)]);
        assert_eq!(
            managed.refusals(&[cap(Capability::NETWORK_REQUEST)], None, &[]),
            vec![(cap(Capability::NETWORK_REQUEST), Refusal::ManagedPolicy)],
            "managed policy outranks the allow beneath it, and says so"
        );

        // Advertised by the sandbox and ruled on by nobody: the capability
        // exists and nothing granted it. A rule that denied it would be the
        // other answer, and the two are not the same problem.
        assert_eq!(
            default_policy().refusals(&[cap(Capability::SUBAGENT_SPAWN)], None, &[]),
            vec![(cap(Capability::SUBAGENT_SPAWN), Refusal::Ungranted)]
        );

        // And the same for the other advertised-and-ungranted capability. The
        // sandbox says this build can reach the network — an MCP server over
        // `url` is why — and no default rule says it may, so a run that says
        // nothing still does not get it.
        assert_eq!(
            default_policy().refusals(&[cap(Capability::NETWORK_REQUEST)], None, &[]),
            vec![(cap(Capability::NETWORK_REQUEST), Refusal::Ungranted)]
        );

        // Not advertised at all: no configuration could grant it here, which is
        // a fact about the build rather than a decision anyone made.
        let unavailable =
            PolicyEngine::new().with_rules([PolicyRule::allow(cap(Capability::NETWORK_REQUEST))]);
        assert_eq!(
            unavailable.refusals(&[cap(Capability::NETWORK_REQUEST)], None, &[]),
            vec![(cap(Capability::NETWORK_REQUEST), Refusal::SandboxUnavailable)]
        );
    }

    #[test]
    fn refusals_agree_with_the_decision() {
        // One walk produces both, so the two can never tell a caller different
        // stories. Checked across the default policy's own surface, where all
        // three answers — allow, ask, deny — actually occur.
        let engine = default_policy();
        for capability in default_sandbox_capabilities() {
            let decision = engine.evaluate(std::slice::from_ref(&capability), &[]);
            let refused = engine.refusals(std::slice::from_ref(&capability), None, &[]);
            assert_eq!(
                decision == PolicyDecision::Deny,
                !refused.is_empty(),
                "{capability:?}: decision {decision:?}, refusals {refused:?}"
            );
        }

        // A capability nobody listed is refused, and the reason is the build's.
        let unknown = cap("mystery.capability");
        assert_eq!(
            engine.refusals(std::slice::from_ref(&unknown), None, &[]),
            vec![(unknown, Refusal::SandboxUnavailable)]
        );
    }
}
