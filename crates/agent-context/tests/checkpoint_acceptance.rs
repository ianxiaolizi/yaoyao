//! Acceptance: the context engine refreshes the projection checkpoint, and the
//! checkpoint never becomes a second source of truth.
//!
//! The engine folds through `Projection::build`, which already prefers a valid
//! checkpoint and falls back to the log when it cannot trust one. What was
//! missing was the writer, so every restart folded the whole log. These drive
//! derivations over a real `JsonlEventStore` — the same store the product uses,
//! because the property under test is about a file another process will read —
//! and check both halves: the refresh happens when it is worth doing, and what
//! it leaves behind is only ever a cache the log still backs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_context::ProjectionContextEngine;
use agent_ports::context_engine::{ContextEngine, DerivedContext};
use agent_ports::event_store::EventStore;
use agent_projection::{CheckpointVerdict, ModelContextProjection, Projection};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{
    AttemptId, EventId, EventSeq, InboxItemId, SchemaVersion, SessionId, TurnId,
};
use agent_protocol::inbox::{InboxItem, InboxItemStatus, InboxKind};
use agent_protocol::model::ModelResponse;
use agent_store_jsonl::JsonlEventStore;
use chrono::Utc;
use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION: &str = "checkpoint-acceptance";

/// Events per synthetic turn: the user's item arrives, the turn starts, the
/// model answers.
const TURN_EVENTS: usize = 3;

struct Fixture {
    data: TempDir,
    store: Arc<JsonlEventStore>,
    session: SessionId,
    /// The seq of the last event appended, or `None` before the first.
    tail: Option<EventSeq>,
}

impl Fixture {
    fn new() -> Self {
        let data = tempfile::tempdir().expect("temp data dir");
        Self {
            store: Arc::new(JsonlEventStore::new(data.path())),
            data,
            session: SessionId::from_string(SESSION),
            tail: None,
        }
    }

    /// Append `turns` more synthetic turns to the log.
    async fn append_turns(&mut self, turns: usize) {
        let from = self.tail.map_or(EventSeq::ZERO, EventSeq::next);
        let drafts = transcript(&self.session, turns);
        self.store.append(&self.session, from, drafts).await.expect("append turns");
        self.tail = self.read_log().await.last().map(|event| event.seq);
    }

    async fn read_log(&self) -> Vec<agent_protocol::events::StoredEvent> {
        self.store.read_stream(&self.session, EventSeq::ZERO, usize::MAX).await.expect("read log")
    }

    /// An engine over this fixture's store, as a fresh process would build one.
    fn engine(&self) -> ProjectionContextEngine {
        ProjectionContextEngine::new(self.store.clone(), self.data.path())
    }

    async fn derive(&self) -> DerivedContext {
        self.engine().derive(&self.session).await.expect("derive")
    }

    fn checkpoint_file(&self) -> PathBuf {
        self.data.path().join(SESSION).join("checkpoints").join("model-context.json")
    }

    /// What a doctor run would say about the checkpoint on disk.
    async fn verdict(&self) -> CheckpointVerdict {
        let store: Arc<dyn EventStore> = self.store.clone();
        Projection::new(ModelContextProjection::new(), store, self.data.path())
            .check_checkpoint(&self.session)
            .await
            .expect("check checkpoint")
    }
}

/// A synthetic turn: an inbox item, the turn that consumes it, the answer.
fn transcript(session: &SessionId, turns: usize) -> Vec<EventDraft> {
    let mut drafts = Vec::with_capacity(turns * TURN_EVENTS);
    for index in 0..turns {
        let item = InboxItem {
            item_id: InboxItemId::new(),
            session_id: session.clone(),
            kind: InboxKind::NextTurn,
            payload: json!({ "text": format!("question {index}") }),
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };
        let turn_id = TurnId::new();
        drafts.push(draft(
            session,
            EventType::INBOX_APPENDED,
            Actor::User,
            None,
            serde_json::to_value(&item).expect("serialize item"),
        ));
        drafts.push(draft(
            session,
            EventType::TURN_STARTED,
            Actor::Scheduler,
            Some(turn_id.clone()),
            json!({ "inbox_item": item.item_id.as_str() }),
        ));
        drafts.push(draft(
            session,
            EventType::MODEL_RESPONSE_COMPLETED,
            Actor::Agent,
            Some(turn_id),
            json!({
                "attempt_id": AttemptId::new().as_str(),
                "response": serde_json::to_value(ModelResponse::text(format!("answer {index}")))
                    .expect("serialize response"),
            }),
        ));
    }
    drafts
}

fn draft(
    session: &SessionId,
    event_type: &str,
    actor: Actor,
    turn_id: Option<TurnId>,
    payload: Value,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session.clone(),
        run_id: None,
        turn_id,
        step_id: None,
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: json!({}),
        ignorable: false,
    }
}

/// Read the checkpoint file as plain JSON, so a test can damage it the way a
/// bad build or a stray edit would.
fn checkpoint_json(path: &Path) -> Value {
    let bytes = std::fs::read(path).expect("read checkpoint");
    serde_json::from_slice(&bytes).expect("checkpoint is JSON")
}

fn write_checkpoint_json(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).expect("serialize checkpoint"))
        .expect("write checkpoint");
}

