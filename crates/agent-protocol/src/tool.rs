//! Tool protocol: definitions, invocations, and results.
//!
//! These are the stable DTOs the kernel, loop, and adapters exchange. A model
//! proposes a tool call; the loop turns it into a `ToolInvocation`; the gateway
//! decides under policy and approval and states its decision on the result it
//! returns ([`ToolPolicyRecord`]); the adapter's large output is offloaded as
//! `artifacts`.
//!
//! The decision is on the **result**, not on the invocation: the invocation is
//! the question and the caller builds it, while the answer is the gate's. A
//! field on the request would be one the caller could fill with a decision
//! nobody made.

use crate::permissions::{
    ArtifactRef, Capability, PolicyDecision, RedactionMetadata, RefusalLayer,
};
use serde::{Deserialize, Serialize};

/// How much risk a tool carries, used by policy and replay/recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSideEffect {
    Pure,
    Read,
    Write,
    Network,
    Destructive,
}

/// Where a tool definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Builtin,
    Plugin,
    Mcp,
}

/// Whether a tool run reached a terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Completed,
    Failed,
    Cancelled,
}

/// The `outcome` field a `tool/failed` carries when the call did not run
/// because it is waiting on an approval, rather than because it failed.
///
/// Three parties have to agree on this word: the loop writes it, the
/// model-context projection must not read it as an answer (nothing was
/// executed, so the call stays open), and `resume` is what answers it once a
/// decision exists.
pub const APPROVAL_PENDING_OUTCOME: &str = "approval_pending";

/// The `outcome` field a `tool/failed` carries when an approval settled
/// negatively — denied, expired or cancelled. The call did not run, so it is
/// not an observation about the world and must not be read as a failure the
/// judgment gate holds the turn on.
pub const APPROVAL_DENIED_OUTCOME: &str = "approval_denied";

/// The `outcome` field a `tool/failed` carries when a kernel gate held the
/// step before any of its calls ran.
///
/// The call did not run and did not fail: the loop refused to dispatch the
/// batch until the model answered the check (record a branch, or stop replaying
/// the same call). It gets a result anyway because every tool call in a
/// provider response needs one; the text is the gate's instruction, and the
/// judgment gate must not read it as an observation about the world.
pub const GATE_REFUSED_OUTCOME: &str = "gate_refused";

/// The `outcome` field a `tool/failed` carries when the gateway refused the
/// call before it ran (unknown tool, policy, resource).
///
/// The distinction is what the judgment gate keys on: a refusal is a decision
/// with actionable text, while a tool that ran and reported failure is an
/// observation about the world — the class that ends in a silent stop.
pub const GATEWAY_REFUSED_OUTCOME: &str = "gateway_refused";

/// Why a tool call did not produce what it was asked for.
///
/// One vocabulary for both sides of the gateway seam. A provider says what went
/// wrong with a call it was handed; the gateway says why a call never reached
/// one. Both are written into the same `tool/failed` payload as `error_kind`,
/// so a reader counting failures by class — the audit this came from, a
/// metrics command, a person reading one turn — does not have to know which
/// side answered in order to add them up.
///
/// `Failed` is not the absence of a class: it is the one only a tool can give,
/// "I ran, and the answer is no". Everything else here says the call did not
/// get that far, and each names a different place to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    /// The arguments do not match the tool's declared `input_schema`.
    InvalidArguments,
    /// Policy or the file boundary refused the call; nothing ran.
    PermissionDenied,
    /// The name is not on this run's surface.
    UnknownTool,
    /// The call ran past its deadline.
    Timeout,
    /// Whatever stands behind the tool could not be reached.
    Unavailable,
    /// The call was cut off before it could answer.
    Cancelled,
    /// The far side answered in a shape this build cannot read — including a
    /// tool whose own declaration cannot be enforced.
    Protocol,
    /// The tool ran and answered that it could not do the thing.
    Failed,
}

impl ToolErrorKind {
    /// The word the log keeps. Stable: it is read by people and by anything
    /// that counts these.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::PermissionDenied => "permission_denied",
            Self::UnknownTool => "unknown_tool",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Cancelled => "cancelled",
            Self::Protocol => "protocol",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ToolErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A tool's declared metadata, used by policy and manifest generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    /// Model-facing prose. It is the only field a model reasons about when
    /// choosing a tool, so an empty one is a real defect, not a nicety.
    pub description: String,
    pub source: ToolSource,
    pub side_effect: ToolSideEffect,
    pub capability_requirements: Vec<Capability>,
    /// Whether the tool is safe to retry (idempotent).
    pub idempotent: bool,
    pub timeout_ms: Option<u64>,
    pub input_schema: serde_json::Value,
}

