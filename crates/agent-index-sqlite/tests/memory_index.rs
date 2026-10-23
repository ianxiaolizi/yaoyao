//! Contract tests for the SQLite memory index, driven through a real
//! `JsonlEventStore` and a real data directory.
//!
//! The index is only meaningful relative to logs it did not author, so these
//! tests never hand it a fake store: they append real events, then ask what a
//! person searching a long conversation would find.

use std::sync::Arc;

use agent_index_sqlite::SqliteMemoryIndex;
use agent_ports::event_store::EventStore;
use agent_ports::memory_index::{MemoryHit, MemoryIndex, MemoryIndexError, MemoryQuery};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, RunId, SchemaVersion, SessionId, TurnId};
use agent_store_jsonl::JsonlEventStore;
use chrono::Utc;
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<JsonlEventStore>,
    index: SqliteMemoryIndex,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp data dir");
    let store = Arc::new(JsonlEventStore::new(dir.path()));
    let index = SqliteMemoryIndex::open(store.clone(), dir.path());
    Fixture { dir, store, index }
}

fn draft(session: &SessionId, event_type: &str, payload: Value) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: session.clone(),
        run_id: Some(RunId::new()),
        turn_id: Some(TurnId::from_string("t1")),
        step_id: None,
        actor: Actor::System,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: json!({}),
        ignorable: false,
    }
}

/// Append one event and return the sequence the store gave it.
async fn append(
    store: &JsonlEventStore,
    session: &SessionId,
    event_type: &str,
    payload: Value,
) -> u64 {
    let log = store.read_stream(session, EventSeq::ZERO, usize::MAX).await.expect("read the log");
    let next = log.last().map(|event| event.seq.as_u64() + 1).unwrap_or(0);
    let receipt = store
        .append(session, EventSeq::new(next), vec![draft(session, event_type, payload)])
        .await
        .expect("append");
    receipt.appended[0].seq.as_u64()
}

async fn search(
    fixture: &Fixture,
    session: &SessionId,
    text: &str,
    limit: usize,
) -> Vec<MemoryHit> {
    fixture
        .index
        .search(&MemoryQuery { session_id: session.clone(), text: text.to_string(), limit })
        .await
        .expect("search")
}

fn session(name: &str) -> SessionId {
    SessionId::from_string(name)
}

#[tokio::test]
async fn the_first_event_is_indexed_too() {
    // The log's first event is seq 0, so an index mark that meant "last
    // indexed" could not say "nothing indexed yet" without skipping it.
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "first words"}}),
    )
    .await;

    let hits = search(&fixture, &sid, "first", 10).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].seq, EventSeq::new(0));
    assert_eq!(hits[0].event_type, EventType::INBOX_APPENDED);
    assert_eq!(hits[0].turn_id, Some(TurnId::from_string("t1")));
    assert_eq!(hits[0].step_id, None);
}

#[tokio::test]
async fn search_finds_turn_text_assistant_text_and_tool_output() {
    let fixture = fixture();
    let sid = session("s1");

    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"kind": "next_turn", "payload": {"text": "remember the word xylophone"}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::MODEL_RESPONSE_COMPLETED,
        json!({"response": {"content_blocks": [
            {"type": "text", "text": "I have noted the xylophone."}
        ]}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::TOOL_COMPLETED,
        json!({"tool_name": "fs.read", "content": "a xylophone in the file"}),
    )
    .await;

    let hits = search(&fixture, &sid, "xylophone", 10).await;
    let types: Vec<&str> = hits.iter().map(|hit| hit.event_type.as_str()).collect();
    assert_eq!(
        types,
        [EventType::TOOL_COMPLETED, EventType::MODEL_RESPONSE_COMPLETED, EventType::INBOX_APPENDED]
    );
    let seqs: Vec<u64> = hits.iter().map(|hit| hit.seq.as_u64()).collect();
    assert_eq!(seqs, [2, 1, 0], "newest first");
    assert!(hits[2].snippet.contains("xylophone"));
}

#[tokio::test]
async fn search_folds_ascii_case_only() {
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "XYLOPHONE"}}),
    )
    .await;

    assert_eq!(search(&fixture, &sid, "xylophone", 10).await.len(), 1);
    assert_eq!(search(&fixture, &sid, "XyLoPhOnE", 10).await.len(), 1);
}

#[tokio::test]
async fn a_second_search_indexes_nothing_again() {
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "needle one"}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "needle two"}}),
    )
    .await;

    assert_eq!(search(&fixture, &sid, "needle", 10).await.len(), 2);
    // A second search re-reads the mark, not the whole log into the table.
    assert_eq!(search(&fixture, &sid, "needle", 10).await.len(), 2);

    // Events appended between searches are picked up; rows already indexed are
    // replaced rather than duplicated.
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "needle three"}}),
    )
    .await;
    assert_eq!(search(&fixture, &sid, "needle", 10).await.len(), 3);

    // The entry count is the number of indexed events, not the number of syncs
    // or searches that ran.
    assert_eq!(fixture.index.rebuild(&sid).await.expect("rebuild"), 3);
}

