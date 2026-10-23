//! Where a run's instructions come from.
//!
//! The kernel never reads a project file. It asks this seam once per run, keeps
//! the answer for the run, and records it in `run/started`; the app layer is the
//! one place that knows what a workspace instruction file even is. Resolving per
//! run rather than once per process is deliberate: a long-lived `serve` must
//! notice an edited `AGENTS.md` on the next turn, and a session that outlives
//! several runs has no single moment at which its instructions could be fixed.

use agent_protocol::instructions::RunInstructions;

/// Why a source could not produce the run's instructions.
///
/// Every variant is a refusal to start the run. Falling back to a built-in
/// prompt would be a run executing under instructions the person did not write
/// and could not see — the failure this seam exists to make impossible.
#[derive(Debug, thiserror::Error)]
pub enum InstructionError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is {size} bytes, over the {limit}-byte instruction cap")]
    TooLarge { path: String, limit: usize, size: u64 },
    #[error("{path} is not valid UTF-8")]
    NotUtf8 { path: String },
    /// An instruction file that resolves outside the workspace. Instructions
    /// become model context; a link out of the boundary would make the model's
    /// prompt depend on a file the workspace was never allowed to read.
    #[error("{path} resolves outside the workspace: {resolved}")]
    OutsideWorkspace { path: String, resolved: String },
}

/// What a run is told before its first request.
pub trait InstructionSource: Send + Sync {
    /// The snapshot for the run about to open.
    ///
    /// Called once per run, before the turn is opened, so a failure consumes
    /// nothing: the inbox item stays pending for the next attempt.
    fn for_run(&self) -> Result<RunInstructions, InstructionError>;
}
