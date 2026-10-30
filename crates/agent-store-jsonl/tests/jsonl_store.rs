//! Contract tests for the JSONL EventStore, mirroring the FakeEventStore tests
//! plus crash-recovery and torn-tail coverage that only a real store can prove.

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::*;
use agent_store_jsonl::JsonlEventStore;
use chrono::Utc;
use std::time::Duration;

fn draft(session_id: &str, event_type: &str) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: SessionId::from_string(session_id),
        run_id: Some(RunId::new()),
        turn_id: Some(TurnId::new()),
        step_id: Some(StepId::new()),
        actor: Actor::System,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

#[tokio::test]
async fn appends_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("s1");

    let receipt = store
        .append(&sid, EventSeq::ZERO, vec![draft("s1", EventType::TURN_STARTED)])
        .await
        .unwrap();
    assert_eq!(receipt.appended.len(), 1);
    assert_eq!(receipt.appended[0].seq, EventSeq::ZERO);

    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.session_id, sid);
    assert_eq!(events[0].event.event_type.as_str(), EventType::TURN_STARTED);
}

#[tokio::test]
async fn assigns_contiguous_seq_across_appends() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("s2");

    let r1 = store
        .append(&sid, EventSeq::ZERO, vec![draft("s2", EventType::TURN_STARTED)])
        .await
        .unwrap();
    let r2 = store
        .append(&sid, EventSeq::new(1), vec![draft("s2", EventType::TURN_COMPLETED)])
        .await
        .unwrap();

    assert_eq!(r1.appended[0].seq, EventSeq::ZERO);
    assert_eq!(r2.appended[0].seq, EventSeq::new(1));
}

#[tokio::test]
async fn rejects_stale_expected_next_seq() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("s3");

    store.append(&sid, EventSeq::ZERO, vec![draft("s3", EventType::TURN_STARTED)]).await.unwrap();

    let err = store
        .append(&sid, EventSeq::ZERO, vec![draft("s3", EventType::TURN_COMPLETED)])
        .await
        .unwrap_err();
    match err {
        EventStoreError::SequenceConflict { expected, actual } => {
            assert_eq!(expected, EventSeq::ZERO);
            assert_eq!(actual, EventSeq::new(1));
        }
        other => panic!("expected SequenceConflict, got {other:?}"),
    }
}

/// The background writer's entry point: the sequence is decided by the store,
/// under the lock that writes it, so two writers that never consulted each
/// other still land in order.
///
/// Racing calls are the *normal* case for this method — a turn and the task
/// collecting a child's settlement write the same log — and neither may see a
/// conflict: there was no decision built on a read that a conflict would say to
/// re-make, which is exactly the distinction `append`'s refusal draws.
#[tokio::test]
async fn append_at_tail_lands_after_whatever_the_log_is_at() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("tail");

    // An empty log starts at zero rather than refusing to have a tail.
    let first =
        store.append_at_tail(&sid, vec![draft("tail", EventType::TURN_STARTED)]).await.unwrap();
    assert_eq!(first.appended[0].seq, EventSeq::ZERO);

    let second =
        store.append_at_tail(&sid, vec![draft("tail", EventType::STEP_STARTED)]).await.unwrap();
    assert_eq!(second.appended[0].seq, EventSeq::new(1));

    // Two writers at once, neither holding anything the other can invalidate.
    let (a, b) = tokio::join!(
        store.append_at_tail(&sid, vec![draft("tail", EventType::TOOL_STARTED)]),
        store.append_at_tail(&sid, vec![draft("tail", EventType::TOOL_COMPLETED)]),
    );
    let (a, b) =
        (a.expect("the first background append"), b.expect("the second background append"));
    let mut seqs = vec![a.appended[0].seq.as_u64(), b.appended[0].seq.as_u64()];
    seqs.sort_unstable();
    assert_eq!(seqs, vec![2, 3], "both writers landed, at distinct sequence numbers");

    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let seqs: Vec<u64> = events.iter().map(|event| event.seq.as_u64()).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3], "the log is contiguous after racing writers");
}

