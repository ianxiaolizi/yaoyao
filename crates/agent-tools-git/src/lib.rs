//! agent-tools-git: read-only git inspection tools.
//!
//! The tools answer "what changed here?" in a shape a reader can act on rather
//! than as a wall of text: status entries carry a state word, and a diff is one
//! object per file with its patch and its line counts. Both are read-only, and
//! both read the repository in process with libgit2 rather than spawning
//! `git` — the AppContainer backend cannot give git a working directory,
//! while a process that already has one reads the same repository fine.
//!
//! The repository data is rendered into the machine text git would have
//! printed and parsed by this crate's parsers, so the tool results keep their
//! shape and the parsers stay the single definition of that shape.

#![deny(unsafe_code)]

mod parse;
mod provider;

pub use parse::{FileDiff, StatusEntry, parse_diff, parse_status};
pub use provider::{GIT_TIMEOUT_MS, GitToolProvider};
