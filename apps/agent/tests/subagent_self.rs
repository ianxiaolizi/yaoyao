//! `subagent.run` with a `self` backend, end to end.
//!
//! A `self` backend starts **this binary** in a process of its own and speaks
//! the subagent wire to it (CONTRACT §7 #136). The in-process and stdio
//! backends each have a suite of their own; what only this file can check is
//! what the third kind exists for: a delegated turn that has its own process,
//! its own session log and its own writable face, while the parent's log holds
//! nothing but the spawn and the settlement.
//!
//! Two kinds of child are exercised. A **read-and-answer** child names no
//! `scope` and runs on the echo model, which is enough to check the carrier.
//! A **work** child names a `scope` and is driven by a scripted
//! OpenAI-compatible endpoint, because the question "does a write outside the
//! scope get refused" needs a model that actually calls a tool, and the echo
//! provider by construction never does.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;

const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";
const ENV_SANDBOX: &str = "AGENT_SANDBOX";
const ENV_BASE_URL: &str = "AGENT_MODEL_BASE_URL";
const ENV_MODEL_NAME: &str = "AGENT_MODEL_NAME";
const ENV_API_KEY: &str = "AGENT_MODEL_API_KEY";

/// What one run of the binary produced.
struct Ran {
    ok: bool,
    out: String,
    err: String,
}

fn dirs() -> (TempDir, TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap())
}

fn ran_with(
    data: &Path,
    workspace: &Path,
    config: &Path,
    env: &[(&str, &str)],
    argv: &[&str],
) -> Ran {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env("AGENT_DATA_DIR", data)
        .env("AGENT_WORKSPACE_DIR", workspace)
        .env("AGENT_MODEL_PROVIDER", "echo")
        .env(ENV_USER_CONFIG, config)
        .env_remove(ENV_MANAGED_POLICY)
        .env_remove(ENV_SANDBOX)
        .env_remove(ENV_BASE_URL)
        .env_remove(ENV_MODEL_NAME)
        .env_remove(ENV_API_KEY);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.args(argv).output().expect("the binary starts");
    Ran {
        ok: output.status.success(),
        out: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        err: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

fn ran(data: &Path, workspace: &Path, config: &Path, argv: &[&str]) -> Ran {
    ran_with(data, workspace, config, &[], argv)
}

/// The profile body that trusts and enables the provider, and lets a
/// delegation through the policy.
///
/// The file capabilities are named as well as `subagent.spawn`: a child built
/// from the parent's grants receives the parent's file authority, and a parent
/// that stopped to ask about a write would be a parent whose child can never
/// write — which is the one thing a `work` child exists to do.
const DELEGATING: &str = "tools = { allow = [\"subagent.spawn\", \"fs.read\", \"fs.write\", \"git.status\"] }\n\
                          plugins = { enable = [\"subagent.worker\"] }\n";

/// A user configuration with one `self` provider.
///
/// `scope` is what makes it a work child: without one the child is
/// read-and-answer and its surface never gains a write tool.
fn config(home: &TempDir, scope: Option<&str>) -> PathBuf {
    let path = home.path().join("user.toml");
    let scope_line = scope.map(|scope| format!("scope = [\"{scope}\"]\n")).unwrap_or_default();
    std::fs::write(
        &path,
        format!(
            "capabilities = [\"subagent.spawn\", \"fs.read\", \"fs.write\", \"git.status\"]\n\
             plugins = [\"subagent.worker@1.0.0\"]\n\n\
             [subagents.worker]\n\
             kind = \"self\"\n\
             requires = [\"subagent.spawn\"]\n\
             timeout = \"90s\"\n\
             {scope_line}\n\
             [subagents.worker.env]\n\
             {ENV_API_KEY} = \"secret://env/{ENV_API_KEY}\"\n\n\
             [profiles.p]\n{DELEGATING}"
        ),
    )
    .expect("a user configuration");
    path
}

/// Every event the session's log holds, in order, as the store wrote them.
fn events(data: &Path, session: &str) -> Vec<Value> {
    let path = data.join(session).join("events.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no log at {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let line: Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"));
            line["event"].clone()
        })
        .collect()
}

fn types(events: &[Value]) -> Vec<String> {
    events.iter().map(|event| event["event_type"].as_str().unwrap_or("?").to_string()).collect()
}

fn one<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    let matching: Vec<&Value> =
        events.iter().filter(|event| event["event_type"] == event_type).collect();
    assert_eq!(matching.len(), 1, "expected one {event_type} among {:?}", types(events));
    matching[0]
}

