//! M6 acceptance: the plugin ecosystem, end to end.
//!
//! Five claims are supposed to be true of this milestone. Each one below is
//! answered by running the thing and reading what it left behind, not by
//! asking the run whether it worked:
//!
//! | M6 criterion | how it is evidenced |
//! |---|---|
//! | an unauthorized plugin registers nothing | the registry refuses, and its own surface, its generation and the session's log are all unchanged |
//! | a plugin's tools and a built-in's go through one policy | the same two tools are asked, then denied, and each time the recorded shape is the same shape |
//! | disabling a plugin does not touch the turn already running | a real turn is held mid-flight while `agent plugin disable` runs in another process; the held turn's two requests carry an identical tool list, the next turn's request carries none of that server's tools, and a later process starts without it |
//! | a second MCP server is configuration, not code | two servers start from one configuration file and both answer |
//! | with the sidecar off, the baseline answers and the log replays | the trace names the rule baseline, carries no sidecar report, and re-derives its own conclusion |
//!
//! The first and the in-process half of the third run over the real kernel, for
//! the reason `m5_acceptance` gives: the machinery between the call and the log
//! is production code down to the last line. The third fakes only the network;
//! the first never reaches one, because the refusal it is about happens before a
//! turn exists. The second, fourth and fifth drive the assembled binary with
//! real child processes, because what they are about is configuration reaching a
//! process — a capability list, a profile, a server entry.
//!
//! ## How the third anchor is triggered
//!
//! Twice, from two sides. The end-to-end test presses the product's own button:
//! a real turn is stopped inside itself by the fake endpoint, `agent plugin
//! disable` runs as a second process against the same session, and the request
//! bodies the endpoint wrote down show the frozen manifest of the held turn and
//! the shrunken one of the next. The in-process test above it stays because it
//! reaches a seam the end-to-end one cannot — a *tool* that retires a plugin
//! while the turn that called it is still running, which is the same freeze
//! entered from the other end.
//!
//! One limit is recorded rather than hidden. Every process builds the plugin
//! surface its own configuration names, so a runtime change binds the runs that
//! cross a boundary after it — not the configuration file, which still enables
//! the server and which the test leaves untouched. What makes the change outlive
//! its process anyway is the session log: a later assembly reads the last thing
//! said about each plugin, and a withdrawal the operator asked for keeps it from
//! being started at all. `agent plugin enable` is the way back, and it works by
//! the same rule read in the other direction. The end of the test walks both
//! directions and asserts what each later process actually offered the model,
//! so that the day the rule stops holding is the day a test fails rather than
//! the day someone notices.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile};
use agent_kernel::plugin::{PluginError, PluginHandle, PluginRegistry, PluginRuntime};
use agent_model_openai::{
    OpenAiCompatConfig, OpenAiCompatProvider, Transport, TransportError, TransportRequest,
    TransportResponse, to_wire_name,
};
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::ids::{EventSeq, PluginId, SessionId};
use agent_protocol::permissions::{Capability, CapabilitySet};
use agent_protocol::plugin::{PluginCapabilities, PluginContributions, PluginManifest};
use agent_store_jsonl::JsonlEventStore;
use agent_testkit::FixedSecret;
use chrono::Utc;
use serde_json::Value;
use tempfile::TempDir;

/// The session every in-process test here runs under.
const SESSION: &str = "m6-acceptance";

/// The capabilities a run has when a test wants a plugin to be admitted.
fn authorized(capabilities: &[&str]) -> CapabilitySet {
    capabilities.iter().map(|c| Capability::new(*c)).collect()
}

fn manifest(id: &str, requires: &[&str]) -> PluginManifest {
    PluginManifest {
        schema_version: agent_protocol::versioning::formats::PLUGIN_MANIFEST,
        id: PluginId::from_string(id),
        version: "1.0.0".to_string(),
        // The version the *product* declares, read from it rather than copied:
        // "a plugin speaking this build's API registers" is the claim, and a
        // copy that stayed at the old number would quietly be a different one.
        plugin_api: agent::plugins::PLUGIN_API.to_string(),
        capabilities: PluginCapabilities {
            requires: requires.iter().map(|c| Capability::new(*c)).collect(),
        },
        contributions: PluginContributions { tool_provider: true, ..Default::default() },
        config_schema: None,
    }
}

// ---------------------------------------------------------------------------
// The one fake: bytes on a wire
// ---------------------------------------------------------------------------

/// A transport that replays response bodies in order and keeps what it was
/// asked to send.
///
/// Running out of scripted responses is an error rather than a hang: a test
/// that expected three model calls and got four should fail loudly.
struct ScriptedTransport {
    bodies: Mutex<std::collections::VecDeque<String>>,
    posts: Mutex<Vec<String>>,
}

impl ScriptedTransport {
    fn new(bodies: Vec<String>) -> Self {
        Self { bodies: Mutex::new(bodies.into()), posts: Mutex::new(Vec::new()) }
    }

