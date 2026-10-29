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
    let mut found: Vec<&str> = hits.iter().map(|hit| hit.event_type.as_str()).collect();
    found.sort_unstable();
    let mut expected = vec![
        EventType::INBOX_APPENDED,
        EventType::MODEL_RESPONSE_COMPLETED,
        EventType::TOOL_COMPLETED,
    ];
    expected.sort_unstable();
    assert_eq!(found, expected, "{hits:?}");

    // Relevance order, not log order — and whichever order the ranking picks,
    // it never puts a hit above one it scored higher.
    let scores: Vec<f64> = hits.iter().map(|hit| hit.score).collect();
    assert!(scores.windows(2).all(|pair| pair[0] >= pair[1]), "scores descend: {scores:?}");
    assert!(hits.iter().any(|hit| hit.snippet.contains("xylophone")), "{hits:?}");
}

#[tokio::test]
async fn search_folds_case_across_all_of_unicode() {
    // The trigram tokenizer folds case itself, and it folds every script, not
    // just ASCII — so a Latin word and a Cyrillic one are both found by a
    // lower-case query. This is a v2 widening: the fallback scan below still
    // uses `lower()`, which is ASCII-only, so only a short query has the old
    // limit.
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "XYLOPHONE"}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "ПРИВЕТ мир"}}),
    )
    .await;

    assert_eq!(search(&fixture, &sid, "xylophone", 10).await.len(), 1);
    assert_eq!(search(&fixture, &sid, "XyLoPhOnE", 10).await.len(), 1);
    assert_eq!(search(&fixture, &sid, "привет", 10).await.len(), 1);
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
async fn limit_keeps_the_best_scoring_matches() {
    // Five events that say the same thing differ only in where they are, so
    // they tie on every part of the score except recency and the limit keeps
    // the latest two — which is also what "newest first" used to mean.
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

// ---------------------------------------------------------------------------
// Ranking: what "most relevant first" is made of.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_denser_match_ranks_above_a_longer_one() {
    let fixture = fixture();
    let sid = session("s1");
    // The dense event is older and shorter; the sparse one is a long aside.
    // Density has to outrank recency here, or the ordering is just the log
    // turned inside out.
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "widget widget widget"}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text":
            "a widget inside a much longer handbook of assorted tools and notes"}}),
    )
    .await;
    for index in 0..4 {
        append(
            &fixture.store,
            &sid,
            EventType::INBOX_APPENDED,
            json!({"payload": {"text": format!("unrelated filler sentence number {index}")}}),
        )
        .await;
    }

    let hits = search(&fixture, &sid, "widget", 10).await;
    let seqs: Vec<u64> = hits.iter().map(|hit| hit.seq.as_u64()).collect();
    assert_eq!(seqs, [0, 1], "the denser, older match comes first: {hits:?}");
}

#[tokio::test]
async fn identical_matches_are_broken_by_recency() {
    let fixture = fixture();
    let sid = session("s1");
    for _ in 0..3 {
        append(
            &fixture.store,
            &sid,
            EventType::INBOX_APPENDED,
            json!({"payload": {"text": "the same words every time"}}),
        )
        .await;
    }

    let hits = search(&fixture, &sid, "same words", 10).await;
    let seqs: Vec<u64> = hits.iter().map(|hit| hit.seq.as_u64()).collect();
    assert_eq!(seqs, [2, 1, 0], "nothing separates them but position: {hits:?}");
}

#[tokio::test]
async fn the_persons_own_words_outrank_a_reply_that_says_the_same_thing() {
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "deploy the artifact"}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::MODEL_RESPONSE_COMPLETED,
        json!({"response": {"content_blocks": [{"type": "text", "text": "deploy the artifact"}]}}),
    )
    .await;
    for index in 0..6 {
        append(
            &fixture.store,
            &sid,
            EventType::TOOL_COMPLETED,
            json!({"tool_name": "fs.list", "content": format!("filler {index}")}),
        )
        .await;
    }

    let hits = search(&fixture, &sid, "deploy the artifact", 10).await;
    assert_eq!(hits.len(), 2, "{hits:?}");
    // The reply is the later of the two, so recency pulls the other way; what
    // kind of event it is outweighs a couple of log positions.
    assert_eq!(hits[0].event_type, EventType::INBOX_APPENDED, "{hits:?}");
    assert_eq!(hits[1].seq.as_u64(), 1, "{hits:?}");
}

// ---------------------------------------------------------------------------
// The two search paths.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_query_with_quotes_is_a_phrase_not_syntax() {
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "she said \"deploy now\" twice"}}),
    )
    .await;

    // Balanced quotes and a lone one: both are part of the phrase, and neither
    // can end it early or leave the index's query syntax unclosed.
    assert_eq!(search(&fixture, &sid, "said \"deploy now\"", 10).await.len(), 1);
    assert_eq!(search(&fixture, &sid, "\"deploy now", 10).await.len(), 1);
}

