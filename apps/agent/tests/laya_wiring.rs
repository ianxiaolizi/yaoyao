//! The Laya sidecar's wiring, end to end: a real sidecar process, the assembled
//! binary, and a decision trace in the session log that a later reader can
//! check for itself.
//!
//! The adapter's own tests check the wire and the mapping, and the broker's
//! check the selection rule. What only this file can check is the assembly: that
//! a sidecar named in a configuration file is attached, asked, and **recorded**
//! — and that a turn whose sidecar is down, slow, or turned off degrades to the
//! rule baseline with a trace that says which of those happened. Those four
//! outcomes are what a reader of the history has to be able to tell apart, and
//! the log is where they are told apart.
//!
//! The fake sidecar is a real child process on a real socket, for the reason the
//! MCP fixture is: the part most likely to be wrong is the part that crosses a
//! process boundary, and an in-process stand-in would not exercise it.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use agent_ports::decision::DecisionTrace;
use serde_json::Value;
use tempfile::TempDir;

/// The configuration variables the binary documents: the name an operator's
/// script writes, stated here rather than read off the product, so that a
/// rename on both sides cannot pass unnoticed.
const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// The sidecar fixture, an `examples/` target of the adapter's own package.
///
/// It is an example rather than a `src/bin/` binary so that `cargo build
/// --workspace` does not put a test double where product binaries go; it is
/// `cargo test` that builds examples, and that is how the gates run. A run of
/// this package alone does not, so it is built here — a test that skipped itself
/// when a fixture was missing would report success for a wiring it never
/// exercised.
fn sidecar_bin() -> PathBuf {
    let agent = PathBuf::from(env!("CARGO_BIN_EXE_agent"));
    let directory = agent.parent().expect("the binary is in a directory").join("examples");
    let path =
        directory.join(if cfg!(windows) { "fake-laya-server.exe" } else { "fake-laya-server" });
    if path.exists() {
        return path;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // Build into the same target directory this test binary lives under: a
    // coverage build relocates it, and a default build would not be found here.
    let profile = directory.parent().expect("the profile directory");
    let mut build = Command::new(cargo);
    build
        .args(["build", "-p", "agent-decision-laya", "--example", "fake-laya-server"])
        .arg("--target-dir")
        .arg(profile.parent().expect("the target directory"))
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.ends_with("release") {
        build.arg("--release");
    }
    let built = build.status().expect("cargo is on the path");
    assert!(built.success(), "the fake sidecar fixture could not be built");
    assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    path
}

/// A running fake sidecar, killed when this value is dropped.
struct Sidecar {
    child: Child,
    /// The endpoint to configure: the bound address, plus the behaviour the
    /// fake selects from the query string.
    url: String,
}

impl Sidecar {
    /// Start one on a free port. `mode` is one of the fixture's behaviours
    /// (`slow`, `abstain`, ...); `None` answers well-formed.
    fn start(mode: Option<&str>) -> Self {
        let mut child = Command::new(sidecar_bin())
            .args(["--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the fake sidecar starts");

        let stdout = child.stdout.take().expect("stdout was piped");
        let line = BufReader::new(stdout)
            .lines()
            .next()
            .expect("the fixture announces its address")
            .expect("stdout is UTF-8");
        let address = line.strip_prefix("listening ").unwrap_or_else(|| {
            panic!("the fixture announced something unexpected: {line}");
        });

        let base = format!("http://{address}");
        let url = match mode {
            Some(mode) => format!("{base}/?mode={mode}"),
            None => base,
        };
        Self { child, url }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What one run of the binary produced.
struct Ran {
    ok: bool,
    err: String,
}

fn dirs() -> (TempDir, TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap())
}

/// One `run` of the binary under a user configuration, and nothing else.
///
/// The configuration variables are cleared for the reason the sibling suites
/// give: a developer with a user configuration exported would have every run
/// here quietly execute under it.
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
        err: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

fn write_config(home: &TempDir, text: &str) -> PathBuf {
    let path = home.path().join("user.toml");
    std::fs::write(&path, text).expect("a user configuration");
    path
}

/// A user configuration defining the sidecar at `url`, optionally with a
/// profile composed on top of it.
fn config_with_sidecar(home: &TempDir, url: &str, profile: &str) -> PathBuf {
    write_config(
        home,
        &format!(
            "[decisions.laya]\nurl = \"{url}\"\nmodel = \"multilingual\"\n\n[profiles.p]\n{profile}"
        ),
    )
}

/// The decision trace the run recorded, as the argument to the turn's event.
fn trace_of(data: &Path) -> DecisionTrace {
    let path = data.join("default").join("events.jsonl");
    let log = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("the session log at {} could not be read: {e}", path.display()));

    let mut found = None;
    for line in log.lines() {
        let record: Value = serde_json::from_str(line).expect("every line is a stored event");
        if record["event"]["event_type"] == "decision/trace" {
            let trace = &record["event"]["payload"]["trace"];
            found = Some(
                serde_json::from_value(trace.clone())
                    .expect("the event carries the trace this build writes"),
            );
        }
    }
    found.expect("the run recorded a decision trace")
}

/// One backend's report, by id. The trace is compared through the same accessor
/// a reader would use, rather than by indexing into JSON.
fn report<'a>(
    trace: &'a DecisionTrace,
    backend_id: &str,
) -> &'a agent_ports::decision::DecisionBackendOutcome {
    &trace
        .report_for(backend_id)
        .unwrap_or_else(|| {
            let ids: Vec<&str> = trace.backends.iter().map(|r| r.backend_id.as_str()).collect();
            panic!("backend '{backend_id}' did not run; the trace has {ids:?}")
        })
        .outcome
}

