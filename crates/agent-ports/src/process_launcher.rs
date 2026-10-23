//! The seam between the execution environment and the thing that actually
//! starts a process.
//!
//! `ExecutionEnvironment` owns the *policy* of running a command: the working
//! directory boundary, the deadline, the output cap, and the exit-code
//! convention. `ProcessLauncher` owns only the spawning — which is the part that
//! differs between a plain local process, an AppContainer process, and a
//! container. Keeping them apart means a new backend cannot accidentally
//! reimplement the boundary, and the environment cannot accidentally assume a
//! particular way of spawning.
//!
//! `try_wait` reports `Option<i32>` rather than `std::process::ExitStatus`
//! because a launcher that runs the command elsewhere has no local process to
//! ask: it knows the exit code or it does not. `None` means still running; a
//! process that ended without a code (killed by a signal) is `Some(-1)`, the
//! same value the local backend has always reported.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;

/// Whether the caller needs to talk to the child over stdin.
///
/// A shell command does not: closing the write end immediately gives the child
/// EOF, which is what a command that never reads stdin expects, and what a
/// command that does read it must see instead of blocking. A long-lived helper
/// started by an environment does need it — the request/response channel is the
/// pipe — so the write end is kept and handed to the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StdinMode {
    /// Close the write end before returning: the child reads EOF.
    #[default]
    Closed,
    /// Keep the write end and expose it through
    /// [`RunningProcess::take_stdin`].
    Pipe,
}

/// Everything a launcher needs to start one command.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub program: String,
    pub args: Vec<String>,
    /// Already resolved against the workspace root by the environment.
    pub cwd: PathBuf,
    /// Already scrubbed by [`crate::process_env`]; the launcher passes exactly
    /// these.
    pub env: Vec<(OsString, OsString)>,
    /// Whether this child is spoken to over stdin.
    pub stdin: StdinMode,
}

/// A process that was started and can be waited on or killed.
pub trait RunningProcess: Send {
    /// `None` while running, `Some(code)` once it has ended. `Some(-1)` when it
    /// ended without an exit code.
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
    /// Block until it ends (used after a kill, to reap it).
    fn wait(&mut self) -> io::Result<Option<i32>>;
    /// Kill the process and, where the backend can, everything it started.
    fn kill_tree(&mut self) -> io::Result<()>;
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>>;
    /// The write end of the child's stdin, when the request asked for
    /// [`StdinMode::Pipe`] and this launcher can provide it.
    ///
    /// `None` means this launcher cannot keep stdin open. A caller that needs
    /// the channel must treat that as a refusal to start its helper, never as
    /// "the child gets an empty stdin" — a helper that reads a closed pipe sees
    /// EOF and exits, and the caller would spend the rest of the run failing
    /// every request against a process that is already gone.
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        None
    }
}

/// Why a command could not be started.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("could not start a process: {0}")]
    Io(#[from] io::Error),
    /// The selected backend is not usable here (missing profile, missing
    /// container runtime, missing image). This is a refusal to run, never a
    /// reason to fall back to a less confined backend.
    #[error("execution backend unavailable: {0}")]
    BackendUnavailable(String),
}

/// Starts commands for an execution environment.
pub trait ProcessLauncher: Send + Sync {
    fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn RunningProcess>, LaunchError>;
}
