//! The command line, end to end: the real binary, the real store, on disk.
//!
//! Everything else tests the command line as a function. This spawns the
//! assembled binary the way a user would and reads what it left behind — which
//! is the only way to check the things that are decided before any command
//! runs: which session a line acts on, whether the session nobody named was
//! touched, and where the two outputs of `--events` go.
//!
//! Each test gets its own data and workspace directories, so the runs are
//! isolated from each other and from whatever is in the developer's home.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// The assembled binary, pointed at throwaway directories.
///
/// `AGENT_MODEL_PROVIDER` is set rather than left to its default because a
/// machine with a live endpoint configured exports it — an inherited value
/// would send these runs to the network and, worse, would do it *sometimes*,
/// depending on whose machine ran the suite.
///
/// The configuration variables are removed for the same reason, and it matters
/// more: a developer with a user configuration exported would have every
/// assertion in this file quietly run under it.
fn agent(data: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env("AGENT_DATA_DIR", data)
        .env("AGENT_WORKSPACE_DIR", workspace)
        .env("AGENT_MODEL_PROVIDER", "echo")
        .env_remove(ENV_USER_CONFIG)
        .env_remove(ENV_MANAGED_POLICY)
        // The sandbox is a host decision; a developer's exported value would
        // otherwise change what these runs are allowed to do.
        .env_remove("AGENT_SANDBOX");
    command
}

/// The configuration variables the binary documents, named here as the
/// user-facing contract they are: a rename that went unnoticed on both sides
/// would be a rename that broke every deployment, so this states the name an
/// operator's script writes rather than reading it off the product.
const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// What one run of the binary produced.
struct Ran {
    ok: bool,
    /// The status a script reads without parsing anything. `None` only when the
    /// process was killed by a signal, which nothing here does on purpose.
    code: Option<i32>,
    out: String,
    err: String,
}

fn ran(data: &Path, workspace: &Path, argv: &[&str]) -> Ran {
    ran_with(data, workspace, &[], argv)
}

/// The same, with configuration injected into this one process's environment.
fn ran_with(data: &Path, workspace: &Path, env: &[(&str, &str)], argv: &[&str]) -> Ran {
    let mut command = agent(data, workspace);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.args(argv).output().expect("the binary starts");
    Ran {
        ok: output.status.success(),
        code: output.status.code(),
        out: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        err: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

/// The `--events` chain: one JSON object per line.
fn chain(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"))
        })
        .collect()
}

fn types(chain: &[serde_json::Value]) -> Vec<&str> {
    chain.iter().map(|event| event["type"].as_str().expect("a type")).collect()
}

fn log_file(data: &Path, session: &str) -> PathBuf {
    data.join(session).join("events.jsonl")
}

/// A data directory and a workspace directory, thrown away together.
fn dirs() -> (TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap())
}

/// The two, plus somewhere to keep a configuration file that is neither.
fn dirs_with_config() -> (TempDir, TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap())
}

/// Write a project configuration where the binary looks for one.
fn write_project_config(workspace: &Path, text: &str) {
    let dir = workspace.join(".agent");
    std::fs::create_dir_all(&dir).expect("a project directory");
    std::fs::write(dir.join("config.toml"), text).expect("a project configuration");
}

/// Write a configuration file and return the path that points at it.
fn write_config(home: &TempDir, name: &str, text: &str) -> String {
    let path = home.path().join(name);
    std::fs::write(&path, text).expect("a configuration");
    path.display().to_string()
}

#[test]
fn an_unnamed_data_directory_lands_in_the_workspace_not_the_callers_cwd() {
    // Two throwaway directories and no `AGENT_DATA_DIR` between them: which one
    // the run's own state appears in is the whole question.
    let (workspace, elsewhere) = dirs();
    let output = Command::new(env!("CARGO_BIN_EXE_agent"))
        .env("AGENT_WORKSPACE_DIR", workspace.path())
        .env("AGENT_MODEL_PROVIDER", "echo")
        .env_remove("AGENT_DATA_DIR")
        .env_remove(ENV_USER_CONFIG)
        .env_remove(ENV_MANAGED_POLICY)
        .current_dir(elsewhere.path())
        .args(["run", "hello"])
        .output()
        .expect("the binary starts");
    assert!(output.status.success(), "the run failed: {}", String::from_utf8_lossy(&output.stderr));

    // The log is where the workspace says, under the default session.
    let log = workspace.path().join(".agent-data").join("default").join("events.jsonl");
    assert!(log.exists(), "no event log at {log:?}");

    // And nowhere else: the directory the command was invoked from is untouched,
    // which is what makes a run's state findable rather than cwd-shaped.
    assert!(
        !elsewhere.path().join(".agent-data").exists(),
        "the run's state landed in the caller's cwd"
    );
}

#[test]
fn the_named_session_is_the_one_that_grows() {
    let (data, workspace) = dirs();
    let r = ran(data.path(), workspace.path(), &["run", "hello", "--session", "other"]);

    assert!(r.ok, "{}", r.err);
    assert_eq!(r.out.trim(), "echo: hello");
    assert!(log_file(data.path(), "other").is_file(), "the named session was written");
    assert!(!data.path().join("default").exists(), "the session nobody named was not created");
}

#[test]
fn the_session_flag_works_in_every_position() {
    // The same command three ways, each into its own session — because a
    // shared flag that only works in front of the command is a shared flag
    // nobody can rely on.
    let (data, workspace) = dirs();
    let lines: [(&str, [&str; 4]); 3] = [
        ("before", ["--session", "before", "run", "hi"]),
        ("after", ["run", "hi", "--session", "after"]),
        ("between", ["run", "--session", "between", "hi"]),
    ];
    for (session, argv) in lines {
        let r = ran(data.path(), workspace.path(), &argv);
        assert!(r.ok, "{session}: {}", r.err);
        assert_eq!(r.out.trim(), "echo: hi", "{session}");
        assert!(log_file(data.path(), session).is_file(), "{session}");
    }
}

#[test]
fn the_events_chain_is_the_work_this_command_did() {
    let (data, workspace) = dirs();
    let r = ran(data.path(), workspace.path(), &["run", "hello", "--events"]);
    assert!(r.ok, "{}", r.err);

    let events = chain(&r.out);
    assert!(events.len() > 1, "a turn is more than one event: {}", r.out);
    // A turn on a session nobody has used yet starts that session first, so a
    // run's log and `session new`'s log begin the same way.
    assert_eq!(events[0]["type"], "session/started", "a new session starts first: {}", r.out);

    // A fresh session, so the chain is the whole log and its sequence runs from
    // zero without a gap.
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], i as u64, "seq is contiguous from zero: {}", r.out);
    }
    assert!(events.iter().all(|event| event["session_id"] == "default"));
    assert!(events.iter().any(|event| event["type"] == "turn/completed"));
    assert!(
        events.iter().any(|event| event["payload"]["payload"]["text"] == "hello"),
        "the text the user typed is in the chain: {}",
        r.out
    );

    // The two outputs are split: every line of stdout is an event, and the
    // human line is still there to read. The answer text does appear inside the
    // chain — it is what the turn recorded — so what is asserted is the shape
    // of stdout, not the absence of the words.
    for line in r.out.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("stdout is the chain and nothing else: {line:?}: {e}"));
    }
    // stderr is the human half: the model's own text shown as it arrived, then
    // the answer the command settled on. stdout stayed the chain throughout,
    // which is what keeps `--events | jq` usable while a person still reads.
    let human: Vec<&str> = r.err.trim().lines().collect();
    assert_eq!(
        *human.last().expect("an answer"),
        "echo: hello",
        "the answer is the last thing written: {}",
        r.err
    );
    assert_eq!(
        human[..human.len() - 1].join(""),
        "[model] echo: hello",
        "and the model's text was shown as it arrived: {}",
        r.err
    );
}

#[test]
fn a_second_command_shows_only_its_own_work() {
    let (data, workspace) = dirs();
    let first = ran(data.path(), workspace.path(), &["run", "first"]);
    assert!(first.ok, "{}", first.err);

    let second = ran(data.path(), workspace.path(), &["run", "second", "--events"]);
    assert!(second.ok, "{}", second.err);
    let events = chain(&second.out);

    assert!(
        events[0]["seq"].as_u64().expect("a seq") > 0,
        "the chain starts where the first command left off, not at zero: {}",
        second.out
    );
    let completed = events.iter().filter(|e| e["type"] == "turn/completed").count();
    assert_eq!(completed, 1, "only this command's turn is in the chain: {}", second.out);
}

#[test]
fn two_shapes_of_one_output_are_refused() {
    let (data, workspace) = dirs();
    let r = ran(data.path(), workspace.path(), &["--json", "--events", "run", "hi"]);

    assert!(!r.ok, "a line asking for two outputs fails rather than picking one");
    assert_eq!(r.code, Some(2), "nothing ran, so there is no answer to read: {}", r.err);
    assert!(r.err.contains("--json") && r.err.contains("--events"), "{}", r.err);
    assert!(r.err.contains("Usage"), "the refusal says how to call it: {}", r.err);
    assert!(r.out.is_empty(), "nothing was printed under the contradiction: {}", r.out);
    assert!(!data.path().join("default").exists(), "and nothing ran");
}

/// A command that did not run says so with `2`, whichever road it took there.
///
/// The other end of the alphabet is `1`: a command whose subject answered "no".
/// Two commands have such a subject — `tool`, when the call really came back
/// failed (`mcp_third_party.rs` pins that one, with a third-party server
/// supplying the refusal), and `doctor`, when the build is unhealthy. What this
/// pins is that nothing *else* lands on `1`: a script that reads `$?` and sees
/// it is looking at an answer, and every way of having no answer at all is here
/// instead.
#[test]
fn a_command_that_did_not_run_exits_two() {
    let (data, workspace) = dirs();

    for (why, argv) in [
        ("a usage error", vec!["tool"]),
        ("a name nobody registered", vec!["tool", "no.such.tool", "{}"]),
        ("a turn with nothing to say", vec!["run"]),
    ] {
        let r = ran(data.path(), workspace.path(), &argv);
        assert_eq!(r.code, Some(2), "{why}: {argv:?} exited {:?} ({})", r.code, r.err);
    }
}

