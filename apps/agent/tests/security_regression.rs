//! Security regression: the fail-closed and approval invariants, in one suite.
//!
//! The plan's stage 5.5 asks for six properties to be "fixed into a dedicated
//! suite" rather than left scattered across the layers that happen to enforce
//! them. The list is:
//!
//! | invariant | this file's test |
//! |---|---|
//! | no policy grants it ⇒ refuse | `a_call_with_no_matching_policy_is_refused` |
//! | the sandbox does not provide it ⇒ refuse | `a_capability_the_environment_does_not_advertise_is_refused` |
//! | a path outside the workspace ⇒ refuse | `a_path_outside_the_workspace_is_refused` |
//! | a dangerous call ⇒ ask | `a_dangerous_call_stops_the_turn_for_an_answer` |
//! | a denied approval ⇒ never execute | `a_denied_call_never_touches_the_world` |
//! | the UI/model is not the source of truth | `state_comes_from_the_log_not_from_a_claim` |
//!
//! These tests do not replace the deeper ones. `policy.rs`, `guarded.rs`,
//! `resource_guard.rs`, `resume.rs` and `m5_acceptance.rs` each prove one layer
//! in more detail; what was missing was a single place where the six claims run
//! against the product's own assembly and can be read as one statement. The two
//! genuinely new assertions are the last two rows: denial was only tested at the
//! kernel level (m5 covers the grant path, `m6`'s non-execution is a policy
//! refusal, not an approval refusal), and "a claim is not a fact" had no direct
//! test at all — only the incidental fact that state can be rebuilt from the
//! log.
//!
//! The stack is [`stack::assemble`] — the function the binary calls. Where the
//! product would read configuration and select a provider, the test supplies the
//! same resolved profile and a scripted model; everything between the wire and
//! the world is production code.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile};
use agent_kernel::plugin::PluginRuntime;
use agent_kernel::resume::{Disposition, ResumeOutcome};
use agent_kernel::scheduler::RunReport;
use agent_model_openai::{
    OpenAiCompatConfig, OpenAiCompatProvider, Transport, TransportError, TransportRequest,
    TransportResponse, to_wire_name,
};
use agent_ports::agent_loop::StopReason;
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_ports::tool_gateway::ToolGateway;
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::events::{Actor, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, PluginId, SessionId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::permissions::{ApprovalStatus, Capability, CapabilitySet};
use agent_protocol::plugin::{PluginCapabilities, PluginContributions, PluginManifest};
use agent_testkit::FixedSecret;
use chrono::Utc;
use tempfile::TempDir;

/// The session every test runs under.
const SESSION: &str = "security-regression";

/// A command that writes a marker only if it actually runs.
///
/// The denial test needs a side effect it can look for afterwards. `echo` alone
/// would leave the world unchanged whether it ran or not.
const MARKER_COMMAND: &str = "echo ran > marker.txt";

// ---------------------------------------------------------------------------
// The one fake: bytes on a wire
// ---------------------------------------------------------------------------

/// A transport that replays response bodies in order and keeps what it was
/// asked to send.
///
/// Running out of scripted responses is an error rather than a hang: a test
/// that expected three model calls and got four should fail loudly.
struct ScriptedTransport {
    bodies: Mutex<VecDeque<String>>,
    posts: Mutex<Vec<TransportRequest>>,
}

impl ScriptedTransport {
    fn new(bodies: Vec<String>) -> Self {
        Self { bodies: Mutex::new(bodies.into()), posts: Mutex::new(Vec::new()) }
    }

    fn posts(&self) -> Vec<String> {
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
        let next = self.bodies.lock().ok().and_then(|mut queue| queue.pop_front());
        let Some(body) = next else {
            return Err(TransportError::Connect("no scripted response left".to_string()));
        };
        Ok(TransportResponse {
            status: 200,
            body: Box::pin(futures_util::stream::iter(vec![Ok(body.into_bytes())])),
        })
    }
}

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

/// The route this harness's provider is registered under.
fn route() -> ModelRoute {
    ModelRoute::new(ModelTier::Main, "scripted-endpoint", "scripted-model")
}

/// The configuration a run in a directory with no configuration has.
fn default_profile() -> ResolvedProfile {
    agent_config::resolve(Layers::default()).expect("the defaults resolve")
}

// ---------------------------------------------------------------------------
// The product's assembly, with a scripted model
// ---------------------------------------------------------------------------

struct Process {
    stack: stack::Runtime,
    transport: Arc<ScriptedTransport>,
    session: SessionId,
}

/// Assemble a process the way the binary does, over a scripted model.
async fn process(workspace: &Path, data_dir: &Path, script: Vec<String>) -> Process {
    let transport = Arc::new(ScriptedTransport::new(script));
    let stack = stack::assemble(stack::Inputs {
        session: SessionId::from_string(SESSION),
        workspace_root: workspace.to_path_buf(),
        data_dir: data_dir.to_path_buf(),
        resolved: default_profile(),
        model: Some(SelectedProvider {
            provider: scripted_provider(transport.clone()),
            route: route(),
        }),
        decisions: Decisions::FromConfiguration,
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        // These runs are about the policy and approval boundary, and their
        // model scripts are fixed sequences counted in calls. The thinking
        // pipeline would hold the first irreversible call for a branch record
        // the scripts were never written to produce.
        judgment: false,
    })
    .await
    .expect("the assembly the binary runs");

    Process { stack, transport, session: SessionId::from_string(SESSION) }
}

impl Process {
    /// Queue `text` as the next turn's input and drive it.
    async fn ask(&self, text: &str) -> RunReport {
        self.stack
            .scheduler
            .inbox()
            .append(
                &self.session,
                InboxKind::NextTurn,
                serde_json::json!({ "text": text }),
                0,
                Actor::User,
            )
            .await
            .expect("queue the turn");
        self.stack
            .scheduler
            .run_pending(&self.session, Utc::now())
            .await
            .expect("run pending")
            .expect("a turn ran")
    }

    async fn log(&self) -> Vec<StoredEvent> {
        self.stack
            .store
            .read_stream(&self.session, EventSeq::ZERO, usize::MAX)
            .await
            .expect("read the log")
    }

    async fn count(&self, event_type: &str) -> usize {
        self.log().await.into_iter().filter(|e| e.event.event_type.as_str() == event_type).count()
    }

    /// Every event of one type whose payload names `tool`.
    ///
    /// Per tool, so a call that quietly never ran cannot hide behind one that
    /// did.
    async fn tool_events(&self, event_type: &str, tool: &str) -> Vec<StoredEvent> {
        self.log()
            .await
            .into_iter()
            .filter(|e| {
                e.event.event_type.as_str() == event_type
                    && e.event.payload["tool_name"].as_str() == Some(tool)
            })
            .collect()
    }

    /// The `content` the model will read for each event of a type naming `tool`.
    async fn tool_contents(&self, event_type: &str, tool: &str) -> Vec<String> {
        self.tool_events(event_type, tool)
            .await
            .into_iter()
            .filter_map(|e| e.event.payload["content"].as_str().map(str::to_string))
            .collect()
    }

    fn posts(&self) -> Vec<String> {
        self.transport.posts()
    }
}

// ---------------------------------------------------------------------------
// A plugin fixture for the two capability gates
// ---------------------------------------------------------------------------

/// The capabilities a test says the run has been granted.
fn authorized(capabilities: &[&str]) -> CapabilitySet {
    capabilities.iter().map(|c| Capability::new(*c)).collect()
}

fn manifest(id: &str, requires: &[&str]) -> PluginManifest {
    PluginManifest {
        schema_version: agent_protocol::versioning::formats::PLUGIN_MANIFEST,
        id: PluginId::from_string(id),
        version: "1.0.0".to_string(),
        plugin_api: agent::plugins::PLUGIN_API.to_string(),
        capabilities: PluginCapabilities {
            requires: requires.iter().map(|c| Capability::new(*c)).collect(),
        },
        contributions: PluginContributions { tool_provider: true, ..Default::default() },
        config_schema: None,
    }
}

/// A definition contributed by a test plugin.
///
/// `requires` is stated explicitly: a tool that declares nothing is refused by
/// the gateway before it runs, so the empty set would not be a neutral fixture.
fn definition(name: &str, side_effect: ToolSideEffect, requires: &[&str]) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("{name}, contributed by a test plugin"),
        source: ToolSource::Builtin,
        side_effect,
        capability_requirements: requires.iter().map(|c| Capability::new(*c)).collect(),
        idempotent: true,
        timeout_ms: None,
        input_schema: serde_json::json!({ "type": "object" }),
    }
}

