//! agent-memory-sqlite: the durable long-term memory store.
//!
//! One database per data directory, at `<data_dir>/memory.db`, holding every
//! fact remembered in that workspace. It sits beside the session logs rather
//! than after them: a memory is its own source of truth, not a derived view of
//! anything, so there is nothing to rebuild it from.
//!
//! That difference decides what a version mismatch means. The session index
//! refuses an unknown version and offers `reindex`; this store refuses an
//! unknown version and offers nothing automatic, because converting facts this
//! build cannot read would be guessing at someone's memory. The answer is to
//! export it and migrate by hand.
//!
//! The file is the privacy boundary. A fact lives here once; `forget` deletes
//! the row. Nothing copies facts into a session log, so deleting here is the
//! deletion — apart from the turn in which someone said the sentence, which is
//! the session log's business and not this store's.
//!
//! The same store is what a model reaches when a configuration turns the fact
//! tools on (`[memory] enabled`): `memory.recall`, `memory.remember` and
//! `memory.forget`. They are a view over the store, not a second writer —
//! every one goes through [`MemoryStore`](agent_ports::memory_store::MemoryStore),
//! so the conflict and deletion rules a person meets at the command line are
//! the ones a model meets.

#![deny(unsafe_code)]

mod store;
mod tool;

pub use store::{MEMORY_STORE_VERSION, SqliteMemoryStore};
pub use tool::{FORGET_TOOL, MemoryFactsToolProvider, Now, RECALL_TOOL, REMEMBER_TOOL};