/// A command's own flag without a value is a usage error, and nothing runs.
///
/// `export --out` used to read as "no directory was named": the flag was last on
/// the line, so there was no next word, and the bundle went to its default place
/// as if nothing had been asked for. Whoever typed it asked for something; the
/// only honest reading of a missing value is that it is missing.
#[test]
fn a_commands_own_flag_without_a_value_does_not_run() {
    let (data, workspace) = dirs();
    let r = ran(data.path(), workspace.path(), &["export", "--out"]);

    assert_eq!(r.code, Some(2), "a flag with no value did not run: {}", r.err);
    assert!(r.err.contains("--out") && r.err.contains("needs a value"), "{}", r.err);
    assert!(r.out.is_empty(), "there is no answer to read: {}", r.out);
    assert!(!data.path().join("default").exists(), "and nothing ran");
}

/// The other end of the alphabet: a tool that ran, and whose answer was no.
///
/// A call that never ran exits `2` (above). A call that *reached* a tool is
/// different even when it comes back bad: the failure is the answer, it is
/// printed, and `$?` says `1`. That is the standing a non-zero `shell.run` and
/// a third-party server's refusal already had — the two used to disagree
/// depending on nothing but whether the backend reported its failure as a
/// provider error or as a failed result.
#[test]
fn a_tool_that_ran_and_said_no_exits_one() {
    let (data, workspace) = dirs();

    // Inside the workspace, so the boundary admits it; not there, so the tool
    // is the one with something to say.
    let absent = workspace.path().join("absent.txt");
    let args = serde_json::json!({ "path": absent.display().to_string() }).to_string();
    let r = ran(data.path(), workspace.path(), &["tool", "fs.read", &args]);

    assert_eq!(r.code, Some(1), "a failed tool call said {:?} ({})", r.code, r.err);
    assert!(r.out.contains("absent.txt"), "the answer names the file it is about: {}", r.out);
}

/// A healthy doctor exits `0`, in both shapes.
///
/// The other verdict — an unhealthy build, exit `1` (§3 C3.4, 裁决 #5) — is
/// driven by the corruption tests below, one per class the store can produce.
/// This test stays the guard on the wiring: an exit status honoured too eagerly
/// shows up as a healthy doctor leaving non-zero.
#[test]
fn a_healthy_doctor_exits_zero() {
    let (data, workspace) = dirs();

    let text = ran(data.path(), workspace.path(), &["doctor"]);
    assert_eq!(text.code, Some(0), "a healthy build said {:?} ({})", text.code, text.err);
    assert!(text.out.contains("healthy"), "{}", text.out);
    assert!(text.out.contains("[execution_sandbox]"), "the backend is reported: {}", text.out);
    assert!(text.out.contains("mode=local"), "an unnamed run is local: {}", text.out);

    let json = ran(data.path(), workspace.path(), &["doctor", "--json"]);
    assert_eq!(json.code, Some(0), "a healthy build said {:?} ({})", json.code, json.err);
    assert!(json.out.contains("\"aggregate_status\": \"passed\""), "{}", json.out);
}

/// The sandbox is selected by the host, reported by the doctor, and a backend
/// this build does not have stops a run instead of quietly running something
/// less confined.
#[test]
fn the_selected_sandbox_is_reported_and_an_unavailable_one_stops_the_run() {
    let (data, workspace) = dirs();

    let read_only =
        ran_with(data.path(), workspace.path(), &[("AGENT_SANDBOX", "read-only")], &["doctor"]);
    assert_eq!(read_only.code, Some(0), "{}", read_only.err);
    assert!(read_only.out.contains("[execution_sandbox]"), "{}", read_only.out);
    assert!(read_only.out.contains("mode=read-only"), "{}", read_only.out);

    // Docker is a backend this build does not ship, and saying so is a failure
    // the doctor exits `1` for.
    let docker =
        ran_with(data.path(), workspace.path(), &[("AGENT_SANDBOX", "docker")], &["doctor"]);
    assert_eq!(docker.code, Some(1), "{}", docker.out);
    assert!(docker.out.contains("mode=docker"), "{}", docker.out);
    assert!(docker.out.contains("no backend for it"), "{}", docker.out);

    // The AppContainer backend exists in this build, so the doctor reports it
    // and answers according to whether this machine's one-time setup ran:
    // either ready (and it says with which SID), or failed naming the setup
    // command. Both are correct answers to different machines, and the test
    // pins the pairing rather than one of the machines.
    let app_container =
        ran_with(data.path(), workspace.path(), &[("AGENT_SANDBOX", "app-container")], &["doctor"]);
    assert!(app_container.out.contains("mode=app-container"), "{}", app_container.out);
    if app_container.out.contains("setup is not in place") {
        assert_eq!(app_container.code, Some(1), "{}", app_container.out);
    } else {
        assert_eq!(app_container.code, Some(0), "{}", app_container.out);
        assert!(app_container.out.contains("sid="), "{}", app_container.out);
    }

    // Selecting Docker for a run is a refusal to start, not a local run.
    let docker_run =
        ran_with(data.path(), workspace.path(), &[("AGENT_SANDBOX", "docker")], &["run", "hello"]);
    assert_eq!(docker_run.code, Some(2), "{}", docker_run.err);
    assert!(docker_run.err.contains("Docker"), "{}", docker_run.err);

    let unknown =
        ran_with(data.path(), workspace.path(), &[("AGENT_SANDBOX", "sandbox")], &["run", "hello"]);
    assert_eq!(unknown.code, Some(2), "{}", unknown.err);
    assert!(unknown.err.contains("unknown execution sandbox"), "{}", unknown.err);
}

/// `sandbox` is how the one-time machine setup is run and checked. The command
/// reports either "ready" or "not set up", and never a third thing that reads
/// like success.
#[test]
fn the_sandbox_command_reports_setup_and_refuses_nonsense() {
    let (data, workspace) = dirs();

    let usage = ran(data.path(), workspace.path(), &["sandbox"]);
    assert_eq!(usage.code, Some(2));
    assert!(usage.err.contains("sandbox setup"), "{}", usage.err);

    let unknown = ran(data.path(), workspace.path(), &["sandbox", "boil"]);
    assert_eq!(unknown.code, Some(2));
    assert!(unknown.err.contains("unknown sandbox subcommand"), "{}", unknown.err);

    let status = ran(data.path(), workspace.path(), &["sandbox", "status"]);
    assert!(status.out.contains("profile="), "{}", status.out);
    // `0` is a set-up machine and `2` is one where setup has not run; the
    // command's job is to tell them apart, not to make both look fine.
    assert!(matches!(status.code, Some(0) | Some(2)), "{:?} {}", status.code, status.err);
    if status.code == Some(2) {
        assert!(status.err.contains("sandbox setup"), "{}", status.err);
    }
}

/// One complete turn, so a doctor run has a session to judge.
fn completed_run(data: &Path, workspace: &Path) {
    let r = ran(data, workspace, &["run", "hello"]);
    assert_eq!(r.code, Some(0), "the fixture run failed: {}", r.err);
}

fn log_lines(data: &Path, session: &str) -> Vec<String> {
    let text = std::fs::read_to_string(log_file(data, session)).expect("the event log");
    text.lines().map(str::to_string).collect()
}

fn write_log_lines(data: &Path, session: &str, lines: &[String]) {
    std::fs::write(log_file(data, session), lines.join("\n") + "\n").expect("rewritten log");
}

/// A torn tail is a class of its own, and the doctor is where it is reported.
///
/// The store repairs the fragment when it opens the log, so the evidence is
/// gone afterwards — the second run below is deliberately healthy. That the
/// repair is one-shot is the limit ST-8 states, not a bug: the dropped bytes
/// were never a record.
#[test]
fn a_torn_tail_is_reported_and_the_repair_is_reported_once() {
    let (data, workspace) = dirs();
    completed_run(data.path(), workspace.path());

    let log = log_file(data.path(), "default");
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).expect("the log opens");
    file.write_all(b"{\"seq\":13,\"recorded_at\":\"2026-10-").expect("a torn write");
    drop(file);

    let r = ran(data.path(), workspace.path(), &["doctor"]);
    assert_eq!(r.code, Some(1), "an unhealthy build said {:?} ({})", r.code, r.err);
    assert!(r.out.contains("[log_tail]"), "the class is named: {}", r.out);
    assert!(r.out.contains("torn fragment"), "{}", r.out);

    let again = ran(data.path(), workspace.path(), &["doctor"]);
    assert_eq!(again.code, Some(0), "the repair is done and honest: {}", again.err);
    assert!(again.out.contains("tail intact"), "{}", again.out);
}

/// A gap in the durable sequence is the second class, and the check that names
/// it reads raw lines rather than the store: opening the log fails closed on
/// non-contiguous seq, so the store cannot say *what* the gap is.
#[test]
fn a_sequence_gap_exits_one_and_names_the_check() {
    let (data, workspace) = dirs();
    completed_run(data.path(), workspace.path());

    let mut lines = log_lines(data.path(), "default");
    let mut event: serde_json::Value = serde_json::from_str(&lines[5]).expect("an event");
    event["seq"] = serde_json::json!(6);
    lines[5] = event.to_string();
    write_log_lines(data.path(), "default", &lines);

    // JSON here rather than text: the class has to be readable by a script too.
    let r = ran(data.path(), workspace.path(), &["doctor", "--json"]);
    assert_eq!(r.code, Some(1), "an unhealthy build said {:?} ({})", r.code, r.err);
    assert!(r.out.contains("\"seq_contiguity\""), "the class is named: {}", r.out);
    assert!(r.out.contains("seq gap"), "{}", r.out);
}

