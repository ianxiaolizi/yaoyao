//! agent-store-jsonl: the first durable EventStore — one `events.jsonl` per session.
//!
//! Append-only JSONL on disk. `append` writes lines without fsync; `flush`
//! syncs to make them crash-durable. On reopen the store rebuilds its runtime
//! state by replaying the file and truncating any torn trailing line.

#![deny(unsafe_code)]

mod store;

pub use store::{JsonlEventStore, has_session, session_ids};
