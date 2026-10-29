//! Fault injection: the process dies mid-turn, and the next one repairs.
//!
//! # Why a whole process and not a store that returns an error
//!
//! The promise a turn journal makes is about a process that stops existing:
//! what is on disk when the operating system takes the process away, with no
//! destructor, no flush-on-shutdown, and nobody left to handle a failure. A test
//! that makes a store return `Err` at a chosen point proves something else — it
//! proves the callers react to an error, which is not the interesting case here.
//!
//! So every row of this matrix is a real death. The test re-executes its own
//! binary (the child-process trick: `current_exe` plus one test name), the child
//! assembles the product's stack through [`stack::assemble`] — the same function
//! `apps/agent` calls — opens a turn, and is killed inside it. The parent waits
//! for the child to reach the kill point before terminating it, so the moment is
//! chosen rather than raced: either the child's transport says the request is in
//! flight, or the session log itself says the call was dispatched. `Child::kill`
//! is `TerminateProcess` on Windows and `SIGKILL` everywhere else: no unwinding,
//! no `Drop`, no last write.
//!
//! # The rows
//!
//! | kill point | when | what the log holds |
//! |---|---|---|
//! | `model-call-1` | the first request is in flight | run and turn open; no step |
//! | `between-steps` | step one is committed, step two's request is in flight | run and turn open; step one closed with its call |
//! | `during-tool` | the tool the model asked for is running | run, turn and step open; `tool/started` with no outcome |
//!
//! `between-steps` is also the shape a crash immediately before the terminal
//! batch leaves — the last step closed, the turn not — because the terminal batch
//! is the only write after a step ends. The two moments are one row.
//!
//! Every row asserts the same six things, because they are the same six
//! questions a restart asks:
//!
//! * while the child lives it is the session's one driver, and a second driver
//!   is refused by pid;
//! * when it dies its claim goes with it, so the next process can drive;
//! * the user's message is still pending — a crash is not an answer;
//! * `Recovery` closes what was open, and marks a call that was dispatched and
//!   never came back as `tool/outcome_unknown` rather than "it did not run";
//! * the next turn completes, and the model is told the truth about the call
//!   whose outcome nobody recorded;
//! * a second `Recovery` pass appends nothing.
//!
//! # What is not here
//!
//! A crash *between* `append` and `flush` cannot be produced while the machine
//! stays up: the bytes are in the page cache either way, and a test cannot
//! un-write them. The store's own flush tests pin that half. What this matrix
//! pins is the half it can: every write a turn depends on was flushed before the
//! next thing happened. `during-tool` is the sharpest version of it — the parent
//! kills the child as soon as `tool/started` can be read out of the session log
//! while the command it names is still running, and that line is read back again
//! after the process is gone.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile, UserConfig};
use agent_kernel::recovery::RecoveryReport;
use agent_model_openai::{
    OpenAiCompatConfig, OpenAiCompatProvider, Transport, TransportError, TransportRequest,
    TransportResponse, to_wire_name,
};
use agent_ports::agent_loop::StopReason;
use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::execution_environment::SandboxMode;
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_protocol::events::{Actor, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::inbox::{InboxItemStatus, InboxKind};
use agent_store_jsonl::JsonlEventStore;
use agent_testkit::FixedSecret;
use chrono::Utc;

// ---------------------------------------------------------------------------
// The kill points
// ---------------------------------------------------------------------------

/// One place a process can die inside a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kill {
    /// The first model request is in flight: the turn is open and nothing about
    /// a step is on disk.
    ModelCallOne,
    /// Step one is committed and step two's request is in flight.
    BetweenSteps,
    /// The tool the model asked for is running: its dispatch is durable and its
    /// outcome is not.
    DuringTool,
}

impl Kill {
    fn name(self) -> &'static str {
        match self {
            Kill::ModelCallOne => "model-call-1",
            Kill::BetweenSteps => "between-steps",
            Kill::DuringTool => "during-tool",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [Kill::ModelCallOne, Kill::BetweenSteps, Kill::DuringTool]
            .into_iter()
            .find(|kill| kill.name() == name)
    }

