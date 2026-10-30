//! What each command does, one module per family of verbs.
//!
//! `main.rs` keeps what every process needs — the exit alphabet, the dispatch
//! table, the argument loop — and the commands live here so a handler can be
//! read as the thing it answers rather than as a branch in a match.

pub(crate) mod plugin;
pub(crate) mod schedule;
pub(crate) mod session;
