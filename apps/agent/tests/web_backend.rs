//! The GUI's driver, in process: the assembly the binary builds for `agent web`.
//!
//! The browser smoke drives the real binary through Edge, and `web_end_to_end`
//! drives it over loopback, but both run the driver in a child process. What a
//! child does is not visible to a coverage run, and a failure there arrives as
//! a missing answer rather than as a named one. These tests call
//! [`WebBackend`] directly — the same assembly, in this process — so a turn the
//! page starts can be checked against the log the terminal would read, and the
//! refusals (a model this process cannot build, a decision for a turn that is
//! not waiting) can be read as the codes the HTTP layer maps.

use std::sync::Arc;

use agent::web_runtime::{WebBackend, WebInputs};
use agent_config::Layers;
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_protocol::events::EventType;
use agent_protocol::ids::{EventSeq, SessionId};
use agent_store_jsonl::JsonlEventStore;
use agent_web::{Backend, Decision, LiveChunk, LiveSink};
use tempfile::TempDir;
use tokio::sync::broadcast;

const SESSION: &str = "web-driver";

/// A driver over a throwaway workspace and data directory, plus the store it
/// writes, so a test can read what the page would have been told.
fn driver(dir: &TempDir) -> (Arc<WebBackend>, Arc<dyn EventStore>) {
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&data).expect("data dir");
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(&data));
    let backend = Arc::new(WebBackend::new(WebInputs {
        workspace_root: workspace,
        data_dir: data,
        // The configuration a run in a directory with no configuration has, as
        // the acceptance suites state it: the environment belongs to the whole
        // process and a test does not get to rewrite it.
        resolved: agent_config::resolve(Layers::default()).expect("the defaults resolve"),
        sandbox: SandboxMode::LocalProcess,
        network_allowed: false,
        judgment: true,
        // The one provider this build can always make, and the one the browser
        // smoke runs: no endpoint, no credential, no clock to wait on.
        provider: "echo".to_string(),
        store: store.clone(),
    }));
    (backend, store)
}

/// The sink a turn publishes live text on. No receiver: a page that closed its
/// tab is not an error, and this proves the publish path does not need one.
fn sink() -> LiveSink {
    broadcast::channel::<LiveChunk>(16).0
}

async fn log(store: &Arc<dyn EventStore>, session: &SessionId) -> Vec<String> {
    store
        .read_stream(session, EventSeq::ZERO, usize::MAX)
        .await
        .expect("read the log")
        .iter()
        .map(|event| event.event.event_type.as_str().to_string())
        .collect()
}

#[tokio::test]
async fn a_turn_from_the_page_writes_the_log_the_terminal_reads() {
    let dir = TempDir::new().expect("a temporary directory");
    let (backend, store) = driver(&dir);
    let session = SessionId::from_string(SESSION);

    backend.run_turn(&session, "say hello", &sink()).await.expect("the turn ran");

    // The same events the terminal writes for `agent ask`: the page is not a
    // separate kind of turn, and the log does not know which surface asked.
    let types = log(&store, &session).await;
    assert!(types.iter().any(|name| name == EventType::TURN_STARTED), "{types:?}");
    assert!(types.iter().any(|name| name == EventType::TURN_COMPLETED), "{types:?}");

    // And the prompt in the log is the text the page sent.
    let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.expect("read");
    let completed = events
        .iter()
        .find(|event| event.event.event_type.as_str() == EventType::TURN_COMPLETED)
        .expect("the turn ended");
    let answer = completed.event.payload["final_content"].as_str().unwrap_or_default();
    assert!(answer.contains("say hello"), "the echo provider answers the prompt: {answer}");
}

#[tokio::test]
async fn a_model_choice_is_written_down_and_a_bad_one_is_refused() {
    let dir = TempDir::new().expect("a temporary directory");
    let (backend, store) = driver(&dir);
    let session = SessionId::from_string(SESSION);

    // A provider this build can make is recorded in the session's own log: the
    // choice belongs to the session, not to the server.
    let choice = backend.set_model(&session, "echo", None).await.expect("echo is buildable");
    assert_eq!(choice.provider, "echo");
    let types = log(&store, &session).await;
    assert!(
        types.iter().any(|name| name == EventType::SESSION_MODEL_SELECTED),
        "the choice is an event: {types:?}"
    );

    // One it cannot build is refused before anything is written: a stored
    // choice this process cannot honour would break every later turn.
    let refused = backend.set_model(&session, "not-a-provider", None).await.expect_err("refused");
    assert_eq!(refused.code, "invalid_model", "{refused:?}");
    let after = log(&store, &session).await;
    assert_eq!(after, types, "a refused choice writes nothing");
}

#[tokio::test]
async fn deleting_a_session_removes_its_stack_and_its_log() {
    let dir = TempDir::new().expect("a temporary directory");
    let (backend, store) = driver(&dir);
    let session = SessionId::from_string(SESSION);

    // A turn first, so the session has a cached runtime as well as a log: the
    // delete has to drop both, and on Windows the stack's open handles are what
    // would otherwise keep the directory on the disk.
    backend.run_turn(&session, "hello", &sink()).await.expect("the turn ran");
    let log_path = dir.path().join("data").join(SESSION).join("events.jsonl");
    assert!(log_path.is_file(), "the turn wrote a log");

    backend.delete_session(&session).await.expect("the session was deleted");
    assert!(!dir.path().join("data").join(SESSION).exists(), "the directory is gone");

    // The store agrees: nothing is left under the id, and a read that once saw
    // a turn now sees the empty log of a session that has never been written.
    let left = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.expect("reads");
    assert!(left.is_empty(), "the deleted session reads back empty: {left:?}");
}

#[tokio::test]
async fn a_decision_for_a_turn_that_is_not_waiting_is_refused_by_name() {
    let dir = TempDir::new().expect("a temporary directory");
    let (backend, _store) = driver(&dir);
    let session = SessionId::from_string(SESSION);

    // No turn has ever been yielded, so there is no card to settle. The refusal
    // is about the approval, not about the server.
    let refused = backend
        .settle_approval(&session, "no-such-approval", Decision::Once, &sink())
        .await
        .expect_err("nothing to settle");
    assert_eq!(refused.code, "approval_not_pending", "{refused:?}");

    // Nor is there anything to resume: the same answer from the other verb.
    let resumed = backend.resume(&session, &sink()).await.expect_err("nothing to resume");
    assert_eq!(resumed.code, "internal", "{resumed:?}");

    // An interrupt with nothing running is an answer, not a failure: `false`
    // means the click had nothing to stop.
    assert!(!backend.interrupt(&session).await.expect("an answer"));
}

#[tokio::test]
async fn a_fork_of_a_closed_turn_names_the_child() {
    let dir = TempDir::new().expect("a temporary directory");
    let (backend, store) = driver(&dir);
    let session = SessionId::from_string(SESSION);

    backend.run_turn(&session, "first", &sink()).await.expect("the turn ran");
    let child = backend
        .fork(&session, None, "try another way")
        .await
        .expect("the fork ran")
        .expect("a closed turn is a boundary to fork from");

    // The child is a session of its own, with the parent's turn behind it.
    let child_log = store.read_stream(&child, EventSeq::ZERO, usize::MAX).await;
    let events = child_log.expect("the child has a log");
    assert!(!events.is_empty(), "the fork copied the turn it forked from");
}