    /// The file the child's transport creates when the request that reaches
    /// this kill point is in flight, in the workspace the parent watches.
    ///
    /// `during-tool` has no file signal, and needs none: its kill point is
    /// observed in the log itself. The parent reads `tool/started` out of the
    /// session log while the command is still running — which is the durability
    /// that row is about, and a stronger signal than any file the tool could
    /// write, because the tool cannot write it before its own dispatch is
    /// durable.
    fn signal(self) -> Option<&'static str> {
        match self {
            Kill::ModelCallOne => Some("model-1.signal"),
            Kill::BetweenSteps => Some("model-2.signal"),
            Kill::DuringTool => None,
        }
    }
}

/// The environment variable that turns the child test into the child process.
const ENV_KILL: &str = "YAOYAO_CRASH_KILL";
const ENV_WORKSPACE: &str = "YAOYAO_CRASH_WORKSPACE";
const ENV_DATA: &str = "YAOYAO_CRASH_DATA_DIR";

/// The test the parent re-executes. It does nothing under a normal run: it is
/// `#[ignore]`d, and the parent is the only caller that runs it — with
/// [`ENV_KILL`] set to the row it is standing in for.
const CHILD_TEST: &str = "the_crashing_child";

/// The exit code the child uses when the turn ended without dying. The parent
/// reports it as "the kill point never fired", which is the one failure that
/// would otherwise look like a pass: a crash test in which nothing crashed.
const EXIT_NEVER_FIRED: i32 = 2;

const SESSION: &str = "crash-matrix";

/// The user's words. Plain ASCII so that a substring search in a request body
/// means what it says.
const PROMPT: &str = "read notes.txt and tell me the word";

// ---------------------------------------------------------------------------
// The child: the process that dies
// ---------------------------------------------------------------------------

/// The transport the child runs on. It never answers the request that reaches
/// the kill point — the parent kills the process while the answer is pending.
struct DyingTransport {
    kill: Kill,
    workspace: PathBuf,
    calls: Mutex<usize>,
}

impl DyingTransport {
    fn new(kill: Kill, workspace: &Path) -> Self {
        Self { kill, workspace: workspace.to_path_buf(), calls: Mutex::new(0) }
    }

    /// Signal the parent, then never answer.
    async fn hang(&self) -> Result<TransportResponse, TransportError> {
        let signal = self.kill.signal().expect("this kill point signals with a file");
        std::fs::write(self.workspace.join(signal), b"here").expect("signal the parent");
        std::future::pending::<()>().await;
        unreachable!("a pending future does not resolve")
    }
}

#[async_trait::async_trait]
impl Transport for DyingTransport {
    async fn post(&self, _request: TransportRequest) -> Result<TransportResponse, TransportError> {
        let call = {
            let mut calls = self.calls.lock().expect("the call count is not poisoned");
            *calls += 1;
            *calls
        };
        match self.kill {
            Kill::ModelCallOne if call == 1 => self.hang().await,
            Kill::BetweenSteps if call == 1 => Ok(response(tool_call_body(
                "call_read",
                &to_wire_name("fs.read"),
                serde_json::json!({ "path": "notes.txt" }),
            ))),
            Kill::BetweenSteps => self.hang().await,
            // The tool is what keeps this process alive long enough to be
            // killed inside the call: the command runs until the parent has
            // seen `tool/started` in the log and terminated the agent.
            Kill::DuringTool if call == 1 => Ok(response(tool_call_body(
                "call_slow",
                &to_wire_name("shell.run"),
                serde_json::json!({ "command": slow_command() }),
            ))),
            _ => Err(TransportError::Connect(format!(
                "unscripted request {call} for {}",
                self.kill.name()
            ))),
        }
    }
}

