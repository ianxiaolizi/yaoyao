//! Permission, approval, workspace, lease, and secret types shared across the
//! kernel, adapters, and durable events.
//!
//! Everything here is a serializable value or reference — never a live handle.
//! Secrets are referenced by name, not carried as plaintext; leases carry
//! references and metadata, never process handles or credentials.

use std::collections::BTreeSet;

use crate::ids::{ApprovalId, SessionId};
use crate::permission::{ActionFacts, ActionTarget, Reversibility, RiskLevel};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The outcome of evaluating a tool request against policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

/// Which layer refused a capability, and therefore where the fix belongs.
///
/// The policy engine tries its layers in a fixed order and the first to say no
/// is the one that refused. Two of these four are not policy decisions at all:
/// a capability the execution environment does not provide is a fact about the
/// build, and one no rule grants is a rule somebody has not written yet. Both
/// are fixed in a different place from a rule that forbids the call, so a
/// refusal that names only "denied by policy" leaves the reader to guess which
/// of the three they are looking at.
///
/// The vocabulary lives here, not in the engine, because it is written into
/// durable `tool/failed` payloads: whatever the log spells is a stable word a
/// reader and a counter agree on, and that has to be a protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalLayer {
    /// An administrator's managed policy denies it. Nothing below this layer
    /// can override the decision.
    ManagedPolicy,
    /// A profile or configuration rule says deny.
    Rule,
    /// The bound execution environment does not provide the capability, so
    /// nothing here could grant it.
    SandboxUnavailable,
    /// This build provides the capability and no rule grants it.
    Ungranted,
}

impl RefusalLayer {
    /// The word the log keeps. Stable: it is read by people and by anything
    /// that counts these.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ManagedPolicy => "managed_policy",
            Self::Rule => "rule",
            Self::SandboxUnavailable => "sandbox_unavailable",
            Self::Ungranted => "ungranted",
        }
    }

    /// How the layer reads inside a refusal a person is shown.
    pub fn described(self) -> &'static str {
        match self {
            Self::ManagedPolicy => "is denied by managed policy",
            Self::Rule => "is denied by a policy rule",
            Self::SandboxUnavailable => "is not provided by this build's execution sandbox",
            Self::Ungranted => "is not granted by any policy rule",
        }
    }
}

/// A namespaced capability identifier, such as `fs.read` or `process.spawn`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capability(pub String);

impl Capability {
    pub const FS_READ: &'static str = "fs.read";
    pub const FS_WRITE: &'static str = "fs.write";
    pub const FS_EXTERNAL_PATH: &'static str = "fs.external_path";
    pub const PROCESS_SPAWN: &'static str = "process.spawn";
    pub const NETWORK_REQUEST: &'static str = "network.request";
    /// A command that reaches the network on its own, outside any egress rule.
    ///
    /// Reserved for a later phase and advertised by nothing in this build: a
    /// process's traffic cannot be held to an origin allowlist, so a backend
    /// that offered this would offer exactly the reach `network.request` exists
    /// to bound. Declaring the name now is what makes "no backend advertises a
    /// raw process network" a statement about a capability rather than about an
    /// absence — a gap nothing names is a gap the next surface can fill by
    /// accident.
    pub const NETWORK_PROCESS: &'static str = "network.process";
    pub const NETWORK_PACKAGE_REGISTRY: &'static str = "network.package_registry";
    pub const GIT_STATUS: &'static str = "git.status";
    pub const GIT_COMMIT: &'static str = "git.commit";
    pub const GIT_PUSH: &'static str = "git.push";
    pub const SECRET_READ: &'static str = "secret.read";
    pub const SUBAGENT_SPAWN: &'static str = "subagent.spawn";
    /// Search this session's own history. Unlike `fs.read`, the material is the
    /// conversation itself — the grant is about reading a log the run already
    /// owns, not about reaching outside it.
    pub const MEMORY_READ: &'static str = "memory.read";
    /// Write to, or delete from, the long-term memory store (`memory.db`).
    ///
    /// Separate from `memory.read` because the two are different claims: reading
    /// what was already remembered changes nothing, while a write outlives the
    /// session that made it and a delete is real. The store sits beside the
    /// session logs rather than inside one, so this is the one memory capability
    /// that reaches past the run holding it.
    pub const MEMORY_WRITE: &'static str = "memory.write";

    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The capabilities some authority has granted — a profile, a user, a plugin
/// manifest's authorized set.
///
/// Ordered rather than hashed, for two reasons that both come down to
/// reproducibility: the set is recorded in the durable log, so the same grant
/// must serialize to the same bytes every run rather than in whatever order a
/// process's hash seed happens to produce; and "is `requires` a subset of
/// `authorized`" is asked once per plugin registration, where a sorted walk is
/// the direct way to answer it and to name what is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// An empty set. Nothing is authorized.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `capability` is in the set.
    pub fn contains(&self, capability: &Capability) -> bool {
        self.0.contains(capability)
    }