    /// Every request body that reached the wire, in order, as text.
    fn posts(&self) -> Vec<String> {
        self.posts.lock().map(|posts| posts.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Transport for ScriptedTransport {
    async fn post(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        if let Ok(mut posts) = self.posts.lock() {
            posts.push(String::from_utf8_lossy(&request.body).to_string());
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

fn sse(lines: &[&str]) -> String {
    lines.iter().map(|line| format!("{line}\n\n")).collect()
}

/// A streamed tool call, as an endpoint would send one.
fn tool_call_body(call_id: &str, tool: &str, arguments: Value) -> String {
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

// ---------------------------------------------------------------------------
// A plugin that contributes tools
// ---------------------------------------------------------------------------

/// A tool surface a test hands to the registry, in place of a real plugin.
///
/// The registry never constructs a contribution — the caller passes one in —
/// so a plugin here is exactly the value a real one would produce: definitions
/// and a way to run them.
struct TestPlugin {
    definition: ToolDefinition,
    /// Counted so a test can tell "the tool ran" from "the tool was offered".
    calls: Arc<AtomicUsize>,
    /// What `invoke` does, if it does anything.
    effect: Option<Retire>,
}

/// Retire another plugin: the act this milestone's third claim is about.
struct Retire {
    handle: Arc<Mutex<Option<PluginHandle>>>,
}

impl TestPlugin {
    fn new(definition: ToolDefinition) -> Self {
        Self { definition, calls: Arc::new(AtomicUsize::new(0)), effect: None }
    }

    fn retiring(definition: ToolDefinition, handle: Arc<Mutex<Option<PluginHandle>>>) -> Self {
        Self { definition, calls: Arc::new(AtomicUsize::new(0)), effect: Some(Retire { handle }) }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// A definition contributed by a test plugin.
///
/// `requires` is stated per call rather than filled in with the empty set: a
/// tool that declares no capabilities is refused by the gateway before it runs
/// (YA-07), so an omitted declaration is no longer a neutral way to describe a
/// fixture — it is a claim that nothing can grant the tool anything.
fn definition(name: &str, side_effect: ToolSideEffect, requires: &[&str]) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("{name}, contributed by a test plugin"),
        // Deliberately not what the registry will report: a registry stamps the
        // provenance of what it hands out, and a plugin does not get to say
        // where it came from.
        source: ToolSource::Builtin,
        side_effect,
        capability_requirements: requires.iter().map(|c| Capability::new(*c)).collect(),
        idempotent: true,
        timeout_ms: None,
        input_schema: serde_json::json!({ "type": "object" }),
    }
}

#[async_trait::async_trait]
impl ToolProvider for TestPlugin {
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

        if let Some(Retire { handle }) = &self.effect {
            // Taken out of the slot and dropped before the await: a `std` mutex
            // guard held across one is not something to be casual about, and the
            // handle is only good for one retirement anyway.
            let handle = handle.lock().ok().and_then(|mut slot| slot.take());
            if let Some(handle) = handle {
                handle.disable().await.map_err(|error| ToolProviderError::Failed {
                    tool: self.definition.name.clone(),
                    message: error.to_string(),
                })?;
            }
        }

        Ok(ToolResult::completed(format!("{} ran", self.definition.name)))
    }
}

/// One process's worth of the stack, over a log that outlives it.
///
/// The stack is [`stack::assemble`] — the function the binary itself calls, not
/// a copy of what it does — so the registry under test is the one `apps/agent`
/// builds, and the names it reserves are the ones the product reserves: read off
/// the built-in providers' own definitions, plus the delegation tool when a run
/// has one. What the process environment would have supplied, the test supplies:
/// where the run's files live, and a scripted model in place of an endpoint.
struct Harness {
    stack: stack::Runtime,
    transport: Arc<ScriptedTransport>,
    session: SessionId,
}

impl Harness {
    async fn new(data: &Path, script: Vec<String>) -> Self {
        let transport = Arc::new(ScriptedTransport::new(script));
        let stack = stack::assemble(stack::Inputs {
            session: SessionId::from_string(SESSION),
            // One directory for both, as this suite has always had it: these
            // tests turn on the plugin surface, not on where the boundary sits
            // relative to the log.
            workspace_root: data.to_path_buf(),
            data_dir: data.to_path_buf(),
            resolved: default_profile(),
            model: Some(SelectedProvider {
                provider: scripted_provider(transport.clone()),
                route: route(),
            }),
            decisions: Decisions::FromConfiguration,
            sandbox: SandboxMode::LocalProcess,
            network_allowed: false,
            judgment: true,
        })
        .await
        .expect("the assembly the binary runs");

        Self { stack, transport, session: SessionId::from_string(SESSION) }
    }

    /// Queue `text` as the next turn's input and run it.
    async fn ask(&self, text: &str) {
        self.stack
            .scheduler
            .inbox()
            .append(
                &self.session,
                agent_protocol::inbox::InboxKind::NextTurn,
                serde_json::json!({ "text": text }),
                0,
                agent_protocol::events::Actor::User,
            )
            .await
            .expect("queue the turn");
        self.stack
            .scheduler
            .run_pending(&self.session, Utc::now())
            .await
            .expect("run pending")
            .expect("a turn ran");
    }

    async fn log(&self) -> Vec<agent_protocol::events::StoredEvent> {
        self.stack
            .store
            .read_stream(&self.session, EventSeq::ZERO, usize::MAX)
            .await
            .expect("read the log")
    }

    /// The `tools` array of each request that reached the wire.
    ///
    /// Parsed rather than pattern-matched: the claim is that two requests carry
    /// the *same* manifest, and comparing the parsed values is comparing the
    /// bytes without a JSON library's key ordering getting a vote.
    fn offered(&self) -> Vec<Value> {
        self.transport
            .posts()
            .iter()
            .map(|body| {
                let body: Value =
                    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON: {body}: {e}"));
                body["tools"].clone()
            })
            .collect()
    }
}

/// The configuration a run in a directory with no configuration has.
///
/// The binary reads three layers — managed policy, user configuration, project
/// configuration — off the environment and the disk. A test can do neither: this
/// edition makes `set_var` unsafe, and the environment belongs to the whole
/// process. So it states the same input the loader would have produced, through
/// the same `resolve`, and the assembly above is the same one either way.
fn default_profile() -> ResolvedProfile {
    agent_config::resolve(Layers::default()).expect("the defaults resolve")
}

// ---------------------------------------------------------------------------
// M6 anchor 1 — an unauthorized plugin registers nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_requiring_what_it_was_not_granted_registers_nothing() {
    let data = TempDir::new().unwrap();
    let store = Arc::new(JsonlEventStore::new(data.path()));
    let registry = Arc::new(PluginRegistry::new(store.clone()));
    let session = SessionId::from_string(SESSION);

    // The plugin wants the network. The run has been granted `process.spawn`
    // and nothing else.
    let plugin = Arc::new(TestPlugin::new(definition(
        "m6.greedy.ping",
        ToolSideEffect::Read,
        &[Capability::NETWORK_REQUEST],
    )));
    let runtime = PluginRuntime { tool_provider: Some(plugin.clone() as Arc<dyn ToolProvider>) };

    let refused = registry
        .register(
            &session,
            manifest("m6.greedy", &["network.request"]),
            runtime,
            &authorized(&["process.spawn"]),
        )
        .await
        .expect_err("an ungranted capability is a refusal");

    let PluginError::Unauthorized { missing, .. } = &refused else {
        panic!("expected the authorization gate to refuse, got {refused}");
    };
    assert_eq!(
        missing.iter().map(Capability::as_str).collect::<Vec<_>>(),
        vec!["network.request"],
        "the refusal names the capability, which is the only thing the operator can act on"
    );

    // The world, not the refusal: nothing was contributed, nothing advanced,
    // and the session's log does not mention a plugin it never loaded.
    assert!(registry.tool_definitions().is_empty(), "a refused plugin contributed a tool");
    assert_eq!(
        registry.generation(),
        0,
        "the surface changed by a registration that did not happen"
    );
    assert!(registry.registered_ids().is_empty());
    let log = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.expect("read the log");
    assert!(
        log.is_empty(),
        "a refusal is a decision taken from configuration, not a fact about the session: {:?}",
        log.iter().map(|e| e.event.event_type.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(plugin.calls(), 0, "nothing ran, so nothing was invoked");
}

// ---------------------------------------------------------------------------
// M6 anchor 1b — a plugin cannot take a name the run already has
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_that_takes_a_built_in_name_is_refused_and_the_built_in_still_answers() {
    let data = TempDir::new().unwrap();
    // The harness roots its execution environment here, so this is the file the
    // built-in `fs.read` will be asked for.
    std::fs::write(data.path().join("note.txt"), "the real file").unwrap();

    // Turn one: read the built-in's own file, then answer. The names the run
    // already has are not stated here at all: the assembly derives them from the
    // built-in providers' own definitions, and its refusal of the impostor below
    // is the evidence that it did.
    let harness = Harness::new(
        data.path(),
        vec![
            tool_call_body(
                "call_read",
                &to_wire_name("fs.read"),
                serde_json::json!({"path": "note.txt"}),
            ),
            answer_body("read it"),
        ],
    )
    .await;

    // A plugin offering `fs.read` would be offering a name the model is already
    // offered, and the call would go to the built-in whatever the plugin did.
    let impostor = Arc::new(TestPlugin::new(definition(
        "fs.read",
        ToolSideEffect::Read,
        &[Capability::FS_READ],
    )));
    let error = harness
        .stack
        .registry
        .register(
            &harness.session,
            manifest("m6.impostor", &[]),
            PluginRuntime { tool_provider: Some(impostor.clone() as Arc<dyn ToolProvider>) },
            &authorized(&[]),
        )
        .await
        .expect_err("a built-in name is refused");

    assert!(
        matches!(&error, PluginError::BuiltinNameTaken { tool_name, .. }
            if tool_name == "fs.read"),
        "{error:?}"
    );
    assert!(
        harness.stack.registry.registered_ids().is_empty(),
        "the impostor left something behind"
    );
    assert_eq!(harness.stack.registry.generation(), 0);

    // And the name still works: refused is not the same as missing.
    harness.ask("read the note").await;
    let log = harness.log().await;
    let answered = log.iter().find(|e| e.event.event_type.as_str() == "tool/completed");
    let answered = answered.unwrap_or_else(|| {
        panic!(
            "the built-in did not answer: {:?}",
            log.iter().map(|e| e.event.event_type.as_str()).collect::<Vec<_>>()
        )
    });
    assert!(
        answered.event.payload.to_string().contains("the real file"),
        "the answer was not the file's: {}",
        answered.event.payload
    );
    assert_eq!(impostor.calls(), 0, "the impostor never ran");
}

// ---------------------------------------------------------------------------
// M6 anchor 2 — one policy, whichever door the tool came through
// ---------------------------------------------------------------------------

/// The configuration variables the binary documents: the name an operator's
/// script writes, stated here rather than read off the product, so that a
/// rename on both sides cannot pass unnoticed.
const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// A fixture binary, an `examples/` target of the package it names.
///
/// It is an example rather than a `src/bin/` binary so that `cargo build
/// --workspace` does not put a test double where product binaries go; it is
/// `cargo test` that builds examples, and that is how the gates run. A run that
/// does not build this package's examples builds one here.
fn fixture_bin(package: &str, name: &str) -> PathBuf {
    let agent = PathBuf::from(env!("CARGO_BIN_EXE_agent"));
    let directory = agent.parent().expect("the binary is in a directory").join("examples");
    let path = directory.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() });
    if path.exists() {
        return path;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // Build into the same target directory this test binary lives under: a
    // coverage build relocates it, and a default build would not be found here.
    let profile = directory.parent().expect("the profile directory");
    let mut build = Command::new(cargo);
    build
        .args(["build", "-p", package, "--example", name])
        .arg("--target-dir")
        .arg(profile.parent().expect("the target directory"))
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.ends_with("release") {
        build.arg("--release");
    }
    let built = build.status().expect("cargo is on the path");
    assert!(built.success(), "the {name} fixture could not be built");
    assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    path
}

struct Ran {
    ok: bool,
    out: String,
    err: String,
}

fn dirs() -> (TempDir, TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap())
}

/// One run of the assembled binary under a user configuration, and nothing else.
///
/// The configuration variables are cleared first: a developer with a user
/// configuration exported would have every acceptance run here quietly accepting
/// something else.
fn ran(data: &Path, workspace: &Path, config: &Path, argv: &[&str]) -> Ran {
    let output = Command::new(env!("CARGO_BIN_EXE_agent"))
        .env("AGENT_DATA_DIR", data)
        .env("AGENT_WORKSPACE_DIR", workspace)
        .env("AGENT_MODEL_PROVIDER", "echo")
        .env(ENV_USER_CONFIG, config)
        .env_remove(ENV_MANAGED_POLICY)
        .args(argv)
        .output()
        .expect("the binary starts");
    Ran {
        ok: output.status.success(),
        out: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        err: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

fn write_config(home: &TempDir, text: &str) -> PathBuf {
    let path = home.path().join("user.toml");
    std::fs::write(&path, text).expect("a user configuration");
    path
}

fn events(data: &Path, session: &str) -> Vec<Value> {
    let path = data.join(session).join("events.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("no log at {}: {e}", path.display());
    });
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let record: Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"));
            record["event"].clone()
        })
        .collect()
}

/// Everything the session recorded that names this tool, in order.
///
/// Per tool on purpose: two tools leaving the same trail is the claim, and a
/// test that pooled them could not tell that from one tool leaving two trails.
fn trail(events: &[Value], tool: &str) -> Vec<Value> {
    events.iter().filter(|e| e["payload"]["tool_name"] == tool).cloned().collect()
}

/// The event types and payload field names of one trail, as one comparable
/// shape. Values differ between two calls — a tool name, an argument summary,
/// a fresh id — and the shape is what this anchor is about.
fn shape(trail: &[Value]) -> Vec<(String, Vec<String>)> {
    trail
        .iter()
        .map(|event| {
            let mut keys: Vec<String> = event["payload"]
                .as_object()
                .map(|fields| fields.keys().cloned().collect())
                .unwrap_or_default();
            keys.sort();
            (event["event_type"].as_str().unwrap_or("?").to_string(), keys)
        })
        .collect()
}

#[test]
fn a_plugin_tool_and_a_built_in_tool_are_judged_by_the_same_policy() {
    let server = fixture_bin("agent-tools-mcp", "fake-mcp-server");

    // Two tools from two doors — `shell.run` is built in, `mcp__fake__echo`
    // arrived through the plugin registry — declaring the same capability,
    // which is what makes them the same question to the policy engine.
    let config = |disposition: &str| {
        format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.fake\"]\n\n\
             [mcp.servers.fake]\n\
             command = '{command}'\n\
             requires = [\"process.spawn\"]\n\n\
             [profiles.p]\n\
             tools = {{ allow = [\"process.spawn\"] }}\n\
             policy = {{ shell = \"{disposition}\" }}\n\
             plugins = {{ enable = [\"mcp.fake\"] }}\n",
            command = server.display(),
        )
    };
    let calls: [(&str, &str); 2] =
        [("shell.run", r#"{"command":"echo hi"}"#), ("mcp__fake__echo", r#"{"text":"hi"}"#)];

    // --- Asked: neither runs until a person says so, and both say so the same
    // way. ---
    let (asked_data, workspace, home) = dirs();
    let asked = write_config(&home, &config("ask"));
    for (tool, arguments) in calls {
        let r = ran(
            asked_data.path(),
            workspace.path(),
            &asked,
            &["--profile", "p", "tool", tool, arguments],
        );
        assert!(!r.ok, "{tool} ran without the decision it was supposed to wait for: {}", r.out);
        assert!(r.err.contains("approval pending"), "{tool}: {}", r.err);
        assert!(r.err.contains(tool), "the question names the tool it is about: {}", r.err);
    }

    let asked_events = events(asked_data.path(), "default");
    let shell = trail(&asked_events, "shell.run");
    let mcp = trail(&asked_events, "mcp__fake__echo");
    assert_eq!(
        shape(&shell),
        shape(&mcp),
        "the same capability, judged twice, left two different shapes"
    );
    assert_eq!(shell.len(), 1, "asking is one event per call: {shell:#?}");
    assert_eq!(
        shell[0]["payload"]["capability"], "process.spawn",
        "the question is about the capability, not about which door the tool came through"
    );
    assert_eq!(shell[0]["payload"]["capability"], mcp[0]["payload"]["capability"]);

    // --- Denied: the same two tools, refused, with nothing recorded about
    // either. A policy that reached only one of the two doors would show here.
    let (denied_data, workspace, home) = dirs();
    let denied = write_config(&home, &config("deny"));
    for (tool, arguments) in calls {
        let r = ran(
            denied_data.path(),
            workspace.path(),
            &denied,
            &["--profile", "p", "tool", tool, arguments],
        );
        assert!(!r.ok, "{tool} ran under a deny: {}", r.out);
        // The refusal is the same sentence with a different name in it — and it
        // names the layer, because "denied by policy" covered four different
        // layers here and a reader cannot fix a layer the message will not name.
        // This one came from the profile's own rule.
        let expected = format!(
            "policy denied tool '{tool}': capability process.spawn is denied by a policy rule"
        );
        assert!(r.err.contains(&expected), "{tool}: {}", r.err);
    }
    let denied_events = events(denied_data.path(), "default");
    assert!(trail(&denied_events, "shell.run").is_empty(), "a denied call was recorded as a call");
    assert!(trail(&denied_events, "mcp__fake__echo").is_empty());
}

// ---------------------------------------------------------------------------
// M6 anchor 3 — the turn in flight keeps the surface it started with
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_turn_keeps_the_manifest_it_started_with_when_a_plugin_is_retired() {
    let data = TempDir::new().unwrap();

    // Turn one: call the tool that retires the other plugin, then answer.
    // Turn two: answer.
    let harness = Harness::new(
        data.path(),
        vec![
            tool_call_body("call_retire", &to_wire_name("m6.retire"), serde_json::json!({})),
            answer_body("retired"),
            answer_body("carried on"),
        ],
    )
    .await;

    // Two plugins. One contributes a marker that does nothing; the other
    // contributes the tool that retires it. Both declare the capability they
    // claim — without one the gateway refuses the call before the plugin is
    // reached, and this test is about the surface, not the gate.
    let marker = Arc::new(TestPlugin::new(definition(
        "m6.marker",
        ToolSideEffect::Read,
        &[Capability::FS_READ],
    )));
    let slot: Arc<Mutex<Option<PluginHandle>>> = Arc::new(Mutex::new(None));
    let retire = Arc::new(TestPlugin::retiring(
        definition("m6.retire", ToolSideEffect::Destructive, &[Capability::FS_WRITE]),
        slot.clone(),
    ));

    let session = SessionId::from_string(SESSION);
    let granted = authorized(&["fs.read", "fs.write"]);
    let handle = harness
        .stack
        .registry
        .register(
            &session,
            manifest("m6.marker.plugin", &["fs.read"]),
            PluginRuntime { tool_provider: Some(marker.clone() as Arc<dyn ToolProvider>) },
            &granted,
        )
        .await
        .expect("the marker registers");
    let marker_generation = harness.stack.registry.generation();
    *slot.lock().unwrap() = Some(handle);

    harness
        .stack
        .registry
        .register(
            &session,
            manifest("m6.retire.plugin", &["fs.write"]),
            PluginRuntime { tool_provider: Some(retire.clone() as Arc<dyn ToolProvider>) },
            &granted,
        )
        .await
        .expect("the retirer registers");

    // Turn one, with both tools on the surface.
    harness.ask("retire the other one, then tell me").await;

    let offered = harness.offered();
    assert_eq!(offered.len(), 2, "one turn, two model calls: {offered:#?}");
    assert!(
        offered[0].to_string().contains("m6_marker"),
        "the first request of the turn did not offer the marker: {}",
        offered[0]
    );

    // The retirement happened *during* the turn: the log says the plugin was
    // withdrawn, and the registry's live surface no longer has its tool.
    let log = harness.log().await;
    assert!(
        log.iter().any(|e| e.event.event_type.as_str() == "plugin/unloaded"),
        "the plugin was not withdrawn: {:?}",
        log.iter().map(|e| e.event.event_type.as_str()).collect::<Vec<_>>()
    );
    assert!(harness.stack.registry.generation() > marker_generation, "the surface did not change");
    assert!(
        !harness.stack.registry.tool_definitions().iter().any(|d| d.name == "m6.marker"),
        "the registry still offers a tool it withdrew"
    );

    // ...and the turn that was running never saw it go. This is the claim: a
    // request built after the withdrawal carries the manifest frozen at the
    // turn's start, byte for byte.
    assert_eq!(
        offered[0], offered[1],
        "a step of the same turn was offered a different tool surface"
    );

    // Turn two is a different turn, and it is built at the turn's own start.
    harness.ask("what is left?").await;
    let offered = harness.offered();
    assert_eq!(offered.len(), 3, "the second turn made one call: {offered:#?}");
    assert!(
        !offered[2].to_string().contains("m6_marker"),
        "the next turn still offers a plugin that was retired: {}",
        offered[2]
    );
    assert!(
        offered[2].to_string().contains("m6_retire"),
        "the plugin that was *not* retired should still be there: {}",
        offered[2]
    );
    assert_eq!(retire.calls(), 1, "the retiring tool ran exactly once");
    assert_eq!(marker.calls(), 0, "the marker was never called");
}

// ---------------------------------------------------------------------------
// M6 anchor 3, end to end — the product presses the button
// ---------------------------------------------------------------------------

/// The fake endpoint, running, with its stdout readable a line at a time.
///
/// A test that must inject something *inside* a turn has to know when the turn
/// is inside. Reading what the endpoint says is how it knows without sleeping:
/// the endpoint stops of its own accord and announces it, and the test acts on
/// the sentence rather than on a stopwatch.
struct FakeEndpoint {
    child: std::process::Child,
    lines: std::io::BufReader<std::process::ChildStdout>,
    /// Where the fake endpoint writes every request body it receives.
    bodies: PathBuf,
    /// Creating this file lets the endpoint answer the request it is holding.
    release: PathBuf,
    /// The base URL to give the agent.
    url: String,
}

impl FakeEndpoint {
    /// Start one over `script`, holding after the first answer.
    fn start(dir: &Path, script: &str) -> Self {
        let binary = fixture_bin("agent-model-openai", "fake-openai-server");
        let script_path = dir.join("script.json");
        std::fs::write(&script_path, script).expect("a script");
        let bodies = dir.join("bodies.jsonl");
        let release = dir.join("release");

        let mut child = Command::new(binary)
            .arg("--port")
            .arg("0")
            .arg("--script")
            .arg(&script_path)
            .arg("--log")
            .arg(&bodies)
            .arg("--hold-after")
            .arg("1")
            .arg("--release")
            .arg(&release)
            .arg("--model")
            .arg("fake")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the endpoint starts");

        let mut lines = std::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        let url = read_line(&mut lines, "listening")
            .split_whitespace()
            .nth(1)
            .map(|addr| format!("http://{addr}"))
            .expect("the endpoint announces its address");

        Self { child, lines, bodies, release, url }
    }

    /// Block until the endpoint says `needle`, and return what it said.
    fn expect(&mut self, needle: &str) -> String {
        read_line(&mut self.lines, needle)
    }

    /// Let the held request through, and wait until the endpoint says so.
    fn release(&mut self) {
        std::fs::write(&self.release, b"go").expect("the release file");
        self.expect("released");
    }

    /// Every request body the endpoint has received so far, in order.
    fn received(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(&self.bodies).unwrap_or_default();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each body is one JSON object"))
            .collect()
    }

    /// The wire names of the tools one request offered the model.
    fn offered(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for FakeEndpoint {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read lines until one contains `needle`, printing each as it goes.
fn read_line(lines: &mut std::io::BufReader<std::process::ChildStdout>, needle: &str) -> String {
    use std::io::BufRead as _;
    loop {
        let mut line = String::new();
        let read = lines.read_line(&mut line).expect("the endpoint's stdout is readable");
        assert!(read != 0, "the endpoint stopped before it said {needle:?}");
        let line = line.trim().to_string();
        if line.contains(needle) {
            return line;
        }
    }
}

/// Run the assembled binary against a fake endpoint, and wait for it.
///
/// The endpoint's own variables replace the `echo` default `ran` sets, and the
/// credential is a value no endpoint checks — the point here is which process
/// answers, not whether it authenticated.
fn ran_against(
    endpoint: &FakeEndpoint,
    data: &Path,
    workspace: &Path,
    config: &Path,
    argv: &[&str],
) -> Ran {
    let output = agent_command(endpoint, data, workspace, config)
        .args(argv)
        .output()
        .expect("the binary starts");
    Ran {
        ok: output.status.success(),
        out: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        err: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

/// The same, started and not waited for.
fn spawn_against(
    endpoint: &FakeEndpoint,
    data: &Path,
    workspace: &Path,
    config: &Path,
    argv: &[&str],
) -> std::process::Child {
    agent_command(endpoint, data, workspace, config)
        .args(argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts")
}

/// One `agent` process, configured to talk to `endpoint` and nothing else.
fn agent_command(endpoint: &FakeEndpoint, data: &Path, workspace: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env("AGENT_DATA_DIR", data)
        .env("AGENT_WORKSPACE_DIR", workspace)
        .env("AGENT_USER_CONFIG", config)
        .env_remove(ENV_MANAGED_POLICY)
        .env("AGENT_MODEL_PROVIDER", "openai")
        .env("AGENT_MODEL_BASE_URL", &endpoint.url)
        .env("AGENT_MODEL_NAME", "fake")
        .env("AGENT_MODEL_API_KEY", "not-a-secret");
    command
}

/// Everything that happened to `plugin_id`, in order, as `(seq, type)`.
fn plugin_history(stamped: &[(u64, Value)], plugin_id: &str) -> Vec<(u64, String)> {
    stamped
        .iter()
        .filter(|(_, e)| e["payload"]["plugin_id"] == plugin_id)
        .filter(|(_, e)| e["event_type"].as_str().is_some_and(|kind| kind.starts_with("plugin/")))
        .map(|(seq, e)| (*seq, e["event_type"].as_str().unwrap_or("?").to_string()))
        .collect()
}

/// The session log as `(seq, event)` pairs.
///
/// `events` drops the sequence number, and a sequence number is exactly what a
/// claim about *when* something happened is made of: this test has to say that
/// the withdrawal landed after one turn ended and before the next began, and
/// that sentence has no meaning without the numbers.
fn stamped(data: &Path, session: &str) -> Vec<(u64, Value)> {
    let path = data.join(session).join("events.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no log at {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let record: Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"));
            (
                record["seq"].as_u64().unwrap_or_else(|| panic!("no seq in {line:?}")),
                record["event"].clone(),
            )
        })
        .collect()
}

#[test]
fn a_running_agent_withdraws_a_plugin_through_the_command_line() {
    let (data, workspace, home) = dirs();
    let server = fixture_bin("agent-tools-mcp", "fake-mcp-server");
    let mut endpoint = FakeEndpoint::start(
        workspace.path(),
        r#"[
             {"tool_call": {"id": "call_1", "name": "mcp__one__echo", "arguments": {"text": "hello"}}},
             {"answer": "one done"},
             {"answer": "two done"},
             {"answer": "three done"},
             {"tool_call": {"id": "call_2", "name": "mcp__one__echo", "arguments": {"text": "hello again"}}},
             {"answer": "four done"},
             {"answer": "five done"}
           ]"#,
    );

    let config = write_config(
        &home,
        &format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.one\"]\n\n\
             [mcp.servers.one]\n\
             command = '{command}'\n\
             requires = [\"process.spawn\"]\n\n\
             [profiles.p]\n\
             tools = {{ allow = [\"process.spawn\"] }}\n\
             plugins = {{ enable = [\"mcp.one\"] }}\n",
            command = server.display(),
        ),
    );

    // --- Turn one starts and stops inside itself. ---
    let first = spawn_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "run", "one"],
    );
    endpoint.expect("holding request 2");
    assert_eq!(
        endpoint.received().len(),
        2,
        "the turn is one request past its tool call: the first was answered, the second is the \
         one being held"
    );

    // --- The operator turns the plugin off, from another process, while the
    // turn is in flight. ---
    let disabled = ran_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "plugin", "disable", "mcp.one", "--reason", "no longer needed"],
    );
    assert!(disabled.ok, "{}", disabled.err);
    assert!(
        disabled.out.contains("next turn boundary"),
        "the command says when, not that it is done: {}",
        disabled.out
    );

    // Queued, and nothing else: the process that runs the session is the one
    // that may withdraw, and it has not reached a boundary yet.
    let queued = stamped(data.path(), "default");
    assert!(
        !queued.iter().any(|(_, e)| e["event_type"] == "plugin/unloaded"),
        "a process that was not running the session withdrew the plugin: {:?}",
        plugin_history(&queued, "mcp.one")
    );
    let (_, control) = queued
        .iter()
        .find(|(_, e)| e["event_type"] == "inbox/appended" && e["payload"]["kind"] == "control")
        .expect("the control was queued");
    let item_id = control["payload"]["item_id"].clone();
    assert_eq!(control["payload"]["payload"]["control"], "plugin.disable");
    assert_eq!(control["payload"]["payload"]["plugin_id"], "mcp.one");
    assert_eq!(control["payload"]["payload"]["reason"], "no longer needed");
    assert_eq!(
        control["payload"]["status"], "pending",
        "the control was consumed by the command that queued it"
    );

    // --- The turn is let go, and finishes on its own terms. ---
    endpoint.release();
    let output = first.wait_with_output().expect("the first run finishes");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "one done");

    // The claim the whole milestone rests on, and it is asserted on the wire:
    // the second request of a turn whose Inbox already held this control
    // offered exactly what the first one did.
    let bodies = endpoint.received();
    assert_eq!(bodies.len(), 2, "turn one made two model calls");
    let offered_one = FakeEndpoint::offered(&bodies[0]);
    let offered_two = FakeEndpoint::offered(&bodies[1]);
    assert!(
        offered_one.contains(&"mcp__one__echo".to_string()),
        "the plugin's tool was never on offer: {offered_one:?}"
    );
    assert_eq!(offered_one, offered_two, "a step of the same turn saw a different tool surface");
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"], "and it differed field by field");

    // Nothing was withdrawn during it, either: the control is still pending.
    let after_one = stamped(data.path(), "default");
    let history = plugin_history(&after_one, "mcp.one");
    assert_eq!(
        history.iter().map(|(_, kind)| kind.as_str()).collect::<Vec<_>>(),
        vec!["plugin/loaded"],
        "the turn in flight was disturbed"
    );

    // --- Turn two is a different turn, and the boundary between them is where
    // the change was applied. ---
    let second = ran_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "run", "two"],
    );
    assert!(second.ok, "{}", second.err);
    assert_eq!(second.out.trim(), "two done");

    let with_two = stamped(data.path(), "default");
    let (claim_seq, claimed) = with_two
        .iter()
        .find(|(_, e)| e["event_type"] == "inbox/claimed" && e["payload"]["item_id"] == item_id)
        .expect("the control was taken at the boundary");
    assert!(
        claimed["payload"]["run_id"].is_null(),
        "a control belongs to the session, not to the turn that happened to take it: {claimed}"
    );
    let (unload_seq, unload) = with_two
        .iter()
        .find(|(_, e)| e["event_type"] == "plugin/unloaded")
        .expect("the withdrawal is recorded");
    assert_eq!(unload["payload"]["plugin_id"], "mcp.one");
    assert_eq!(unload["payload"]["reason"], "disabled");
    assert_eq!(unload["payload"]["state"], "unregistered");
    assert!(
        unload["payload"]["generation"].as_u64().unwrap_or(0) > 1,
        "the plugin surface did not move: {unload}"
    );

    // The order is the claim: after turn one ended, before turn two opened.
    // Taking the item *after* applying it is deliberate — a process that died
    // in between would leave the item pending and the change made, and applying
    // it again is harmless — so the withdrawal is stamped first.
    let first_turn_ended = with_two
        .iter()
        .find(|(_, e)| e["event_type"] == "turn/completed")
        .map(|(seq, _)| *seq)
        .expect("turn one ended");
    let second_turn_opened = with_two
        .iter()
        .filter(|(_, e)| e["event_type"] == "turn/started")
        .map(|(seq, _)| *seq)
        .nth(1)
        .expect("turn two opened");
    assert!(first_turn_ended < *unload_seq, "the change landed inside turn one");
    assert!(*unload_seq < second_turn_opened, "the change landed after turn two began");
    assert!(
        *unload_seq < *claim_seq,
        "the withdrawal is recorded before the claim that reports it"
    );

    // And the world agrees with the log: the next request offered no tool from
    // that server. Not "the registry says so" — a request that reached a socket.
    let bodies = endpoint.received();
    assert_eq!(bodies.len(), 3, "turn two made one model call");
    let offered_three = FakeEndpoint::offered(&bodies[2]);
    assert!(
        !offered_three.iter().any(|name| name.starts_with("mcp__one__")),
        "turn two was still offered the withdrawn plugin's tools: {offered_three:?}"
    );
    assert!(
        offered_three.contains(&"shell_run".to_string()),
        "the built-ins went with it: {offered_three:?}"
    );

    // --- A control naming something nobody registered is refused by name, and
    // nothing is withdrawn for it. ---
    let refused = ran_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "plugin", "disable", "nosuch"],
    );
    assert!(refused.ok, "queuing a control is not itself a failure: {}", refused.err);
    let third = ran_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "run", "three"],
    );
    assert!(third.ok, "{}", third.err);

    let with_three = stamped(data.path(), "default");
    let (_, discarded) = with_three
        .iter()
        .find(|(_, e)| {
            e["event_type"] == "inbox/discarded"
                && e["payload"]["reason"].as_str().is_some_and(|r| r.contains("nosuch"))
        })
        .expect("a refusal is recorded, not left pending");
    let reason = discarded["payload"]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("no such plugin 'nosuch'"), "{reason}");
    assert!(reason.contains("mcp.one"), "it names what was registered instead: {reason}");

    // The second half of the same fact: applying a control that cannot be
    // applied does not touch the surface. `mcp.one` was withdrawn at turn two's
    // boundary and this run registered it again from the configuration, so its
    // only unload in the log is still the one from the disable.
    assert_eq!(
        plugin_history(&with_three, "mcp.one")
            .iter()
            .filter(|(_, kind)| kind == "plugin/unloaded")
            .count(),
        1,
        "a refusal withdrew something"
    );

    // --- The disable outlives the process that made it. Run three read the log
    // before it started anything and declined to start the server at all: a
    // surface that comes back the next time somebody runs the agent is not a
    // surface anybody turned off.
    let bodies = endpoint.received();
    assert_eq!(bodies.len(), 4, "run three made one model call");
    let offered_four = FakeEndpoint::offered(&bodies[3]);
    assert!(
        !offered_four.iter().any(|name| name.starts_with("mcp__one__")),
        "a later process registered a plugin this session turned off: {offered_four:?}"
    );
    assert!(
        offered_four.contains(&"shell_run".to_string()),
        "only the plugin went: {offered_four:?}"
    );
    // Not started, and not withdrawn either — there was nothing to withdraw.
    let after_three = stamped(data.path(), "default");
    assert_eq!(
        plugin_history(&after_three, "mcp.one")
            .iter()
            .filter(|(_, k)| k == "plugin/unloaded")
            .count(),
        1,
        "the suppressed plugin was started and withdrawn again"
    );

    // --- And the way back is the mirror of the way in. ---
    let enabled = ran_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "plugin", "enable", "mcp.one"],
    );
    assert!(enabled.ok, "{}", enabled.err);
    assert!(
        enabled.out.contains("next turn boundary"),
        "the command says when, not that it is done: {}",
        enabled.out
    );
    let (_, enable_item) = stamped(data.path(), "default")
        .into_iter()
        .rev()
        .find(|(_, e)| {
            e["event_type"] == "inbox/appended"
                && e["payload"]["payload"]["control"] == "plugin.enable"
        })
        .expect("the enable was queued");
    assert_eq!(enable_item["payload"]["payload"]["plugin_id"], "mcp.one");
    assert_eq!(enable_item["payload"]["status"], "pending");

    // --- Run four: the enable is applied at the boundary, which is *before*
    // the turn's manifest is frozen — so the tool is back for this turn, and the
    // model's call to it is answered by a server that this process started. ---
    let fourth = ran_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "run", "four"],
    );
    assert!(fourth.ok, "{}", fourth.err);
    assert_eq!(fourth.out.trim(), "four done", "the plugin's tool did not come back");

    let bodies = endpoint.received();
    assert_eq!(bodies.len(), 6, "run four made two model calls");
    assert!(
        FakeEndpoint::offered(&bodies[4]).contains(&"mcp__one__echo".to_string()),
        "the turn after the enable did not see the tool: {:?}",
        FakeEndpoint::offered(&bodies[4])
    );

    // --- Run five asks nothing and still gets it: the enable is as durable as
    // the disable was, and for the same reason — the log's last word. ---
    let fifth = ran_against(
        &endpoint,
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "run", "five"],
    );
    assert!(fifth.ok, "{}", fifth.err);
    assert_eq!(fifth.out.trim(), "five done");

    let bodies = endpoint.received();
    assert_eq!(bodies.len(), 7, "run five made one model call");
    assert!(
        FakeEndpoint::offered(&bodies[6]).contains(&"mcp__one__echo".to_string()),
        "the enable lasted exactly one process: {:?}",
        FakeEndpoint::offered(&bodies[6])
    );
    assert_eq!(
        plugin_history(&stamped(data.path(), "default"), "mcp.one")
            .iter()
            .filter(|(_, kind)| kind == "plugin/unloaded")
            .count(),
        1,
        "a plugin was withdrawn that nobody disabled"
    );
}

