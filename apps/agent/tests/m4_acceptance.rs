//! M4 acceptance: the assembled stack, end to end.
//!
//! These do not test a part, and they do not test a second assembly either: the
//! stack they drive is the one `apps/agent` runs, built by the same
//! `agent::stack::assemble` the binary calls — the real JSONL store, the
//! projection-backed context engine, the guarded tool gateway, the real plugin
//! registry on the real tool surface, the real decision broker in shadow mode.
//! They drive turns through that stack's Scheduler and read the durable log
//! back. A change to the product's wiring is therefore a change to this suite's
//! subject, and the suite goes red.
//!
//! The one seam that is faked is the network: [`ScriptedTransport`] stands in
//! for the TCP connection and replays bytes. Everything above it is real, which
//! is the point — the request that reaches the wire, the SSE that comes back,
//! and the events that land in the log are all produced by production code.
//! (`agent-model-openai/tests/live_endpoint.rs` is the same path against a real
//! endpoint, ignored because it needs a credential.)
//!
//! The acceptance criteria these cover, and where the rest live:
//!
//! | M4 criterion | here | elsewhere |
//! |---|---|---|
//! | FakeModel contract green | parity of the same turn's log | `agent-testkit/tests/fakes.rs` |
//! | model → tool → model → answer | `a_real_adapter_drives_model_tool_model_to_an_answer` | `live_endpoint.rs` (ignored) |
//! | history derives from the durable projection only | the rebuilt view after each turn | `agent-context/tests/model_context_acceptance.rs` |
//! | an interrupted stream writes no half response | `an_interrupted_stream_leaves_no_half_response` | `agent-loop` unit tests |
//! | failure/retry/cancel become attempt facts | `a_provider_failure_becomes_an_attempt_fact` | `agent-loop` unit tests (cancel) |
//! | a shadow decision does not touch the main chain | `the_shadow_decision_changes_nothing_the_model_sees` | `agent-loop` + `agent-decision` |

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile, UserConfig};
use agent_context::ProjectionContextEngine;
use agent_kernel::scheduler::RunReport;
use agent_model_openai::{
    OpenAiCompatConfig, OpenAiCompatProvider, Transport, TransportError, TransportRequest,
    TransportResponse, to_wire_name,
};
use agent_ports::agent_loop::StopReason;
use agent_ports::context_engine::ContextEngine;
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_protocol::events::{Actor, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::model::{MessageRole, ModelMessage};
use agent_testkit::FixedSecret;
use chrono::Utc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The one fake: bytes on a wire
// ---------------------------------------------------------------------------

/// One scripted HTTP response.
enum Scripted {
    /// A 200 whose whole body arrives, then the connection ends.
    Ok(String),
    /// A 200 whose body arrives and then the connection drops — the transport
    /// errors after the bytes, which is what a mid-answer disconnect looks like.
    Cut(String, TransportError),
    /// A non-200 status with a body.
    Status(u16, String),
}

impl Scripted {
    fn cut(body: &str) -> Self {
        Self::Cut(body.to_string(), TransportError::Connect("connection reset".to_string()))
    }
}

/// A transport that replays `responses` in order and records what it was asked
/// to send. Running out of scripted responses is an error, not a hang: a test
/// that expects two calls and gets three should fail loudly.
struct ScriptedTransport {
    responses: Mutex<Vec<Scripted>>,
    posts: Mutex<Vec<TransportRequest>>,
}

impl ScriptedTransport {
    fn new(responses: Vec<Scripted>) -> Self {
        Self { responses: Mutex::new(responses), posts: Mutex::new(Vec::new()) }
    }

    /// Every request body this transport was handed, as text.
    fn bodies(&self) -> Vec<String> {
        self.posts
            .lock()
            .map(|posts| {
                posts.iter().map(|p| String::from_utf8_lossy(&p.body).to_string()).collect()
            })
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Transport for ScriptedTransport {
    async fn post(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        if let Ok(mut posts) = self.posts.lock() {
            posts.push(request);
        }
        let next = self
            .responses
            .lock()
            .ok()
            .and_then(|mut queue| if queue.is_empty() { None } else { Some(queue.remove(0)) });
        let Some(scripted) = next else {
            return Err(TransportError::Connect("no scripted response left".to_string()));
        };
        let chunks: Vec<Result<Vec<u8>, TransportError>> = match scripted {
            Scripted::Ok(body) => vec![Ok(body.into_bytes())],
            Scripted::Cut(body, error) => vec![Ok(body.into_bytes()), Err(error)],
            Scripted::Status(status, body) => {
                return Ok(TransportResponse {
                    status,
                    body: Box::pin(futures_util::stream::iter(vec![Ok(body.into_bytes())])),
                });
            }
        };
        Ok(TransportResponse { status: 200, body: Box::pin(futures_util::stream::iter(chunks)) })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// An SSE body from raw `data:` payload lines.
fn sse(lines: &[&str]) -> String {
    lines.iter().map(|line| format!("{line}\n\n")).collect()
}

/// A streamed tool call, as an endpoint would send it.
fn tool_call_body(call_id: &str, tool: &str, arguments: serde_json::Value) -> String {
    sse(&[
        &format!(
            r#"data: {{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"{call_id}","function":{{"name":"{tool}"}}}}]}},"finish_reason":null}}]}}"#
        ),
        &format!(
            r#"data: {{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":{}}}}}]}},"finish_reason":null}}]}}"#,
            serde_json::to_string(&arguments.to_string()).expect("arguments as a JSON string")
        ),
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ])
}

/// A streamed text answer.
fn answer_body(text: &str) -> String {
    sse(&[
        &format!(
            r#"data: {{"choices":[{{"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#
        ),
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ])
}

fn scripted_provider(transport: Arc<ScriptedTransport>) -> Arc<OpenAiCompatProvider> {
    let config = OpenAiCompatConfig::new("https://scripted.example.com", "scripted-model")
        .with_provider_id("scripted-endpoint")
        .with_timeout_ms(5_000);
    Arc::new(OpenAiCompatProvider::new(
        config,
        Arc::new(FixedSecret::new("sk-test-not-a-real-key")),
        transport as Arc<dyn Transport>,
    ))
}

/// The route this harness's provider is registered under — the same shape
/// `apps/agent` derives from the provider it selected.
fn route() -> ModelRoute {
    ModelRoute::new(ModelTier::Main, "scripted-endpoint", "scripted-model")
}

/// A session assembled the way `apps/agent` assembles one, over a scripted
/// wire.
///
/// "The way it assembles" is not a copy of the assembly: [`stack::assemble`] is
/// the function the binary itself calls, so this harness gets the real store,
/// the real file boundary, the real policy, the real tool surface and the real
/// kernel services — and a change to any of them fails these tests instead of
/// passing unnoticed. What the harness supplies is the two things the process
/// environment would otherwise supply: where the run's files live, and the
/// model, which here is a scripted transport rather than an endpoint.
struct Harness {
    stack: stack::Runtime,
    session_id: SessionId,
    transport: Arc<ScriptedTransport>,
    /// The workspace the tools are confined to, kept alive for the test. Shared
    /// when two harnesses must run over the same workspace — the run's
    /// instructions name it, so two different roots would be two different
    /// prompts.
    workspace: Arc<TempDir>,
    data_dir: TempDir,
}

/// Build a harness. `shadow` off means the loop runs with no decision advisor at
/// all, which is the baseline a shadow run is compared against.
async fn harness(responses: Vec<Scripted>, shadow: bool) -> Harness {
    harness_in(Arc::new(tempfile::tempdir().expect("temp workspace")), responses, shadow).await
}

/// A harness over a workspace the caller already owns, so two runs can share
/// one root.
async fn harness_in(workspace: Arc<TempDir>, responses: Vec<Scripted>, shadow: bool) -> Harness {
    harness_with(workspace, responses, shadow, default_profile()).await
}

/// A harness over an explicit profile, for tests that need a budget the
/// defaults do not carry.
async fn harness_with(
    workspace: Arc<TempDir>,
    responses: Vec<Scripted>,
    shadow: bool,
    resolved: ResolvedProfile,
) -> Harness {
    let data_dir = tempfile::tempdir().expect("temp data dir");

    let transport = Arc::new(ScriptedTransport::new(responses));
    let stack = stack::assemble(stack::Inputs {
        session: SessionId::from_string("m4"),
        workspace_root: workspace.path().to_path_buf(),
        data_dir: data_dir.path().to_path_buf(),
        resolved,
        model: Some(SelectedProvider {
            provider: scripted_provider(transport.clone()),
            route: route(),
        }),
        decisions: if shadow { Decisions::FromConfiguration } else { Decisions::Unobserved },
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: true,
    })
    .await
    .expect("the assembly the binary runs");

    Harness { stack, session_id: SessionId::from_string("m4"), transport, workspace, data_dir }
}

/// The configuration a run in a directory with no configuration has.
///
/// The binary reads three layers — managed policy, user configuration, project
/// configuration — off the environment and the disk. A test can do neither: this
/// edition makes `set_var` unsafe, and the environment belongs to the whole
/// process. So it states the same input the loader would have produced, through
/// the same `resolve`, and the assembly below is the same one either way.
fn default_profile() -> ResolvedProfile {
    agent_config::resolve(Layers::default()).expect("the defaults resolve")
}

impl Harness {
    /// Write a file into the workspace the tools are confined to.
    fn seed_file(&self, name: &str, contents: &str) {
        std::fs::write(self.workspace.path().join(name), contents).expect("seed a workspace file");
    }

    /// Submit `text` as the next turn's input and drive it.
    async fn ask(&self, text: &str) -> RunReport {
        self.stack
            .scheduler
            .inbox()
            .append(
                &self.session_id,
                InboxKind::NextTurn,
                serde_json::json!({ "text": text }),
                0,
                Actor::User,
            )
            .await
            .expect("append input");
        self.stack
            .scheduler
            .run_pending(&self.session_id, Utc::now())
            .await
            .expect("run pending")
            .expect("a turn ran")
    }

    /// The durable log, in order.
    async fn log(&self) -> Vec<StoredEvent> {
        self.stack
            .store
            .read_stream(&self.session_id, EventSeq::ZERO, usize::MAX)
            .await
            .expect("read log")
    }

    /// The durable event types, in order.
    async fn types(&self) -> Vec<String> {
        self.log().await.into_iter().map(|e| e.event.event_type.to_string()).collect()
    }

    /// The durable event types with the shadow observation removed.
    async fn types_without_decisions(&self) -> Vec<String> {
        self.types().await.into_iter().filter(|t| t != EventType::DECISION_TRACE).collect()
    }

    /// Every event of one type.
    async fn events_of(&self, event_type: &str) -> Vec<StoredEvent> {
        self.log().await.into_iter().filter(|e| e.event.event_type.as_str() == event_type).collect()
    }

    /// The history a fresh process would derive from the log — the model's
    /// view, rebuilt rather than remembered.
    async fn derived(&self) -> Vec<ModelMessage> {
        let engine = ProjectionContextEngine::new(self.stack.store.clone(), self.data_dir.path());
        engine.derive(&self.session_id).await.expect("derive").messages
    }

    /// The bodies of the requests that actually reached the wire.
    fn sent_bodies(&self) -> Vec<String> {
        self.transport.bodies()
    }
}

/// A request body with the state layer's opening time replaced by a marker.
///
/// The state layer reads `State at <rfc3339>.`; the format has no fractional
/// seconds, so the next `.` ends it. The body is JSON text, where the layer's
/// own line breaks are escaped but the time itself is literal.
fn without_opening_time(body: &str) -> String {
    const MARKER: &str = "State at ";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find(MARKER) {
        let (head, tail) = rest.split_at(start + MARKER.len());
        out.push_str(head);
        match tail.find('.') {
            Some(end) => {
                out.push_str("<time>");
                rest = &tail[end..];
            }
            None => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The text of a message's text blocks, concatenated.
fn text_of(message: &ModelMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            agent_protocol::model::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Criteria
// ---------------------------------------------------------------------------

/// The `model → tool → model → answer` turn, over the real adapter and the real
/// guarded tool gateway.
#[tokio::test]
async fn a_real_adapter_drives_model_tool_model_to_an_answer() {
    let h = harness(
        vec![
            Scripted::Ok(tool_call_body(
                "call_1",
                &to_wire_name("fs.read"),
                serde_json::json!({ "path": "notes.txt" }),
            )),
            Scripted::Ok(answer_body("the file says rhubarb")),
        ],
        true,
    )
    .await;
    h.seed_file("notes.txt", "rhubarb");

    let report = h.ask("read notes.txt and tell me the word").await;

    assert_eq!(report.stop_reason, StopReason::FinalAnswer);
    assert_eq!(report.final_content.as_deref(), Some("the file says rhubarb"));

    // The turn was routed once, and the route is in the events — not only in
    // this process's configuration.
    let attempts = h.events_of(EventType::MODEL_ATTEMPT_STARTED).await;
    assert_eq!(attempts.len(), 2, "one attempt per step");
    for attempt in &attempts {
        assert_eq!(attempt.event.payload["route"]["tier"], "main");
        assert_eq!(attempt.event.payload["route"]["provider"], "scripted-endpoint");
    }

    // Two steps: the call, then the answer. The shadow trace precedes the first,
    // and nothing else appears between them — the whole turn is spelled out here
    // so an extra event cannot hide in a "contains" gap.
    let types = h.types().await;
    let opened = types.iter().position(|t| t == EventType::TURN_STARTED).expect("the turn opened");
    let expected = [
        EventType::TURN_STARTED,
        EventType::MODEL_TOOLS_FROZEN,
        EventType::DECISION_TRACE,
        EventType::REASON_RECORDED,
        EventType::REASON_RECORDED,
        EventType::REASON_RECORDED,
        EventType::STEP_STARTED,
        EventType::MODEL_REQUEST_PREPARED,
        EventType::MODEL_ATTEMPT_STARTED,
        EventType::MODEL_RESPONSE_COMPLETED,
        EventType::REASON_RECORDED,
        EventType::TOOL_STARTED,
        EventType::TOOL_COMPLETED,
        EventType::REASON_RECORDED,
        EventType::STEP_COMPLETED,
        EventType::STEP_STARTED,
        EventType::MODEL_REQUEST_PREPARED,
        EventType::MODEL_ATTEMPT_STARTED,
        EventType::MODEL_RESPONSE_COMPLETED,
        EventType::REASON_RECORDED,
        EventType::STEP_COMPLETED,
        // The claim lands with the ending: the item is answered when the turn
        // ends, not when it opens.
        EventType::INBOX_CLAIMED,
        EventType::TURN_COMPLETED,
    ];
    assert_eq!(&types[opened..opened + expected.len()], expected);

    // The tool really ran: the result the model got back is the file's content.
    let completed = h.events_of(EventType::TOOL_COMPLETED).await;
    assert_eq!(completed[0].event.payload["content"], "rhubarb");

    // The second request carried the assistant's tool call and the tool's
    // answer — through the real wire serialization, not a fabricated message
    // list.
    let bodies = h.sent_bodies();
    assert_eq!(bodies.len(), 2, "one request per step");
    let second: serde_json::Value = serde_json::from_str(&bodies[1]).expect("a JSON body");
    let messages = second["messages"].as_array().expect("messages");
    let call = messages
        .iter()
        .flat_map(|m| m.get("tool_calls").and_then(|c| c.as_array()).cloned().unwrap_or_default())
        .next()
        .expect("the assistant's tool call");
    assert_eq!(call["function"]["name"], "fs_read", "internal ids go out as wire names");
    let tool_reply =
        messages.iter().find(|m| m["role"] == "tool").expect("the tool's answer to the model");
    assert_eq!(tool_reply["content"], "rhubarb");
    assert_eq!(tool_reply["tool_call_id"], "call_1");

    // Rebuilt from the log alone, the conversation now holds the call and its
    // result — nothing is carried in this process's memory.
    let derived = h.derived().await;
    assert!(derived.iter().any(|m| m.role == MessageRole::Tool), "the tool result is durable");
    assert!(derived.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, agent_protocol::model::ContentBlock::ToolUse { .. }))
    }));
    assert_eq!(text_of(derived.last().expect("an answer")), "the file says rhubarb");

    // The log explains itself: `why` walks from the answer back through the
    // observation, the call, the stance, the reading and the turn's input.
    let outcome =
        agent_cli::reason::run_why(h.stack.store.as_ref(), &h.session_id, None, None, false)
            .await
            .expect("the chain reads back");
    assert_eq!(outcome.verdict, agent_cli::Verdict::Answered);
    let agent_cli::CliOutput::Text(text) = outcome.output else {
        panic!("why rendered as JSON without being asked");
    };
    assert!(text.contains("[action] fs.read"), "{text}");
    assert!(text.contains("[observation] fs.read completed"), "{text}");
    assert!(text.contains("because action/dispatch:"), "{text}");
    assert!(text.contains("because observation/result:"), "{text}");
    assert!(text.contains("chain check: every node names a rule"), "{text}");
}

/// A real failure changes the posture mid-turn. The next request must carry the
/// repair posture, and `why` must show the switch as a node with the failed
/// observation behind it — not a silent change of heart.
#[tokio::test]
async fn a_failed_call_switches_the_turn_and_why_says_so() {
    let h = harness(
        vec![
            Scripted::Ok(tool_call_body(
                "call_1",
                &to_wire_name("fs.read"),
                serde_json::json!({ "path": "missing.txt" }),
            )),
            Scripted::Ok(answer_body("I could not read it")),
        ],
        false,
    )
    .await;

    let report = h.ask("read missing.txt").await;
    assert_eq!(report.stop_reason, StopReason::FinalAnswer);
    assert_eq!(report.final_content.as_deref(), Some("I could not read it"));

    let outcome =
        agent_cli::reason::run_why(h.stack.store.as_ref(), &h.session_id, None, None, false)
            .await
            .expect("the chain reads back");
    let agent_cli::CliOutput::Text(text) = outcome.output else {
        panic!("why rendered as JSON without being asked");
    };
    assert!(text.contains("[stance] stance switched:"), "{text}");
    assert!(text.contains("-> repair"), "{text}");
    assert!(text.contains("because switch/failure-repair:"), "{text}");
    assert!(text.contains("because verdict/after-failure:"), "{text}");
}

/// Older history is summarized before the first request of the turn that would
/// carry it, through the real projection, the real estimator and the real
/// wire — and the rewrite is durable, so a fresh derivation reads it back.
#[tokio::test]
async fn a_compaction_rewrites_the_history_the_next_turn_sends() {
    // A 3000-character answer is roughly 750 estimated tokens. The window is
    // 1000 and the trigger is 1%, so the second turn is over the threshold; the
    // retention share is 500, so the answer alone is kept and the first
    // question becomes the candidate.
    let layers = Layers {
        workspace: agent_config::WorkspaceConfig::from_toml_str(
            "[budget]\nmax_input_tokens = 1000\ncompact_at_percent = 1\n",
        )
        .expect("a valid budget"),
        ..Layers::default()
    };
    let resolved = agent_config::resolve(layers).expect("the budget resolves");
    let long_answer = "a".repeat(3000);
    let h = harness_with(
        Arc::new(tempfile::tempdir().expect("temp workspace")),
        vec![
            Scripted::Ok(answer_body(&long_answer)),
            Scripted::Ok(answer_body("the summary")),
            Scripted::Ok(answer_body("second answer")),
        ],
        false,
        resolved,
    )
    .await;

    assert_eq!(h.ask("first question").await.stop_reason, StopReason::FinalAnswer);
    assert_eq!(h.ask("second question").await.stop_reason, StopReason::FinalAnswer);

    // The second request carried the summary instead of the first question's
    // own message, and the verbatim record carries the question's words.
    let bodies = h.sent_bodies();
    assert_eq!(bodies.len(), 3, "one call per turn, plus the summary");
    assert!(bodies[1].contains("first question"), "the summary request reads the history");
    assert!(bodies[2].contains("the summary"), "the step request carries the rewrite");
    assert!(
        bodies[2].contains("Recorded user messages (usertext/1")
            && bodies[2].contains("first question"),
        "the record carries the user's words: {}",
        bodies[2]
    );

    // The log says what was summarized and what replaced it, in that order.
    let compaction = h.events_of(EventType::CONTEXT_COMPACTED).await;
    assert_eq!(compaction.len(), 1);
    assert_eq!(compaction[0].event.payload["summary"], "the summary");
    assert_eq!(compaction[0].event.payload["message_count"], 1);
    assert_eq!(compaction[0].event.payload["usertext"]["view_version"], "usertext/1");
    assert!(
        compaction[0].event.payload["usertext"]["text"]
            .as_str()
            .unwrap()
            .contains("first question"),
        "the event records the words verbatim"
    );
    let summary_uid = compaction[0].event.payload["summary_message_uid"]
        .as_str()
        .expect("the summary uid")
        .to_string();
    let types = h.types().await;
    let started = types
        .iter()
        .position(|t| t == EventType::CONTEXT_COMPACTION_STARTED)
        .expect("the attempt opened");
    assert_eq!(types[started + 1], EventType::CONTEXT_COMPACTED);

    // A fresh process deriving from the log alone sees the summary in the first
    // question's place, the record of its words beside it, and the long answer
    // is still there.
    let derived = h.derived().await;
    assert!(
        derived
            .iter()
            .any(|m| m.message_uid.as_str() == summary_uid && text_of(m) == "the summary"),
        "the summary is durable history"
    );
    assert!(!derived.iter().any(|m| text_of(m) == "first question"), "the old message is gone");
    let record = derived
        .iter()
        .find(|m| m.message_uid.as_str().starts_with("msg/usertext/"))
        .expect("the user record is durable history");
    assert!(text_of(record).contains("first question"), "the words are carried verbatim");
    assert!(derived.iter().any(|m| text_of(m) == long_answer), "the retained message stayed");
}

/// The loop's own `plan.update` is offered on the wire with the rest, answered
/// by the loop rather than the gateway, and the plan it records is in the next
/// request's task layer — through the real adapter's serialization.
#[tokio::test]
async fn a_plan_recorded_by_the_loop_reaches_the_next_request_and_the_log() {
    let h = harness(
        vec![
            Scripted::Ok(tool_call_body(
                "call_1",
                &to_wire_name("plan.update"),
                serde_json::json!({
                    "goal": "ship the parser fix",
                    "items": [
                        { "text": "read the failing test", "status": "done" },
                        { "text": "fix the tokenizer", "status": "doing" }
                    ],
                    "facts": ["the failure is in tokenize()"],
                }),
            )),
            Scripted::Ok(answer_body("the plan is recorded")),
        ],
        false,
    )
    .await;

    let report = h.ask("fix the parser").await;
    assert_eq!(report.stop_reason, StopReason::FinalAnswer);

    // The tool was offered to the endpoint under a wire-safe name.
    let bodies = h.sent_bodies();
    assert_eq!(bodies.len(), 2, "one request per step");
    let first: serde_json::Value = serde_json::from_str(&bodies[0]).expect("a JSON body");
    let offered: Vec<&str> = first["tools"]
        .as_array()
        .expect("tools on the wire")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(offered.contains(&"plan_update"), "{offered:?}");

    // The turn spells out the call, the ledger event, and its completion — the
    // plan is recorded between the start and the result, and nothing was sent
    // to a provider tool.
    let types = h.types().await;
    let opened = types.iter().position(|t| t == EventType::TURN_STARTED).expect("the turn opened");
    let expected = [
        EventType::TURN_STARTED,
        EventType::MODEL_TOOLS_FROZEN,
        EventType::REASON_RECORDED,
        EventType::REASON_RECORDED,
        EventType::REASON_RECORDED,
        EventType::STEP_STARTED,
        EventType::MODEL_REQUEST_PREPARED,
        EventType::MODEL_ATTEMPT_STARTED,
        EventType::MODEL_RESPONSE_COMPLETED,
        EventType::REASON_RECORDED,
        EventType::TOOL_STARTED,
        EventType::PLAN_UPDATED,
        EventType::TOOL_COMPLETED,
        EventType::REASON_RECORDED,
        EventType::STEP_COMPLETED,
        EventType::STEP_STARTED,
        EventType::MODEL_REQUEST_PREPARED,
        EventType::MODEL_ATTEMPT_STARTED,
        EventType::MODEL_RESPONSE_COMPLETED,
        EventType::REASON_RECORDED,
        EventType::STEP_COMPLETED,
        // The claim lands with the ending: the item is answered when the turn
        // ends, not when it opens.
        EventType::INBOX_CLAIMED,
        EventType::TURN_COMPLETED,
    ];
    assert_eq!(&types[opened..opened + expected.len()], expected);

    let updates = h.events_of(EventType::PLAN_UPDATED).await;
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].event.payload["plan"]["goal"], "ship the parser fix");
    assert_eq!(updates[0].event.payload["plan"]["items"][1]["status"], "doing");

    // Rebuilt from the log alone, the conversation still holds the call and
    // the loop's answer to it: the plan event contributes no message of its
    // own, and the tool result does.
    let derived = h.derived().await;
    let tool_text = |message: &ModelMessage| -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                agent_protocol::model::ContentBlock::ToolResult { content, .. } => {
                    Some(content.clone())
                }
                _ => None,
            })
            .collect()
    };
    assert!(
        derived.iter().any(|m| m.role == MessageRole::Tool
            && tool_text(m).contains("plan recorded: 2 step(s), 1 fact(s)")),
        "the ledger event itself stays out of the transcript: {derived:?}"
    );

    // The second request carried it in the task layer, and the first said the
    // ledger was empty.
    let second: serde_json::Value = serde_json::from_str(&bodies[1]).expect("a JSON body");
    let system_text = |body: &serde_json::Value| -> String {
        body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message["role"] == "system")
            .filter_map(|message| message["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let before = system_text(&first);
    assert!(before.contains("Plan: none recorded yet."), "{before}");
    let after = system_text(&second);
    assert!(after.contains("Goal: ship the parser fix"), "{after}");
    assert!(after.contains("2. [doing] fix the tokenizer"), "{after}");
    assert!(after.contains("- the failure is in tokenize()"), "{after}");
}

/// M4: an interrupted stream writes no half response.
#[tokio::test]
async fn an_interrupted_stream_leaves_no_half_response() {
    let partial = sse(&[
        r#"data: {"choices":[{"delta":{"content":"half an ans"}}]}"#,
        // No `finish_reason`, no `[DONE]`, and then the connection dies.
    ]);
    let h = harness(vec![Scripted::cut(&partial)], true).await;

    let report = h.ask("say something long").await;

    // The turn ends as a failure, and it says so.
    assert_eq!(report.stop_reason, StopReason::FatalError);
    assert_eq!(report.final_content, None);

    let types = h.types().await;
    assert!(types.contains(&EventType::MODEL_ATTEMPT_FAILED.to_string()), "{types:?}");
    assert!(!types.contains(&EventType::MODEL_RESPONSE_COMPLETED.to_string()), "{types:?}");

    // Nothing of the partial answer is in the log, so nothing can be rebuilt
    // from it: a half response never becomes history.
    let derived = h.derived().await;
    assert!(
        derived.iter().all(|m| m.role != MessageRole::Assistant),
        "no assistant message may survive an interrupted attempt: {derived:?}"
    );
    assert!(!serde_json::to_string(&h.log().await).expect("log as text").contains("half an ans"));

    // The failure is a fact with a stable reason, not a dropped error.
    let failed = h.events_of(EventType::MODEL_ATTEMPT_FAILED).await;
    assert!(failed[0].event.payload["error_kind"].is_string(), "{:?}", failed[0].event.payload);
}

/// M4: failure becomes an attempt fact.
#[tokio::test]
async fn a_provider_failure_becomes_an_attempt_fact() {
    // A 5xx is transient, so the adapter asks again — the script has to hold
    // what the retry will ask for, or the third call would meet an empty queue
    // and the fact under test would be about the fixture rather than the
    // adapter. Three copies: the first attempt and its two retries.
    let unwell = r#"{"error":{"message":"upstream is unwell"}}"#.to_string();
    let h = harness(
        vec![
            Scripted::Status(500, unwell.clone()),
            Scripted::Status(500, unwell.clone()),
            Scripted::Status(500, unwell),
        ],
        true,
    )
    .await;

    let report = h.ask("hello").await;

    assert_eq!(report.stop_reason, StopReason::FatalError);
    // Retries happen inside the adapter, so the loop still sees one attempt and
    // one failure: the step's accounting is not multiplied by the retry budget.
    let failed = h.events_of(EventType::MODEL_ATTEMPT_FAILED).await;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].event.payload["error_kind"], "http");
    // The credential that was sent is nowhere in the log.
    let log = serde_json::to_string(&h.log().await).expect("log as text");
    assert!(!log.contains("sk-test-not-a-real-key"), "the key must never be recorded");
    assert!(!h.types().await.contains(&EventType::MODEL_RESPONSE_COMPLETED.to_string()));
}