    /// Add a capability; returns whether it was newly added.
    pub fn insert(&mut self, capability: Capability) -> bool {
        self.0.insert(capability)
    }

    /// Whether nothing is authorized.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many capabilities are authorized.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The authorized capabilities, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }

    /// The members of `requested` this set does not contain.
    ///
    /// Empty means `requested` is a subset — the question a registration gate
    /// asks. Returned rather than reduced to a `bool` because the refusal has to
    /// say *which* capability was missing; "unauthorized" alone leaves an
    /// operator diffing two lists by hand.
    pub fn missing_from<'a>(&self, requested: &'a [Capability]) -> Vec<&'a Capability> {
        requested.iter().filter(|c| !self.0.contains(*c)).collect()
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a CapabilitySet {
    type Item = &'a Capability;
    type IntoIter = std::collections::btree_set::Iter<'a, Capability>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// The user's decision on a pending approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Grant,
    Deny,
}

/// How broadly a grant applies.
///
/// All three are grants, and the difference is only how long one is worth
/// something: `allow_once` is the call `resume` dispatches and nothing after it,
/// `allow_for_turn` ends when the session's next turn ends, and
/// `allow_for_session` ends on its own clock. Which of those is *currently* in
/// effect is a question about the session's log, so it is answered by
/// `agent-kernel`'s approval service — the policy engine, which has neither log
/// nor clock, is handed the answer and not the scope (YA-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    AllowOnce,
    AllowForTurn,
    AllowForSession,
}

/// Lifecycle status of a pending approval.
///
/// `Expired` and `Cancelled` are both "the wait ended without an answer", and
/// both are reached by the `agent-kernel` approval service: a request or a grant
/// that ran out of time folds to `Expired`, and a request taken back — the user
/// stopping a held session — is `Cancelled` (YA-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Granted,
    Denied,
    Expired,
    Cancelled,
}

/// A durable pending-approval fact.
///
/// The fields after `args_summary` bind the request to the one action it is
/// about (B4). A request written before they existed reads back with them all
/// absent, and an absent digest means "the old, capability-wide question" —
/// which no longer authorises an `L2`/`L3` call (see
/// [`LeaseSpec::matches_action`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub approval_id: ApprovalId,
    pub session_id: SessionId,
    pub capability: Capability,
    pub tool_name: String,
    /// A short, already-redacted summary of the arguments, for the human deciding.
    pub args_summary: String,
    pub status: ApprovalStatus,
    pub created_at: DateTime<Utc>,
    /// The exact action this question is about, by fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_digest: Option<String>,
    /// The risk the action carried when the question was raised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskLevel>,
    /// The work the action belongs to, and how many changes that work had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_revision: Option<u64>,
    /// Whether the action can be taken back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reversibility: Option<Reversibility>,
    /// What the action would touch — paths, commands, origins, secret
    /// references. Never values: see [`ActionTarget`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<ActionTarget>,
}

