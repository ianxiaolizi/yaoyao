//! LocalExecutionEnvironment — a workspace-confined local file/process backend
//! (LocalProcess mode).
//!
//! Reads and writes are confined to the workspace root via canonicalization
//! (resolving `.`/`..` and symlinks); commands run only in `LocalProcess` mode
//! and only with a working directory inside that root. This is the
//! directory-level confinement the first version provides, not the
//! OS/container isolation of later backends.
//!
//! Two limits here are not decoration:
//!
//! * **A command that overruns its deadline is killed, not waited on.** The
//!   environment returns it as an ordinary result with the `timeout(1)` exit
//!   code, alongside whatever output it managed to produce first — the caller
//!   sees how far it got. A tool result is the right shape for that, because a
//!   slow command is an outcome of running the command, not a failure of the
//!   environment.
//! * **A command is not a sandbox.** Nothing here inspects what a shell command
//!   *will* do. `shell.run` is gated by approval and by the working directory it
//!   is given; its own writes are its own business. What this type does protect
//!   is the file-write path it controls: hooks and other executable metadata
//!   under `.git` may not be written through it, because a repository that can
//!   rewrite its own hooks has arranged to run code on the next commit.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use agent_ports::execution_environment::{
    CommandOutput, CommandSpec, DirEntry, DirEntryKind, ExecutionEnvironment,
    ExecutionEnvironmentError, SandboxMode, ShellInvocation, host_shell,
};
use agent_ports::process_env::child_environment;
use agent_ports::process_launcher::{
    LaunchError, ProcessLauncher, RunningProcess, SpawnRequest, StdinMode,
};
use agent_protocol::permissions::Capability;

/// Most output kept from one command, per stream.
///
/// Past this the excess is dropped and `truncated` is set. Draining continues
/// regardless: a child whose pipe fills up blocks forever, so "we stopped
/// keeping it" must never become "we stopped reading it".
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;

/// Most bytes one `read_file` will return.
///
/// The command cap above bounds what a process may say; this bounds what a
/// file may be. One tool call reading a multi-gigabyte file — or a special
/// file with no end at all — would otherwise put an unbounded allocation
/// behind a single model decision. Past the cap the read is **refused**, not
/// truncated: `fs.edit` reads through this same path, and an edit computed
/// from half a file would write the wrong content back without a trace.
pub const MAX_FILE_BYTES: usize = 1 << 20;

/// How often the deadline is re-checked while a child runs.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long the output readers get to finish after the child is gone. It only
/// matters when a killed process left a grandchild holding the pipe open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The exit code reported for a command killed at its deadline, matching
/// `timeout(1)` so the convention is not something a reader has to learn here.
const TIMEOUT_EXIT_CODE: i32 = 124;

/// Directories a write may not create or modify, relative to the workspace
/// root. Lower-cased and compared component-wise; on a case-insensitive
/// filesystem that is the only correct comparison, and on a case-sensitive one
/// it merely refuses a `.GIT/hooks` that nobody has a reason to write.
const PROTECTED_METADATA: &[&[&str]] = &[&[".git", "hooks"]];

/// Where the environment reads the parent process's variables from.
///
/// A function rather than a snapshot because the environment is built once and
/// runs for as long as the process does; tests replace it so the scrub can be
/// checked without mutating process-global state (`std::env::set_var` is
/// unsafe in edition 2024, and a test that changes the whole process's
/// environment races every other test).
type EnvironmentSource = Arc<dyn Fn() -> Vec<(OsString, OsString)> + Send + Sync>;

/// Which shell `shell.run` commands are wrapped in.
#[derive(Debug, Clone)]
enum Shell {
    /// The host convention: `cmd /C` on Windows, `sh -c` elsewhere.
    Host,
    /// PowerShell, named by path. Used where the host shell cannot start a
    /// child process: inside a Windows AppContainer `cmd.exe` is refused a
    /// child, while PowerShell is not.
    PowerShell(String),
}

/// A local execution backend confined to one workspace root.
pub struct LocalExecutionEnvironment {
    workspace_root: PathBuf,
    mode: SandboxMode,
    launcher: Arc<dyn ProcessLauncher>,
    parent_environment: EnvironmentSource,
    shell: Shell,
    /// Whether the backend can reach the network. Only meaningful for a mode
    /// whose confinement can take it away; the local process has whatever the
    /// host has.
    network_allowed: bool,
}

impl LocalExecutionEnvironment {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            mode: SandboxMode::LocalProcess,
            launcher: Arc::new(StdProcessLauncher),
            parent_environment: Arc::new(|| std::env::vars_os().collect()),
            shell: Shell::Host,
            network_allowed: true,
        }
    }

    /// The same backend with a different process launcher (an AppContainer or
    /// container launcher). File operations, the path boundary, the deadline
    /// and the output caps stay here; only spawning moves.
    pub fn with_launcher(
        workspace_root: impl Into<PathBuf>,
        launcher: Arc<dyn ProcessLauncher>,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            mode: SandboxMode::LocalProcess,
            launcher,
            parent_environment: Arc::new(|| std::env::vars_os().collect()),
            shell: Shell::Host,
            network_allowed: true,
        }
    }

    /// The AppContainer backend: commands run under a launcher that confines
    /// them to the workspace, and the shell is PowerShell because `cmd.exe`
    /// cannot start a child process once the token is a lowbox token.
    ///
    /// `network_allowed` is the one capability the launcher can take away; when
    /// it is false the advertisement leaves `network.request` out, so an HTTP
    /// MCP server is never started and a rule cannot grant it.
    pub fn app_container(
        workspace_root: impl Into<PathBuf>,
        launcher: Arc<dyn ProcessLauncher>,
        network_allowed: bool,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            mode: SandboxMode::WorkspaceSandbox,
            launcher,
            parent_environment: Arc::new(|| std::env::vars_os().collect()),
            shell: Shell::PowerShell(powershell_program()),
            network_allowed,
        }
    }

    /// A read-only backend: no writes and no command execution.
    ///
    /// Selected by `AGENT_SANDBOX=read-only`. The refusal happens here, at the
    /// environment, rather than only in the policy engine: a policy rule can be
    /// replaced by a profile, and "read-only" is a claim about the backend.
    pub fn read_only(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            mode: SandboxMode::ReadOnly,
            launcher: Arc::new(StdProcessLauncher),
            parent_environment: Arc::new(|| std::env::vars_os().collect()),
            shell: Shell::Host,
            network_allowed: true,
        }
    }

    /// Resolve `path` against the workspace root and verify it stays inside.
    /// Returns the absolute path to operate on.
    ///
    /// The path handed back is the canonical one — the very path the check was
    /// made on. Returning the caller's spelling instead would verify one path
    /// and open another, which is how a `..` (or a symlink) that reads as inside
    /// reaches the filesystem unchanged.
    fn resolve(&self, path: &str) -> Result<Resolved, ExecutionEnvironmentError> {
        resolve_within(&self.workspace_root, path)
    }

    /// How the AppContainer shell starts: inside a drive mapped onto the
    /// workspace root.
    ///
    /// The launcher grants the workspace and nothing above it, so PowerShell
    /// cannot always adopt the process working directory — when the workspace
    /// sits below directories the container may not open, PowerShell falls back
    /// to the drive root (or its own installation directory), and every
    /// relative path a command uses resolves somewhere else. Mapping the
    /// workspace onto a PowerShell drive and starting there makes the shell's
    /// root the sandbox's root: what the shell cannot reach above the
    /// workspace is also what it cannot name. `New-PSDrive` only builds a
    /// provider view; the process working directory (what a native child
    /// inherits) stays the real path.
    ///
    /// The drive is a named one (`workspace:`) rather than a letter so it
    /// cannot shadow a real drive.
    ///
    /// The output encoding is set to UTF-8 first: PowerShell 5.1 decodes a
    /// native program's output with the console code page (936 on this host)
    /// and writes its own messages the same way, so a UTF-8 child and a
    /// localized error both reached the agent as mojibake. Setting it is
    /// best-effort — a shell that cannot set it still runs the command.
    ///
    /// The exit code is taken from the native command (`$LASTEXITCODE`) when
    /// there is one. Without this, a command that succeeds but writes to stderr
    /// under `2>&1` exits 1: PowerShell 5.1 turns the redirected stderr line
    /// into an error record, and the script's success state follows the error
    /// record rather than the program's own exit code. A model then sees a
    /// failed tool result for a command that succeeded and either retries what
    /// already worked or edits what was never broken (2026-10-21 real use,
    /// `cargo test` 7/7 passing reported as `exit code 1`).
    fn app_container_command(&self, command: &str, cwd: &Path) -> String {
        let root = process_cwd(
            self.workspace_root.canonicalize().unwrap_or_else(|_| self.workspace_root.clone()),
        );
        // When the requested directory is inside the workspace the drive is
        // rooted at the workspace, so `..` stays inside the sandbox instead of
        // stopping at a drive root. When it is not (only possible if
        // canonicalization of the two differs), the command's own directory is
        // the least surprising root.
        let (root, relative) = match cwd.strip_prefix(&root) {
            Ok(relative) => (root, relative.to_path_buf()),
            Err(_) => (cwd.to_path_buf(), PathBuf::new()),
        };
        let mut location = String::from("workspace:\\");
        if !relative.as_os_str().is_empty() {
            location.push_str(&relative.to_string_lossy());
        }
        format!(
            "$__yaoyao_workspace = '{root}'\n\
             try {{\n\
             \x20   [Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n\
             }} catch {{\n\
             \x20   [Console]::Error.WriteLine('sandbox: could not set the output encoding \
             to UTF-8: ' + $_.Exception.Message)\n\
             }}\n\
             try {{\n\
             \x20   $null = New-PSDrive -Name workspace -PSProvider FileSystem -Root \
             $__yaoyao_workspace -ErrorAction Stop\n\
             \x20   Set-Location -LiteralPath '{location}' -ErrorAction Stop\n\
             }} catch {{\n\
             \x20   [Console]::Error.WriteLine('sandbox: the shell could not start inside \
             the workspace: ' + $_.Exception.Message)\n\
             \x20   exit 70\n\
             }}\n\
             {command}\n\
             $__yaoyao_ok = $?\n\
             $__yaoyao_exit = $LASTEXITCODE\n\
             if ($null -ne $__yaoyao_exit) {{ exit $__yaoyao_exit }}\n\
             if ($__yaoyao_ok) {{ exit 0 }} else {{ exit 1 }}",
            root = root.to_string_lossy().replace('\'', "''"),
            location = location.replace('\'', "''"),
            command = command,
        )
    }
}

