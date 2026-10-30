//! `subagent.run` with an `in-process` backend, end to end.
//!
//! The stdio backend has its own suite (`subagent_wiring`) because a program on
//! the other side of a wire is a different risk from a turn in this process. What
//! only this file can check is the claim the in-process backend exists for: the
//! child's history is the session the delegation forked, so the objective is
//! answered **in that session**, by this build's own loop, with nothing started
//! outside this process — and a run whose sandbox cannot start a process at all
//! can still delegate one (CONTRACT §7 #133).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";
const ENV_SANDBOX: &str = "AGENT_SANDBOX";

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
        .env_remove(ENV_SANDBOX);
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

/// The profile body that trusts and enables the provider, and lets a call
/// through the policy.
const DELEGATING: &str = "tools = { allow = [\"subagent.spawn\"] }\n\
                          plugins = { enable = [\"subagent.inner\"] }\n";

/// A user configuration with one in-process provider.
///
/// No `command`: an in-process backend names no program, and resolution refuses
/// one that does — which is the shape check the sibling suite cannot exercise.
fn config(home: &TempDir, profile: &str) -> PathBuf {
    let path = home.path().join("user.toml");
    std::fs::write(
        &path,
        format!(
            "capabilities = [\"subagent.spawn\"]\n\
             plugins = [\"subagent.inner@1.0.0\"]\n\n\
             [subagents.inner]\n\
             kind = \"in-process\"\n\
             requires = [\"subagent.spawn\"]\n\n\
             [profiles.p]\n{profile}"
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

#[test]
fn a_child_turn_runs_in_the_forked_session_and_answers_its_objective() {
    let (data, workspace, home) = dirs();
    let config = config(&home, DELEGATING);
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
            r#"{"provider":"inner","objective":"look at the notes"}"#,
        ],
    );
    assert!(r.ok, "{}", r.err);
    assert!(r.out.contains("completed: echo: look at the notes"), "{}", r.out);

    let log = events(data.path(), "default");
    let spawned = one(&log, "subagent/spawned");
    let completed = one(&log, "subagent/completed");
    let child = spawned["payload"]["child_session_id"].as_str().expect("a child session");
    assert_eq!(spawned["payload"]["provider"], "inner");
    assert_eq!(completed["payload"]["outcome"]["status"], "completed");
    assert_eq!(completed["payload"]["child_session_id"], child);

    // The child is a session, and it opens by saying what it was forked for.
    let child_log = events(data.path(), child);
    assert_eq!(child_log[0]["event_type"], "session/forked", "{:?}", types(&child_log));
    assert_eq!(child_log[0]["payload"]["fork_reason"], "look at the notes");
    assert_eq!(child_log[0]["payload"]["parent_session_id"], "default");
    assert!(
        child_log[0]["payload"]["inherited_event_count"].as_u64().unwrap_or(0) > 0,
        "a child inherits its parent's history"
    );

    // The point of an in-process child: its turn is in *that* session's log,
    // written by this build's own loop — not an answer that arrived over a wire
    // and a session that holds only a header.
    assert!(
        types(&child_log).iter().any(|t| t == "turn/completed"),
        "the child's session has no finished turn of its own: {:?}",
        types(&child_log)
    );
}

/// A run that cannot start a process can still delegate — to the one backend
/// that is a turn in this process. This used to be impossible: every backend was
/// a child process, so a read-only run had no delegation at all.
#[test]
fn a_run_that_can_start_no_process_still_delegates_in_process() {
    let (data, workspace, home) = dirs();
    let config = config(&home, DELEGATING);
    let read_only: &[(&str, &str)] = &[(ENV_SANDBOX, "read-only")];
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config, read_only);

    let r = ran_with(
        data.path(),
        workspace.path(),
        &config,
        read_only,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"inner","objective":"look at the notes"}"#,
        ],
    );

    assert!(r.ok, "{}", r.err);
    assert!(r.out.contains("completed: echo: look at the notes"), "{}", r.out);
    assert_eq!(children(data.path()).len(), 1, "the child is a session of its own");
}

