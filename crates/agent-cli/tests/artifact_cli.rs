//! `artifact get`: the reader half of the artifact store.
//!
//! The store's own tests prove that bytes round-trip. These prove the command
//! a person or a script actually holds: a locator from the log reaches the
//! bytes, and the exit status tells a caller which kind of answer it got
//! without parsing stdout (C3.1).

use agent_cli::{CliOutcome, CliOutput, Verdict, run_artifact_get};
use agent_ports::artifact_store::ArtifactStore;
use agent_testkit::InMemoryArtifactStore;

fn text(outcome: &CliOutcome) -> &str {
    match &outcome.output {
        CliOutput::Text(text) => text,
        CliOutput::Json(document) => panic!("expected a text answer, got {document}"),
    }
}

fn json(outcome: &CliOutcome) -> serde_json::Value {
    match &outcome.output {
        CliOutput::Json(document) => serde_json::from_str(document.as_str()).unwrap(),
        CliOutput::Text(text) => panic!("expected a JSON answer, got {text}"),
    }
}

#[test]
fn a_locator_fetches_the_bytes_it_names() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"hello artifact", "text/plain").unwrap();

    let outcome = run_artifact_get(&store, &artifact.locator(), None, false).unwrap();

    assert_eq!(outcome.verdict, Verdict::Answered);
    assert_eq!(text(&outcome), "hello artifact");
}

#[test]
fn a_bare_hash_is_the_same_request() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"same blob, shorter reference", "text/plain").unwrap();

    let outcome = run_artifact_get(&store, &artifact.hash, None, false).unwrap();

    assert_eq!(outcome.verdict, Verdict::Answered);
    assert_eq!(text(&outcome), "same blob, shorter reference");
}

#[test]
fn out_writes_the_bytes_and_the_json_names_the_file() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"written to a file", "text/plain").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.txt");

    let outcome = run_artifact_get(&store, &artifact.locator(), path.to_str(), true).unwrap();

    assert_eq!(outcome.verdict, Verdict::Answered);
    assert_eq!(std::fs::read(&path).unwrap(), b"written to a file");
    let answer = json(&outcome);
    assert_eq!(answer["locator"], artifact.locator());
    assert_eq!(answer["hash"], artifact.hash);
    assert_eq!(answer["bytes"], 17);
    assert_eq!(answer["out"], path.to_str().unwrap());
}

#[test]
fn a_missing_artifact_is_an_answer_not_a_failure_to_run() {
    let store = InMemoryArtifactStore::default();
    let missing = "0".repeat(64);

    let outcome = run_artifact_get(&store, &missing, None, false).unwrap();

    // The store was reached and said "no"; that is an answer about the
    // artifact, so the status is 1, not 2.
    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
    assert!(text(&outcome).contains("artifact not found"), "{}", text(&outcome));
}

#[test]
fn a_missing_artifact_keeps_one_json_shape() {
    let store = InMemoryArtifactStore::default();
    let missing = "0".repeat(64);

    let outcome = run_artifact_get(&store, &missing, None, true).unwrap();

    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
    let answer = json(&outcome);
    assert_eq!(answer["found"], false);
    assert_eq!(answer["hash"], missing);
    assert!(answer["reason"].as_str().unwrap().contains("not found"));
}

#[test]
fn a_damaged_blob_is_an_answer_about_that_artifact() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"the true bytes", "text/plain").unwrap();
    store.damage(&artifact.hash, b"someone else's bytes");

    let outcome = run_artifact_get(&store, &artifact.locator(), None, false).unwrap();

    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
    assert!(text(&outcome).contains("does not match its content"), "{}", text(&outcome));
}

#[test]
fn a_binary_blob_is_not_dumped_into_a_terminal() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(&[0xff, 0xfe, 0x00, 0x01], "application/octet-stream").unwrap();

    let outcome = run_artifact_get(&store, &artifact.locator(), None, false).unwrap();

    // Nothing was delivered, so this is "did not run" (2), and the answer says
    // what to do instead — it does not print the bytes.
    assert_eq!(outcome.verdict, Verdict::DidNotRun);
    assert!(text(&outcome).contains("--out <path>"), "{}", text(&outcome));
    assert!(!text(&outcome).contains('\u{fffd}'));
}

#[test]
fn json_without_out_is_refused_before_anything_is_read() {
    let store = InMemoryArtifactStore::default();
    let artifact = store.put(b"text", "text/plain").unwrap();

    let error = run_artifact_get(&store, &artifact.locator(), None, true).unwrap_err();

    assert!(error.contains("--json needs --out"), "{error}");
}

#[test]
fn a_locator_under_another_scheme_is_not_silently_a_hash() {
    let store = InMemoryArtifactStore::default();

    let error = run_artifact_get(&store, "artifact://md5/abc", None, false).unwrap_err();

    assert!(error.contains("not an artifact locator"), "{error}");
}

#[test]
fn a_name_the_store_cannot_read_is_not_an_answer() {
    let store = InMemoryArtifactStore::default();

    // Not a blob name at all: the store never looked for bytes, so there is no
    // artifact to answer about.
    let error = run_artifact_get(&store, "not-a-hash", None, false).unwrap_err();

    assert!(error.contains("not a blob name"), "{error}");
}