/// A path that passed the boundary: the root it was checked against, and the
/// absolute path to operate on.
///
/// Both are already resolved, and every helper below takes them as given. A
/// helper must not resolve again: the second resolution is the one that can
/// disagree with the first, and then the check and the open see different
/// files.
#[derive(Debug)]
pub(crate) struct Resolved {
    pub(crate) root: PathBuf,
    pub(crate) full: PathBuf,
}

/// Resolve `path` against `root` and verify it stays inside, in process.
///
/// The kernel resolves the path (`canonicalize`), symlinks and all, and the
/// result is compared against the resolved root. This is the in-process
/// backend's resolver; the sandbox worker cannot use it ([`resolve_walked`]
/// says why) but answers with the same words.
fn resolve_within(root: &Path, path: &str) -> Result<Resolved, ExecutionEnvironmentError> {
    let root = root.canonicalize().map_err(|error| {
        ExecutionEnvironmentError::Io(io::Error::new(
            error.kind(),
            format!("the workspace root {} could not be resolved: {error}", root.display()),
        ))
    })?;
    let candidate =
        if Path::new(path).is_absolute() { PathBuf::from(path) } else { root.join(path) };

    // Canonicalize the candidate; for a not-yet-existing write target, fall
    // back to its deepest existing ancestor and re-attach the missing names.
    // A path that cannot be resolved that far — one whose missing tail ends in
    // `..` — is refused rather than passed through: "the kernel could not
    // resolve this" must not read as "this is inside".
    match canonicalize_lenient(&candidate) {
        Some(canonical) if canonical.starts_with(&root) => Ok(Resolved { root, full: canonical }),
        _ => Err(ExecutionEnvironmentError::PathOutsideWorkspace {
            path: path.to_string(),
            root: root.display().to_string(),
        }),
    }
}

/// How many reparse points one resolution may follow before it is refused.
///
/// Only a loop reaches the cap; a legitimate tree resolves in one or two hops.
const MAX_LINK_HOPS: usize = 32;

/// Resolve `path` against the already-resolved `root` without asking the
/// kernel for a final path.
///
/// `std::fs::canonicalize` cannot be used from inside the AppContainer: the
/// call ends in `GetFinalPathNameByHandleW`, and a container process is refused
/// that for every path — the workspace, `C:\Windows\System32`, even `C:\`.
/// Measured on this host with the worker's own diagnostics; reading, listing
/// and `symlink_metadata` all work, so the restriction is the query, not the
/// access. A worker that canonicalized therefore could not open one file.
///
/// So the path is resolved here instead: `.` and `..` are folded lexically, and
/// every reparse point met under the root is followed explicitly with
/// `read_link`, the whole resolution restarting from the link's target. The
/// candidate is compared against the root before each hop and again at the end,
/// so a link that points outside is refused like a `..` would be.
///
/// `root` must be absolute and already resolved by the caller (the host
/// resolves it; see [`SandboxedFileEnvironment`](crate::SandboxedFileEnvironment)).
pub(crate) fn resolve_walked(
    root: &Path,
    path: &str,
) -> Result<Resolved, ExecutionEnvironmentError> {
    // The kernel's canonical spelling is a verbatim path, which a caller's
    // `C:\...` spelling would never be a prefix of. The plain form names the
    // same directory and compares in the caller's terms.
    let root = process_cwd(root.to_path_buf());
    let root = root.as_path();
    let outside = || ExecutionEnvironmentError::PathOutsideWorkspace {
        path: path.to_string(),
        root: root.display().to_string(),
    };
    let mut candidate = if Path::new(path).is_absolute() {
        fold_lexically(Path::new(path))
    } else {
        fold_lexically(&root.join(path))
    };
    for _ in 0..=MAX_LINK_HOPS {
        if !candidate.starts_with(root) {
            return Err(outside());
        }
        match first_reparse_point(root, &candidate)? {
            None => return Ok(Resolved { root: root.to_path_buf(), full: candidate }),
            Some(link) => {
                let target = std::fs::read_link(&link).map_err(ExecutionEnvironmentError::Io)?;
                let parent =
                    link.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
                candidate = fold_lexically(&parent.join(target));
            }
        }
    }
    Err(ExecutionEnvironmentError::Io(io::Error::other(format!(
        "the path {path:?} follows more than {MAX_LINK_HOPS} links"
    ))))
}

/// Fold `.` and `..` without touching the filesystem.
///
/// A `..` at the top of the path is dropped rather than climbing above it: the
/// caller's containment check then refuses the result if the drop moved it out
/// of the root. Folding before any link is followed is deliberate — it can only
/// keep a path where it was spelled, never move it somewhere it was not.
fn fold_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => folded.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let above_is_name = folded
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if above_is_name {
                    folded.pop();
                }
            }
            Component::Normal(name) => folded.push(name),
        }
    }
    folded
}

