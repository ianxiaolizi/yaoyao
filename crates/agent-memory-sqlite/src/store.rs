//! The SQLite-backed [`MemoryStore`] adapter.
//!
//! Every operation opens its own connection on the blocking pool, with WAL and
//! a busy timeout, the same shape the session index uses: rusqlite is
//! synchronous, and a recall called from the async edge must not stall the
//! runtime.
//!
//! Times are stored as whole seconds since the epoch. The format is not for
//! display — [`MemoryFact`] carries real timestamps — it is so that comparisons
//! ("is this expired", "which is newer") are exact integer comparisons rather
//! than string ordering, which would depend on how many fractional digits
//! happened to be formatted on the day a row was written.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_ports::memory_store::{
    DEFAULT_CONFIDENCE, DEFAULT_NAMESPACE, ListFacts, MemoryFact, MemoryOrigin, MemorySensitivity,
    MemorySource, MemoryStatus, MemoryStore, MemoryStoreError, Recall, Remember,
};
use agent_protocol::ids::{SessionId, fresh_id};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, named_params, params};

/// The shape of the store. Not the event schema version (C6.1) and not the
/// session index's version: this one names the files that cannot be rebuilt,
/// so a mismatch is refused outright.
pub const MEMORY_STORE_VERSION: &str = "memory-store/0.1.0";

/// The file this store keeps, under the data directory it was opened with.
pub const FILE_NAME: &str = "memory.db";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Every fact lives in one table. `fact_key` is null for facts that have no
/// identity; the partial unique index is what turns "one active fact per key"
/// from a rule the code checks into one the database also refuses to break.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS store_meta (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        schema_version TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS facts (
        id TEXT PRIMARY KEY,
        namespace TEXT NOT NULL,
        fact_key TEXT,
        text TEXT NOT NULL,
        confidence REAL NOT NULL,
        origin TEXT NOT NULL,
        source_session TEXT,
        source_call TEXT,
        sensitivity TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        supersedes TEXT,
        expires_at INTEGER,
        status TEXT NOT NULL
    );
    CREATE UNIQUE INDEX IF NOT EXISTS one_active_fact_per_key
        ON facts(namespace, fact_key)
        WHERE fact_key IS NOT NULL AND status = 'active';
    CREATE INDEX IF NOT EXISTS facts_by_namespace ON facts(namespace, status);
";

/// The column order every read uses, so a query cannot drift from the mapper.
const COLUMNS: &str = "id, namespace, fact_key, text, confidence, origin, source_session, \
                       source_call, sensitivity, created_at, updated_at, supersedes, expires_at, \
                       status";

/// The durable store, over one data directory.
pub struct SqliteMemoryStore {
    data_dir: PathBuf,
}

impl SqliteMemoryStore {
    /// Open the store under `data_dir`, beside the session logs. Nothing is
    /// created until the first operation.
    pub fn open(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into() }
    }

    /// Where the database lives. Public so a doctor check can name the file
    /// without opening it.
    pub fn path(&self) -> PathBuf {
        self.data_dir.join(FILE_NAME)
    }
}

#[async_trait::async_trait]
impl MemoryStore for SqliteMemoryStore {
    async fn remember(
        &self,
        request: Remember,
        now: DateTime<Utc>,
    ) -> Result<MemoryFact, MemoryStoreError> {
        let path = self.path();
        blocking(move || {
            let mut conn = ready(&path)?;
            remember_now(&mut conn, request, now)
        })
        .await
    }

    async fn forget(&self, id: &str) -> Result<bool, MemoryStoreError> {
        let path = self.path();
        let id = id.to_string();
        blocking(move || {
            let conn = ready(&path)?;
            conn.execute("DELETE FROM facts WHERE id = ?1", params![id])
                .map(|rows| rows > 0)
                .map_err(store_err)
        })
        .await
    }

    async fn recall(
        &self,
        query: &Recall,
        now: DateTime<Utc>,
    ) -> Result<Vec<MemoryFact>, MemoryStoreError> {
        let path = self.path();
        let query = query.clone();
        blocking(move || {
            let conn = ready(&path)?;
            recall_now(&conn, &query, now)
        })
        .await
    }

    async fn list(
        &self,
        filter: &ListFacts,
        now: DateTime<Utc>,
    ) -> Result<Vec<MemoryFact>, MemoryStoreError> {
        let path = self.path();
        let filter = filter.clone();
        blocking(move || {
            let conn = ready(&path)?;
            list_now(&conn, &filter, now)
        })
        .await
    }
}