/// The sessions in the data directory other than the parent's.
fn children(data: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(data)
        .expect("the data directory")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name != "default" && name != "artifacts")
        .collect();
    names.sort();
    names
}

/// The processes whose command line contains `needle`.
///
/// A child is found by the data directory it was told to use: that path is
/// unique to one test, so a parallel test's child cannot be mistaken for this
/// one, and no parent/child link has to be guessed from process times.
fn agent_processes_using(needle: &str) -> Vec<u32> {
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='agent.exe'\" | \
         Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains('{needle}') }} | \
         ForEach-Object {{ $_.ProcessId }}"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("powershell runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Wait until the condition holds, or say how long it did not.
fn eventually(what: &str, check: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(30) {
        if check() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("{what} never held within 30s");
}

/// Every byte under `root`, so a test can ask "did this value reach the disk?".
fn every_byte_under(root: &Path) -> Vec<u8> {
    let mut all = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                all.extend_from_slice(&bytes);
            }
        }
    }
    all
}

/// A parent that has finished a turn, which is the only kind of parent a fork
/// will copy a child's history from.
fn a_parent_with_a_closed_turn(data: &Path, workspace: &Path, config: &Path, env: &[(&str, &str)]) {
    let run = ran_with(data, workspace, config, env, &["--profile", "p", "run", "hello"]);
    assert!(run.ok, "{}", run.err);
    assert!(
        types(&events(data, "default")).iter().any(|t| t == "turn/completed"),
        "the parent has no closed turn, so nothing below could fork from it"
    );
}

/// A scripted OpenAI-compatible endpoint.
///
/// One request per connection, answered with the chunks `reply` produces for
/// the body it received. It is a real socket rather than a fake transport
/// because the child is a real process: the only way to make it call a tool is
/// to be the endpoint it dials.
struct Endpoint {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Endpoint {
    fn start(reply: impl Fn(&str) -> Vec<Value> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("a bound address").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut stream) = connection else { continue };
                let Some(body) = read_request(&mut stream) else { continue };
                recorded.lock().expect("the recorder").push(body.clone());
                respond(&mut stream, &reply(&body));
            }
        });
        Self { port, seen }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().expect("the recorder").clone()
    }
}

/// One request's body, read off a socket that is already open.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

/// One response: every chunk as a `data:` frame, then the sentinel.
fn respond(stream: &mut TcpStream, chunks: &[Value]) {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(chunk).expect("a chunk serializes"));
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// An answer with no tool call.
fn says(text: &str) -> Vec<Value> {
    vec![
        json!({ "choices": [{ "delta": { "content": text }, "finish_reason": null }] }),
        json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
    ]
}

/// An answer that asks for one tool.
fn calls(id: &str, tool: &str, arguments: Value) -> Vec<Value> {
    vec![json!({ "choices": [{ "delta": { "tool_calls": [{
        "index": 0,
        "id": id,
        "function": { "name": tool, "arguments": arguments.to_string() },
    }] }, "finish_reason": "tool_calls" }] })]
}

#[test]
fn a_self_child_runs_in_a_process_of_its_own_and_answers_its_objective() {
    let (data, workspace, home) = dirs();
    let config = config(&home, None);
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config, &[]);

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"worker","objective":"look at the notes"}"#,
        ],
    );
    assert!(r.ok, "{}", r.err);
    assert!(r.out.contains("completed: echo: look at the notes"), "{}", r.out);

    let log = events(data.path(), "default");
    let spawned = one(&log, "subagent/spawned");
    let completed = one(&log, "subagent/completed");
    let child = spawned["payload"]["child_session_id"].as_str().expect("a child session");
    assert_eq!(completed["payload"]["outcome"]["status"], "completed");

    // The child is a session of its own and its turn is in *that* log — written
    // by the child process, from its own loop, in the store both processes
    // share. A parent that reported the answer without a child log would be a
    // delegation nobody could audit afterwards.
    assert_eq!(children(data.path()), vec![child.to_string()], "{:?}", types(&log));
    let child_log = events(data.path(), child);
    assert_eq!(child_log[0]["event_type"], "session/forked", "{:?}", types(&child_log));
    let child_types = types(&child_log);
    for wanted in ["model/response.completed", "turn/completed"] {
        assert!(
            child_types.iter().any(|t| t == wanted),
            "the child's log has no {wanted}: {child_types:?}"
        );
    }

    // The parent holds the spawn and the settlement and nothing of the child's
    // own turn: no tool calls, no model attempts, no plan.
    for unwanted in ["tool/started", "tool/completed", "plan/updated"] {
        assert!(
            !types(&log).iter().any(|t| t == unwanted),
            "the parent's log holds the child's {unwanted}: {:?}",
            types(&log)
        );
    }
}

