//! Memory acceptance: the model searches the session's own history, and — when
//! the configuration turns it on — reads and writes the facts that outlive it.
//!
//! The index and the fact store are tested in their own crates; what is checked
//! here is the wiring the product ships: `stack::assemble` puts the tools on the
//! surface, the capability gate admits them under the default policy and refuses
//! them under an explicit deny or a sandbox that cannot back them, a write waits
//! for a person, and the ordinary tool event chain carries each call and its
//! result — so a replay derives the conversation the model saw.

use std::sync::Arc;

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile, WorkspaceTools};
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
// The fact store's methods are the port's, not inherent ones: the trait has to
// be in scope for a test to read back what the run wrote.
use agent_ports::memory_store::MemoryStore as _;
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
    harness_with(Layers::default(), SandboxMode::LocalProcess, responses).await
}

/// The same, under a stated configuration and execution backend.
async fn harness_with(
    layers: Layers,
    sandbox: SandboxMode,
    responses: Vec<ModelResponse>,
) -> Harness {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let session = SessionId::from_string("memory");
    let model = Arc::new(FakeModelProvider::new(responses));
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
        sandbox,
        network_allowed: false,
        judgment: true,
    })
    .await
    .expect("the assembly the binary runs");
    Harness { stack, model, session, data_dir, _workspace: workspace }
}

/// Whether the first request the model saw offered `tool`.
fn offered(harness: &Harness, tool: &str) -> bool {
    harness
        .model
        .requests()
        .first()
        .expect("the model was asked at least once")
        .tool_manifest
        .entries
        .iter()
        .any(|entry| entry.tool_id == tool)
}

impl Harness {
    /// Queue one user turn and run it. `None` is a turn that stopped for an
    /// answer — an approval — rather than reaching a final answer.
    async fn drive(&self, text: &str) -> Option<agent_kernel::scheduler::RunReport> {
        self.stack
            .scheduler
            .inbox()
            .append(&self.session, InboxKind::NextTurn, json!({ "text": text }), 0, Actor::User)
            .await
            .expect("append input");
        self.stack.scheduler.run_pending(&self.session, Utc::now()).await.expect("run pending")
    }

    async fn log(&self) -> Vec<StoredEvent> {
        self.stack
            .store
            .read_stream(&self.session, EventSeq::ZERO, usize::MAX)
            .await
            .expect("read the log")
    }

    async fn ask(&self, text: &str) -> (Option<String>, Vec<StoredEvent>) {
        let report = self.drive(text).await.expect("a turn ran");
        (report.final_content, self.log().await)
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

// ---------------------------------------------------------------------------
// Long-term memory: the facts that outlive the session. Off unless the
// configuration turns them on; writes ask; reads are free.
// ---------------------------------------------------------------------------

/// Layers whose user configuration wrote `[memory] enabled = <on>`.
fn memory_layers(on: bool) -> Layers {
    Layers {
        user: agent_config::UserConfig::from_toml_str(&format!("[memory]\nenabled = {on}\n"))
            .expect("the user configuration"),
        ..Layers::default()
    }
}

/// The durable store the assembled run writes, opened the way the CLI opens it.
fn store(harness: &Harness) -> agent_memory_sqlite::SqliteMemoryStore {
    agent_memory_sqlite::SqliteMemoryStore::open(harness.data_dir.path().to_path_buf())
}

fn tool_events<'a>(log: &'a [StoredEvent], event_type: &str, tool: &str) -> Vec<&'a StoredEvent> {
    log.iter()
        .filter(|stored| {
            stored.event.event_type.as_str() == event_type
                && stored.event.payload["tool_name"].as_str() == Some(tool)
        })
        .collect()
}

#[tokio::test]
async fn the_fact_tools_are_off_the_surface_until_the_configuration_turns_them_on() {
    // The default profile is what every run starts from: no `[memory]` section,
    // so no provider is built and the names are not even registered.
    let harness = harness(vec![
        FakeModelProvider::tool_call("memory.remember", json!({"text": "a fact"})),
        FakeModelProvider::answer("nothing was written"),
    ])
    .await;

    let (_, log) = harness.ask("remember that the build is green").await;

    assert!(offered(&harness, "memory.search"), "the session index is still there");
    for name in ["memory.recall", "memory.remember", "memory.forget"] {
        assert!(!offered(&harness, name), "{name} must not be offered by default");
    }
    let failed = tool_events(&log, EventType::TOOL_FAILED, "memory.remember");
    assert_eq!(failed.len(), 1, "a name nobody registered cannot be called");
    let content = failed[0].event.payload["content"].as_str().expect("a refusal string");
    assert!(content.contains("unknown tool"), "{content}");
    assert!(
        tool_events(&log, EventType::TOOL_COMPLETED, "memory.remember").is_empty(),
        "nothing ran"
    );
}

