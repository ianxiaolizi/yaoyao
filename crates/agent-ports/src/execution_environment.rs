//! ExecutionEnvironment trait — the sandbox/execution backend for file and
//! process operations.
//!
//! The environment owns the sandbox boundary: it must reject paths outside its
//! workspace roots and commands its mode does not permit. It also **advertises**
//! what it can do (`advertised_capabilities`), because "the sandbox says this
//! build can" and "the policy says this run may" are different questions and the
//! policy engine needs the first to answer the second.

use agent_protocol::permissions::Capability;
use serde::{Deserialize, Serialize};

/// The sandbox mode an environment provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Read-only workspace, no command execution and no writes.
    ReadOnly,
    /// Local process execution with approval as the backstop. Directory-level
    /// confinement only; the command itself is not isolated by the OS.
    LocalProcess,
    /// A workspace sandbox: commands run under an OS confinement mechanism
    /// (Windows AppContainer), so reads and writes outside the workspace are
    /// refused by the kernel rather than by this process.
    WorkspaceSandbox,
    /// A container backend: commands run inside a container with the workspace
    /// mounted at a fixed path.
    Container,
}

impl SandboxMode {
    /// The capabilities this mode's backend provides at all.
    ///
    /// This is the *advertisement*, not a grant: a capability listed here still
    /// needs a rule, and a rule naming a capability absent here is refused
    /// before it is consulted (`CONTRACT.md` C2.2). Read-only removes the
    /// capabilities that write — `fs.write`, `memory.write` and
    /// `process.spawn` — so a configuration that reaches for them is refused as
    /// unavailable rather than quietly attempted. Remembering a fact is a write
    /// like any other: it outlives the run, and a read-only run promises none of
    /// that.
    ///
    /// `network.request` and `subagent.spawn` are build-level: the binary ships
    /// MCP-over-HTTP and subagent backends. A backend that cannot reach the
    /// network (an AppContainer without the internet capability; a container
    /// started with `--network none`) overrides this and leaves the entry out.
    pub fn advertised_capabilities(self) -> Vec<Capability> {
        match self {
            // A read-only run can look at the workspace and search its own
            // history, and nothing else: no writes, no child process (so no
            // `shell.run`, no `git.status`, no subagent backend), and no
            // network — every other capability this build has is reached
            // through one of those two.
            SandboxMode::ReadOnly => {
                vec![Capability::new(Capability::FS_READ), Capability::new(Capability::MEMORY_READ)]
            }
            // A confinement backend that restricts the network overrides this
            // and leaves `network.request` out (slice B).
            SandboxMode::LocalProcess | SandboxMode::WorkspaceSandbox | SandboxMode::Container => {
                vec![
                    Capability::new(Capability::FS_READ),
                    Capability::new(Capability::FS_WRITE),
                    Capability::new(Capability::GIT_STATUS),
                    Capability::new(Capability::MEMORY_READ),
                    Capability::new(Capability::MEMORY_WRITE),
                    Capability::new(Capability::PROCESS_SPAWN),
                    Capability::new(Capability::NETWORK_REQUEST),
                    Capability::new(Capability::SUBAGENT_SPAWN),
                ]
            }
        }
    }
}

/// How a backend spells "run this string as a shell command".
///
/// Not the command itself — [`CommandSpec`] is — but the wrapper the backend's
/// shell grammar needs. It is a separate value because the wrapper is a property
/// of the backend, not of the tool: the same `shell.run` must go through `cmd /C`
/// on a local process and through PowerShell inside an AppContainer, where
/// `cmd.exe` cannot start a child process at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellInvocation {
    pub program: String,
    pub args: Vec<String>,
}

/// The host shell convention: `cmd /C` on Windows, `sh -c` elsewhere.
pub fn host_shell(command: &str) -> ShellInvocation {
    #[cfg(windows)]
    {
        ShellInvocation {
            program: "cmd".to_string(),
            args: vec!["/C".to_string(), command.to_string()],
        }
    }
    #[cfg(not(windows))]
    {
        ShellInvocation {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), command.to_string()],
        }
    }
}

/// A command to run in an environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Working directory; must be inside a workspace root.
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// A command's output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Whether stdout+stderr exceeded the size cap and was truncated.
    pub truncated: bool,
}

/// What one directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirEntryKind {
    File,
    Directory,
    /// Reported, never followed. A link is the one entry whose target the
    /// listing does not vouch for, and a walk that followed them could leave
    /// the workspace through a link that was made after the boundary check.
    Symlink,
    Other,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: DirEntryKind,
    /// Bytes for a regular file; zero for anything else.
    pub size: u64,
}