/// Run a blocking store operation on the pool, naming a pool failure as a store
/// failure — the operation never got to run, so it has no opinion of its own.
async fn blocking<T>(
    operation: impl FnOnce() -> Result<T, MemoryStoreError> + Send + 'static,
) -> Result<T, MemoryStoreError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|e| MemoryStoreError::Store(format!("store task failed: {e}")))?
}

fn store_err(error: rusqlite::Error) -> MemoryStoreError {
    MemoryStoreError::Store(error.to_string())
}

/// Open the database, create the schema when it is absent, and check the
/// version.
///
/// An unknown version is refused rather than adopted: this is the one file
/// whose rows have no other home, so converting a shape this build cannot read
/// would be guessing at someone's memory rather than repairing a cache.
fn ready(path: &Path) -> Result<Connection, MemoryStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            MemoryStoreError::Store(format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    let conn = Connection::open(path)
        .map_err(|e| MemoryStoreError::Store(format!("cannot open {}: {e}", path.display())))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| MemoryStoreError::Store(format!("cannot set the busy timeout: {e}")))?;
    // WAL so a recall can read while a remember writes; the returned mode is
    // not needed, only the setting.
    conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0))
        .map_err(store_err)?;
    conn.execute_batch(SCHEMA).map_err(store_err)?;
    let found: Option<String> = conn
        .query_row("SELECT schema_version FROM store_meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .map_err(store_err)?;
    match found {
        None => {
            conn.execute(
                "INSERT INTO store_meta (id, schema_version) VALUES (1, ?1)",
                params![MEMORY_STORE_VERSION],
            )
            .map_err(store_err)?;
            Ok(conn)
        }
        Some(version) if version == MEMORY_STORE_VERSION => Ok(conn),
        Some(version) => Err(MemoryStoreError::Version {
            found: version,
            expected: MEMORY_STORE_VERSION.to_string(),
        }),
    }
}

/// The validated, normalized shape of a request, before it becomes a row.
struct Prepared {
    namespace: String,
    key: Option<String>,
    text: String,
    confidence: f32,
    origin: MemoryOrigin,
    source: Option<MemorySource>,
    sensitivity: MemorySensitivity,
    supersedes: Option<String>,
    expires_at: Option<i64>,
}

fn prepare(request: Remember, now: DateTime<Utc>) -> Result<Prepared, MemoryStoreError> {
    let text = request.text.trim();
    if text.is_empty() {
        return Err(MemoryStoreError::Invalid("a fact needs text".to_string()));
    }
    let confidence = request.confidence.unwrap_or(DEFAULT_CONFIDENCE);
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(MemoryStoreError::Invalid(format!(
            "confidence must be between 0 and 1, got {confidence}"
        )));
    }
    let namespace =
        request.namespace.as_deref().map(str::trim).unwrap_or(DEFAULT_NAMESPACE).to_string();
    if namespace.is_empty() {
        return Err(MemoryStoreError::Invalid("a namespace cannot be empty".to_string()));
    }
    let key = match request.key.as_deref().map(str::trim) {
        None => None,
        Some("") => {
            return Err(MemoryStoreError::Invalid(
                "a key cannot be empty; omit it for a fact without an identity".to_string(),
            ));
        }
        Some(key) => Some(key.to_string()),
    };
    let now = now.timestamp();
    if let Some(expires) = request.expires_at
        && expires.timestamp() <= now
    {
        return Err(MemoryStoreError::Invalid(
            "an expiry that has already passed would be a fact forgotten on arrival".to_string(),
        ));
    }
    Ok(Prepared {
        namespace,
        key,
        text: text.to_string(),
        confidence,
        origin: request.origin,
        source: request.source,
        sensitivity: request.sensitivity,
        supersedes: request.supersedes,
        expires_at: request.expires_at.map(|at| at.timestamp()),
    })
}