/// A request to run a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    /// The model's `ToolUse` id this invocation came from. `None` for
    /// invocations started outside a model response, such as `agent tool`.
    pub provider_tool_call_id: Option<String>,
}

impl ToolInvocation {
    /// An invocation proposed without a model-originated call id.
    pub fn proposed(tool_name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self { tool_name: tool_name.into(), arguments, provider_tool_call_id: None }
    }

    /// An invocation derived from a model's `ToolUse` block. Keeping the call id
    /// here is what lets a replay rebuild a valid tool-role message later.
    pub fn from_tool_call(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments,
            provider_tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// What the policy gate decided about one call, and — when it said no — which
/// layer said it.
///
/// Three parties read this. A person reading a session wants to know why a call
/// ran or did not, in a word rather than in prose. The judgment gate needs to
/// tell "the world said no" from "the gate said no". And anything that counts
/// refusals by layer — `agent metrics` today, whatever folds the log next —
/// can only do it if the layer is a value.
///
/// The prose (`reason` on a refusal) stays beside it: the layer says where the
/// fix belongs, the sentence says what was refused. One is for counting, the
/// other for reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicyRecord {
    pub decision: PolicyDecision,
    /// Every capability the call's tool declares, in declaration order.
    ///
    /// Empty for a tool that declares nothing — which is itself the reason a
    /// call like that is refused, so an empty list is a finding and not a gap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    /// The capabilities that were refused, and the layer that refused each.
    ///
    /// Sorted by capability rather than by request order: this is a set of
    /// refusals, and two records made from the same decision have to be equal.
    /// Empty exactly when nothing was refused — a `deny` of a call that
    /// declares no capabilities has nothing to name here.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub refusals: std::collections::BTreeMap<Capability, RefusalLayer>,
}

impl ToolPolicyRecord {
    /// The gate let the call through under these capabilities.
    pub fn allowed(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            decision: PolicyDecision::Allow,
            capabilities: capabilities.into_iter().collect(),
            refusals: Default::default(),
        }
    }

    /// The gate refused the call.
    pub fn denied(
        capabilities: impl IntoIterator<Item = Capability>,
        refusals: impl IntoIterator<Item = (Capability, RefusalLayer)>,
    ) -> Self {
        Self {
            decision: PolicyDecision::Deny,
            capabilities: capabilities.into_iter().collect(),
            refusals: refusals.into_iter().collect(),
        }
    }
}

/// Facts about a result that are cheaper to carry than to parse out of its text.
///
/// The model does not see these: the request keeps exactly the tool's own
/// words, so nothing here changes what is sent. They are recorded beside the
/// result so later readers — the step ledger, and the turn-boundary trimming
/// that compares results — can describe and group results without re-parsing
/// prose that only happens to look structured. A provider that knows nothing
/// about a fact leaves it `None`; a fact is never guessed from the text.
///
/// The locator of an offloaded result is not here: `ToolResult::artifacts`
/// already carries it as a value. Repeating it would make two copies that can
/// disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFacts {
    /// The exit status of a process the tool ran. `Some(0)` is recorded too:
    /// "the command ran and said yes" is a fact a reader needs, not only the
    /// failure codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Lines in the full result, before any preview replaced part of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_count: Option<u64>,
}

impl ToolFacts {
    /// Whether nothing is known. An empty set is omitted from payloads, so a
    /// result with no facts serializes exactly as it did before this type
    /// existed.
    pub fn is_empty(&self) -> bool {
        self.exit_code.is_none() && self.line_count.is_none()
    }
}

/// A tool run's result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: ToolStatus,
    /// Small result text that can enter the model context directly.
    pub content: String,
    /// Large results offloaded to the ArtifactStore.
    pub artifacts: Vec<ArtifactRef>,
    /// Whether output reached the framework and did not reach the model — cut
    /// by the execution environment's size cap, or a content block the adapter
    /// had no way to carry (an MCP image or audio payload, say) while the rest
    /// of the result went through.
    ///
    /// This is not the other fact an artifact states. `truncated` says output
    /// was lost before the model saw it; `artifacts` says what was large or had
    /// no text form was *kept* and named by a locator. A result may be either,
    /// both, or neither, and the two must not be read as one — which is why
    /// offloading a block clears this flag rather than setting it.
    #[serde(default)]
    pub truncated: bool,
    /// Structured facts beside the text, for readers that must not parse it.
    #[serde(default, skip_serializing_if = "ToolFacts::is_empty")]
    pub facts: ToolFacts,
    /// What the gate decided about this call.
    ///
    /// Set by the gateway, which is the only party that made the decision. A
    /// provider that fills it in is stating something it did not decide, so the
    /// gateway clears the field on the way in and stamps its own answer after.
    /// `None` means this path states no decision — a resumed call whose decision
    /// is recorded in the approval events, or a gateway that enforces nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ToolPolicyRecord>,
    pub redaction_metadata: RedactionMetadata,
    pub duration_ms: Option<u64>,
}

