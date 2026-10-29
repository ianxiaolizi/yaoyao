//! The SQLite-backed [`MemoryIndex`] adapter.
//!
//! One database per session, next to that session's `events.jsonl`. Keeping it
//! per session means a search can never cross a conversation boundary, and a
//! session's index can be deleted without touching another's.
//!
//! Every blocking operation runs on the blocking pool: rusqlite is synchronous,
//! and a search called from the async edge must not stall the runtime. Each
//! operation opens its own connection with a busy timeout and WAL journaling,
//! so a rebuild racing a search waits rather than corrupting.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_ports::event_store::{EventStore, is_safe_dir_name};
use agent_ports::memory_index::{MemoryHit, MemoryIndex, MemoryIndexError, MemoryQuery};
use agent_protocol::ids::{EventSeq, SessionId, StepId, TurnId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::extract::extract;

/// The shape of the derived index. Not the event schema version (C6.1): this
/// version may change whenever the extraction or the tables change, and the
/// answer to a mismatch is always `rebuild`.
pub const INDEX_SCHEMA_VERSION: &str = "memory-index/0.2.0";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many characters of context the snippet shows on each side of the match.
const SNIPPET_CHARS: usize = 80;

/// The shortest query the trigram index can answer. Below this the tokenizer has
/// no trigram to look up — a two-character Chinese word is the everyday case —
/// so the search falls back to scanning `text` for the substring.
const MIN_INDEXED_CHARS: usize = 3;

/// How much a hit's term rarity counts. `bm25()` is the index's own answer to
/// "how unusual is this match in this session", negated so that bigger is
/// better; it lands in roughly `0..20` for a session-sized log.
const BM25_WEIGHT: f64 = 1.0;

/// How much being near the end of the log counts, on a `0..1` scale before the
/// weight. Recency is a tiebreaker and a nudger, never a reason on its own: a
/// stale hit that is a much better match should still win.
const RECENCY_WEIGHT: f64 = 0.5;

/// What kind of event it is, added to the score.
///
/// The person's own words are the best thing to find again; the assistant's
/// reply is context around them; tool output is where a search usually starts
/// from and is weighted by nothing extra, because a match there is already
/// specific.
const TYPE_WEIGHT_INPUT: f64 = 0.3;
const TYPE_WEIGHT_REPLY: f64 = 0.1;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS index_meta (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        schema_version TEXT NOT NULL,
        next_seq INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS entries (
        seq INTEGER PRIMARY KEY,
        event_type TEXT NOT NULL,
        turn_id TEXT,
        step_id TEXT,
        text TEXT NOT NULL
    );
    -- External content: the text is stored once, in `entries`. `trigram` is the
    -- tokenizer that needs no word boundaries, which is what makes Chinese and
    -- code identifiers indexable without a segmentation step.
    CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
        text,
        content='entries',
        content_rowid='seq',
        tokenize='trigram'
    );
";

/// A [`MemoryIndex`] over one data directory.
pub struct SqliteMemoryIndex {
    store: Arc<dyn EventStore>,
    data_dir: PathBuf,
}

impl SqliteMemoryIndex {
    /// Build an index over `store`'s sessions, keeping its databases under
    /// `data_dir` (the same root the event logs live under).
    pub fn open(store: Arc<dyn EventStore>, data_dir: impl Into<PathBuf>) -> Self {
        Self { store, data_dir: data_dir.into() }
    }

    fn db_path(&self, session_id: &SessionId) -> Result<PathBuf, MemoryIndexError> {
        // The same judgement the store and the projection make: a session id
        // names a directory segment, and one that can escape it must not be
        // able to make this crate create files outside the data directory.
        if !is_safe_dir_name(session_id.as_str()) {
            return Err(MemoryIndexError::Index(format!(
                "session id '{}' cannot name a directory",
                session_id.as_str()
            )));
        }
        Ok(self.data_dir.join(session_id.as_str()).join("memory.sqlite"))
    }

    /// Bring the index up to the log's current end.
    ///
    /// Two steps, because one of them is async: read the mark, ask the store
    /// for everything after it, then write the rows and the new mark in one
    /// transaction. Events with no indexable text still advance the mark —
    /// otherwise the next search would read them again forever.
    async fn sync(&self, session_id: &SessionId) -> Result<(), MemoryIndexError> {
        let path = self.db_path(session_id)?;
        let next = {
            let path = path.clone();
            blocking(move || ready(&path, false).and_then(|conn| current_seq(&conn))).await?
        };

        // The mark is the *next* sequence expected, not the last one indexed:
        // the log's first event is seq 0, so a mark that meant "last indexed"
        // would have no value to mean "nothing indexed yet". `read_stream` is
        // inclusive of its `from`, which is exactly the first unindexed event.
        let events = self
            .store
            .read_stream(session_id, EventSeq::new(next), usize::MAX)
            .await
            .map_err(|e| MemoryIndexError::Store(e.to_string()))?;

        let fresh: Vec<_> = events.into_iter().filter(|event| event.seq.as_u64() >= next).collect();
        let Some(last) = fresh.last().map(|event| event.seq.as_u64()) else {
            return Ok(());
        };
        let rows: Vec<Row> = fresh
            .iter()
            .filter_map(|event| {
                let extracted = extract(event);
                extracted.text.map(|text| Row {
                    seq: event.seq.as_u64(),
                    event_type: event.event.event_type.as_str().to_string(),
                    turn_id: event.event.turn_id.as_ref().map(|id| id.as_str().to_string()),
                    step_id: event.event.step_id.as_ref().map(|id| id.as_str().to_string()),
                    text,
                })
            })
            .collect();

        blocking(move || write_rows(&path, &rows, last + 1)).await
    }
}

#[async_trait::async_trait]
impl MemoryIndex for SqliteMemoryIndex {
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryHit>, MemoryIndexError> {
        let needle = query.text.trim().to_string();
        if needle.is_empty() {
            return Err(MemoryIndexError::Index("the search text is empty".to_string()));
        }
        let path = self.db_path(&query.session_id)?;
        self.sync(&query.session_id).await?;
        let limit = query.limit;
        blocking(move || search_at(&path, &needle, limit)).await
    }

    async fn rebuild(&self, session_id: &SessionId) -> Result<usize, MemoryIndexError> {
        let path = self.db_path(session_id)?;
        {
            let path = path.clone();
            blocking(move || reset(&path)).await?;
        }
        self.sync(session_id).await?;
        let path = self.db_path(session_id)?;
        blocking(move || {
            let conn = ready(&path, false)?;
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
                    .map_err(|e| MemoryIndexError::Index(format!("cannot count entries: {e}")))?;
            Ok(count as usize)
        })
        .await
    }
}

/// One indexed event, ready to insert.
struct Row {
    seq: u64,
    event_type: String,
    turn_id: Option<String>,
    step_id: Option<String>,
    text: String,
}

/// Run a blocking index operation on the blocking pool.
async fn blocking<T>(
    operation: impl FnOnce() -> Result<T, MemoryIndexError> + Send + 'static,
) -> Result<T, MemoryIndexError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|e| MemoryIndexError::Index(format!("index task failed: {e}")))?
}

