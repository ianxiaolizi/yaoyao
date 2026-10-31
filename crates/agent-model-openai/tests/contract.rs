//! Contract tests for the OpenAI-compatible adapter.
//!
//! Everything here runs offline: the transport is faked at the byte-stream
//! level, so no test depends on an endpoint being reachable.

use std::sync::{Arc, Mutex};

use agent_model_openai::{
    ChatRequest, OpenAiCompatConfig, OpenAiCompatProvider, SseDecoder, StreamAccumulator,
    Transport, TransportError, TransportRequest, TransportResponse, WireChunk, WireError,
    WireUsage, chat_request, normalize_finish_reason, to_wire_messages, to_wire_name,
    to_wire_tools, wire_name_index,
};
use agent_ports::model_provider::{
    ContentBlock, FinishReason, MessageRole, ModelBudget, ModelCapability, ModelMessage,
    ModelProvider, ModelProviderError, ModelRequest, ModelRoute, ModelStreamError,
    ModelStreamEvent, ModelStreamSink, ModelTier, ReasoningEffort, ResponseFormat, ToolManifest,
    Usage,
};
use agent_protocol::model::{ModelCapabilitySource, ModelCapabilities};
use agent_protocol::ids::{SessionId, StepId, TurnId};
use agent_protocol::tool::{ToolDefinition, ToolSideEffect, ToolSource};
use agent_testkit::{FixedSecret, MissingSecret, RecordingSink};

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// A transport that replays a scripted chunk sequence.
struct FakeTransport {
    status: u16,
    chunks: Vec<Result<Vec<u8>, TransportError>>,
    /// What `GET /models` answers, when a test asks for capabilities.
    get_chunks: Vec<Result<Vec<u8>, TransportError>>,
    /// When set, the POST itself fails and no response body ever exists.
    post_error: Option<TransportError>,
    seen: Mutex<Vec<TransportRequest>>,
    gets: Mutex<usize>,
}

impl FakeTransport {
    fn new(status: u16, chunks: Vec<Result<Vec<u8>, TransportError>>) -> Self {
        Self {
            status,
            chunks,
            get_chunks: Vec::new(),
            post_error: None,
            seen: Mutex::new(Vec::new()),
            gets: Mutex::new(0),
        }
    }

    /// A transport whose `GET /models` answers with `body`.
    fn models(body: &str) -> Self {
        let mut transport = Self::body("");
        transport.get_chunks = vec![Ok(body.as_bytes().to_vec())];
        transport
    }

    fn get_count(&self) -> usize {
        self.gets.lock().map(|count| *count).unwrap_or_default()
    }

    /// A 200 response whose whole body is already in hand.
    fn body(body: &str) -> Self {
        Self::raw(body.as_bytes().to_vec())
    }

    /// A 200 response delivered as one byte chunk.
    fn raw(bytes: Vec<u8>) -> Self {
        Self::new(200, vec![Ok(bytes)])
    }

    /// A 200 response delivered as several byte chunks, as a network would.
    fn chunked(chunks: Vec<Vec<u8>>) -> Self {
        Self::new(200, chunks.into_iter().map(Ok).collect())
    }

    /// A response with a non-200 status.
    fn status(status: u16, body: &str) -> Self {
        Self::new(status, vec![Ok(body.as_bytes().to_vec())])
    }

    /// A 200 response whose body dies partway through.
    fn interrupted(body: &str, error: TransportError) -> Self {
        Self::new(200, vec![Ok(body.as_bytes().to_vec()), Err(error)])
    }

    /// A transport that cannot reach the endpoint at all.
    fn unreachable(error: TransportError) -> Self {
        Self {
            status: 0,
            chunks: Vec::new(),
            get_chunks: Vec::new(),
            post_error: Some(error),
            seen: Mutex::new(Vec::new()),
            gets: Mutex::new(0),
        }
    }

    fn requests(&self) -> Vec<TransportRequest> {
        self.seen.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Transport for FakeTransport {
    async fn post(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(request);
        }
        if let Some(error) = self.post_error.clone() {
            return Err(error);
        }
        let chunks = self.chunks.clone();
        Ok(TransportResponse {
            status: self.status,
            body: Box::pin(futures_util::stream::iter(chunks)),
        })
    }

    async fn get(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        if let Ok(mut gets) = self.gets.lock() {
            *gets += 1;
        }
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(request);
        }
        let chunks = self.get_chunks.clone();
        Ok(TransportResponse {
            status: self.status,
            body: Box::pin(futures_util::stream::iter(chunks)),
        })
    }
}

/// One attempt's world, as an [`AttemptsTransport`] hands it over.
#[derive(Clone)]
enum Outcome {
    /// The POST fails before any response exists.
    Unreachable(TransportError),
    /// A response with this status, whose body arrives as these chunks.
    Response(u16, Vec<Result<Vec<u8>, TransportError>>),
}

/// A transport that answers differently call by call, so a retry meets a
/// different world than the attempt before it. The last answer repeats once the
/// script runs out, so a test that forgot to script a retry sees that failure
/// again rather than a fixture error about an empty queue.
struct AttemptsTransport {
    script: Vec<Outcome>,
    calls: Mutex<usize>,
}

impl AttemptsTransport {
    fn new(script: Vec<Outcome>) -> Self {
        assert!(!script.is_empty(), "a transport needs something to answer with");
        Self { script, calls: Mutex::new(0) }
    }

    /// A complete answer, as one chunk.
    fn answered(content: &str) -> Outcome {
        Outcome::Response(200, complete(content))
    }

