//! MemoryStore — long-term memory as explicitly written, deletable facts.
//!
//! This is deliberately not the session index. A session's log is history: it
//! records what was said, it is append-only, and it can only be searched inside
//! one conversation (C7 #30). A memory is a judgement about what is worth
//! carrying forward — it has an author, a source, a confidence and, crucially,
//! a way to be removed. Those two things want different storage, not a wider
//! query over the same one.
//!
//! A fact is therefore its own source of truth. There is no log to rebuild it
//! from, so an unknown schema version is refused and the remedy is to export
//! and migrate by hand — never a silent conversion of a shape this build does
//! not understand.
//!
//! Deletion is real: a forgotten fact is gone from the store, not hidden behind
//! a flag. The conversation that produced it still holds the sentence in its
//! own log; purging a session log is a separate decision this port makes no
//! claim about.
//!
//! Search is lexical, for the reasons the session index gives: the same query
//! must give the same answer on every machine, and a substring scan matches
//! Chinese without a tokenizer. Embeddings are a later schema version's
//! business, and they would have to be opt-in.

use agent_protocol::ids::SessionId;
use chrono::{DateTime, Utc};

/// The namespace a fact gets when nobody names one. Namespaces exist so a
/// future profile or project scope does not need a schema change; this build
/// writes and reads exactly one.
pub const DEFAULT_NAMESPACE: &str = "workspace";

/// The confidence a fact gets when nobody names one.
///
/// Only explicit writes exist in this build — a person said "remember this", or
/// the model asked to store what a person asked it to — so the honest default
/// is certainty about *what was said*, not about the world. Extraction from
/// events would be the path that brings lower values.
pub const DEFAULT_CONFIDENCE: f32 = 1.0;

/// Who wrote the fact down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOrigin {
    /// The model called `memory.remember` and the write was approved.
    Agent,
    /// A person typed `agent memory remember`.
    User,
}

impl MemoryOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::User => "user",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "agent" => Some(Self::Agent),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// How careful a reader should be about passing this fact on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemorySensitivity {
    #[default]
    Normal,
    /// Personal material that should not be repeated into output it was not
    /// asked for. Nothing enforces redaction yet; the field exists so that a
    /// fact can be marked at the moment it is written, when the author knows.
    Private,
}

impl MemorySensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Private => "private",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "normal" => Some(Self::Normal),
            "private" => Some(Self::Private),
            _ => None,
        }
    }
}

/// Whether a fact is the current answer for its key, or one a later fact
/// replaced.
///
/// Superseded facts stay: "how did this change over time" is a question a
/// memory store should be able to answer, and the audit asks that a conflict
/// keep both sides rather than silently overwrite. Forgetting is what removes a
/// row; superseding only retires it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryStatus {
    Active,
    Superseded,
}

impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// Where a fact came from, as far as this store can say.
///
/// The session is the run that wrote it; the call is the model's tool-use id,
/// which names the exact exchange in that session's log. A fact typed at the
/// command line has no session and no call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySource {
    pub session: SessionId,
    pub call: Option<String>,
}

/// One remembered fact.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryFact {
    /// Opaque, stable, and safe to show: the only handle `forget` accepts.
    pub id: String,
    pub namespace: String,
    /// The fact's identity within its namespace. Two facts may share a key only
    /// when one supersedes the other; facts without a key never collide.
    pub key: Option<String>,
    pub text: String,
    pub confidence: f32,
    pub origin: MemoryOrigin,
    pub source: Option<MemorySource>,
    pub sensitivity: MemorySensitivity,
    pub created_at: DateTime<Utc>,
    /// When this fact stopped being active. Equal to `created_at` until a newer
    /// fact supersedes it.
    pub updated_at: DateTime<Utc>,
    /// The fact this one replaced, if any.
    pub supersedes: Option<String>,
    /// When the fact should stop being remembered. `None` means until
    /// forgotten.
    pub expires_at: Option<DateTime<Utc>>,
    pub status: MemoryStatus,
}