/// M4: a shadow decision does not touch the main chain.
///
/// The same turn is driven twice over the same scripted wire — once with the
/// real broker in shadow mode, once with no advisor at all. Everything the
/// model and the log see must be identical; the only difference is the
/// observation, which is itself the evidence that nothing acted on it.
///
/// One exception is structural rather than behavioural: the state layer names
/// when its turn opened, and two runs open at different wall-clock times. The
/// comparison below normalises that timestamp; every other byte must match.
#[tokio::test]
async fn the_shadow_decision_changes_nothing_the_model_sees() {
    fn script() -> Vec<Scripted> {
        vec![
            Scripted::Ok(tool_call_body(
                "call_1",
                &to_wire_name("fs.read"),
                serde_json::json!({ "path": "notes.txt" }),
            )),
            Scripted::Ok(answer_body("the file says rhubarb")),
        ]
    }

    // One workspace for both, because the run's instructions name it: the
    // claim is that the shadow decision changes nothing, not that two
    // differently-located workspaces produce the same prompt.
    let workspace = Arc::new(tempfile::tempdir().expect("temp workspace"));
    let with_shadow = harness_in(workspace.clone(), script(), true).await;
    with_shadow.seed_file("notes.txt", "rhubarb");
    let with_shadow_report = with_shadow.ask("read notes.txt").await;

    let without = harness_in(workspace, script(), false).await;
    without.seed_file("notes.txt", "rhubarb");
    let without_report = without.ask("read notes.txt").await;

    // What the model was sent is byte-for-byte the same, apart from the state
    // layer's opening time, which differs by construction.
    let shadow_bodies: Vec<String> =
        with_shadow.sent_bodies().iter().map(|body| without_opening_time(body)).collect();
    let plain_bodies: Vec<String> =
        without.sent_bodies().iter().map(|body| without_opening_time(body)).collect();
    assert_eq!(shadow_bodies, plain_bodies);
    // The turn ended the same way, with the same answer.
    assert_eq!(with_shadow_report.stop_reason, without_report.stop_reason);
    assert_eq!(with_shadow_report.final_content, without_report.final_content);
    // The step/tool/model events are the same, in the same order.
    assert_eq!(
        with_shadow.types_without_decisions().await,
        without.types_without_decisions().await
    );
    // And the shadow run's only extra event is the trace itself, recorded
    // between the turn opening and the first step.
    let shadow_types = with_shadow.types().await;
    assert_eq!(shadow_types.len(), without.types().await.len() + 1);
    let turn_start =
        shadow_types.iter().position(|t| t == EventType::TURN_STARTED).expect("the turn opened");
    assert_eq!(
        shadow_types[turn_start + 1],
        EventType::MODEL_TOOLS_FROZEN,
        "the tool set is frozen before the route: {shadow_types:?}"
    );
    assert_eq!(
        shadow_types[turn_start + 2],
        EventType::DECISION_TRACE,
        "the route is decided before the first step: {shadow_types:?}"
    );

    // The trace says what it is: a shadow observation by the rule baseline,
    // which answered with the tier the router had already chosen.
    let traces = with_shadow.events_of(EventType::DECISION_TRACE).await;
    assert_eq!(traces.len(), 1);
    let trace = &traces[0].event.payload["trace"];
    assert_eq!(trace["mode"], "shadow");
    assert_eq!(trace["family"], "model_route");
    assert_eq!(trace["chosen_backend"], "rule");
    assert!(trace["aborted"].is_null());

    // Nothing in the log will let a future reader mistake the advice for a
    // decision: the route the model was actually given is the router's.
    let attempts = with_shadow.events_of(EventType::MODEL_ATTEMPT_STARTED).await;
    assert_eq!(attempts[0].event.payload["route"]["model"], "scripted-model");

    // Sanity: the file was really read in both runs, so the comparison above is
    // about the shadow and not about two turns that both did nothing.
    assert_eq!(with_shadow.events_of(EventType::TOOL_COMPLETED).await.len(), 1);
    assert_eq!(without.events_of(EventType::TOOL_COMPLETED).await.len(), 1);
}