    fn calls(&self) -> usize {
        self.calls.lock().map(|calls| *calls).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Transport for AttemptsTransport {
    async fn post(&self, _: TransportRequest) -> Result<TransportResponse, TransportError> {
        let index = {
            let mut calls = self.calls.lock().expect("the call counter");
            let index = *calls;
            *calls += 1;
            index
        };
        let outcome = self
            .script
            .get(index)
            .or_else(|| self.script.last())
            .expect("at least one answer")
            .clone();
        match outcome {
            Outcome::Unreachable(error) => Err(error),
            Outcome::Response(status, chunks) => {
                Ok(TransportResponse { status, body: Box::pin(futures_util::stream::iter(chunks)) })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A sink that refuses every delta, standing in for a consumer that already
/// closed. `RecordingSink` only closes on a poisoned mutex, so it cannot model
/// this path.
struct ClosedSink;

#[async_trait::async_trait]
impl ModelStreamSink for ClosedSink {
    async fn on_event(&self, _event: ModelStreamEvent) -> Result<(), ModelStreamError> {
        Err(ModelStreamError::Closed)
    }
}

const TEST_KEY: &str = "sk-test-not-a-real-key";

fn config() -> OpenAiCompatConfig {
    OpenAiCompatConfig::new("https://api.example.com", "test-model")
        .with_provider_id("test-endpoint")
        .with_timeout_ms(5_000)
}

/// A provider over a fake transport, plus the handle to inspect what it saw.
fn provider_over(transport: FakeTransport) -> (OpenAiCompatProvider, Arc<FakeTransport>) {
    let transport = Arc::new(transport);
    let provider = OpenAiCompatProvider::new(
        config(),
        Arc::new(FixedSecret::new(TEST_KEY)),
        Arc::clone(&transport) as Arc<dyn Transport>,
    );
    (provider, transport)
}

/// A provider whose `GET /models` answers with `models_body`.
fn catalog_provider(models_body: &str) -> (OpenAiCompatProvider, Arc<FakeTransport>) {
    provider_over(FakeTransport::models(models_body))
}

fn request_with_budget(budget: ModelBudget) -> ModelRequest {
    let mut request = request(vec![user("hi")]);
    request.budget = budget;
    request
}

fn request(messages: Vec<ModelMessage>) -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from_string("s1"),
        turn_id: TurnId::new(),
        step_id: StepId::new(),
        context_revision: 0,
        messages,
        tool_manifest: ToolManifest::empty(),
        capabilities: vec![ModelCapability::new(ModelCapability::STREAMING)],
        response_format: None,
        model_route: None,
        budget: ModelBudget::default(),
    }
}

fn user(text: &str) -> ModelMessage {
    ModelMessage::text(MessageRole::User, text)
}

/// A registered tool, for building manifests.
fn definition(name: &str, description: &str, input_schema: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        source: ToolSource::Builtin,
        side_effect: ToolSideEffect::Read,
        capability_requirements: Vec::new(),
        idempotent: true,
        timeout_ms: None,
        input_schema,
    }
}

/// Wrap raw `data:` payload lines as an SSE body.
fn sse(lines: &[&str]) -> Vec<u8> {
    lines.iter().map(|line| format!("{line}\n\n")).collect::<String>().into_bytes()
}

/// A whole answer, as one chunk: content, `finish_reason`, and the sentinel.
fn complete(content: &str) -> Vec<Result<Vec<u8>, TransportError>> {
    let delta = format!(
        r#"data: {{"choices":[{{"delta":{{"content":"{content}"}},"finish_reason":null}}]}}"#
    );
    let body = sse(&[
        delta.as_str(),
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ]);
    vec![Ok(body)]
}

/// A connection that died before one byte of the answer arrived.
fn died_silently(cause: &str) -> Vec<Result<Vec<u8>, TransportError>> {
    vec![Err(TransportError::Io(cause.to_string()))]
}

/// A connection that delivered part of an answer and then died.
fn spoke_then_died(cause: &str) -> Vec<Result<Vec<u8>, TransportError>> {
    let body = sse(&[r#"data: {"choices":[{"delta":{"content":"half"},"finish_reason":null}]}"#]);
    vec![Ok(body), Err(TransportError::Io(cause.to_string()))]
}

/// A provider over an [`AttemptsTransport`], with an explicit retry budget.
fn provider_with_retries(
    transport: AttemptsTransport,
    retries: u32,
) -> (OpenAiCompatProvider, Arc<AttemptsTransport>) {
    let transport = Arc::new(transport);
    let provider = OpenAiCompatProvider::new(
        config().with_retries(retries),
        Arc::new(FixedSecret::new(TEST_KEY)),
        Arc::clone(&transport) as Arc<dyn Transport>,
    );
    (provider, transport)
}

// ---------------------------------------------------------------------------
// Request shaping
// ---------------------------------------------------------------------------

#[test]
fn chat_request_serializes_the_openai_shape() {
    let body = chat_request(&config(), &request(vec![user("hi")])).unwrap();
    let json = serde_json::to_value(&body).unwrap();

    assert_eq!(json["model"], "test-model");
    assert_eq!(json["stream"], true);
    assert_eq!(json["stream_options"]["include_usage"], true);
    assert_eq!(json["messages"][0]["role"], "user");
    assert_eq!(json["messages"][0]["content"], "hi");
    // No tools are offered, so the key must be absent rather than an empty list.
    assert!(json.get("tools").is_none());
    assert!(json.get("max_tokens").is_none());
}

#[test]
fn a_route_overrides_the_configured_model_and_a_budget_overrides_max_tokens() {
    let mut request = request(vec![user("hi")]);
    request.model_route = Some(ModelRoute::new(ModelTier::Main, "test-endpoint", "routed-model"));
    request.budget = ModelBudget { max_tokens: Some(64), ..ModelBudget::default() };

    let body = chat_request(&config(), &request).unwrap();
    assert_eq!(body.model, "routed-model");
    assert_eq!(body.max_tokens, Some(64));
}

#[test]
fn chat_request_sends_reasoning_effort_but_not_the_reservation_as_max_tokens() {
    let request = request_with_budget(ModelBudget {
        max_tokens: Some(262_144),
        reasoning_effort: Some(ReasoningEffort::Max),
        ..ModelBudget::default()
    });
    let wire = chat_request(&config(), &request).unwrap();
    assert_eq!(wire.max_tokens, Some(262_144));
    assert_eq!(wire.reasoning_effort, Some(ReasoningEffort::Max));
    assert_eq!(serde_json::to_value(&wire).unwrap()["reasoning_effort"], "max");

    // Nothing names an effort: the field is absent, not a fabricated default.
    let bare = request_with_budget(ModelBudget::default());
    let json = serde_json::to_value(chat_request(&config(), &bare).unwrap()).unwrap();
    assert!(json.get("reasoning_effort").is_none(), "{json}");
}

#[tokio::test]
async fn models_endpoint_maps_deepseek_capabilities() {
    let (provider, _) = catalog_provider(
        r#"{"data":[{"id":"deepseek-flash","context_window":1048576,"max_output_tokens":393216,"reasoning_efforts":["low","high","max"],"default_reasoning_effort":"high"}]}"#,
    );
    let resolved = provider.model_capabilities("deepseek-flash").await.unwrap();
    assert_eq!(resolved.capabilities.context_window_tokens, 1_048_576);
    assert_eq!(resolved.capabilities.max_output_tokens, 393_216);
    assert_eq!(
        resolved.capabilities.reasoning_efforts,
        vec![ReasoningEffort::Low, ReasoningEffort::High, ReasoningEffort::Max]
    );
    assert_eq!(resolved.capabilities.default_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(resolved.source, ModelCapabilitySource::Provider);
}

#[tokio::test]
async fn an_unusable_models_response_uses_the_bundled_catalog() {
    let (provider, _) = catalog_provider("not json");
    let resolved = provider.model_capabilities("deepseek-flash").await.unwrap();
    assert_eq!(resolved.capabilities.context_window_tokens, 1_048_576);
    assert_eq!(resolved.source, ModelCapabilitySource::Bundled);
}

#[tokio::test]
async fn an_unknown_model_with_an_unusable_answer_is_a_labelled_fallback() {
    let (provider, _) = catalog_provider(r#"{"data":[{"id":"other","context_window":1}]}"#);
    let resolved = provider.model_capabilities("unknown-model").await.unwrap();
    assert_eq!(
        resolved.capabilities,
        agent_model_openai::fallback_capabilities(),
        "no usable answer must not be read as a zero window"
    );
    assert_eq!(resolved.source, ModelCapabilitySource::Fallback);
}

#[tokio::test]
async fn a_discovered_capability_set_is_asked_for_once() {
    let (provider, transport) = catalog_provider(
        r#"{"data":[{"id":"m","context_window":1000,"max_output_tokens":100}]}"#,
    );
    let first: ModelCapabilities = provider.model_capabilities("m").await.unwrap().capabilities;
    let second = provider.model_capabilities("m").await.unwrap().capabilities;
    assert_eq!(first, second);
    assert_eq!(transport.get_count(), 1, "the second answer came from the cache");
    // The discovery really did go to the models endpoint.
    let seen = transport.requests();
    assert_eq!(seen[0].url, "https://api.example.com/models");
    assert_eq!(seen[0].bearer_token, TEST_KEY);
}

#[test]
fn tool_use_and_tool_result_map_onto_the_wire_roles() {
    let assistant = ModelMessage::new(
        MessageRole::Assistant,
        vec![
            ContentBlock::text("looking"),
            ContentBlock::tool_use("call_1", "fs.read", serde_json::json!({ "path": "a.txt" })),
        ],
    );
    let wire = to_wire_messages(&assistant);
    assert_eq!(wire.len(), 1);
    assert_eq!(wire[0].role, "assistant");
    assert_eq!(wire[0].content.as_deref(), Some("looking"));
    let calls = wire[0].tool_calls.as_ref().expect("tool calls");
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].kind, "function");
    // The dotted internal id is projected onto the wire charset on the way out.
    assert_eq!(calls[0].function.name, "fs_read");
    // Arguments travel as a JSON string, not as a nested object.
    assert_eq!(calls[0].function.arguments, r#"{"path":"a.txt"}"#);

    // Two results in one protocol message become two wire messages: the wire
    // format pairs each result with the call it answers.
    let results = ModelMessage::new(
        MessageRole::Tool,
        vec![
            ContentBlock::tool_result("call_1", "fs.read", "a", false),
            ContentBlock::tool_result("call_2", "fs.read", "boom", true),
        ],
    );
    let wire = to_wire_messages(&results);
    assert_eq!(wire.len(), 2);
    assert_eq!(wire[0].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(wire[0].name.as_deref(), Some("fs_read"));
    assert_eq!(wire[1].content.as_deref(), Some("boom"));
}

#[test]
fn an_assistant_turn_with_only_tool_calls_sends_an_explicit_null_content() {
    let message = ModelMessage::new(
        MessageRole::Assistant,
        vec![ContentBlock::tool_use("call_1", "fs.read", serde_json::json!({}))],
    );
    let json = serde_json::to_value(&to_wire_messages(&message)[0]).unwrap();

    // Present and null — never omitted, never an empty string. At least one
    // vendor types this field as required-but-nullable, so "absent" is a
    // different request from "null".
    assert!(json.get("content").is_some(), "content must be present: {json}");
    assert!(json["content"].is_null(), "content must be null: {json}");
    assert!(json.get("tool_calls").is_some());
}

#[test]
fn a_manifest_becomes_the_tools_array_in_manifest_order() {
    let mut request = request(vec![user("hi")]);
    request.tool_manifest = ToolManifest::from_definitions(&[
        definition("shell.run", "Run a command", serde_json::json!({ "type": "object" })),
        definition(
            "fs.read",
            "Read a file",
            serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        ),
    ]);

    let json = serde_json::to_value(chat_request(&config(), &request).unwrap()).unwrap();
    let tools = json["tools"].as_array().expect("tools");

    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["type"], "function");
    // Sorted by tool id, not by the order the definitions were collected in:
    // the same tool set must always produce the same request body.
    assert_eq!(tools[0]["function"]["name"], "fs_read");
    assert_eq!(tools[1]["function"]["name"], "shell_run");
    assert_eq!(tools[0]["function"]["description"], "Read a file");
    assert_eq!(tools[0]["function"]["parameters"]["properties"]["path"]["type"], "string");
}

#[test]
fn a_tool_without_a_description_omits_the_field_rather_than_sending_an_empty_string() {
    let mut request = request(vec![user("hi")]);
    request.tool_manifest =
        ToolManifest::from_definitions(&[definition("echo", "   ", serde_json::json!({}))]);

    let json = serde_json::to_value(chat_request(&config(), &request).unwrap()).unwrap();
    assert!(json["tools"][0]["function"].get("description").is_none(), "{json}");
    // The schema is always sent: omitting `parameters` would describe a tool
    // that takes no arguments, which is not the tool that was registered.
    assert_eq!(json["tools"][0]["function"]["parameters"], serde_json::json!({}));
}

#[test]
fn a_tool_id_is_projected_onto_the_charset_the_endpoint_accepts() {
    // Not cosmetic: an endpoint that validates the name rejects a dotted id
    // outright, which takes the whole request down with it.
    assert_eq!(to_wire_name("fs.read"), "fs_read");
    assert_eq!(to_wire_name("shell.run"), "shell_run");
    assert_eq!(to_wire_name("a-b_c.d"), "a-b_c_d");
    // A name that is already legal survives untouched, so internal ids and wire
    // names stay identical wherever they can.
    assert_eq!(to_wire_name("plain"), "plain");
    for id in ["fs.read", "shell.run", "mcp__server__tool"] {
        assert!(
            to_wire_name(id).chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{id} does not project onto a legal function name"
        );
    }
}

#[test]
fn two_tool_ids_that_share_a_wire_name_are_rejected_rather_than_silently_merged() {
    // `fs.read` and `fs_read` are distinct tools here and the same name on the
    // wire. Picking one would hand the model a call that the other answers, so
    // the request is refused instead of guessed at.
    let manifest = ToolManifest::from_definitions(&[
        definition("fs.read", "Read a file", serde_json::json!({})),
        definition("fs_read", "Read a file", serde_json::json!({})),
    ]);
    match to_wire_tools(&manifest) {
        Err(WireError::ToolNameCollision { first, second, wire_name }) => {
            assert_eq!(wire_name, "fs_read");
            assert_eq!((first.as_str(), second.as_str()), ("fs.read", "fs_read"));
        }
        other => panic!("expected a name collision, got {other:?}"),
    }
}

#[test]
fn a_name_collision_fails_the_request_instead_of_sending_a_renamed_tool() {
    let mut request = request(vec![user("hi")]);
    request.tool_manifest = ToolManifest::from_definitions(&[
        definition("fs.read", "Read a file", serde_json::json!({})),
        definition("fs_read", "Read a file", serde_json::json!({})),
    ]);
    assert!(matches!(chat_request(&config(), &request), Err(WireError::ToolNameCollision { .. })));
}

#[test]
fn the_same_tool_id_listed_twice_is_not_a_collision() {
    // The manifest is a set. The check compares ids, not insertions.
    let manifest = ToolManifest::from_definitions(&[
        definition("fs.read", "Read a file", serde_json::json!({})),
        definition("fs.read", "Read a file", serde_json::json!({})),
    ]);
    assert!(to_wire_tools(&manifest).is_ok());
}

#[test]
fn the_name_index_hands_back_the_internal_id_for_each_wire_name() {
    let manifest = ToolManifest::from_definitions(&[
        definition("fs.read", "Read", serde_json::json!({})),
        definition("shell.run", "Run", serde_json::json!({})),
    ]);
    let index = wire_name_index(&manifest);
    assert_eq!(index.get("fs_read").map(String::as_str), Some("fs.read"));
    assert_eq!(index.get("shell_run").map(String::as_str), Some("shell.run"));
    // A name the manifest never offered is the model inventing a tool. Absence
    // is how the adapter tells itself to leave that call alone.
    assert!(!index.contains_key("rm_rf"));
}

#[test]
fn every_name_that_leaves_for_the_endpoint_is_within_the_vendors_charset() {
    // The endpoint validates the function name wherever it appears — in
    // `tools[]`, on an assistant call, and on the result answering it — and
    // rejects the entire request if one is out of charset. So this walks the
    // serialized body rather than the typed one: the charset is a property of
    // the wire, and only the wire can be wrong about it.
    let mut request = request(vec![
        user("read the file"),
        ModelMessage::new(
            MessageRole::Assistant,
            vec![
                ContentBlock::text("on it"),
                ContentBlock::tool_use(
                    "call_1",
                    "fs.read",
                    serde_json::json!({ "path": "greeting.txt" }),
                ),
                ContentBlock::tool_use("call_2", "shell.run", serde_json::json!({ "cmd": "ls" })),
            ],
        ),
        ModelMessage::new(
            MessageRole::Tool,
            vec![
                ContentBlock::tool_result("call_1", "fs.read", "m3l0n", false),
                ContentBlock::tool_result("call_2", "shell.run", "boom", true),
            ],
        ),
    ]);
    request.tool_manifest = ToolManifest::from_definitions(&[
        definition("fs.read", "Read a file", serde_json::json!({ "type": "object" })),
        definition("shell.run", "Run a command", serde_json::json!({ "type": "object" })),
    ]);

    let json = serde_json::to_value(chat_request(&config(), &request).unwrap()).unwrap();

    let mut names: Vec<String> = json["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
        .collect();
    for message in json["messages"].as_array().unwrap() {
        if let Some(name) = message.get("name").and_then(|n| n.as_str()) {
            names.push(name.to_string());
        }
        for call in message.get("tool_calls").and_then(|c| c.as_array()).into_iter().flatten() {
            names.push(call["function"]["name"].as_str().unwrap().to_string());
        }
    }

    assert_eq!(names.len(), 6, "two offered, two called, two answered: {names:?}");
    for name in &names {
        assert!(
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "'{name}' is a name an endpoint that validates function names would reject"
        );
    }
    // The internal ids really did need the projection, and none of them leaked.
    assert!(names.contains(&"fs_read".to_string()), "{names:?}");
    assert!(names.contains(&"shell_run".to_string()), "{names:?}");
    assert!(!names.iter().any(|name| name.contains('.')), "{names:?}");
}

#[test]
fn response_format_maps_only_when_requested() {
    let mut request = request(vec![user("hi")]);
    assert!(chat_request(&config(), &request).unwrap().response_format.is_none());

    request.response_format = Some(ResponseFormat::JsonObject);
    let shape = chat_request(&config(), &request).unwrap().response_format.unwrap();
    assert_eq!(serde_json::to_value(&shape).unwrap()["type"], "json_object");

    request.response_format =
        Some(ResponseFormat::JsonSchema { schema: serde_json::json!({ "type": "object" }) });
    let shape = chat_request(&config(), &request).unwrap().response_format.unwrap();
    let json = serde_json::to_value(&shape).unwrap();
    assert_eq!(json["type"], "json_schema");
    assert_eq!(json["json_schema"]["name"], "response");
    assert_eq!(json["json_schema"]["schema"]["type"], "object");
}

#[test]
fn unknown_finish_reasons_pass_through_verbatim() {
    assert_eq!(normalize_finish_reason("stop"), FinishReason::new(FinishReason::STOP));
    assert_eq!(normalize_finish_reason("length"), FinishReason::new(FinishReason::LENGTH));
    assert_eq!(normalize_finish_reason("tool_calls"), FinishReason::new(FinishReason::TOOL_CALLS));
    assert_eq!(
        normalize_finish_reason("pause_turn").as_str(),
        "pause_turn",
        "a vendor-specific reason must not be collapsed"
    );
}

#[test]
fn a_chat_request_round_trips_through_json() {
    let body = chat_request(&config(), &request(vec![user("hi")])).unwrap();
    let json = serde_json::to_string(&body).unwrap();
    let decoded: ChatRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, body);
}

// ---------------------------------------------------------------------------
// SSE decoding
// ---------------------------------------------------------------------------

#[test]
fn sse_decoder_handles_split_boundaries_and_multi_line_data() {
    let mut decoder = SseDecoder::new();
    // A frame split mid-line yields nothing until the terminator arrives.
    assert!(decoder.push(b"data: {\"a\"").is_empty());
    let frames = decoder.push(b":1}\n\n");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "{\"a\":1}");

    // Several data lines join with a newline, as the spec requires.
    let frames = decoder.push(b"event: chunk\ndata: one\ndata: two\n\n");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("chunk"));
    assert_eq!(frames[0].data, "one\ntwo");
}

#[test]
fn sse_decoder_handles_crlf_comments_and_keep_alives() {
    let mut decoder = SseDecoder::new();
    // A comment frame (the usual keep-alive) yields nothing.
    assert!(decoder.push(b": keep-alive\n\n").is_empty());
    // CRLF terminators.
    let frames = decoder.push(b"data: [DONE]\r\n\r\n");
    assert_eq!(frames.len(), 1);
    assert!(frames[0].is_done());
}

#[test]
fn sse_decoder_flushes_a_frame_that_lost_its_blank_line() {
    let mut decoder = SseDecoder::new();
    // The `data:` line arrived and was terminated, but the blank line that ends
    // the block never followed.
    assert!(decoder.push(b"data: [DONE]\n").is_empty(), "no blank line yet");
    let frames = decoder.flush();
    assert_eq!(frames.len(), 1);
    assert!(frames[0].is_done());
    // A decoder already ended on a blank line has nothing left to flush.
    let mut decoder = SseDecoder::new();
    assert_eq!(decoder.push(b"data: [DONE]\n\n").len(), 1);
    assert!(decoder.flush().is_empty(), "flushing a drained decoder yields nothing");
}

#[test]
fn sse_decoder_discards_an_unterminated_trailing_line() {
    let mut decoder = SseDecoder::new();
    // A cut mid-line: the bytes are a prefix of a frame, not a frame. Salvaging
    // them would turn a truncation into a spurious protocol error.
    assert!(decoder.push(b"data: {\"a\":1").is_empty());
    assert!(decoder.flush().is_empty(), "an incomplete line is not a frame");

    // Complete lines ahead of the cut still flush.
    let mut decoder = SseDecoder::new();
    assert!(decoder.push(b"data: one\ndata: tw").is_empty());
    let frames = decoder.flush();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "one");
}

#[test]
fn a_chunk_split_inside_a_multi_byte_character_is_not_mangled() {
    // "中" is three bytes; split it across two chunks.
    let payload = "data: 中文\n\n".as_bytes().to_vec();
    let (head, tail) = payload.split_at(8);
    let mut decoder = SseDecoder::new();
    assert!(decoder.push(head).is_empty(), "an incomplete line must not decode");
    assert!(decoder.pending_bytes() > 0);
    let frames = decoder.push(tail);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].data, "中文");
}

#[test]
fn a_wire_chunk_ignores_fields_the_adapter_does_not_model() {
    // Real endpoints send ids, timestamps, and fingerprints. An unknown field
    // must not make the chunk undecodable.
    let chunk: WireChunk = serde_json::from_str(
        r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"m",
            "system_fingerprint":"fp_1",
            "choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
    )
    .unwrap();
    assert_eq!(chunk.choices.len(), 1);
    assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
}

// ---------------------------------------------------------------------------
// Accumulation
// ---------------------------------------------------------------------------

#[test]
fn accumulator_collects_text_reasoning_usage_and_finish_reason() {
    let mut accumulator = StreamAccumulator::new();
    let chunks = [
        r#"{"choices":[{"delta":{"reasoning_content":"think"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"content":"hel"},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9,"prompt_cache_hit_tokens":4,"prompt_cache_miss_tokens":3,"completion_tokens_details":{"reasoning_tokens":1}}}"#,
    ];
    let mut events = 0;
    for chunk in chunks {
        events += accumulator.absorb(chunk).unwrap().len();
    }
    assert_eq!(events, 3);
    assert_eq!(accumulator.deltas(), 3);
    assert_eq!(accumulator.reasoning(), "think");

    let response = accumulator.finish();
    assert_eq!(response.text_content(), "hello");
    assert_eq!(response.finish_reason, FinishReason::new(FinishReason::STOP));
    let usage = response.usage.expect("usage");
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(usage.output_tokens, Some(2));
    assert_eq!(usage.total_tokens, Some(9));
    assert_eq!(usage.cache_hit_tokens, Some(4));
    assert_eq!(usage.cache_miss_tokens, Some(3));
    assert_eq!(usage.reasoning_tokens, Some(1));
    // Reasoning is live-only: it must not settle into the response.
    assert_eq!(response.content_blocks.len(), 1);
}

#[test]
fn a_usage_without_cache_fields_reports_none_rather_than_zero() {
    let usage: Usage = serde_json::from_str::<WireUsage>(
        r#"{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}"#,
    )
    .unwrap()
    .into();
    assert_eq!(usage.cache_hit_tokens, None);
    assert_eq!(usage.cache_miss_tokens, None);
    assert_eq!(usage.reasoning_tokens, None);
}

#[test]
fn openai_style_cached_tokens_fill_the_cache_hit_count() {
    let usage: Usage = serde_json::from_str::<WireUsage>(
        r#"{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":6}}"#,
    )
    .unwrap()
    .into();
    assert_eq!(usage.cache_hit_tokens, Some(6));
    assert_eq!(usage.cache_miss_tokens, None);
}

#[test]
fn accumulator_assembles_a_tool_call_from_its_fragments() {
    let mut accumulator = StreamAccumulator::new();
    let chunks = [
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"fs.read"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ];
    let mut emitted = Vec::new();
    for chunk in chunks {
        emitted.extend(accumulator.absorb(chunk).unwrap());
    }
    assert_eq!(emitted.len(), 3, "each fragment is forwarded as a live delta");

    let response = accumulator.finish();
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_call_id, "call_1");
    assert_eq!(calls[0].tool_name, "fs.read");
    assert_eq!(calls[0].arguments["path"], "a.txt");
    assert_eq!(response.finish_reason, FinishReason::new(FinishReason::TOOL_CALLS));
    assert!(!response.is_final());
}

