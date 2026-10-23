//! `subagent.run`, end to end: a real child process, forked from a real parent
//! session, recorded in the parent's log by the assembled binary.
//!
//! The port, the driver and the stdio backend each have their own tests. What
//! only this file can check is the assembly — that a provider named in a
//! configuration file becomes a tool a caller can call, that the call leaves the
//! three durable things it is supposed to leave (a spawn and a settlement in the
//! parent's log, and a session of its own for the child), and that each way of
//! asking for more than the run has is a refusal rather than a run that half
//! happened.
//!
//! The child is a real process. A test that faked the process boundary would be
//! testing the part of this wiring that is hardest to get wrong.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// The configuration variables the binary documents. Named here rather than
/// imported, as in the sibling suites: an integration test cannot see the
/// binary's modules, and these are a deployment contract.
const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// A fixture binary, an `examples/` target of the backend's own package.
///
/// It is an example rather than a `src/bin/` binary so that `cargo build
/// --workspace` does not put a test double where product binaries go; it is
/// `cargo test` that builds examples, and that is how the gates run. A run of
/// this package alone does not, so it is built here — a test that quietly
/// skipped itself when a fixture was missing would report success for a wiring
/// it never exercised.
fn fixture_bin(name: &str) -> PathBuf {
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
        .args(["build", "-p", "agent-subagent-stdio", "--example", name])
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

fn child_bin() -> PathBuf {
    fixture_bin("fake-subagent")
}

/// What one run of the binary produced.
struct Ran {
    ok: bool,
    out: String,
    err: String,
}

fn dirs() -> (TempDir, TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap())
}

/// One run, with a user configuration and nothing else in the environment.
///
/// The configuration variables are cleared first for the reason the sibling
/// suites give: a developer with a user configuration exported would have every
/// run here quietly execute under it.
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

/// A user configuration with one subagent provider, and whatever profile a test
/// wants composed on top of it.
///
/// `subagent.spawn` is the capability the provider declares, and the honest one:
/// delegating starts a program. It is also the capability this build's sandbox
/// advertises and its default rules do not grant, so the two halves of the
/// question — may this run delegate at all, and may *this call* — are asked
/// separately, and the profile is where a test answers the second one.
fn config(home: &TempDir, user_caps: &str, requires: &str, profile: &str) -> PathBuf {
    let path = home.path().join("user.toml");
    std::fs::write(
        &path,
        format!(
            "capabilities = [{user_caps}]\n\
             plugins = [\"subagent.fake@1.0.0\"]\n\n\
             [subagents.fake]\n\
             command = '{command}'\n\
             args = [\"result\"]\n\
             requires = [{requires}]\n\n\
             [profiles.p]\n{profile}",
            command = child_bin().display(),
        ),
    )
    .expect("a user configuration");
    path
}

/// The profile body that both trusts and enables the provider, and lets a call
/// through the policy — what a run that is meant to delegate looks like.
const DELEGATING: &str = "tools = { allow = [\"subagent.spawn\"] }\n\
                          plugins = { enable = [\"subagent.fake\"] }\n";

/// The same configuration, with a working directory written into the provider's
/// section — the one line the boundary test is about.
fn config_with_cwd(home: &TempDir, cwd: &str) -> PathBuf {
    let path = home.path().join("user.toml");
    std::fs::write(
        &path,
        format!(
            "capabilities = [\"subagent.spawn\"]\n\
             plugins = [\"subagent.fake@1.0.0\"]\n\n\
             [subagents.fake]\n\
             command = '{command}'\n\
             args = [\"result\"]\n\
             cwd = '{cwd}'\n\
             requires = [\"subagent.spawn\"]\n\n\
             [profiles.p]\n{DELEGATING}",
            command = child_bin().display(),
        ),
    )
    .expect("a user configuration");
    path
}

/// Every event the session's log holds, in order, as the store wrote them.
///
/// The stored shape nests the event under `event`; tests read the event itself,
/// because that is the thing the design talks about.
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

/// The one event of this type, which every test here expects exactly one of.
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
        .filter(|name| name != "default")
        .collect();
    names.sort();
    names
}

/// A parent that has finished a turn, which is the only kind of parent a fork
/// will copy a child's history from.
fn a_parent_with_a_closed_turn(data: &Path, workspace: &Path, config: &Path) {
    let run = ran(data, workspace, config, &["--profile", "p", "run", "hello"]);
    assert!(run.ok, "{}", run.err);
    assert!(
        types(&events(data, "default")).iter().any(|t| t == "turn/completed"),
        "the parent has no closed turn, so nothing below could fork from it"
    );
}