#[tokio::test]
async fn is_idempotent_on_duplicate_event_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("s4");
    let d = draft("s4", EventType::TURN_STARTED);

    let r1 = store.append(&sid, EventSeq::ZERO, vec![d.clone()]).await.unwrap();
    assert_eq!(r1.appended.len(), 1);

    let r2 = store.append(&sid, EventSeq::new(1), vec![d]).await.unwrap();
    assert_eq!(r2.appended.len(), 0);
    assert_eq!(r2.duplicates, 1);

    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1); // applied once, not twice
}

#[tokio::test]
async fn flush_reports_durable_seq() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("s5");

    store.append(&sid, EventSeq::ZERO, vec![draft("s5", EventType::TURN_STARTED)]).await.unwrap();
    store
        .append(&sid, EventSeq::new(1), vec![draft("s5", EventType::TURN_COMPLETED)])
        .await
        .unwrap();

    let flush = store.flush(&sid).await.unwrap();
    assert_eq!(flush.durable_seq, EventSeq::new(1));
}

#[tokio::test]
async fn reopens_and_recovers_durable_events() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sid = SessionId::from_string("s6");

    {
        let store = JsonlEventStore::new(root.clone());
        store
            .append(&sid, EventSeq::ZERO, vec![draft("s6", EventType::TURN_STARTED)])
            .await
            .unwrap();
        store
            .append(&sid, EventSeq::new(1), vec![draft("s6", EventType::TURN_COMPLETED)])
            .await
            .unwrap();
        store.flush(&sid).await.unwrap();
    }

    let reopened = JsonlEventStore::new(root.clone());
    let events = reopened.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, EventSeq::ZERO);
    assert_eq!(events[1].seq, EventSeq::new(1));

    // Idempotency also survives reopen: the same event id must be skipped.
    let d = events[0].event.clone();
    let receipt = reopened.append(&sid, EventSeq::new(2), vec![d]).await.unwrap();
    assert_eq!(receipt.appended.len(), 0);
    assert_eq!(receipt.duplicates, 1);
}

#[tokio::test]
async fn truncates_torn_tail_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sid = SessionId::from_string("s7");

    {
        let store = JsonlEventStore::new(root.clone());
        store
            .append(&sid, EventSeq::ZERO, vec![draft("s7", EventType::TURN_STARTED)])
            .await
            .unwrap();
        store.flush(&sid).await.unwrap();
    }
    // This covers the reopen path only — the store above is gone before the
    // fragment exists, which is exactly the case that heals itself. The window
    // a *live* session falls into is `an_open_session_drops_a_torn_tail_before_appending`.

    // Simulate a crash mid-write: a partial line with no trailing newline.
    let events_path = root.join("s7").join("events.jsonl");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&events_path).unwrap();
        f.write_all(b"{\"seq\":1,\"recorded_at\":\"2026-").unwrap();
    }

    let reopened = JsonlEventStore::new(root.clone());
    let events = reopened.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, EventSeq::ZERO);

    // The torn fragment was truncated; the next append continues from seq 1.
    let receipt = reopened
        .append(&sid, EventSeq::new(1), vec![draft("s7", EventType::TURN_COMPLETED)])
        .await
        .unwrap();
    assert_eq!(receipt.appended.len(), 1);
    assert_eq!(receipt.appended[0].seq, EventSeq::new(1));
}

/// The window `truncates_torn_tail_on_reopen` cannot reach: the session is
/// already open in this process when a dead writer's fragment appears. The next
/// append has to clear it, because the write goes to the end of the *file* — an
/// append left in place would become the head of the new record, and the two
/// would be one line that parses as neither. That is what made the session
/// unreadable, in this process and in every one that opened it afterwards.
///
/// What is asserted is that the log *reads back*, not that the append returned
/// `Ok`: appending onto the fragment returns `Ok` too — that is the defect — so
/// a test that stopped at the receipt would stay green with the bug in place.
#[tokio::test]
async fn an_open_session_drops_a_torn_tail_before_appending() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sid = SessionId::from_string("s8");
    let store = JsonlEventStore::new(root.clone());
    store.append(&sid, EventSeq::ZERO, vec![draft("s8", EventType::TURN_STARTED)]).await.unwrap();

    // A writer that died before reaching its newline, with the session open.
    append_raw(&root, "s8", b"{\"seq\":1,\"recorded_at\":\"2026-");

    let receipt = store
        .append(&sid, EventSeq::new(1), vec![draft("s8", EventType::TURN_COMPLETED)])
        .await
        .unwrap();
    assert_eq!(receipt.appended.len(), 1);
    assert_eq!(receipt.appended[0].seq, EventSeq::new(1));

    // Readable here ...
    assert_eq!(seqs(&store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap()), vec![0, 1]);

    // ... and from a fresh process, which is what says the bytes left the file
    // rather than being tolerated in one process's memory.
    let reopened = JsonlEventStore::new(root.clone());
    assert_eq!(seqs(&reopened.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap()), vec![0, 1]);
}