#[test]
fn parallel_tool_calls_keep_their_first_seen_order() {
    let mut accumulator = StreamAccumulator::new();
    accumulator
        .absorb(
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":1,"id":"call_b","function":{"name":"second","arguments":"{}"}},
                {"index":0,"id":"call_a","function":{"name":"first","arguments":"{}"}}
            ]},"finish_reason":null}]}"#,
        )
        .unwrap();
    let calls = accumulator.finish().tool_calls();
    assert_eq!(calls[0].tool_name, "second");
    assert_eq!(calls[1].tool_name, "first");
}

#[test]
fn a_stream_without_a_finish_reason_settles_as_unknown() {
    let mut accumulator = StreamAccumulator::new();
    accumulator.absorb(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
    let response = accumulator.finish();
    assert_eq!(response.finish_reason, FinishReason::new(FinishReason::UNKNOWN));
    assert!(response.is_final());
}

#[test]
fn a_malformed_chunk_is_an_error_rather_than_silent_data_loss() {
    let mut accumulator = StreamAccumulator::new();
    assert!(accumulator.absorb("not json").is_err());
    let many = r#"{"choices":[{"delta":{}},{"delta":{}}]}"#;
    assert!(accumulator.absorb(many).is_err(), "more than one choice is unexpected");
}

// ---------------------------------------------------------------------------
// Provider behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_streams_deltas_to_the_sink_and_settles_the_response() {
    let body = sse(&[
        r#"data: {"model":"test-model","choices":[{"delta":{"content":"he"},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"content":"llo"},"finish_reason":"stop"}]}"#,
        r#"data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
        "data: [DONE]",
    ]);
    let (provider, _) = provider_over(FakeTransport::raw(body));
    let sink = RecordingSink::new();

    let response = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();

    assert_eq!(response.text_content(), "hello");
    assert_eq!(response.finish_reason, FinishReason::new(FinishReason::STOP));
    assert_eq!(response.usage.unwrap().total_tokens, Some(4));
    assert_eq!(response.provider_metadata["provider"], "test-endpoint");
    assert_eq!(response.provider_metadata["model"], "test-model");
    assert_eq!(sink.events().len(), 2, "usage and [DONE] are not deltas");
}