/// M4: the run is told what its workspace says, and the log can prove it.
///
/// This goes through the real assembly on purpose: the kernel test proves the
/// scheduler records what it was handed, but only this one notices if
/// `stack::assemble` stops handing it anything.
#[tokio::test]
async fn the_run_records_the_instructions_the_model_was_given() {
    let h = harness(vec![Scripted::Ok(answer_body("done"))], false).await;
    std::fs::write(h.workspace.path().join("AGENTS.md"), "Always run the tests.\n")
        .expect("seed instructions");

    h.ask("hello").await;

    let started = h.events_of(EventType::RUN_STARTED).await;
    let text = started[0].event.payload["instructions"]["text"]
        .as_str()
        .expect("the run recorded its prompt")
        .to_string();
    assert!(text.contains("Always run the tests."), "{text}");
    assert!(text.contains("Workspace root:"), "{text}");

    // The first thing on the wire is that prompt.
    let body: serde_json::Value =
        serde_json::from_str(h.sent_bodies().first().expect("one request")).expect("request json");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], serde_json::json!(text));

    // And a fresh derivation from the log rebuilds the same message, so a later
    // turn's transcript says what this run was told rather than a paraphrase.
    let derived = h.derived().await;
    assert_eq!(derived[0].role, MessageRole::System);
    assert_eq!(text_of(&derived[0]), text);
}

