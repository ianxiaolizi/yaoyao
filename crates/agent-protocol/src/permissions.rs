//! Permission, approval, workspace, lease, and secret types shared across the
//! kernel, adapters, and durable events.
//!
//! Everything here is a serializable value or reference — never a live handle.
//! Secrets are referenced by name, not carried as plaintext; leases carry
//! references and metadata, never process handles or credentials.

use std::collections::BTreeSet;

use crate::ids::{ApprovalId, SessionId};
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
}

/// The user's reply to a pending approval — decision, scope, actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalReply {
    pub approval_id: ApprovalId,
    pub decision: ApprovalDecision,
    pub scope: ApprovalScope,
    pub actor: String,
    pub created_at: DateTime<Utc>,
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
    /// The redaction patterns applied (e.g. `api_key`, `token`), not the values.
    pub patterns: Vec<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseSpec {
    pub capability: Capability,
    pub scope: ApprovalScope,
    pub expires_at: Option<DateTime<Utc>>,
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
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: ApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.approval_id, req.approval_id);
        assert_eq!(decoded.capability, req.capability);
        assert_eq!(decoded.status, ApprovalStatus::Pending);
    }

    #[test]
    fn approval_reply_round_trips_through_serde() {
        let reply = ApprovalReply {
            approval_id: ApprovalId::new(),
            decision: ApprovalDecision::Grant,
            scope: ApprovalScope::AllowOnce,
            actor: "user".to_string(),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&reply).unwrap();
        let decoded: ApprovalReply = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.decision, ApprovalDecision::Grant);
        assert_eq!(decoded.scope, ApprovalScope::AllowOnce);
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
        };
        let w: Workspace =
            serde_json::from_str(&serde_json::to_string(&workspace).unwrap()).unwrap();
        let l: LeaseSpec = serde_json::from_str(&serde_json::to_string(&lease).unwrap()).unwrap();
        assert_eq!(w, workspace);
        assert_eq!(l, lease);
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