/// A command that takes long enough for the parent to see the dispatch in the
/// log and kill the process.
///
/// It must outlast the parent's polling and fit inside `shell.run`'s own
/// deadline, which is 30s: nine seconds leaves room at both ends. It writes
/// nothing — a command's own file writes would drag this test into the
/// integrity labels of the machine it runs on, and the log already says what
/// this row needs to know.
fn slow_command() -> String {
    #[cfg(windows)]
    {
        "ping -n 10 127.0.0.1".to_string()
    }
    #[cfg(not(windows))]
    {
        "sleep 9".to_string()
    }
}

/// The child process's whole job: assemble the product's stack over the two
/// directories the parent handed over, open a turn, and stop inside it.
async fn child(kill: Kill) {
    let workspace = PathBuf::from(std::env::var(ENV_WORKSPACE).expect(ENV_WORKSPACE));
    let data = PathBuf::from(std::env::var(ENV_DATA).expect(ENV_DATA));
    let session = SessionId::from_string(SESSION);

    let transport: Arc<dyn Transport> = Arc::new(DyingTransport::new(kill, &workspace));
    let runtime = assemble(&workspace, &data, transport).await;

    runtime
        .scheduler
        .inbox()
        .append(
            &session,
            InboxKind::NextTurn,
            serde_json::json!({ "text": PROMPT }),
            0,
            Actor::User,
        )
        .await
        .expect("append the input");

    match runtime.scheduler.run_pending(&session, Utc::now()).await {
        Ok(_) => {
            eprintln!("the kill point `{}` never fired: the turn ran to the end", kill.name());
            std::process::exit(EXIT_NEVER_FIRED);
        }
        Err(error) => {
            eprintln!("the turn ended before the kill point `{}`: {error}", kill.name());
            std::process::exit(EXIT_NEVER_FIRED);
        }
    }
}

// ---------------------------------------------------------------------------
// The parent: the next process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_with_the_first_request_in_flight_leaves_nothing_a_restart_cannot_close() {
    matrix(Kill::ModelCallOne).await;
}

#[tokio::test]
async fn a_crash_between_two_steps_keeps_the_message_and_the_step_it_finished() {
    matrix(Kill::BetweenSteps).await;
}

#[tokio::test]
async fn a_crash_during_a_tool_call_is_never_silently_repeated() {
    matrix(Kill::DuringTool).await;
}

/// The body of the child process, reached only by re-executing this binary.
///
/// Under an ordinary `cargo test` run it is ignored; the parent runs it with
/// `--ignored --exact` and [`ENV_KILL`] naming the row.
#[tokio::test]
#[ignore = "re-executed by the crash matrix, which supplies YAOYAO_CRASH_KILL"]
async fn the_crashing_child() {
    let Some(kill) = std::env::var(ENV_KILL).ok().and_then(|name| Kill::parse(&name)) else {
        // Selected by hand rather than by the matrix: there is nothing to run.
        return;
    };
    child(kill).await;
}

