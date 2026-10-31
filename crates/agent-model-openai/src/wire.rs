//! Wire types and the normalization boundary.
//!
//! Everything vendor-specific stops here: `ModelMessage` goes out as
//! OpenAI-shaped JSON, and streaming chunks come back as the protocol's
//! `ModelStreamEvent`s.

use agent_protocol::model::{
    ContentBlock, ManifestEntry, MessageRole, ModelMessage, ModelRequest, ModelStreamEvent,
    ReasoningEffort, ResponseFormat, ToolManifest, Usage,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::OpenAiCompatConfig;

/// A chat-completions request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<WireMessage>,
    pub stream: bool,
    pub stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// How deeply the model should reason. Sent only when the run's budget
    /// names a depth; a provider that has not declared support gets no field
    /// rather than a fabricated value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<WireResponseFormat>,
    /// Omitted entirely when no tool is on offer. Sending an empty array is not
    /// the same request: some vendors treat its presence as "tool use is on".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<WireTool>>,
}

/// Streaming options. `include_usage` is what makes the endpoint report token
/// counts; without it the final usage block never arrives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// One wire message. The same shape covers all four roles; which fields are set
/// depends on the role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireMessage {
    pub role: String,
    /// Never skipped. An assistant turn that carries only tool calls has no
    /// text, and the field must still be present as an explicit `null`: at
    /// least one vendor types it as required-but-nullable, so omitting it is a
    /// different (and rejected) request than sending `null`.
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The tool name on a `tool` message. Some vendors require it, and it is
    /// free to send, so it is always set when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A tool call as sent back to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: WireFunction,
}

/// A tool call's name and serialized arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireFunction {
    pub name: String,
    pub arguments: String,
}

/// One tool offered to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: WireToolFunction,
}

/// A tool's declared interface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A JSON Schema object. Always sent: omitting `parameters` means "no
    /// arguments", which is a different tool than the one we registered.
    pub parameters: serde_json::Value,
}

/// Why a request cannot be expressed on this wire.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WireError {
    #[error(
        "tool names '{first}' and '{second}' both become '{wire_name}' on the wire; \
         the name is what the model calls back with, so they cannot share one"
    )]
    ToolNameCollision { first: String, second: String, wire_name: String },
}

/// The name a tool is offered under.
///
/// The endpoint accepts only `[a-zA-Z0-9_-]` in a function name, so a dotted
/// internal id such as `fs.read` is offered as `fs_read`. The internal id is
/// what policy, events, and the audit trail key on, so it never changes — only
/// this projection does.
pub fn to_wire_name(tool_id: &str) -> String {
    tool_id.replace('.', "_")
}

/// The inverse projection, over one manifest: wire name back to tool id.
///
/// A name that is not in the index is the model inventing a tool, and is left
/// alone so the gateway can answer it with a real "unknown tool" result rather
/// than a silently mismapped call.
pub fn wire_name_index(manifest: &ToolManifest) -> BTreeMap<String, String> {
    manifest.entries.iter().map(|e| (to_wire_name(&e.tool_id), e.tool_id.clone())).collect()
}

/// Project a manifest onto the wire.
///
/// The manifest is already the model-facing view, so this is a rename and a
/// shape change — no field is invented here, and none is dropped silently.
pub fn to_wire_tools(manifest: &ToolManifest) -> Result<Option<Vec<WireTool>>, WireError> {
    if manifest.is_empty() {
        return Ok(None);
    }
    let mut claimed: BTreeMap<String, &str> = BTreeMap::new();
    let mut tools = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let wire_name = to_wire_name(&entry.tool_id);
        if let Some(first) = claimed.insert(wire_name.clone(), &entry.tool_id)
            && first != entry.tool_id
        {
            return Err(WireError::ToolNameCollision {
                first: first.to_string(),
                second: entry.tool_id.clone(),
                wire_name,
            });
        }
        tools.push(WireTool {
            kind: "function".to_string(),
            function: WireToolFunction {
                name: wire_name,
                description: description_of(entry),
                parameters: entry.input_schema.clone(),
            },
        });
    }
    Ok(Some(tools))
}