/// The user's reply to a pending approval — decision, scope, actor.
///
/// `action_digest` and `risk` are copied from the request being answered, so a
/// lease made from this reply is bound to the exact action a person saw.
/// `authority` names the channel the answer came through (`interactive_tty`,
/// `local_http`, `test`); it is an attestation written by the entry point that
/// checked it, and its absence means a reply written before B4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalReply {
    pub approval_id: ApprovalId,
    pub decision: ApprovalDecision,
    pub scope: ApprovalScope,
    pub actor: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
}

/// A named reference to a secret, never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    /// Which provider holds it, e.g. `env` or a profile key.
    pub source: String,
    /// The key/name within that provider.
    pub name: String,
}

impl SecretRef {
    /// The spelling a model writes in a tool argument.
    ///
    /// One constructor, so the parser in the gateway and every place that
    /// lists a reference to the model cannot disagree about the shape. It is a
    /// URI rather than a bare name because arguments are otherwise arbitrary
    /// text: a name alone could be a legitimate value, while a `secret://`
    /// scheme is something a caller has to have meant.
    pub fn reference(&self) -> String {
        format!("secret://{}/{}", self.source, self.name)
    }
}

/// A content-addressed reference to an artifact blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub hash: String,
    pub mime_type: String,
    pub size: u64,
}

impl ArtifactRef {
    /// A stable locator a reader can hand back to the artifact store.
    ///
    /// It is a scheme-qualified name rather than a filesystem path: the kernel,
    /// the loop, and the model all see this string, and none of them is allowed
    /// to know where the blob store happens to keep its files.
    pub fn locator(&self) -> String {
        format!("artifact://sha256/{}", self.hash)
    }

    /// The hash a locator names, if it is one this build wrote.
    ///
    /// The parser lives beside [`Self::locator`] so the two cannot disagree
    /// about the shape: a second copy in the CLI would keep working after the
    /// format here changed, and callers would then hold locators the writer no
    /// longer produces.
    ///
    /// A bare hash is not a locator and returns `None`; callers that accept
    /// that spelling apply it as their own convenience. Whether the result is
    /// a *blob name* is the store's check, not this one.
    pub fn hash_from_locator(locator: &str) -> Option<&str> {
        let hash = locator.strip_prefix("artifact://sha256/")?;
        (!hash.is_empty()).then_some(hash)
    }

    /// The structured reference written into tool-result events and returned to
    /// the model: what to ask for, how big it is, what it is, and how to get it.
    ///
    /// One constructor for the shape, so the log, the tool result, and any
    /// future reader cannot disagree about the field names.
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "locator": self.locator(),
            "bytes": self.size,
            "mime": self.mime_type,
        })
    }
}

/// What was redacted from a tool result — never the secret values themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionMetadata {
    pub redacted: bool,
    /// The redaction patterns actually applied (e.g. `api_key`, `token`), not
    /// the values. A name here means the text was changed.
    pub patterns: Vec<String>,
    /// Findings recorded *without* changing the text — the dry-run layer.
    ///
    /// The high-entropy detector reports here by default: a long mixed-class
    /// token may be a secret or may be a base64 blob in ordinary code, and the
    /// default is to say so rather than mangle a result whose meaning the
    /// caller cannot recover. A name here therefore never means "removed";
    /// `patterns` is the list that does.
    ///
    /// Absent from payloads written before this field existed, which is why it
    /// reads back as empty rather than as a malformed log.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A logical workspace: where reads and writes are allowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub workspace_root: String,
    pub writable_roots: Vec<String>,
    pub external_directories: Vec<String>,
}