/// Port for executing file and command operations in an isolated environment.
pub trait ExecutionEnvironment: Send + Sync {
    fn name(&self) -> &str;
    fn mode(&self) -> SandboxMode;
    /// What this bound backend actually provides. Defaults to its mode's list;
    /// a backend that can do less than its mode promises (a container with no
    /// network) overrides it.
    fn advertised_capabilities(&self) -> Vec<Capability> {
        self.mode().advertised_capabilities()
    }
    /// How this backend turns a `shell.run` command string into a process.
    ///
    /// Defaults to the host shell. A confinement backend overrides it when its
    /// confinement makes the host shell unusable — inside a Windows AppContainer
    /// `cmd.exe` cannot create a child process, so that backend runs commands
    /// under PowerShell instead. The command *string* still comes from the tool;
    /// only the wrapper belongs to the backend.
    fn shell_invocation(&self, command: &str) -> ShellInvocation {
        host_shell(command)
    }
    /// The command language this backend substitutes for the host convention,
    /// if any.
    ///
    /// The tool description names it so a command author writes the right
    /// syntax and does not wrap a second shell around the command. `None` means
    /// the host convention (`cmd /C` on Windows, `sh -c` elsewhere).
    fn shell_language(&self) -> Option<&'static str> {
        None
    }
    fn read_file(&self, path: &str) -> Result<String, ExecutionEnvironmentError>;
    fn write_file(&self, path: &str, content: &str) -> Result<(), ExecutionEnvironmentError>;
    /// List one directory, sorted by name.
    ///
    /// Sorted because the result is something a model reads: two runs over the
    /// same tree must produce the same answer, or the transcript stops being
    /// reproducible from the log. Links are reported as [`DirEntryKind::Symlink`]
    /// and not resolved.
    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, ExecutionEnvironmentError>;
    /// Run a command.
    ///
    /// A command that exceeds `CommandSpec::timeout_ms` is an **ordinary
    /// result**, not an error: implementations kill it and report exit code
    /// 124 (the `timeout(1)` convention), so the caller reads how far it got
    /// in the same shape as any other completed command. The error channel is
    /// for the environment failing to run anything at all.
    fn run_command(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecutionEnvironmentError>;
}

/// Errors from an ExecutionEnvironment implementation.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionEnvironmentError {
    /// The path that was refused, and the root that refused it.
    ///
    /// The path alone leaves the reader to go and discover what the workspace
    /// even was before they can tell whether the request was a mistake or the
    /// root is narrower than they thought. The environment holds both, so it
    /// says both.
    #[error("path outside workspace: {path} (the workspace root is {root})")]
    PathOutsideWorkspace { path: String, root: String },
    #[error("operation not permitted in mode {mode:?}")]
    NotPermitted { mode: SandboxMode },
    /// The selected backend could not be used (missing AppContainer profile,
    /// missing container runtime). A refusal to run, never a reason to fall
    /// back to a less confined backend.
    #[error("execution backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("path is protected: {0}")]
    ProtectedPath(String),
    /// The working directory a command asked for cannot be used.
    ///
    /// The value is named because the model passed it: the OS reports only
    /// "the directory name is invalid", which leaves the next attempt guessing
    /// which argument was wrong — and a guess is what a small model has to work
    /// with. The hint names the default so the retry has somewhere to go.
    #[error(
        "cannot run in `{path}`: not a directory in the workspace \
         (the workspace root is {root}); omit `cwd` to run in the root"
    )]
    CwdUnusable { path: String, root: String },
    /// The file exists and was reachable, but is larger than the backend's
    /// read cap. Reading it whole would put an unbounded amount of memory
    /// behind one tool call, so the read is refused and says how big the cap
    /// is — a partial read presented as a whole file would be worse than a
    /// refusal, because nothing downstream could tell the difference.
    #[error("file is larger than the {limit}-byte read cap: {path}")]
    FileTooLarge { path: String, limit: usize },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(caps: &[Capability]) -> Vec<&str> {
        caps.iter().map(Capability::as_str).collect()
    }

    #[test]
    fn read_only_advertises_nothing_that_writes_or_starts_a_process() {
        // The advertisement is what the policy engine consults before any rule:
        // a read-only run that advertised `process.spawn` would turn a
        // configured `shell.run` into a call that reaches the environment and
        // fails there, instead of a refusal that says the backend cannot do it.
        let caps = SandboxMode::ReadOnly.advertised_capabilities();
        assert_eq!(names(&caps), vec![Capability::FS_READ, Capability::MEMORY_READ]);
    }

    #[test]
    fn the_default_shell_wrapper_is_the_host_convention() {
        let invocation = host_shell("echo hello");
        #[cfg(windows)]
        {
            assert_eq!(invocation.program, "cmd");
            assert_eq!(invocation.args, vec!["/C", "echo hello"]);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(invocation.program, "sh");
            assert_eq!(invocation.args, vec!["-c", "echo hello"]);
        }
    }

    #[test]
    fn a_local_run_advertises_the_capabilities_the_build_ships() {
        let caps = SandboxMode::LocalProcess.advertised_capabilities();
        for expected in [
            Capability::FS_READ,
            Capability::FS_WRITE,
            Capability::GIT_STATUS,
            Capability::MEMORY_READ,
            Capability::MEMORY_WRITE,
            Capability::PROCESS_SPAWN,
            Capability::NETWORK_REQUEST,
            Capability::SUBAGENT_SPAWN,
        ] {
            assert!(names(&caps).contains(&expected), "{expected} is missing from {caps:?}");
        }
        assert_eq!(caps.len(), 8, "an unadvertised capability is a capability a rule cannot grant");
    }
}