/// One `remember`: validation, the conflict rule, then one transaction.
///
/// The conflict rule is the point. A fact that already answers a key is not
/// overwritten, even when the caller meant to replace it — the caller has to
/// name it, which makes "this replaced that" a thing the store can tell later
/// from "this was always the only one".
fn remember_now(
    conn: &mut Connection,
    request: Remember,
    now: DateTime<Utc>,
) -> Result<MemoryFact, MemoryStoreError> {
    let prepared = prepare(request, now)?;
    let tx = conn.transaction().map_err(store_err)?;

    let current: Option<String> = match &prepared.key {
        None => None,
        Some(key) => tx
            .query_row(
                "SELECT id FROM facts WHERE namespace = ?1 AND fact_key = ?2 AND status = 'active'",
                params![prepared.namespace, key],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_err)?,
    };
    // Whatever holds the key has to be the fact this request replaces. Anything
    // else would leave two active facts answering one key, so it is refused
    // with the id that has to be named instead.
    if let Some(existing) = &current
        && prepared.supersedes.as_ref() != Some(existing)
    {
        return Err(MemoryStoreError::Conflict {
            namespace: prepared.namespace.clone(),
            key: prepared.key.clone().unwrap_or_default(),
            id: existing.clone(),
        });
    }

    if let Some(old_id) = &prepared.supersedes {
        let Some(old) = load_fact(&tx, old_id)? else {
            return Err(MemoryStoreError::Invalid(format!(
                "no fact with id '{old_id}' to supersede"
            )));
        };
        if old.status == MemoryStatus::Superseded {
            return Err(MemoryStoreError::Invalid(format!(
                "fact {old_id} was already superseded; supersede the fact that replaced it"
            )));
        }
        if old.namespace != prepared.namespace {
            return Err(MemoryStoreError::Invalid(
                "a replacing fact must stay in the same namespace".to_string(),
            ));
        }
        if old.key != prepared.key {
            return Err(MemoryStoreError::Invalid(
                "a replacing fact must keep the same key; without it the old fact would keep \
                 answering that key"
                    .to_string(),
            ));
        }
        tx.execute(
            "UPDATE facts SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![MemoryStatus::Superseded.as_str(), now.timestamp(), old_id],
        )
        .map_err(store_err)?;
    }

    let fact = MemoryFact {
        id: format!("mem_{}", fresh_id()),
        namespace: prepared.namespace,
        key: prepared.key,
        text: prepared.text,
        confidence: prepared.confidence,
        origin: prepared.origin,
        source: prepared.source,
        sensitivity: prepared.sensitivity,
        created_at: now,
        updated_at: now,
        supersedes: prepared.supersedes,
        expires_at: prepared.expires_at.map(from_seconds).transpose()?,
        status: MemoryStatus::Active,
    };
    insert_fact(&tx, &fact)?;
    tx.commit().map_err(store_err)?;
    Ok(fact)
}

fn recall_now(
    conn: &Connection,
    query: &Recall,
    now: DateTime<Utc>,
) -> Result<Vec<MemoryFact>, MemoryStoreError> {
    let now = now.timestamp();
    let text = query.text.as_deref().map(str::trim).filter(|text| !text.is_empty());
    // `lower()` folds ASCII only, the same limit the session index's substring
    // fallback has. Chinese has no case to fold, so the common case is exact.
    let sql = format!(
        "SELECT {COLUMNS} FROM facts
         WHERE (:namespace IS NULL OR namespace = :namespace)
           AND (:key IS NULL OR fact_key = :key)
           AND (:text IS NULL OR instr(lower(facts.text), lower(:text)) > 0)
           AND (:include_superseded OR status = 'active')
           AND (:include_expired OR expires_at IS NULL OR expires_at > :now)
         ORDER BY updated_at DESC, rowid DESC
         LIMIT :limit"
    );
    let mut statement = conn.prepare(&sql).map_err(store_err)?;
    let rows = statement
        .query_map(
            named_params! {
                ":namespace": query.namespace,
                ":key": query.key,
                ":text": text,
                ":include_superseded": query.include_superseded,
                ":include_expired": query.include_expired,
                ":now": now,
                ":limit": query.limit as i64,
            },
            raw_from_row,
        )
        .map_err(store_err)?;
    rows.map(|row| into_fact(row.map_err(store_err)?)).collect()
}