/// A temporary execution capability a granted approval put in effect.
/// References and metadata only — no handles or credentials.
///
/// `expires_at` is `Some` only for a scope the clock ends. A turn-scoped grant
/// ends on the log instead — the turn it was for completing — which is not
/// something a timestamp can say, so a caller that needs "is this still worth
/// something" must ask the approval service rather than compare a time.
///
/// A lease is not "a capability the user allowed" any more: it is one action,
/// by fingerprint, in one task revision (B4, CONTRACT §7 #178). A lease whose
/// `action_digest` is absent was written before that and still reads — it just
/// does not cover an `L2`/`L3` action, which is what
/// [`Self::matches_action`] enforces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseSpec {
    pub capability: Capability,
    pub scope: ApprovalScope,
    pub expires_at: Option<DateTime<Utc>>,
    /// The fingerprint of the one action this lease covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_digest: Option<String>,
    /// The work the action belonged to, and its revision at grant time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_revision: Option<u64>,
    /// The risk the action carried when the person answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskLevel>,
}

impl LeaseSpec {
    /// A lease for one action, as a grant of `scope` should leave it.
    ///
    /// One constructor, so the fields a lease is bound to and the fields
    /// [`Self::matches_action`] compares cannot be written by two call sites
    /// that disagree about which action this is.
    pub fn for_action(
        capability: Capability,
        scope: ApprovalScope,
        expires_at: Option<DateTime<Utc>>,
        action: &ActionFacts,
    ) -> Self {
        Self {
            capability,
            scope,
            expires_at,
            action_digest: Some(action.digest.clone()),
            task_id: Some(action.task_id.clone()),
            task_revision: Some(action.task_revision),
            risk: Some(action.risk),
        }
    }