// ---------------------------------------------------------------------------
// Delegation: a receipt in one step, the ending in a later one
// ---------------------------------------------------------------------------

/// The `agent-subagent-stdio` fixture, built into this test's target directory.
///
/// The same helper `subagent_wiring` uses, for the same reason it gives: the
/// fixture is an `examples/` target, which `cargo test` builds and a plain
/// `cargo build --workspace` does not, and a test that quietly skipped itself
/// over a missing fixture would report success for a delegation it never ran.
fn fake_subagent() -> PathBuf {
    let agent = PathBuf::from(env!("CARGO_BIN_EXE_agent"));
    let directory = agent.parent().expect("the binary is in a directory").join("examples");
    let path = directory.join(if cfg!(windows) { "fake-subagent.exe" } else { "fake-subagent" });
    if path.exists() {
        return path;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let profile = directory.parent().expect("the profile directory");
    let mut build = Command::new(cargo);
    build
        .args(["build", "-p", "agent-subagent-stdio", "--example", "fake-subagent"])
        .arg("--target-dir")
        .arg(profile.parent().expect("the target directory"))
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.ends_with("release") {
        build.arg("--release");
    }
    let built = build.status().expect("cargo is on the path");
    assert!(built.success(), "the fake-subagent fixture could not be built");
    assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    path
}

/// A profile in which a turn may delegate.
///
/// Written as TOML and resolved rather than assembled field by field, so what
/// this test exercises is the resolution a configuration file goes through: a
/// layer defines how to reach the provider, `capabilities` authorizes the run to
/// use it, the profile enables it, and a rule lets the call through. All four
/// are needed, and leaving one out is how the refusals elsewhere are produced.
fn delegating_profile() -> ResolvedProfile {
    let user = UserConfig::from_toml_str(&format!(
        "capabilities = [\"subagent.spawn\"]\n\
         plugins = [\"subagent.fake@1.0.0\"]\n\n\
         [subagents.fake]\n\
         command = '{}'\n\
         args = [\"result\"]\n\
         requires = [\"subagent.spawn\"]\n\n\
         [profiles.p]\n\
         tools = {{ allow = [\"subagent.spawn\"] }}\n\
         plugins = {{ enable = [\"subagent.fake\"] }}\n",
        fake_subagent().display(),
    ))
    .expect("the test's configuration parses");
    let layers = Layers { user, profile_override: Some("p".to_string()), ..Layers::default() };
    agent_config::resolve(layers).expect("the test's configuration resolves")
}

/// M4: a turn delegates, goes on, and collects the child's ending in a later
/// step.
///
/// This is the background shape end to end, and the receipt is the proof that
/// the call did not wait: the model's next request reaches the wire at all only
/// because `subagent.run` returned before the child ended. The fixture's
/// `result` mode answers and then sleeps forever, so a blocking call would hang
/// this test rather than pass it.
#[tokio::test]
async fn a_turn_delegates_in_the_background_and_collects_the_child_in_a_later_step() {
    let responses = vec![
        // A child is forked from a *closed* turn, so the session has to have
        // finished one before it can delegate. That is not a test-shortcut: it
        // is the boundary a fork copies up to, and a delegation in a session's
        // first turn is refused with exactly that sentence.
        Scripted::Ok(answer_body("ready")),
        Scripted::Ok(tool_call_body(
            "call-1",
            "subagent.run",
            serde_json::json!({ "provider": "fake", "objective": "summarize the notes" }),
        )),
        // No id: with one child open, naming it is optional. The step after the
        // receipt is the reader a background delegation exists for.
        Scripted::Ok(tool_call_body("call-2", "subagent.wait", serde_json::json!({}))),
        Scripted::Ok(answer_body("all done")),
    ];
    let workspace = Arc::new(tempfile::tempdir().expect("temp workspace"));
    let h = harness_with(workspace, responses, false, delegating_profile()).await;
    h.ask("warm up").await;

    let report = h.ask("delegate the notes").await;
    assert_eq!(report.stop_reason, StopReason::FinalAnswer, "{report:?}");

    // Three more model calls: the run, the wait, the answer. The second was
    // given the receipt rather than the child's result — the ending was not
    // ready when the call returned, which is what "background" means.
    let bodies = h.sent_bodies();
    assert_eq!(bodies.len(), 4, "the turn ran to an answer");
    assert!(bodies[2].contains("running in the background"), "{}", bodies[2]);
    assert!(
        bodies[2].contains("subagent.wait"),
        "the receipt says how to collect it: {}",
        bodies[2]
    );
    // The fourth saw the ending the wait collected: the child's own answer,
    // attributed to the child's session.
    assert!(bodies[3].contains("completed: summarize the notes"), "{}", bodies[3]);

    // The parent's log records one delegation and one ending, under the two
    // actors that own those halves.
    let spawned = h.events_of(EventType::SUBAGENT_SPAWNED).await;
    let settled = h.events_of(EventType::SUBAGENT_COMPLETED).await;
    assert_eq!(spawned.len(), 1, "{:?}", h.types().await);
    assert_eq!(settled.len(), 1, "{:?}", h.types().await);
    assert_eq!(spawned[0].event.actor, Actor::Agent);
    assert_eq!(settled[0].event.actor, Actor::System);
    assert_eq!(settled[0].event.payload["outcome"]["status"], "completed");

    // The child is a session of its own, forked from this turn's parent, and its
    // history opens by saying what it was asked to do.
    let child = SessionId::from_string(
        spawned[0].event.payload["child_session_id"].as_str().expect("a child session"),
    );
    let child_log = h
        .stack
        .store
        .read_stream(&child, EventSeq::ZERO, usize::MAX)
        .await
        .expect("the child's log");
    assert_eq!(child_log[0].event.event_type.as_str(), EventType::SESSION_FORKED);
    assert_eq!(child_log[0].event.payload["fork_reason"], "summarize the notes");

    // And nothing was queued for a later turn: the waiting step had the outcome
    // in hand, and a copy in the Inbox would be pending work nobody will do.
    let queued: Vec<_> = h
        .events_of(EventType::INBOX_APPENDED)
        .await
        .into_iter()
        .filter(|event| event.event.payload["kind"] == "external_result")
        .collect();
    assert!(queued.is_empty(), "{queued:?}");
}

/// M4: a turn that starts a child and ends without collecting it leaves the
/// ending to the drain that runs before the process exits.
///
/// This is the case the exit drain exists for, and the one a caller cannot opt
/// out of by accident: the turn's answer is about the parent's work, and the
/// child's ending is still a fact the log has to hold. What is checked is that
/// the registry the tool filled is the one the runtime exposes — a drain over an
/// empty registry would look identical and record nothing.
#[tokio::test]
async fn a_child_left_running_is_collected_by_the_drain_before_the_process_ends() {
    let responses = vec![
        Scripted::Ok(answer_body("ready")),
        Scripted::Ok(tool_call_body(
            "call-1",
            "subagent.run",
            serde_json::json!({ "provider": "fake", "objective": "summarize the notes" }),
        )),
        // The model does not wait: it answers the parent's question and the
        // child goes on by itself. `support::watch` drains at this boundary in
        // the binary; here the test stands at the same point.
        Scripted::Ok(answer_body("started it, moving on")),
    ];
    let workspace = Arc::new(tempfile::tempdir().expect("temp workspace"));
    let h = harness_with(workspace, responses, false, delegating_profile()).await;
    h.ask("warm up").await;
    h.ask("start the notes work and move on").await;

    assert_eq!(h.events_of(EventType::SUBAGENT_SPAWNED).await.len(), 1, "{:?}", h.types().await);
    assert!(
        h.events_of(EventType::SUBAGENT_COMPLETED).await.is_empty(),
        "nothing collected the child yet: {:?}",
        h.types().await
    );

    let notes = h.stack.children.drain().await;
    assert!(notes.is_empty(), "{notes:?}");
    let settled = h.events_of(EventType::SUBAGENT_COMPLETED).await;
    assert_eq!(settled.len(), 1, "the drain recorded the ending: {:?}", h.types().await);
    assert_eq!(settled[0].event.payload["outcome"]["status"], "completed");
    // Draining twice must not write a second settlement for the same child.
    assert!(h.stack.children.drain().await.is_empty());
    assert_eq!(h.events_of(EventType::SUBAGENT_COMPLETED).await.len(), 1);
}