/// A record written by a future build is refused when the log is opened, and a
/// command that cannot read its history does not run (`2`).
///
/// The store unit tests pin the error kind; this pins the process contract and
/// that the sentence carries both versions, so an operator can tell "this log
/// belongs to a newer build" from "this log is corrupt".
#[test]
fn an_event_from_a_future_schema_exits_two_and_names_the_version() {
    let (data, workspace) = dirs();
    completed_run(data.path(), workspace.path());

    let mut lines = log_lines(data.path(), "default");
    let mut event: serde_json::Value = serde_json::from_str(&lines[0]).expect("an event");
    event["event"]["schema_version"] = serde_json::json!("99.0.0");
    lines[0] = event.to_string();
    write_log_lines(data.path(), "default", &lines);

    let r = ran(data.path(), workspace.path(), &["run", "hello"]);
    assert_eq!(r.code, Some(2), "the command could not read its log: {}{}", r.out, r.err);
    assert!(r.err.contains("99.0.0"), "the refusal names the log's claim: {}", r.err);
    assert!(r.err.contains("0.1.0"), "the refusal names what this build reads: {}", r.err);
}

/// The third class: the process died with a run open. The last line of a
/// completed turn is `run/completed`, so dropping it leaves exactly that.
#[test]
fn a_run_left_open_exits_one_and_names_the_check() {
    let (data, workspace) = dirs();
    completed_run(data.path(), workspace.path());

    let mut lines = log_lines(data.path(), "default");
    let last = lines.last().expect("a log with events");
    assert!(last.contains("\"run/completed\""), "the fixture must end a run: {last}");
    lines.pop();
    write_log_lines(data.path(), "default", &lines);

    let r = ran(data.path(), workspace.path(), &["doctor"]);
    assert_eq!(r.code, Some(1), "an unhealthy build said {:?} ({})", r.code, r.err);
    assert!(r.out.contains("[open_entities]"), "the class is named: {}", r.out);
    assert!(r.out.contains("open with no terminal event"), "{}", r.out);
}

/// The fourth class: a checkpoint whose metadata is valid and whose seq the log
/// holds, but whose state folding the log does not produce. This is the case
/// `build` cannot notice — it would serve the wrong state — so the doctor folds
/// the log up to the checkpoint's own seq and compares.
#[test]
fn a_diverging_checkpoint_exits_one_and_names_the_check() {
    let (data, workspace) = dirs();
    completed_run(data.path(), workspace.path());

    let lines = log_lines(data.path(), "default");
    let last: serde_json::Value = serde_json::from_str(lines.last().expect("events")).unwrap();
    let as_of_seq = last["seq"].as_u64().expect("a seq");
    let checkpoints = data.path().join("default").join("checkpoints");
    std::fs::create_dir_all(&checkpoints).unwrap();

    // A well-formed checkpoint for this projection at the log's own head, with
    // a counter the log does not support.
    let checkpoint = serde_json::json!({
        "session_id": "default",
        "projection_key": "session",
        "as_of_seq": as_of_seq,
        "projection_schema_version": "session/0.1.0",
        "state_version": 3,
        "created_at": "2026-10-17T00:00:00Z",
        "state": {
            "started": true,
            "open_turn": null,
            "open_run": null,
            "open_step": null,
            "turns_started": 99,
            "turns_terminal": 1,
            "runs_started": 1,
            "runs_terminal": 1,
            "steps_started": 1,
            "steps_terminal": 1
        }
    });
    std::fs::write(checkpoints.join("session.json"), checkpoint.to_string()).unwrap();

    let r = ran(data.path(), workspace.path(), &["doctor"]);
    assert_eq!(r.code, Some(1), "an unhealthy build said {:?} ({})", r.code, r.err);
    assert!(r.out.contains("[checkpoint_consistency]"), "the class is named: {}", r.out);
    assert!(r.out.contains("diverges"), "{}", r.out);
}

/// The fifth class: the configuration a run would assemble under cannot be
/// resolved. A command that reads it exits `2` before running; the doctor is
/// different on purpose — it reports the configuration as the finding, and the
/// build is unhealthy because of it.
#[test]
fn an_unresolvable_configuration_exits_one_and_names_the_check() {
    let (data, workspace, home) = dirs_with_config();
    completed_run(data.path(), workspace.path());
    let path = write_config(&home, "user.toml", "not = [valid toml\n");

    let r =
        ran_with(data.path(), workspace.path(), &[(ENV_USER_CONFIG, path.as_str())], &["doctor"]);
    assert_eq!(r.code, Some(1), "an unhealthy build said {:?} ({})", r.code, r.err);
    assert!(r.out.contains("[profile_resolution]"), "the class is named: {}", r.out);
    assert!(r.out.contains("does not resolve"), "{}", r.out);
}

/// A configuration written for a future schema is refused as a *version*
/// problem, and the command that reads it does not run (`2`).
///
/// This is the end-to-end half of YA-67: the `agent-config` unit tests pin the
/// error kind; this pins that the binary surfaces it to a script as "did not
/// run" and that the sentence names the version rather than a TOML syntax
/// complaint — a future file that added no unknown key would otherwise have
/// been accepted silently.
#[test]
fn a_configuration_from_a_future_schema_does_not_run() {
    let (data, workspace, home) = dirs_with_config();
    let path =
        write_config(&home, "user.toml", "schema_version = \"99\"\ncapabilities = [\"fs.read\"]\n");

    let r = ran_with(
        data.path(),
        workspace.path(),
        &[(ENV_USER_CONFIG, path.as_str())],
        &["run", "hello"],
    );
    assert_eq!(
        r.code,
        Some(2),
        "a command that could not configure did not run: {}{}",
        r.out,
        r.err
    );
    assert!(r.err.contains("schema version"), "the refusal names the problem: {}", r.err);
    assert!(r.err.contains("99"), "the refusal names the file's claim: {}", r.err);
}

/// A `resume` with nothing to resume exits `2`: the command ran, nothing
/// happened.
///
/// The other way to land on `2` — `a_command_that_did_not_run_exits_two` above
/// is the ways a command never starts. This one starts, reads the session,
/// finds no wait to settle and stops. `0` would tell a script the turn is
/// resumed; there is no turn. Neither is it `1`: nothing was asked and refused,
/// the log simply has no decision waiting in it. The other two outcomes of
/// `resume` stay `0`, deliberately — `Waiting` is the same "needs a person"
/// answer `agent run` gives when it stops at an approval (§3 C3.3), and
/// `Settled` did the work it was asked for.
#[test]
fn resume_with_nothing_to_resume_exits_two() {
    let (data, workspace) = dirs();

    for extra in [Vec::new(), vec!["--json"]] {
        let mut argv = vec!["resume", "--session", "quiet"];
        argv.extend(extra.iter().copied());
        let r = ran(data.path(), workspace.path(), &argv);
        assert_eq!(r.code, Some(2), "{argv:?} said {:?} ({})", r.code, r.err);
        assert!(
            r.out.contains("nothing to resume") || r.out.contains("nothing_to_resume"),
            "{argv:?} answered neither way: stdout={:?} stderr={:?}",
            r.out,
            r.err
        );
    }
}

/// Two processes, one session: the second is told the first is driving.
///
/// The first process is stood in for by this test holding the claim file
/// itself — the same path, the same OS lock, a different process. Racing two
/// real `ask` runs would test the echo provider's speed rather than the claim:
/// the lock is the whole mechanism, and whoever else takes it looks identical
/// to the store.
#[test]
fn a_second_process_asking_the_same_session_is_told_it_is_busy() {
    let (data, workspace) = dirs();
    let session_dir = data.path().join("contended");

    // A first turn, so the session — and the claim file — exist exactly as the
    // product makes them. The file has to be the product's own: one created
    // here would be labelled with this process's integrity and not the
    // sandboxed binary's, which is a property of the test host and not of the
    // claim.
    let first = ran(data.path(), workspace.path(), &["ask", "hello", "--session", "contended"]);
    assert!(first.ok, "{}", first.err);
    let before = std::fs::read_to_string(session_dir.join("events.jsonl")).expect("the log");

    // The stand-in for the process that is mid-turn: same file, same OS lock,
    // a different process. Racing two real `ask` runs would test the echo
    // provider's speed rather than the claim.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(session_dir.join("driver.lock"))
        .expect("the claim file the product made");
    held.try_lock().expect("the stand-in driver gets the claim first");

    let refused =
        ran(data.path(), workspace.path(), &["ask", "and again", "--session", "contended"]);
    assert!(!refused.ok, "a second driver must not run the turn: {}", refused.out);
    assert_eq!(refused.code, Some(2), "a refusal is a command that did not happen");
    assert!(
        refused.err.contains("already being driven"),
        "the refusal has to say what it is: stdout={:?} stderr={:?}",
        refused.out,
        refused.err
    );

    // The text was appended before the run was refused — a queued prompt and a
    // turn are separate facts — and nothing else was: no run opened on a
    // session another process is driving.
    let after = std::fs::read_to_string(session_dir.join("events.jsonl")).expect("the log");
    assert_eq!(
        after.lines().count(),
        before.lines().count() + 1,
        "only the queued text may be written: {}",
        after
    );
    assert_eq!(after.matches("run/started").count(), before.matches("run/started").count());

    // With the claim released the queued text runs, so this was a refusal and
    // not a command that never works.
    drop(held);
    let later = ran(data.path(), workspace.path(), &["ask", "third", "--session", "contended"]);
    assert!(later.ok, "{}", later.err);
    assert!(
        later.out.contains("echo: and again"),
        "the text queued during the refusal runs first: {}",
        later.out
    );
}