/// A request to remember something.
#[derive(Debug, Clone, PartialEq)]
pub struct Remember {
    /// `None` means [`DEFAULT_NAMESPACE`].
    pub namespace: Option<String>,
    pub key: Option<String>,
    pub text: String,
    /// `None` means [`DEFAULT_CONFIDENCE`]. A value outside `0..=1` is
    /// refused, not clamped: a caller that says 7 is a caller whose number
    /// cannot be trusted.
    pub confidence: Option<f32>,
    pub origin: MemoryOrigin,
    pub source: Option<MemorySource>,
    pub sensitivity: MemorySensitivity,
    /// The fact this one replaces. Required when a fact with the same
    /// namespace and key is already active — the store never overwrites one
    /// silently, because the previous value is information too.
    pub supersedes: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// How many facts a recall returns when the caller names no limit.
pub const DEFAULT_RECALL_LIMIT: usize = 20;

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recall {
    pub namespace: Option<String>,
    /// Exact match on the key, in addition to any text filter.
    pub key: Option<String>,
    /// Substring match, case-folded for ASCII. Empty or absent means no text
    /// filter, which is how "what do you remember" is asked.
    pub text: Option<String>,
    /// Include facts a newer fact replaced. Off by default: the current answer
    /// is the answer unless history was asked for.
    pub include_superseded: bool,
    /// Include facts past their expiry. Off by default, because an expired
    /// fact is one the author said to stop holding.
    pub include_expired: bool,
    /// The most facts to return, newest first. Honoured as given: a zero asks
    /// for nothing and gets nothing.
    pub limit: usize,
}

impl Default for Recall {
    fn default() -> Self {
        Self {
            namespace: None,
            key: None,
            text: None,
            include_superseded: false,
            include_expired: false,
            limit: DEFAULT_RECALL_LIMIT,
        }
    }
}

/// What to list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListFacts {
    pub namespace: Option<String>,
    pub include_superseded: bool,
    pub include_expired: bool,
}

/// Why a memory operation could not complete.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum MemoryStoreError {
    /// The database could not be read or written.
    #[error("memory store: {0}")]
    Store(String),
    /// The store was written by a different schema. Unlike the session index,
    /// there is nothing to rebuild from: export it and migrate by hand.
    #[error(
        "memory store is version {found}, this build expects {expected}; \
         run `agent memory export` and migrate it by hand"
    )]
    Version { found: String, expected: String },
    /// A fact with this namespace and key is already active, and the request
    /// did not say which one it replaces.
    #[error(
        "a fact already answers key '{key}' in namespace '{namespace}' (id {id}); \
         pass supersedes to replace it"
    )]
    Conflict { namespace: String, key: String, id: String },
    /// The request is not a fact: empty text, confidence out of range, an id
    /// that names nothing, or a supersede target that does not fit.
    #[error("{0}")]
    Invalid(String),
}

/// A durable store of remembered facts.
///
/// `remember` is the only writer and `forget` the only eraser; neither is
/// idempotent in the "safe to repeat blindly" sense (`remember` twice with the
/// same key is a conflict, not a no-op), so a caller retrying must look at what
/// the first attempt left behind.
///
/// Every method that judges a time takes `now` as a parameter — "is this
/// expired" is a decision, and a decision that reads the clock cannot be
/// replayed. The process edge supplies the instant (C7 #20).
#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    /// Write a fact, or refuse because one already owns the key.
    async fn remember(
        &self,
        request: Remember,
        now: DateTime<Utc>,
    ) -> Result<MemoryFact, MemoryStoreError>;

    /// Delete a fact for good. Returns whether anything was there to delete:
    /// forgetting a fact that is already gone is an answer, not an error.
    async fn forget(&self, id: &str) -> Result<bool, MemoryStoreError>;

    /// Find active facts that match, newest first.
    async fn recall(
        &self,
        query: &Recall,
        now: DateTime<Utc>,
    ) -> Result<Vec<MemoryFact>, MemoryStoreError>;

    /// Every fact in a namespace, retired and expired included when asked.
    async fn list(
        &self,
        filter: &ListFacts,
        now: DateTime<Utc>,
    ) -> Result<Vec<MemoryFact>, MemoryStoreError>;
}