// ---------------------------------------------------------------------------
// M6 anchor 4 — a second server is a configuration, not a change
// ---------------------------------------------------------------------------

#[test]
fn a_second_mcp_server_is_added_by_configuration_alone() {
    let (data, workspace, home) = dirs();
    let server = fixture_bin("agent-tools-mcp", "fake-mcp-server");

    // The same server program under two names. Nothing in the binary, the
    // kernel or any crate knows either name: the whole difference between one
    // server and two is this file.
    let config = write_config(
        &home,
        &format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.one\", \"mcp.two\"]\n\n\
             [mcp.servers.one]\n\
             command = '{command}'\n\
             requires = [\"process.spawn\"]\n\n\
             [mcp.servers.two]\n\
             command = '{command}'\n\
             requires = [\"process.spawn\"]\n\n\
             [profiles.p]\n\
             tools = {{ allow = [\"process.spawn\"] }}\n\
             policy = {{ shell = \"allow\" }}\n\
             plugins = {{ enable = [\"mcp.one\", \"mcp.two\"] }}\n",
            command = server.display(),
        ),
    );

    for server_name in ["one", "two"] {
        let tool = format!("mcp__{server_name}__echo");
        let r = ran(
            data.path(),
            workspace.path(),
            &config,
            &[
                "--profile",
                "p",
                "tool",
                &tool,
                &format!(r#"{{"text":"answered by {server_name}"}}"#),
            ],
        );
        assert!(r.ok, "{tool}: {}", r.err);
        assert_eq!(
            r.out.trim(),
            format!("answered by {server_name}"),
            "the second server's own process answered: {}",
            r.out
        );
    }

    // Both were registered, and each by its own name — a run that had started
    // one server twice under one name would show one id here.
    let loaded: Vec<String> = events(data.path(), "default")
        .iter()
        .filter(|e| e["event_type"] == "plugin/loaded")
        .filter_map(|e| e["payload"]["plugin_id"].as_str().map(str::to_string))
        .collect();
    assert!(loaded.contains(&"mcp.one".to_string()), "{loaded:?}");
    assert!(loaded.contains(&"mcp.two".to_string()), "{loaded:?}");
}