#[tokio::test]
async fn a_tool_call_that_comes_back_is_restored_to_its_internal_id() {
    let body = sse(&[
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"fs_read"}}]},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"a.txt\"}"}}]},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ]);
    let (provider, _) = provider_over(FakeTransport::raw(body));
    let mut request = request(vec![user("hi")]);
    request.tool_manifest = ToolManifest::from_definitions(&[definition(
        "fs.read",
        "Read a file",
        serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
    )]);

    let response = provider.stream(&request, &RecordingSink::new()).await.unwrap();

    let calls = response.tool_calls();
    assert_eq!(calls.len(), 1);
    // The wire name is an adapter-internal detail: policy, events, and the
    // gateway all key on the internal id, so it never escapes this boundary.
    assert_eq!(calls[0].tool_name, "fs.read");
    assert_eq!(calls[0].arguments["path"], "a.txt");
}

#[tokio::test]
async fn a_tool_name_the_model_invented_is_left_alone() {
    // Renaming an unknown call onto a real tool would be the adapter deciding
    // what the model meant. The name passes through untouched so the gateway
    // can answer with a real "unknown tool" result.
    let body = sse(&[
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"delete_everything","arguments":"{}"}}]},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ]);
    let (provider, _) = provider_over(FakeTransport::raw(body));
    let mut request = request(vec![user("hi")]);
    request.tool_manifest = ToolManifest::from_definitions(&[definition(
        "fs.read",
        "Read a file",
        serde_json::json!({}),
    )]);

    let response = provider.stream(&request, &RecordingSink::new()).await.unwrap();
    assert_eq!(response.tool_calls()[0].tool_name, "delete_everything");
}