/// A plugin that would happily run and counts whether it was asked to.
struct ProbePlugin {
    definition: ToolDefinition,
    calls: AtomicUsize,
}

impl ProbePlugin {
    fn new(definition: ToolDefinition) -> Self {
        Self { definition, calls: AtomicUsize::new(0) }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ToolProvider for ProbePlugin {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![self.definition.clone()]
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        (tool_name == self.definition.name).then_some(self.definition.side_effect)
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        if invocation.tool_name != self.definition.name {
            return Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: vec![self.definition.name.clone()],
            });
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::completed(format!("{} ran", self.definition.name)))
    }
}

fn dirs() -> (TempDir, TempDir) {
    (TempDir::new().expect("a workspace"), TempDir::new().expect("a data directory"))
}

impl Process {
    /// The tool ids the model was offered, read from the frozen manifest the
    /// log keeps for the turn.
    async fn advertised(&self) -> Vec<String> {
        let frozen = self
            .log()
            .await
            .into_iter()
            .rfind(|e| e.event.event_type.as_str() == EventType::MODEL_TOOLS_FROZEN)
            .expect("the turn froze a manifest before its first step");
        frozen.event.payload["entries"]
            .as_array()
            .expect("the frozen manifest lists its entries")
            .iter()
            .filter_map(|entry| entry["tool_id"].as_str().map(str::to_string))
            .collect()
    }