/// One row of the matrix: kill a child at `kill`, then be the process that
/// finds what it left behind.
async fn matrix(kill: Kill) {
    let workspace = tempfile::tempdir().expect("a workspace");
    let data = tempfile::tempdir().expect("a data directory");
    std::fs::write(workspace.path().join("notes.txt"), "rhubarb\n").expect("seed a workspace file");

    let status = crash(kill, workspace.path(), data.path()).await;
    assert!(
        !status.success(),
        "`{}`: the child exited with {status:?} — the kill point never fired, or the \
         turn failed before reaching it",
        kill.name()
    );

    let session = SessionId::from_string(SESSION);
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data.path()));

    // The claim died with the process that held it. Without this the session
    // would refuse every later command as busy, and no restart could help.
    let claim = store
        .driver_lease(&session)
        .await
        .expect("a killed process's driver claim is released by the operating system");
    drop(claim);

    let crashed = types(&store, &session).await;
    assert!(crashed.iter().any(|t| t == EventType::TURN_STARTED), "{crashed:?}");
    assert!(
        !crashed.iter().any(|t| t == EventType::TURN_COMPLETED),
        "a killed process cannot have completed its turn: {crashed:?}"
    );
    assert!(
        !crashed.iter().any(|t| t == EventType::INBOX_CLAIMED),
        "the input is claimed by the terminal batch, which never landed: {crashed:?}"
    );
    match kill {
        Kill::ModelCallOne => assert!(
            !crashed.iter().any(|t| t == EventType::STEP_STARTED),
            "nothing about a step can be on disk before the step's first boundary: {crashed:?}"
        ),
        Kill::BetweenSteps => {
            assert!(
                crashed.iter().any(|t| t == EventType::TOOL_COMPLETED),
                "the call the first step made came back, and its result is durable: {crashed:?}"
            );
            assert_eq!(
                crashed.iter().filter(|t| *t == EventType::STEP_COMPLETED).count(),
                1,
                "the first step ended before the second began: {crashed:?}"
            );
        }
        Kill::DuringTool => {
            assert!(
                crashed.iter().any(|t| t == EventType::TOOL_STARTED),
                "the dispatch is durable before the tool runs: {crashed:?}"
            );
            assert!(
                !crashed.iter().any(|t| terminal_tool(t)),
                "the tool is still running, so no outcome can be on disk: {crashed:?}"
            );
        }
    }

    // --- The restart -------------------------------------------------------
    let transport = Arc::new(ScriptedTransport::new(vec![answer_body("recovered")]));
    let runtime = assemble(workspace.path(), data.path(), transport.clone()).await;

    let repaired = runtime.recovery.recover(&session).await.expect("recover");
    let expected = match kill {
        Kill::ModelCallOne => RecoveryReport {
            closed_step: false,
            closed_turn: true,
            closed_run: true,
            tools_outcome_unknown: 0,
            deferred_to_resume: None,
        },
        Kill::BetweenSteps => RecoveryReport {
            closed_step: false,
            closed_turn: true,
            closed_run: true,
            tools_outcome_unknown: 0,
            deferred_to_resume: None,
        },
        Kill::DuringTool => RecoveryReport {
            closed_step: true,
            closed_turn: true,
            closed_run: true,
            tools_outcome_unknown: 1,
            deferred_to_resume: None,
        },
    };
    assert_eq!(repaired, expected, "`{}` was repaired differently than the row says", kill.name());

    let healed = types(&store, &session).await;
    for closer in [EventType::TURN_INTERRUPTED, EventType::RUN_INTERRUPTED] {
        assert!(healed.iter().any(|t| t == closer), "`{closer}` is missing: {healed:?}");
    }

    if kill == Kill::DuringTool {
        // The call is marked, not guessed: nobody knows whether it ran, and the
        // sentence the model reads says exactly that.
        let unknown = events_of(&store, &session, EventType::TOOL_OUTCOME_UNKNOWN).await;
        assert_eq!(unknown.len(), 1, "one dispatched call had no outcome");
        let payload = &unknown[0].event.payload;
        assert_eq!(payload["tool_name"], "shell.run");
        assert_eq!(payload["provider_tool_call_id"], "call_slow");
        let content = payload["content"].as_str().expect("the sentence the model reads");
        assert!(content.contains("may or may not have run"), "{content}");
        // The row only reaches this point if the dispatch was readable while
        // the command was still running — the tool cannot have returned, or the
        // assertion above ("no outcome can be on disk") would have failed. So
        // this is a call that really was in the middle of doing something, not
        // a convenient fiction.
    }

    // Repair is a fact about the log, not a state a process keeps: a second
    // pass finds nothing, in this process or any later one.
    let again = runtime.recovery.recover(&session).await.expect("recover twice");
    assert_eq!(again, RecoveryReport::default(), "recovery must be idempotent");

    // --- The item the crash did not answer --------------------------------
    let pending = runtime.scheduler.inbox().pending(&session).await.expect("pending");
    assert_eq!(pending.len(), 1, "the message the user wrote is still waiting");
    assert_eq!(pending[0].kind, InboxKind::NextTurn);
    assert_eq!(pending[0].status, InboxItemStatus::Pending);
    assert_eq!(pending[0].payload["text"], PROMPT);

    // --- The turn that finally runs ---------------------------------------
    let second = runtime
        .scheduler
        .run_pending(&session, Utc::now())
        .await
        .expect("the repaired session can be driven")
        .expect("the pending item runs");
    assert_eq!(second.stop_reason, StopReason::FinalAnswer);
    assert_eq!(second.final_content.as_deref(), Some("recovered"));

    let final_log = types(&store, &session).await;
    assert_eq!(
        final_log.iter().filter(|t| *t == EventType::INBOX_CLAIMED).count(),
        1,
        "the item is claimed once, by the turn that answered it: {final_log:?}"
    );
    assert_eq!(final_log.iter().filter(|t| *t == EventType::TURN_COMPLETED).count(), 1);

    let bodies = transport.bodies();
    assert_eq!(bodies.len(), 1, "one step, one request");
    assert!(
        bodies[0].contains(PROMPT),
        "the user's message survived the crash and reached the model again"
    );
    if kill == Kill::DuringTool {
        assert!(
            bodies[0].contains("may or may not have run"),
            "the next turn is told the truth about the call nobody saw finish: {}",
            bodies[0]
        );
    }
}