/// Open the database, creating the schema, and check its version.
///
/// `reset` drops the tables first, which is how `rebuild` starts over without
/// trusting anything the old file says.
fn ready(path: &Path, reset: bool) -> Result<Connection, MemoryIndexError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            MemoryIndexError::Index(format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    let conn = Connection::open(path)
        .map_err(|e| MemoryIndexError::Index(format!("cannot open {}: {e}", path.display())))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| MemoryIndexError::Index(format!("cannot set the busy timeout: {e}")))?;
    // WAL lets a search read while a rebuild writes; the returned mode is not
    // needed, only the setting.
    conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0))
        .map_err(|e| MemoryIndexError::Index(format!("cannot set WAL: {e}")))?;
    if reset {
        conn.execute_batch(
            "DROP TABLE IF EXISTS entries_fts; DROP TABLE IF EXISTS entries; \
             DROP TABLE IF EXISTS index_meta;",
        )
        .map_err(|e| MemoryIndexError::Index(format!("cannot drop the old tables: {e}")))?;
    }
    conn.execute_batch(SCHEMA)
        .map_err(|e| MemoryIndexError::Index(format!("cannot create the schema: {e}")))?;

    let found: Option<String> = conn
        .query_row("SELECT schema_version FROM index_meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .map_err(|e| MemoryIndexError::Index(format!("cannot read the index version: {e}")))?;
    match found {
        Some(found) if found != INDEX_SCHEMA_VERSION => {
            Err(MemoryIndexError::Version { found, expected: INDEX_SCHEMA_VERSION.to_string() })
        }
        Some(_) => Ok(conn),
        None => {
            conn.execute(
                "INSERT INTO index_meta (id, schema_version, next_seq) VALUES (1, ?1, 0)",
                params![INDEX_SCHEMA_VERSION],
            )
            .map_err(|e| MemoryIndexError::Index(format!("cannot initialize the index: {e}")))?;
            Ok(conn)
        }
    }
}

