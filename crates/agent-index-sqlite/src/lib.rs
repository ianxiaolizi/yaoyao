//! agent-index-sqlite: a disposable SQLite read model over the session log.
//!
//! The log is the truth and this index is a cache: deleting `memory.sqlite`
//! loses nothing that cannot be rebuilt from `events.jsonl`. That is why the
//! index has its own version string and refuses one it does not know — the
//! remedy is to rebuild, never to migrate entries whose shape is unknown.
//!
//! Search is lexical on purpose. A lowercased substring scan is deterministic,
//! needs no tokenizer configuration, and matches Chinese text as readily as
//! English. It is not ranking, stemming, or semantics; those are a later
//! schema version's business.

#![deny(unsafe_code)]

pub mod extract;
mod index;
pub mod tool;

pub use index::{INDEX_SCHEMA_VERSION, SqliteMemoryIndex};
pub use tool::MemoryToolProvider;