    /// The refusal the log recorded for one provider call, if it recorded none
    /// of the call succeeding.
    ///
    /// Matched by call id rather than by tool name: a call to a tool the
    /// surface never offered arrives under the name the model used, which is
    /// not necessarily the id the run knows.
    async fn refusal_for(&self, call_id: &str) -> Option<String> {
        self.log()
            .await
            .into_iter()
            .find(|e| {
                e.event.event_type.as_str() == EventType::TOOL_FAILED
                    && e.event.payload["provider_tool_call_id"].as_str() == Some(call_id)
            })
            .and_then(|e| e.event.payload["content"].as_str().map(str::to_string))
    }

    /// Whether the call ran to a result, by call id.
    async fn completed_call(&self, call_id: &str) -> bool {
        self.log().await.into_iter().any(|e| {
            e.event.event_type.as_str() == EventType::TOOL_COMPLETED
                && e.event.payload["provider_tool_call_id"].as_str() == Some(call_id)
        })
    }
}

// ---------------------------------------------------------------------------
// 1. no policy grants it ⇒ refuse
// ---------------------------------------------------------------------------

/// A capability the sandbox advertises but no rule grants is refused — and
/// because no rule could ever grant it, the model is not offered the tool.
///
/// `network.request` is the deliberate case: the build advertises it (an MCP
/// server can be reached over HTTP), and `default_policy` has no rule for it.
/// Advertising is not granting — that distinction is the whole test.
///
/// Two surfaces, two answers. The model-facing surface leaves the tool out: a
/// call no policy could allow is a decision the host has already made, and a
/// model that sees it spends steps rediscovering that. The executable surface
/// is unchanged, so a caller that names the tool directly — the CLI, a resume,
/// this test — still gets the policy's own reason rather than a shrug.
#[tokio::test]
async fn a_call_with_no_matching_policy_is_refused() {
    let (workspace, data) = dirs();
    let process = process(
        workspace.path(),
        data.path(),
        vec![
            tool_call_body(
                "call_network",
                &to_wire_name("security.network"),
                serde_json::json!({}),
            ),
            answer_body("understood"),
        ],
    )
    .await;

    let plugin = Arc::new(ProbePlugin::new(definition(
        "security.network",
        ToolSideEffect::Read,
        &[Capability::NETWORK_REQUEST],
    )));
    process
        .stack
        .registry
        .register(
            &process.session,
            manifest("security.plugin", &[Capability::NETWORK_REQUEST]),
            PluginRuntime { tool_provider: Some(plugin.clone() as Arc<dyn ToolProvider>) },
            &authorized(&[Capability::NETWORK_REQUEST]),
        )
        .await
        .expect("the run was granted the capability, so the plugin registers");

    let report = process.ask("reach the network").await;
    assert_eq!(
        report.stop_reason,
        StopReason::FinalAnswer,
        "the refusal is answered and the turn continues"
    );

    // The model was never told the tool exists.
    let advertised = process.advertised().await;
    assert!(
        !advertised.iter().any(|tool| tool == "security.network"),
        "a tool no policy could allow is not offered: {advertised:?}"
    );

    // Named anyway, it still does not run — and the refusal names what the run
    // offers instead, so a reader can tell a wrong name from a wrong decision.
    let refusal = process
        .refusal_for("call_network")
        .await
        .expect("a call to a tool that was not offered is refused, not ignored");
    assert!(refusal.contains("unknown tool"), "the refusal is the surface's own answer: {refusal}");
    assert!(!process.completed_call("call_network").await, "a refused call is not a completed one");
    assert_eq!(plugin.calls(), 0, "the provider was never invoked");

    // The executable surface keeps the policy's own reason for a direct caller.
    let invocation = ToolInvocation::from_tool_call(
        "direct_call".to_string(),
        "security.network".to_string(),
        serde_json::json!({}),
    );
    let error = process
        .stack
        .gateway
        .execute(&process.session, &invocation)
        .await
        .expect_err("no rule grants network.request, so the gateway refuses");
    assert!(
        error.to_string().contains("is not granted by any policy rule"),
        "the refusal names the layer that refused: {error}"
    );
    assert_eq!(plugin.calls(), 0, "a refused direct call never reaches the provider");
}