#[tokio::test]
async fn a_two_character_query_falls_back_to_a_substring_scan() {
    // Two Chinese characters are the everyday query and are not a trigram, so
    // this is answered by the scan rather than the index. What must not happen
    // is that the query silently matches nothing.
    let fixture = fixture();
    let sid = session("s1");
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "今天记账买了咖啡"}}),
    )
    .await;
    append(
        &fixture.store,
        &sid,
        EventType::INBOX_APPENDED,
        json!({"payload": {"text": "记错了，明天再看"}}),
    )
    .await;

    let hits = search(&fixture, &sid, "记账", 10).await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].seq.as_u64(), 0);
    assert!(hits[0].snippet.contains("记账"), "{}", hits[0].snippet);
}

// ---------------------------------------------------------------------------
// The evaluation baseline.
// ---------------------------------------------------------------------------

/// The corpus's text for one event, shaped the way the extraction reads it.
fn payload_for(event_type: &str, text: &str) -> Value {
    match event_type {
        EventType::MODEL_RESPONSE_COMPLETED => {
            json!({"response": {"content_blocks": [{"type": "text", "text": text}]}})
        }
        EventType::TOOL_COMPLETED => json!({"tool_name": "shell", "content": text}),
        _ => json!({"payload": {"text": text}}),
    }
}

/// A fixed corpus and a fixed set of labels: retrieval quality measured, not
/// asserted one anecdote at a time.
///
/// The corpus is synthetic and lives here so that the baseline only moves when
/// someone edits it deliberately. A ranking change that costs a labeled hit
/// fails here rather than being discovered in use. A label set taken from real
/// sessions is the owner's to provide — until then this is the honest floor,
/// and the thresholds leave room for a small weight tweak without excusing a
/// retriever that has stopped finding things.
#[tokio::test]
async fn a_fixed_corpus_keeps_its_retrieval_quality() {
    const K: usize = 5;

    let fixture = fixture();
    let sid = session("s1");
    let corpus = [
        (EventType::INBOX_APPENDED, "帮我看看 memory 索引为什么搜不到中文"),
        (EventType::MODEL_RESPONSE_COMPLETED, "trigram tokenizer 需要三个字符以上才能命中"),
        (EventType::TOOL_COMPLETED, "INDEX_SCHEMA_VERSION memory-index/0.1.0"),
        (EventType::INBOX_APPENDED, "把索引改成 0.2.0 并且加上 bm25 排序"),
        (EventType::MODEL_RESPONSE_COMPLETED, "已经改成 memory-index/0.2.0，bm25 排序也加上了"),
        (EventType::TOOL_COMPLETED, "test.ps1: 1656 passed; 0 failed; 15 ignored"),
        (EventType::INBOX_APPENDED, "CI 上 gui-smoke 挂了怎么处理"),
        (EventType::MODEL_RESPONSE_COMPLETED, "gui-smoke 失败是因为 runner 的完整性标签"),
        (EventType::TOOL_COMPLETED, "nothing to commit, working tree clean"),
        (EventType::INBOX_APPENDED, "把这次改动提交并推送到 main"),
        (EventType::TOOL_COMPLETED, "pushed main to origin"),
        (EventType::INBOX_APPENDED, "继续下一项审计"),
    ];
    for (event_type, text) in corpus {
        append(&fixture.store, &sid, event_type, payload_for(event_type, text)).await;
    }

    // The seq each query should find, by position in `corpus`.
    let labels = [
        ("working tree clean", 8),
        ("完整性标签", 7),
        ("0.1.0", 2),
        ("1656 passed", 5),
        ("bm25", 3),
    ];

    let mut found = 0;
    let mut reciprocal_sum = 0.0;
    let mut report = Vec::new();
    for (query, expected) in labels {
        let hits = search(&fixture, &sid, query, K).await;
        let seqs: Vec<u64> = hits.iter().map(|hit| hit.seq.as_u64()).collect();
        let rank = seqs.iter().position(|seq| *seq == expected).map(|at| at + 1);
        report.push(format!("'{query}' -> {seqs:?}, wanted {expected}"));
        if let Some(rank) = rank {
            found += 1;
            reciprocal_sum += 1.0 / rank as f64;
        }
    }

    let total = labels.len() as f64;
    let recall = found as f64 / total;
    let mrr = reciprocal_sum / total;
    assert!(recall >= 1.0, "recall@{K} fell to {recall}: {report:#?}");
    assert!(mrr >= 0.9, "MRR fell to {mrr}: {report:#?}");
}