#[tokio::test]
async fn rebuild_returns_the_same_hits() {
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "alpha words"}}),
    )
    .await;
    append(&fixture.store, &sid, EventType::TURN_STEERED, json!({"text": "beta words"})).await;

    let before = search(&fixture, &sid, "words", 10).await;
    let count = fixture.index.rebuild(&sid).await.expect("rebuild");
    let after = search(&fixture, &sid, "words", 10).await;

    assert_eq!(count, 2);
    assert_eq!(after, before, "a rebuild is the same index over the same log");
}

#[tokio::test]
async fn a_payload_the_extraction_cannot_read_is_skipped_not_fatal() {
    let fixture = fixture();
    let sid = session("s1");
    append(&fixture.store, &sid, EventType::INBOX_APPENDED, json!({"payload": {"text": 42}})).await;
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "a good needle"}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::MODEL_RESPONSE_COMPLETED,
        json!({"response": {"content_blocks": "not blocks"}}),
    )
    .await;

    let hits = search(&fixture, &sid, "needle", 10).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].seq.as_u64(), 1);
    // The unreadable payloads still advance the mark: they are not re-read
    // forever, and they contribute no rows.
    assert_eq!(fixture.index.rebuild(&sid).await.expect("rebuild"), 1);
}

#[tokio::test]
async fn a_version_mismatch_is_refused_and_rebuild_recovers() {
    let fixture = fixture();
    let sid = session("s1");
    append(&fixture.store, &sid, EventType::INBOX_APPENDED, json!({"payload": {"text": "needle"}}))
        .await;
    // Build an index, then make it look like another build wrote it.
    assert_eq!(search(&fixture, &sid, "needle", 10).await.len(), 1);
    let db = fixture.dir.path().join(sid.as_str()).join("memory.sqlite");
    {
        let conn = rusqlite::Connection::open(&db).expect("open the derived database");
        conn.execute(
            "UPDATE index_meta SET schema_version = 'memory-index/0.0.1' WHERE id = 1",
            [],
        )
        .expect("age the index");
    }

    let refused = fixture
        .index
        .search(&MemoryQuery { session_id: sid.clone(), text: "needle".to_string(), limit: 10 })
        .await
        .expect_err("a version this build does not know is refused");
    assert!(matches!(
        &refused,
        MemoryIndexError::Version { found, expected }
            if found == "memory-index/0.0.1" && expected == agent_index_sqlite::INDEX_SCHEMA_VERSION
    ));

    // Only an explicit rebuild discards the file; a search never silently
    // rebuilds, because that would hide the upgrade from its caller.
    assert_eq!(fixture.index.rebuild(&sid).await.expect("rebuild"), 1);
    assert_eq!(search(&fixture, &sid, "needle", 10).await.len(), 1);
}

#[tokio::test]
async fn limit_keeps_the_newest_matches() {
    let fixture = fixture();
    let sid = session("s1");
    for index in 0..5 {
        append(
            &fixture.store,
            &sid,
            EventType::INBOX_APPENDED,
            json!({"payload": {"text": format!("limit word {index}")}}),
        )
        .await;
    }

    let hits = search(&fixture, &sid, "limit word", 2).await;
    let seqs: Vec<u64> = hits.iter().map(|hit| hit.seq.as_u64()).collect();
    assert_eq!(seqs, [4, 3]);
}

#[tokio::test]
async fn sessions_do_not_see_each_other() {
    let fixture = fixture();
    let first = session("s1");
    let second = session("s2");
    append(
        &fixture.store,
        &first,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "shared word"}}),
    )
    .await;
    append(
        &fixture.store,
        &second,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "shared word"}}),
    )
    .await;

    let hits = search(&fixture, &first, "shared word", 10).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].seq, EventSeq::new(0));
    assert_eq!(search(&fixture, &second, "shared word", 10).await.len(), 1);
}

#[tokio::test]
async fn chinese_text_matches_by_substring_and_the_snippet_is_cut_on_characters() {
    let fixture = fixture();
    let sid = session("s1");
    // Well past SNIPPET_CHARS on both sides, so a byte-slice snippet would cut
    // a multi-byte character in half.
    let text = format!("{}青花瓷{}", "很长的前缀".repeat(40), "很长的后缀".repeat(40));
    append(&fixture.store, &sid, EventType::INBOX_APPENDED, json!({"payload": {"text": text}}))
        .await;

    let hits = search(&fixture, &sid, "青花瓷", 10).await;
    assert_eq!(hits.len(), 1);
    let snippet = &hits[0].snippet;
    assert!(snippet.contains("青花瓷"), "the match is shown: {snippet}");
    assert!(text.contains(snippet), "the snippet is a substring, not a re-encoding");
    assert!(!snippet.contains('\u{fffd}'), "no character was cut in half");
    assert!(
        snippet.chars().count() <= 80 + 3 + 80,
        "at most the window on each side: {} chars",
        snippet.chars().count()
    );
}

#[tokio::test]
async fn an_empty_search_is_refused() {
    let fixture = fixture();
    let sid = session("s1");
    let refused = fixture
        .index
        .search(&MemoryQuery { session_id: sid, text: "   ".to_string(), limit: 10 })
        .await
        .expect_err("blank text has nothing to look for");
    assert!(matches!(refused, MemoryIndexError::Index(_)));
}