/// Run the child until it reaches `kill`, then terminate it.
async fn crash(kill: Kill, workspace: &Path, data: &Path) -> std::process::ExitStatus {
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env(ENV_KILL, kill.name())
        .env(ENV_WORKSPACE, workspace)
        .env(ENV_DATA, data)
        // Not `Stdio::null()`: opening the NUL device is one of the things this
        // machine's Low-integrity checkout refuses, and a pipe is a stdin the
        // child never reads anyway.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("re-execute this test binary");

    let deadline = Instant::now() + Duration::from_secs(120);
    while !reached(kill, workspace, data) {
        assert!(
            Instant::now() < deadline,
            "`{}`: the child never reached the kill point",
            kill.name()
        );
        if let Some(status) = child.try_wait().expect("poll the child") {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!(
                "`{}`: the child exited with {status:?} before the kill point: {stderr}",
                kill.name()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // While the child is alive it is the session's one driver, and a second
    // one is refused — by name. This half of "exactly one driver" can only be
    // asked from another process; the other half, the claim going away with the
    // process that held it, is the caller's first assertion after this returns.
    let session = SessionId::from_string(SESSION);
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data));
    match store.driver_lease(&session).await {
        Err(EventStoreError::ConcurrentDriver { holder, .. }) => {
            let holder = holder.expect("a driving process records itself while it drives");
            assert_eq!(holder.pid, child.id(), "the refusal names the driver it refused");
        }
        Err(other) => panic!("expected a driver refusal, got {other}"),
        Ok(_) => panic!("a live driver must refuse a second one"),
    }
    drop(store);

    child.kill().expect("terminate the child");
    child.wait().expect("reap the child")
}

/// Whether the child has reached `kill`.
///
/// The two transport rows signal with a file the child's own transport writes.
/// `during-tool` is read out of the session log instead, and read as bytes: the
/// store would be a second writer over a file another process has open, and a
/// plain read is not. Seeing `tool/started` there means the dispatch survived a
/// `flush` while the command it names is still running, which is exactly the
/// window this row exists for.
fn reached(kill: Kill, workspace: &Path, data: &Path) -> bool {
    match kill.signal() {
        Some(signal) => workspace.join(signal).exists(),
        None => match std::fs::read(data.join(SESSION).join("events.jsonl")) {
            Ok(bytes) => String::from_utf8_lossy(&bytes)
                .contains(&format!("\"{}\"", EventType::TOOL_STARTED)),
            Err(_) => false,
        },
    }
}

// ---------------------------------------------------------------------------
// The harness the acceptance suites use, trimmed to one turn's worth
// ---------------------------------------------------------------------------

/// Build the stack the way `apps/agent` builds it, over a scripted wire and two
/// directories the caller owns.
///
/// The judgment pipeline is off: the subject here is what survives a process
/// death, and a turn that the pipeline refuses would not open one.
async fn assemble(workspace: &Path, data: &Path, transport: Arc<dyn Transport>) -> stack::Runtime {
    stack::assemble(stack::Inputs {
        session: SessionId::from_string(SESSION),
        workspace_root: workspace.to_path_buf(),
        data_dir: data.to_path_buf(),
        resolved: profile(),
        model: Some(SelectedProvider { provider: scripted_provider(transport), route: route() }),
        decisions: Decisions::Unobserved,
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: false,
    })
    .await
    .expect("the assembly the binary runs")
}

/// The defaults, with one difference: a shell command runs instead of stopping
/// to ask.
///
/// `during-tool` needs a tool that is still running when the process dies, and
/// the only slow thing on the surface is a command. The default disposition
/// (`ask`) would park the turn at an approval instead of running it — which is
/// a different test, already in the m5 suite.
fn profile() -> ResolvedProfile {
    let layers = Layers {
        user: UserConfig::from_toml_str("[profiles.crash]\npolicy = { shell = \"allow\" }\n")
            .expect("a profile that allows a command"),
        profile_override: Some("crash".to_string()),
        ..Layers::default()
    };
    agent_config::resolve(layers).expect("the profile resolves")
}

fn scripted_provider(transport: Arc<dyn Transport>) -> Arc<OpenAiCompatProvider> {
    let config = OpenAiCompatConfig::new("https://scripted.example.com", "scripted-model")
        .with_provider_id("scripted-endpoint")
        .with_timeout_ms(5_000);
    Arc::new(OpenAiCompatProvider::new(
        config,
        Arc::new(FixedSecret::new("sk-test-not-a-real-key")),
        transport,
    ))
}

fn route() -> ModelRoute {
    ModelRoute::new(ModelTier::Main, "scripted-endpoint", "scripted-model")
}

/// A transport that replays what it was scripted with and records what it was
/// asked to send. Running out of scripted answers is an error, not a hang.
struct ScriptedTransport {
    responses: Mutex<Vec<String>>,
    posts: Mutex<Vec<TransportRequest>>,
}

impl ScriptedTransport {
    fn new(responses: Vec<String>) -> Self {
        Self { responses: Mutex::new(responses), posts: Mutex::new(Vec::new()) }
    }

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
        match next {
            Some(body) => Ok(response(body)),
            None => Err(TransportError::Connect("no scripted response left".to_string())),
        }
    }
}

