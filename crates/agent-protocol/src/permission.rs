//! Action facts: what one tool call would do, in a form two decisions can
//! compare.
//!
//! A permission decision is about an *action*, not about a capability. "The
//! user allowed `process.spawn`" says nothing about which program will run, and
//! a decision reused for a call with a different argument is a decision nobody
//! made. This module is the value that makes the difference sayable: the tool,
//! the capabilities it declares, the risk it carries, whether it can be undone,
//! the concrete targets (a path, a command line, a network origin, a secret
//! reference), and a digest over exactly those stable fields.
//!
//! The digest is the identity an approval is bound to (CONTRACT §7 #175). Two
//! calls with the same digest are the same action; any change to a target, a
//! capability, the tool, the risk, the reversibility or the task revision is a
//! different action and has to be asked about again. Display-only text
//! (`summary`) and wall-clock time are deliberately outside the digest: a
//! sentence that spells the same action differently must not, and a timestamp
//! never identifies an action.
//!
//! Risk is a kernel signal, not a user-facing permission tier. A provider
//! declares the floor it knows for its own tool; the kernel may only raise it
//! (an undeclared tool is `L3`). A classifier's suggestion is advice and can
//! likewise only raise — lowering happens nowhere.
//!
//! This module is data and pure functions only: no filesystem, no network, no
//! clock.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::permissions::Capability;
use crate::tool::ToolSideEffect;

/// Identifies the digest's input shape.
///
/// Bump it whenever the material changes, so a digest written by an older build
/// is never compared against one written by a newer one as if they meant the
/// same thing.
pub const ACTION_DIGEST_VERSION: &str = "action/1";

/// The work one authorization decision belongs to.
///
/// `task_id` is the intent that asked for the work, or `manual:<session>` for a
/// call made outside a turn. `task_revision` is stable for the same intent and
/// changes when the work behind it changes; a lease bound to one revision never
/// covers another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationSubject {
    pub task_id: String,
    pub task_revision: u64,
}

impl AuthorizationSubject {
    pub fn new(task_id: impl Into<String>, task_revision: u64) -> Self {
        Self { task_id: task_id.into(), task_revision }
    }

    /// The subject for a call made outside a task turn.
    pub fn manual(session_id: &str) -> Self {
        Self::new(format!("manual:{session_id}"), 0)
    }
}

/// How much risk one action carries. Internal to the kernel and recorded in the
/// log; it is not a permission the user is asked to understand.
///
/// * `L0` — read-only: workspace reads, read-only checks, read-only HTTP.
/// * `L1` — recoverable write inside the task workspace.
/// * `L2` — external side effect: network writes, dependency installation,
///   system state a later action could compensate for.
/// * `L3` — high-risk and irreversible: deletion, credentials, privilege
///   changes, exfiltration, persistent system changes — and anything no
///   provider classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    L0,
    L1,
    L2,
    L3,
}

impl RiskLevel {
    /// The word the log keeps.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L0 => "l0",
            Self::L1 => "l1",
            Self::L2 => "l2",
            Self::L3 => "l3",
        }
    }

    /// The floor an unclassified tool starts at.
    pub const UNKNOWN: RiskLevel = RiskLevel::L3;

    /// The floor a side-effect class implies when the provider states no risk.
    ///
    /// This is a kernel rule, not a provider's claim: `Read` and `Pure` touch
    /// nothing, `Write` changes the workspace recoverably, `Network` reaches
    /// out, and `Destructive` can delete what it damages. A provider that
    /// knows its own tool better says so through `ToolProvider::risk`.
    pub fn floor_for(side_effect: ToolSideEffect) -> Self {
        match side_effect {
            ToolSideEffect::Pure | ToolSideEffect::Read => Self::L0,
            ToolSideEffect::Write => Self::L1,
            ToolSideEffect::Network => Self::L2,
            ToolSideEffect::Destructive => Self::L3,
        }
    }

    /// Answer with the higher of the two: an estimate may raise the risk of an
    /// action, never lower it.
    pub fn raised_to(self, other: Self) -> Self {
        self.max(other)
    }
}

/// Whether an action can be taken back.
///
/// Recorded beside the risk so an approval card can say it, and so a decision
/// that promised a recoverable point cannot later be read as one that did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    /// The action leaves nothing that a later action has to undo.
    Reversible,
    /// The effect stands, but a compensating action exists.
    Compensatable,
    /// Nothing can take the effect back.
    Irreversible,
}