#[test]
fn ask_is_run_under_another_name() {
    let (data, workspace) = dirs();
    let run = ran(data.path(), workspace.path(), &["run", "hello", "--session", "a", "--events"]);
    let ask = ran(data.path(), workspace.path(), &["ask", "hello", "--session", "b", "--events"]);

    assert!(run.ok, "{}", run.err);
    assert!(ask.ok, "{}", ask.err);
    assert_eq!(
        types(&chain(&run.out)),
        types(&chain(&ask.out)),
        "one implementation: the same events in the same order"
    );
    assert_eq!(run.err, ask.err, "and the same answer");
}

#[test]
fn status_reports_the_session_it_was_given() {
    let (data, workspace) = dirs();
    let turn = ran(data.path(), workspace.path(), &["run", "hello", "--session", "other"]);
    assert!(turn.ok, "{}", turn.err);

    let r = ran(data.path(), workspace.path(), &["status", "--session", "other", "--json"]);
    assert!(r.ok, "{}", r.err);
    let status: serde_json::Value = serde_json::from_str(&r.out).expect("one JSON object");

    assert_eq!(status["session"], "other");
    assert_eq!(status["pending"], 0, "the turn consumed its input");
    assert!(status["open_run"].is_null(), "and it finished: {}", r.out);
    assert!(!data.path().join("default").exists(), "reading a session does not create another");
}

#[test]
fn a_steered_note_reaches_the_turn_another_process_drives() {
    let (data, workspace) = dirs();

    // One process writes the note. Nothing is running yet, so it is addressed
    // to whatever turn comes next — which is exactly what it says back.
    let steer =
        ran(data.path(), workspace.path(), &["steer", "also check the tests", "--session", "s"]);
    assert!(steer.ok, "{}", steer.err);
    assert!(steer.out.contains("turn: next"), "addressed to no turn in particular: {}", steer.out);

    // A different process drives the turn, and the note joins its first step.
    // This is the cross-process half of steering: the note is durable before
    // anyone is listening, and the run that starts later still finds it.
    let run = ran(
        data.path(),
        workspace.path(),
        &["run", "read the notes", "--session", "s", "--events"],
    );
    assert!(run.ok, "{}", run.err);

    let events = chain(&run.out);
    let steered: Vec<_> = events.iter().filter(|e| e["type"] == "turn/steered").collect();
    assert_eq!(steered.len(), 1, "injected once, at a step boundary: {}", run.out);
    assert_eq!(steered[0]["payload"]["text"], "also check the tests");
}

#[test]
fn steer_refuses_to_run_with_nothing_to_say() {
    let (data, workspace) = dirs();
    let r = ran(data.path(), workspace.path(), &["steer", "--session", "s"]);

    assert!(!r.ok, "no text is a usage error, not a note that steers nothing");
    assert!(r.err.contains("Usage"), "{}", r.err);
    assert!(!log_file(data.path(), "s").exists(), "and nothing was written: {}", r.out);
}

#[test]
fn export_writes_the_session_it_was_given() {
    let (data, workspace) = dirs();
    let turn = ran(data.path(), workspace.path(), &["run", "hello", "--session", "s"]);
    assert!(turn.ok, "{}", turn.err);

    let out = data.path().join("exported");
    let r = ran(
        data.path(),
        workspace.path(),
        &["export", "--session", "s", "--out", out.to_str().expect("a UTF-8 path")],
    );
    assert!(r.ok, "{}", r.err);

    let bundle = std::fs::read_to_string(out.join("bundle.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&bundle).expect("one JSON object");
    assert_eq!(value["session"], "s");
    let events = value["events"].as_array().expect("the log");
    assert!(
        events.iter().any(|e| e["event"]["event_type"] == "turn/completed"),
        "the exported events are the session's log: {bundle}"
    );
    assert!(
        value["artifacts"].as_array().expect("an index").is_empty(),
        "this session offloaded nothing, so the index says so"
    );

    let markdown = std::fs::read_to_string(out.join("session.md")).unwrap();
    assert!(markdown.contains("# Session s"), "{markdown}");
    assert!(markdown.contains("turn/completed"), "{markdown}");
}

/// A token of the shape the redactor knows, pasted into a prompt by hand.
///
/// The kernel scrubs tool arguments and tool results on the way into the log,
/// but an inbox item's payload is written verbatim — it is what a person typed,
/// and the queue has no business rewriting it. That is the door this test walks
/// through, and it is the reason `--redacted` exists at all: a session where
/// somebody pasted a key is a session whose log holds that key.
const LEAKED: &str = "ghp_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn export_redacted_scrubs_the_session_a_token_was_pasted_into() {
    let (data, workspace) = dirs();
    let turn = ran(
        data.path(),
        workspace.path(),
        &["run", &format!("here is my token: {LEAKED}"), "--session", "leaky"],
    );
    assert!(turn.ok, "{}", turn.err);

    let plain_out = data.path().join("plain");
    let r = ran(
        data.path(),
        workspace.path(),
        &["export", "--session", "leaky", "--out", plain_out.to_str().expect("a UTF-8 path")],
    );
    assert!(r.ok, "{}", r.err);

    let quiet_out = data.path().join("quiet");
    let r = ran(
        data.path(),
        workspace.path(),
        &[
            "export",
            "--session",
            "leaky",
            "--redacted",
            "--out",
            quiet_out.to_str().expect("a UTF-8 path"),
        ],
    );
    assert!(r.ok, "{}", r.err);

    let plain_markdown = std::fs::read_to_string(plain_out.join("session.md")).unwrap();
    let plain_bundle_text = std::fs::read_to_string(plain_out.join("bundle.json")).unwrap();

    // The control comes first. Without it, a token that never reached the log
    // would satisfy every assertion below.
    assert!(
        plain_bundle_text.contains(LEAKED),
        "the fixture no longer gets the token into the log: {plain_bundle_text}"
    );
    assert!(
        plain_markdown.contains(LEAKED),
        "the one-line summary of an inbox item is where a pasted token shows up: {plain_markdown}"
    );

    // Both halves, because both are rendered from the same scrubbed events and
    // a leak in either is a leak. `session.md` is the one a person pastes into
    // a ticket.
    for half in ["session.md", "bundle.json"] {
        let text = std::fs::read_to_string(quiet_out.join(half)).unwrap();
        assert!(!text.contains(LEAKED), "{half} still carries the token: {text}");
        assert!(text.contains("***"), "{half} should show what it removed: {text}");
    }

    let plain: serde_json::Value =
        serde_json::from_str(&plain_bundle_text).expect("one JSON object");
    let quiet: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(quiet_out.join("bundle.json")).unwrap())
            .expect("one JSON object");
    // Recorded in both directions: a reader must not have to infer "it was
    // scrubbed" from the absence of something.
    assert_eq!(plain["redacted"], false);
    assert_eq!(quiet["redacted"], true);
    // The events are still the log — same count, same ids — with quieter bytes.
    assert_eq!(plain["event_count"], quiet["event_count"]);
    let ids = |bundle: &serde_json::Value| -> Vec<String> {
        bundle["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stored| stored["event"]["event_id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&plain), ids(&quiet));
}

/// A secret that lives in configuration, not in a prompt.
///
/// Most configuration never reaches the log: an MCP server's `env` and
/// `headers` are used where the connection is opened, not recorded. The one
/// value that does travel is the decision backend's label, which records the
/// sidecar's full URL — userinfo included — so that is the path by which a
/// configured secret can end up in an export. A sentinel in a field that never
/// enters the log would make this test pass without testing anything, which is
/// why the positive control below reads the log first.
const CONFIGURED: &str = "sk-cccccccccccccccccccccccccccccccc";

#[test]
fn export_redacted_scrubs_a_secret_that_came_from_configuration() {
    let (data, workspace, home) = dirs_with_config();
    let user = write_config(
        &home,
        "user.toml",
        &format!("[decisions.laya]\nurl = \"http://sidecar:{CONFIGURED}@127.0.0.1:8231\"\n"),
    );
    let env = [(ENV_USER_CONFIG, user.as_str())];

    let turn = ran_with(data.path(), workspace.path(), &env, &["run", "plan the day"]);
    assert!(turn.ok, "{}", turn.err);

    // The control: the configured value really did reach the durable log, via
    // `profile/resolved`'s `laya:<url>` label. Without this, every absence
    // assertion below would be satisfied by a value that never left the file.
    let log = std::fs::read_to_string(log_file(data.path(), "default")).unwrap();
    assert!(
        log.contains(CONFIGURED),
        "the fixture never got the configured secret into the log: {log}"
    );

    let plain_out = data.path().join("plain-config");
    let r = ran_with(
        data.path(),
        workspace.path(),
        &env,
        &["export", "--out", plain_out.to_str().expect("a UTF-8 path")],
    );
    assert!(r.ok, "{}", r.err);

    let quiet_out = data.path().join("quiet-config");
    let r = ran_with(
        data.path(),
        workspace.path(),
        &env,
        &["export", "--redacted", "--out", quiet_out.to_str().expect("a UTF-8 path")],
    );
    assert!(r.ok, "{}", r.err);

    // The bundle carries whole events, so it is the half where a payload value
    // is guaranteed to appear. The markdown is a set of one-line summaries and
    // may never have shown this field; requiring it there would be asserting
    // the renderer's choice rather than the scrubber's behavior.
    let plain_text = std::fs::read_to_string(plain_out.join("bundle.json")).unwrap();
    assert!(
        plain_text.contains(CONFIGURED),
        "bundle.json no longer carries the configuration-derived secret: {plain_text}"
    );
    let quiet_text = std::fs::read_to_string(quiet_out.join("bundle.json")).unwrap();
    assert!(
        !quiet_text.contains(CONFIGURED),
        "bundle.json still carries the configured secret: {quiet_text}"
    );
    assert!(quiet_text.contains("***"), "the bundle should show what it removed: {quiet_text}");

    // The document a person pastes into a ticket must not carry it either.
    let quiet_markdown = std::fs::read_to_string(quiet_out.join("session.md")).unwrap();
    assert!(
        !quiet_markdown.contains(CONFIGURED),
        "session.md still carries the configured secret: {quiet_markdown}"
    );

    let plain: serde_json::Value = serde_json::from_str(&plain_text).unwrap();
    let quiet: serde_json::Value = serde_json::from_str(&quiet_text).unwrap();
    assert_eq!(plain["redacted"], false);
    assert_eq!(quiet["redacted"], true);
}

