//! Memory acceptance: the model searches the session's own history through the
//! assembly the binary runs.
//!
//! The index itself is tested in `agent-index-sqlite`; what is checked here is
//! the wiring the product ships: `stack::assemble` puts `memory.search` on the
//! surface, the capability gate admits it under the default policy and refuses
//! it under an explicit deny, and the ordinary tool event chain carries the
//! call and its result — so a replay derives the conversation the model saw.

use std::sync::Arc;

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile, WorkspaceTools};
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::model_provider::ModelProvider;
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_protocol::events::{Actor, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::model::{ContentBlock, ModelResponse};
use agent_testkit::FakeModelProvider;
use chrono::Utc;
use serde_json::json;

struct Harness {
    stack: stack::Runtime,
    model: Arc<FakeModelProvider>,
    session: SessionId,
    /// Kept alive for the test: the tool's database lives under the data dir.
    data_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

fn profile(layers: Layers) -> ResolvedProfile {
    agent_config::resolve(layers).expect("the configuration resolves")
}

/// Assemble the product's stack over a fake model, with the default profile.
async fn harness(responses: Vec<ModelResponse>) -> Harness {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let session = SessionId::from_string("memory");
    let model = Arc::new(FakeModelProvider::new(responses));
    let provider: Arc<dyn ModelProvider> = model.clone();
    let stack = stack::assemble(stack::Inputs {
        session: session.clone(),
        workspace_root: workspace.path().to_path_buf(),
        data_dir: data_dir.path().to_path_buf(),
        resolved: profile(Layers::default()),
        model: Some(SelectedProvider {
            provider,
            route: ModelRoute::new(ModelTier::Main, "fake", "fake-model"),
        }),
        decisions: Decisions::Unobserved,
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: true,
    })
    .await
    .expect("the assembly the binary runs");
    Harness { stack, model, session, data_dir, _workspace: workspace }
}

impl Harness {
    async fn ask(&self, text: &str) -> (Option<String>, Vec<StoredEvent>) {
        self.stack
            .scheduler
            .inbox()
            .append(&self.session, InboxKind::NextTurn, json!({ "text": text }), 0, Actor::User)
            .await
            .expect("append input");
        let report = self
            .stack
            .scheduler
            .run_pending(&self.session, Utc::now())
            .await
            .expect("run pending")
            .expect("a turn ran");
        let log = self
            .stack
            .store
            .read_stream(&self.session, EventSeq::ZERO, usize::MAX)
            .await
            .expect("read the log");
        (report.final_content, log)
    }
}

fn event<'a>(log: &'a [StoredEvent], event_type: &str) -> &'a StoredEvent {
    log.iter()
        .find(|stored| stored.event.event_type.as_str() == event_type)
        .unwrap_or_else(|| panic!("no {event_type} in the log"))
}

#[tokio::test]
async fn the_model_searches_its_own_history_through_the_assembled_stack() {
    let harness = harness(vec![
        FakeModelProvider::tool_call("memory.search", json!({"query": "xylophone"})),
        FakeModelProvider::answer("the word was in the request you sent me"),
    ])
    .await;

    let (final_content, log) = harness.ask("remember the word xylophone").await;
    assert_eq!(final_content.as_deref(), Some("the word was in the request you sent me"));

    // The surface the model was offered includes the tool.
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one call, one answer");
    assert!(
        requests[0].tool_manifest.entries.iter().any(|entry| entry.tool_id == "memory.search"),
        "the first request offers memory.search"
    );

    // The call went through the ordinary tool chain, and its result is the
    // index's answer — this turn's own input is already in the log, so the
    // search finds it.
    let started = event(&log, EventType::TOOL_STARTED);
    assert_eq!(started.event.payload["tool_name"], "memory.search");
    let completed = event(&log, EventType::TOOL_COMPLETED);
    assert_eq!(completed.event.payload["tool_name"], "memory.search");
    assert_eq!(completed.event.payload["status"], "completed");
    let content = completed.event.payload["content"].as_str().expect("a result string");
    assert!(content.contains("xylophone"), "{content}");
    assert!(content.contains("inbox/appended"), "{content}");

    // The model was handed that same text back.
    let tool_result = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .expect("the result is in the second request");
    assert_eq!(tool_result, content, "the model sees what the log kept");

    // The index is the per-session database beside that session's log.
    assert!(
        harness.data_dir.path().join("memory").join("memory.sqlite").exists(),
        "assembly built the session's index"
    );
}

#[tokio::test]
async fn an_explicit_policy_deny_keeps_the_tool_from_running() {
    let mut layers = Layers::default();
    layers.workspace.tools = Some(WorkspaceTools { deny: vec!["memory.read".to_string()] });

    let workspace = tempfile::tempdir().expect("temp workspace");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let session = SessionId::from_string("memory");
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("memory.search", json!({"query": "xylophone"})),
        FakeModelProvider::answer("the tool was refused"),
    ]));
    let provider: Arc<dyn ModelProvider> = model.clone();
    let stack = stack::assemble(stack::Inputs {
        session: session.clone(),
        workspace_root: workspace.path().to_path_buf(),
        data_dir: data_dir.path().to_path_buf(),
        resolved: profile(layers),
        model: Some(SelectedProvider {
            provider,
            route: ModelRoute::new(ModelTier::Main, "fake", "fake-model"),
        }),
        decisions: Decisions::Unobserved,
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: true,
    })
    .await
    .expect("the assembly the binary runs");

    stack
        .scheduler
        .inbox()
        .append(
            &session,
            InboxKind::NextTurn,
            json!({ "text": "remember the word xylophone" }),
            0,
            Actor::User,
        )
        .await
        .expect("append input");
    stack
        .scheduler
        .run_pending(&session, Utc::now())
        .await
        .expect("run pending")
        .expect("a turn ran");

    let log =
        stack.store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.expect("read the log");
    let failed = event(&log, EventType::TOOL_FAILED);
    assert_eq!(failed.event.payload["tool_name"], "memory.search");
    let content = failed.event.payload["content"].as_str().expect("a refusal string");
    assert!(content.contains("policy denied tool 'memory.search'"), "{content}");
    assert!(
        !log.iter().any(|stored| stored.event.event_type.as_str() == EventType::TOOL_COMPLETED),
        "the denied call never ran"
    );
}