#[tokio::test]
async fn a_manifest_that_cannot_be_expressed_is_refused_before_anything_is_sent() {
    let (provider, transport) = provider_over(FakeTransport::body("data: [DONE]\n\n"));
    let mut request = request(vec![user("hi")]);
    request.tool_manifest = ToolManifest::from_definitions(&[
        definition("fs.read", "Read a file", serde_json::json!({})),
        definition("fs_read", "Read a file", serde_json::json!({})),
    ]);

    let error = provider.stream(&request, &RecordingSink::new()).await.unwrap_err();
    assert_eq!(error.kind(), "protocol", "{error:?}");
    assert!(transport.requests().is_empty(), "a request that cannot be expressed is not sent");
}

#[tokio::test]
async fn provider_chunks_split_mid_event_are_reassembled() {
    // The same body, delivered in awkward pieces: mid-line, mid-JSON, and
    // across a multi-byte character.
    let body = sse(&[
        r#"data: {"choices":[{"delta":{"content":"中"},"finish_reason":null}]}"#,
        "data: [DONE]",
    ]);
    let pieces: Vec<Vec<u8>> = body.chunks(7).map(|c| c.to_vec()).collect();
    let (provider, _) = provider_over(FakeTransport::chunked(pieces));
    let sink = RecordingSink::new();

    let response = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();
    assert_eq!(response.text_content(), "中");
    assert_eq!(sink.events().len(), 1);
}

