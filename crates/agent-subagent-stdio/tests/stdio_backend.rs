//! The stdio backend against a real process.
//!
//! Every test here starts a real binary, writes to its stdin, reads its stdout
//! and, in most of them, kills it. That is the point: the questions this file
//! asks — did the request arrive intact, does a child that answers get put down
//! rather than waited for, does a deadline actually stop something, does a
//! process tree go with its child — are questions about processes, and a fake
//! child would answer them all by construction.
//!
//! The fixture encodes the wire by hand and never imports this crate's types, so
//! these are tests of whether two independent implementations agree rather than
//! of whether this crate agrees with itself.

use std::path::Path;
use std::time::{Duration, Instant};

use agent_ports::subagent::{
    SubagentError, SubagentFeature, SubagentHandle, SubagentOutcome, SubagentProvider,
    SubagentRequest,
};
use agent_protocol::ids::SessionId;
use agent_subagent_stdio::StdioSubagentProvider;

/// The `fake-subagent` example Cargo built for this package's tests.
///
/// It is an example rather than a `src/bin/` binary so that `cargo build
/// --workspace` does not put a test double where product binaries go; `cargo
/// test` builds examples, and that is how the gates run. Cargo exports no
/// `CARGO_BIN_EXE_*` for an example, so the path is worked out from where this
/// test binary sits (`<target>/<profile>/deps/`), and the example is built on
/// the spot when a narrower `--test` selection did not build it — a test that
/// skipped itself would report success for a wire it never walked.
fn fixture() -> String {
    let deps = std::env::current_exe().expect("the test binary has a path");
    let profile = deps.parent().and_then(Path::parent).expect("<target>/<profile>");
    let examples = profile.join("examples");
    let path = examples.join(if cfg!(windows) { "fake-subagent.exe" } else { "fake-subagent" });
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        // Build into the same target directory this test binary lives under: a
        // coverage build relocates it, and a default build would not be found here.
        let mut build = std::process::Command::new(cargo);
        build
            .args(["build", "-p", "agent-subagent-stdio", "--example", "fake-subagent"])
            .arg("--target-dir")
            .arg(profile.parent().expect("the target directory"))
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        if profile.ends_with("release") {
            build.arg("--release");
        }
        let built = build.status().expect("cargo is on the path");
        assert!(built.success(), "the fake-subagent example could not be built");
        assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    }
    path.to_string_lossy().into_owned()
}

/// A provider that runs the fixture in `mode`, with a ceiling well above every
/// deadline the tests ask for unless they say otherwise.
fn provider(mode: &str) -> StdioSubagentProvider {
    StdioSubagentProvider::new("fake", fixture())
        .with_args(vec![mode.to_string()])
        .with_ceiling(Duration::from_secs(30))
}

/// The same, in a mode that takes an argument.
fn provider_with(mode: &str, argument: &str) -> StdioSubagentProvider {
    StdioSubagentProvider::new("fake", fixture())
        .with_args(vec![mode.to_string(), argument.to_string()])
        .with_ceiling(Duration::from_secs(30))
}

fn child(name: &str) -> SessionId {
    SessionId::from_string(name)
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|data| data.len()).unwrap_or(0)
}

