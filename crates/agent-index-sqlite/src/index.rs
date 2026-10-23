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
pub const INDEX_SCHEMA_VERSION: &str = "memory-index/0.1.0";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many characters of context the snippet shows on each side of the match.
const SNIPPET_CHARS: usize = 80;

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
        conn.execute_batch("DROP TABLE IF EXISTS entries; DROP TABLE IF EXISTS index_meta;")
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
        tx.execute(
            "INSERT OR REPLACE INTO entries (seq, event_type, turn_id, step_id, text) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.seq as i64, row.event_type, row.turn_id, row.step_id, row.text],
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

fn search_at(path: &Path, needle: &str, limit: usize) -> Result<Vec<MemoryHit>, MemoryIndexError> {
    let conn = ready(path, false)?;
    let mut statement = conn
        .prepare(
            "SELECT seq, event_type, turn_id, step_id, text FROM entries \
             WHERE instr(lower(text), lower(?1)) > 0 ORDER BY seq DESC LIMIT ?2",
        )
        .map_err(|e| MemoryIndexError::Index(format!("cannot prepare the search: {e}")))?;
    let rows = statement
        .query_map(params![needle, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| MemoryIndexError::Index(format!("cannot run the search: {e}")))?;

    let mut hits = Vec::new();
    for row in rows {
        let (seq, event_type, turn_id, step_id, text) =
            row.map_err(|e| MemoryIndexError::Index(format!("cannot read a hit: {e}")))?;
        hits.push(MemoryHit {
            seq: EventSeq::new(seq.max(0) as u64),
            event_type,
            turn_id: turn_id.map(TurnId::from_string),
            step_id: step_id.map(StepId::from_string),
            snippet: snippet(&text, needle),
        });
    }
    Ok(hits)
}

/// A window around the first match, cut on character boundaries.
///
/// The match is found the way SQLite found it: ASCII-only case folding. Doing
/// Unicode folding here would sometimes find a different position than the one
/// the `WHERE` clause accepted, and a snippet pointing at the wrong place is
/// worse than no highlighting at all.
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
