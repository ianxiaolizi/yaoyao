//! What the store promises: a fact is written with its provenance, a key has
//! one answer at a time, replacing is explicit, and forgetting is deletion.
//!
//! Every call carries its own `now`. That is the port's shape (C7 #20) and it
//! is also what makes these tests deterministic: "has this expired" is asked
//! at an instant this file chooses, not at whatever the machine's clock says
//! while the suite runs.

use std::path::PathBuf;

use agent_memory_sqlite::{MEMORY_STORE_VERSION, SqliteMemoryStore};
use agent_ports::memory_store::{
    DEFAULT_CONFIDENCE, DEFAULT_NAMESPACE, ListFacts, MemoryOrigin, MemorySensitivity,
    MemorySource, MemoryStatus, MemoryStore, MemoryStoreError, Recall, Remember,
};
use agent_protocol::ids::SessionId;
use chrono::{DateTime, Duration, Utc};
use tempfile::TempDir;

/// A store over a throwaway data directory, reachable at its real path so a
/// test can also look at the file the way an operator would.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self { dir: TempDir::new().expect("temp dir") }
    }

    fn store(&self) -> SqliteMemoryStore {
        SqliteMemoryStore::open(self.dir.path())
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("memory.db")
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

/// A fact a person typed: no key, no source, the defaults.
fn typed(text: &str) -> Remember {
    Remember {
        namespace: None,
        key: None,
        text: text.to_string(),
        confidence: None,
        origin: MemoryOrigin::User,
        source: None,
        sensitivity: MemorySensitivity::Normal,
        supersedes: None,
        expires_at: None,
    }
}

/// A fact the model wrote, carrying the session and call it came from.
fn from_agent(session: &SessionId, call: &str, text: &str) -> Remember {
    Remember {
        origin: MemoryOrigin::Agent,
        source: Some(MemorySource { session: session.clone(), call: Some(call.to_string()) }),
        ..typed(text)
    }
}

fn keyed(mut request: Remember, key: &str) -> Remember {
    request.key = Some(key.to_string());
    request
}

#[tokio::test]
async fn a_fact_keeps_its_source_author_and_defaults() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let session = SessionId::from_string("s-1");
    let at = now();

    let fact = store
        .remember(from_agent(&session, "call-9", "  prefers Chinese  "), at)
        .await
        .expect("remembered");

    assert_eq!(fact.id.len(), 4 + 36, "an id is mem_ and a plain uuid: {}", fact.id);
    assert_eq!(fact.text, "prefers Chinese", "the edges are trimmed");
    assert_eq!(fact.namespace, DEFAULT_NAMESPACE);
    assert_eq!(fact.confidence, DEFAULT_CONFIDENCE);
    assert_eq!(fact.origin, MemoryOrigin::Agent);
    assert_eq!(fact.status, MemoryStatus::Active);
    assert_eq!(fact.source, Some(MemorySource { session, call: Some("call-9".to_string()) }));
    assert_eq!(fact.created_at, at, "the caller's instant is the stamp");
    assert_eq!(fact.updated_at, fact.created_at, "nothing has happened to it yet");
    assert!(fact.expires_at.is_none(), "no expiry means until forgotten");
    assert!(fact.supersedes.is_none());
}

#[tokio::test]
async fn a_second_fact_for_an_active_key_is_refused_and_names_the_one_to_supersede() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    let first = store
        .remember(keyed(typed("uses tabs"), "indent"), at)
        .await
        .expect("the first answer to the key");

    let error = store.remember(keyed(typed("uses spaces"), "indent"), at).await.unwrap_err();

    assert_eq!(
        error,
        MemoryStoreError::Conflict {
            namespace: DEFAULT_NAMESPACE.to_string(),
            key: "indent".to_string(),
            id: first.id.clone(),
        }
    );
    assert!(error.to_string().contains(&first.id), "the refusal names what to supersede: {error}");
    let facts = store.list(&ListFacts::default(), at).await.expect("list");
    assert_eq!(facts.len(), 1, "the refused write left nothing behind");
    assert_eq!(facts[0].text, "uses tabs");
}