#[tokio::test]
async fn provider_posts_to_the_configured_endpoint_with_a_bearer_token() {
    let (provider, transport) = provider_over(FakeTransport::body("data: [DONE]\n\n"));
    let sink = RecordingSink::new();

    provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://api.example.com/chat/completions");
    assert_eq!(requests[0].bearer_token, TEST_KEY);
    assert_eq!(requests[0].timeout_ms, 5_000);
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn provider_reports_a_vendors_error_message_on_a_non_200() {
    let error_body = r#"{"error":{"message":"Invalid model","type":"invalid_request_error"}}"#;
    let (provider, _) = provider_over(FakeTransport::status(400, error_body));
    let sink = RecordingSink::new();

    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    match error {
        ModelProviderError::Http { status, body } => {
            assert_eq!(status, 400);
            assert_eq!(body, "Invalid model");
        }
        other => panic!("expected an http error, got {other:?}"),
    }
    assert!(sink.events().is_empty(), "a failed request emits no deltas");
}

#[tokio::test]
async fn provider_never_settles_a_clean_end_without_an_end_signal() {
    // The connection closed cleanly with neither `[DONE]` nor a `finish_reason`:
    // there is no way to tell a finished answer from a truncated one, so it is
    // reported as an interruption.
    let body = sse(&[r#"data: {"choices":[{"delta":{"content":"half"},"finish_reason":null}]}"#]);
    let (provider, _) = provider_over(FakeTransport::raw(body));
    let sink = RecordingSink::new();

    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    match error {
        // No transport failed here, so there is no cause to carry.
        ModelProviderError::StreamInterrupted { received_deltas, cause } => {
            assert_eq!(received_deltas, 1);
            assert!(cause.is_none(), "a clean EOF has no transport cause: {cause:?}");
        }
        other => panic!("expected an interrupted stream, got {other:?}"),
    }
    assert_eq!(sink.events().len(), 1, "the delta that did arrive stays live-only");
}

#[tokio::test]
async fn provider_settles_on_a_finish_reason_without_a_done_sentinel() {
    // A complete answer whose endpoint closed without sending `[DONE]`. The
    // `finish_reason` is the protocol's own end-of-answer signal, so the answer
    // must settle rather than be thrown away as a truncation.
    let body = sse(&[
        r#"data: {"choices":[{"delta":{"content":"done"},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let (provider, _) = provider_over(FakeTransport::raw(body));
    let sink = RecordingSink::new();

    let response = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();
    assert_eq!(response.text_content(), "done");
    assert_eq!(response.finish_reason, FinishReason::new(FinishReason::STOP));
}

#[tokio::test]
async fn provider_flushes_a_final_frame_that_lost_its_blank_line() {
    // The endpoint closed right after the last `data:` line, without the blank
    // line that ends an SSE block. That frame is complete — only its terminator
    // is missing — so it must still settle the attempt.
    let body =
        concat!(r#"data: {"choices":[{"delta":{"content":"tail"},"finish_reason":"stop"}]}"#, "\n");
    let (provider, _) = provider_over(FakeTransport::raw(body.as_bytes().to_vec()));
    let sink = RecordingSink::new();

    let response = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();
    assert_eq!(response.text_content(), "tail");
    assert_eq!(response.finish_reason, FinishReason::new(FinishReason::STOP));

    // The same, but the flushed frame is the `[DONE]` sentinel itself.
    let body = "data: [DONE]\n";
    let (provider, _) = provider_over(FakeTransport::raw(body.as_bytes().to_vec()));
    provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();
}

#[tokio::test]
async fn provider_keeps_a_timeout_and_a_connection_failure_distinct() {
    let sink = RecordingSink::new();

    let (provider, _) = provider_over(FakeTransport::unreachable(TransportError::Timeout));
    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    assert!(matches!(error, ModelProviderError::Timeout { after_ms: 5_000 }), "{error:?}");

    let (provider, _) = provider_over(FakeTransport::unreachable(TransportError::Connect(
        "dns failure".to_string(),
    )));
    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    assert_eq!(error.kind(), "unavailable");
}

#[tokio::test]
async fn provider_keeps_a_mid_stream_transport_cause() {
    // A connection that dies mid-answer keeps the transport's own text, which is
    // the only clue to why the stream stopped.
    let delta = r#"data: {"choices":[{"delta":{"content":"half"},"finish_reason":null}]}"#;
    let (provider, _) = provider_over(FakeTransport::interrupted(
        &format!("{delta}\n\n"),
        TransportError::Io("connection reset by peer".to_string()),
    ));
    let sink = RecordingSink::new();

    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    match error {
        ModelProviderError::StreamInterrupted { received_deltas, cause } => {
            assert_eq!(received_deltas, 1);
            assert_eq!(cause.as_deref(), Some("connection reset by peer"));
        }
        other => panic!("expected an interrupted stream, got {other:?}"),
    }
}

#[tokio::test]
async fn provider_keeps_a_mid_stream_failure_distinct_from_a_clean_end() {
    let sink = RecordingSink::new();
    let delta = r#"data: {"choices":[{"delta":{"content":"half"},"finish_reason":null}]}"#;
    let body = format!("{delta}\n\n");

    // The connection dropped: an interruption, not a settled answer.
    let (provider, _) = provider_over(FakeTransport::interrupted(
        &body,
        TransportError::Connect("connection reset".to_string()),
    ));
    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    assert!(
        matches!(error, ModelProviderError::StreamInterrupted { received_deltas: 1, .. }),
        "{error:?}"
    );
    // The transport's own text survives the collapse into an interruption.
    assert!(format!("{error}").contains("connection reset"), "the cause must be kept: {error}");

    // The endpoint stopped answering mid-stream: the timeout keeps its identity
    // rather than degrading into a generic interruption.
    let (provider, _) = provider_over(FakeTransport::interrupted(&body, TransportError::Timeout));
    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    assert!(matches!(error, ModelProviderError::Timeout { after_ms: 5_000 }), "{error:?}");
}

#[tokio::test]
async fn a_sink_that_stops_accepting_is_a_cancellation_not_an_outage() {
    // The endpoint is reachable and answering; the *consumer* ended the attempt.
    // Reporting that as `Unavailable` would call a stopped listener an
    // unreachable backend and end the turn as a fatal model failure.
    let body = sse(&[r#"data: {"choices":[{"delta":{"content":"half"},"finish_reason":null}]}"#]);
    let (provider, _) = provider_over(FakeTransport::raw(body));

    let error = provider.stream(&request(vec![user("hi")]), &ClosedSink).await.unwrap_err();
    assert_eq!(error.kind(), "cancelled");
    assert!(error.is_cancelled(), "a stopped sink ends the attempt by cancellation: {error:?}");
}

#[tokio::test]
async fn a_missing_credential_fails_closed_without_revealing_anything() {
    let provider = OpenAiCompatProvider::new(
        config(),
        Arc::new(MissingSecret),
        Arc::new(FakeTransport::body("data: [DONE]\n\n")),
    );
    let sink = RecordingSink::new();

    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();
    match error {
        ModelProviderError::Unavailable(message) => {
            assert!(message.contains("AGENT_MODEL_API_KEY"), "names the reference: {message}");
            assert!(message.contains("env"), "names the source: {message}");
            assert!(!message.contains(TEST_KEY), "must not carry a credential: {message}");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
}

#[test]
fn a_transport_request_debug_redacts_the_credential() {
    let request = TransportRequest {
        url: "https://api.example.com/chat/completions".to_string(),
        bearer_token: TEST_KEY.to_string(),
        body: b"{}".to_vec(),
        timeout_ms: 1_000,
    };
    let debug = format!("{request:?}");
    assert!(!debug.contains(TEST_KEY), "a transport request must be safe to log: {debug}");
    assert!(debug.contains("***"));
}

#[test]
fn the_adapter_declares_only_what_it_implements() {
    let (provider, _) = provider_over(FakeTransport::body(""));
    let capabilities: Vec<String> =
        provider.capabilities().iter().map(|c| c.as_str().to_string()).collect();
    assert!(capabilities.contains(&ModelCapability::STREAMING.to_string()));
    assert!(capabilities.contains(&ModelCapability::TOKEN_USAGE.to_string()));
    // It sends tool schemas and reassembles tool calls from the stream, so it
    // can honestly claim these.
    assert!(capabilities.contains(&ModelCapability::TOOL_CALL.to_string()));
    assert!(capabilities.contains(&ModelCapability::PARALLEL_TOOL_CALL.to_string()));
    // Still not claimed: reasoning is accumulated but never round-tripped back
    // as a content block, so promising it would be a lie.
    assert!(!capabilities.contains(&ModelCapability::REASONING.to_string()));
    assert_eq!(provider.provider_id(), "test-endpoint");
}

#[test]
fn the_endpoint_url_tolerates_a_trailing_slash() {
    let config = OpenAiCompatConfig::new("https://api.example.com/", "m");
    assert_eq!(config.chat_completions_url(), "https://api.example.com/chat/completions");
    let prefixed = OpenAiCompatConfig::new("https://api.example.com/v1", "m");
    assert_eq!(prefixed.chat_completions_url(), "https://api.example.com/v1/chat/completions");
}

#[test]
fn the_default_config_names_the_credential_instead_of_holding_it() {
    let config = OpenAiCompatConfig::new("https://api.example.com", "m");
    assert_eq!(config.provider_id, agent_model_openai::DEFAULT_PROVIDER_ID);
    assert_eq!(config.timeout_ms, agent_model_openai::DEFAULT_TIMEOUT_MS);
    assert_eq!(config.retries, agent_model_openai::DEFAULT_RETRIES);
    assert_eq!(config.api_key.source, "env");
    assert_eq!(config.api_key.name, agent_model_openai::ENV_API_KEY);
    // The credential is a reference: there is nowhere in this struct to put a
    // value, which is the point.
    assert_eq!(config.api_key.name, "AGENT_MODEL_API_KEY");
}

// ---------------------------------------------------------------------------
// Retries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_connection_that_never_opened_is_asked_again() {
    // The transient failure that motivated the budget: a connection that was
    // refused once and would have answered the second time.
    let transport = AttemptsTransport::new(vec![
        Outcome::Unreachable(TransportError::Connect("connection refused".to_string())),
        AttemptsTransport::answered("hello"),
    ]);
    let (provider, transport) = provider_with_retries(transport, 2);
    let sink = RecordingSink::new();

    let response = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();

    assert_eq!(response.text_content(), "hello");
    assert_eq!(transport.calls(), 2, "the second attempt is the one that answered");
    assert_eq!(sink.events().len(), 1, "only the attempt that answered reached the sink");
}

#[tokio::test]
async fn a_throttled_endpoint_is_asked_again() {
    // 429 is the endpoint saying "ask again", so asking again is the whole fix.
    let transport = AttemptsTransport::new(vec![
        Outcome::Response(429, vec![Ok(b"slow down".to_vec())]),
        AttemptsTransport::answered("hello"),
    ]);
    let (provider, transport) = provider_with_retries(transport, 2);

    let response =
        provider.stream(&request(vec![user("hi")]), &RecordingSink::new()).await.unwrap();

    assert_eq!(response.text_content(), "hello");
    assert_eq!(transport.calls(), 2);
}

#[tokio::test]
async fn a_request_the_endpoint_rejected_is_not_asked_again() {
    // A 4xx is about the request. Repeating it repeats the rejection, and the
    // attempt it wastes is one the caller did not agree to.
    let transport = AttemptsTransport::new(vec![
        Outcome::Response(400, vec![Ok(br#"{"error":{"message":"Invalid model"}}"#.to_vec())]),
        AttemptsTransport::answered("hello"),
    ]);
    let (provider, transport) = provider_with_retries(transport, 2);

    let error =
        provider.stream(&request(vec![user("hi")]), &RecordingSink::new()).await.unwrap_err();

    assert!(matches!(error, ModelProviderError::Http { status: 400, .. }), "{error:?}");
    assert_eq!(transport.calls(), 1, "a rejection that names the request is final");
}

#[tokio::test]
async fn a_stream_that_died_before_saying_anything_is_asked_again() {
    // Nothing had reached the sink, so a second attempt shows the consumer
    // nothing it has already seen.
    let transport = AttemptsTransport::new(vec![
        Outcome::Response(200, died_silently("connection reset")),
        AttemptsTransport::answered("hello"),
    ]);
    let (provider, transport) = provider_with_retries(transport, 2);
    let sink = RecordingSink::new();

    let response = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap();

    assert_eq!(response.text_content(), "hello");
    assert_eq!(transport.calls(), 2);
    assert_eq!(sink.events().len(), 1);
}

#[tokio::test]
async fn a_stream_that_already_reached_the_sink_is_not_asked_again() {
    // The deltas that arrived are live-only and cannot be retracted, so a
    // second attempt would show the consumer the same beginning twice. The
    // interruption is reported instead, delta count and all.
    let transport = AttemptsTransport::new(vec![
        Outcome::Response(200, spoke_then_died("connection reset")),
        AttemptsTransport::answered("hello"),
    ]);
    let (provider, transport) = provider_with_retries(transport, 2);
    let sink = RecordingSink::new();

    let error = provider.stream(&request(vec![user("hi")]), &sink).await.unwrap_err();

    match error {
        ModelProviderError::StreamInterrupted { received_deltas, cause } => {
            assert_eq!(received_deltas, 1);
            assert_eq!(cause.as_deref(), Some("connection reset"));
        }
        other => panic!("expected an interrupted stream, got {other:?}"),
    }
    assert_eq!(transport.calls(), 1, "a stream the sink has seen part of is not repeated");
}

#[tokio::test]
async fn the_retry_budget_is_finite_and_the_last_failure_is_the_one_reported() {
    // The budget is spent, not stretched: three calls for a budget of two, and
    // the error the caller gets is the one from the final attempt.
    let transport = AttemptsTransport::new(vec![
        Outcome::Response(503, vec![Ok(b"unwell".to_vec())]),
        Outcome::Response(503, vec![Ok(b"unwell".to_vec())]),
        Outcome::Response(503, vec![Ok(b"unwell".to_vec())]),
    ]);
    let (provider, transport) = provider_with_retries(transport, 2);

    let error =
        provider.stream(&request(vec![user("hi")]), &RecordingSink::new()).await.unwrap_err();

    assert!(matches!(error, ModelProviderError::Http { status: 503, .. }), "{error:?}");
    assert_eq!(transport.calls(), 3, "one attempt plus two retries");
}

#[tokio::test]
async fn a_retry_budget_of_zero_means_one_call() {
    let transport = AttemptsTransport::new(vec![
        Outcome::Response(429, vec![Ok(b"slow down".to_vec())]),
        AttemptsTransport::answered("hello"),
    ]);
    let (provider, transport) = provider_with_retries(transport, 0);

    let error =
        provider.stream(&request(vec![user("hi")]), &RecordingSink::new()).await.unwrap_err();

    assert!(matches!(error, ModelProviderError::Http { status: 429, .. }), "{error:?}");
    assert_eq!(transport.calls(), 1);
}

#[tokio::test]
async fn a_credential_that_cannot_be_resolved_is_never_asked_again() {
    // Resolution and encoding happen before the first attempt precisely so this
    // failure cannot be mistaken for a transient one: no POST was made, and a
    // second POST would not make one happen.
    let transport = Arc::new(AttemptsTransport::new(vec![AttemptsTransport::answered("hello")]));
    let provider = OpenAiCompatProvider::new(
        config().with_retries(2),
        Arc::new(MissingSecret),
        Arc::clone(&transport) as Arc<dyn Transport>,
    );

    let error =
        provider.stream(&request(vec![user("hi")]), &RecordingSink::new()).await.unwrap_err();

    assert_eq!(error.kind(), "unavailable", "{error:?}");
    assert_eq!(transport.calls(), 0, "a missing credential is not a connection problem");
}