fn current_seq(conn: &Connection) -> Result<u64, MemoryIndexError> {
    let seq: i64 = conn
        .query_row("SELECT next_seq FROM index_meta WHERE id = 1", [], |row| row.get(0))
        .map_err(|e| MemoryIndexError::Index(format!("cannot read the index mark: {e}")))?;
    Ok(seq.max(0) as u64)
}

fn write_rows(path: &Path, rows: &[Row], next: u64) -> Result<(), MemoryIndexError> {
    let mut conn = ready(path, false)?;
    let tx = conn
        .transaction()
        .map_err(|e| MemoryIndexError::Index(format!("cannot open a transaction: {e}")))?;
    for row in rows {
        // The FTS table keeps `entries` as its content, so its rows are written
        // here rather than by a trigger: one writer, one transaction, and no SQL
        // running behind the code's back.
        //
        // The delete comes first and covers a re-indexed seq, which is what a
        // resumed rebuild writes. It has to: an external-content FTS5 table
        // reads the row it is deleting out of `entries` to know which postings
        // to cancel, so the old text must still be there when the delete runs.
        tx.execute("DELETE FROM entries_fts WHERE rowid = ?1", params![row.seq as i64])
            .map_err(|e| MemoryIndexError::Index(format!("cannot reindex seq {}: {e}", row.seq)))?;
        tx.execute(
            "INSERT OR REPLACE INTO entries (seq, event_type, turn_id, step_id, text) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.seq as i64, row.event_type, row.turn_id, row.step_id, row.text],
        )
        .map_err(|e| MemoryIndexError::Index(format!("cannot index seq {}: {e}", row.seq)))?;
        tx.execute(
            "INSERT INTO entries_fts (rowid, text) VALUES (?1, ?2)",
            params![row.seq as i64, row.text],
        )
        .map_err(|e| MemoryIndexError::Index(format!("cannot index seq {}: {e}", row.seq)))?;
    }
    let stored: i64 = tx
        .query_row("SELECT next_seq FROM index_meta WHERE id = 1", [], |row| row.get(0))
        .map_err(|e| MemoryIndexError::Index(format!("cannot read the index mark: {e}")))?;
    if stored < next as i64 {
        tx.execute("UPDATE index_meta SET next_seq = ?1 WHERE id = 1", params![next as i64])
            .map_err(|e| MemoryIndexError::Index(format!("cannot advance the index mark: {e}")))?;
    }
    tx.commit().map_err(|e| MemoryIndexError::Index(format!("cannot commit the index: {e}")))
}

/// Throw the derived database away and start from an empty one.
///
/// A file SQLite cannot open — a truncated write, a leftover from another
/// program — is not something to diagnose: the index is disposable, so the
/// remedy is to delete it and build again. The first attempt is tried before
/// deleting so an ordinary rebuild does not lose the file needlessly.
fn reset(path: &Path) -> Result<(), MemoryIndexError> {
    match ready(path, true) {
        Ok(_) => Ok(()),
        Err(first) => {
            remove_db(path).map_err(|e| {
                MemoryIndexError::Index(format!("cannot reset {} ({first}): {e}", path.display()))
            })?;
            ready(path, true).map(|_| ()).map_err(|_| first)
        }
    }
}