/// The first component of `candidate` below `root` that is a reparse point.
///
/// A component that does not exist ends the walk: the remainder of the path is
/// something a write may create, and there is no link left in it to follow.
fn first_reparse_point(
    root: &Path,
    candidate: &Path,
) -> Result<Option<PathBuf>, ExecutionEnvironmentError> {
    let Ok(relative) = candidate.strip_prefix(root) else {
        return Ok(None);
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if is_reparse_point(&metadata) => return Ok(Some(current)),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ExecutionEnvironmentError::Io(error)),
        }
    }
    Ok(None)
}

/// Whether this metadata describes a link (a symlink or a junction).
///
/// `FileType::is_symlink` covers both on Windows, but the attributes are read
/// directly there as well: a junction whose tag this build's std does not know
/// would otherwise be treated as an ordinary directory and walked through.
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Whether a write to `full` would land in protected executable metadata.
///
/// Both paths are already resolved by the caller.
fn is_protected_metadata_in(root: &Path, full: &Path) -> bool {
    let Ok(relative) = full.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<String> =
        relative.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect();
    PROTECTED_METADATA.iter().any(|protected| {
        parts.windows(protected.len()).any(|window| {
            window.iter().zip(protected.iter()).all(|(part, name)| part.as_str() == *name)
        })
    })
}

/// Read one file, bounded.
///
/// `resolved` has already passed the boundary; `path` is the caller's spelling,
/// used only in the error.
pub(crate) fn read_file_at(
    resolved: &Resolved,
    path: &str,
) -> Result<String, ExecutionEnvironmentError> {
    let file = std::fs::File::open(&resolved.full).map_err(ExecutionEnvironmentError::Io)?;
    // One byte past the cap is enough to tell "at cap" from "past cap",
    // and the read is bounded whatever the file claims its size is.
    let mut text = String::new();
    file.take((MAX_FILE_BYTES as u64) + 1)
        .read_to_string(&mut text)
        .map_err(ExecutionEnvironmentError::Io)?;
    if text.len() > MAX_FILE_BYTES {
        return Err(ExecutionEnvironmentError::FileTooLarge {
            path: path.to_string(),
            limit: MAX_FILE_BYTES,
        });
    }
    Ok(text)
}

/// List one directory, sorted by name.
pub(crate) fn list_dir_at(resolved: &Resolved) -> Result<Vec<DirEntry>, ExecutionEnvironmentError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&resolved.full).map_err(ExecutionEnvironmentError::Io)? {
        let entry = entry.map_err(ExecutionEnvironmentError::Io)?;
        // `DirEntry::metadata` describes the link itself rather than its
        // target, which is what makes "is this a symlink" answerable here.
        let file_type = entry.file_type().map_err(ExecutionEnvironmentError::Io)?;
        let kind = if file_type.is_symlink() {
            DirEntryKind::Symlink
        } else if file_type.is_dir() {
            DirEntryKind::Directory
        } else if file_type.is_file() {
            DirEntryKind::File
        } else {
            DirEntryKind::Other
        };
        let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size: if kind == DirEntryKind::File { size } else { 0 },
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// Write one file, refusing protected metadata.
pub(crate) fn write_file_at(
    resolved: &Resolved,
    path: &str,
    content: &str,
) -> Result<(), ExecutionEnvironmentError> {
    let full = &resolved.full;
    if is_protected_metadata_in(&resolved.root, full) {
        return Err(ExecutionEnvironmentError::ProtectedPath(path.to_string()));
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(ExecutionEnvironmentError::Io)?;
    }
    std::fs::write(full, content).map_err(ExecutionEnvironmentError::Io)
}

/// The PowerShell to wrap AppContainer commands in: PowerShell 7 when it is
/// installed, Windows PowerShell 5.1 otherwise.
///
/// The two differ in one way a command author has to know about: PowerShell 7
/// supports `&&` and `||`; 5.1 does not. That difference is documented in the
/// contract rather than papered over — the alternative, rewriting a command
/// string into separate statements, would guess at quoting the command's author
/// already resolved.
fn powershell_program() -> String {
    #[cfg(windows)]
    {
        if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                let candidate = directory.join("pwsh.exe");
                if candidate.is_file() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
        }
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        Path::new(&root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(windows))]
    {
        // No AppContainer exists off Windows; this keeps the type constructible
        // so the crate still compiles where the backend is never selected.
        "pwsh".to_string()
    }
}

/// Whether the shell the AppContainer backend would use supports `&&` and
/// `||`.
///
/// The doctor reports this next to the backend: a command author who writes
/// `build && test` gets a syntax error under 5.1, and the report is where that
/// difference is visible before it becomes one.
pub fn app_container_shell_supports_and_and() -> bool {
    // `pwsh.exe` is PowerShell 7; `powershell.exe` is the 5.1 that ships with
    // Windows. Nothing else is ever returned by `powershell_program`.
    !powershell_program().to_ascii_lowercase().ends_with("powershell.exe")
}

/// The shell program the AppContainer backend would start, for the doctor.
pub fn app_container_shell() -> String {
    powershell_program()
}

/// The working directory to hand the child process.
///
/// `Path::canonicalize` returns an extended-length path (`\\?\G:\…`) on
/// Windows. `CreateProcessW` accepts it; PowerShell 5.1 is the one that does
/// not — it can fail to treat it as a location and leave the command running
/// somewhere else. The plain form is what it expects, and the location is set
/// explicitly anyway (`app_container_command`).
///
/// The prefix is kept for UNC paths (dropping it would produce a path that
/// means something else) and for paths past `MAX_PATH`, where it is load
/// bearing.
pub(crate) fn process_cwd(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(plain) = text.strip_prefix(r"\\?\")
            && !plain.starts_with("UNC\\")
            && plain.len() < 260
        {
            return PathBuf::from(plain);
        }
    }
    path
}

/// Canonicalize `path`, falling back to canonicalizing its parent when the leaf
/// does not exist yet (the shape of every write to a new file).
///
/// `None` when neither resolves. The parent fallback is the only leniency here
/// on purpose: with the parent resolved, a `..` above it has already been
/// flattened by the kernel, so the joined leaf is compared in the same terms as
/// the root. When even the parent cannot be resolved there is nothing to hold
/// the leaf against, and a path that cannot be checked is not one to admit.
/// Canonicalize `path`, tolerating names that do not exist yet.
///
/// The kernel resolves a path the caller intends to write, and the leaf of such
/// a path usually does not exist. Only canonicalizing the *parent* covered a
/// missing leaf but not a missing directory above it: `fs.write` to
/// `reports/summary.txt` in a workspace without `reports` was refused as a
/// boundary violation, which read as a policy denial rather than as "the tool
/// did not create the directory it advertises creating" (2026-10-21 real use).
///
/// So the deepest ancestor the kernel *can* resolve is canonicalized and the
/// missing names are re-attached verbatim. Re-attaching is sound only because
/// every name in that tail is a plain one: [`Path::file_name`] yields `None`
/// for a tail that ends in `..` or `.`, so such a path is refused instead of
/// being folded without the kernel. The caller still compares the result
/// against the root, and the resolved base is the kernel's spelling, not the
/// caller's.
fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }
    let mut missing: Vec<OsString> = Vec::new();
    let mut ancestor = path;
    loop {
        missing.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
        if let Ok(base) = ancestor.canonicalize() {
            let mut resolved = base;
            for name in missing.iter().rev() {
                resolved.push(name);
            }
            return Some(resolved);
        }
    }
}

impl ExecutionEnvironment for LocalExecutionEnvironment {
    fn name(&self) -> &str {
        "local"
    }

    fn mode(&self) -> SandboxMode {
        self.mode
    }

    fn advertised_capabilities(&self) -> Vec<Capability> {
        let mut capabilities = self.mode.advertised_capabilities();
        if !self.network_allowed {
            capabilities
                .retain(|capability| *capability != Capability::new(Capability::NETWORK_REQUEST));
        }
        capabilities
    }