/// A stdio provider in the same configuration is refused in that run, and the
/// refusal is said out loud rather than turning into a call-time failure.
#[test]
fn a_stdio_provider_is_not_admitted_by_a_run_that_can_start_no_process() {
    let (data, workspace, home) = dirs();
    let path = home.path().join("user.toml");
    std::fs::write(
        &path,
        "capabilities = [\"subagent.spawn\"]\n\
         plugins = [\"subagent.outer@1.0.0\", \"subagent.inner@1.0.0\"]\n\n\
         [subagents.outer]\n\
         command = 'nothing-here'\n\
         requires = [\"subagent.spawn\"]\n\n\
         [subagents.inner]\n\
         kind = \"in-process\"\n\
         requires = [\"subagent.spawn\"]\n\n\
         [profiles.p]\n\
         tools = { allow = [\"subagent.spawn\"] }\n\
         plugins = { enable = [\"subagent.outer\", \"subagent.inner\"] }\n",
    )
    .expect("a user configuration");
    let config = &path;
    let read_only: &[(&str, &str)] = &[(ENV_SANDBOX, "read-only")];
    a_parent_with_a_closed_turn(data.path(), workspace.path(), config, read_only);

    let r = ran_with(
        data.path(),
        workspace.path(),
        config,
        read_only,
        &["--profile", "p", "tool", "subagent.run", r#"{"provider":"outer","objective":"x"}"#],
    );

    assert!(!r.ok, "a backend the sandbox cannot supply ran anyway: {}", r.out);
    assert!(r.err.contains("process.spawn"), "the refusal says why: {}", r.err);
    assert_eq!(children(data.path()).len(), 0, "a refused backend forked a child");
}

/// What an in-process backend cannot honour is refused at negotiation, before a
/// child's history exists — a schema dropped silently would hand back an answer
/// that looks conformant and is not.
#[test]
fn a_schema_an_in_process_child_cannot_honour_never_forks_one() {
    let (data, workspace, home) = dirs();
    let config = config(&home, DELEGATING);
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
            r#"{"provider":"inner","objective":"x","output_schema":{"type":"object"}}"#,
        ],
    );

    assert!(!r.ok, "a schema this backend cannot honour was accepted: {}", r.out);
    assert!(r.err.contains("output_schema"), "the refusal names it: {}", r.err);
    assert_eq!(children(data.path()).len(), 0, "a refused request forked a child");
    assert!(
        !types(&events(data.path(), "default")).iter().any(|t| t == "subagent/spawned"),
        "a request that never ran was recorded as a delegation"
    );
}

/// A child's answer too large to sit in the parent's context comes back the way
/// every large tool output does — a preview naming an `artifact://` locator —
/// and the artifact reads back whole.
///
/// This backend has no answer channel of its own, and does not need one: the
/// child was never asked for a schema (`output_schema` is refused at
/// negotiation), so `Completed.result` is `None` by construction and the words
/// are the `summary`, which the gateway bounds like any other result. Pinned
/// here because it is the property that makes a long answer survivable, and it
/// holds one layer above the backend rather than in it.
#[test]
fn a_long_answer_reaches_the_parent_as_a_locator_that_reads_back_whole() {
    let (data, workspace, home) = dirs();
    let config = config(&home, DELEGATING);
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config, &[]);

    // Past any configured tool-output limit, and past what one step of a
    // parent's context could hold.
    // 8 KiB is the floor a run's tool-output limit is clamped to, and this is
    // past it while still fitting a Windows command line.
    let objective = "the notes say the sentinel is RHUBARB-7731.\n".repeat(260);
    let request = serde_json::json!({ "provider": "inner", "objective": objective }).to_string();

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "subagent.run", request.as_str()],
    );
    assert!(r.ok, "{}", r.err);
    assert!(r.out.contains("completed: echo: the notes say"), "{}", r.out);
    assert!(
        !r.out.contains(&objective),
        "the child's whole answer went inline instead of being offloaded"
    );

    let locator = r
        .out
        .split("artifact://sha256/")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no artifact locator in {}", r.out));
    let hash: String = locator.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    assert_eq!(hash.len(), 64, "a locator names a sha-256: {locator}");

    let blob = data.path().join("artifacts").join(format!("{hash}.blob"));
    let whole = std::fs::read_to_string(&blob)
        .unwrap_or_else(|e| panic!("no artifact at {}: {e}", blob.display()));
    assert!(
        whole.ends_with(&objective),
        "the artifact does not hold the child's answer: {} bytes",
        whole.len()
    );

    // The settlement records the same answer, so a reader of the parent's log
    // does not depend on the artifact surviving either.
    let log = events(data.path(), "default");
    let summary = one(&log, "subagent/completed")["payload"]["outcome"]["summary"]
        .as_str()
        .expect("the settlement carries the child's words")
        .to_string();
    assert!(summary.ends_with(&objective), "{} bytes", summary.len());
}