fn remove_db(path: &Path) -> std::io::Result<()> {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The search for a query long enough for the trigram index.
///
/// The needle arrives as one quoted phrase, so its trigrams are looked up in
/// `entries_fts` rather than scanned: `trigram` needs no word boundaries, which
/// is what lets a Chinese sentence or a code identifier be matched without a
/// segmentation step.
///
/// The score is three things added together. `bm25()` answers "how densely do
/// these words match inside this event": a phrase that occurs more often wins,
/// and between equal counts the shorter event wins. Its values are negative for
/// a good match, so the sign is flipped to make bigger better. `?4` is the log's
/// end, which turns the absolute position into a `0..1` "how late in this
/// conversation" — the same log searched before and after it grows ranks for the
/// same reasons. The last term is the kind of event, because the person's own
/// words are the best thing to find again.
///
/// The event types are literals here rather than parameters; a test pins them
/// against [`EventType`] so this SQL cannot drift from the events the loop
/// writes.
const INDEXED_SEARCH: &str = "
    SELECT e.seq, e.event_type, e.turn_id, e.step_id, e.text,
           (-f.rank) * ?3
             + (CAST(e.seq AS REAL) / ?4) * ?5
             + CASE e.event_type
                   WHEN 'inbox/appended' THEN ?6
                   WHEN 'turn/steered' THEN ?6
                   WHEN 'model/response.completed' THEN ?7
                   ELSE 0.0
               END AS score
      FROM entries AS e
      JOIN (
            SELECT rowid AS seq, bm25(entries_fts) AS rank
              FROM entries_fts
             WHERE entries_fts MATCH ?1
           ) AS f
        ON f.seq = e.seq
     ORDER BY score DESC, e.seq DESC
     LIMIT ?2
";

/// The same ranking for a query the trigram index cannot answer.
///
/// Two characters — `记账` is the everyday case — are not a trigram, so there is
/// nothing to look up and the search scans `text` for the substring instead. The
/// substring test is the v1 one, `lower()` on both sides, which folds ASCII only;
/// the index itself folds all of Unicode, so a short query is the one place where
/// case is a limit. Everything else about the ranking is identical, minus the
/// density term the scan has no way to compute.
const SCAN_SEARCH: &str = "
    SELECT seq, event_type, turn_id, step_id, text,
           (CAST(seq AS REAL) / ?3) * ?4
             + CASE event_type
                   WHEN 'inbox/appended' THEN ?5
                   WHEN 'turn/steered' THEN ?5
                   WHEN 'model/response.completed' THEN ?6
                   ELSE 0.0
               END AS score
      FROM entries
     WHERE instr(lower(text), lower(?1)) > 0
     ORDER BY score DESC, seq DESC
     LIMIT ?2
";

fn search_at(path: &Path, needle: &str, limit: usize) -> Result<Vec<MemoryHit>, MemoryIndexError> {
    let conn = ready(path, false)?;
    // The log's end, so a position is a place in this conversation rather than
    // an absolute number that grows with it. At least 1: a row can only exist
    // once the log has an event, and the division must never be by zero.
    let end = current_seq(&conn)?.max(1) as f64;
    let limit = limit as i64;

    if needle.chars().count() >= MIN_INDEXED_CHARS {
        query(
            &conn,
            INDEXED_SEARCH,
            params![
                phrase(needle),
                limit,
                BM25_WEIGHT,
                end,
                RECENCY_WEIGHT,
                TYPE_WEIGHT_INPUT,
                TYPE_WEIGHT_REPLY
            ],
            needle,
        )
    } else {
        // The needle reaches the scan as typed — no quoting, no phrase — because
        // a substring is exactly what was asked for; `lower()` folds the ASCII
        // case the scan always folded.
        query(
            &conn,
            SCAN_SEARCH,
            params![needle, limit, end, RECENCY_WEIGHT, TYPE_WEIGHT_INPUT, TYPE_WEIGHT_REPLY],
            needle,
        )
    }
}

/// Run one of the two searches and read its rows into hits.
fn query(
    conn: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
    needle: &str,
) -> Result<Vec<MemoryHit>, MemoryIndexError> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|e| MemoryIndexError::Index(format!("cannot prepare the search: {e}")))?;
    let rows = statement
        .query_map(parameters, |row| hit_from(row, needle))
        .map_err(|e| MemoryIndexError::Index(format!("cannot run the search: {e}")))?;

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row.map_err(|e| MemoryIndexError::Index(format!("cannot read a hit: {e}")))?);
    }
    Ok(hits)
}