/// Wait until the file has grown to `at_least` bytes, or say how big it got.
async fn wait_for_growth(path: &Path, at_least: u64) {
    for _ in 0..300 {
        if size(path) >= at_least {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the file never reached {at_least} bytes; it is {} bytes", size(path));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_is_started_asked_and_heard_from() {
    let provider = provider("result");
    let request = SubagentRequest {
        context_refs: vec!["artifact://notes".to_string()],
        tool_allowlist: vec!["fs.read".to_string()],
        timeout_ms: Some(700),
        ..SubagentRequest::new("summarize the notes")
    };

    let handle = provider
        .spawn(&child("c1"), None, request)
        .await
        .expect("the child starts and is told what to do");
    let outcome = provider.wait(&handle).await.expect("the child is heard from");

    let SubagentOutcome::Completed { summary, result } = outcome else {
        panic!("expected the child's answer, got {outcome:?}");
    };
    assert_eq!(summary, "summarize the notes");

    // The child echoed the request it received, which is the only place the
    // parent's side of the wire can be checked: what arrived there is what was
    // written here.
    let echo = result.expect("the fixture echoes its request");
    assert_eq!(echo["type"], "request");
    assert_eq!(echo["objective"], "summarize the notes");
    assert_eq!(echo["context_refs"][0], "artifact://notes");
    assert_eq!(echo["tool_allowlist"][0], "fs.read");
    assert_eq!(
        echo["timeout_ms"], 700,
        "the child must be told the deadline it is actually held to"
    );

    // The fixture does not exit after answering. `wait` returned anyway, which
    // is the whole of the claim that a parent ends its children rather than
    // waiting for them to end themselves.
    assert!(
        provider.wait(&handle).await.is_ok(),
        "the outcome stays readable after the child has ended"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_answers_with_less_than_it_was_asked_for_says_so() {
    // The fixture writes a `progress` line before this one, so this also covers
    // a child that talks between its request and its answer: tolerated, and not
    // mistaken for the answer.
    let provider = provider("partial");
    let handle = provider
        .spawn(&child("c2"), None, SubagentRequest::new("read all three notes"))
        .await
        .expect("the child starts");
    let outcome = provider.wait(&handle).await.expect("the child is heard from");

    assert_eq!(
        outcome,
        SubagentOutcome::Partial { summary: "two of the three notes".to_string() },
        "a partial answer is the child's own report and is passed on unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_fails_carries_what_it_says_it_changed() {
    let provider = provider("fail");
    let handle = provider
        .spawn(&child("c3"), None, SubagentRequest::new("summarize"))
        .await
        .expect("the child starts");
    let outcome = provider.wait(&handle).await.expect("the child is heard from");

    let SubagentOutcome::Failed { message, side_effects } = outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert!(message.contains("empty"), "{message}");
    assert_eq!(
        side_effects,
        vec!["created notes/summary.md".to_string()],
        "a failed child may still have changed the world, so what it says it did survives"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_writes_something_the_wire_does_not_define_is_put_down() {
    // Four kinds of disagreement, each of which has to end the delegation the
    // same way: the parent cannot go on reading a child whose meaning it does
    // not have, and a child doing that is not one whose further output should
    // be believed.
    for garbage in ["not-json", "unknown-type", "unknown-status", "no-summary"] {
        let provider = provider_with("garbage", garbage);
        let handle = provider
            .spawn(&child("c4"), None, SubagentRequest::new("anything"))
            .await
            .expect("the child starts");

        let outcome = provider.wait(&handle).await.expect("the child is heard from");
        let SubagentOutcome::Failed { message, .. } = &outcome else {
            panic!("{garbage}: expected a failure, got {outcome:?}");
        };
        assert!(message.contains("protocol violation"), "{garbage}: {message}");

        // And the child, which was written to keep running after saying it, is
        // gone: `dispose` finds nothing left to release only if the supervisor
        // already reaped it, which it does having put the child down.
        provider.dispose(&handle).await.expect("disposing is fine");
        assert!(
            matches!(provider.wait(&handle).await, Err(SubagentError::Unknown { .. })),
            "{garbage}: the child outlived its own protocol violation"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_dies_without_answering_says_how_and_what_it_said() {
    // Silence is not an answer. A child that exits having said nothing leaves
    // the parent with two facts it does have — the exit and the child's own last
    // words — and both belong in the failure rather than being guessed at.
    let provider = provider("silent-exit");
    let handle = provider
        .spawn(&child("c5"), None, SubagentRequest::new("anything"))
        .await
        .expect("the child starts");
    let outcome = provider.wait(&handle).await.expect("the child is heard from");

    let SubagentOutcome::Failed { message, .. } = outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert!(message.contains("exit code 3"), "{message}");
    assert!(message.contains("nothing to say about that"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_never_answers_is_stopped_at_its_deadline() {
    // The one feature this backend enforces rather than carries: it owns the
    // process, so a deadline is a real one.
    let dir = tempfile::tempdir().expect("a temp dir");
    let heartbeat = dir.path().join("heartbeat");
    let provider = provider_with("stubborn", heartbeat.to_str().expect("a path"));
    let request = SubagentRequest { timeout_ms: Some(400), ..SubagentRequest::new("anything") };

    let started = Instant::now();
    let handle = provider.spawn(&child("c6"), None, request).await.expect("the child starts");
    let outcome = provider.wait(&handle).await.expect("the deadline is reported");
    let elapsed = started.elapsed();

    let SubagentOutcome::TimedOut { message } = outcome else {
        panic!("expected a timeout, got {outcome:?}");
    };
    assert!(message.contains("400"), "the failure should name the deadline: {message}");
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(20),
        "the child was stopped at neither its deadline nor a sane time after it: {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_child_takes_what_it_started_with_it() {
    // A delegated agent may start its own work, and that work is a grandchild.
    // A parent that kills only the direct child reports the delegation over
    // while its descendants are still going.
    let dir = tempfile::tempdir().expect("a temp dir");
    let heartbeat = dir.path().join("heartbeat");
    let provider = provider_with("stubborn", heartbeat.to_str().expect("a path"));

    let handle = provider
        .spawn(&child("c7"), None, SubagentRequest::new("anything"))
        .await
        .expect("the child starts");
    wait_for_growth(&heartbeat, 4).await;

    let outcome = provider.cancel(&handle).await.expect("the child is stopped");
    assert_eq!(outcome, SubagentOutcome::Cancelled);

    // Whatever was in flight when the child died lands now; after that the file
    // must not move again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let settled = size(&heartbeat);
    tokio::time::sleep(Duration::from_millis(600)).await;

    if cfg!(windows) {
        assert_eq!(
            size(&heartbeat),
            settled,
            "the grandchild outlived the child that started it, so the tree was not taken down"
        );
    } else {
        // Only the direct child dies here: reaching a process group needs a
        // POSIX API this workspace denies itself. Same gap M5 recorded for
        // `shell.run`, and the same reason it is recorded rather than hidden.
        assert!(settled > 0, "the heartbeat never ran, so nothing was tested");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_the_backend_cannot_honour_never_starts_a_process() {
    // A budget is enforced by the loop that spends it, and this backend runs no
    // loop. It says so instead of passing the limit down a wire where nothing
    // holds it — the silent downgrade the negotiation exists to prevent.
    //
    // The proof that *no process* started is the program name: it cannot be
    // executed, so if the refusal came from anything other than the negotiation
    // this would be a spawn failure instead.
    let provider = StdioSubagentProvider::new("fake", "definitely-not-a-real-program-xyz")
        .with_ceiling(Duration::from_secs(30));
    let request = SubagentRequest {
        budget: Some(agent_protocol::profile::RunBudget { max_steps: 3, ..Default::default() }),
        ..SubagentRequest::new("anything")
    };

    let refused = provider
        .spawn(&child("c8"), None, request)
        .await
        .expect_err("a request asking for a budget is refused");

    match refused {
        SubagentError::Unsupported { ref features, .. } => {
            assert_eq!(features.iter().collect::<Vec<_>>(), vec![SubagentFeature::Budget]);
        }
        other => panic!("expected a negotiation refusal, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_program_that_cannot_be_started_names_the_child_it_could_not() {
    let provider = StdioSubagentProvider::new("fake", "definitely-not-a-real-program-xyz");

    let failed = provider
        .spawn(&child("c9"), None, SubagentRequest::new("anything"))
        .await
        .expect_err("a program that does not exist cannot be a child");

    let SubagentError::Spawn { child_session_id, ref message } = failed else {
        panic!("expected a spawn failure, got {failed}");
    };
    assert_eq!(child_session_id, child("c9"));
    assert!(message.contains("definitely-not-a-real-program-xyz"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_ceiling_bounds_what_a_request_may_ask_for() {
    // A request may shorten a child's time and never extend it: the party that
    // configured the backend is the party that knows what a child may cost.
    let dir = tempfile::tempdir().expect("a temp dir");
    let heartbeat = dir.path().join("heartbeat");
    let provider = provider_with("stubborn", heartbeat.to_str().expect("a path"))
        .with_ceiling(Duration::from_millis(300));
    let request = SubagentRequest {
        // Far longer than the ceiling, and it must not be honoured.
        timeout_ms: Some(60_000),
        ..SubagentRequest::new("anything")
    };

    let started = Instant::now();
    let handle = provider.spawn(&child("c10"), None, request).await.expect("the child starts");
    let outcome = provider.wait(&handle).await.expect("the deadline is reported");

    assert!(matches!(outcome, SubagentOutcome::TimedOut { .. }), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the request's own deadline was honoured over the configured ceiling"
    );
    assert!(
        provider.wait(&handle).await.is_ok(),
        "the child is still readable after it was stopped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_this_backend_does_not_hold_cannot_be_waited_for_or_cancelled() {
    // Answering "it ended somehow" for a child that was never started would turn
    // a bookkeeping mistake into a delegation outcome, which is the one place a
    // mistake must not be able to hide.
    let provider = provider("result");
    let stranger = SubagentHandle::new("fake", child("never-started"), None);

    assert!(matches!(provider.wait(&stranger).await, Err(SubagentError::Unknown { .. })));
    assert!(matches!(provider.cancel(&stranger).await, Err(SubagentError::Unknown { .. })));

    // Disposing, though, is a no-op rather than an error: the only reason to
    // dispose something unknown is not knowing whether it is known.
    provider.dispose(&stranger).await.expect("disposing an unknown child is a no-op");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disposing_a_running_child_stops_it_and_releases_the_handle() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let heartbeat = dir.path().join("heartbeat");
    let provider = provider_with("stubborn", heartbeat.to_str().expect("a path"));

    let handle = provider
        .spawn(&child("c11"), None, SubagentRequest::new("anything"))
        .await
        .expect("the child starts");
    wait_for_growth(&heartbeat, 4).await;

    provider.dispose(&handle).await.expect("the child is released");

    // Released means released: the provider no longer knows the child, and
    // `wait` says so rather than inventing an ending for something it dropped.
    assert!(matches!(provider.wait(&handle).await, Err(SubagentError::Unknown { .. })));
    provider.dispose(&handle).await.expect("disposing twice is a no-op");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let settled = size(&heartbeat);
    tokio::time::sleep(Duration::from_millis(600)).await;
    if cfg!(windows) {
        assert_eq!(size(&heartbeat), settled, "a disposed child was left running");
    }
}
