//! M5 acceptance: the local coding MVP, end to end.
//!
//! These assemble what `apps/agent` assembles — not a second assembly that
//! resembles it, but the same `agent::stack::assemble` the binary calls: the
//! real JSONL store, the projection-backed context engine, the guarded tool
//! gateway over the file *and* git tools, the real approval service, the kernel
//! Scheduler, Recovery and Resume — and drive one real task in a real git
//! repository. Wire the product differently and this file's subject changes with
//! it.
//!
//! The one seam that is faked is the network: [`ScriptedTransport`] stands in
//! for the TCP connection and replays bytes, exactly as `m4_acceptance` does.
//! What that buys is an acceptance run with no endpoint and no credential, and
//! the same verdict every time. What it costs is that the model's judgement is
//! not under test — the framework is. Every call below is really proposed,
//! really gated, really executed against a file on disk, and really recorded;
//! the scripted model only decides *what to ask for*. A script that asks for the
//! right things in the right order still cannot get the task done unless the
//! machinery between the wire and the file system works, and that machinery is
//! production code down to the last line of it.
//!
//! Nothing here asserts on the model's words. Following the rule this project
//! took from DSH — *verify the world, not the self-report* — the assertions run
//! the check themselves and read the files back:
//!
//! | M5 criterion | how it is evidenced |
//! |---|---|
//! | a real task in a real repository | we run the check; it fails before and passes after |
//! | the task survives being interrupted | the process holding the call is dropped; a second assembly over the same log finishes it |
//! | steering at a step boundary | a note left before the run is in a request that reached the wire |
//! | every change, command and request is audited | the durable log's own event chain, counted by tool name |
//! | the output refers to real artifact / diff / test result | the `git.diff` result is compared against the change we can see ourselves |
//!
//! Nothing is shared with `m4_acceptance.rs` on purpose. An acceptance test that
//! can only be read next to another file is half a test.