    fn shell_invocation(&self, command: &str) -> ShellInvocation {
        match &self.shell {
            Shell::Host => host_shell(command),
            // `-NoProfile`: a profile script is user configuration the sandbox
            // did not ask for, and it could change what the command means.
            // `-NonInteractive`: nothing in this backend has a console to answer
            // a prompt on, so a prompt must fail instead of waiting.
            Shell::PowerShell(program) => ShellInvocation {
                program: program.clone(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ],
            },
        }
    }

    fn shell_language(&self) -> Option<&'static str> {
        // Only the AppContainer backend substitutes PowerShell; `cmd.exe` cannot
        // start a child process under a lowbox token.
        matches!(self.shell, Shell::PowerShell(_)).then_some("PowerShell")
    }

    fn read_file(&self, path: &str) -> Result<String, ExecutionEnvironmentError> {
        read_file_at(&self.resolve(path)?, path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, ExecutionEnvironmentError> {
        // Listing is a read, so it is allowed in ReadOnly mode; the path still
        // goes through the same boundary as a read of one of these files.
        list_dir_at(&self.resolve(path)?)
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), ExecutionEnvironmentError> {
        if self.mode == SandboxMode::ReadOnly {
            return Err(ExecutionEnvironmentError::NotPermitted { mode: self.mode });
        }
        write_file_at(&self.resolve(path)?, path, content)
    }

    fn run_command(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecutionEnvironmentError> {
        if self.mode == SandboxMode::ReadOnly {
            return Err(ExecutionEnvironmentError::NotPermitted { mode: self.mode });
        }
        // The working directory goes through the same boundary as a file path:
        // a command may no more choose to run outside the workspace than it may
        // read outside it.
        let cwd = match &spec.cwd {
            Some(c) => self.resolve(c)?.full,
            None => self.workspace_root.canonicalize().map_err(ExecutionEnvironmentError::Io)?,
        };
        // Checked here rather than left to the spawn: the OS answers a missing
        // directory with "the directory name is invalid" (os error 267), which
        // does not name the argument that was wrong. The model chose the value,
        // so the refusal has to quote it — otherwise the next attempt has
        // nothing to change.
        if !cwd.is_dir() {
            return Err(ExecutionEnvironmentError::CwdUnusable {
                path: spec.cwd.clone().unwrap_or_else(|| cwd.display().to_string()),
                root: self.workspace_root.display().to_string(),
            });
        }
        let cwd = process_cwd(cwd);

        // The child gets the allowlisted environment, never the parent's whole
        // one: the parent holds the model key (`process_env` says why).
        let env = child_environment(&(self.parent_environment)());
        let mut args = spec.args.clone();
        if matches!(self.shell, Shell::PowerShell(_))
            && let Some(index) = args.iter().position(|arg| arg == "-Command")
            && let Some(command) = args.get(index + 1).cloned()
        {
            args[index + 1] = self.app_container_command(&command, &cwd);
        }
        let request = SpawnRequest {
            program: spec.program.clone(),
            args,
            cwd,
            env,
            stdin: StdinMode::Closed,
        };
        let process = self.launcher.spawn(request).map_err(|error| match error {
            LaunchError::Io(error) => ExecutionEnvironmentError::Io(error),
            LaunchError::BackendUnavailable(reason) => {
                ExecutionEnvironmentError::BackendUnavailable(reason)
            }
        })?;
        // Armed until the process is known to be reaped. An early return — the
        // `try_wait` error path below — would otherwise leave the child running
        // with nothing left that knows its id.
        let mut child = ReapOnDrop { process, reaped: false };

        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        // Read on their own threads: the pipes need draining while this thread
        // watches the deadline, and a single thread can only do one of those.
        let out_reader = child.process.take_stdout().map(|stdout| {
            thread::spawn(move || {
                let _ = out_tx.send(read_capped(stdout));
            })
        });
        let err_reader = child.process.take_stderr().map(|stderr| {
            thread::spawn(move || {
                let _ = err_tx.send(read_capped(stderr));
            })
        });

        let deadline = spec.timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let mut timed_out = false;
        let status = loop {
            match child.process.try_wait() {
                Ok(Some(code)) => break Some(code),
                Ok(None) => {}
                Err(e) => {
                    // Do not leave a process behind on the one path that has no
                    // status to report: kill it, reap it, then report the error.
                    let _ = child.process.kill_tree();
                    let _ = child.process.wait();
                    child.reaped = true;
                    return Err(ExecutionEnvironmentError::Io(e));
                }
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                timed_out = true;
                let _ = child.process.kill_tree();
                // Reap it: a child that is never waited on stays a zombie.
                break child.process.wait().ok().flatten();
            }
            thread::sleep(POLL_INTERVAL);
        };
        child.reaped = true;

        let (stdout, out_cut) = collect(out_rx, out_reader);
        let (mut stderr, err_cut) = collect(err_rx, err_reader);
        if timed_out {
            let banner = format!(
                "command killed: it exceeded its {} ms timeout",
                spec.timeout_ms.unwrap_or_default()
            );
            stderr = if stderr.is_empty() { banner } else { format!("{banner}\n{stderr}") };
        }

        let exit_code = if timed_out { TIMEOUT_EXIT_CODE } else { status.unwrap_or(-1) };
        Ok(CommandOutput { exit_code, stdout, stderr, truncated: out_cut || err_cut })
    }
}

/// Wait for one stream's reader to report, bounded by the drain grace.
///
/// A reader that never reports has lost its pipe to a process that outlived the
/// command; its thread is left to end on its own rather than wedging the turn.
fn collect(
    rx: mpsc::Receiver<(String, bool)>,
    reader: Option<thread::JoinHandle<()>>,
) -> (String, bool) {
    match rx.recv_timeout(DRAIN_GRACE) {
        Ok(result) => {
            // Reported already, so the join is immediate — and joining keeps the
            // thread from outliving the call in the ordinary case.
            if let Some(handle) = reader {
                let _ = handle.join();
            }
            result
        }
        // Never reported: joining here would block for as long as the process
        // holding the pipe lives, which is exactly the wedge this avoids.
        Err(_) => (String::new(), false),
    }
}

/// Read a stream to the end, keeping at most [`MAX_OUTPUT_BYTES`] of it.
///
/// The `bool` is whether anything past the cap was dropped. Reading continues
/// after the cap so the writer is never blocked on a full pipe.
fn read_capped(mut reader: impl Read) -> (String, bool) {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut cut = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                let take = room.min(n);
                kept.extend_from_slice(&chunk[..take]);
                if take < n {
                    cut = true;
                }
            }
        }
    }
    (decode_stream(&kept), cut)
}

