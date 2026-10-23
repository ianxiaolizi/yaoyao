//! MemoryIndex trait — a disposable search index over a session's durable log.
//!
//! The log is the only source of truth; this port is a read model over it.
//! Everything here is derived and may be deleted and rebuilt at any time, which
//! is what keeps a new storage format from ever being a migration: a version
//! mismatch is refused, and `rebuild` starts over from the events.
//!
//! Search is deliberately lexical. A v1 index answers "where did this word
//! appear", not "what did this mean": a lowercased substring scan is
//! deterministic, needs no tokenizer configuration, and matches Chinese text
//! the same way it matches English. Ranking, stemming, and fuzzy matching are
//! the kind of thing a later schema version can buy, not a default that makes
//! the same query answer differently on two machines.

use agent_protocol::ids::{EventSeq, SessionId, StepId, TurnId};

/// One search: which session, what to look for, and how many hits to return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuery {
    pub session_id: SessionId,
    pub text: String,
    /// The most hits to return. Search returns newest first, so a small limit
    /// keeps the most recent occurrences.
    pub limit: usize,
}

/// One event whose text matched, with a readable window around the match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHit {
    pub seq: EventSeq,
    pub event_type: String,
    pub turn_id: Option<TurnId>,
    pub step_id: Option<StepId>,
    /// The matched text around its first occurrence, cut on character
    /// boundaries so a multi-byte match is never split.
    pub snippet: String,
}

/// Why a search could not answer, or the index could not be rebuilt.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryIndexError {
    /// The durable log could not be read.
    #[error("cannot read the session log: {0}")]
    Store(String),
    /// The index could not be read or written. A corrupt file is this: the
    /// remedy is `rebuild`, which is the caller's decision, not something a
    /// search does silently.
    #[error("memory index: {0}")]
    Index(String),
    /// The index was written by a different schema. Refused rather than
    /// guessed at: the entries may be in a shape this build does not know.
    #[error("memory index is version {found}, this build expects {expected}; run reindex")]
    Version { found: String, expected: String },
}

/// A disposable read model over one session's log.
///
/// Both methods must be safe to call at any time: `search` catches the index up
/// with the log first, so a hit set is always relative to the log as it was at
/// query time. Neither method appends an event — an index is not a fact.
#[async_trait::async_trait]
pub trait MemoryIndex: Send + Sync {
    /// Find events whose extracted text contains `query.text`, newest first.
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryHit>, MemoryIndexError>;

    /// Discard the derived index and build it again from the log.
    ///
    /// Returns the number of indexed entries. This is the escape hatch the
    /// version refusal points at: after a schema change, or a damaged file, the
    /// index is rebuilt rather than migrated.
    async fn rebuild(&self, session_id: &SessionId) -> Result<usize, MemoryIndexError>;
}