impl Reversibility {
    /// The class a risk level implies when nothing more specific is known.
    pub fn for_risk(risk: RiskLevel) -> Self {
        match risk {
            RiskLevel::L0 | RiskLevel::L1 => Self::Reversible,
            RiskLevel::L2 => Self::Compensatable,
            RiskLevel::L3 => Self::Irreversible,
        }
    }
}

/// One concrete object an action would touch.
///
/// These are the values a decision is bound to: the permission a call ran under
/// names the path, command line or origin it actually had, and a later call
/// that names a different one is a different action. `Secret` carries the
/// canonical reference only — never a resolved value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionTarget {
    /// A path a file tool would read or write, and the writable root it is
    /// expected to sit under (`None` when this build cannot name one).
    WorkspacePath { path: String, writable_root: Option<String> },
    /// A program a tool would run.
    ///
    /// A tool that takes a whole command line rather than a program and an
    /// argument vector (this build's `shell.run`) records the host shell as the
    /// program and the line as its one argument: the line is what a person
    /// approved and what has to change for the digest to change.
    Command { program: String, args: Vec<String>, cwd: Option<String> },
    /// An HTTP request's destination and method.
    Network { scheme: String, host: String, port: u16, method: String },
    /// A named secret the call would use. The reference is `secret://<source>/<name>`;
    /// the value is never part of an action fact.
    Secret { reference: String },
    /// A persistent data store a tool would change (the memory store, today).
    DataStore { name: String },
    /// A target this build cannot describe more precisely — the tool's own name
    /// is all that is stably known.
    Other { tool: String },
}

impl ActionTarget {
    /// A stable rendering used to sort targets and to hash them.
    ///
    /// Serialization is canonical (`serde_json::Map` is ordered), so this is a
    /// function of the target's value and not of how it was built.
    fn canonical(&self) -> String {
        serde_json::to_string(self).expect("an action target serializes without error")
    }
}

/// One tool call as permission policy sees it.
///
/// Built once per call, before any decision is recorded; `digest` is the stable
/// identity a lease or an approval is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionFacts {
    /// The work the call belongs to. An intent id, or `manual:<session>` for a
    /// call made outside a turn.
    pub task_id: String,
    /// How many changes that work has had. A lease bound to revision `n` does
    /// not cover revision `n+1`.
    pub task_revision: u64,
    pub tool_name: String,
    /// Every capability the call's tool declares, sorted and deduplicated.
    pub capabilities: Vec<Capability>,
    pub risk: RiskLevel,
    pub reversibility: Reversibility,
    /// The concrete targets, sorted by their canonical form.
    pub targets: Vec<ActionTarget>,
    /// SHA-256 of the canonical argument JSON. The arguments themselves stay
    /// out of the facts so no value a caller wrote can leak into a record.
    pub arguments_sha256: String,
    /// Display-only text for a person deciding. Deliberately outside the digest.
    pub summary: String,
    /// SHA-256 over the stable fields above (see [`ACTION_DIGEST_VERSION`]).
    pub digest: String,
}

/// The inputs one action is built from.
///
/// A struct rather than a long argument list, and the only public constructor:
/// the digest has one producer, so two call sites cannot disagree about which
/// fields it covers.
#[derive(Debug, Clone)]
pub struct ActionSpec {
    pub task_id: String,
    pub task_revision: u64,
    pub tool_name: String,
    pub capabilities: Vec<Capability>,
    pub risk: RiskLevel,
    pub reversibility: Reversibility,
    pub targets: Vec<ActionTarget>,
    pub arguments: serde_json::Value,
    pub summary: String,
}

impl ActionFacts {
    /// Build the facts (and their digest) from one action's description.
    pub fn from_spec(spec: ActionSpec) -> Self {
        let ActionSpec {
            task_id,
            task_revision,
            tool_name,
            capabilities,
            risk,
            reversibility,
            targets,
            arguments,
            summary,
        } = spec;

        let mut capabilities = capabilities;
        capabilities.sort();
        capabilities.dedup();
        let mut targets = targets;
        targets.sort_by_key(ActionTarget::canonical);
        targets.dedup();

        let arguments_sha256 = sha256_hex(&canonical_json(&arguments));
        let digest = sha256_hex(&digest_material(
            &task_id,
            task_revision,
            &tool_name,
            &capabilities,
            risk,
            reversibility,
            &targets,
            &arguments_sha256,
        ));

        Self {
            task_id,
            task_revision,
            tool_name,
            capabilities,
            risk,
            reversibility,
            targets,
            arguments_sha256,
            summary,
            digest,
        }
    }