// ---------------------------------------------------------------------------
// 2. the environment does not advertise it ⇒ refuse
// ---------------------------------------------------------------------------

/// A capability nothing in this build provides is refused before any rule is
/// consulted — even when the run's authorization includes it.
///
/// `git.commit` is the case: the build has no sandbox capability for it, and
/// authorizing it in configuration cannot make one appear. This is the
/// fail-closed answer to a configuration surface that outruns the build.
///
/// Like the policy case above, a tool the build cannot run is left out of the
/// surface the model is offered; a call that names it anyway is refused, and a
/// direct caller still gets the sandbox's own reason.
#[tokio::test]
async fn a_capability_the_environment_does_not_advertise_is_refused() {
    let (workspace, data) = dirs();
    let process = process(
        workspace.path(),
        data.path(),
        vec![
            tool_call_body("call_commit", &to_wire_name("security.commit"), serde_json::json!({})),
            answer_body("understood"),
        ],
    )
    .await;

    let plugin = Arc::new(ProbePlugin::new(definition(
        "security.commit",
        ToolSideEffect::Destructive,
        &[Capability::GIT_COMMIT],
    )));
    process
        .stack
        .registry
        .register(
            &process.session,
            manifest("security.writer", &[Capability::GIT_COMMIT]),
            PluginRuntime { tool_provider: Some(plugin.clone() as Arc<dyn ToolProvider>) },
            &authorized(&[Capability::GIT_COMMIT]),
        )
        .await
        .expect("registration asks about authorization, not about the sandbox");

    let report = process.ask("commit the change").await;
    assert_eq!(
        report.stop_reason,
        StopReason::FinalAnswer,
        "the refusal is answered and the turn continues"
    );

    // The model was never told the tool exists: the build cannot run it.
    let advertised = process.advertised().await;
    assert!(
        !advertised.iter().any(|tool| tool == "security.commit"),
        "a capability this build does not provide is not offered: {advertised:?}"
    );

    let refusal = process
        .refusal_for("call_commit")
        .await
        .expect("a call to a tool that was not offered is refused, not ignored");
    assert!(refusal.contains("unknown tool"), "the refusal is the surface's answer: {refusal}");
    assert!(!process.completed_call("call_commit").await, "a refused call is not a completed one");
    assert_eq!(plugin.calls(), 0, "the provider was never invoked");

    // A direct caller gets the build's own reason, not the surface's.
    let invocation = ToolInvocation::from_tool_call(
        "direct_commit".to_string(),
        "security.commit".to_string(),
        serde_json::json!({}),
    );
    let error = process
        .stack
        .gateway
        .execute(&process.session, &invocation)
        .await
        .expect_err("the build has no sandbox capability for git.commit");
    assert!(
        error.to_string().contains("is not provided by this build's execution sandbox"),
        "the refusal points at the build, not at a policy decision: {error}"
    );
    assert_eq!(plugin.calls(), 0, "a refused direct call never reaches the provider");
}