/// Decode one captured stream into text.
///
/// UTF-8 comes first because many tools emit it regardless of the console.
/// Otherwise a Windows command wrote the console's OEM code page (GBK on a
/// Chinese system), and reading that as UTF-8 turns every localized failure
/// message into replacement characters — exactly the text a model needs in
/// order to decide what to adjust. GBK is decoded per the Encoding Standard;
/// on a host whose OEM page is something else the bytes still come back as
/// text, and the lossy fallback keeps whatever cannot be decoded visible as
/// `�` rather than silently dropping it.
///
/// The one case that must not be mistaken for GBK is a UTF-8 stream the output
/// cap cut mid-character: its prefix is valid and only the tail is incomplete,
/// so it is decoded lossily instead of through the other code page.
fn decode_stream(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(e) if e.valid_up_to() + 8 >= bytes.len() && e.error_len().is_none() => {
            String::from_utf8_lossy(bytes).into_owned()
        }
        Err(_) => {
            #[cfg(windows)]
            {
                let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
                if !had_errors {
                    return text.into_owned();
                }
            }
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

/// The plain local launcher: one `std::process::Command`, with the scrubbed
/// environment and piped output.
pub struct StdProcessLauncher;

struct StdProcess {
    child: Child,
    /// Kept so `taskkill` runs with the same scrubbed environment as the child
    /// it is killing, rather than with the agent's whole environment.
    env: Vec<(OsString, OsString)>,
}

impl ProcessLauncher for StdProcessLauncher {
    fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn RunningProcess>, LaunchError> {
        let stdin = match request.stdin {
            StdinMode::Closed => Stdio::inherit(),
            StdinMode::Pipe => Stdio::piped(),
        };
        let child = Command::new(&request.program)
            .args(&request.args)
            .current_dir(&request.cwd)
            .env_clear()
            .envs(request.env.iter().map(|(name, value)| (name, value)))
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Box::new(StdProcess { child, env: request.env }))
    }
}

impl RunningProcess for StdProcess {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.child.try_wait()?.map(|status| status.code().unwrap_or(-1)))
    }

    fn wait(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(self.child.wait()?.code().unwrap_or(-1)))
    }

    /// A `shell.run` that started a build server has children that outlive the
    /// shell. Killing only the direct child would leave them running while the
    /// tool reports the command finished, so on Windows the whole tree goes.
    fn kill_tree(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            let pid = self.child.id().to_string();
            let killed = Command::new("taskkill")
                .args(["/T", "/F", "/PID", pid.as_str()])
                .env_clear()
                .envs(self.env.iter().map(|(name, value)| (name, value)))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !killed {
                // Already gone is the common case for the fallback; it is not
                // an error the caller can act on.
                let _ = self.child.kill();
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            // Reaching the whole process group needs a POSIX API the workspace
            // does not allow itself (unsafe is denied), so only the direct child
            // dies here. The bounded drain above is what keeps that from wedging
            // the turn. Full process-group containment is a known gap, not a
            // claim to make now.
            let _ = self.child.kill();
            Ok(())
        }
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.child.stdout.take().map(|stream| Box::new(stream) as Box<dyn Read + Send>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.child.stderr.take().map(|stream| Box::new(stream) as Box<dyn Read + Send>)
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.child.stdin.take().map(|stream| Box::new(stream) as Box<dyn Write + Send>)
    }
}

/// Kills and reaps the process on any path that returns before it was reaped.
///
/// The ordinary completion path and the timeout path both reap explicitly; this
/// exists for the paths nobody wrote on purpose — an error return, a panic, a
/// future `?` added between spawn and wait. `std::process::exit` still bypasses
/// destructors; a job object (the sandboxed backends) is the mechanism that
/// survives that, because the kernel closes the handle.
struct ReapOnDrop {
    process: Box<dyn RunningProcess>,
    reaped: bool,
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.process.kill_tree();
            let _ = self.process.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that sleeps well past any timeout a test sets, in the host's
    /// own shell so the test does not depend on a binary being installed.
    fn sleeper() -> CommandSpec {
        #[cfg(windows)]
        let (program, args) = ("cmd", vec!["/C", "ping -n 6 127.0.0.1"]);
        #[cfg(not(windows))]
        let (program, args) = ("sh", vec!["-c", "sleep 5"]);
        CommandSpec {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            timeout_ms: None,
        }
    }

    /// A `cwd` that is not a directory is refused with the value quoted.
    ///
    /// The premise is a real one: a small model asked to run a command wrote
    /// `"cwd": "{{workspace_root}}"` — a placeholder it invented — and the
    /// spawn failed with `os error 267`, which says neither which argument was
    /// wrong nor what to do. The next attempt could only guess.
    #[test]
    fn a_cwd_that_is_not_a_directory_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let spec = sleeper();
        let spec = CommandSpec { cwd: Some("{{workspace_root}}".to_string()), ..spec };

