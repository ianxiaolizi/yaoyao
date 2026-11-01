//! The seam between a structured check and the thing that runs it.
//!
//! A check is a program and its arguments ([`agent_protocol::verification::CheckSpec`]).
//! Running it means starting a process, capturing what it printed, and measuring
//! — which is exactly what an execution environment already does. The kernel
//! must not know how that happens: an AppContainer check runs through a different
//! backend than a local one, and neither may change what a pass means.
//!
//! The method is synchronous, like [`ExecutionEnvironment`](crate::execution_environment::ExecutionEnvironment)'s
//! `run_command`: the work is a child process plus a wait, and the adapters that
//! do it are the ones that already own a launcher. Wrapping it in an async trait
//! would make every implementation spawn a blocking task of its own to say the
//! same thing.
//!
//! A failure to start is **not** an error of this port. "The program could not
//! run" is a fact the evidence records (`CheckRun::failure`), because it is a
//! fact about the check and not about the machine being unusable: a person
//! reading the log must be able to tell "your check never started" from "the
//! agent could not write anything at all".
//!
//! [`ExecutionEnvironment`]: crate::execution_environment::ExecutionEnvironment

use agent_protocol::verification::{CheckRun, CheckSpec};
use chrono::{DateTime, Utc};

/// Runs structured checks.
pub trait CommandVerifier: Send + Sync {
    /// Run `spec` once, with `started_at` as the instant the caller asked for it,
    /// and report what happened.
    ///
    /// Implementations must start `program` directly — never through a shell —
    /// and must bound the run by `spec.timeout_ms`. A check that overran its
    /// deadline is reported as `timed_out` rather than as a failure of the task.
    ///
    /// The start instant is handed in rather than read here. An adapter has no
    /// business reading a wall clock: it measures how long the run took on a
    /// monotonic clock and adds that to `started_at`, which keeps the whole
    /// workspace's one wall-clock read at the app edge (STYLE §1). It also makes
    /// a replayed check report the same timestamps as the run it replays.
    fn verify(&self, spec: &CheckSpec, started_at: DateTime<Utc>) -> CheckRun;
}
