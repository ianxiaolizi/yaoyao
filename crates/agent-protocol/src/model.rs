//! Provider-neutral model protocol.
//!
//! The kernel and the loop only ever see these types; an adapter normalizes
//! vendor payloads into content blocks at the edge. Open sets — capabilities and
//! finish reasons — use the same string-namespace + constant-table pattern as
//! `EventType`, so a value a provider invents never breaks deserialization.

use crate::events::EventType;
use crate::ids::{AttemptId, InboxItemId, MessageUid, RunId, SessionId, StepId, TurnId, fresh_id};
use crate::tool::{ToolDefinition, ToolSource};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A model capability, such as `"streaming"` or `"tool_call"`.
///
/// Providers declare what they support; requests declare what they require. A
/// provider that lacks a required capability must disable the feature or
/// degrade explicitly — never silently reshape the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelCapability(pub String);

impl ModelCapability {
    pub const STREAMING: &'static str = "streaming";
    pub const TOOL_CALL: &'static str = "tool_call";
    pub const PARALLEL_TOOL_CALL: &'static str = "parallel_tool_call";
    pub const REASONING: &'static str = "reasoning";
    pub const IMAGE_INPUT: &'static str = "image_input";
    pub const STRUCTURED_OUTPUT: &'static str = "structured_output";
    pub const PROMPT_CACHE: &'static str = "prompt_cache";
    pub const RESPONSE_CONTINUATION: &'static str = "response_continuation";
    pub const TOKEN_USAGE: &'static str = "token_usage";

    /// Create a capability from a wire string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why the provider stopped generating. An open set: unknown values are
/// preserved verbatim rather than collapsed to a catch-all.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FinishReason(pub String);

impl FinishReason {
    pub const STOP: &'static str = "stop";
    pub const LENGTH: &'static str = "length";
    pub const TOOL_CALLS: &'static str = "tool_calls";
    pub const CONTENT_FILTER: &'static str = "content_filter";
    pub const CANCELLED: &'static str = "cancelled";
    pub const ERROR: &'static str = "error";
    /// The provider's value is not meaningful; used when an attempt ends
    /// without a usable finish reason.
    pub const UNKNOWN: &'static str = "unknown";

    /// Create a finish reason from a wire string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    /// The wire string, matching the OpenAI-compatible role names.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One normalized piece of model-visible content.
///
/// Tool traffic is a content block rather than a separate response field, so
/// there is exactly one durable representation of what the model saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text { text: String },
    /// A tool the model wants to run. It is a proposal, never an execution.
    ToolUse { tool_call_id: String, tool_name: String, arguments: serde_json::Value },
    /// A tool outcome fed back into the conversation.
    ToolResult { tool_call_id: String, tool_name: String, content: String, is_error: bool },
}

impl ContentBlock {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// A tool proposal block.
    pub fn tool_use(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::ToolUse { tool_call_id: tool_call_id.into(), tool_name: tool_name.into(), arguments }
    }

    /// A tool outcome block.
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: content.into(),
            is_error,
        }
    }

    /// The text of a `Text` block, if this is one.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    /// The `(tool_call_id, tool_name, arguments)` of a `ToolUse` block.
    pub fn as_tool_use(&self) -> Option<(&str, &str, &serde_json::Value)> {
        match self {
            Self::ToolUse { tool_call_id, tool_name, arguments } => {
                Some((tool_call_id, tool_name, arguments))
            }
            _ => None,
        }
    }

    /// The `(tool_call_id, tool_name, content, is_error)` of a `ToolResult` block.
    pub fn as_tool_result(&self) -> Option<(&str, &str, &str, bool)> {
        match self {
            Self::ToolResult { tool_call_id, tool_name, content, is_error } => {
                Some((tool_call_id, tool_name, content, *is_error))
            }
            _ => None,
        }
    }
}

/// One message in the request context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMessage {
    /// Stable identity, used by context revision.
    pub message_uid: MessageUid,
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

impl ModelMessage {
    /// A message with a freshly generated uid.
    ///
    /// For messages the loop sends that are *also* rebuilt later from the log,
    /// use the `*_for_*` constructors below instead: their uid is a function of
    /// durable ids, so the sent message and the rebuilt one are the same
    /// message, not two messages with the same content.
    pub fn new(role: MessageRole, content: Vec<ContentBlock>) -> Self {
        Self { message_uid: MessageUid::new(), role, content }
    }