#[test]
fn a_sidecar_that_answers_is_attached_asked_and_recorded() {
    let (data, workspace, home) = dirs();
    let sidecar = Sidecar::start(None);
    let config = config_with_sidecar(&home, &sidecar.url, "");

    let r = ran(data.path(), workspace.path(), &config, &["run", "hello"]);
    assert!(r.ok, "{}", r.err);
    // The run says which advisors it attached, before any turn runs.
    assert!(r.err.contains("decision backend: laya:"), "{}", r.err);

    let trace = trace_of(data.path());
    // Who answered, and who else was asked: the sidecar won, and the baseline
    // still ran. A trace that kept only the winner would have nothing to
    // compare the sidecar against.
    assert_eq!(trace.chosen_backend.as_deref(), Some("laya"));
    assert!(trace.aborted.is_none(), "the backends were reached");
    assert!(report(&trace, "rule").is_answered());
    assert_eq!(
        trace.report_for("laya").and_then(|r| r.revision.as_deref()),
        Some("multilingual"),
        "the trace attributes the answer to the checkpoint the configuration pinned"
    );

    // The answer is the sidecar's, not the baseline's: the fixture picks the
    // last option of the question, which is not the tier the router chose.
    let chosen = trace.chosen.as_ref().expect("the sidecar answered");
    assert_eq!(
        chosen.answer_for("tier").map(|a| a.value.clone()),
        Some(agent_ports::decision::DecisionValue::Categorical { choice: "thinking".to_string() })
    );

    // And the record is checkable after the fact: a reader holding only the log
    // re-derives the same conclusion from the same reports.
    agent_decision::verify(&trace)
        .expect("the trace the run persisted replays to its own conclusion");
}