        let error = env.run_command(&spec).unwrap_err();
        assert!(
            matches!(error, ExecutionEnvironmentError::CwdUnusable { .. }),
            "expected a cwd refusal, got {error:?}"
        );
        let text = error.to_string();
        assert!(text.contains("{{workspace_root}}"), "the value is not named: {text}");
        assert!(text.contains("omit `cwd`"), "no way out is offered: {text}");
        assert!(text.contains(dir.path().to_string_lossy().as_ref()), "the root is not named");
    }

    #[test]
    fn read_inside_workspace_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        assert_eq!(env.read_file("a.txt").unwrap(), "hello");
    }

    #[test]
    fn a_file_past_the_read_cap_is_refused_rather_than_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());

        let refused = env.read_file("big.txt").unwrap_err();
        assert!(
            matches!(
                refused,
                ExecutionEnvironmentError::FileTooLarge { limit, .. } if limit == MAX_FILE_BYTES
            ),
            "{refused}"
        );
        // A file exactly at the cap is still readable: the boundary is a cap,
        // not a margin.
        std::fs::write(dir.path().join("at-cap.txt"), vec![b'a'; MAX_FILE_BYTES]).unwrap();
        assert_eq!(env.read_file("at-cap.txt").unwrap().len(), MAX_FILE_BYTES);
    }

    #[test]
    fn read_outside_workspace_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("s.txt");
        std::fs::write(&secret, "secret").unwrap();
        let refused = env.read_file(secret.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(refused, ExecutionEnvironmentError::PathOutsideWorkspace { .. }),
            "{refused:?}"
        );
        // The path alone leaves the reader to work out what the workspace even
        // was before they can tell whether this was a mistake or the root is
        // narrower than they thought.
        let message = refused.to_string();
        let root = std::fs::canonicalize(dir.path()).unwrap().display().to_string();
        assert!(message.contains(&root), "the root is named: {message}");
    }

    /// Spell `relative` a second way: absolute, by pasting it onto the root.
    ///
    /// A caller reaches the boundary by either spelling, and they are not
    /// equivalent to it: an absolute path skips the join the relative one goes
    /// through, and on Windows that join runs against a canonicalized (verbatim)
    /// root, where `..` is folded away before the check ever sees it. The
    /// absolute spelling is therefore the one that carries the `..` all the way
    /// to the check — a test with only the relative form would pass on a host
    /// that never had the defect to begin with.
    fn also_spelled_absolutely(root: &Path, relative: &str) -> String {
        format!("{}/{relative}", root.display())
    }

    #[test]
    fn a_path_whose_parent_does_not_exist_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let root = std::fs::canonicalize(dir.path()).unwrap();

        // `nope` is not there, so the kernel cannot resolve `nope/..` and each
        // `..` runs against the workspace root itself. Read as text the path is
        // inside; resolved it is the workspace's parent. The refusal has to come
        // from the boundary, not from the filesystem failing to open the file —
        // otherwise the check and the open are answering about different paths.
        //
        // The failing half of this is the POSIX one: there `canonicalize` leaves
        // the `..` in place when `nope` is missing, so the check has nothing but
        // the caller's text to work from. On Windows `canonicalize` folds the
        // `..` away by itself and the boundary refuses either way, so this test
        // holds on both the old and the new code there — it guards POSIX.
        let relative = "nope/../../etc/passwd";
        for path in [relative.to_string(), also_spelled_absolutely(&root, relative)] {
            let refused = env.read_file(&path).unwrap_err();
            assert!(
                matches!(refused, ExecutionEnvironmentError::PathOutsideWorkspace { .. }),
                "{path} was answered with {refused:?}"
            );
        }
    }

    #[test]
    fn a_write_creates_the_directories_above_a_missing_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());

        // The tool description promises "creating it"; neither the leaf nor the
        // directory above it exists. This used to be refused as a boundary
        // violation because `resolve` gave up one level above the leaf.
        env.write_file("reports/deep/summary.txt", "x\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("reports/deep/summary.txt")).unwrap(),
            "x\n"
        );
    }

    #[test]
    fn a_missing_path_outside_the_workspace_is_still_denied() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let path = outside.path().join("not-yet").join("f.txt");

        // The leaf and its directory are both missing, so the kernel cannot
        // canonicalize the path; resolving the existing ancestor must still
        // compare that ancestor against the root.
        let refused = env.write_file(&path.to_string_lossy(), "x").unwrap_err();
        assert!(
            matches!(refused, ExecutionEnvironmentError::PathOutsideWorkspace { .. }),
            "{refused:?}"
        );
    }

    #[test]
    fn resolve_hands_back_the_path_it_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let root = std::fs::canonicalize(dir.path()).unwrap();

        // Whichever way it is spelled, the check is made on the canonical path,
        // so that is the one the caller is handed; returning the spelling the
        // caller used would verify one path and open another.
        let relative = "sub/../f.txt";
        for path in [relative.to_string(), also_spelled_absolutely(&root, relative)] {
            let resolved = env.resolve(&path).unwrap().full;
            assert!(
                !resolved.components().any(|c| matches!(
                    c,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )),
                "{path} resolved to {resolved:?}"
            );
            assert_eq!(
                resolved,
                std::fs::canonicalize(dir.path().join("f.txt")).unwrap(),
                "{path}"
            );
        }
    }

    /// Make a file symlink, or say the machine's policy would not allow one.
    ///
    /// Windows needs a privilege to create one, so a test that depends on it
    /// reports "cannot" and returns instead of failing.
    fn symlink_file(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[test]
    fn the_walked_resolver_folds_dots_and_keeps_paths_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = process_cwd(std::fs::canonicalize(dir.path()).unwrap());
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/f.txt"), "x").unwrap();

        let resolved = resolve_walked(&root, "sub/../sub/f.txt").unwrap();
        assert_eq!(resolved.root, root);
        assert_eq!(resolved.full, root.join("sub").join("f.txt"));

        // An absolute spelling of the same path is admitted as well.
        let absolute = root.join("sub").join("f.txt").display().to_string();
        assert_eq!(resolve_walked(&root, &absolute).unwrap().full, resolved.full);

        // A leaf that does not exist yet is a write target, not an error.
        assert_eq!(resolve_walked(&root, "sub/new.txt").unwrap().full, root.join("sub/new.txt"));
    }

    #[test]
    fn the_walked_resolver_refuses_a_path_that_climbs_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for path in ["..", "../outside.txt", "sub/../../outside.txt"] {
            let refused = resolve_walked(&root, path).unwrap_err();
            assert!(
                matches!(refused, ExecutionEnvironmentError::PathOutsideWorkspace { .. }),
                "{path} was answered with {refused:?}"
            );
        }
        let outside = std::env::temp_dir().join("yaoyao-outside.txt");
        let refused = resolve_walked(&root, &outside.display().to_string()).unwrap_err();
        assert!(
            matches!(refused, ExecutionEnvironmentError::PathOutsideWorkspace { .. }),
            "{refused:?}"
        );
    }

    #[test]
    fn the_walked_resolver_follows_a_link_that_stays_inside_and_refuses_one_that_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = process_cwd(std::fs::canonicalize(dir.path()).unwrap());
        let target = root.join("real.txt");
        std::fs::write(&target, "inside").unwrap();
        let outside_target = outside.path().join("secret.txt");
        std::fs::write(&outside_target, "secret").unwrap();

        if !symlink_file(&target, &root.join("inside-link")) {
            eprintln!("skipped: this machine does not allow creating symlinks");
        } else {
            let resolved = resolve_walked(&root, "inside-link").unwrap();
            assert_eq!(
                resolved.full,
                process_cwd(std::fs::canonicalize(&target).unwrap()),
                "a link that stays inside resolves to its target"
            );
        }

        if !symlink_file(&outside_target, &root.join("outside-link")) {
            eprintln!("skipped: this machine does not allow creating symlinks");
        } else {
            let refused = resolve_walked(&root, "outside-link").unwrap_err();
            assert!(
                matches!(refused, ExecutionEnvironmentError::PathOutsideWorkspace { .. }),
                "a link out of the workspace was answered with {refused:?}"
            );
        }
    }

    #[test]
    fn write_is_denied_in_read_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::read_only(dir.path().to_path_buf());
        assert!(matches!(
            env.write_file("x.txt", "x"),
            Err(ExecutionEnvironmentError::NotPermitted { .. })
        ));
    }

    #[test]
    fn command_is_denied_in_read_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::read_only(dir.path().to_path_buf());
        let spec =
            CommandSpec { program: "echo".into(), args: vec![], cwd: None, timeout_ms: None };
        assert!(matches!(
            env.run_command(&spec),
            Err(ExecutionEnvironmentError::NotPermitted { .. })
        ));
    }

    #[test]
    fn an_app_container_environment_runs_shell_commands_through_powershell() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let launcher = Arc::new(FakeLauncher { behaviour: Behaviour::Exit(0), recorded });
        let env =
            LocalExecutionEnvironment::app_container(dir.path().to_path_buf(), launcher, false);

        assert_eq!(env.mode(), SandboxMode::WorkspaceSandbox);
        let invocation = env.shell_invocation("echo hi");
        // `cmd.exe` cannot start a child once the token is a lowbox token; the
        // shell that can is PowerShell, so the wrapper is not the host's.
        assert!(
            invocation.program.to_ascii_lowercase().ends_with("powershell.exe")
                || invocation.program.to_ascii_lowercase().ends_with("pwsh.exe"),
            "wrapper was {:?}",
            invocation.program
        );
        assert_eq!(invocation.args, vec!["-NoProfile", "-NonInteractive", "-Command", "echo hi"]);
    }

    #[test]
    fn a_denied_network_is_taken_out_of_the_app_container_advertisement() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = |behaviour| {
            Arc::new(FakeLauncher {
                behaviour,
                recorded: Arc::new(Mutex::new(Recorded::default())),
            })
        };

        let denied = LocalExecutionEnvironment::app_container(
            dir.path().to_path_buf(),
            launcher(Behaviour::Exit(0)),
            false,
        );
        let denied_caps = denied.advertised_capabilities();
        assert!(!denied_caps.contains(&Capability::new(Capability::NETWORK_REQUEST)));
        assert!(denied_caps.contains(&Capability::new(Capability::PROCESS_SPAWN)));

        let allowed = LocalExecutionEnvironment::app_container(
            dir.path().to_path_buf(),
            launcher(Behaviour::Exit(0)),
            true,
        );
        assert!(
            allowed
                .advertised_capabilities()
                .contains(&Capability::new(Capability::NETWORK_REQUEST))
        );
    }

    #[test]
    fn writing_a_hook_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path().join(".git").join("hooks")).unwrap();
        // Every path below is resolved by the kernel before it is checked, so
        // each naming a directory that is not there would be refused as
        // unresolvable. The fixture supplies the three that the spellings need:
        // `sub` for `sub/..`, `.GIT/HOOKS` for the case variant (a different
        // directory on a case-sensitive filesystem), and `hooks` for the
        // look-alike written at the end. These used to pass without existing
        // only because an unresolvable path was handed to the guard as a string.
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::create_dir_all(dir.path().join(".GIT").join("HOOKS")).unwrap();
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();

        // A repository that can rewrite its own hooks has arranged to run code
        // on the next commit; the write path has to refuse, however the path is
        // spelled.
        for path in [
            ".git/hooks/pre-commit",
            "./.git/hooks/pre-commit",
            ".git/./hooks/pre-commit",
            "sub/../.git/hooks/pre-commit",
            ".GIT/HOOKS/pre-commit",
        ] {
            assert!(
                matches!(
                    env.write_file(path, "#!/bin/sh\nrm -rf /\n"),
                    Err(ExecutionEnvironmentError::ProtectedPath(_))
                ),
                "{path} was allowed"
            );
        }
        assert!(!dir.path().join(".git/hooks/pre-commit").exists());

        // The rest of `.git`, and a directory that merely looks like it, stay
        // writable — this is a guard on one name, not a second sandbox.
        env.write_file(".git/config", "[core]\n").unwrap();
        env.write_file("hooks/pre-commit", "harmless\n").unwrap();
    }

    #[test]
    fn an_explicit_cwd_outside_the_workspace_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let mut spec = sleeper();
        spec.timeout_ms = Some(1_000);
        spec.cwd = Some(outside.path().to_string_lossy().into_owned());

        // The command never starts: the boundary is decided before anything is
        // spawned, so "the cwd was rejected" cannot be confused with "it ran
        // and failed".
        assert!(matches!(
            env.run_command(&spec),
            Err(ExecutionEnvironmentError::PathOutsideWorkspace { .. })
        ));
    }

    #[test]
    fn a_command_that_overruns_its_deadline_is_killed_and_reported_as_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let mut spec = sleeper();
        spec.timeout_ms = Some(300);

        let started = Instant::now();
        let output = env.run_command(&spec).expect("a timeout is an outcome, not an error");
        let elapsed = started.elapsed();

        assert_eq!(output.exit_code, TIMEOUT_EXIT_CODE);
        assert!(output.stderr.contains("timeout"), "stderr was {:?}", output.stderr);
        // The point of the deadline: it is enforced, not merely declared. The
        // sleeper runs for seconds; anything near that means it was waited on.
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    }

    // Only Windows kills the tree; elsewhere `kill_tree` falls back to killing
    // the direct child, so this test would assert a guarantee the platform does
    // not make. The gap is real and recorded in `kill_tree`, not hidden here.
    #[cfg(windows)]
    #[test]
    fn a_timed_out_command_takes_its_descendants_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        // Three levels: the shell we start, a second shell it starts, and a
        // ping inside that. Only the last one outlives the deadline, and only
        // if nothing killed it.
        // Absolute paths throughout: on a host with
        // `NoDefaultCurrentDirectoryInExePath=1`, `cmd` resolves neither the
        // batch file nor the shell it starts from the working directory, and the
        // fixture would fail for a reason that has nothing to do with the kill.
        let survivor = dir.path().join("survivor.bat");
        std::fs::write(
            dir.path().join("outer.bat"),
            format!("cmd /C \"{}\"\r\n", survivor.display()),
        )
        .unwrap();
        std::fs::write(&survivor, "ping -n 3 127.0.0.1 > nul\r\necho survivor > marker.txt\r\n")
            .unwrap();

        let spec = CommandSpec {
            program: "cmd".to_string(),
            args: vec![
                "/C".to_string(),
                dir.path().join("outer.bat").to_string_lossy().into_owned(),
            ],
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            timeout_ms: Some(300),
        };
        let output = env.run_command(&spec).expect("a timeout is an outcome, not an error");
        assert_eq!(output.exit_code, TIMEOUT_EXIT_CODE);

        // Well past the point where an orphaned grandchild would have finished
        // its ping and written the marker. A surviving descendant is not a
        // leftover detail: it is the build server the user thought was stopped.
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !dir.path().join("marker.txt").exists(),
            "a descendant outlived the deadline and wrote marker.txt"
        );

        // Killing a tree must not leave the environment itself unusable.
        let after = env
            .run_command(&CommandSpec {
                program: "cmd".to_string(),
                args: vec!["/C".to_string(), "echo ok".to_string()],
                cwd: None,
                timeout_ms: Some(10_000),
            })
            .expect("the environment still runs commands");
        assert_eq!(after.exit_code, 0);
    }

    #[test]
    fn a_finished_command_reports_its_exit_code_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        #[cfg(windows)]
        let (program, args) = ("cmd", vec!["/C", "echo hello & exit 3"]);
        #[cfg(not(windows))]
        let (program, args) = ("sh", vec!["-c", "echo hello; exit 3"]);
        let spec = CommandSpec {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            timeout_ms: Some(30_000),
        };

        let output = env.run_command(&spec).unwrap();
        assert_eq!(output.exit_code, 3);
        assert!(output.stdout.contains("hello"), "stdout was {:?}", output.stdout);
        assert!(!output.truncated);
    }

    #[test]
    fn output_past_the_cap_is_kept_partial_and_flagged() {
        let flood = vec![b'x'; MAX_OUTPUT_BYTES + 4_096];

        let (kept, cut) = read_capped(std::io::Cursor::new(flood));

        assert!(cut);
        assert_eq!(kept.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn output_within_the_cap_is_kept_whole_and_unflagged() {
        let (kept, cut) = read_capped(std::io::Cursor::new(b"short".to_vec()));
        assert_eq!(kept, "short");
        assert!(!cut);
    }

    #[cfg(windows)]
    #[test]
    fn gbk_output_from_a_command_is_decoded_as_text() {
        // "错误" in GBK, followed by ASCII: the shape a localized Windows
        // command writes to a redirected pipe. Read as UTF-8 it is noise, and
        // noise is not a reason a model can act on.
        let (kept, cut) =
            read_capped(std::io::Cursor::new(vec![0xb4, 0xed, 0xce, 0xf3, b':', b' ', b'4', b'2']));
        assert!(!cut);
        assert_eq!(kept, "错误: 42");
    }

    #[test]
    fn a_utf8_stream_cut_mid_character_still_reads_as_utf8() {
        // The cap can slice a multi-byte character in half. That tail is
        // incomplete UTF-8, not a different encoding, and the text before it
        // must not be re-decoded as GBK and mangled.
        let mut bytes = "完成".as_bytes().to_vec();
        bytes.pop();
        let text = decode_stream(&bytes);
        assert!(text.starts_with('完'), "{text:?}");
        assert!(text.contains('\u{FFFD}'), "{text:?}");
    }

    // The launcher seam exists so the environment's behavior around a process
    // — what environment it is given, what happens when waiting fails — can be
    // checked without depending on a real process's timing.

    use std::sync::Mutex;

    #[derive(Clone, Copy)]
    enum Behaviour {
        Exit(i32),
        /// Never finishes: the environment has to enforce the deadline.
        Hang,
        /// The platform can no longer tell us about the process.
        Fail,
    }

    #[derive(Default)]
    struct Recorded {
        requests: Vec<SpawnRequest>,
        kills: usize,
        waits: usize,
    }

    struct FakeLauncher {
        behaviour: Behaviour,
        recorded: Arc<Mutex<Recorded>>,
    }

    struct FakeProcess {
        behaviour: Behaviour,
        recorded: Arc<Mutex<Recorded>>,
    }

    impl ProcessLauncher for FakeLauncher {
        fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn RunningProcess>, LaunchError> {
            self.recorded.lock().unwrap().requests.push(request);
            Ok(Box::new(FakeProcess { behaviour: self.behaviour, recorded: self.recorded.clone() }))
        }
    }

    impl RunningProcess for FakeProcess {
        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            match self.behaviour {
                Behaviour::Exit(code) => Ok(Some(code)),
                Behaviour::Hang => Ok(None),
                Behaviour::Fail => Err(io::Error::other("the process handle became unusable")),
            }
        }

        fn wait(&mut self) -> io::Result<Option<i32>> {
            self.recorded.lock().unwrap().waits += 1;
            Ok(Some(0))
        }

        fn kill_tree(&mut self) -> io::Result<()> {
            self.recorded.lock().unwrap().kills += 1;
            Ok(())
        }

        fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }

        fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }
    }

    fn quick(program: &str) -> CommandSpec {
        CommandSpec {
            program: program.to_string(),
            args: Vec::new(),
            cwd: None,
            timeout_ms: Some(30_000),
        }
    }

    fn with_fake(
        behaviour: Behaviour,
        parent: Vec<(OsString, OsString)>,
    ) -> (LocalExecutionEnvironment, Arc<Mutex<Recorded>>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let launcher = Arc::new(FakeLauncher { behaviour, recorded: recorded.clone() });
        let environment = LocalExecutionEnvironment {
            workspace_root: dir.path().to_path_buf(),
            mode: SandboxMode::LocalProcess,
            launcher,
            parent_environment: Arc::new(move || parent.clone()),
            shell: Shell::Host,
            network_allowed: true,
        };
        (environment, recorded, dir)
    }

    #[test]
    fn a_command_does_not_see_the_agent_s_own_environment() {
        let parent = vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("AGENT_MODEL_API_KEY"), OsString::from("sk-live-secret")),
        ];
        let (environment, recorded, _dir) = with_fake(Behaviour::Exit(0), parent);

        environment.run_command(&quick("true")).unwrap();

        let recorded = recorded.lock().unwrap();
        let request = recorded.requests.first().expect("the command was started");
        let names: Vec<String> =
            request.env.iter().map(|(name, _)| name.to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|name| name.eq_ignore_ascii_case("PATH")));
        assert!(!names.iter().any(|name| name.eq_ignore_ascii_case("AGENT_MODEL_API_KEY")));
        assert!(
            !request
                .env
                .iter()
                .any(|(_, value)| value.to_string_lossy().contains("sk-live-secret")),
            "the model key reached a child process"
        );
    }

    #[test]
    fn a_wait_failure_kills_and_reaps_instead_of_leaking_the_child() {
        let (environment, recorded, _dir) = with_fake(Behaviour::Fail, Vec::new());

        let error = environment.run_command(&quick("true")).unwrap_err();

        assert!(matches!(error, ExecutionEnvironmentError::Io(_)), "got {error:?}");
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.kills, 1, "a process we lost track of must still be killed");
        assert_eq!(recorded.waits, 1, "a killed child must still be reaped");
    }

    #[test]
    fn a_deadline_kills_the_process_the_backend_started() {
        let (environment, recorded, _dir) = with_fake(Behaviour::Hang, Vec::new());
        let mut spec = quick("true");
        spec.timeout_ms = Some(30);

        let output = environment.run_command(&spec).unwrap();

        assert_eq!(output.exit_code, TIMEOUT_EXIT_CODE);
        assert_eq!(recorded.lock().unwrap().kills, 1);
    }

    #[cfg(windows)]
    #[test]
    fn the_extended_length_prefix_is_stripped_from_a_plain_windows_path() {
        // PowerShell 5.1 inside an AppContainer only adopts a `\\?\` path as
        // its location when the text is ASCII, so the workspace would stay at
        // System32. Hand the child the plain form.
        assert_eq!(process_cwd(PathBuf::from(r"\\?\C:\workspace")), PathBuf::from(r"C:\workspace"));
        assert_eq!(process_cwd(PathBuf::from(r"\\?\C:\工作区")), PathBuf::from("C:\\工作区"));
    }

    #[cfg(windows)]
    #[test]
    fn the_extended_length_prefix_stays_where_dropping_it_would_change_the_meaning() {
        // A UNC share's extended form is `\\?\UNC\server\share`; the plain
        // `server\share` is a different, relative path.
        assert_eq!(
            process_cwd(PathBuf::from(r"\\?\UNC\server\share")),
            PathBuf::from(r"\\?\UNC\server\share")
        );
        // Past MAX_PATH the prefix is what allows the path to be opened at all.
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(process_cwd(PathBuf::from(&long)), PathBuf::from(&long));
        // A path that never had the prefix is passed through untouched.
        assert_eq!(process_cwd(PathBuf::from(r"C:\plain")), PathBuf::from(r"C:\plain"));
    }

    /// An AppContainer-backed environment whose spawns are recorded instead of
    /// run: the shell contract can be read from the request it builds.
    fn app_container_recording(
        workspace: &Path,
    ) -> (LocalExecutionEnvironment, Arc<Mutex<Recorded>>) {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let launcher =
            Arc::new(FakeLauncher { behaviour: Behaviour::Exit(0), recorded: recorded.clone() });
        let environment = LocalExecutionEnvironment {
            workspace_root: workspace.to_path_buf(),
            mode: SandboxMode::LocalProcess,
            launcher,
            parent_environment: Arc::new(Vec::new),
            shell: Shell::PowerShell("powershell.exe".to_string()),
            network_allowed: true,
        };
        (environment, recorded)
    }

    fn powershell_spec(command: &str, cwd: Option<&str>) -> CommandSpec {
        CommandSpec {
            program: "powershell.exe".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
            cwd: cwd.map(String::from),
            timeout_ms: Some(30_000),
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_app_container_shell_starts_at_the_workspace_whatever_its_ancestors_admit() {
        let dir = tempfile::tempdir().unwrap();
        let (environment, recorded) = app_container_recording(dir.path());

        environment.run_command(&powershell_spec("Get-ChildItem .", None)).unwrap();

        let recorded = recorded.lock().unwrap();
        let args = &recorded.requests.first().expect("the command was started").args;
        let script = args.last().unwrap();
        assert!(script.contains("New-PSDrive -Name workspace -PSProvider FileSystem"), "{script}");
        assert!(
            script.contains(&format!(
                "$__yaoyao_workspace = '{}'",
                process_cwd(dir.path().canonicalize().unwrap()).display()
            )),
            "{script}"
        );
        assert!(script.contains(r"Set-Location -LiteralPath 'workspace:\'"), "{script}");
        // Native output and PowerShell's own messages both go through the
        // console encoding; without this a UTF-8 child or a localized error is
        // mojibake in the tool result.
        assert!(
            script.contains("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8"),
            "{script}"
        );
        // The author's command is what runs; the prefix only sets the stage.
        let command_at = script.find("Get-ChildItem .").expect("the command is in the script");
        // The exit code is read after the command, from the native program when
        // there is one: PowerShell 5.1 treats a redirected stderr line as an
        // error record, so `$?` alone would report a successful command as a
        // failure.
        let exit_at = script
            .find("$__yaoyao_exit = $LASTEXITCODE")
            .expect("the native exit code is captured");
        assert!(command_at < exit_at, "{script}");
        assert!(
            script.contains("if ($null -ne $__yaoyao_exit) { exit $__yaoyao_exit }"),
            "{script}"
        );
        assert!(script.contains("if ($__yaoyao_ok) { exit 0 } else { exit 1 }"), "{script}");
    }

    #[test]
    fn only_the_app_container_shell_is_named_for_the_tool_description() {
        let dir = tempfile::tempdir().unwrap();
        let (environment, _) = app_container_recording(dir.path());
        assert_eq!(environment.shell_language(), Some("PowerShell"));
        assert_eq!(LocalExecutionEnvironment::new(dir.path()).shell_language(), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_command_started_in_a_subdirectory_keeps_that_directory_inside_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let (environment, recorded) = app_container_recording(dir.path());

        environment.run_command(&powershell_spec("Get-ChildItem .", Some("sub"))).unwrap();

        let recorded = recorded.lock().unwrap();
        let script = recorded.requests.first().unwrap().args.last().unwrap();
        assert!(script.contains(r"Set-Location -LiteralPath 'workspace:\sub'"), "{script}");
    }

    #[cfg(windows)]
    #[test]
    fn a_quote_in_the_workspace_path_does_not_end_the_powershell_literal_early() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("o'brien");
        std::fs::create_dir(&workspace).unwrap();
        let (environment, recorded) = app_container_recording(&workspace);

        environment.run_command(&powershell_spec("Get-ChildItem .", None)).unwrap();

        let recorded = recorded.lock().unwrap();
        let script = recorded.requests.first().unwrap().args.last().unwrap();
        assert!(script.contains("o''brien"), "{script}");
        assert!(!script.contains("o'brien'"), "{script}");
    }
}