#[test]
fn interrupt_refuses_a_session_with_nothing_running() {
    let (data, workspace) = dirs();
    // A session that exists and has stopped: this run ends before the next
    // command starts, so nothing is in flight and no other process is holding a
    // run open. That state — an interrupt arriving at an idle session — is the
    // one that has to be refused, because the scheduler reads a pending
    // interrupt at the *top* of its boundary loop: an item queued here would
    // not wait, it would kill the next turn's first step.
    let turn = ran(data.path(), workspace.path(), &["run", "hello", "--session", "idle"]);
    assert!(turn.ok, "{}", turn.err);
    let before = std::fs::read_to_string(log_file(data.path(), "idle")).unwrap();

    let r = ran(data.path(), workspace.path(), &["interrupt", "--session", "idle"]);

    assert_eq!(r.code, Some(2), "a command with nothing to do exits 2: {}{}", r.out, r.err);
    assert!(r.out.contains("no run to interrupt"), "{}", r.out);
    let after = std::fs::read_to_string(log_file(data.path(), "idle")).unwrap();
    assert_eq!(before, after, "a refused interrupt must not write to the log");
}

#[test]
fn the_help_lists_the_commands_that_exist() {
    // The command exists in the dispatch table *and* in what a user is told.
    // Both halves are cheap to check here and expensive to notice missing: a
    // command nobody can discover is a command that is not there.
    let (data, workspace) = dirs();
    let r = ran(data.path(), workspace.path(), &["interrupt", "--session", "nothing"]);
    assert_eq!(r.code, Some(2), "{}", r.err);

    let help = ran(data.path(), workspace.path(), &["nonesuch"]);
    assert_eq!(help.code, Some(2));
    assert!(help.err.contains("interrupt"), "the usage block does not list it: {}", help.err);
    assert!(
        help.err.contains("--redacted"),
        "the usage block does not list --redacted: {}",
        help.err
    );
}

#[test]
fn web_names_the_model_provider_and_refuses_one_it_cannot_build() {
    // Which model the page talks to is decided when the server starts, so a
    // provider that cannot be built has to be refused there: serving a page
    // whose every turn would fail is worse than not starting.
    let (data, workspace) = dirs();

    let help = ran(data.path(), workspace.path(), &["web", "--help"]);
    assert!(help.ok, "{}", help.err);
    assert!(help.out.contains("--provider <echo|openai>"), "{}", help.out);
    assert!(help.out.contains("--model <name>"), "{}", help.out);

    let refused = ran(data.path(), workspace.path(), &["web", "--provider", "nope"]);
    assert_eq!(refused.code, Some(2), "{}", refused.err);
    assert!(refused.err.contains("model provider:"), "{}", refused.err);
    assert!(
        !refused.out.contains("http://127.0.0.1"),
        "a refused provider must not print a URL: {}",
        refused.out
    );
}

#[test]
fn a_run_records_the_configuration_it_executed_under() {
    let (data, workspace, home) = dirs_with_config();
    let user = write_config(
        &home,
        "user.toml",
        r#"
capabilities = ["fs.read"]

[profiles.coding]
tools = { allow = ["fs.read"] }
budget = { max_steps = 3 }
"#,
    );
    write_project_config(workspace.path(), "profile = \"coding\"\n");

    let r = ran_with(
        data.path(),
        workspace.path(),
        &[(ENV_USER_CONFIG, user.as_str())],
        &["run", "hello", "--events"],
    );
    assert!(r.ok, "{}", r.err);

    let events = chain(&r.out);
    let resolved = events
        .iter()
        .find(|event| event["type"] == "profile/resolved")
        .unwrap_or_else(|| panic!("the run recorded its configuration: {}", r.out));
    assert_eq!(resolved["payload"]["profile_id"], "coding");
    assert_eq!(resolved["payload"]["budget"]["max_steps"], 3);
    assert_eq!(resolved["payload"]["model_route"]["provider"], "echo");
    assert!(
        resolved["payload"]["config_hash"].as_str().expect("a hash").len() >= 16,
        "the snapshot is sealed: {}",
        resolved["payload"]
    );

    // Written before the run it describes opens: the configuration was resolved
    // before there was a run to record it on.
    let seen = types(&events);
    let resolved_at = seen.iter().position(|t| *t == "profile/resolved").expect("resolved");
    let started_at = seen.iter().position(|t| *t == "run/started").expect("started");
    assert!(resolved_at < started_at, "{seen:?}");
}

#[test]
fn a_run_records_the_tiers_the_configuration_named() {
    // The table is only worth recording if it is what the router was built
    // from; a config that names `fast` has to leave a trace a reader can find
    // without going back to the files it was resolved from.
    let (data, workspace, home) = dirs_with_config();
    let user = write_config(
        &home,
        "user.toml",
        r#"
capabilities = ["fs.read"]

[profiles.coding]
tools = { allow = ["fs.read"] }
model = { main = "big", fast = "small" }
"#,
    );
    write_project_config(workspace.path(), "profile = \"coding\"\n");

    let r = ran_with(
        data.path(),
        workspace.path(),
        &[(ENV_USER_CONFIG, user.as_str())],
        &["run", "hello", "--events"],
    );
    assert!(r.ok, "{}", r.err);

    let events = chain(&r.out);
    let resolved = events
        .iter()
        .find(|event| event["type"] == "profile/resolved")
        .unwrap_or_else(|| panic!("the run recorded its configuration: {}", r.out));
    assert_eq!(resolved["payload"]["model_route"]["model"], "big");
    let routes = &resolved["payload"]["model_routes"];
    assert_eq!(routes["fast"]["model"], "small");
    assert_eq!(routes["fast"]["tier"], "fast");
    // One provider kind per run: a tier changes the model, not the vendor.
    assert_eq!(routes["fast"]["provider"], "echo");
    // `main` is the run's own route, not a table entry.
    assert!(routes.get("main").is_none(), "{routes}");
}

#[test]
fn a_configuration_that_does_not_resolve_stops_the_run() {
    let (data, workspace, home) = dirs_with_config();
    let user = write_config(&home, "user.toml", "[profiles.only-this]\n");
    // The project names a profile the user layer does not define — a project
    // may choose, and may not invent.
    write_project_config(workspace.path(), "profile = \"missing\"\n");

    let r = ran_with(
        data.path(),
        workspace.path(),
        &[(ENV_USER_CONFIG, user.as_str())],
        &["run", "hello"],
    );
    assert!(!r.ok, "a run under a configuration nobody agreed to does not start");
    assert_eq!(r.code, Some(2), "nothing ran, so there is no answer to read: {}", r.err);
    assert!(r.err.contains("configuration"), "{}", r.err);
    assert!(r.out.is_empty(), "nothing was answered: {}", r.out);
    assert!(!data.path().join("default").exists(), "and nothing was written: {}", r.err);
}

#[test]
fn a_project_may_narrow_and_may_not_authorize_itself() {
    let (data, workspace, home) = dirs_with_config();
    let user = write_config(&home, "user.toml", "capabilities = [\"fs.read\"]\n");
    // A capability declaration is the user layer's to make. A project that
    // writes one is refused rather than ignored.
    write_project_config(workspace.path(), "capabilities = [\"fs.write\"]\n");

    let r = ran_with(
        data.path(),
        workspace.path(),
        &[(ENV_USER_CONFIG, user.as_str())],
        &["run", "hello"],
    );
    assert!(!r.ok, "{}", r.out);
    assert!(
        r.err.contains("workspace config"),
        "the refusal names the file that is wrong: {}",
        r.err
    );
    assert!(!data.path().join("default").exists(), "{}", r.err);
}

#[test]
fn a_managed_deny_overrules_the_grant_it_lands_on_and_says_so() {
    let (data, workspace, home) = dirs_with_config();
    let user = write_config(&home, "user.toml", "capabilities = [\"fs.read\", \"git.status\"]\n");
    let managed = write_config(&home, "managed.toml", "deny = [\"fs.read\"]\n");

    let r = ran_with(
        data.path(),
        workspace.path(),
        &[(ENV_USER_CONFIG, user.as_str()), (ENV_MANAGED_POLICY, managed.as_str())],
        &["run", "hello", "--events"],
    );
    assert!(r.ok, "{}", r.err);
    // Reported rather than swallowed: the grant was removed, and the person who
    // wrote it is the one who can do something about it.
    assert!(
        r.err.contains("denied by managed policy"),
        "the removal is reported on stderr: {}",
        r.err
    );

    let events = chain(&r.out);
    let resolved = events
        .iter()
        .find(|event| event["type"] == "profile/resolved")
        .unwrap_or_else(|| panic!("{}", r.out));
    let granted: Vec<&str> = resolved["payload"]["capability_set"]
        .as_array()
        .expect("a capability set")
        .iter()
        .map(|cap| cap.as_str().expect("a capability"))
        .collect();
    assert_eq!(granted, ["git.status"], "the operator's refusal is absolute");
}

#[test]
fn a_flag_before_the_plugin_id_does_not_give_its_value_as_the_id() {
    let (data, workspace) = dirs();

    // The flag comes first, the way an operator would naturally write it. The
    // command has to know that `because` belongs to `--reason`; reading only
    // non-`--` words would make it the plugin id (YA-76).
    let r = ran(
        data.path(),
        workspace.path(),
        &["plugin", "disable", "--reason", "because", "mcp.one"],
    );
    assert!(r.ok, "queuing a control is not a failure: {}", r.err);
    assert!(
        r.out.contains("plugin 'mcp.one'"),
        "the command names the plugin it queued: {}",
        r.out
    );

    let log = std::fs::read_to_string(log_file(data.path(), "default")).expect("the session log");
    let control = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a stored event"))
        .find(|event| event["event"]["event_type"] == "inbox/appended")
        .expect("the control was queued");
    assert_eq!(
        control["event"]["payload"]["payload"]["plugin_id"], "mcp.one",
        "the value of `--reason` was read as the plugin id: {control}"
    );
    assert_eq!(control["event"]["payload"]["payload"]["reason"], "because");
}

