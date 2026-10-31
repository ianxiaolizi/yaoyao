//! Acceptance: the assembled product on a model-derived budget.
//!
//! Both tests drive `agent::stack::assemble` — the assembly the binary runs —
//! over a scripted wire, and read back only what a reader of the log could
//! read: the frozen profile snapshot, the events, and the bytes that reached
//! the transport. The network is the one seam that is faked, as in
//! `m4_acceptance.rs`; everything above it is production code.
//!
//! The two facts pinned here are the ones that motivated the change:
//!
//! - a model that declares a million-token window is not run on the build's
//!   old fixed cap; its budget is derived from what it said, and the snapshot
//!   records the numbers and their sources;
//! - a response that spent its whole output cap thinking is retried once, and
//!   the retry's answer — not the empty first response — is what the turn
//!   reports.

use std::sync::{Arc, Mutex};

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile};
use agent_model_openai::{
    OpenAiCompatConfig, OpenAiCompatProvider, Transport, TransportError, TransportRequest,
    TransportResponse,
};
use agent_ports::agent_loop::StopReason;
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_protocol::events::{Actor, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::inbox::InboxKind;
use agent_testkit::FixedSecret;
use chrono::Utc;
use tempfile::TempDir;

/// A transport that replays scripted SSE bodies and records what it sent.
///
/// Chat traffic consumes the script; `GET /models` answers from the catalog
/// without consuming anything, so a test scripts exactly the attempts it
/// expects.
struct ScriptedTransport {
    responses: Mutex<Vec<String>>,
    posts: Mutex<Vec<String>>,
    models: String,
}

impl ScriptedTransport {
    fn new(responses: Vec<String>, models: String) -> Self {
        Self { responses: Mutex::new(responses), posts: Mutex::new(Vec::new()), models }
    }

    fn bodies(&self) -> Vec<String> {
        self.posts.lock().map(|posts| posts.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Transport for ScriptedTransport {
    async fn post(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        if let Ok(mut posts) = self.posts.lock() {
            posts.push(String::from_utf8_lossy(&request.body).to_string());
        }
        let next = self
            .responses
            .lock()
            .ok()
            .and_then(|mut queue| if queue.is_empty() { None } else { Some(queue.remove(0)) });
        let Some(body) = next else {
            return Err(TransportError::Connect("no scripted response left".to_string()));
        };
        Ok(TransportResponse {
            status: 200,
            body: Box::pin(futures_util::stream::iter(vec![Ok(body.into_bytes())])),
        })
    }

    async fn get(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        assert!(
            request.url.ends_with("/models"),
            "discovery asks for the model list: {}",
            request.url
        );
        Ok(TransportResponse {
            status: 200,
            body: Box::pin(futures_util::stream::iter(vec![Ok(self.models.clone().into_bytes())])),
        })
    }
}

/// One SSE body from raw `data:` payload lines.
fn sse(lines: &[&str]) -> String {
    lines.iter().map(|line| format!("{line}\n\n")).collect()
}

/// A streamed answer, as an endpoint would send it.
fn answer_body(text: &str) -> String {
    sse(&[
        &format!(
            r#"data: {{"choices":[{{"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#
        ),
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ])
}

/// A response the provider cut at the output cap with nothing but reasoning:
/// the ending is `length`, no content arrived, and the usage says the whole
/// output was reasoning tokens.
fn reasoning_only_body(reasoning_tokens: u64) -> String {
    sse(&[
        r#"data: {"choices":[{"delta":{"reasoning_content":"think"},"finish_reason":null}]}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        &format!(
            r#"data: {{"choices":[],"usage":{{"prompt_tokens":100,"completion_tokens":{reasoning_tokens},"total_tokens":{},"completion_tokens_details":{{"reasoning_tokens":{reasoning_tokens}}}}}}}"#,
            100 + reasoning_tokens
        ),
        "data: [DONE]",
    ])
}

/// The catalog the scripted endpoint serves for the model this harness names:
/// the numbers a one-million-token reasoning model declares.
fn deepseek_flash_catalog() -> String {
    serde_json::json!({
        "data": [{
            "id": "scripted-model",
            "context_window": 1_048_576,
            "max_output_tokens": 393_216,
            "reasoning_efforts": ["low", "high", "max"],
            "default_reasoning_effort": "high",
        }]
    })
    .to_string()
}

fn resolved_profile() -> ResolvedProfile {
    agent_config::resolve(Layers::default()).expect("the defaults resolve")
}

fn route() -> ModelRoute {
    ModelRoute::new(ModelTier::Main, "scripted-endpoint", "scripted-model")
}

/// The assembled stack over a scripted wire, built by the same
/// `stack::assemble` the binary calls.
struct Harness {
    stack: stack::Runtime,
    session_id: SessionId,
    transport: Arc<ScriptedTransport>,
    _workspace: TempDir,
    _data_dir: TempDir,
}

async fn assemble(responses: Vec<String>) -> Harness {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let transport = Arc::new(ScriptedTransport::new(responses, deepseek_flash_catalog()));
    let config = OpenAiCompatConfig::new("https://scripted.example.com", "scripted-model")
        .with_provider_id("scripted-endpoint")
        .with_timeout_ms(5_000);
    let provider = Arc::new(OpenAiCompatProvider::new(
        config,
        Arc::new(FixedSecret::new("sk-test-not-a-real-key")),
        transport.clone() as Arc<dyn Transport>,
    ));
    let stack = stack::assemble(stack::Inputs {
        session: SessionId::from_string("budget"),
        workspace_root: workspace.path().to_path_buf(),
        data_dir: data_dir.path().to_path_buf(),
        resolved: resolved_profile(),
        model: Some(SelectedProvider { provider, route: route() }),
        decisions: Decisions::Unobserved,
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: true,
    })
    .await
    .expect("the assembly the binary runs");
    Harness {
        stack,
        session_id: SessionId::from_string("budget"),
        transport,
        _workspace: workspace,
        _data_dir: data_dir,
    }
}

impl Harness {
    /// Submit `text` as the next turn's input and drive it.
    async fn ask(&self, text: &str) -> agent_kernel::scheduler::RunReport {
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

    async fn log(&self) -> Vec<StoredEvent> {
        self.stack
            .store
            .read_stream(&self.session_id, EventSeq::ZERO, usize::MAX)
            .await
            .expect("read log")
    }
}

/// A million-token model gets the budget its declaration implies, and the
/// snapshot says where each number came from.
#[tokio::test]
async fn a_one_million_window_model_gets_a_model_derived_budget() {
    let h = assemble(Vec::new()).await;
    let profile = h.stack.profile_snapshot();
    let budget = profile.budget.context.expect("a resolved context budget");

    assert_eq!(budget.context_window_tokens, 1_048_576);
    assert_eq!(budget.completion_cap_tokens, 262_144);
    assert_eq!(budget.input_working_set_tokens, 262_144);
    assert_eq!(budget.compaction_trigger_tokens, 209_715);
    assert_eq!(budget.retain_recent_tokens, 125_829);

    // The cap asked of the provider is the derived one, not the build's old
    // completion default, and the snapshot records the derivation.
    assert_eq!(profile.budget.max_output_tokens, Some(262_144));
    assert_eq!(
        profile.budget_sources.get("context_window_tokens").map(String::as_str),
        Some("provider:/models")
    );
    assert_eq!(
        profile.budget_sources.get("input_working_set_tokens").map(String::as_str),
        Some("preset:balanced")
    );
}

/// A reasoning-only `length` response is retried once; the turn settles on the
/// retry's answer, and the empty first response is never reported as one.
#[tokio::test]
async fn reasoning_only_length_is_retried_and_not_reported_as_an_answer() {
    let h = assemble(vec![reasoning_only_body(8_000), answer_body("finished")]).await;
    let outcome = h.ask("finish the task").await;

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert_eq!(outcome.final_content.as_deref(), Some("finished"));
    // Two attempts: the truncated one and the recovery it bought.
    assert_eq!(h.transport.bodies().len(), 2);

    // The second request was made with the recovered cap: the first was sent
    // at the derived 262,144, the retry at the ceiling the model declared.
    let bodies = h.transport.bodies();
    let first: serde_json::Value = serde_json::from_str(&bodies[0]).expect("a JSON body");
    let second: serde_json::Value = serde_json::from_str(&bodies[1]).expect("a JSON body");
    assert_eq!(first["max_tokens"], 262_144);
    assert_eq!(second["max_tokens"], 393_216);

    // The log says the recovery happened, beside the attempt it replaced: two
    // prepared requests, and the second one is the recovery's.
    let prepared: Vec<_> = h
        .log()
        .await
        .into_iter()
        .filter(|event| event.event.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
        .collect();
    assert_eq!(prepared.len(), 2, "one prepared record per attempt");
    assert_eq!(prepared[1].event.payload["output_recovery"], "reasoning_only");
}