#[test]
fn a_delegation_forks_runs_and_records_a_child() {
    let (data, workspace, home) = dirs();
    let config = config(&home, r#""subagent.spawn""#, r#""subagent.spawn""#, DELEGATING);
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config);

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"fake","objective":"summarize the notes",
                "output_schema":{"type":"object"}}"#,
        ],
    );
    assert!(r.ok, "{}", r.err);

    // What the caller sees: the child's own answer, and the child named by the
    // session its history lives in.
    assert!(r.out.contains("completed: summarize the notes"), "{}", r.out);
    assert!(r.out.contains("subagent "), "the child is named in the result: {}", r.out);
    // The fixture echoes the request it received, which is the only place the
    // parent's side of the wire can be checked: the schema reached the child
    // rather than being dropped on the way.
    assert!(r.out.contains(r#""objective":"summarize the notes""#), "{}", r.out);
    assert!(r.out.contains(r#""output_schema":{"type":"object"}"#), "{}", r.out);

    let log = events(data.path(), "default");
    let spawned = one(&log, "subagent/spawned");
    let completed = one(&log, "subagent/completed");
    let child =
        spawned["payload"]["child_session_id"].as_str().expect("a child session").to_string();

    assert_eq!(spawned["payload"]["provider"], "fake");
    assert_eq!(spawned["actor"], "agent", "asking for a child is the parent's own act");
    assert_eq!(
        spawned["payload"]["request"]["objective"], "summarize the notes",
        "the log holds the request that was made, not a paraphrase of it"
    );
    assert_eq!(completed["actor"], "system", "an ending arrives from outside the parent's run");
    assert_eq!(completed["payload"]["child_session_id"], child);
    assert_eq!(completed["payload"]["outcome"]["status"], "completed");
    assert_eq!(
        spawned["payload"]["fork_at_turn_id"], completed["payload"]["parent_turn_id"],
        "the child is attributed to the turn whose history it inherited"
    );

    // The child is a session, and its log opens by saying where it came from and
    // what it was asked to do — the fork's reason *is* the objective.
    let child_log = events(data.path(), &child);
    assert_eq!(child_log[0]["event_type"], "session/forked", "{:?}", types(&child_log));
    assert_eq!(child_log[0]["payload"]["fork_reason"], "summarize the notes");
    assert_eq!(child_log[0]["payload"]["parent_session_id"], "default");
    assert!(
        child_log[0]["payload"]["inherited_event_count"].as_u64().unwrap_or(0) > 0,
        "a child inherits its parent's history, which is why a fork is how one is made"
    );

    // Nothing was queued for a parent that was not waiting: this build's caller
    // has the outcome in hand, and a copy in the Inbox would be pending work
    // nobody is going to do.
    let queued: Vec<&Value> = log
        .iter()
        .filter(|event| {
            event["event_type"] == "inbox/appended" && event["payload"]["kind"] == "external_result"
        })
        .collect();
    assert!(queued.is_empty(), "the outcome was queued as well as returned: {queued:?}");
}

#[test]
fn a_delegation_the_policy_does_not_allow_never_starts_a_child() {
    let (data, workspace, home) = dirs();
    // Authorized and enabled — so the provider is resolved and the tool is on
    // the surface — and no rule says a call may run. The default policy
    // advertises `subagent.spawn` and grants nothing, which is what makes
    // delegating something a profile opts into.
    let config = config(
        &home,
        r#""subagent.spawn""#,
        r#""subagent.spawn""#,
        "plugins = { enable = [\"subagent.fake\"] }\n",
    );
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config);

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "subagent.run", r#"{"provider":"fake","objective":"x"}"#],
    );

    assert!(!r.ok, "a call policy denied must not report success: {}", r.out);
    // The remedy for this one is to add a rule, which is a different edit from
    // the one the sandbox and managed-policy layers would ask for — so the
    // refusal says which of the four it is.
    assert!(r.err.contains("is not granted by any policy rule"), "{}", r.err);
    assert!(r.err.contains("subagent.spawn"), "{}", r.err);

    assert!(children(data.path()).is_empty(), "a denied call forked a child");
    assert!(
        !types(&events(data.path(), "default")).iter().any(|t| t == "subagent/spawned"),
        "a call that never ran was recorded as a delegation"
    );
}

#[test]
fn a_provider_the_run_is_not_authorized_for_refuses_the_run() {
    let (data, workspace, home) = dirs();
    // The user authorizes nothing, and the profile turns the provider on anyway.
    let config =
        config(&home, "", r#""subagent.spawn""#, "plugins = { enable = [\"subagent.fake\"] }\n");

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "status"]);

    assert!(!r.ok, "the run was never authorized to begin: {}", r.out);
    assert!(r.err.contains("not authorized"), "{}", r.err);
    // A refusal at resolution time is a decision not to run, not a run that
    // failed halfway: no store was opened and nothing was forked.
    assert!(!data.path().join("default").exists(), "no session log was opened");
}