/// Clearing the fragment must not take the complete line in front of it: the
/// tail examined holds bytes another writer *finished*, and those are facts.
#[tokio::test]
async fn a_complete_record_in_front_of_the_fragment_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sid = SessionId::from_string("s9");
    let store = JsonlEventStore::new(root.clone());
    store.append(&sid, EventSeq::ZERO, vec![draft("s9", EventType::TURN_STARTED)]).await.unwrap();

    // Another writer completes a record, then a third dies mid-write. This
    // store has folded neither: both arrived after it last read the tail.
    let other = JsonlEventStore::new(root.clone());
    other
        .append(&sid, EventSeq::new(1), vec![draft("s9", EventType::TURN_COMPLETED)])
        .await
        .unwrap();
    append_raw(&root, "s9", b"{\"seq\":2,\"recorded_at\":\"2026-");

    // The complete record is folded on the way to the write, so the next
    // sequence is 2 — the fragment contributed none.
    let receipt = store
        .append(&sid, EventSeq::new(2), vec![draft("s9", EventType::TURN_CANCELLED)])
        .await
        .unwrap();
    assert_eq!(receipt.appended[0].seq, EventSeq::new(2));

    let reopened = JsonlEventStore::new(root.clone());
    let events = reopened.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(seqs(&events), vec![0, 1, 2]);
    assert_eq!(events[1].event.event_type.as_str(), EventType::TURN_COMPLETED);
}

/// A read leaves a torn tail alone, deliberately: a reader that arrives while
/// another process is mid-write must wait for the newline rather than call a
/// healthy log corrupt. Only the writer, holding the lease, knows the writer is
/// gone.
#[tokio::test]
async fn a_reader_leaves_a_torn_tail_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sid = SessionId::from_string("s10");
    let store = JsonlEventStore::new(root.clone());
    store.append(&sid, EventSeq::ZERO, vec![draft("s10", EventType::TURN_STARTED)]).await.unwrap();

    append_raw(&root, "s10", b"{\"seq\":1,\"recorded_at\":\"2026-");

    assert_eq!(seqs(&store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap()), vec![0]);

    // Untouched, not repaired: a read is not a position from which to rewrite.
    let bytes = std::fs::read(root.join("s10").join("events.jsonl")).unwrap();
    assert!(bytes.ends_with(b"{\"seq\":1,\"recorded_at\":\"2026-"));
}