#[tokio::test]
async fn superseding_retires_the_old_fact_and_recall_returns_only_the_current_one() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    let old = store.remember(keyed(typed("uses tabs"), "indent"), at).await.expect("first");
    let mut request = keyed(typed("uses spaces"), "indent");
    request.supersedes = Some(old.id.clone());

    let new = store.remember(request, at + Duration::seconds(5)).await.expect("the replacement");

    assert_eq!(new.supersedes.as_deref(), Some(old.id.as_str()));
    let current =
        store.recall(&Recall { key: Some("indent".into()), ..Recall::default() }, at).await;
    let current = current.expect("recall");
    assert_eq!(current.len(), 1, "the current answer only");
    assert_eq!(current[0].id, new.id);

    let history = store
        .recall(
            &Recall { key: Some("indent".into()), include_superseded: true, ..Recall::default() },
            at,
        )
        .await
        .expect("recall with history");
    assert_eq!(history.len(), 2, "the replaced fact is kept, not overwritten");
    let retired = history.iter().find(|fact| fact.id == old.id).expect("the old fact");
    assert_eq!(retired.status, MemoryStatus::Superseded);
    assert!(retired.updated_at > retired.created_at, "retiring is what moved updated_at");
}

#[tokio::test]
async fn a_supersede_that_names_nothing_is_refused() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let mut request = typed("uses spaces");
    request.supersedes = Some("mem_does-not-exist".to_string());

    let error = store.remember(request, now()).await.unwrap_err();

    assert!(matches!(error, MemoryStoreError::Invalid(ref message) if message.contains("no fact")));
}

#[tokio::test]
async fn a_replacement_that_drops_the_key_is_refused() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    let old = store.remember(keyed(typed("uses tabs"), "indent"), at).await.expect("first");
    let mut request = typed("uses spaces");
    request.supersedes = Some(old.id.clone());

    let error = store.remember(request, at).await.unwrap_err();

    assert!(
        matches!(error, MemoryStoreError::Invalid(ref message) if message.contains("same key")),
        "{error}"
    );
}

#[tokio::test]
async fn a_replacement_in_another_namespace_is_refused() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    let old = store.remember(keyed(typed("uses tabs"), "indent"), at).await.expect("first");
    let mut request = keyed(typed("uses spaces"), "indent");
    request.namespace = Some("other".to_string());
    request.supersedes = Some(old.id.clone());

    let error = store.remember(request, at).await.unwrap_err();

    assert!(
        matches!(error, MemoryStoreError::Invalid(ref message) if message.contains("namespace")),
        "{error}"
    );
}

#[tokio::test]
async fn forgetting_deletes_the_row_for_good() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    let fact = store.remember(typed("a server password hint"), at).await.expect("remembered");

    assert!(store.forget(&fact.id).await.expect("forget"));
    assert!(
        store.recall(&Recall::default(), at).await.expect("recall").is_empty(),
        "a forgotten fact is not recalled"
    );
    assert!(
        store
            .list(
                &ListFacts {
                    include_superseded: true,
                    include_expired: true,
                    ..Default::default()
                },
                at
            )
            .await
            .expect("list")
            .is_empty(),
        "not even history keeps it"
    );
    assert!(!store.forget(&fact.id).await.expect("forget again"), "forgetting twice is an answer");

    // The table itself no longer holds the sentence: "delete" is not a flag.
    let conn = rusqlite::Connection::open(fixture.db()).expect("the database on disk");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM facts WHERE instr(text, 'a server password hint') > 0",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(rows, 0, "the text is gone from the table, not merely hidden from reads");
}

#[tokio::test]
async fn an_expired_fact_is_not_recalled_unless_asked_for() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    let mut request = typed("the deploy window is tuesday");
    request.expires_at = Some(at + Duration::hours(1));
    store.remember(request, at).await.expect("remembered while it still holds");

    assert_eq!(
        store.recall(&Recall::default(), at).await.expect("recall").len(),
        1,
        "an expiry in the future does not hide the fact"
    );

    let later = at + Duration::hours(2);
    assert!(
        store.recall(&Recall::default(), later).await.expect("recall").is_empty(),
        "asked after the expiry, it is not the answer any more"
    );
    let history = store
        .recall(&Recall { include_expired: true, ..Recall::default() }, later)
        .await
        .expect("recall with expired");
    assert_eq!(history.len(), 1, "it is kept until forgotten");
}