use std::collections::VecDeque;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use agent::providers::SelectedProvider;
use agent::stack::{self, Decisions};
use agent_config::{Layers, ResolvedProfile};
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
use agent_protocol::events::{Actor, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::permissions::ApprovalScope;
use agent_protocol::tool::ToolStatus;
use agent_testkit::FixedSecret;
use chrono::Utc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// The file the task is about, as it starts — wrong.
const ANSWER_BEFORE: &str = "the answer is 41\n";
/// And as it has to end.
const ANSWER_AFTER: &str = "the answer is 42\n";
/// A file the task is not about. It must come out of the run byte for byte.
const UNTOUCHED: &str = "nothing here should move\n";

const SESSION: &str = "m5-acceptance";

/// The check the task is about: it fails while `answer.txt` says 41 and passes
/// once it says 42. The command's exit code *is* the verdict — there is nothing
/// to read and nothing to interpret.
///
/// It is written twice because `shell.run` runs it through `cmd /C` on Windows
/// and `sh -c` everywhere else, which is the same fork its own `shell_spec`
/// makes. A check that only held on one of them would be a check CI cannot run.
#[cfg(windows)]
const CHECK: &str = "findstr /C:42 answer.txt";
#[cfg(not(windows))]
const CHECK: &str = "grep -q 42 answer.txt";

/// The note left before the run, to be delivered at a step boundary.
const NOTE: &str = "show me the diff when the check passes";

/// A throwaway git repository with one wrong answer in it.
struct Repo {
    workspace: TempDir,
    data: TempDir,
}

impl Repo {
    fn new() -> Self {
        let workspace = tempfile::tempdir().expect("a workspace");
        let data = tempfile::tempdir().expect("a data directory");
        std::fs::write(workspace.path().join("answer.txt"), ANSWER_BEFORE).expect("seed");
        std::fs::write(workspace.path().join("untouched.txt"), UNTOUCHED).expect("seed");
        git(workspace.path(), &["init", "-q"]);
        // Line endings normalized in the test's own repository so that the
        // assertions below are about the edit and not about what the machine's
        // `core.autocrlf` happened to be. A developer's repository is not this;
        // this test is not about a developer's repository.
        git(workspace.path(), &["config", "core.autocrlf", "false"]);
        git(workspace.path(), &["add", "answer.txt", "untouched.txt"]);
        git(workspace.path(), &["commit", "-q", "-m", "the answer, as we believe it"]);
        Self { workspace, data }
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn data(&self) -> &Path {
        self.data.path()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.workspace.path().join(name)).expect("read it back")
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=acceptance", "-c", "user.email=acceptance@example.invalid"])
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::null())
        .status()
        .expect("git is installed");
    assert!(status.success(), "git {args:?} failed");
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let output =
        Command::new("git").args(args).current_dir(repo).output().expect("git is installed");
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Run the check ourselves, the way the tool runs it.
///
/// The acceptance criterion is that the task got done, so the test does not ask
/// the agent whether it did — it runs the command and looks at the exit code.
fn check_passes(workspace: &Path) -> bool {
    #[cfg(windows)]
    let (program, args) = ("cmd", vec!["/C", CHECK]);
    #[cfg(not(windows))]
    let (program, args) = ("sh", vec!["-c", CHECK]);
    Command::new(program)
        .args(args)
        .current_dir(workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the check runs")
        .success()
}

// ---------------------------------------------------------------------------
// The one fake: bytes on a wire
// ---------------------------------------------------------------------------

/// A transport that replays response bodies in order and records what it was
/// asked to send.
///
/// Running out of scripted responses is an error, not a hang: a test that
/// expects five model calls and gets six should fail loudly rather than wait.
struct ScriptedTransport {
    bodies: Mutex<VecDeque<String>>,
    posts: Mutex<Vec<TransportRequest>>,
}

impl ScriptedTransport {
    fn new(bodies: Vec<String>) -> Self {
        Self { bodies: Mutex::new(bodies.into()), posts: Mutex::new(Vec::new()) }
    }

    /// Every request body this transport was handed, as text.
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

/// The route this harness's provider is registered under — the same shape
/// `apps/agent` derives from the provider it selected.
fn route() -> ModelRoute {
    ModelRoute::new(ModelTier::Main, "scripted-endpoint", "scripted-model")
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// One process's worth of the stack, over a log and a workspace that outlive it.
///
/// The stack is [`stack::assemble`] — the same call the binary makes — over the
/// same store, the same file boundary, the same policy, the same file-and-git
/// tool surface and the same kernel Scheduler, Recovery and Resume. What the
/// process environment would have supplied, the test supplies instead: where the
/// run's files are, and a scripted model in place of an endpoint.
///
/// The model script is per-process, and that is the honest part: history comes
/// from the log, never from the provider, so the second process knows what the
/// first one was doing only by reading it back.
struct Process {
    stack: stack::Runtime,
    transport: Arc<ScriptedTransport>,
    session_id: SessionId,
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
        // The subject here is the approval and resume boundary, and the model
        // is a fixed script counted in attempts. The thinking pipeline would
        // hold the scripted edit for a branch record it was never scripted to
        // write, which would measure this test's premise instead of its task.
        judgment: false,
    })
    .await
    .expect("the assembly the binary runs");

    Process { stack, transport, session_id: SessionId::from_string(SESSION) }
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

impl Process {
    /// Queue `text` as the next turn's input and drive it.
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
            .expect("queue the task");
        self.drive().await
    }

    /// Drive whatever is already queued.
    async fn drive(&self) -> RunReport {
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
            .expect("read the log")
    }

    async fn count(&self, event_type: &str) -> usize {
        self.log().await.into_iter().filter(|e| e.event.event_type.as_str() == event_type).count()
    }

    /// Every event of one type whose payload names `tool`.
    ///
    /// Counters here are per tool on purpose: a call that quietly never ran can
    /// then not hide behind a call that did.
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

    /// The bodies of the requests that actually reached the wire.
    fn posts(&self) -> Vec<String> {
        self.transport.posts()
    }
}

// ---------------------------------------------------------------------------
// The acceptance run
// ---------------------------------------------------------------------------

/// M5, all five criteria, in one turn that stops and is carried on.
#[tokio::test]
async fn a_task_in_a_real_repository_is_done_and_the_world_says_so() {
    let repo = Repo::new();
    assert!(!check_passes(repo.path()), "the task starts from a failing check");

    // --- Process one: read, fix, and stop at the command that needs asking. ---
    let first = process(
        repo.path(),
        repo.data(),
        vec![
            // Step 1: look at what the task is about.
            tool_call_body(
                "call_read",
                &to_wire_name("fs.read"),
                serde_json::json!({ "path": "answer.txt" }),
            ),
            // Step 2: the fix.
            tool_call_body(
                "call_edit",
                &to_wire_name("fs.edit"),
                serde_json::json!({ "path": "answer.txt", "oldString": "41", "newString": "42" }),
            ),
            // Step 3: run the check. `process.spawn` is `ask`, so this is where
            // the turn stops.
            tool_call_body(
                "call_check",
                &to_wire_name("shell.run"),
                serde_json::json!({ "command": CHECK }),
            ),
        ],
    )
    .await;

    // M5 criterion 3, before anything runs: the note is durable first, and
    // the turn that starts later still finds it. Addressed to whatever turn
    // comes next, it is delivered at a step boundary inside that turn rather
    // than queued as one of its own.
    first.stack.scheduler.inbox().steer(&first.session_id, NOTE, None).await.expect("leave a note");

    let stopped = first.ask("answer.txt gives the wrong answer; fix it and run the check").await;

    // M5 criterion 2, first half: the run stops rather than guessing.
    assert_eq!(stopped.stop_reason, StopReason::NeedsUserInput);
    let pending = first.stack.approvals.pending(&first.session_id).await.expect("pending");
    assert_eq!(pending.len(), 1, "one call is waiting");
    assert_eq!(pending[0].tool_name, "shell.run", "and it is the one that runs a command");
    assert_eq!(
        first.count(EventType::TOOL_COMPLETED).await,
        2,
        "the two calls that needed no decision ran; the one that did, did not"
    );

    // The note reached the model in the process that ran the turn. This is the
    // only place steering has ever mattered, and it is over the wire rather
    // than in a projection this test could have got wrong.
    let first_posts = first.posts();
    assert!(
        first_posts.iter().any(|body| body.contains(NOTE)),
        "the note is in a request the run sent: {first_posts:#?}"
    );

    let approval_id = pending[0].approval_id.clone();
    drop(first); // the process exits with the decision unmade

    // --- Process two: a fresh assembly over the same log and the same repo. ---
    let second = process(
        repo.path(),
        repo.data(),
        vec![
            // The step after the wait: the diff the note asked for.
            tool_call_body("call_diff", &to_wire_name("git.diff"), serde_json::json!({})),
            // And the answer.
            answer_body("the check passes now"),
        ],
    )
    .await;
    second.stack.recovery.recover(&second.session_id).await.expect("recovery");
    second
        .stack
        .approvals
        .grant(&second.session_id, &approval_id, ApprovalScope::AllowOnce, "user")
        .await
        .expect("grant");

    let outcome = second
        .stack
        .resume
        .resume(&second.session_id, "the decision is made")
        .await
        .expect("resume");
    assert!(
        matches!(
            outcome,
            ResumeOutcome::Settled {
                disposition: Disposition::Ran { status: ToolStatus::Completed },
                ..
            }
        ),
        "the held command should run once the decision is in: {outcome:?}"
    );

    let report = second.drive().await;
    assert_eq!(report.stop_reason, StopReason::FinalAnswer);

    // --- M5 criterion 1: the task is done, verified by us. ---
    assert!(check_passes(repo.path()), "the check the run was asked to fix now passes");
    assert_eq!(repo.read("answer.txt"), ANSWER_AFTER);
    assert_eq!(
        git_out(repo.path(), &["status", "--porcelain"]).trim(),
        "M answer.txt",
        "the run changed the file the task was about and nothing else"
    );

    // --- M5 criterion 4: every request, decision, command and change is in
    // the log. ---
    assert_eq!(second.tool_events(EventType::TOOL_COMPLETED, "fs.read").await.len(), 1);
    assert_eq!(second.tool_events(EventType::TOOL_COMPLETED, "fs.edit").await.len(), 1);
    assert_eq!(
        second.tool_events(EventType::TOOL_COMPLETED, "shell.run").await.len(),
        1,
        "the command ran once: a resume that ran it twice would show twice"
    );
    assert_eq!(second.tool_events(EventType::TOOL_COMPLETED, "git.diff").await.len(), 1);
    assert_eq!(second.count(EventType::APPROVAL_REQUESTED).await, 1);
    assert_eq!(second.count(EventType::APPROVAL_GRANTED).await, 1);
    assert_eq!(
        second.count(EventType::MODEL_ATTEMPT_STARTED).await,
        5,
        "one attempt per model call: three before the wait, two after"
    );
    assert_eq!(second.count(EventType::TURN_STEERED).await, 1);
    assert_eq!(second.count(EventType::TURN_YIELDED).await, 1);
    assert_eq!(
        second.count(EventType::TOOL_OUTCOME_UNKNOWN).await,
        0,
        "nothing was left unsettled"
    );

    // --- M5 criterion 5: the diff the run saw is the diff that happened. ---
    let diff_events = second.tool_events(EventType::TOOL_COMPLETED, "git.diff").await;
    let diff = diff_events[0].event.payload["content"].as_str().expect("the diff result is text");
    assert!(diff.contains("-the answer is 41"), "the line that was there: {diff}");
    assert!(diff.contains("+the answer is 42"), "and the line that replaced it: {diff}");

    // --- M5 criterion 1, the other half: the file the task was not about
    // came out byte for byte. ---
    assert_eq!(repo.read("untouched.txt"), UNTOUCHED);
}

// ---------------------------------------------------------------------------
// The same acceptance, against a real model
// ---------------------------------------------------------------------------

/// The assembled binary, pointed at this repository.
///
/// The configuration variables are cleared: an acceptance run is a run under
/// the built-in defaults, and a developer with a user configuration exported
/// would otherwise be accepting something else.
fn binary(repo: &Repo, argv: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agent"))
        .args(argv)
        .env("AGENT_DATA_DIR", repo.data())
        .env("AGENT_WORKSPACE_DIR", repo.path())
        .env("AGENT_MODEL_PROVIDER", "openai")
        .env_remove("AGENT_USER_CONFIG")
        .env_remove("AGENT_MANAGED_POLICY")
        .output()
        .expect("the binary runs")
}

/// The first approval id in an `--events` chain, if the run stopped on one.
///
/// Read out of the chain rather than out of the sentence printed for a person:
/// the id a human copies off the terminal and the id in the log are the same
/// fact, and a test that scraped the prose would not notice if they stopped
/// being.
fn approval_of(chain: &str) -> Option<String> {
    chain
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == EventType::APPROVAL_REQUESTED)
        .and_then(|event| event["payload"]["approval_id"].as_str().map(str::to_string))
}

/// Whether an `--events` chain shows `tool` running to a completed result.
fn ran_tool(chain: &str, tool: &str) -> bool {
    chain.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).any(
        |event| {
            event["type"] == EventType::TOOL_COMPLETED
                && event["payload"]["tool_name"].as_str() == Some(tool)
        },
    )
}

/// The same task, against a real endpoint, driven through the real binary.
///
/// Ignored because it needs a credential and a network. Run it with the four
/// `AGENT_MODEL_*` variables set:
///
/// ```text
/// cargo test -p agent --test m5_acceptance a_real_model_fixes -- --ignored --nocapture
/// ```
///
/// This one is allowed to fail for the model's reason as well as the
/// framework's: a real model that cannot fix this file has not completed the
/// task, and saying so is what an acceptance test is for. The scripted test
/// above is the one that answers for the framework, because it is the one that
/// cannot fail for anybody else's reason.
///
/// Three invocations rather than one, because the pauses between them — start,
/// decide, carry on — are the thing being accepted, and because the wait between
/// them is the wait a person at a terminal would sit through. The assembly
/// itself is not the subject here: the scripted run above drives the same one
/// without a network, and this test is the one that says the binary, pointed at
/// a real repository and a real endpoint, does the same thing.
#[test]
#[ignore = "needs a live endpoint: AGENT_MODEL_BASE_URL/NAME/API_KEY and AGENT_MODEL_PROVIDER=openai"]
fn a_real_model_fixes_the_same_task() {
    let repo = Repo::new();
    assert!(!check_passes(repo.path()), "the task starts from a failing check");

    let task = format!(
        "In this repository the file answer.txt gives the wrong answer. Edit it so that it reads \
         exactly \"{ANSWER_AFTER}\" (with the trailing newline), then run the command `{CHECK}` to \
         confirm it passes, and show me the diff."
    );

    let run = binary(&repo, &["run", &task, "--session", SESSION, "--events"]);
    assert!(run.status.success(), "run: {}", String::from_utf8_lossy(&run.stderr));
    let chain = String::from_utf8_lossy(&run.stdout).to_string();

    let approval_id = approval_of(&chain).unwrap_or_else(|| {
        panic!("the run had to ask before running a command, and did not:\n{chain}")
    });

    let approved = binary(&repo, &["approve", &approval_id, "--session", SESSION]);
    assert!(approved.status.success(), "approve: {}", String::from_utf8_lossy(&approved.stderr));

    let resumed =
        binary(&repo, &["resume", "the decision is made", "--session", SESSION, "--events"]);
    assert!(resumed.status.success(), "resume: {}", String::from_utf8_lossy(&resumed.stderr));
    let carried_on = String::from_utf8_lossy(&resumed.stdout).to_string();
    assert!(
        ran_tool(&carried_on, "shell.run"),
        "the granted command ran after the decision:\n{carried_on}"
    );

    // The world, not the run's account of itself.
    assert!(check_passes(repo.path()), "the check the model was asked to fix now passes");
    assert_eq!(
        git_out(repo.path(), &["status", "--porcelain"]).trim(),
        "M answer.txt",
        "only the file the task was about may have changed"
    );
    assert_eq!(repo.read("untouched.txt"), UNTOUCHED);
}