#[test]
fn a_provider_the_profile_does_not_enable_is_not_on_the_surface() {
    let (data, workspace, home) = dirs();
    // Trusted and defined, and the profile does not turn it on: a definition is
    // how to reach a provider, and enabling it is whether this run does.
    let config = config(&home, r#""subagent.spawn""#, r#""subagent.spawn""#, "");

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "subagent.run", r#"{"provider":"fake","objective":"x"}"#],
    );

    assert!(!r.ok);
    assert!(r.err.contains("subagent.run"), "{}", r.err);
    assert!(children(data.path()).is_empty(), "nothing ran, so nothing was forked");
}

#[test]
fn a_request_the_backend_cannot_honour_never_forks_a_child() {
    let (data, workspace, home) = dirs();
    let config = config(&home, r#""subagent.spawn""#, r#""subagent.spawn""#, DELEGATING);
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config);

    // A budget is enforced by the loop that spends it, and an external process
    // runs its own loop. The backend says so instead of passing the bound down a
    // wire where nothing holds it.
    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"fake","objective":"x","budget":{"max_steps":3}}"#,
        ],
    );

    assert!(!r.ok, "{}", r.out);
    assert!(r.err.contains("cannot honour"), "{}", r.err);
    assert!(r.err.contains("budget"), "{}", r.err);

    // The negotiation happens before the fork, which is the whole claim that a
    // refusal costs nothing: no child session, and no delegation in the log.
    assert!(children(data.path()).is_empty(), "a refused request forked a child");
    assert!(
        !types(&events(data.path(), "default")).iter().any(|t| t == "subagent/spawned"),
        "a delegation that never started was recorded"
    );
}

#[test]
fn a_provider_whose_working_directory_is_outside_the_workspace_is_not_on_the_surface() {
    // A configured `cwd` is a path in a configuration file, and those are held
    // to the workspace boundary like any other. A provider the boundary refuses
    // is skipped with a note rather than started anywhere.
    let (data, workspace, home) = dirs();
    let elsewhere = TempDir::new().unwrap();
    let config = config_with_cwd(&home, &elsewhere.path().display().to_string());

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "run", "hello"]);

    assert!(r.ok, "a misconfigured provider is not fatal: {}", r.err);
    assert!(r.err.contains("subagent provider 'fake' is not started"), "{}", r.err);
    assert!(r.err.contains("path outside workspace"), "{}", r.err);

    // With every backend refused there is no delegation tool at all: a tool a
    // model could see but never use is worse than one that is not there.
    let absent = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "subagent.run", r#"{"provider":"fake","objective":"x"}"#],
    );
    assert!(!absent.ok, "{}", absent.out);
    assert!(absent.err.contains("subagent.run"), "{}", absent.err);
    assert!(children(data.path()).is_empty(), "a refused provider still ran");
}

#[test]
fn a_backend_this_run_does_not_have_is_refused_by_name() {
    let (data, workspace, home) = dirs();
    let config = config(&home, r#""subagent.spawn""#, r#""subagent.spawn""#, DELEGATING);
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config);

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "subagent.run", r#"{"provider":"ghost","objective":"x"}"#],
    );

    assert!(!r.ok, "{}", r.out);
    assert!(r.err.contains("'ghost' is not a backend this run has"), "{}", r.err);
    // And the refusal says what the run does have, because a caller that named
    // the wrong provider can only act on that if it is told the right one.
    assert!(r.err.contains("fake"), "{}", r.err);
    assert!(children(data.path()).is_empty(), "nothing ran, so nothing was forked");
}

#[test]
fn an_argument_the_request_does_not_define_is_refused() {
    let (data, workspace, home) = dirs();
    let config = config(&home, r#""subagent.spawn""#, r#""subagent.spawn""#, DELEGATING);
    a_parent_with_a_closed_turn(data.path(), workspace.path(), &config);

    // `timeout` is the wire's word for what the ceiling is written as in a
    // configuration file. Accepting it silently would leave a caller believing
    // it had set a deadline that nothing read.
    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &[
            "--profile",
            "p",
            "tool",
            "subagent.run",
            r#"{"provider":"fake","objective":"x","timeout":"5s"}"#,
        ],
    );

    assert!(!r.ok, "{}", r.out);
    assert!(r.err.contains("not a subagent request"), "{}", r.err);
    assert!(r.err.contains("timeout_ms"), "the refusal names the field it wanted: {}", r.err);
    assert!(children(data.path()).is_empty(), "nothing ran, so nothing was forked");
}