/// An empty description is a defect upstream; sending `""` would spend tokens
/// saying nothing, so it is treated as absent.
fn description_of(entry: &ManifestEntry) -> Option<String> {
    let trimmed = entry.description.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// The requested response shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireResponseFormat {
    JsonObject,
    JsonSchema { json_schema: JsonSchemaSpec },
}

/// The named wrapper around a JSON schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonSchemaSpec {
    pub name: String,
    pub schema: serde_json::Value,
}

/// One streaming chunk.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WireChunk {
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
    #[serde(default)]
    pub model: Option<String>,
}

/// One choice within a chunk. Only the first is read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WireChoice {
    #[serde(default)]
    pub delta: WireDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The incremental part of a choice.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WireDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// Reasoning text, as exposed by some vendors.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<WireDeltaToolCall>>,
}

/// A partial tool call. Every field is optional because they arrive spread
/// across chunks: the id and name come with the first, the arguments follow.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WireDeltaToolCall {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireDeltaFunction>,
}

/// The name/arguments half of a partial tool call.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WireDeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Token accounting as reported by the endpoint.
///
/// DeepSeek splits the prompt count into cache hit/miss and OpenAI-style
/// vendors report the cached part under `prompt_tokens_details`. Both shapes
/// are accepted; a field absent from the payload stays `None`, never zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<WireCompletionTokensDetails>,
}

/// The prompt-side breakdown some vendors send alongside the top-level count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct WirePromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// The completion-side breakdown. `reasoning_tokens` is a subset of
/// `completion_tokens`, not an extra cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct WireCompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl From<WireUsage> for Usage {
    fn from(wire: WireUsage) -> Self {
        Self {
            input_tokens: wire.prompt_tokens,
            output_tokens: wire.completion_tokens,
            total_tokens: wire.total_tokens,
            cache_hit_tokens: wire
                .prompt_cache_hit_tokens
                .or_else(|| wire.prompt_tokens_details.and_then(|details| details.cached_tokens)),
            cache_miss_tokens: wire.prompt_cache_miss_tokens,
            reasoning_tokens: wire
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens),
        }
    }
}

/// An error body. Vendors disagree on the shape, so it is best-effort.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WireErrorEnvelope {
    #[serde(default)]
    pub error: Option<WireErrorBody>,
    #[serde(default)]
    pub message: Option<String>,
}

/// The inner error object.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WireErrorBody {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

/// Build the request body for one attempt.
pub fn chat_request(
    config: &OpenAiCompatConfig,
    request: &ModelRequest,
) -> Result<ChatRequest, WireError> {
    let model = request
        .model_route
        .as_ref()
        .map(|route| route.model.clone())
        .unwrap_or_else(|| config.model.clone());
    Ok(ChatRequest {
        model,
        messages: request.messages.iter().flat_map(to_wire_messages).collect(),
        stream: true,
        stream_options: StreamOptions { include_usage: true },
        max_tokens: request.budget.max_tokens.or(config.default_max_tokens),
        reasoning_effort: request.budget.reasoning_effort,
        response_format: request.response_format.as_ref().and_then(to_wire_response_format),
        tools: to_wire_tools(&request.tool_manifest)?,
    })
}