/// One hit: the six columns both searches select, in that order.
fn hit_from(row: &rusqlite::Row<'_>, needle: &str) -> rusqlite::Result<MemoryHit> {
    let seq: i64 = row.get(0)?;
    Ok(MemoryHit {
        seq: EventSeq::new(seq.max(0) as u64),
        event_type: row.get(1)?,
        turn_id: row.get::<_, Option<String>>(2)?.map(TurnId::from_string),
        step_id: row.get::<_, Option<String>>(3)?.map(StepId::from_string),
        snippet: snippet(&row.get::<_, String>(4)?, needle),
        score: row.get(5)?,
    })
}

/// The needle as one FTS5 phrase.
///
/// Everything between the quotes is a literal, so no character of a person's
/// query can be read as FTS5 syntax; a quote of its own is doubled, which is how
/// FTS5 escapes one inside a string. The phrase is what makes the trigram lookup
/// a substring test rather than a bag of trigrams: the tokens have to appear
/// next to each other.
fn phrase(needle: &str) -> String {
    format!("\"{}\"", needle.replace('"', "\"\""))
}

/// A window around the first match, cut on character boundaries.
///
/// The window is placed by an ASCII-case-insensitive search, which is the folding
/// the fallback scan uses. The trigram index folds case across all of Unicode, so
/// a hit that differs only in a non-ASCII letter's case — `ПРИВЕТ` found by
/// `привет` — is a real hit whose window starts at the head of the text rather
/// than at the match. Folding in Rust instead would find a different position
/// than the one SQLite accepted, and a window pointing at the wrong place is
/// worse than none; the hit itself is right either way.
fn snippet(text: &str, needle: &str) -> String {
    let match_start = find_ascii_case_insensitive(text, needle).unwrap_or(0);
    let mut match_start = match_start.min(text.len());
    while match_start > 0 && !text.is_char_boundary(match_start) {
        match_start -= 1;
    }

    let mut start = match_start;
    for (kept, (index, _)) in text[..match_start].char_indices().rev().enumerate() {
        if kept == SNIPPET_CHARS {
            break;
        }
        start = index;
    }

    let mut end = text.len();
    for (kept, (index, _)) in text[match_start..].char_indices().enumerate() {
        if kept == SNIPPET_CHARS {
            end = match_start + index;
            break;
        }
    }
    text[start..end].to_string()
}

fn find_ascii_case_insensitive(text: &str, needle: &str) -> Option<usize> {
    let haystack = text.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&start| {
        haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

#[cfg(test)]
mod tests {
    use agent_protocol::events::EventType;

    use super::*;

    /// The scoring SQL names the event kinds it weighs, as literals. If the
    /// loop ever renames one, the weight would silently stop applying; this is
    /// what makes that a test failure instead.
    #[test]
    fn the_scoring_names_the_event_types_this_build_writes() {
        for sql in [INDEXED_SEARCH, SCAN_SEARCH] {
            for event_type in [
                EventType::INBOX_APPENDED,
                EventType::TURN_STEERED,
                EventType::MODEL_RESPONSE_COMPLETED,
            ] {
                assert!(
                    sql.contains(&format!("'{event_type}'")),
                    "{event_type} is scored in {sql}"
                );
            }
        }
    }

    /// A quote in the query must not end the phrase early: FTS5 reads `""`
    /// inside a string as one literal quote.
    #[test]
    fn a_quote_in_the_query_stays_inside_the_phrase() {
        assert_eq!(phrase("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(phrase("记账"), "\"记账\"");
    }
}
