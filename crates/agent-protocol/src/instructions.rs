//! What a run is told before its first request.
//!
//! The instructions are a run-level snapshot, not a session-level constant and
//! not something a provider assembles per request. A session outlives the files
//! it was started beside: `AGENTS.md` can change between two runs, and a prompt
//! assembled per request would let the model see text that was never recorded.
//! Recording the snapshot in `run/started` is what keeps "what the model saw"
//! and "what the log says" the same fact, and it is what a later fold rebuilds
//! the transcript's system message from.
//!
//! The text is opaque here. Which preamble produced it is named by `builtin`, so
//! a reader that finds an identifier it does not know can tell it is looking at
//! a prompt this build did not compose rather than assuming it is current.

use serde::{Deserialize, Serialize};

/// The instruction snapshot one run executes under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInstructions {
    /// The text placed in the run's system message.
    pub text: String,
    /// The project instruction file folded into `text`, named relative to the
    /// workspace root. `None` is a workspace without one — a legal state, not
    /// a read that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_file: Option<String>,
    /// Which built-in preamble produced `text`.
    pub builtin: String,
}