#[tokio::test]
async fn a_long_session_leaves_a_checkpoint_doctor_calls_correct() {
    let mut fixture = Fixture::new();
    // Past the default interval, so the refresh is due.
    fixture.append_turns(120).await;
    let derived = fixture.derive().await;

    let path = fixture.checkpoint_file();
    assert!(path.exists(), "a derivation past the interval writes a checkpoint");

    // The refresh is only worth anything if the state it saved is the state the
    // log folds to: doctor` is the consumer that says so, and it says so by
    // folding the log itself.
    let verdict = fixture.verdict().await;
    assert_eq!(verdict, CheckpointVerdict::Matches { as_of_seq: Some(derived.revision) });

    // The staging file must not outlive the write it staged.
    let files: Vec<String> = std::fs::read_dir(path.parent().expect("checkpoints dir"))
        .expect("read dir")
        .map(|entry| entry.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, vec!["model-context.json".to_string()]);
}

#[tokio::test]
async fn a_short_session_writes_nothing() {
    let mut fixture = Fixture::new();
    fixture.append_turns(10).await;
    fixture.derive().await;

    assert!(
        !fixture.checkpoint_file().exists(),
        "below the interval a derivation is a read, not a write"
    );
}

#[tokio::test]
async fn the_interval_is_what_decides_the_write() {
    let mut fixture = Fixture::new();
    fixture.append_turns(1).await;
    fixture.engine().with_checkpoint_interval(0).derive(&fixture.session).await.expect("derive");

    assert!(
        fixture.checkpoint_file().exists(),
        "an interval of zero refreshes on the first derivation that folded anything"
    );
}

#[tokio::test]
async fn a_derivation_from_a_checkpoint_equals_folding_the_whole_log() {
    let mut fixture = Fixture::new();
    fixture.append_turns(120).await;
    fixture.derive().await;

    // A restart with a warm checkpoint folds only the tail: the saved seq is
    // behind the end of the log, which is the premise of this test. (That
    // `build` actually takes such a checkpoint is pinned in `agent-projection`.)
    fixture.append_turns(20).await;
    let saved = checkpoint_json(&fixture.checkpoint_file())["as_of_seq"]
        .as_u64()
        .expect("the checkpoint names the seq it folded");
    let tail = fixture.tail.expect("the log has events").as_u64();
    assert!(saved < tail, "the checkpoint ({saved}) is behind the tail ({tail})");

    let resumed = fixture.derive().await;
    let from_the_log = fixture
        .engine()
        .without_checkpoints()
        .derive(&fixture.session)
        .await
        .expect("derive from the log");

    assert_eq!(resumed.messages, from_the_log.messages, "the shortcut changes no message");
    assert_eq!(resumed.revision, from_the_log.revision, "the revision is the log's, either way");
}

#[tokio::test]
async fn a_checkpoint_from_another_build_is_not_served() {
    let mut fixture = Fixture::new();
    fixture.append_turns(120).await;
    fixture.derive().await;

    // What a build whose fold changed looks like: a state saved under a version
    // this one does not fold the same way.
    let path = fixture.checkpoint_file();
    let mut value = checkpoint_json(&path);
    value["state_version"] = json!(u64::MAX);
    write_checkpoint_json(&path, &value);

    let derived = fixture.derive().await;
    let from_the_log = fixture
        .engine()
        .without_checkpoints()
        .derive(&fixture.session)
        .await
        .expect("derive from the log");
    assert_eq!(derived.messages, from_the_log.messages, "an unusable checkpoint is a miss");

    // And the derivation that read it replaced it, so the cache heals.
    assert_eq!(
        fixture.verdict().await,
        CheckpointVerdict::Matches { as_of_seq: Some(derived.revision) }
    );
}

#[tokio::test]
async fn a_wrong_checkpoint_is_reported_by_doctor_rather_than_served_silently() {
    let mut fixture = Fixture::new();
    fixture.append_turns(120).await;
    fixture.derive().await;

    // Damaged in place: still this projection's, still grounded in the log, but
    // its state no longer matches the fold. `build` trusts it — it is a cache,
    // and a lookup that folds the log anyway would buy nothing — so the report
    // is the safety net, and it has to actually fire.
    let path = fixture.checkpoint_file();
    let mut value = checkpoint_json(&path);
    value["state"]["messages"].as_array_mut().expect("the folded transcript is an array").pop();
    write_checkpoint_json(&path, &value);

    assert!(
        matches!(fixture.verdict().await, CheckpointVerdict::Diverges { .. }),
        "doctor folds the log up to the checkpoint and must see the difference"
    );
}

#[tokio::test]
async fn a_checkpoint_that_cannot_be_written_does_not_fail_the_derivation() {
    let mut fixture = Fixture::new();
    fixture.append_turns(120).await;

    // A file where the checkpoint root should be: every write beneath it fails,
    // which is what a full disk or a read-only data directory looks like here.
    let blocked = fixture.data.path().join("blocked");
    std::fs::write(&blocked, b"not a directory").expect("occupy the path");

    let engine = ProjectionContextEngine::new(fixture.store.clone(), &blocked);
    let derived = engine.derive(&fixture.session).await.expect("derive still succeeds");
    let from_the_log = fixture
        .engine()
        .without_checkpoints()
        .derive(&fixture.session)
        .await
        .expect("derive from the log");
    assert_eq!(derived.messages, from_the_log.messages, "a failed write changes no message");
}