/// Convert one protocol message into wire messages.
///
/// Usually one in, one out. The exception is a `tool` message carrying several
/// results: the wire format pairs each result with the call it answers, so it
/// becomes one message per result.
///
/// Every tool name that leaves here — on a call and on a result alike — is
/// projected through [`to_wire_name`], because the endpoint validates the name
/// wherever it appears, not only in `tools[]`.
pub fn to_wire_messages(message: &ModelMessage) -> Vec<WireMessage> {
    let role = message.role.as_str().to_string();
    if message.role == MessageRole::Tool {
        let results: Vec<WireMessage> = message
            .content
            .iter()
            .filter_map(ContentBlock::as_tool_result)
            // `is_error` has no wire equivalent; the outcome text already
            // carries the failure, so only the text is sent.
            .map(|(tool_call_id, tool_name, content, _is_error)| WireMessage {
                role: "tool".to_string(),
                content: Some(content.to_string()),
                tool_calls: None,
                tool_call_id: Some(tool_call_id.to_string()),
                name: Some(to_wire_name(tool_name)),
            })
            .collect();
        if !results.is_empty() {
            return results;
        }
    }

    let tool_calls: Vec<WireToolCall> = message
        .content
        .iter()
        .filter_map(ContentBlock::as_tool_use)
        .map(|(tool_call_id, tool_name, arguments)| WireToolCall {
            id: tool_call_id.to_string(),
            kind: "function".to_string(),
            function: WireFunction {
                name: to_wire_name(tool_name),
                arguments: to_arguments_string(arguments),
            },
        })
        .collect();
    let text = message.text_content();
    // An assistant turn that only proposes tools has no text; sending an empty
    // string there is not the same as sending nothing.
    let content = if text.is_empty() && !tool_calls.is_empty() { None } else { Some(text) };
    vec![WireMessage {
        role,
        content,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
        name: None,
    }]
}

fn to_wire_response_format(format: &ResponseFormat) -> Option<WireResponseFormat> {
    match format {
        ResponseFormat::Text => None,
        ResponseFormat::JsonObject => Some(WireResponseFormat::JsonObject),
        ResponseFormat::JsonSchema { schema } => Some(WireResponseFormat::JsonSchema {
            json_schema: JsonSchemaSpec { name: "response".to_string(), schema: schema.clone() },
        }),
    }
}

fn to_arguments_string(arguments: &serde_json::Value) -> String {
    // Arguments are sent as a JSON *string*, not as a nested object.
    serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string())
}

/// Map a vendor finish reason onto the protocol's vocabulary. Unknown values
/// pass through unchanged rather than collapsing to a catch-all.
pub fn normalize_finish_reason(raw: &str) -> agent_protocol::model::FinishReason {
    use agent_protocol::model::FinishReason;
    let mapped = match raw {
        "stop" => FinishReason::STOP,
        "length" => FinishReason::LENGTH,
        "tool_calls" | "function_call" => FinishReason::TOOL_CALLS,
        "content_filter" => FinishReason::CONTENT_FILTER,
        other => other,
    };
    FinishReason::new(mapped)
}

/// The delta events a chunk carries, in the order they arrived.
pub fn chunk_deltas(chunk: &WireChunk) -> Vec<ModelStreamEvent> {
    let Some(choice) = chunk.choices.first() else { return Vec::new() };
    let mut events = Vec::new();
    if let Some(text) = choice.delta.content.as_ref().filter(|t| !t.is_empty()) {
        events.push(ModelStreamEvent::TextDelta { text: text.clone() });
    }
    if let Some(text) = choice.delta.reasoning_content.as_ref().filter(|t| !t.is_empty()) {
        events.push(ModelStreamEvent::ReasoningDelta { text: text.clone() });
    }
    for call in choice.delta.tool_calls.iter().flatten() {
        events.push(ModelStreamEvent::ToolCallDelta {
            index: call.index.unwrap_or(0),
            tool_call_id: call.id.clone(),
            tool_name: call.function.as_ref().and_then(|f| f.name.clone()),
            arguments_delta: call
                .function
                .as_ref()
                .and_then(|f| f.arguments.clone())
                .unwrap_or_default(),
        });
    }
    events
}

/// The most useful message an error body offers, falling back to the raw body.
pub fn describe_error_body(body: &str) -> String {
    if let Ok(envelope) = serde_json::from_str::<WireErrorEnvelope>(body) {
        if let Some(message) = envelope.error.and_then(|e| e.message) {
            return message;
        }
        if let Some(message) = envelope.message {
            return message;
        }
    }
    body.to_string()
}