/// The same objective on both carriers settles the same way: the shape of a
/// settlement is the port's, not the backend's.
#[test]
fn the_same_objective_settles_the_same_way_on_both_carriers() {
    let settle = |kind: &str| {
        let (data, workspace, home) = dirs();
        let path = home.path().join("user.toml");
        std::fs::write(
            &path,
            format!(
                "capabilities = [\"subagent.spawn\", \"fs.read\", \"fs.write\", \"git.status\"]\n\
                 plugins = [\"subagent.worker@1.0.0\"]\n\n\
                 [subagents.worker]\n\
                 kind = \"{kind}\"\n\
                 requires = [\"subagent.spawn\"]\n\n\
                 [profiles.p]\n{DELEGATING}"
            ),
        )
        .expect("a user configuration");
        a_parent_with_a_closed_turn(data.path(), workspace.path(), &path, &[]);
        let r = ran(
            data.path(),
            workspace.path(),
            &path,
            &[
                "--profile",
                "p",
                "tool",
                "subagent.run",
                r#"{"provider":"worker","objective":"the same question"}"#,
            ],
        );
        assert!(r.ok, "{}", r.err);
        let log = events(data.path(), "default");
        let outcome = one(&log, "subagent/completed")["payload"]["outcome"].clone();
        (outcome["status"].clone(), outcome["summary"].clone())
    };

    let in_process = settle("in-process");
    let self_hosted = settle("self");
    assert_eq!(self_hosted, in_process, "the carrier changed the settlement");
}

#[test]
fn a_work_child_writes_where_its_scope_points() {
    let (data, workspace, home) = dirs();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::create_dir_all(workspace.path().join("notes")).unwrap();
    let config = config(&home, Some("src"));
    let endpoint = Endpoint::start(|body| {
        if body.contains("\"role\":\"tool\"") {
            says("done")
        } else if body.contains("WRITE_REPORT_PROBE") {
            calls("call_1", "fs_write", json!({ "path": "src/report.txt", "content": "in scope" }))
        } else {
            says("hello")
        }
    });
    let env: &[(&str, &str)] = &[
        ("AGENT_MODEL_PROVIDER", "openai"),
        (ENV_BASE_URL, &endpoint.base_url()),
        (ENV_MODEL_NAME, "scripted"),
        (ENV_API_KEY, "sk-test-in-scope"),
    ];
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config, env);

    let r = ran_with(
        data.path(),
        workspace.path(),
        &config,
        env,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"worker","objective":"WRITE_REPORT_PROBE: write the report"}"#,
        ],
    );
    assert!(r.ok, "{}", r.err);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("src").join("report.txt")).unwrap(),
        "in scope",
        "a write inside the scope did not land: {}",
        r.err
    );
    assert!(r.out.contains("completed: done"), "{}", r.out);
}

/// A write outside the scope is refused by the child's own boundary, the
/// refusal names the directories it would allow, and the file never appears.
/// The settlement cannot claim success: it carries the child's own words, and
/// the parent's log holds the spawn and the settlement and nothing else.
#[test]
fn a_work_child_cannot_write_outside_its_scope() {
    let (data, workspace, home) = dirs();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::create_dir_all(workspace.path().join("notes")).unwrap();
    let config = config(&home, Some("src"));
    let endpoint = Endpoint::start(|body| {
        if body.contains("\"role\":\"tool\"") {
            says("I was not allowed to write there.")
        } else if body.contains("WRITE_REPORT_PROBE") {
            calls(
                "call_1",
                "fs_write",
                json!({ "path": "notes/leak.txt", "content": "out of scope" }),
            )
        } else {
            says("hello")
        }
    });
    let env: &[(&str, &str)] = &[
        ("AGENT_MODEL_PROVIDER", "openai"),
        (ENV_BASE_URL, &endpoint.base_url()),
        (ENV_MODEL_NAME, "scripted"),
        (ENV_API_KEY, "sk-test-out-of-scope"),
    ];
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config, env);

    let r = ran_with(
        data.path(),
        workspace.path(),
        &config,
        env,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"worker","objective":"WRITE_REPORT_PROBE: write the report"}"#,
        ],
    );
    assert!(r.ok, "out={} err={}", r.out, r.err);
    assert!(
        !workspace.path().join("notes").join("leak.txt").exists(),
        "a write outside the scope landed on disk"
    );

    // The refusal is recorded in the child's log, naming what the boundary
    // would have allowed — a refusal that only said "no" would leave the model
    // guessing what to do instead.
    let log = events(data.path(), "default");
    let child = one(&log, "subagent/spawned")["payload"]["child_session_id"]
        .as_str()
        .expect("a child session")
        .to_string();
    let child_log = events(data.path(), &child);
    let refusals: Vec<String> = child_log
        .iter()
        .filter(|event| {
            event["event_type"].as_str().is_some_and(|t| t.starts_with("tool/"))
                && event.to_string().contains("leak.txt")
        })
        .map(|event| event.to_string())
        .collect();
    assert!(!refusals.is_empty(), "the child's log holds no refusal: {:?}", types(&child_log));
    assert!(
        refusals.iter().any(|event| event.contains("src")),
        "the refusal does not say where a write would be allowed: {refusals:?}"
    );

    // The parent's log is the spawn and the settlement, and the settlement is
    // the child's words — the child cannot report a success nobody saw.
    let parent_types = types(&log);
    for wanted in ["subagent/spawned", "subagent/completed"] {
        assert!(parent_types.iter().any(|t| t == wanted), "{parent_types:?}");
    }
    assert!(
        !parent_types.iter().any(|t| t.starts_with("tool/")),
        "the parent's log holds the child's tool call: {parent_types:?}"
    );
}