/// Append raw bytes to a session's log, as a writer that died mid-write leaves
/// them. No code path of the store can produce this state, which is why the
/// tests reach for the file itself.
fn append_raw(root: &std::path::Path, session: &str, bytes: &[u8]) {
    use std::io::Write;
    let path = root.join(session).join("events.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

fn seqs(events: &[agent_protocol::events::StoredEvent]) -> Vec<u64> {
    events.iter().map(|e| e.seq.as_u64()).collect()
}

#[tokio::test]
async fn subscribes_from_a_seq_inclusively_then_keeps_watching() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("s8");

    store.append(&sid, EventSeq::ZERO, vec![draft("s8", EventType::TURN_STARTED)]).await.unwrap();
    store
        .append(&sid, EventSeq::new(1), vec![draft("s8", EventType::TURN_COMPLETED)])
        .await
        .unwrap();

    // `ZERO` is included. Read as exclusive, as this subscription used to read
    // it, the whole log becomes unaskable: there is no sequence below zero to
    // start after, so the first event of every session went missing.
    let mut sub = store.subscribe(&sid, EventSeq::ZERO).await.unwrap();
    let first = sub.next().await.unwrap().expect("the event at seq 0");
    assert_eq!(first.seq, EventSeq::ZERO);
    let second = sub.next().await.unwrap().expect("the event at seq 1");
    assert_eq!(second.seq, EventSeq::new(1));

    // Caught up is not finished: a later write arrives, and the subscription
    // waits for it rather than ending.
    store
        .append(&sid, EventSeq::new(2), vec![draft("s8", EventType::RUN_COMPLETED)])
        .await
        .unwrap();
    let third = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("a caught-up subscription waits instead of ending")
        .unwrap()
        .expect("the event appended after the subscription began");
    assert_eq!(third.seq, EventSeq::new(2));
}

#[tokio::test]
async fn a_subscription_sees_another_processs_append() {
    let dir = tempfile::tempdir().unwrap();
    // Two stores over one directory is what two processes look like from here:
    // one file, two memories of it. Nothing in this process raises a signal for
    // the other's write, so this passes only if the subscription re-reads the
    // log on its own as well.
    let writer = JsonlEventStore::new(dir.path().to_path_buf());
    let reader = JsonlEventStore::new(dir.path().to_path_buf())
        .with_poll_interval(Duration::from_millis(10));
    let sid = SessionId::from_string("s9");

    writer.append(&sid, EventSeq::ZERO, vec![draft("s9", EventType::TURN_STARTED)]).await.unwrap();
    let mut sub = reader.subscribe(&sid, EventSeq::ZERO).await.unwrap();
    let first = sub.next().await.unwrap().expect("the event that was already there");
    assert_eq!(first.seq, EventSeq::ZERO);

    writer
        .append(&sid, EventSeq::new(1), vec![draft("s9", EventType::TURN_COMPLETED)])
        .await
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("a foreign append arrives without a signal")
        .unwrap()
        .expect("the foreign append");
    assert_eq!(second.seq, EventSeq::new(1));
    assert_eq!(second.event.event_type.as_str(), EventType::TURN_COMPLETED);
}

#[tokio::test]
async fn a_subscription_ends_only_when_the_store_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf())
        .with_poll_interval(Duration::from_millis(10));
    let sid = SessionId::from_string("s10");
    let mut sub = store.subscribe(&sid, EventSeq::ZERO).await.unwrap();

    // Quiet is not closed: this has to time out rather than report an end.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sub.next()).await.is_err(),
        "an idle log is not the end of a subscription"
    );

    drop(store);
    assert!(sub.next().await.unwrap().is_none(), "the store is gone, so the subscription ends");
}

#[tokio::test]
async fn rejects_unsafe_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("../escape");

    let err = store
        .append(&sid, EventSeq::ZERO, vec![draft("../escape", EventType::TURN_STARTED)])
        .await
        .unwrap_err();
    match err {
        EventStoreError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
        other => panic!("expected Io(InvalidInput), got {other:?}"),
    }
}

#[tokio::test]
async fn delete_removes_the_directory_and_a_reused_id_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlEventStore::new(dir.path().to_path_buf());
    let sid = SessionId::from_string("doomed");

    store
        .append(
            &sid,
            EventSeq::ZERO,
            vec![draft("doomed", EventType::TURN_STARTED), draft("doomed", EventType::RUN_STARTED)],
        )
        .await
        .unwrap();
    assert!(dir.path().join("doomed").join("events.jsonl").is_file());

    store.delete_session(&sid).await.unwrap();
    assert!(!dir.path().join("doomed").exists(), "the session directory is gone");

    // The same id, written again, is a new log: the cache that held the old one
    // went with it, so seq starts at zero instead of continuing a dead history.
    store
        .append(&sid, EventSeq::ZERO, vec![draft("doomed", EventType::TURN_STARTED)])
        .await
        .unwrap();
    let events = store.read_stream(&sid, EventSeq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq, EventSeq::ZERO);

    // Deleting twice, or deleting a session that never existed, is not an error:
    // what was asked for is that it not be there.
    store.delete_session(&sid).await.unwrap();
    store.delete_session(&SessionId::from_string("never-written")).await.unwrap();
}