#[tokio::test]
async fn an_expiry_that_has_already_passed_is_refused() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    let mut request = typed("a fact for yesterday");
    request.expires_at = Some(at - Duration::seconds(1));

    let error = store.remember(request, at).await.unwrap_err();

    assert!(matches!(error, MemoryStoreError::Invalid(ref message) if message.contains("expiry")));
}

#[tokio::test]
async fn facts_without_a_key_never_collide() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    store.remember(typed("likes tea"), at).await.expect("first");
    store.remember(typed("likes tea"), at).await.expect("second");

    assert_eq!(store.recall(&Recall::default(), at).await.expect("recall").len(), 2);
}

#[tokio::test]
async fn a_fact_needs_text_and_a_confidence_that_means_something() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();

    assert!(matches!(
        store.remember(typed("   "), at).await.unwrap_err(),
        MemoryStoreError::Invalid(_)
    ));

    let mut too_sure = typed("nothing is this certain");
    too_sure.confidence = Some(7.0);
    assert!(matches!(
        store.remember(too_sure, at).await.unwrap_err(),
        MemoryStoreError::Invalid(_)
    ));

    let mut empty_key = typed("a key of nothing");
    empty_key.key = Some("  ".to_string());
    assert!(matches!(
        store.remember(empty_key, at).await.unwrap_err(),
        MemoryStoreError::Invalid(_)
    ));
}

#[tokio::test]
async fn recall_matches_text_without_caring_about_ascii_case() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    store.remember(typed("Deploys go out on Friday"), at).await.expect("first");
    store.remember(typed("the staging host is blue"), at).await.expect("second");

    let hits = store.recall(&Recall { text: Some("FRIDAY".into()), ..Recall::default() }, at).await;
    let hits = hits.expect("recall");

    assert_eq!(hits.len(), 1, "the match folds ASCII case: {}", hits.len());
    assert_eq!(hits[0].text, "Deploys go out on Friday");
}

#[tokio::test]
async fn recall_returns_the_newest_first_and_honours_the_limit() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let at = now();
    for (index, text) in ["first", "second", "third"].into_iter().enumerate() {
        let when = at + Duration::seconds(index as i64);
        store.remember(typed(text), when).await.expect("remembered");
    }

    let hits = store.recall(&Recall { limit: 2, ..Recall::default() }, at).await.expect("recall");

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].text, "third");
    assert_eq!(hits[1].text, "second");
}

#[tokio::test]
async fn a_private_fact_keeps_its_sensitivity() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let mut request = typed("their anniversary is in May");
    request.sensitivity = MemorySensitivity::Private;

    let fact = store.remember(request, now()).await.expect("remembered");

    assert_eq!(fact.sensitivity, MemorySensitivity::Private);
}

#[tokio::test]
async fn the_store_survives_reopening() {
    let fixture = Fixture::new();
    let at = now();
    fixture.store().remember(typed("survives"), at).await.expect("remembered");

    let facts = fixture.store().recall(&Recall::default(), at).await.expect("recall after reopen");

    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].text, "survives");
}

#[tokio::test]
async fn a_store_from_another_schema_is_refused_and_says_to_export() {
    let fixture = Fixture::new();
    let at = now();
    fixture.store().remember(typed("written by this build"), at).await.expect("remembered");
    {
        let conn = rusqlite::Connection::open(fixture.db()).expect("the database on disk");
        conn.execute("UPDATE store_meta SET schema_version = 'memory-store/9.9.9'", [])
            .expect("the version is rewritten");
    }

    let error = fixture.store().recall(&Recall::default(), at).await.unwrap_err();

    assert!(
        matches!(error, MemoryStoreError::Version { ref found, .. } if found == "memory-store/9.9.9"),
        "{error}"
    );
    assert!(error.to_string().contains("export"), "the refusal names the way out: {error}");
    assert_eq!(MEMORY_STORE_VERSION, "memory-store/0.1.0", "the version this build writes");
}