/// The credential the child needs to reach a model is the one thing that
/// travels in its environment, and it reaches no log: not the parent's, not
/// the child's, not an artifact.
#[test]
fn the_credential_reaches_the_child_and_reaches_no_log() {
    let (data, workspace, home) = dirs();
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    let path = home.path().join("user.toml");
    std::fs::write(
        &path,
        format!(
            "capabilities = [\"subagent.spawn\", \"fs.read\", \"fs.write\", \"git.status\"]\n\
             plugins = [\"subagent.worker@1.0.0\"]\n\n\
             [subagents.worker]\n\
             kind = \"self\"\n\
             requires = [\"subagent.spawn\"]\n\n\
             [subagents.worker.env]\n\
             {ENV_API_KEY} = \"secret://env/{ENV_API_KEY}\"\n\n\
             [profiles.p]\n{DELEGATING}"
        ),
    )
    .expect("a user configuration");
    let endpoint = Endpoint::start(|body| {
        if body.contains("\"role\":\"tool\"") {
            says("noted")
        } else if body.contains("CREDENTIAL_PROBE") {
            calls("call_1", "fs_read", json!({ "path": "notes.txt" }))
        } else {
            says("hello")
        }
    });
    std::fs::write(workspace.path().join("notes.txt"), "the notes").unwrap();

    // Distinctive enough that a byte scan cannot find it by accident.
    let secret = "sk-probe-2f9c1b7a-CREDENTIAL-LEAK-CHECK";
    let env: &[(&str, &str)] = &[
        ("AGENT_MODEL_PROVIDER", "openai"),
        (ENV_BASE_URL, &endpoint.base_url()),
        (ENV_MODEL_NAME, "scripted"),
        (ENV_API_KEY, secret),
    ];
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &path, env);
    let r = ran_with(
        data.path(),
        workspace.path(),
        &path,
        env,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"worker","objective":"CREDENTIAL_PROBE: read the notes"}"#,
        ],
    );
    assert!(r.ok, "out={} err={}", r.out, r.err);

    // The child really did reach the endpoint — with the key the parent resolved
    // from its own reference — so "the key was not needed" is not the reason it
    // is absent from the logs.
    let requests = endpoint.requests();
    assert!(
        requests.iter().any(|body| body.contains("CREDENTIAL_PROBE")),
        "the child never reached the endpoint: {} request(s)",
        requests.len()
    );

    assert!(!r.out.contains(secret), "the credential reached stdout");
    assert!(!r.err.contains(secret), "the credential reached stderr");
    let on_disk = String::from_utf8_lossy(&every_byte_under(data.path())).to_string();
    assert!(!on_disk.contains(secret), "the credential was written under the data directory");
}

