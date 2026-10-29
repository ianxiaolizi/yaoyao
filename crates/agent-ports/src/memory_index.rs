//! MemoryIndex trait — a disposable search index over a session's durable log.
//!
//! The log is the only source of truth; this port is a read model over it.
//! Everything here is derived and may be deleted and rebuilt at any time, which
//! is what keeps a new storage format from ever being a migration: a version
//! mismatch is refused, and `rebuild` starts over from the events.
//!
//! Search is deliberately lexical. A v2 index answers "where did this word
//! appear, and how much does it look like what was asked for": a trigram index
//! scores how densely the words match inside an event, then how recent it is,
//! then what kind of event it is, and a short query falls back to a substring
//! scan so Chinese still matches without a tokenizer. Stemming, synonyms and
//! embeddings are the kind of thing a later schema version could buy, not a
//! default that makes the same query answer differently on two machines — every
//! score comes from one deterministic formula, and no model is asked for an
//! opinion.

use agent_protocol::ids::{EventSeq, SessionId, StepId, TurnId};

/// One search: which session, what to look for, and how many hits to return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuery {
    pub session_id: SessionId,
    pub text: String,
    /// The most hits to return, taken from the most relevant end. Ties on
    /// score are broken by recency, so a small limit keeps the strongest
    /// matches and, among equals, the latest ones.
    pub limit: usize,
}

/// One event whose text matched, with a readable window around the match.
///
/// Hits arrive in relevance order — most relevant first — not in log order.
/// [`MemoryHit::score`] is what the order was made from.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub seq: EventSeq,
    pub event_type: String,
    pub turn_id: Option<TurnId>,
    pub step_id: Option<StepId>,
    /// The matched text around its first occurrence, cut on character
    /// boundaries so a multi-byte match is never split.
    pub snippet: String,
    /// How well this event matches, higher first.
    ///
    /// Comparable only within one hit set: the components are how densely the
    /// words match inside this event (the index's own bm25), a recency term
    /// derived from the log position, and a weight for what kind of event this
    /// is. Two different queries say nothing about each other through this
    /// number, and nothing outside the index should read it as a probability.
    pub score: f64,
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
    /// Find events whose extracted text contains `query.text`, most relevant
    /// first — see [`MemoryHit::score`] for what relevance is made of.
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryHit>, MemoryIndexError>;

    /// Discard the derived index and build it again from the log.
    ///
    /// Returns the number of indexed entries. This is the escape hatch the
    /// version refusal points at: after a schema change, or a damaged file, the
    /// index is rebuilt rather than migrated.
    async fn rebuild(&self, session_id: &SessionId) -> Result<usize, MemoryIndexError>;
}