    /// A single-text-block message.
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self::new(role, vec![ContentBlock::text(text)])
    }

    /// The instructions of one run.
    ///
    /// The uid comes from the run, so the message the loop places before
    /// `run/started` exists and the one a fold rebuilds from that event are the
    /// same message rather than two that say the same thing.
    pub fn system_for_run(run_id: &RunId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/system/{run_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A per-request reminder about the context window, for one turn.
    ///
    /// Unlike the messages above this one is not history: the projection never
    /// folds it, and it is placed only in this turn's requests. The uid comes
    /// from the turn so every step of the turn carries the same notice rather
    /// than a new message each step.
    pub fn context_notice_for_turn(turn_id: &TurnId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/notice/{turn_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A kernel note inside a turn: the loop refused to end here.
    ///
    /// It is user-role, not system: several chat templates (Qwen's among them)
    /// accept a system message only at the start, so a mid-conversation system
    /// message would be refused by the provider rather than read by the model.
    /// The text says it comes from the kernel, so it is not mistaken for the
    /// user's own words. Like the notice it is not history; its uid is the
    /// turn's, so the same note is replaced rather than repeated.
    pub fn kernel_note_for_turn(turn_id: &TurnId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/kernel/{turn_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A reminder that the run's budget is nearly spent, for one turn.
    ///
    /// It is placed at the tail of the request, after this turn's own traffic,
    /// because that is where the model reads last and where a wrap-up
    /// instruction is least likely to be crowded out. User-role for the same
    /// reason as [`kernel_note_for_turn`](Self::kernel_note_for_turn): a
    /// mid-conversation system message is refused by some chat templates. It is
    /// not history — the projection never folds it — and its uid is the turn's,
    /// so later steps replace it rather than repeat it.
    pub fn budget_notice_for_turn(turn_id: &TurnId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/budget/{turn_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Where the run stands, placed at the tail of every request.
    ///
    /// Objective facts (steps, budget, the last failure) and the model's own
    /// `state/1` account, so a step that has lost its history reads its purpose
    /// at the point where it acts. User-role for the same reason as the budget
    /// notice: some chat templates accept a system message only at the start.
    /// Not history — the projection never folds it — and its uid is the turn's,
    /// so each step replaces the previous block rather than piling them up.
    pub fn work_state_for_turn(turn_id: &TurnId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/work-state/{turn_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// The state of the world when one turn opens.
    ///
    /// Like the context notice, this is not history: the projection never
    /// folds it. Its uid comes from the turn, so every step of the turn
    /// carries the same layer rather than a new one per request.
    pub fn state_for_turn(turn_id: &TurnId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/state/{turn_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// What one turn is being asked to do.
    ///
    /// Not history, for the same reason as [`state_for_turn`](Self::state_for_turn):
    /// the task belongs to the turn that is open, and a later fold must not
    /// replay it into a conversation where it was never asked.
    pub fn task_for_turn(turn_id: &TurnId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/task/{turn_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// What this run can do: its execution boundary and its tool index.
    ///
    /// Run-scoped, not turn-scoped: the boundary is fixed for the run, and the
    /// same text under the same uid is what lets prompt caching reuse it.
    /// Not history — the projection never folds it — so a rebuild of the log
    /// does not invent a capability statement that no event recorded.
    pub fn capability_for_run(run_id: &RunId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/capability/{run_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// The handover summary standing in for older history.
    ///
    /// It is a `system` message because it is not something either side said in
    /// the conversation: it is the loop telling the model what came before.
    /// Its uid comes from the compaction event's attempt, so a fold that reads
    /// the log rebuilds this exact message rather than a second summary.
    pub fn summary_for(attempt_id: &AttemptId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/summary/{attempt_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Whether this message is a compaction summary.
    ///
    /// A summary is a `system` message but not a preamble: it stands in for
    /// history, and a later compaction must be able to replace it. A consumer
    /// that treats every leading `system` message as instructions would keep
    /// every summary forever, one per compaction.
    pub fn is_summary(&self) -> bool {
        self.message_uid.as_str().starts_with("msg/summary/")
    }

    /// The deterministic ledger of the steps a compaction summarized.
    ///
    /// It travels beside the summary, under its own uid, so a later compaction
    /// can carry its lines forward instead of asking the model to retell them.
    /// Like the summary it is history, not a preamble.
    pub fn ledger_for(attempt_id: &AttemptId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/ledger/{attempt_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Whether this message is a step ledger written by a compaction.
    pub fn is_ledger(&self) -> bool {
        self.message_uid.as_str().starts_with("msg/ledger/")
    }

    /// The verbatim record of the user's messages a compaction removed.
    ///
    /// It travels beside the summary and the ledger, under its own uid, so a
    /// later compaction can carry the user's words forward instead of trusting
    /// a second model-written summary to repeat them. Like the summary it is
    /// history, not a preamble.
    pub fn usertext_for(attempt_id: &AttemptId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/usertext/{attempt_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Whether this message is a record of the user's removed messages.
    pub fn is_usertext(&self) -> bool {
        self.message_uid.as_str().starts_with("msg/usertext/")
    }

    /// The record of the failed calls a compaction removed.
    ///
    /// It travels beside the summary, the ledger and the user's words, under
    /// its own uid: a later compaction carries it whole, so the earliest
    /// failure survives even after the ledger's tail no longer reaches it.
    pub fn failures_for(attempt_id: &AttemptId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/failures/{attempt_id}")),
            role: MessageRole::System,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Whether this message is a record of failures.
    pub fn is_failures(&self) -> bool {
        self.message_uid.as_str().starts_with("msg/failures/")
    }

    /// Whether this message stands in for history rather than opening it.
    ///
    /// Every compaction product does: a summary, the ledger beside it, the
    /// verbatim record of the user's words, and the record of failures. They
    /// are `system` messages, but a preamble walk that stopped only at
    /// summaries would let a ledger or a record be treated as run instructions.
    pub fn is_compaction_output(&self) -> bool {
        self.is_summary() || self.is_ledger() || self.is_usertext() || self.is_failures()
    }

    /// The user message of one turn.
    pub fn user_for_turn(turn_id: &TurnId, text: impl Into<String>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/user/{turn_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A message the user injected between two steps of a running turn.
    ///
    /// It is a `user` message like the turn's opening input, but its identity
    /// comes from the inbox item that carried it — two steering notes in one
    /// turn are two messages, and a rebuild from `turn/steered` produces the
    /// same two rather than one merged block.
    pub fn user_for_steering(
        turn_id: &TurnId,
        item_id: &InboxItemId,
        text: impl Into<String>,
    ) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/user/{turn_id}/steer/{item_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// The assistant message of one model attempt.
    pub fn assistant_for_attempt(attempt_id: &AttemptId, content: Vec<ContentBlock>) -> Self {
        Self {
            message_uid: MessageUid::from_string(format!("msg/assistant/{attempt_id}")),
            role: MessageRole::Assistant,
            content,
        }
    }

    /// One tool outcome, identified by the step it was invoked in and the
    /// provider's own call id.
    pub fn tool_for_call(
        step_id: &StepId,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        let tool_call_id = tool_call_id.into();
        Self {
            message_uid: MessageUid::from_string(format!("msg/tool/{step_id}/{tool_call_id}")),
            role: MessageRole::Tool,
            content: vec![ContentBlock::tool_result(tool_call_id, tool_name, content, is_error)],
        }
    }

    /// Concatenated text blocks; empty when the message carries only tool traffic.
    pub fn text_content(&self) -> String {
        self.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
    }
}

/// A flattened view of a `ToolUse` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// One tool offered for a single request.
///
/// It is both halves of one fact: what the model is told about the tool, and
/// what an invocation is later checked against. `description` and `input_schema`
/// are what go on the wire; the two hashes are what make the snapshot
/// comparable, so a call can be rejected when the tool changed underneath it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub tool_id: String,
    pub registration_version: String,
    pub source: ToolSource,
    /// Model-facing prose, as sent.
    pub description: String,
    /// The JSON Schema of the arguments, as sent.
    pub input_schema: serde_json::Value,
    /// Hash of the input schema alone — the narrow question "did the arguments
    /// this call was written against change?".
    pub schema_hash: String,
}

/// The immutable set of tools available to one model request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifest {
    pub manifest_id: String,
    pub created_at: DateTime<Utc>,
    /// Tools usable for this request, already filtered by policy and
    /// registration state. An invocation that does not match this snapshot
    /// fails closed.
    pub entries: Vec<ManifestEntry>,
}

impl ToolManifest {
    /// A manifest offering no tools.
    pub fn empty() -> Self {
        Self { manifest_id: fresh_id(), created_at: Utc::now(), entries: Vec::new() }
    }

    /// A manifest over the given entries.
    pub fn new(entries: Vec<ManifestEntry>) -> Self {
        Self { manifest_id: fresh_id(), created_at: Utc::now(), entries }
    }

    /// A manifest offering one entry per definition.
    ///
    /// Entries are sorted by `tool_id`. The order reaches the wire, so it has
    /// to be a property of the set and not of however the caller happened to
    /// collect the definitions — otherwise the same tools arrive in a different
    /// order on every request and defeat prompt caching.
    pub fn from_definitions(definitions: &[ToolDefinition]) -> Self {
        let mut entries: Vec<ManifestEntry> = definitions
            .iter()
            .map(|definition| ManifestEntry {
                tool_id: definition.name.clone(),
                registration_version: definition_version(definition),
                source: definition.source,
                description: definition.description.clone(),
                input_schema: definition.input_schema.clone(),
                schema_hash: hash_json(&definition.input_schema),
            })
            .collect();
        entries.sort_by(|a, b| a.tool_id.cmp(&b.tool_id));
        Self::new(entries)
    }

    /// Whether no tool is offered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A stable digest of a definition's whole declared surface.
///
/// It answers "is this still the tool the model was told about?" — broader than
/// [`ManifestEntry::schema_hash`], which covers only the arguments.
fn definition_version(definition: &ToolDefinition) -> String {
    let declared = serde_json::json!({
        "name": definition.name,
        "description": definition.description,
        "side_effect": definition.side_effect,
        "input_schema": definition.input_schema,
        "idempotent": definition.idempotent,
    });
    hash_json(&declared)
}

/// Hash a JSON value into a short stable digest.
///
/// The canonical digest every persisted declaration in this protocol uses; see
/// [`crate::digest`] for why it is FNV-1a and why it lives in one place.
fn hash_json(value: &serde_json::Value) -> String {
    crate::digest::json(value)
}

/// Which model an agent role should use.
///
/// `PartialOrd`/`Ord` exist so a route table can be a `BTreeMap` keyed by tier:
/// two runs of one configuration must produce the same serialization, and a map
/// ordered by its key is how that holds without a hash seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Main,
    Fast,
    Thinking,
}

impl ModelTier {
    /// The wire string, matching the serde representation.
    ///
    /// Anything that names a tier outside serde — a decision question's options,
    /// for instance — uses this rather than its own spelling, so the two can
    /// never drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Fast => "fast",
            Self::Thinking => "thinking",
        }
    }
}

/// The router's choice for one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRoute {
    pub tier: ModelTier,
    pub provider: String,
    pub model: String,
}

impl ModelRoute {
    /// A route to a named model.
    pub fn new(tier: ModelTier, provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self { tier, provider: provider.into(), model: model.into() }
    }
}

/// The shape of the response the caller wants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema { schema: serde_json::Value },
}

/// Per-request spend limits. Every field is optional; `None` means unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelBudget {
    pub max_tokens: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub wall_clock_ms: Option<u64>,
}

/// Token accounting. Inner fields are optional because providers may report a
/// partial breakdown; a provider with no `token_usage` capability reports no
/// `Usage` at all rather than zeros.
///
/// `cache_hit_tokens`/`cache_miss_tokens` are the parts of `input_tokens` the
/// provider served from (or recomputed outside) its prompt cache. They are
/// recorded because a cache hit costs a fraction of a miss, so the hit rate is
/// the number that says whether a context layout is actually cheaper. The new
/// fields are skipped when absent, so a provider that does not report them
/// writes the same payload it always did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_tokens: Option<u64>,
    /// Reasoning tokens inside `output_tokens`. Recorded, not content: the
    /// reasoning text itself never settles into a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// The tokens added for a message's own framing (role, separators).
///
/// Public because the context inspector accounts for it as its own category;
/// it is the same number, not a second estimate.
pub const FRAME_TOKENS: usize = 4;

/// Fixed-point cost of one ASCII letter, in thousandths of a token.
///
/// Calibrated on the project's endpoint (2026-10-20) over sixteen samples in
/// eight corpus classes, half used for fitting and half held out. The earlier
/// single ASCII rate (0.26) was already correct for prose but 40% low for
/// listings, CSV, and log lines, where digits and punctuation dominate. The
/// holdout stayed within 15% of the provider's reported input for every class.
const UNITS_PER_ASCII_LETTER: u64 = 180;

/// Cost of one ASCII digit. Numerals tokenize at close to one token per
/// character; treating them like letters was the largest source of
/// underestimation on structured tool output.
const UNITS_PER_ASCII_DIGIT: u64 = 920;

/// Cost of one ASCII whitespace character. Spaces and line breaks are cheaper
/// than letters on prose but still carry tokens, especially as indentation.
const UNITS_PER_ASCII_SPACE: u64 = 360;

/// Cost of one other ASCII punctuation character.
const UNITS_PER_ASCII_PUNCT: u64 = 460;

/// Cost of one CJK ideograph (also kana and Hangul syllables).
///
/// The rate depends on vocabulary: common conversational Chinese measured
/// 0.51–0.57 tokens per character, technical prose 0.75–0.94. 0.60 is the
/// middle that keeps both document-shaped samples and the short conversational
/// samples within 15% of their measured usage. CJK punctuation and full-width
/// forms are not ideographs and fall through to the conservative rate.
const UNITS_PER_IDEOGRAPH: u64 = 600;

/// Cost of any other non-ASCII character. Conservative: these are rare in the
/// traffic this project sees, and over-counting them cannot exhaust a window.
/// CJK punctuation and full-width forms are counted here because they usually
/// tokenize as one token each.
const UNITS_PER_OTHER: u64 = 1_000;

/// Thousandths of a token in one token.
const UNITS_PER_TOKEN: u64 = 1_000;

/// Estimated tokens of one string.
///
/// Pure and deterministic: the same string always estimates the same, which is
/// what lets a recorded drop count be reproduced from the log. It is an
/// estimate, not a tokenizer: the per-character classes are calibrated against
/// the endpoint's reported usage, and real usage is still recorded alongside it.
pub fn estimate_text(text: &str) -> usize {
    let units: u64 = text
        .chars()
        .map(|ch| {
            if ch.is_ascii() {
                if ch.is_ascii_alphabetic() {
                    UNITS_PER_ASCII_LETTER
                } else if ch.is_ascii_digit() {
                    UNITS_PER_ASCII_DIGIT
                } else if ch.is_ascii_whitespace() {
                    UNITS_PER_ASCII_SPACE
                } else {
                    UNITS_PER_ASCII_PUNCT
                }
            } else if is_ideograph(ch) {
                UNITS_PER_IDEOGRAPH
            } else {
                UNITS_PER_OTHER
            }
        })
        .sum();
    usize::try_from(units.div_ceil(UNITS_PER_TOKEN)).unwrap_or(usize::MAX)
}

/// Whether `ch` is an ideograph, a kana, or a Hangul syllable.
///
/// The ranges cover unified and compatibility ideographs plus Japanese kana
/// and Hangul syllables, the characters that most often merge into multi-
/// character tokens. CJK punctuation and full-width forms are deliberately
/// outside this set: they usually cost a full token each.
fn is_ideograph(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF)
}

/// Estimated tokens of one content block.
///
/// The parts a message is made of, before its own framing: this is what lets a
/// report attribute cost to a category and still add up to the message estimate.
pub fn estimate_block(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => estimate_text(text),
        ContentBlock::ToolUse { tool_name, arguments, .. } => {
            estimate_text(tool_name) + estimate_text(&arguments.to_string())
        }
        ContentBlock::ToolResult { tool_name, content, .. } => {
            estimate_text(tool_name) + estimate_text(content)
        }
    }
}

/// Estimated tokens of one message, framing included.
pub fn estimate_message(message: &ModelMessage) -> usize {
    message.content.iter().map(estimate_block).sum::<usize>() + FRAME_TOKENS
}

/// One model request. Everything the model may see is already persisted and
/// summarized into `messages` by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub step_id: StepId,
    /// Revision of the derived context these messages were built from.
    pub context_revision: u64,
    pub messages: Vec<ModelMessage>,
    pub tool_manifest: ToolManifest,
    /// Capabilities this request depends on.
    pub capabilities: Vec<ModelCapability>,
    pub response_format: Option<ResponseFormat>,
    /// `None` when no router decided a route; the provider then uses its own
    /// configured default model.
    pub model_route: Option<ModelRoute>,
    pub budget: ModelBudget,
}

/// A settled model response. Only completed responses are persisted; a partial
/// stream never reaches this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content_blocks: Vec<ContentBlock>,
    /// `None` when the provider does not report usage.
    pub usage: Option<Usage>,
    pub finish_reason: FinishReason,
    /// Opaque provider payload. The kernel never interprets it.
    #[serde(default)]
    pub provider_metadata: serde_json::Value,
}

impl ModelResponse {
    /// A final text answer with no tool calls.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content_blocks: vec![ContentBlock::text(text)],
            usage: None,
            finish_reason: FinishReason::new(FinishReason::STOP),
            provider_metadata: serde_json::Value::Null,
        }
    }

    /// Tool calls proposed by this response, flattened from `ToolUse` blocks.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content_blocks
            .iter()
            .filter_map(|block| {
                block.as_tool_use().map(|(tool_call_id, tool_name, arguments)| ToolCall {
                    tool_call_id: tool_call_id.to_string(),
                    tool_name: tool_name.to_string(),
                    arguments: arguments.clone(),
                })
            })
            .collect()
    }

    /// Concatenated text blocks.
    pub fn text_content(&self) -> String {
        self.content_blocks.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
    }

    /// Whether this response ends the turn — that is, it proposes no tools.
    pub fn is_final(&self) -> bool {
        !self.content_blocks.iter().any(|b| b.as_tool_use().is_some())
    }
}

/// An incremental chunk from a streaming model attempt.
///
/// These are live-only: they carry no durable sequence number and never take
/// part in replay. A stream becomes durable truth solely by settling into a
/// completed response or into an attempt failure/cancellation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    /// The provider began producing this attempt.
    Started,
    /// A chunk of assistant text.
    TextDelta { text: String },
    /// A partial tool call. `tool_call_id` and `tool_name` arrive with the
    /// first chunk of each call; later chunks carry only more
    /// `arguments_delta`.
    ToolCallDelta {
        index: u32,
        tool_call_id: Option<String>,
        tool_name: Option<String>,
        arguments_delta: String,
    },
    /// A chunk of reasoning content, when the provider exposes it.
    ReasoningDelta { text: String },
}

impl ModelStreamEvent {
    /// The live event type name this chunk maps to.
    pub fn live_event_type(&self) -> &'static str {
        match self {
            Self::Started => EventType::MODEL_STREAM_STARTED,
            Self::TextDelta { .. } => EventType::MODEL_STREAM_DELTA,
            Self::ToolCallDelta { .. } => EventType::MODEL_STREAM_TOOL_CALL_DELTA,
            Self::ReasoningDelta { .. } => EventType::MODEL_STREAM_REASONING_DELTA,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capability_and_finish_reason_are_transparent_strings() {
        assert_eq!(
            serde_json::to_string(&ModelCapability::new(ModelCapability::TOOL_CALL)).unwrap(),
            "\"tool_call\""
        );
        assert_eq!(
            serde_json::to_string(&FinishReason::new(FinishReason::TOOL_CALLS)).unwrap(),
            "\"tool_calls\""
        );
    }

    #[test]
    fn finish_reason_preserves_unknown_provider_values() {
        let exotic = FinishReason::new("provider_specific_pause");
        let json = serde_json::to_string(&exotic).unwrap();
        let decoded: FinishReason = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, exotic);
        assert_ne!(decoded, FinishReason::new(FinishReason::STOP));
    }

    #[test]
    fn content_blocks_round_trip_with_type_tag() {
        let blocks = vec![
            ContentBlock::text("hello"),
            ContentBlock::tool_use("call_1", "fs.read", json!({ "path": "a.txt" })),
            ContentBlock::tool_result("call_1", "fs.read", "contents", false),
        ];
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("\"type\":\"tool_use\""), "{json}");
        let decoded: Vec<ContentBlock> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn message_role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&MessageRole::Assistant).unwrap(), "\"assistant\"");
        assert_eq!(MessageRole::Tool.as_str(), "tool");
    }

    #[test]
    fn model_message_keeps_uid_and_joins_text_blocks() {
        let message = ModelMessage::new(
            MessageRole::Assistant,
            vec![
                ContentBlock::text("first"),
                ContentBlock::tool_use("call_1", "fs.read", json!({})),
                ContentBlock::text("second"),
            ],
        );
        assert_eq!(message.text_content(), "first\nsecond");

        let decoded: ModelMessage =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(decoded.message_uid, message.message_uid);
        assert_eq!(decoded.content.len(), 3);
    }

    fn definition(name: &str, schema: serde_json::Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("does {name}"),
            source: ToolSource::Builtin,
            side_effect: crate::tool::ToolSideEffect::Read,
            capability_requirements: Vec::new(),
            idempotent: true,
            timeout_ms: None,
            input_schema: schema,
        }
    }

    #[test]
    fn manifest_is_sorted_and_carries_what_the_model_needs() {
        // Deliberately not in name order: the manifest must not depend on how
        // the caller collected the definitions.
        let manifest = ToolManifest::from_definitions(&[
            definition("shell.run", json!({ "type": "object" })),
            definition("fs.read", json!({ "type": "object", "properties": {} })),
        ]);

        let ids: Vec<&str> = manifest.entries.iter().map(|e| e.tool_id.as_str()).collect();
        assert_eq!(ids, vec!["fs.read", "shell.run"]);
        assert_eq!(manifest.entries[0].description, "does fs.read");
        assert_eq!(manifest.entries[0].input_schema["type"], "object");
    }

    #[test]
    fn manifest_hashes_change_only_with_what_they_cover() {
        let schema = json!({ "type": "object", "properties": { "path": { "type": "string" } } });
        let base = ToolManifest::from_definitions(&[definition("fs.read", schema.clone())]);

        // Same declaration → same digests, which is what makes a manifest
        // comparable across processes and Rust versions.
        let again = ToolManifest::from_definitions(&[definition("fs.read", schema.clone())]);
        assert_eq!(base.entries[0].schema_hash, again.entries[0].schema_hash);
        assert_eq!(base.entries[0].registration_version, again.entries[0].registration_version);

        // A changed argument shape moves both digests.
        let widened = ToolManifest::from_definitions(&[definition(
            "fs.read",
            json!({ "type": "object", "properties": { "path": { "type": "string" }, "n": {} } }),
        )]);
        assert_ne!(base.entries[0].schema_hash, widened.entries[0].schema_hash);
        assert_ne!(base.entries[0].registration_version, widened.entries[0].registration_version);

        // A changed description moves only the broader one: the arguments a
        // call was written against are untouched.
        let mut reworded = definition("fs.read", schema);
        reworded.description = "read a file, honestly".to_string();
        let reworded = ToolManifest::from_definitions(&[reworded]);
        assert_eq!(base.entries[0].schema_hash, reworded.entries[0].schema_hash);
        assert_ne!(base.entries[0].registration_version, reworded.entries[0].registration_version);
    }

    #[test]
    fn model_request_round_trips_with_manifest_and_budget() {
        let request = ModelRequest {
            session_id: SessionId::from_string("s1"),
            turn_id: TurnId::new(),
            step_id: StepId::new(),
            context_revision: 3,
            messages: vec![ModelMessage::text(MessageRole::User, "hi")],
            tool_manifest: ToolManifest::new(vec![ManifestEntry {
                tool_id: "fs.read".to_string(),
                registration_version: "v1".to_string(),
                source: ToolSource::Builtin,
                description: "Read a file".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
                schema_hash: "abc".to_string(),
            }]),
            capabilities: vec![ModelCapability::new(ModelCapability::TOOL_CALL)],
            response_format: Some(ResponseFormat::JsonObject),
            model_route: Some(ModelRoute::new(ModelTier::Main, "openai", "gpt-x")),
            budget: ModelBudget { max_tokens: Some(256), ..ModelBudget::default() },
        };
        let json = serde_json::to_string(&request).unwrap();
        let decoded: ModelRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.context_revision, 3);
        assert_eq!(decoded.tool_manifest.entries.len(), 1);
        assert_eq!(decoded.tool_manifest.entries[0].source, ToolSource::Builtin);
        assert_eq!(decoded.model_route.as_ref().map(|r| r.tier), Some(ModelTier::Main));
        assert_eq!(decoded.budget.max_tokens, Some(256));
        assert_eq!(decoded.budget.max_cost_usd, None);
    }

    #[test]
    fn derived_message_uids_are_a_function_of_durable_ids() {
        let turn = TurnId::from_string("t1");
        let attempt = AttemptId::from_string("a1");
        let step = StepId::from_string("s1");

        // Same ids in, same message out — this is what lets the loop's sent
        // history and a projection's rebuilt history be the same thing.
        assert_eq!(
            ModelMessage::user_for_turn(&turn, "hi"),
            ModelMessage::user_for_turn(&TurnId::from_string("t1"), "hi")
        );
        assert_eq!(
            ModelMessage::assistant_for_attempt(&attempt, vec![ContentBlock::text("yo")]),
            ModelMessage::assistant_for_attempt(
                &AttemptId::from_string("a1"),
                vec![ContentBlock::text("yo")]
            )
        );
        assert_eq!(
            ModelMessage::tool_for_call(&step, "call_1", "fs.read", "ok", false),
            ModelMessage::tool_for_call(
                &StepId::from_string("s1"),
                "call_1",
                "fs.read",
                "ok",
                false
            )
        );

        // Different facts, different uids.
        assert_ne!(
            ModelMessage::user_for_turn(&turn, "hi").message_uid,
            ModelMessage::user_for_turn(&TurnId::from_string("t2"), "hi").message_uid
        );
        assert_ne!(
            ModelMessage::tool_for_call(&step, "call_1", "fs.read", "ok", false).message_uid,
            ModelMessage::tool_for_call(&step, "call_2", "fs.read", "ok", false).message_uid
        );

        // The per-turn layers are named after the turn, so every step of one
        // turn carries the same two messages.
        assert_eq!(
            ModelMessage::state_for_turn(&turn, "state"),
            ModelMessage::state_for_turn(&TurnId::from_string("t1"), "state")
        );
        assert_eq!(ModelMessage::task_for_turn(&turn, "task").message_uid.as_str(), "msg/task/t1");
        assert_eq!(
            ModelMessage::state_for_turn(&turn, "state").message_uid.as_str(),
            "msg/state/t1"
        );
        assert_ne!(
            ModelMessage::task_for_turn(&turn, "task").message_uid,
            ModelMessage::task_for_turn(&TurnId::from_string("t2"), "task").message_uid
        );
        assert_eq!(ModelMessage::state_for_turn(&turn, "state").role, MessageRole::System);

        // The capability layer is named after the run: every turn of one run
        // carries the same message, and a second run gets a different one.
        let run = RunId::from_string("r1");
        assert_eq!(
            ModelMessage::capability_for_run(&run, "caps").message_uid.as_str(),
            "msg/capability/r1"
        );
        assert_ne!(
            ModelMessage::capability_for_run(&run, "caps").message_uid,
            ModelMessage::capability_for_run(&RunId::from_string("r2"), "caps").message_uid
        );
        assert_eq!(ModelMessage::capability_for_run(&run, "caps").role, MessageRole::System);
    }

    #[test]
    fn model_response_round_trips_and_keeps_provider_metadata() {
        let response = ModelResponse {
            content_blocks: vec![ContentBlock::text("done")],
            usage: Some(Usage { input_tokens: Some(10), ..Usage::default() }),
            finish_reason: FinishReason::new(FinishReason::STOP),
            provider_metadata: json!({ "request_id": "req_1" }),
        };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: ModelResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.content_blocks, response.content_blocks);
        assert_eq!(decoded.provider_metadata["request_id"], "req_1");
        assert_eq!(decoded.usage.unwrap().output_tokens, None);
    }

    #[test]
    fn tool_calls_project_only_tool_use_blocks() {
        let response = ModelResponse {
            content_blocks: vec![
                ContentBlock::text("let me look"),
                ContentBlock::tool_use("call_1", "fs.read", json!({ "path": "a.txt" })),
                ContentBlock::tool_use("call_2", "fs.write", json!({ "path": "b.txt" })),
            ],
            usage: None,
            finish_reason: FinishReason::new(FinishReason::TOOL_CALLS),
            provider_metadata: serde_json::Value::Null,
        };
        let calls = response.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool_call_id, "call_1");
        assert_eq!(calls[1].tool_name, "fs.write");
        assert!(!response.is_final());

        assert!(ModelResponse::text("bye").is_final());
        assert_eq!(ModelResponse::text("bye").tool_calls().len(), 0);
    }

    #[test]
    fn usage_absence_differs_from_all_none_usage() {
        assert!(ModelResponse::text("x").usage.is_none());
        let partial = Usage::default();
        assert_eq!(partial.total_tokens, None);
        assert_ne!(Some(partial), None);
    }

    #[test]
    fn stream_events_round_trip_and_map_to_live_types() {
        let chunks = vec![
            ModelStreamEvent::Started,
            ModelStreamEvent::TextDelta { text: "hi".to_string() },
            ModelStreamEvent::ToolCallDelta {
                index: 0,
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("fs.read".to_string()),
                arguments_delta: "{\"pa".to_string(),
            },
            ModelStreamEvent::ReasoningDelta { text: "hmm".to_string() },
        ];
        let json = serde_json::to_string(&chunks).unwrap();
        assert!(json.contains("\"type\":\"tool_call_delta\""), "{json}");
        let decoded: Vec<ModelStreamEvent> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, chunks);

        assert_eq!(chunks[0].live_event_type(), EventType::MODEL_STREAM_STARTED);
        assert_eq!(chunks[1].live_event_type(), EventType::MODEL_STREAM_DELTA);
        assert_eq!(chunks[2].live_event_type(), EventType::MODEL_STREAM_TOOL_CALL_DELTA);
        assert_eq!(chunks[3].live_event_type(), EventType::MODEL_STREAM_REASONING_DELTA);
    }

    #[test]
    fn empty_manifest_offers_no_tools() {
        let manifest = ToolManifest::empty();
        assert!(manifest.is_empty());
        assert!(!manifest.manifest_id.is_empty());
    }
}