// ---------------------------------------------------------------------------
// Scheduled turns (`schedule`, `tick`)
// ---------------------------------------------------------------------------

/// The item id `schedule at` printed, for the cancel test below.
fn scheduled_id(output: &str) -> String {
    let words: Vec<&str> = output.split_whitespace().collect();
    assert_eq!(words.first(), Some(&"scheduled"), "unexpected schedule output: {output:?}");
    words.get(1).expect("an item id").to_string()
}

#[test]
fn a_scheduled_turn_runs_once_and_then_there_is_nothing_to_run() {
    let (data, workspace) = dirs();
    let data = data.path();
    let workspace = workspace.path();

    let scheduled = ran(data, workspace, &["schedule", "at", "+0s", "wake"]);
    assert!(scheduled.ok, "{}", scheduled.err);
    assert!(
        scheduled.out.contains("wake") || scheduled.out.contains("scheduled"),
        "{}",
        scheduled.out
    );

    let listed = ran(data, workspace, &["schedule", "list"]);
    assert!(listed.ok, "{}", listed.err);
    assert!(listed.out.contains("wake"), "{}", listed.out);

    // The item was due when it was written, so this tick runs it.
    let ticked = ran(data, workspace, &["tick"]);
    assert!(ticked.ok, "{}", ticked.err);
    assert_eq!(ticked.out.trim(), "echo: wake");

    // Claimed inside the terminal batch: a second tick has nothing to do and
    // says so with the exit status, not by pretending a turn ran.
    let again = ran(data, workspace, &["tick"]);
    assert_eq!(again.code, Some(2), "{}", again.out);
    assert!(again.out.contains("no turn ran"), "{}", again.out);
    let listed = ran(data, workspace, &["schedule", "list"]);
    assert_eq!(listed.out.trim(), "no scheduled items");
}

#[test]
fn a_cancelled_schedule_never_runs() {
    let (data, workspace) = dirs();
    let data = data.path();
    let workspace = workspace.path();

    let scheduled = ran(data, workspace, &["schedule", "at", "+0s", "do not wake"]);
    let id = scheduled_id(scheduled.out.trim());

    let cancelled = ran(data, workspace, &["schedule", "cancel", &id]);
    assert!(cancelled.ok, "{}", cancelled.err);
    assert!(cancelled.out.contains(&id), "{}", cancelled.out);

    let listed = ran(data, workspace, &["schedule", "list"]);
    assert_eq!(listed.out.trim(), "no scheduled items");
    assert_eq!(ran(data, workspace, &["tick"]).code, Some(2));
}

#[test]
fn a_future_schedule_is_listed_but_not_run() {
    let (data, workspace) = dirs();
    let data = data.path();
    let workspace = workspace.path();

    let scheduled = ran(data, workspace, &["schedule", "at", "+1w", "later"]);
    assert!(scheduled.ok, "{}", scheduled.err);

    assert_eq!(ran(data, workspace, &["tick"]).code, Some(2));
    let listed = ran(data, workspace, &["schedule", "list"]);
    assert!(listed.out.contains("later"), "{}", listed.out);
    assert!(!listed.out.contains("overdue"), "{}", listed.out);
}

#[test]
fn a_time_this_build_cannot_read_is_refused_before_anything_is_written() {
    let (data, workspace) = dirs();

    let r = ran(data.path(), workspace.path(), &["schedule", "at", "nonsense", "wake"]);

    assert_eq!(r.code, Some(2), "{}", r.out);
    assert!(r.err.contains("nonsense"), "{}", r.err);
    assert!(
        !log_file(data.path(), "default").exists(),
        "a refused time must not append a scheduled item"
    );
}

#[test]
fn scheduling_needs_text_and_cancelling_needs_an_id() {
    let (data, workspace) = dirs();
    let data = data.path();
    let workspace = workspace.path();

    assert_eq!(ran(data, workspace, &["schedule", "at", "+1m"]).code, Some(2));
    assert_eq!(ran(data, workspace, &["schedule", "cancel"]).code, Some(2));
    assert_eq!(ran(data, workspace, &["tick", "--events"]).code, Some(2));

    // `serve` refuses the flag before it assembles anything: with `--events` it
    // would have to hold a subscription for as long as the daemon runs, which is
    // exactly the unbounded chain `tick` already refuses.
    let served = ran(data, workspace, &["serve", "--events"]);
    assert_eq!(served.code, Some(2));
    assert!(served.err.contains("--events"), "{}", served.err);
}