fn response(body: String) -> TransportResponse {
    TransportResponse {
        status: 200,
        body: Box::pin(futures_util::stream::iter(vec![Ok(body.into_bytes())])),
    }
}

/// An SSE body from raw `data:` payload lines.
fn sse(lines: &[&str]) -> String {
    lines.iter().map(|line| format!("{line}\n\n")).collect()
}

/// A streamed tool call, as an endpoint would send one.
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

// ---------------------------------------------------------------------------
// Reading the log back
// ---------------------------------------------------------------------------

/// A terminal tool event: one that says what became of a call.
fn terminal_tool(event_type: &str) -> bool {
    matches!(
        event_type,
        EventType::TOOL_COMPLETED
            | EventType::TOOL_FAILED
            | EventType::TOOL_CANCELLED
            | EventType::TOOL_INTERRUPTED
            | EventType::TOOL_OUTCOME_UNKNOWN
    )
}

async fn log(store: &Arc<dyn EventStore>, session: &SessionId) -> Vec<StoredEvent> {
    store.read_stream(session, EventSeq::ZERO, usize::MAX).await.expect("read the log")
}

async fn types(store: &Arc<dyn EventStore>, session: &SessionId) -> Vec<String> {
    log(store, session).await.into_iter().map(|e| e.event.event_type.to_string()).collect()
}

async fn events_of(
    store: &Arc<dyn EventStore>,
    session: &SessionId,
    event_type: &str,
) -> Vec<StoredEvent> {
    log(store, session)
        .await
        .into_iter()
        .filter(|e| e.event.event_type.as_str() == event_type)
        .collect()
}