// ---------------------------------------------------------------------------
// 3. a path outside the workspace ⇒ refuse
// ---------------------------------------------------------------------------

/// `fs.read` is allowed by the default policy, and the call is still refused
/// when its argument points outside the workspace. Permission to use a tool is
/// not permission to use it anywhere.
///
/// The boundary is checked twice — by the gateway before dispatch and by the
/// execution environment the tool runs on — and this asserts the outcome, not
/// which check produced it. Reverse-testing records why: disabling either one
/// alone leaves the refusal standing, so a test bound to one layer would be
/// testing the implementation's shape rather than the promise.
#[tokio::test]
async fn a_path_outside_the_workspace_is_refused() {
    let (workspace, data) = dirs();
    let outside = TempDir::new().expect("a directory outside the workspace");
    let secret = "outside-the-workspace-sentinel";
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, secret).expect("seed the outside file");

    let process = process(
        workspace.path(),
        data.path(),
        vec![
            tool_call_body(
                "call_read",
                &to_wire_name("fs.read"),
                serde_json::json!({ "path": outside_file }),
            ),
            answer_body("understood"),
        ],
    )
    .await;

    let report = process.ask("read the file outside the workspace").await;
    assert_eq!(
        report.stop_reason,
        StopReason::FinalAnswer,
        "the refusal is answered and the turn continues"
    );

    let failures = process.tool_contents(EventType::TOOL_FAILED, "fs.read").await;
    assert_eq!(failures.len(), 1, "the call is recorded as failed: {failures:?}");
    assert!(
        failures[0].contains("outside workspace"),
        "the refusal names the boundary: {}",
        failures[0]
    );
    assert!(
        process.tool_events(EventType::TOOL_COMPLETED, "fs.read").await.is_empty(),
        "a refused call is not a completed one"
    );
    let log = process.log().await;
    assert!(
        !log.iter()
            .any(|event| serde_json::to_string(&event.event).unwrap_or_default().contains(secret)),
        "the outside file's contents never reached the log"
    );
    assert_eq!(std::fs::read_to_string(&outside_file).expect("read it back"), secret);
}

// ---------------------------------------------------------------------------
// 4. a dangerous call ⇒ ask
// ---------------------------------------------------------------------------