#[test]
fn schedule_and_tick_speak_json_when_asked() {
    let (data, workspace) = dirs();
    let data = data.path();
    let workspace = workspace.path();

    let scheduled = ran(data, workspace, &["schedule", "at", "+0s", "wake", "--json"]);
    assert!(scheduled.ok, "{}", scheduled.err);
    let doc: serde_json::Value = serde_json::from_str(&scheduled.out).expect("one JSON object");
    assert!(doc["item_id"].is_string(), "{doc}");
    assert!(doc["due_at"].is_string(), "{doc}");
    assert_eq!(doc["text"], "wake");

    let listed = ran(data, workspace, &["schedule", "list", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&listed.out).expect("one JSON object");
    assert_eq!(doc["items"].as_array().expect("items").len(), 1, "{doc}");

    let ticked = ran(data, workspace, &["tick", "--json"]);
    assert!(ticked.ok, "{}", ticked.err);
    let doc: serde_json::Value = serde_json::from_str(&ticked.out).expect("one JSON object");
    assert_eq!(doc["ran"], true, "{doc}");
    assert_eq!(doc["final_content"], "echo: wake", "{doc}");

    let nothing = ran(data, workspace, &["tick", "--json"]);
    assert_eq!(nothing.code, Some(2));
    let doc: serde_json::Value = serde_json::from_str(&nothing.out).expect("one JSON object");
    assert_eq!(doc["ran"], false, "{doc}");
}

// ---------------------------------------------------------------------------
// memory: the disposable read model the CLI searches. It is rebuilt from the
// log, so every assertion here is really "the log said this, and the index
// found it".
// ---------------------------------------------------------------------------

#[test]
fn memory_search_finds_a_word_an_earlier_run_wrote() {
    let (data, workspace) = dirs();
    let turn = ran(
        data.path(),
        workspace.path(),
        &["run", "remember the word xylophone", "--session", "mem"],
    );
    assert!(turn.ok, "{}", turn.err);

    let found =
        ran(data.path(), workspace.path(), &["memory", "search", "xylophone", "--session", "mem"]);
    assert!(found.ok, "{}", found.err);
    assert_eq!(found.code, Some(0));
    assert!(found.out.contains("inbox/appended"), "{}", found.out);
    assert!(found.out.contains("xylophone"), "{}", found.out);
}

#[test]
fn memory_search_that_matches_nothing_is_an_answer_not_a_failure() {
    let (data, workspace) = dirs();
    let turn = ran(data.path(), workspace.path(), &["run", "hello", "--session", "mem"]);
    assert!(turn.ok, "{}", turn.err);

    let found =
        ran(data.path(), workspace.path(), &["memory", "search", "zeppelin", "--session", "mem"]);
    assert!(found.ok, "{}", found.err);
    assert_eq!(found.code, Some(0), "zero hits is still an answer");
    assert!(found.out.contains("no matches for 'zeppelin'"), "{}", found.out);
}

#[test]
fn memory_search_reports_one_json_object_with_the_documented_shape() {
    let (data, workspace) = dirs();
    let turn = ran(
        data.path(),
        workspace.path(),
        &["run", "remember the word xylophone", "--session", "mem"],
    );
    assert!(turn.ok, "{}", turn.err);

    let found = ran(
        data.path(),
        workspace.path(),
        &["memory", "search", "xylophone", "--session", "mem", "--json"],
    );
    assert!(found.ok, "{}", found.err);
    let doc: serde_json::Value = serde_json::from_str(&found.out).expect("one JSON object");
    assert_eq!(doc["session"], "mem", "{doc}");
    assert_eq!(doc["query"], "xylophone", "{doc}");
    let hits = doc["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "{doc}");
    let mut scores = Vec::new();
    for hit in hits {
        assert!(hit["seq"].is_u64(), "{hit}");
        assert!(hit["event_type"].is_string(), "{hit}");
        assert!(hit["snippet"].is_string(), "{hit}");
        assert!(hit.get("turn_id").is_some(), "{hit}");
        assert!(hit.get("step_id").is_some(), "{hit}");
        scores.push(hit["score"].as_f64().expect("a score"));
    }
    // Hits arrive in relevance order, so the score is what the array is sorted
    // by — and it is reported, not just used, so a caller can see why.
    assert!(scores.windows(2).all(|pair| pair[0] >= pair[1]), "best first: {doc}");
}

#[test]
fn memory_search_with_no_words_is_a_usage_error() {
    let (data, workspace) = dirs();
    let r = ran(data.path(), workspace.path(), &["memory", "search", "--session", "mem"]);
    assert_eq!(r.code, Some(2));
    assert!(r.err.contains("memory search <text>"), "{}", r.err);
}

#[test]
fn memory_search_refuses_events_because_it_writes_none() {
    let (data, workspace) = dirs();
    let r = ran(
        data.path(),
        workspace.path(),
        &["memory", "search", "word", "--session", "mem", "--events"],
    );
    assert_eq!(r.code, Some(2));
    assert!(r.err.contains("writes no events"), "{}", r.err);
}

#[test]
fn memory_reindex_rebuilds_the_index_and_search_still_works() {
    let (data, workspace) = dirs();
    let turn = ran(
        data.path(),
        workspace.path(),
        &["run", "remember the word xylophone", "--session", "mem"],
    );
    assert!(turn.ok, "{}", turn.err);
    let before = ran(
        data.path(),
        workspace.path(),
        &["memory", "search", "xylophone", "--session", "mem", "--json"],
    );
    let before: serde_json::Value = serde_json::from_str(&before.out).expect("one JSON object");
    let count = before["hits"].as_array().expect("hits").len();

    let rebuilt = ran(data.path(), workspace.path(), &["memory", "reindex", "--session", "mem"]);
    assert!(rebuilt.ok, "{}", rebuilt.err);
    assert_eq!(rebuilt.code, Some(0));
    assert!(
        rebuilt.out.contains(&format!("indexed {count} entries for session 'mem'")),
        "{}",
        rebuilt.out
    );

    let after = ran(
        data.path(),
        workspace.path(),
        &["memory", "search", "xylophone", "--session", "mem", "--json"],
    );
    let after: serde_json::Value = serde_json::from_str(&after.out).expect("one JSON object");
    assert_eq!(after["hits"], before["hits"], "same log, same index, same hits");
}

/// The blob the run writes and the blob `artifact get` reads are one store.
///
/// This is the wiring the unit tests cannot see: the command resolves
/// `<data_dir>/artifacts` on its own, without assembling a run, and the locator
/// in the log is enough to reach the bytes.
#[test]
fn artifact_get_fetches_a_blob_from_the_data_directory() {
    let (data, workspace) = dirs();
    let artifacts = data.path().join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("an artifacts directory");
    // "hello world" is the store's known-answer fixture: the name is the
    // SHA-256 of the content, not one this test invents.
    let hash = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    std::fs::write(artifacts.join(format!("{hash}.blob")), b"hello world").expect("a blob on disk");
    let out = data.path().join("fetched.txt");
    let locator = format!("artifact://sha256/{hash}");

    let fetched = ran(
        data.path(),
        workspace.path(),
        &["artifact", "get", &locator, "--out", out.to_str().unwrap()],
    );

    assert!(fetched.ok, "{}", fetched.err);
    assert_eq!(fetched.code, Some(0));
    assert_eq!(std::fs::read(&out).unwrap(), b"hello world");
    assert!(fetched.out.contains("wrote 11 byte(s)"), "{}", fetched.out);
}

/// A locator that names nothing is an answer (1), not a command that could not
/// run (2) — a script can tell "no such blob" from "I could not look".
#[test]
fn a_missing_artifact_exits_one_and_names_it() {
    let (data, workspace) = dirs();
    let missing = "0".repeat(64);

    let fetched = ran(data.path(), workspace.path(), &["artifact", "get", &missing]);

    assert_eq!(fetched.code, Some(1), "stdout: {} stderr: {}", fetched.out, fetched.err);
    assert!(fetched.out.contains("artifact not found"), "{}", fetched.out);
}

#[test]
fn artifact_with_no_verb_or_no_reference_is_a_usage_error() {
    let (data, workspace) = dirs();

    let no_verb = ran(data.path(), workspace.path(), &["artifact"]);
    assert_eq!(no_verb.code, Some(2));
    assert!(no_verb.err.contains("artifact get <locator|hash>"), "{}", no_verb.err);

    let no_reference = ran(data.path(), workspace.path(), &["artifact", "get"]);
    assert_eq!(no_reference.code, Some(2));
    assert!(no_reference.err.contains("artifact get <locator|hash>"), "{}", no_reference.err);
}

#[test]
fn artifact_get_writes_no_events_so_events_is_refused() {
    let (data, workspace) = dirs();
    let missing = "0".repeat(64);

    let refused = ran(data.path(), workspace.path(), &["artifact", "get", &missing, "--events"]);

    assert_eq!(refused.code, Some(2));
    assert!(refused.err.contains("does not support --events"), "{}", refused.err);
}

// ---------------------------------------------------------------------------
// context inspect: a reader over the derived context and the recorded request
// ---------------------------------------------------------------------------

/// An empty session is an answer about an empty context, not a command that
/// could not run: exit 0, and the report says there is nothing there.
#[test]
fn context_inspect_of_an_empty_session_is_an_answer() {
    let (data, workspace) = dirs();

    let r = ran(data.path(), workspace.path(), &["context", "inspect", "--session", "new"]);

    assert_eq!(r.code, Some(0), "stdout: {} stderr: {}", r.out, r.err);
    assert!(r.out.contains("messages: 0 in context"), "{}", r.out);
    assert!(r.out.contains("last request: none (no model request in this log)"), "{}", r.out);
    assert!(r.out.contains("by kind:"), "{}", r.out);
    assert!(r.out.contains("framing"), "{}", r.out);
}

/// After a turn, the inspector reports the settled context and the request the
/// log recorded — read back, not re-derived.
#[test]
fn context_inspect_after_a_turn_reports_the_last_request() {
    let (data, workspace) = dirs();
    let turn = ran(data.path(), workspace.path(), &["run", "hello", "--session", "ctx"]);
    assert!(turn.ok, "{}", turn.err);

    let r = ran(
        data.path(),
        workspace.path(),
        &["context", "inspect", "--session", "ctx", "--json", "--top", "2"],
    );

    assert_eq!(r.code, Some(0), "stdout: {} stderr: {}", r.out, r.err);
    let doc: serde_json::Value = serde_json::from_str(&r.out).expect("one JSON object");
    assert_eq!(doc["session"], "ctx", "{doc}");
    assert!(doc["context_revision"].is_u64(), "{doc}");
    assert!(doc["context"]["message_count"].as_u64().unwrap() > 0, "{doc}");
    assert_eq!(
        doc["context"]["largest_messages"].as_array().expect("a list").len(),
        2,
        "--top bounds the list: {doc}"
    );
    let last = &doc["last_request"];
    assert!(last["attempt_id"].is_string(), "a request was recorded: {doc}");
    assert!(last["tool_count"].is_u64(), "{doc}");
    // The per-turn layers are recorded on the request, not folded into
    // history: the state layer says where the turn stood, the task layer what
    // it was for.
    let state = last["context_layers"]["state"]["text"].as_str().expect("a state layer");
    assert!(state.starts_with("State at "), "{doc}");
    assert!(state.contains("Queued input(s): none."), "{doc}");
    let task = last["context_layers"]["task"]["text"].as_str().expect("a task layer");
    assert!(task.contains("Task for this turn: \"hello\""), "{doc}");
    assert!(task.contains("Input: user turn."), "{doc}");
    // The capability layer is run-scoped and just as absent from history: it
    // states the boundary the stack was assembled with and indexes the tools.
    let capability = &last["context_capability"];
    let capability_uid = capability["message_uid"].as_str().expect("a capability message");
    assert!(capability_uid.starts_with("msg/capability/"), "it is named by the run: {doc}");
    let text = capability["text"].as_str().expect("a capability layer");
    assert!(text.starts_with("Capabilities in this run:\n"), "{doc}");
    assert!(text.contains("- sandbox: local"), "{doc}");
    // The local backend runs commands as the user: the layer says the network
    // is not bounded rather than claiming a denial nothing enforces.
    assert!(
        text.contains("- network for sandboxed commands: not bounded by this backend"),
        "{doc}"
    );
    // This stack registers no tool providers, and the layer says so rather
    // than printing an empty index.
    assert!(text.contains("- no tools are available in this run."), "{doc}");
    // The two revisions answer different questions: the top-level one names
    // what the derivation just folded, and the request's names the prefix it
    // saw. The first turn of a session saw only what was queued for it — the
    // start at sequence 0 and its input at sequence 1 — so its request says 1
    // even though the log has grown since.
    assert_eq!(last["context_revision"], 1, "{doc}");

    // The request's messages are recorded in order, as references. Their text
    // is durable where each was produced; the list and the hashes are what
    // prove which messages, in which order, the model saw.
    let refs = last["messages"].as_array().expect("the ordered message refs");
    assert_eq!(
        refs.len() as u64,
        last["message_count"].as_u64().expect("the count"),
        "the record covers exactly the messages sent: {doc}"
    );
    assert!(refs[0]["message_uid"].as_str().expect("a uid").starts_with("msg/system/"), "{doc}");
    assert_eq!(refs[1]["message_uid"], capability_uid, "the capability layer's place is recorded");
    assert_eq!(refs[2]["message_uid"], last["context_layers"]["state"]["message_uid"], "{doc}");
    assert_eq!(refs[3]["message_uid"], last["context_layers"]["task"]["message_uid"], "{doc}");
    assert!(refs[refs.len() - 1]["message_uid"].as_str().expect("a uid").starts_with("msg/user/"));
    for recorded in refs {
        assert_eq!(
            recorded["hash"].as_str().expect("a digest").len(),
            16,
            "every reference is checkable: {doc}"
        );
    }
    assert_eq!(last["messages_hash"].as_str().expect("a request digest").len(), 16, "{doc}");

    // The tool definitions are frozen once for the turn, in full. This run
    // offers none, and the record says exactly that instead of omitting the
    // fact.
    let frozen = &last["tool_definitions"];
    assert!(frozen["seq"].is_u64(), "the freeze is an event in the log: {doc}");
    assert_eq!(frozen["manifest_id"], last["tool_manifest_id"], "{doc}");
    assert_eq!(frozen["tool_count"], 0, "{doc}");
    assert_eq!(frozen["entries"].as_array().expect("entries").len(), 0, "{doc}");
}

#[test]
fn context_inspect_writes_no_events_so_events_is_refused() {
    let (data, workspace) = dirs();

    let refused = ran(data.path(), workspace.path(), &["context", "inspect", "--events"]);

    assert_eq!(refused.code, Some(2));
    assert!(refused.err.contains("does not support --events"), "{}", refused.err);
}

#[test]
fn context_inspect_needs_a_positive_top() {
    let (data, workspace) = dirs();

    let r = ran(data.path(), workspace.path(), &["context", "inspect", "--top", "0"]);

    assert_eq!(r.code, Some(2));
    assert!(r.err.contains("--top needs a positive number"), "{}", r.err);
}

/// The git tools under AppContainer, where a spawned `git` cannot obtain a
/// working directory.
///
/// This is the regression that motivated reading the repository in process:
/// the same command that failed with "Unable to read current working
/// directory: Permission denied" has to answer with the status it was asked
/// for. Ignored by default because it needs the machine's one-time AppContainer
/// setup; run it with `AGENT_APPCONTAINER_TEST=1`.
#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn git_status_answers_under_app_container() {
    if std::env::var("AGENT_APPCONTAINER_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let (data, workspace) = dirs();

    // The repository is created by the host; the tool under test is not
    // allowed to shell out to git for this.
    let init = Command::new("git").current_dir(workspace.path()).args(["init", "-q", "."]).status();
    match init {
        Ok(status) if status.success() => {}
        _ => {
            eprintln!("skipped: git is not on PATH");
            return;
        }
    }
    std::fs::write(workspace.path().join("tracked.txt"), "one\n").unwrap();

    let r = ran_with(
        data.path(),
        workspace.path(),
        &[("AGENT_SANDBOX", "app-container")],
        &["tool", "git.status", "{}"],
    );

    assert_eq!(r.code, Some(0), "{}", r.err);
    assert!(r.out.contains("tracked.txt"), "the entry is missing: {}", r.out);
    assert!(r.out.contains("untracked"), "the state is missing: {}", r.out);
}

/// The file tools under AppContainer: the worker inside the container answers
/// `fs.write` / `fs.read` / `fs.list`, and the local and container backends
/// answer the same way for the same calls.
///
/// The comparison is the point. A worker that cannot resolve paths (the first
/// version could not: a container may not ask the kernel for a final path)
/// shows up here as a refusal the local backend does not make.
#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn the_file_tools_answer_under_app_container() {
    if std::env::var("AGENT_APPCONTAINER_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let (data, workspace) = dirs();
    std::fs::create_dir_all(workspace.path().join("notes")).expect("a directory to write into");

    let container = [("AGENT_SANDBOX", "app-container")];
    let write_args = serde_json::json!({ "path": "notes/a.txt", "content": "hello" }).to_string();
    let read_args = serde_json::json!({ "path": "notes/a.txt" }).to_string();
    let list_args = serde_json::json!({ "path": "notes" }).to_string();
    let absent_args = serde_json::json!({ "path": "notes/absent.txt" }).to_string();
    let escape_args = serde_json::json!({ "path": "../outside.txt" }).to_string();
    let calls: [(&str, &str); 5] = [
        ("fs.write", &write_args),
        ("fs.read", &read_args),
        ("fs.list", &list_args),
        ("fs.read", &absent_args),
        ("fs.read", &escape_args),
    ];

    let container_ran: Vec<Ran> = calls
        .iter()
        .map(|(tool, args)| {
            ran_with(data.path(), workspace.path(), &container, &["tool", tool, args])
        })
        .collect();

    assert_eq!(container_ran[0].code, Some(0), "{}", container_ran[0].err);
    assert_eq!(container_ran[0].out.trim(), "wrote notes/a.txt");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes/a.txt")).expect("the file is written"),
        "hello"
    );
    assert_eq!(container_ran[1].out.trim(), "hello", "{}", container_ran[1].err);
    assert!(container_ran[2].out.contains("a.txt"), "{}", container_ran[2].out);
    assert_eq!(container_ran[3].code, Some(1), "{}", container_ran[3].err);
    assert_eq!(container_ran[4].code, Some(2), "{}", container_ran[4].err);

    // The same calls on the local backend: same answers, same exits.
    for (index, (tool, args)) in calls.iter().enumerate() {
        let local = ran(data.path(), workspace.path(), &["tool", tool, args]);
        assert_eq!(local.code, container_ran[index].code, "{tool}: {}", local.err);
        assert_eq!(local.out, container_ran[index].out, "{tool}");
    }
}

/// `session new` starts a session, and the start is the log's first event.
///
/// The chain is read through `--events`, which also checks where the two
/// outputs go: the event on stdout, the sentence on stderr.
#[test]
fn session_new_records_the_start_first() {
    let (data, workspace) = dirs();
    let created =
        ran(data.path(), workspace.path(), &["session", "new", "--id", "cli-one", "--events"]);
    assert_eq!(created.code, Some(0), "{}", created.err);
    assert_eq!(types(&chain(&created.out)), ["session/started"]);
    assert!(created.err.contains("session cli-one created"), "{}", created.err);

    // On disk too: the first event is the start, not a byproduct of one.
    let log = std::fs::read_to_string(log_file(data.path(), "cli-one")).expect("the log");
    let first: serde_json::Value =
        serde_json::from_str(log.lines().next().expect("an event")).expect("JSON");
    assert_eq!(first["event"]["event_type"], "session/started");

    // Asking again is not an error and does not write a second start.
    let again =
        ran(data.path(), workspace.path(), &["session", "new", "--id", "cli-one", "--json"]);
    assert_eq!(again.code, Some(0), "{}", again.err);
    let answer: serde_json::Value = serde_json::from_str(&again.out).expect("JSON");
    assert_eq!(answer["created"], false);
    let log = std::fs::read_to_string(log_file(data.path(), "cli-one")).expect("the log");
    assert_eq!(log.lines().filter(|line| line.contains("\"session/started\"")).count(), 1);
}

/// `session list` reads the directory the logs are in, so both clients agree.
#[test]
fn session_list_reads_the_data_directory() {
    let (data, workspace) = dirs();
    for id in ["list-a", "list-b"] {
        let created = ran(data.path(), workspace.path(), &["session", "new", "--id", id]);
        assert_eq!(created.code, Some(0), "{}", created.err);
    }

    let listed = ran(data.path(), workspace.path(), &["session", "list", "--json"]);
    assert_eq!(listed.code, Some(0), "{}", listed.err);
    let answer: serde_json::Value = serde_json::from_str(&listed.out).expect("JSON");
    let ids: Vec<&str> = answer["sessions"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|row| row["session_id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids.len(), 2, "{}", listed.out);
    assert!(ids.contains(&"list-a") && ids.contains(&"list-b"), "{ids:?}");

    // An empty data directory lists nothing rather than failing.
    let (empty, empty_workspace) = dirs();
    let none = ran(empty.path(), empty_workspace.path(), &["session", "list"]);
    assert_eq!(none.code, Some(0), "{}", none.err);
    assert_eq!(none.out.trim(), "no sessions");
}

/// Archiving hides a session without removing it; deleting removes it.
#[test]
fn a_session_is_hidden_before_it_is_deleted() {
    let (data, workspace) = dirs();
    let created = ran(data.path(), workspace.path(), &["session", "new", "--id", "tidy"]);
    assert_eq!(created.code, Some(0), "{}", created.err);

    let archived = ran(data.path(), workspace.path(), &["session", "archive", "tidy", "--json"]);
    assert_eq!(archived.code, Some(0), "{}", archived.err);
    let answer: serde_json::Value = serde_json::from_str(&archived.out).expect("JSON");
    assert_eq!(answer["archived"], true);
    let log = std::fs::read_to_string(log_file(data.path(), "tidy")).expect("the log stays");
    assert!(log.contains("session/archived"));

    let unarchived = ran(data.path(), workspace.path(), &["session", "unarchive", "tidy"]);
    assert_eq!(unarchived.code, Some(0), "{}", unarchived.err);

    // A session that is not there cannot be archived, and the refusal does not
    // bring one into being.
    let ghost = ran(data.path(), workspace.path(), &["session", "archive", "ghost"]);
    assert_eq!(ghost.code, Some(2), "{}", ghost.err);
    assert!(!data.path().join("ghost").exists(), "a refused archive creates no session");

    let deleted = ran(data.path(), workspace.path(), &["session", "delete", "tidy"]);
    assert_eq!(deleted.code, Some(0), "{}", deleted.err);
    assert!(!log_file(data.path(), "tidy").exists(), "the log is gone");

    // Deleting twice is a success twice: the first request was for it to be
    // gone, and it is.
    let twice = ran(data.path(), workspace.path(), &["session", "delete", "tidy"]);
    assert_eq!(twice.code, Some(0), "{}", twice.err);
    assert!(twice.out.contains("did not exist"), "{}", twice.out);
}

/// A turn in another process refuses the delete, and says whose turn it is.
///
/// The claim is held here, on this side of the process boundary, so the refusal
/// comes from the store's own file lock rather than from anything the two
/// callers share in memory.
#[test]
fn deleting_a_session_another_process_is_driving_is_refused() {
    use agent_ports::event_store::EventStore;
    use agent_protocol::ids::SessionId;

    let (data, workspace) = dirs();
    let created = ran(data.path(), workspace.path(), &["session", "new", "--id", "held"]);
    assert_eq!(created.code, Some(0), "{}", created.err);

    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let store = agent_store_jsonl::JsonlEventStore::new(data.path());
    let held = runtime
        .block_on(store.driver_lease(&SessionId::from_string("held")))
        .expect("this process takes the driver claim");

    let refused = ran(data.path(), workspace.path(), &["session", "delete", "held"]);
    assert_eq!(refused.code, Some(2), "{} {}", refused.err, refused.out);
    assert!(refused.out.contains("already being driven"), "{}", refused.out);
    assert!(
        refused.out.contains(&format!("pid {}", std::process::id())),
        "the refusal names the holder: {}",
        refused.out
    );
    assert!(log_file(data.path(), "held").exists(), "a refused delete removes nothing");

    // Released, the same delete goes through — the refusal was the claim, not
    // the session.
    drop(held);
    let deleted = ran(data.path(), workspace.path(), &["session", "delete", "held"]);
    assert_eq!(deleted.code, Some(0), "{}", deleted.err);
    assert!(!log_file(data.path(), "held").exists());
}