/// A `self` child given nothing to serve writes no result line and exits as a
/// child that never ran. The parent reads that as an answer it did not get, not
/// as a turn that failed — the distinction `EXIT_DID_NOT_RUN` exists for.
#[test]
fn a_child_told_nothing_writes_no_result_line_and_runs_no_turn() {
    let (data, workspace) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_agent"))
        .args([
            "subagent",
            "serve",
            "--child-session",
            "child",
            "--workspace",
            workspace.path().to_str().expect("a UTF-8 workspace"),
            "--data-dir",
            data.path().to_str().expect("a UTF-8 data directory"),
            "--provider",
            "echo",
            "--judgment",
            "gates",
            "--network",
            "deny",
            "--grants",
            "fs.read",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("the binary starts");

    assert_eq!(output.status.code(), Some(2), "a child that did not run exits 2");
    assert!(output.stdout.is_empty(), "a child that ran no turn wrote a result line");
    let err = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(err.contains("subagent serve failed"), "{err}");
    assert!(!data.path().join("child").exists(), "a child that ran no turn opened its session");
}

/// The path a `self` backend is started from is this binary, and no
/// configuration may name another one: `command` beside `kind = "self"` is an
/// entry that says two conflicting things, and resolution refuses it.
#[test]
fn a_self_backend_that_names_a_command_is_refused() {
    let (data, workspace, home) = dirs();
    let path = home.path().join("user.toml");
    std::fs::write(
        &path,
        "capabilities = [\"subagent.spawn\"]\n\
         plugins = [\"subagent.worker@1.0.0\"]\n\n\
         [subagents.worker]\n\
         kind = \"self\"\n\
         command = 'other-program'\n\
         requires = [\"subagent.spawn\"]\n\n\
         [profiles.p]\n\
         tools = { allow = [\"subagent.spawn\"] }\n\
         plugins = { enable = [\"subagent.worker\"] }\n",
    )
    .expect("a user configuration");

    let r = ran(data.path(), workspace.path(), &path, &["--profile", "p", "run", "hello"]);
    assert!(!r.ok, "a self backend with a command resolved anyway: {}", r.out);
    assert!(r.err.contains("command"), "the refusal names the conflict: {}", r.err);
}

/// A parent that dies without being able to stop anything takes its child with
/// it (CONTRACT §7 #136).
///
/// The child is deliberately stuck in a model request — the one state where a
/// graceful stop cannot happen, because nothing in the child is watching for
/// one — and the parent is killed outright. What is left is the OS: the child
/// was put in a job that dies with its parent, so an orphan is not a thing this
/// carrier can produce, and the child spends no tokens after the parent is
/// gone because it is no longer running.
#[test]
fn a_child_dies_with_a_parent_killed_outright() {
    let (data, workspace, home) = dirs();
    let config = config(&home, None);
    let endpoint = Endpoint::start(|body| {
        if body.contains("HANG_PROBE") {
            // Never answer: the child is inside a request and will not see
            // anything the parent might have said.
            std::thread::sleep(std::time::Duration::from_secs(300));
        }
        says("hello")
    });
    let env: &[(&str, &str)] = &[
        ("AGENT_MODEL_PROVIDER", "openai"),
        (ENV_BASE_URL, &endpoint.base_url()),
        (ENV_MODEL_NAME, "scripted"),
        (ENV_API_KEY, "sk-test-parent-death"),
    ];
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config, env);

    let mut parent = Command::new(env!("CARGO_BIN_EXE_agent"))
        .env("AGENT_DATA_DIR", data.path())
        .env("AGENT_WORKSPACE_DIR", workspace.path())
        .env("AGENT_MODEL_PROVIDER", "openai")
        .env(ENV_USER_CONFIG, &config)
        .env_remove(ENV_MANAGED_POLICY)
        .env_remove(ENV_SANDBOX)
        .env(ENV_BASE_URL, endpoint.base_url())
        .env(ENV_MODEL_NAME, "scripted")
        .env(ENV_API_KEY, "sk-test-parent-death")
        .args([
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"worker","objective":"HANG_PROBE: wait forever"}"#,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the parent starts");

    // The child's command line carries this test's data directory, so it is
    // found without guessing at process trees.
    let needle = data.path().display().to_string();
    let found = Arc::new(Mutex::new(None));
    let slot = found.clone();
    eventually("a self child was started", || {
        let processes = agent_processes_using(&needle);
        if let Some(pid) = processes.first().copied() {
            *slot.lock().expect("the slot") = Some(pid);
            true
        } else {
            false
        }
    });
    let child = found.lock().expect("the slot").expect("a child pid");

    let killed = Command::new("taskkill")
        .args(["/F", "/PID", &parent.id().to_string()])
        .output()
        .expect("taskkill runs");
    assert!(killed.status.success(), "the parent was not killed: {killed:?}");

    eventually("the child was gone with its parent", || {
        !agent_processes_using(&needle).contains(&child)
    });
    let _ = parent.wait();
}