fn list_now(
    conn: &Connection,
    filter: &ListFacts,
    now: DateTime<Utc>,
) -> Result<Vec<MemoryFact>, MemoryStoreError> {
    let now = now.timestamp();
    let sql = format!(
        "SELECT {COLUMNS} FROM facts
         WHERE (:namespace IS NULL OR namespace = :namespace)
           AND (:include_superseded OR status = 'active')
           AND (:include_expired OR expires_at IS NULL OR expires_at > :now)
         ORDER BY namespace ASC, fact_key IS NULL, fact_key ASC, updated_at DESC, rowid DESC"
    );
    let mut statement = conn.prepare(&sql).map_err(store_err)?;
    let rows = statement
        .query_map(
            named_params! {
                ":namespace": filter.namespace,
                ":include_superseded": filter.include_superseded,
                ":include_expired": filter.include_expired,
                ":now": now,
            },
            raw_from_row,
        )
        .map_err(store_err)?;
    rows.map(|row| into_fact(row.map_err(store_err)?)).collect()
}

fn load_fact(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
) -> Result<Option<MemoryFact>, MemoryStoreError> {
    let sql = format!("SELECT {COLUMNS} FROM facts WHERE id = ?1");
    let raw = tx.query_row(&sql, params![id], raw_from_row).optional().map_err(store_err)?;
    raw.map(into_fact).transpose()
}

fn insert_fact(tx: &rusqlite::Transaction<'_>, fact: &MemoryFact) -> Result<(), MemoryStoreError> {
    tx.execute(
        "INSERT INTO facts (id, namespace, fact_key, text, confidence, origin, source_session, \
                            source_call, sensitivity, created_at, updated_at, supersedes, \
                            expires_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            fact.id,
            fact.namespace,
            fact.key,
            fact.text,
            fact.confidence as f64,
            fact.origin.as_str(),
            fact.source.as_ref().map(|source| source.session.as_str()),
            fact.source.as_ref().and_then(|source| source.call.clone()),
            fact.sensitivity.as_str(),
            fact.created_at.timestamp(),
            fact.updated_at.timestamp(),
            fact.supersedes,
            fact.expires_at.map(|at| at.timestamp()),
            fact.status.as_str(),
        ],
    )
    .map_err(store_err)?;
    Ok(())
}

fn from_seconds(seconds: i64) -> Result<DateTime<Utc>, MemoryStoreError> {
    DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| MemoryStoreError::Store(format!("stored time {seconds} is out of range")))
}

/// A row exactly as it is stored, before any interpretation.
struct Raw {
    id: String,
    namespace: String,
    key: Option<String>,
    text: String,
    confidence: f64,
    origin: String,
    source_session: Option<String>,
    source_call: Option<String>,
    sensitivity: String,
    created_at: i64,
    updated_at: i64,
    supersedes: Option<String>,
    expires_at: Option<i64>,
    status: String,
}

fn raw_from_row(row: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        id: row.get(0)?,
        namespace: row.get(1)?,
        key: row.get(2)?,
        text: row.get(3)?,
        confidence: row.get(4)?,
        origin: row.get(5)?,
        source_session: row.get(6)?,
        source_call: row.get(7)?,
        sensitivity: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        supersedes: row.get(11)?,
        expires_at: row.get(12)?,
        status: row.get(13)?,
    })
}

/// A stored string this build does not recognise is corruption, not a default:
/// reading it as "normal" or "active" would quietly make a private fact public
/// or a retired one current.
fn into_fact(raw: Raw) -> Result<MemoryFact, MemoryStoreError> {
    let origin = MemoryOrigin::parse(&raw.origin).ok_or_else(|| {
        MemoryStoreError::Store(format!("unknown origin '{}' in fact {}", raw.origin, raw.id))
    })?;
    let sensitivity = MemorySensitivity::parse(&raw.sensitivity).ok_or_else(|| {
        MemoryStoreError::Store(format!(
            "unknown sensitivity '{}' in fact {}",
            raw.sensitivity, raw.id
        ))
    })?;
    let status = MemoryStatus::parse(&raw.status).ok_or_else(|| {
        MemoryStoreError::Store(format!("unknown status '{}' in fact {}", raw.status, raw.id))
    })?;
    let source = match (raw.source_session, raw.source_call) {
        (None, _) => None,
        (Some(session), call) => {
            Some(MemorySource { session: SessionId::from_string(session), call })
        }
    };
    Ok(MemoryFact {
        id: raw.id,
        namespace: raw.namespace,
        key: raw.key,
        text: raw.text,
        confidence: raw.confidence as f32,
        origin,
        source,
        sensitivity,
        created_at: from_seconds(raw.created_at)?,
        updated_at: from_seconds(raw.updated_at)?,
        supersedes: raw.supersedes,
        expires_at: raw.expires_at.map(from_seconds).transpose()?,
        status,
    })
}