    /// Answer with `risk` if it is higher than the recorded one, otherwise keep
    /// what was recorded.
    ///
    /// The one way a later estimate (a classifier's suggestion, a kernel rule)
    /// can touch risk: raising it re-computes the digest, lowering is not
    /// expressible.
    pub fn raised_to(&self, risk: RiskLevel) -> Self {
        if risk <= self.risk {
            return self.clone();
        }
        let mut raised = self.clone();
        raised.risk = risk;
        raised.reversibility = Reversibility::for_risk(risk);
        raised.digest = sha256_hex(&digest_material(
            &raised.task_id,
            raised.task_revision,
            &raised.tool_name,
            &raised.capabilities,
            raised.risk,
            raised.reversibility,
            &raised.targets,
            &raised.arguments_sha256,
        ));
        raised
    }

    /// The risk at which this action has to be treated.
    pub fn requires_high_risk_isolation(&self) -> bool {
        self.risk >= RiskLevel::L2
    }

    /// Whether this action needs a backend that confines it from the host.
    ///
    /// Two things put an action's effects outside what the run can account for:
    /// it starts a process, whose effects are whatever the program does, or it
    /// writes a path this build cannot place under a writable root — a write the
    /// workspace boundary has not vouched for. Either at `L2` or above is an
    /// action that has to run confined, and a backend that cannot confine it has
    /// to refuse it rather than run it as the invoking user. Below `L2` neither
    /// applies: a recoverable write inside the task workspace is what the
    /// workspace boundary exists for.
    ///
    /// This build fills `writable_root` on no target, so every declared path
    /// counts as one it cannot place. There is no `L2` path action today; a tool
    /// that adds one meets the conservative answer rather than the permissive
    /// one, and a later layer that knows the roots can narrow this predicate
    /// without changing what the answer means.
    pub fn requires_isolated_execution(&self) -> bool {
        self.requires_high_risk_isolation()
            && self.targets.iter().any(|target| {
                matches!(target, ActionTarget::Command { .. } | ActionTarget::WorkspacePath { .. })
            })
    }
}

/// The bytes the action digest covers, in one place.
#[allow(clippy::too_many_arguments)]
fn digest_material(
    task_id: &str,
    task_revision: u64,
    tool_name: &str,
    capabilities: &[Capability],
    risk: RiskLevel,
    reversibility: Reversibility,
    targets: &[ActionTarget],
    arguments_sha256: &str,
) -> String {
    let material = serde_json::json!({
        "schema": ACTION_DIGEST_VERSION,
        "task_id": task_id,
        "task_revision": task_revision,
        "tool": tool_name,
        "capabilities": capabilities.iter().map(Capability::as_str).collect::<Vec<_>>(),
        "risk": risk.as_str(),
        "reversibility": serde_json::to_value(reversibility).expect("reversibility serializes"),
        "targets": targets.iter().map(ActionTarget::canonical).collect::<Vec<_>>(),
        "arguments_sha256": arguments_sha256,
    });
    canonical_json(&material)
}

/// The canonical serialization of a JSON value.
fn canonical_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("serde_json serializes a Value without error")
}