impl ToolResult {
    /// A successful result with no artifacts and no redaction.
    pub fn completed(content: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Completed,
            content: content.into(),
            artifacts: Vec::new(),
            truncated: false,
            facts: ToolFacts::default(),
            policy: None,
            redaction_metadata: RedactionMetadata::default(),
            duration_ms: None,
        }
    }

    /// A failed result with no artifacts and no redaction.
    pub fn failed(content: impl Into<String>) -> Self {
        Self { status: ToolStatus::Failed, ..Self::completed(content) }
    }

    /// Mark that the underlying output was cut at the source.
    pub fn truncated(mut self) -> Self {
        self.truncated = true;
        self
    }

    /// Record the exit status of the process this result came from.
    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.facts.exit_code = Some(exit_code);
        self
    }

    /// Record how many lines the full result had before any preview.
    pub fn with_line_count(mut self, line_count: u64) -> Self {
        self.facts.line_count = Some(line_count);
        self
    }

    /// State what the gate decided. Called by the gateway, and by nothing else.
    pub fn with_policy(mut self, policy: ToolPolicyRecord) -> Self {
        self.policy = Some(policy);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn side_effect_and_status_serialize_snake_case() {
        assert_eq!(serde_json::to_string(&ToolSideEffect::Write).unwrap(), "\"write\"");
        assert_eq!(serde_json::to_string(&ToolStatus::Cancelled).unwrap(), "\"cancelled\"");
        assert_eq!(serde_json::to_string(&ToolSource::Builtin).unwrap(), "\"builtin\"");
    }

    #[test]
    fn an_error_kind_keeps_one_word_in_json_and_in_the_log() {
        // The serialized word and `as_str` are two spellings of one thing, and
        // the log, a metrics count and a config file all read it. Two spellings
        // that could drift are two vocabularies.
        for kind in [
            ToolErrorKind::InvalidArguments,
            ToolErrorKind::PermissionDenied,
            ToolErrorKind::UnknownTool,
            ToolErrorKind::Timeout,
            ToolErrorKind::Unavailable,
            ToolErrorKind::Cancelled,
            ToolErrorKind::Protocol,
            ToolErrorKind::Failed,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn tool_definition_round_trips_through_serde() {
        let def = ToolDefinition {
            name: "fs.read".to_string(),
            description: "Read a text file".to_string(),
            source: ToolSource::Builtin,
            side_effect: ToolSideEffect::Read,
            capability_requirements: vec![Capability::new(Capability::FS_READ)],
            idempotent: true,
            timeout_ms: Some(1000),
            input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        };
        let json = serde_json::to_string(&def).unwrap();
        let decoded: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, "fs.read");
        assert_eq!(decoded.description, "Read a text file");
        assert_eq!(decoded.side_effect, ToolSideEffect::Read);
        assert_eq!(decoded.capability_requirements, def.capability_requirements);
        assert!(decoded.idempotent);
    }

    #[test]
    fn tool_invocation_round_trips() {
        let no_id = ToolInvocation::proposed("fs.read", json!({ "path": "src/main.rs" }));
        let decoded: ToolInvocation =
            serde_json::from_str(&serde_json::to_string(&no_id).unwrap()).unwrap();
        assert!(decoded.provider_tool_call_id.is_none());
    }

    /// The decision used to be a field on the invocation, documented as
    /// "attached by the policy pipeline before execution" — and nothing ever
    /// attached it. A request carrying a decision is a request the caller can
    /// fill with one nobody made; it is the gate's answer, and it belongs on
    /// the result the gate returns.
    #[test]
    fn a_request_carries_no_decision_only_the_case_for_one() {
        let inv = ToolInvocation::proposed("shell.run", json!({ "command": "ls" }));
        let value = serde_json::to_value(&inv).unwrap();
        assert!(value.get("policy_decision").is_none(), "the question is not the answer: {value}");
        assert!(value.get("policy").is_none(), "{value}");
    }

    #[test]
    fn model_originated_invocation_keeps_the_provider_call_id() {
        let inv = ToolInvocation::from_tool_call("call_1", "fs.read", json!({ "path": "a.txt" }));
        assert_eq!(inv.provider_tool_call_id.as_deref(), Some("call_1"));

        let decoded: ToolInvocation =
            serde_json::from_str(&serde_json::to_string(&inv).unwrap()).unwrap();
        assert_eq!(decoded.provider_tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn a_policy_record_states_what_was_decided_and_who_decided_it() {
        let allowed = ToolPolicyRecord::allowed([Capability::new(Capability::FS_READ)]);
        assert_eq!(
            serde_json::to_value(&allowed).unwrap(),
            json!({ "decision": "allow", "capabilities": ["fs.read"] }),
            "a refusal set that is empty adds no key"
        );

        let denied = ToolPolicyRecord::denied(
            [Capability::new(Capability::PROCESS_SPAWN)],
            [(Capability::new(Capability::PROCESS_SPAWN), RefusalLayer::SandboxUnavailable)],
        );
        let value = serde_json::to_value(&denied).unwrap();
        assert_eq!(
            value,
            json!({
                "decision": "deny",
                "capabilities": ["process.spawn"],
                "refusals": { "process.spawn": "sandbox_unavailable" },
            })
        );
        // The layer survives a round trip as the same layer, not as a string
        // someone has to parse back.
        let decoded: ToolPolicyRecord = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, denied);
    }

    #[test]
    fn a_result_states_no_policy_unless_the_gate_states_one() {
        let plain = ToolResult::completed("ok");
        assert!(plain.policy.is_none());
        assert!(serde_json::to_value(&plain).unwrap().get("policy").is_none());

        let stamped = ToolResult::completed("ok")
            .with_policy(ToolPolicyRecord::allowed([Capability::new(Capability::FS_READ)]));
        let value = serde_json::to_value(&stamped).unwrap();
        assert_eq!(value["policy"]["decision"], json!("allow"));
        let decoded: ToolResult = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.policy, stamped.policy);
    }

    #[test]
    fn a_result_written_before_the_field_existed_reads_as_no_decision() {
        // Not "allow": a record with nothing in it cannot be read as a decision
        // anybody made, and guessing one would attribute a decision to a gate
        // that never spoke.
        let older: ToolResult = serde_json::from_value(json!({
            "status": "completed",
            "content": "hello",
            "artifacts": [],
            "redaction_metadata": { "redacted": false, "patterns": [] },
            "duration_ms": null,
        }))
        .unwrap();
        assert!(older.policy.is_none());
    }

    #[test]
    fn tool_result_round_trips_with_artifacts() {
        let result = ToolResult {
            status: ToolStatus::Completed,
            content: "hello".to_string(),
            artifacts: vec![ArtifactRef {
                hash: "abc".to_string(),
                mime_type: "text/plain".to_string(),
                size: 5,
            }],
            truncated: false,
            facts: ToolFacts::default(),
            policy: None,
            redaction_metadata: RedactionMetadata {
                redacted: true,
                patterns: vec!["token".to_string()],
                warnings: Vec::new(),
            },
            duration_ms: Some(12),
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.status, ToolStatus::Completed);
        assert_eq!(decoded.content, "hello");
        assert_eq!(decoded.artifacts.len(), 1);
        assert!(decoded.redaction_metadata.redacted);
    }

    #[test]
    fn helper_constructors_set_status_and_defaults() {
        let ok = ToolResult::completed("done");
        assert_eq!(ok.status, ToolStatus::Completed);
        assert!(ok.artifacts.is_empty());
        assert!(!ok.truncated);
        assert!(!ok.redaction_metadata.redacted);

        let err = ToolResult::failed("boom");
        assert_eq!(err.status, ToolStatus::Failed);
    }

    #[test]
    fn truncation_is_opt_in_and_absent_results_read_as_untruncated() {
        assert!(ToolResult::completed("cut").truncated().truncated);

        // A result written before the flag existed must not read as truncated:
        // "we lost output" is not a thing to infer from a missing field.
        let older: ToolResult = serde_json::from_value(json!({
            "status": "completed",
            "content": "hello",
            "artifacts": [],
            "redaction_metadata": { "redacted": false, "patterns": [] },
            "duration_ms": null,
        }))
        .unwrap();
        assert!(!older.truncated);
    }

    #[test]
    fn facts_are_omitted_when_empty_and_round_trip_when_set() {
        let plain = ToolResult::completed("ok");
        assert!(plain.facts.is_empty());
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value.get("facts").is_none(), "an empty fact set must not add a key");

        let result = ToolResult::completed("ok").with_exit_code(3).with_line_count(12);
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["facts"], json!({ "exit_code": 3, "line_count": 12 }));

        let decoded: ToolResult = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.facts.exit_code, Some(3));
        assert_eq!(decoded.facts.line_count, Some(12));
    }

    #[test]
    fn a_result_without_facts_reads_as_unknown() {
        // A result written before the field existed says nothing about its
        // exit status or size; both must read as unknown, not as zero.
        let older: ToolResult = serde_json::from_value(json!({
            "status": "completed",
            "content": "hello",
            "artifacts": [],
            "redaction_metadata": { "redacted": false, "patterns": [] },
            "duration_ms": null,
        }))
        .unwrap();
        assert!(older.facts.is_empty());
    }
}