#[tokio::test]
async fn remembering_waits_for_a_person_even_when_memory_is_on() {
    let harness = harness_with(
        memory_layers(true),
        SandboxMode::LocalProcess,
        vec![
            FakeModelProvider::tool_call(
                "memory.remember",
                json!({"text": "the deploy window is Tuesday", "key": "deploy-window"}),
            ),
            FakeModelProvider::answer("stored it"),
        ],
    )
    .await;

    // The call is on the surface — and it stops the turn, because `memory.write`
    // is `ask` by default.
    let stopped =
        harness.drive("remember that deploys go out on Tuesday").await.expect("a turn ran");
    assert_eq!(
        stopped.stop_reason,
        agent_ports::agent_loop::StopReason::NeedsUserInput,
        "the turn stops at the question rather than deciding it"
    );
    let pending = harness.stack.approvals.pending(&harness.session).await.expect("pending");
    assert_eq!(pending.len(), 1, "one call is waiting");
    assert_eq!(pending[0].tool_name, "memory.remember");
    assert_eq!(
        pending[0].capability,
        agent_protocol::permissions::Capability::new(
            agent_protocol::permissions::Capability::MEMORY_WRITE
        )
    );
    assert!(
        store(&harness)
            .recall(&agent_ports::memory_store::Recall::default(), Utc::now())
            .await
            .expect("recall")
            .is_empty(),
        "nothing is written before the answer"
    );

    let approval_id = pending[0].approval_id.clone();
    harness
        .stack
        .approvals
        .grant(
            &harness.session,
            &approval_id,
            agent_protocol::permissions::ApprovalScope::AllowOnce,
            "user",
        )
        .await
        .expect("grant");
    let outcome =
        harness.stack.resume.resume(&harness.session, "yes, remember it").await.expect("resume");
    assert!(
        matches!(
            outcome,
            agent_kernel::resume::ResumeOutcome::Settled {
                disposition: agent_kernel::resume::Disposition::Ran {
                    status: agent_protocol::tool::ToolStatus::Completed
                },
                ..
            }
        ),
        "the approved call runs on the continuation: {outcome:?}"
    );

    let log = harness.log().await;
    let completed = tool_events(&log, EventType::TOOL_COMPLETED, "memory.remember");
    assert_eq!(completed.len(), 1, "the call ran once");
    let content = completed[0].event.payload["content"].as_str().expect("a result string");
    assert!(content.contains("remembered mem_"), "{content}");

    // The fact is in the store the CLI lists from, credited to this session and
    // the model call that asked for it.
    let facts = store(&harness)
        .recall(&agent_ports::memory_store::Recall::default(), Utc::now())
        .await
        .expect("recall");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].text, "the deploy window is Tuesday");
    assert_eq!(facts[0].key.as_deref(), Some("deploy-window"));
    assert_eq!(facts[0].origin, agent_ports::memory_store::MemoryOrigin::Agent);
    let source = facts[0].source.as_ref().expect("a fact the model wrote carries its source");
    assert_eq!(source.session, harness.session);
    assert!(source.call.is_some(), "the tool-use id names the exchange in the log");
}

#[tokio::test]
async fn a_read_only_run_offers_recall_and_not_the_writes() {
    // Read-only removes `memory.write` from what the backend advertises, so the
    // gateway drops every tool whose calls would all be refused. What stays is
    // what the run can actually do.
    let harness = harness_with(
        memory_layers(true),
        SandboxMode::ReadOnly,
        vec![
            FakeModelProvider::tool_call("memory.recall", json!({})),
            FakeModelProvider::answer("nothing yet"),
        ],
    )
    .await;

    let (final_content, log) = harness.ask("what do you remember?").await;
    assert_eq!(final_content.as_deref(), Some("nothing yet"));

    assert!(offered(&harness, "memory.recall"), "reading changes nothing, so it stays");
    for name in ["memory.remember", "memory.forget"] {
        assert!(!offered(&harness, name), "{name} cannot run in a read-only run");
    }
    let completed = tool_events(&log, EventType::TOOL_COMPLETED, "memory.recall");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].event.payload["content"].as_str(), Some("nothing is remembered yet"));
    // A recall that found nothing wrote nothing: the store it opened is empty.
    assert!(
        store(&harness)
            .list(&agent_ports::memory_store::ListFacts::default(), Utc::now())
            .await
            .expect("list")
            .is_empty(),
        "a read-only run changed no facts"
    );
}