fn sha256_hex(bytes: &str) -> String {
    format!("{:x}", Sha256::digest(bytes.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(arguments: serde_json::Value) -> ActionSpec {
        ActionSpec {
            task_id: "intent-1".to_string(),
            task_revision: 3,
            tool_name: "fs.write".to_string(),
            capabilities: vec![Capability::new(Capability::FS_WRITE)],
            risk: RiskLevel::L1,
            reversibility: Reversibility::Reversible,
            targets: vec![ActionTarget::WorkspacePath {
                path: "src/main.rs".to_string(),
                writable_root: Some("C:/repo".to_string()),
            }],
            arguments,
            summary: "write src/main.rs".to_string(),
        }
    }

    #[test]
    fn an_action_digest_is_stable_across_object_key_order() {
        let one = ActionFacts::from_spec(spec(json!({ "path": "a.txt", "content": "x" })));
        let other = ActionFacts::from_spec(spec(json!({ "content": "x", "path": "a.txt" })));
        assert_eq!(one.digest, other.digest);
        assert_eq!(one.arguments_sha256, other.arguments_sha256);
    }

    #[test]
    fn a_path_change_changes_the_action_digest() {
        let one = ActionFacts::from_spec(spec(json!({ "path": "a.txt" })));
        let mut moved = spec(json!({ "path": "a.txt" }));
        moved.targets = vec![ActionTarget::WorkspacePath {
            path: "b.txt".to_string(),
            writable_root: Some("C:/repo".to_string()),
        }];
        let other = ActionFacts::from_spec(moved);
        assert_ne!(one.digest, other.digest);
    }

    #[test]
    fn a_program_change_changes_the_action_digest() {
        let command = |program: &str| {
            let mut spec = spec(json!({ "command": "cargo test" }));
            spec.targets = vec![ActionTarget::Command {
                program: program.to_string(),
                args: vec!["test".to_string()],
                cwd: None,
            }];
            ActionFacts::from_spec(spec)
        };
        assert_ne!(command("cargo").digest, command("rm").digest);
    }

    #[test]
    fn a_network_origin_change_changes_the_action_digest() {
        let network = |host: &str| {
            let mut spec = spec(json!({ "url": format!("https://{host}/index") }));
            spec.targets = vec![ActionTarget::Network {
                scheme: "https".to_string(),
                host: host.to_string(),
                port: 443,
                method: "GET".to_string(),
            }];
            ActionFacts::from_spec(spec)
        };
        assert_ne!(network("docs.rs").digest, network("example.com").digest);
    }

    #[test]
    fn a_secret_reference_changes_the_digest_but_plaintext_never_enters_it() {
        let with = |reference: &str| {
            let mut spec = spec(json!({ "token": reference }));
            spec.risk = RiskLevel::L3;
            spec.reversibility = Reversibility::Irreversible;
            spec.targets = vec![ActionTarget::Secret { reference: reference.to_string() }];
            ActionFacts::from_spec(spec)
        };
        let one = with("secret://env/TOKEN_A");
        let other = with("secret://env/TOKEN_B");
        assert_ne!(one.digest, other.digest);

        // A value in the arguments is hashed, never carried: the facts a log or
        // an approval card can show hold the reference and nothing else.
        let leaked = spec(json!({ "token": "s3cr3t-plaintext", "path": "a.txt" }));
        let facts = ActionFacts::from_spec(leaked);
        let rendered = format!("{facts:?}");
        assert!(!rendered.contains("s3cr3t-plaintext"), "{rendered}");
        let json = serde_json::to_string(&facts).unwrap();
        assert!(!json.contains("s3cr3t-plaintext"), "{json}");
    }

    #[test]
    fn a_display_summary_does_not_move_the_digest() {
        let mut one = spec(json!({ "path": "a.txt" }));
        let mut other = spec(json!({ "path": "a.txt" }));
        one.summary = "short".to_string();
        other.summary = "a much longer sentence saying the same thing".to_string();
        assert_eq!(ActionFacts::from_spec(one).digest, ActionFacts::from_spec(other).digest);
    }

    #[test]
    fn the_task_revision_is_part_of_the_digest() {
        let one = spec(json!({ "path": "a.txt" }));
        let mut other = spec(json!({ "path": "a.txt" }));
        other.task_revision += 1;
        assert_ne!(ActionFacts::from_spec(one).digest, ActionFacts::from_spec(other).digest);
    }

    #[test]
    fn capability_order_does_not_move_the_digest() {
        let mut one = spec(json!({}));
        let mut other = spec(json!({}));
        one.capabilities =
            vec![Capability::new(Capability::FS_WRITE), Capability::new(Capability::FS_READ)];
        other.capabilities =
            vec![Capability::new(Capability::FS_READ), Capability::new(Capability::FS_WRITE)];
        assert_eq!(ActionFacts::from_spec(one).digest, ActionFacts::from_spec(other).digest);
    }

    #[test]
    fn target_order_does_not_move_the_digest() {
        let target = |path: &str| ActionTarget::WorkspacePath {
            path: path.to_string(),
            writable_root: None,
        };
        let mut one = spec(json!({}));
        let mut other = spec(json!({}));
        one.targets = vec![target("a.txt"), target("b.txt")];
        other.targets = vec![target("b.txt"), target("a.txt")];
        assert_eq!(ActionFacts::from_spec(one).digest, ActionFacts::from_spec(other).digest);
    }

    #[test]
    fn a_risk_suggestion_can_raise_but_never_lower() {
        // L1 action, a classifier says L0: still L1. A classifier says L3: it
        // is raised, and the digest moves with it, because the action it
        // describes is no longer the one that was classified.
        let facts = ActionFacts::from_spec(spec(json!({ "path": "a.txt" })));
        let lowered = facts.raised_to(RiskLevel::L0);
        assert_eq!(lowered.risk, RiskLevel::L1);
        assert_eq!(lowered.digest, facts.digest);

        let raised = facts.raised_to(RiskLevel::L3);
        assert_eq!(raised.risk, RiskLevel::L3);
        assert_eq!(raised.reversibility, Reversibility::Irreversible);
        assert_ne!(raised.digest, facts.digest);
        // Raising an already-risky action is not a second, different action.
        assert_eq!(raised.raised_to(RiskLevel::L2).digest, raised.digest);
    }

    #[test]
    fn only_high_risk_commands_and_workspace_writes_need_an_isolating_backend() {
        // The four cells of the predicate: risk, and whether the action
        // reaches through a process or a path.
        let command = |risk| {
            let mut spec = spec(json!({ "command": "cargo test" }));
            spec.risk = risk;
            spec.reversibility = Reversibility::for_risk(risk);
            spec.targets = vec![ActionTarget::Command {
                program: "shell".to_string(),
                args: vec!["cargo test".to_string()],
                cwd: None,
            }];
            ActionFacts::from_spec(spec)
        };
        let path = |risk| {
            let mut spec = spec(json!({ "path": "a.txt" }));
            spec.risk = risk;
            spec.reversibility = Reversibility::for_risk(risk);
            ActionFacts::from_spec(spec)
        };

        // The shipped write is `L1` inside the workspace: the boundary that
        // bounds it is the workspace boundary, not a sandbox.
        assert!(!path(RiskLevel::L1).requires_isolated_execution());
        assert!(path(RiskLevel::L2).requires_isolated_execution());
        assert!(command(RiskLevel::L2).requires_isolated_execution());
        assert!(command(RiskLevel::L3).requires_isolated_execution());

        // A network read is `L2` and is *not* this predicate's business: what
        // bounds it is the origin allowlist (B6), not a process sandbox.
        let mut network = spec(json!({ "url": "https://docs.rs/index.html" }));
        network.risk = RiskLevel::L2;
        network.reversibility = Reversibility::Compensatable;
        network.targets = vec![ActionTarget::Network {
            scheme: "https".to_string(),
            host: "docs.rs".to_string(),
            port: 443,
            method: "GET".to_string(),
        }];
        assert!(!ActionFacts::from_spec(network).requires_isolated_execution());

        // And neither is a target this build cannot describe at all: that is a
        // surface question for the policy, not an isolation one.
        let mut other = spec(json!({}));
        other.risk = RiskLevel::L3;
        other.reversibility = Reversibility::Irreversible;
        other.targets = vec![ActionTarget::Other { tool: "mystery.tool".to_string() }];
        assert!(!ActionFacts::from_spec(other).requires_isolated_execution());
    }

    #[test]
    fn a_side_effect_class_implies_a_floor_risk() {
        assert_eq!(RiskLevel::floor_for(ToolSideEffect::Pure), RiskLevel::L0);
        assert_eq!(RiskLevel::floor_for(ToolSideEffect::Read), RiskLevel::L0);
        assert_eq!(RiskLevel::floor_for(ToolSideEffect::Write), RiskLevel::L1);
        assert_eq!(RiskLevel::floor_for(ToolSideEffect::Network), RiskLevel::L2);
        assert_eq!(RiskLevel::floor_for(ToolSideEffect::Destructive), RiskLevel::L3);
    }

    #[test]
    fn risk_and_reversibility_keep_one_spelling_in_json() {
        for (risk, word) in [
            (RiskLevel::L0, "\"l0\""),
            (RiskLevel::L1, "\"l1\""),
            (RiskLevel::L2, "\"l2\""),
            (RiskLevel::L3, "\"l3\""),
        ] {
            assert_eq!(serde_json::to_string(&risk).unwrap(), word);
            assert_eq!(serde_json::to_string(&risk).unwrap(), format!("\"{}\"", risk.as_str()));
        }
        assert_eq!(
            serde_json::to_string(&Reversibility::Compensatable).unwrap(),
            "\"compensatable\""
        );
    }

    #[test]
    fn action_facts_round_trip_through_serde() {
        let facts = ActionFacts::from_spec(spec(json!({ "path": "a.txt" })));
        let decoded: ActionFacts =
            serde_json::from_str(&serde_json::to_string(&facts).unwrap()).unwrap();
        assert_eq!(decoded, facts);
        assert_eq!(decoded.digest.len(), 64, "a sha-256 digest is 64 hex characters");
    }
}