#[test]
fn a_sidecar_that_is_not_there_degrades_to_the_rule_baseline() {
    let (data, workspace, home) = dirs();
    // Started and stopped, so the port is one nothing is listening on.
    let sidecar = Sidecar::start(None);
    let url = sidecar.url.clone();
    drop(sidecar);
    let config = config_with_sidecar(&home, &url, "");

    let r = ran(data.path(), workspace.path(), &config, &["run", "hello"]);
    // The run is not refused: a sidecar that is down is a decision backend that
    // failed, not a reason the conversation cannot happen.
    assert!(r.ok, "{}", r.err);

    let trace = trace_of(data.path());
    assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
    assert!(
        trace.aborted.is_none(),
        "the backend was asked and could not be reached, which is not the same as nobody being asked"
    );

    // Which failure label appears here is a property of this host's TCP stack,
    // not of the wiring. Measured on this machine: `connect()` to a closed
    // loopback port returns `WSAECONNREFUSED` after **2.004s** — Windows lets
    // the first SYN go unanswered before failing — which is longer than the
    // broker's 250ms budget, so a stopped sidecar is recorded as having run out
    // of time. On a host that refuses immediately the same code path records
    // `unavailable`. The decision is the same either way, and that is the point
    // worth asserting: a backend that did not answer hands the turn to the rule
    // baseline, whichever way it failed.
    let outcome = report(&trace, "laya").clone();
    let agent_ports::decision::DecisionBackendOutcome::Failed { error_kind } = &outcome else {
        panic!("a sidecar that is not running cannot have answered: {outcome:?}");
    };
    assert!(
        error_kind == "timeout" || error_kind == "unavailable",
        "a backend that could not be reached failed, as '{error_kind}': {outcome:?}"
    );
    agent_decision::verify(&trace).expect("degrading is a conclusion the record supports");
}

#[test]
fn a_sidecar_that_refuses_the_question_degrades_to_the_rule_baseline() {
    let (data, workspace, home) = dirs();
    // Reached, and answered — with an HTTP 500, the way a live `laya-serve`
    // answers a question it dislikes. This is the other half of the failure
    // surface, and the half that is deterministic on any host.
    let sidecar = Sidecar::start(Some("fail"));
    let config = config_with_sidecar(&home, &sidecar.url, "");

    let r = ran(data.path(), workspace.path(), &config, &["run", "hello"]);
    assert!(r.ok, "{}", r.err);

    let trace = trace_of(data.path());
    assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
    assert_eq!(
        report(&trace, "laya").clone(),
        agent_ports::decision::DecisionBackendOutcome::Failed {
            error_kind: "protocol".to_string()
        },
        "the sidecar was reached and declined, which is not the same fact as being unreachable"
    );
    agent_decision::verify(&trace).expect("the record still supports its own conclusion");
}

#[test]
fn a_sidecar_that_is_too_slow_loses_the_backend_deadline_and_the_rule_answers() {
    let (data, workspace, home) = dirs();
    // Two seconds late, against a 250ms per-backend deadline.
    let sidecar = Sidecar::start(Some("slow"));
    let config = config_with_sidecar(&home, &sidecar.url, "");

    let r = ran(data.path(), workspace.path(), &config, &["run", "hello"]);
    assert!(r.ok, "{}", r.err);

    let trace = trace_of(data.path());
    assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
    assert_eq!(
        report(&trace, "laya").clone(),
        agent_ports::decision::DecisionBackendOutcome::Failed { error_kind: "timeout".to_string() }
    );
    // The deadline, not the sidecar, decided: the call is bounded well under
    // the two seconds the fixture would have taken. This is the property that
    // makes a shadow decision allowed on the turn's path at all.
    assert!(trace.latency_ms < 1_500, "the call waited {}ms", trace.latency_ms);
}

#[test]
fn a_profile_that_turns_the_sidecar_off_runs_without_one() {
    let (data, workspace, home) = dirs();
    let sidecar = Sidecar::start(None);
    // Defined by the user layer and turned off by the scenario: the definition
    // is how to reach it, the profile is whether this run does.
    let config = config_with_sidecar(&home, &sidecar.url, "decisions = { laya = false }\n");

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "run", "hello"]);
    assert!(r.ok, "{}", r.err);
    assert!(
        !r.err.contains("decision backend:"),
        "a sidecar this run did not attach is not announced: {}",
        r.err
    );

    let trace = trace_of(data.path());
    assert!(trace.report_for("laya").is_none(), "the sidecar was not asked");
    assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
    assert!(
        trace.aborted.is_none(),
        "'nobody was asked' must not read as 'nothing ran': the baseline answered"
    );
}