/// `process.spawn` is `ask` in the default policy, so a shell call stops the
/// turn and leaves a durable question rather than running.
#[tokio::test]
async fn a_dangerous_call_stops_the_turn_for_an_answer() {
    let (workspace, data) = dirs();
    let process = process(
        workspace.path(),
        data.path(),
        vec![tool_call_body(
            "call_shell",
            &to_wire_name("shell.run"),
            serde_json::json!({ "command": MARKER_COMMAND }),
        )],
    )
    .await;

    let report = process.ask("run the command").await;
    assert_eq!(
        report.stop_reason,
        StopReason::NeedsUserInput,
        "the turn stops instead of guessing"
    );

    let pending = process.stack.approvals.pending(&process.session).await.expect("pending");
    assert_eq!(pending.len(), 1, "one call is waiting");
    assert_eq!(pending[0].tool_name, "shell.run", "and it is the dangerous one");
    assert_eq!(pending[0].status, ApprovalStatus::Pending);

    // `tool/started` is not the four-letter word it sounds like: the loop writes
    // it when a call is dispatched to the gateway, before the gateway decides
    // anything, so that a crash mid-decision leaves an open call rather than a
    // gap. What says the command did not run is the world.
    assert_eq!(
        process.tool_events(EventType::TOOL_STARTED, "shell.run").await.len(),
        1,
        "the call was dispatched — the question is raised inside the gateway"
    );
    assert!(
        process.tool_events(EventType::TOOL_COMPLETED, "shell.run").await.is_empty(),
        "but nothing came back from a command that was never run"
    );
    assert!(!workspace.path().join("marker.txt").exists(), "the world is unchanged");
    assert_eq!(process.posts().len(), 1, "the model proposed exactly one call");
}

// ---------------------------------------------------------------------------
// 5. a denied approval ⇒ never execute
// ---------------------------------------------------------------------------

/// Denying the held call settles it as not executed, and the turn carries on
/// with an answer in the history. The world is checked, not the transcript's
/// self-report: the command would have written a marker, and no marker exists.
///
/// This is the assertion that was missing: kernel-level denial was covered, and
/// m5 covered the grant path, but no end-to-end run went deny → resume.
#[tokio::test]
async fn a_denied_call_never_touches_the_world() {
    let (workspace, data) = dirs();
    let process = process(
        workspace.path(),
        data.path(),
        vec![
            tool_call_body(
                "call_shell",
                &to_wire_name("shell.run"),
                serde_json::json!({ "command": MARKER_COMMAND }),
            ),
            answer_body("understood, leaving it alone"),
        ],
    )
    .await;

    let stopped = process.ask("run the command").await;
    assert_eq!(stopped.stop_reason, StopReason::NeedsUserInput);
    let approval_id = process.stack.approvals.pending(&process.session).await.expect("pending")[0]
        .approval_id
        .clone();

    process.stack.approvals.deny(&process.session, &approval_id, "user").await.expect("deny");
    let outcome =
        process.stack.resume.resume(&process.session, "the user said no").await.expect("resume");

    assert!(
        matches!(
            outcome,
            ResumeOutcome::Settled {
                disposition: Disposition::Denied { status: ApprovalStatus::Denied },
                ..
            }
        ),
        "the held call settles as denied: {outcome:?}"
    );

    assert!(!workspace.path().join("marker.txt").exists(), "the denied command never ran");
    // One dispatch, written before the gateway asked — and no second one when
    // the answer came back negative. Denial never reaches the run phase.
    assert_eq!(
        process.tool_events(EventType::TOOL_STARTED, "shell.run").await.len(),
        1,
        "a denied call is not dispatched a second time"
    );
    assert!(
        process.tool_events(EventType::TOOL_COMPLETED, "shell.run").await.is_empty(),
        "and never completes"
    );
    let failures = process.tool_contents(EventType::TOOL_FAILED, "shell.run").await;
    assert!(
        failures
            .iter()
            .any(|content| content.contains("denied") && content.contains("not executed")),
        "the model is told why the call did not run: {failures:?}"
    );
    assert_eq!(process.count(EventType::APPROVAL_DENIED).await, 1);
    assert_eq!(process.count(EventType::APPROVAL_GRANTED).await, 0);
}