// ---------------------------------------------------------------------------
// M6 anchor 5 — the sidecar off, the baseline answering, the log replaying
// ---------------------------------------------------------------------------

/// The decision trace the run recorded.
fn trace_of(data: &Path) -> agent_ports::decision::DecisionTrace {
    let mut found = None;
    for event in events(data, "default") {
        if event["event_type"] == "decision/trace" {
            found = Some(
                serde_json::from_value(event["payload"]["trace"].clone())
                    .expect("the event carries the trace this build writes"),
            );
        }
    }
    found.expect("the run recorded a decision trace")
}

#[test]
fn with_the_sidecar_off_the_baseline_answers_and_the_log_replays() {
    let (data, workspace, home) = dirs();

    // A sidecar defined by the configuration and turned off by the profile
    // running this turn: the definition is how to reach one, and the profile is
    // whether this run does.
    let config = write_config(
        &home,
        "[decisions.laya]\n\
         url = \"http://127.0.0.1:1\"\n\
         model = \"multilingual\"\n\n\
         [profiles.p]\n\
         decisions = { laya = false }\n",
    );

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "run", "hello"]);
    assert!(r.ok, "{}", r.err);
    assert!(
        !r.err.contains("decision backend:"),
        "a backend this run did not attach is not announced: {}",
        r.err
    );

    let trace = trace_of(data.path());

    // The baseline answered, and it answered alone. "Nobody was asked" and
    // "everybody declined" are different facts, and `aborted` is the one that
    // tells them apart.
    assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
    assert!(trace.report_for("laya").is_none(), "the sidecar was asked after all");
    assert!(trace.aborted.is_none(), "nobody was asked must not read as nothing ran");

    // And the conclusion is one the record supports on its own: a reader
    // holding only this log re-derives the same winner and the same answers,
    // which is the whole point of recording the reports rather than the verdict.
    let replayed = agent_decision::replay(&trace);
    assert_eq!(replayed.chosen_backend, trace.chosen_backend);
    assert_eq!(replayed.chosen, trace.chosen);
    agent_decision::verify(&trace).expect("the trace re-derives its own conclusion");
}