    /// Whether this lease covers `action`.
    ///
    /// The rule, in descending order of what it protects:
    ///
    /// * The lease must be about a capability the action declares.
    /// * `L3` is never covered by a standing lease. A high-risk action is
    ///   confirmed one call at a time, so the only way for it to run is the
    ///   `resume` path holding a granted `allow_once` for exactly that
    ///   fingerprint — never a turn- or session-wide grant.
    /// * A lease that names a fingerprint covers exactly that fingerprint,
    ///   plus the task revision it was given for (the digest covers both, but
    ///   comparing them too makes a mismatched record say *why*).
    /// * A lease that names no fingerprint is a pre-B4 capability-wide grant.
    ///   It still covers `L0`/`L1`, where re-asking for every read would make
    ///   the old grants worthless — and covers nothing above that.
    /// * A lease cannot cover an action riskier than the one it was given for.
    pub fn matches_action(&self, action: &ActionFacts) -> bool {
        if !action.capabilities.contains(&self.capability) {
            return false;
        }
        if action.risk >= RiskLevel::L3 {
            return false;
        }
        if self.risk.is_some_and(|risk| risk < action.risk) {
            return false;
        }
        match &self.action_digest {
            Some(digest) => {
                digest == &action.digest
                    && self.task_id.as_deref().is_none_or(|id| id == action.task_id)
                    && self.task_revision.is_none_or(|revision| revision == action.task_revision)
            }
            None => action.risk <= RiskLevel::L1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn policy_decision_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&PolicyDecision::Allow).unwrap(), "\"allow\"");
        assert_eq!(serde_json::to_string(&PolicyDecision::Ask).unwrap(), "\"ask\"");
        assert_eq!(serde_json::to_string(&PolicyDecision::Deny).unwrap(), "\"deny\"");
    }

    #[test]
    fn a_refusal_layer_keeps_one_word_in_json_and_in_the_log() {
        // Two spellings of one thing — `as_str` for the prose and serde for the
        // payload — would be two vocabularies the moment either drifts. The log
        // carries this word, so the log and the code have to agree on it.
        for layer in [
            RefusalLayer::ManagedPolicy,
            RefusalLayer::Rule,
            RefusalLayer::SandboxUnavailable,
            RefusalLayer::Ungranted,
        ] {
            assert_eq!(
                serde_json::to_string(&layer).unwrap(),
                format!("\"{}\"", layer.as_str()),
                "{layer:?}"
            );
            // Every layer says where the fix belongs; a layer with no sentence
            // would leave the refusal text with a hole in it.
            assert!(!layer.described().is_empty(), "{layer:?}");
        }
    }

    #[test]
    fn capability_is_transparent_string() {
        let c = Capability::new(Capability::FS_READ);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"fs.read\"");
        let decoded: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, c);
    }

    #[test]
    fn approval_scope_and_decision_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ApprovalScope::AllowForTurn).unwrap(),
            "\"allow_for_turn\""
        );
        assert_eq!(serde_json::to_string(&ApprovalDecision::Grant).unwrap(), "\"grant\"");
    }

    #[test]
    fn approval_request_round_trips_through_serde() {
        let req = ApprovalRequest {
            approval_id: ApprovalId::new(),
            session_id: SessionId::new(),
            capability: Capability::new(Capability::PROCESS_SPAWN),
            tool_name: "shell.run".to_string(),
            args_summary: "git push origin main".to_string(),
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
            action_digest: Some("sha256:abc".to_string()),
            risk: Some(RiskLevel::L3),
            task_id: Some("intent-1".to_string()),
            task_revision: Some(2),
            reversibility: Some(Reversibility::Irreversible),
            targets: vec![ActionTarget::Command {
                program: "shell".to_string(),
                args: vec!["git push origin main".to_string()],
                cwd: None,
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: ApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.approval_id, req.approval_id);
        assert_eq!(decoded.capability, req.capability);
        assert_eq!(decoded.status, ApprovalStatus::Pending);
        assert_eq!(decoded.action_digest.as_deref(), Some("sha256:abc"));
        assert_eq!(decoded.risk, Some(RiskLevel::L3));
        assert_eq!(decoded.targets, req.targets);
    }

    #[test]
    fn an_approval_written_before_b4_still_reads() {
        // The fields that bind a request to one action are absent from every
        // log written before they existed; the question itself must still be
        // readable, or an old session cannot show why it is waiting.
        let old = json!({
            "approval_id": "ap-1",
            "session_id": "s-1",
            "capability": "process.spawn",
            "tool_name": "shell.run",
            "args_summary": "ls",
            "status": "pending",
            "created_at": "2026-01-01T00:00:00Z"
        });
        let request: ApprovalRequest = serde_json::from_value(old).expect("an old request reads");
        assert!(request.action_digest.is_none());
        assert!(request.risk.is_none());
        assert!(request.targets.is_empty());
    }

    #[test]
    fn approval_reply_round_trips_through_serde() {
        let reply = ApprovalReply {
            approval_id: ApprovalId::new(),
            decision: ApprovalDecision::Grant,
            scope: ApprovalScope::AllowOnce,
            actor: "user".to_string(),
            created_at: Utc::now(),
            action_digest: Some("sha256:abc".to_string()),
            risk: Some(RiskLevel::L2),
            authority: Some("interactive_tty".to_string()),
        };
        let json = serde_json::to_string(&reply).unwrap();
        let decoded: ApprovalReply = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.decision, ApprovalDecision::Grant);
        assert_eq!(decoded.scope, ApprovalScope::AllowOnce);
        assert_eq!(decoded.action_digest.as_deref(), Some("sha256:abc"));
        assert_eq!(decoded.risk, Some(RiskLevel::L2));
    }

    #[test]
    fn secret_ref_and_artifact_ref_round_trip() {
        let secret = SecretRef { source: "env".to_string(), name: "OPENAI_API_KEY".to_string() };
        let artifact = ArtifactRef {
            hash: "abc123".to_string(),
            mime_type: "text/plain".to_string(),
            size: 42,
        };

        let s: SecretRef = serde_json::from_str(&serde_json::to_string(&secret).unwrap()).unwrap();
        let a: ArtifactRef =
            serde_json::from_str(&serde_json::to_string(&artifact).unwrap()).unwrap();
        assert_eq!(s, secret);
        assert_eq!(a, artifact);
    }

    #[test]
    fn a_secret_ref_has_one_spelling_in_a_tool_argument() {
        let secret = SecretRef { source: "env".to_string(), name: "TOOL_TOKEN".to_string() };
        assert_eq!(secret.reference(), "secret://env/TOOL_TOKEN");
    }

    #[test]
    fn workspace_and_lease_round_trip() {
        let workspace = Workspace {
            workspace_root: "C:/repo".to_string(),
            writable_roots: vec!["C:/repo/src".to_string()],
            external_directories: vec![],
        };
        let lease = LeaseSpec {
            capability: Capability::new(Capability::FS_WRITE),
            scope: ApprovalScope::AllowOnce,
            expires_at: None,
            action_digest: Some("sha256:abc".to_string()),
            task_id: Some("intent-1".to_string()),
            task_revision: Some(1),
            risk: Some(RiskLevel::L1),
        };
        let w: Workspace =
            serde_json::from_str(&serde_json::to_string(&workspace).unwrap()).unwrap();
        let l: LeaseSpec = serde_json::from_str(&serde_json::to_string(&lease).unwrap()).unwrap();
        assert_eq!(w, workspace);
        assert_eq!(l, lease);
    }

    fn facts(risk: RiskLevel, path: &str) -> ActionFacts {
        facts_at(risk, path, 3)
    }

    fn facts_at(risk: RiskLevel, path: &str, task_revision: u64) -> ActionFacts {
        ActionFacts::from_spec(crate::permission::ActionSpec {
            task_id: "intent-1".to_string(),
            task_revision,
            tool_name: "fs.write".to_string(),
            capabilities: vec![Capability::new(Capability::FS_WRITE)],
            risk,
            reversibility: Reversibility::for_risk(risk),
            targets: vec![ActionTarget::WorkspacePath {
                path: path.to_string(),
                writable_root: None,
            }],
            arguments: json!({ "path": path }),
            summary: "write".to_string(),
        })
    }

    #[test]
    fn a_lease_matches_only_the_action_it_was_given_for() {
        let action = facts(RiskLevel::L2, "a.txt");
        let lease = LeaseSpec::for_action(
            Capability::new(Capability::FS_WRITE),
            ApprovalScope::AllowForTurn,
            None,
            &action,
        );
        assert!(lease.matches_action(&action));

        // Another path is another action, and another revision is another task.
        assert!(!lease.matches_action(&facts(RiskLevel::L2, "b.txt")));
        assert!(
            !lease.matches_action(&facts_at(RiskLevel::L2, "a.txt", 4)),
            "a newer task revision is a different action"
        );
    }

    #[test]
    fn a_lease_never_covers_a_higher_risk_action_than_it_was_given_for() {
        // A classifier can raise an action after the person answered; the
        // raised action has a new fingerprint, and the lease does not follow.
        let action = facts(RiskLevel::L1, "a.txt");
        let lease = LeaseSpec::for_action(
            Capability::new(Capability::FS_WRITE),
            ApprovalScope::AllowForSession,
            None,
            &action,
        );
        assert!(!lease.matches_action(&facts(RiskLevel::L2, "a.txt")));
    }

    #[test]
    fn no_standing_lease_covers_an_l3_action() {
        // Per-action confirmation is not a phrase about the CLI: it is this
        // line. Even a lease whose fingerprint happens to match cannot authorise
        // a high-risk call, so the only route to run one is the resume path
        // holding a granted `allow_once`.
        let action = facts(RiskLevel::L3, "a.txt");
        let lease = LeaseSpec::for_action(
            Capability::new(Capability::FS_WRITE),
            ApprovalScope::AllowForSession,
            None,
            &action,
        );
        assert!(!lease.matches_action(&action));
    }

    #[test]
    fn an_old_capability_only_lease_covers_low_risk_and_nothing_above_it() {
        let old = LeaseSpec {
            capability: Capability::new(Capability::FS_WRITE),
            scope: ApprovalScope::AllowForSession,
            expires_at: None,
            action_digest: None,
            task_id: None,
            task_revision: None,
            risk: None,
        };
        assert!(old.matches_action(&facts(RiskLevel::L0, "a.txt")));
        assert!(old.matches_action(&facts(RiskLevel::L1, "a.txt")));
        assert!(!old.matches_action(&facts(RiskLevel::L2, "a.txt")));
        assert!(!old.matches_action(&facts(RiskLevel::L3, "a.txt")));

        // A capability the action does not declare is never covered.
        let mut other = facts(RiskLevel::L1, "a.txt");
        other.capabilities = vec![Capability::new(Capability::FS_READ)];
        assert!(!old.matches_action(&other));
    }

    #[test]
    fn redaction_metadata_defaults_to_no_redaction() {
        let meta = RedactionMetadata::default();
        assert!(!meta.redacted);
        assert!(meta.patterns.is_empty());
        let decoded: RedactionMetadata =
            serde_json::from_value(json!({ "redacted": false, "patterns": [] })).unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn an_artifact_ref_names_its_blob_without_naming_a_path() {
        let artifact =
            ArtifactRef { hash: "abc".to_string(), mime_type: "text/plain".to_string(), size: 42 };

        // The locator has to be usable by a reader that has no idea where the
        // blob store put its files, so it may not be a filesystem path.
        assert_eq!(artifact.locator(), "artifact://sha256/abc");

        let reference = artifact.reference();
        assert_eq!(reference["locator"], "artifact://sha256/abc");
        assert_eq!(reference["bytes"], 42);
        assert_eq!(reference["mime"], "text/plain");

        // The reader's half: what the writer produced is exactly what it reads
        // back. A parser that accepted a different spelling would let a reader
        // ask for a blob the writer never named.
        assert_eq!(ArtifactRef::hash_from_locator(&artifact.locator()), Some("abc"));
    }

    #[test]
    fn a_locator_names_nothing_under_another_scheme_or_without_a_hash() {
        assert_eq!(ArtifactRef::hash_from_locator("artifact://sha256/"), None);
        assert_eq!(ArtifactRef::hash_from_locator("artifact://md5/abc"), None);
        assert_eq!(ArtifactRef::hash_from_locator("abc"), None);
        assert_eq!(ArtifactRef::hash_from_locator(""), None);
    }

    fn caps(names: &[&str]) -> Vec<Capability> {
        names.iter().map(|n| Capability::new(*n)).collect()
    }

    #[test]
    fn a_capability_set_names_what_is_missing_from_a_request() {
        let authorized: CapabilitySet =
            caps(&[Capability::FS_READ, Capability::GIT_STATUS]).into_iter().collect();

        assert!(authorized.missing_from(&caps(&[Capability::FS_READ])).is_empty());
        assert_eq!(
            authorized.missing_from(&caps(&[
                Capability::FS_READ,
                Capability::PROCESS_SPAWN,
                Capability::GIT_PUSH,
            ])),
            vec![
                &Capability::new(Capability::PROCESS_SPAWN),
                &Capability::new(Capability::GIT_PUSH)
            ]
        );
    }

    #[test]
    fn the_empty_set_authorizes_nothing() {
        let authorized = CapabilitySet::new();
        assert!(authorized.is_empty());
        assert_eq!(authorized.len(), 0);
        assert_eq!(authorized.missing_from(&caps(&[Capability::FS_READ])).len(), 1);
    }

    #[test]
    fn a_capability_set_serializes_in_a_stable_order() {
        // Built in one order, written in another: the payload has to be the
        // same bytes either way, or two runs of the same config disagree.
        let one: CapabilitySet =
            caps(&[Capability::FS_WRITE, Capability::FS_READ]).into_iter().collect();
        let other: CapabilitySet =
            caps(&[Capability::FS_READ, Capability::FS_WRITE]).into_iter().collect();

        assert_eq!(one, other);
        assert_eq!(serde_json::to_string(&one).unwrap(), serde_json::to_string(&other).unwrap());

        let decoded: CapabilitySet =
            serde_json::from_str(&serde_json::to_string(&one).unwrap()).unwrap();
        assert_eq!(decoded, one);
    }
}