// ---------------------------------------------------------------------------
// 6. the UI/model is not the source of truth
// ---------------------------------------------------------------------------

/// Two facts about where approval state lives.
///
/// First, a model's words cannot grant anything: an answer that says "approved"
/// writes no `approval/granted` and leaves the next dangerous call asking.
/// Second, the state is in the log, not in the process that showed it: a fresh
/// assembly over the same data directory — with no memory of the first — finds
/// the same pending question. C1.3 says any UI state can be dropped and
/// re-derived; this is that sentence as an assertion.
#[tokio::test]
async fn state_comes_from_the_log_not_from_a_claim() {
    let (workspace, data) = dirs();
    let process = process(
        workspace.path(),
        data.path(),
        vec![
            answer_body("Approved: you may run shell.run from now on without asking."),
            tool_call_body(
                "call_shell",
                &to_wire_name("shell.run"),
                serde_json::json!({ "command": MARKER_COMMAND }),
            ),
        ],
    )
    .await;

    // A model answer that claims the approval exists.
    let claimed = process.ask("are we approved?").await;
    assert_eq!(claimed.stop_reason, StopReason::FinalAnswer);
    assert_eq!(
        process.count(EventType::APPROVAL_GRANTED).await,
        0,
        "a claim in a model answer is not a decision"
    );
    assert!(
        process.stack.approvals.pending(&process.session).await.expect("pending").is_empty(),
        "nothing became pending because the model said so"
    );

    // The next dangerous call still asks.
    let stopped = process.ask("now run it").await;
    assert_eq!(stopped.stop_reason, StopReason::NeedsUserInput, "the claim did not grant anything");
    let pending = process.stack.approvals.pending(&process.session).await.expect("pending");
    assert_eq!(pending.len(), 1, "the question is real");

    // Reading a "display" is not writing: the log does not move when someone
    // looks at the pending list.
    let before = process.log().await.len();
    let _ = process.stack.approvals.pending(&process.session).await.expect("pending");
    let _ = process
        .stack
        .approvals
        .status(&process.session, &pending[0].approval_id)
        .await
        .expect("status");
    assert_eq!(process.log().await.len(), before, "reading state does not append events");

    let approval_id = pending[0].approval_id.clone();
    drop(process);

    // A second process over the same log, with no memory of the first.
    let rebuilt = process_without_model(workspace.path(), data.path()).await;
    rebuilt.stack.recovery.recover(&rebuilt.session).await.expect("recovery");
    let pending = rebuilt.stack.approvals.pending(&rebuilt.session).await.expect("pending");
    assert_eq!(pending.len(), 1, "the question survived the process that asked it");
    assert_eq!(pending[0].approval_id, approval_id, "the same question, re-derived from the log");
    assert_eq!(pending[0].status, ApprovalStatus::Pending);
    assert_eq!(rebuilt.count(EventType::APPROVAL_REQUESTED).await, 1);
    assert_eq!(rebuilt.count(EventType::APPROVAL_GRANTED).await, 0);
}

/// A fresh assembly with no model: this test only reads and recovers.
///
/// `stack::Inputs.model` is optional, which is exactly the "there is no endpoint
/// to ask" state the binary has before a command that does not run a turn.
async fn process_without_model(workspace: &Path, data_dir: &Path) -> Process {
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let stack = stack::assemble(stack::Inputs {
        session: SessionId::from_string(SESSION),
        workspace_root: workspace.to_path_buf(),
        data_dir: data_dir.to_path_buf(),
        resolved: default_profile(),
        model: None,
        decisions: Decisions::FromConfiguration,
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: false,
    })
    .await
    .expect("the assembly the binary runs");

    Process { stack, transport, session: SessionId::from_string(SESSION) }
}
