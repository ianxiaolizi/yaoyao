//! The sandbox file worker, and the environment that talks to it.
//!
//! Under the AppContainer backend the shell is confined by the kernel, but the
//! `fs.*` tools used to read and write in the host process — a boundary made of
//! path checks, not of access tokens. This module moves those three operations
//! (`read_file`, `write_file`, `list_dir`) into a helper process running inside
//! the AppContainer. The tools do not change: they still call an
//! [`ExecutionEnvironment`], and this environment forwards each file operation
//! over a pipe.
//!
//! The helper is this same executable, started with the hidden
//! [`WORKER_COMMAND`] subcommand. It is copied into a directory the package SID
//! may execute (the sandbox's own `worker` directory, granted with the rest of
//! the sandbox directories). That copy has to happen **before the first spawn
//! in this process**, because the workspace and sandbox grants are applied once,
//! at that spawn; a later copy would not be covered by them.
//!
//! Failure is closed: a worker that cannot start, times out, or dies makes the
//! tool call fail. Nothing falls back to reading in the host process, because
//! that would silently downgrade the boundary the user selected.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use agent_ports::execution_environment::{
    CommandOutput, CommandSpec, DirEntry, DirEntryKind, ExecutionEnvironment,
    ExecutionEnvironmentError, SandboxMode, ShellInvocation,
};
use agent_ports::process_env::child_environment;
use agent_ports::process_launcher::{ProcessLauncher, RunningProcess, SpawnRequest, StdinMode};
use serde_json::{Value, json};

use crate::env::{LocalExecutionEnvironment, MAX_FILE_BYTES, resolve_walked};

/// The hidden subcommand that turns this binary into the sandbox's file worker.
pub const WORKER_COMMAND: &str = "__sandbox-worker";

/// Largest frame either side will read. A read response carries at most one
/// file (the environment's 1 MiB cap) plus JSON escaping, so this leaves room
/// without letting a corrupted length allocate an arbitrary amount.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// How long one request may take before the worker is considered stuck. A read
/// or a listing of a workspace file is milliseconds; this bound exists for a
/// worker that stopped answering, not for a slow disk.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the worker gets to answer the handshake. Start-up includes the
/// one-time workspace grant (a tree walk), which is measured in seconds on a
/// large checkout, so this is deliberately generous.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// How much of the worker's stderr is kept for failure messages.
const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// Serve one worker process on stdin/stdout until shutdown or EOF.
pub fn serve(root: &Path) -> io::Result<()> {
    let mut reader = BufReader::new(io::stdin());
    let mut writer = io::stdout();
    serve_on(&mut reader, &mut writer, root)
}

fn serve_on(reader: &mut dyn BufRead, writer: &mut dyn Write, root: &Path) -> io::Result<()> {
    loop {
        let Some(request) = read_frame(reader)? else {
            return Ok(());
        };
        let op = request.get("op").and_then(Value::as_str).unwrap_or_default();
        let path = request.get("path").and_then(Value::as_str).unwrap_or_default();
        let response = match op {
            "ping" => json!({ "ok": { "root": root.to_string_lossy() } }),
            "read" => match resolve_walked(root, path)
                .and_then(|resolved| crate::env::read_file_at(&resolved, path))
            {
                Ok(content) => json!({ "ok": { "content": content } }),
                Err(error) => error_response(&error),
            },
            "write" => {
                let content = request.get("content").and_then(Value::as_str).unwrap_or_default();
                match resolve_walked(root, path)
                    .and_then(|resolved| crate::env::write_file_at(&resolved, path, content))
                {
                    Ok(()) => json!({ "ok": Value::Null }),
                    Err(error) => error_response(&error),
                }
            }
            "list" => match resolve_walked(root, path)
                .and_then(|resolved| crate::env::list_dir_at(&resolved))
            {
                Ok(entries) => json!({
                    "ok": entries
                        .iter()
                        .map(|entry| json!({
                            "name": entry.name,
                            "kind": kind_name(entry.kind),
                            "size": entry.size,
                        }))
                        .collect::<Vec<_>>(),
                }),
                Err(error) => error_response(&error),
            },
            "shutdown" => {
                write_frame(writer, &json!({ "ok": Value::Null }))?;
                return Ok(());
            }
            other => json!({
                "error": {
                    "kind": "protocol",
                    "message": format!("the worker does not know the operation {other:?}"),
                }
            }),
        };
        write_frame(writer, &response)?;
    }
}

fn kind_name(kind: DirEntryKind) -> &'static str {
    match kind {
        DirEntryKind::File => "file",
        DirEntryKind::Directory => "directory",
        DirEntryKind::Symlink => "symlink",
        DirEntryKind::Other => "other",
    }
}

fn error_response(error: &ExecutionEnvironmentError) -> Value {
    // The client wraps an `io` answer back into an `io::Error`, whose display
    // adds the "IO error: " prefix the typed error already has; sending the
    // typed message would print it twice.
    let message = match error {
        ExecutionEnvironmentError::Io(inner) => inner.to_string(),
        other => other.to_string(),
    };
    json!({
        "error": {
            "kind": error_kind(error),
            "message": message,
        }
    })
}

fn error_kind(error: &ExecutionEnvironmentError) -> &'static str {
    match error {
        ExecutionEnvironmentError::PathOutsideWorkspace { .. } => "outside",
        ExecutionEnvironmentError::NotPermitted { .. } => "not_permitted",
        ExecutionEnvironmentError::BackendUnavailable(_) => "unavailable",
        ExecutionEnvironmentError::ProtectedPath(_) => "protected",
        ExecutionEnvironmentError::FileTooLarge { .. } => "too_large",
        ExecutionEnvironmentError::CwdUnusable { .. } => "bad_cwd",
        ExecutionEnvironmentError::Io(_) => "io",
    }
}

fn read_frame(reader: &mut dyn BufRead) -> io::Result<Option<Value>> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the worker pipe ended inside a frame header",
                ));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a frame of {length} bytes is outside the protocol bounds"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let value = serde_json::from_slice(&body).map_err(|error| {
        io::Error::new(io::ErrorKind::InvalidData, format!("a frame was not JSON: {error}"))
    })?;
    Ok(Some(value))
}

fn write_frame(writer: &mut dyn Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a response is larger than the protocol allows",
        ));
    }
    writer.write_all(&(body.len() as u32).to_le_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// The `ExecutionEnvironment` for the AppContainer backend: commands go through
/// the launcher as before, file operations go through a worker inside it.
pub struct SandboxedFileEnvironment {
    /// The workspace root as the worker confines paths to, resolved here.
    ///
    /// The resolution has to happen in the host: inside the AppContainer a
    /// process may not ask the kernel for a final path
    /// ([`resolve_walked`](crate::env::resolve_walked) says what that costs), so
    /// the worker is handed one that is already absolute and link-free. The
    /// failure is kept and reported by the first file operation, like a
    /// staging failure.
    worker_root: Result<PathBuf, String>,
    inner: LocalExecutionEnvironment,
    launcher: Arc<dyn ProcessLauncher>,
    worker_exe: Result<PathBuf, String>,
    worker: Mutex<Option<Worker>>,
}

impl SandboxedFileEnvironment {
    /// Build the environment and stage the worker copy.
    ///
    /// The copy happens here, at assembly, rather than lazily: the grants are
    /// applied at the first spawn in this process, and a copy made after that
    /// would not be covered by them. A staging failure is kept and reported by
    /// the first file operation (fail closed), so that a run which never uses
    /// `fs.*` is not refused over a file it would never touch.
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        launcher: Arc<dyn ProcessLauncher>,
        network_allowed: bool,
        worker_dir: impl Into<PathBuf>,
    ) -> Self {
        let root: PathBuf = workspace_root.into();
        let worker_root = root.canonicalize().map(crate::env::process_cwd).map_err(|error| {
            format!("the workspace root {} could not be resolved: {error}", root.display())
        });
        let inner =
            LocalExecutionEnvironment::app_container(root, launcher.clone(), network_allowed);
        let worker_exe = stage_worker(&worker_dir.into());
        Self { worker_root, inner, launcher, worker_exe, worker: Mutex::new(None) }
    }

    fn worker_exe(&self) -> Result<&Path, ExecutionEnvironmentError> {
        match &self.worker_exe {
            Ok(path) => Ok(path),
            Err(reason) => Err(ExecutionEnvironmentError::BackendUnavailable(reason.clone())),
        }
    }

    fn worker_root(&self) -> Result<&Path, ExecutionEnvironmentError> {
        self.worker_root
            .as_deref()
            .map_err(|reason| ExecutionEnvironmentError::BackendUnavailable(reason.clone()))
    }

    /// Run one request, starting the worker on first use and replacing it once
    /// if the pipe has failed. A second failure is reported, not retried again:
    /// a worker that dies twice in a row is not going to answer the third time.
    fn call(&self, request: Value, path: &str) -> Result<Value, ExecutionEnvironmentError> {
        let exe = self.worker_exe()?;
        let root = self.worker_root()?;
        let mut guard = self.worker.lock().map_err(|_| {
            ExecutionEnvironmentError::Io(io::Error::other("the worker lock was poisoned"))
        })?;
        let mut last: Option<ExecutionEnvironmentError> = None;
        for _ in 0..2 {
            if guard.is_none() {
                let worker = start_worker(exe, root, self.launcher.as_ref())?;
                *guard = Some(worker);
            }
            let worker = guard.as_mut().expect("just started");
            match worker.request(&request) {
                Ok(response) => return parse_response(&response, path, root),
                Err(error) => {
                    // The pipe is unusable: kill what is left and try once more
                    // with a fresh process. Writes are idempotent, so a retry
                    // cannot duplicate an effect.
                    let tail = worker.kill();
                    *guard = None;
                    last = Some(ExecutionEnvironmentError::Io(io::Error::other(format!(
                        "{error}{tail}"
                    ))));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            ExecutionEnvironmentError::Io(io::Error::other("the sandbox file worker failed"))
        }))
    }
}

impl ExecutionEnvironment for SandboxedFileEnvironment {
    fn name(&self) -> &str {
        "app-container"
    }

    fn mode(&self) -> SandboxMode {
        self.inner.mode()
    }

    fn advertised_capabilities(&self) -> Vec<agent_protocol::permissions::Capability> {
        self.inner.advertised_capabilities()
    }

    fn shell_invocation(&self, command: &str) -> ShellInvocation {
        self.inner.shell_invocation(command)
    }

    fn shell_language(&self) -> Option<&'static str> {
        self.inner.shell_language()
    }

    fn read_file(&self, path: &str) -> Result<String, ExecutionEnvironmentError> {
        let value = self.call(json!({ "op": "read", "path": path }), path)?;
        value.get("content").and_then(Value::as_str).map(str::to_string).ok_or_else(|| {
            ExecutionEnvironmentError::Io(io::Error::other(
                "the worker answered a read without content",
            ))
        })
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), ExecutionEnvironmentError> {
        self.call(json!({ "op": "write", "path": path, "content": content }), path)?;
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, ExecutionEnvironmentError> {
        let value = self.call(json!({ "op": "list", "path": path }), path)?;
        let entries = value.as_array().ok_or_else(|| {
            ExecutionEnvironmentError::Io(io::Error::other(
                "the worker answered a listing that was not a list",
            ))
        })?;
        entries
            .iter()
            .map(|entry| {
                let name = entry.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ExecutionEnvironmentError::Io(io::Error::other("a listed entry had no name"))
                })?;
                let kind = match entry.get("kind").and_then(Value::as_str).unwrap_or_default() {
                    "file" => DirEntryKind::File,
                    "directory" => DirEntryKind::Directory,
                    "symlink" => DirEntryKind::Symlink,
                    other => {
                        return Err(ExecutionEnvironmentError::Io(io::Error::other(format!(
                            "a listed entry had the unknown kind {other:?}"
                        ))));
                    }
                };
                let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
                Ok(DirEntry { name: name.to_string(), kind, size })
            })
            .collect()
    }

    fn run_command(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecutionEnvironmentError> {
        self.inner.run_command(spec)
    }
}

impl Drop for SandboxedFileEnvironment {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.worker.lock()
            && let Some(mut worker) = guard.take()
        {
            worker.kill();
        }
    }
}

struct Worker {
    process: Box<dyn RunningProcess>,
    stdin: Box<dyn Write + Send>,
    responses: mpsc::Receiver<io::Result<Value>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
}

impl Worker {
    fn request(&mut self, request: &Value) -> io::Result<Value> {
        write_frame(&mut self.stdin, request)?;
        match self.responses.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the worker did not answer within {REQUEST_TIMEOUT:?}"),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "the worker pipe closed"))
            }
        }
    }

    /// Kill the worker and return whatever it left on stderr, formatted for a
    /// failure message (empty when there was nothing to say).
    fn kill(&mut self) -> String {
        let _ = self.process.kill_tree();
        let _ = self.process.wait();
        let tail = self.stderr_tail.lock().map(|bytes| bytes.clone()).unwrap_or_default();
        if tail.is_empty() {
            return String::new();
        }
        format!("; worker stderr: {}", String::from_utf8_lossy(&tail).trim())
    }
}

fn start_worker(
    exe: &Path,
    root: &Path,
    launcher: &dyn ProcessLauncher,
) -> Result<Worker, ExecutionEnvironmentError> {
    let parent: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let request = SpawnRequest {
        program: exe.to_string_lossy().into_owned(),
        args: vec![
            WORKER_COMMAND.to_string(),
            "--root".to_string(),
            root.to_string_lossy().into_owned(),
        ],
        cwd: root.to_path_buf(),
        env: child_environment(&parent),
        stdin: StdinMode::Pipe,
    };
    let mut process = launcher.spawn(request).map_err(|error| {
        ExecutionEnvironmentError::BackendUnavailable(format!(
            "the sandbox file worker could not start: {error}"
        ))
    })?;
    let stdin = process.take_stdin().ok_or_else(|| {
        ExecutionEnvironmentError::BackendUnavailable(
            "the execution backend kept no stdin for the sandbox file worker".to_string(),
        )
    })?;
    let stdout = process.take_stdout().ok_or_else(|| {
        ExecutionEnvironmentError::Io(io::Error::other("the worker has no stdout pipe"))
    })?;
    let stderr_tail = Arc::new(Mutex::new(Vec::new()));
    if let Some(stderr) = process.take_stderr() {
        let tail = stderr_tail.clone();
        thread::spawn(move || drain_stderr(stderr, tail));
    }
    let (sender, responses) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let next = match read_frame(&mut reader) {
                Ok(Some(value)) => Ok(value),
                Ok(None) => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the worker closed its output without answering",
                )),
                Err(error) => Err(error),
            };
            let failed = next.is_err();
            if sender.send(next).is_err() {
                return;
            }
            if failed {
                return;
            }
        }
    });
    let mut worker = Worker { process, stdin, responses, stderr_tail };
    let handshake = json!({ "op": "ping" });
    write_frame(&mut worker.stdin, &handshake).map_err(|error| {
        ExecutionEnvironmentError::BackendUnavailable(format!(
            "the sandbox file worker could not be reached: {error}"
        ))
    })?;
    match worker.responses.recv_timeout(STARTUP_TIMEOUT) {
        Ok(Ok(value)) if value.get("ok").is_some() => Ok(worker),
        Ok(Ok(value)) => {
            let tail = worker.kill();
            Err(ExecutionEnvironmentError::BackendUnavailable(format!(
                "the sandbox file worker refused to start: {value}{tail}"
            )))
        }
        Ok(Err(error)) => {
            let tail = worker.kill();
            Err(ExecutionEnvironmentError::BackendUnavailable(format!(
                "the sandbox file worker failed its handshake: {error}{tail}"
            )))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let tail = worker.kill();
            Err(ExecutionEnvironmentError::BackendUnavailable(format!(
                "the sandbox file worker did not answer within {STARTUP_TIMEOUT:?}{tail}"
            )))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let tail = worker.kill();
            Err(ExecutionEnvironmentError::BackendUnavailable(format!(
                "the sandbox file worker closed its pipe during the handshake{tail}"
            )))
        }
    }
}

/// Keep the last few KiB of the worker's stderr. The worker itself writes
/// nothing on the happy path; this is for the case where it dies before a
/// response and the reason is the only thing worth having.
fn drain_stderr(mut stream: Box<dyn Read + Send>, tail: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                if let Ok(mut kept) = tail.lock() {
                    kept.extend_from_slice(&buffer[..read]);
                    let excess = kept.len().saturating_sub(STDERR_TAIL_BYTES);
                    if excess > 0 {
                        kept.drain(..excess);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Turn one worker response into the environment's typed result.
///
/// The worker sends the same error vocabulary the in-process backend uses; the
/// path and the root are reconstructed from the request because the worker's
/// own display of them is the text a user is not supposed to have to parse.
fn parse_response(
    value: &Value,
    path: &str,
    root: &Path,
) -> Result<Value, ExecutionEnvironmentError> {
    if let Some(ok) = value.get("ok") {
        return Ok(ok.clone());
    }
    let error = value.get("error");
    let kind = error.and_then(|entry| entry.get("kind")).and_then(Value::as_str).unwrap_or("io");
    let message = error
        .and_then(|entry| entry.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("the worker reported a failure without a message")
        .to_string();
    Err(match kind {
        "outside" => ExecutionEnvironmentError::PathOutsideWorkspace {
            path: path.to_string(),
            root: root.display().to_string(),
        },
        "too_large" => ExecutionEnvironmentError::FileTooLarge {
            path: path.to_string(),
            limit: MAX_FILE_BYTES,
        },
        "protected" => ExecutionEnvironmentError::ProtectedPath(path.to_string()),
        "not_permitted" => {
            ExecutionEnvironmentError::NotPermitted { mode: SandboxMode::WorkspaceSandbox }
        }
        "unavailable" => ExecutionEnvironmentError::BackendUnavailable(message),
        "protocol" => ExecutionEnvironmentError::Io(io::Error::other(message)),
        _ => ExecutionEnvironmentError::Io(io::Error::other(message)),
    })
}

/// Copy the running executable into the sandbox worker directory.
///
/// The copy is skipped when a sidecar says it was made from the same executable
/// (same size and modification time). The name is fixed so a stale copy is
/// overwritten rather than accumulating.
fn stage_worker(worker_dir: &Path) -> Result<PathBuf, String> {
    let source = std::env::current_exe().map_err(|error| {
        format!("the executable to stage as the sandbox file worker is unknown: {error}")
    })?;
    let metadata = std::fs::metadata(&source).map_err(|error| {
        format!("the executable {} could not be inspected: {error}", source.display())
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let signature = format!("{}:{modified}", metadata.len());
    let destination = worker_dir.join("yaoyao-worker.exe");
    let sidecar = worker_dir.join("yaoyao-worker.meta");
    if destination.is_file()
        && std::fs::read_to_string(&sidecar).is_ok_and(|recorded| recorded == signature)
    {
        return Ok(destination);
    }
    std::fs::create_dir_all(worker_dir).map_err(|error| {
        format!(
            "the sandbox worker directory {} could not be created: {error}",
            worker_dir.display()
        )
    })?;
    std::fs::copy(&source, &destination).map_err(|error| {
        format!("the worker copy {} could not be written: {error}", destination.display())
    })?;
    // A failed sidecar write only costs one extra copy next run; it does not
    // change whether the worker can run.
    let _ = std::fs::write(&sidecar, signature);
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn run(requests: &[Value], root: &Path) -> (Vec<Value>, io::Result<()>) {
        let mut input = Vec::new();
        for request in requests {
            let body = serde_json::to_vec(request).unwrap();
            input.extend_from_slice(&(body.len() as u32).to_le_bytes());
            input.extend_from_slice(&body);
        }
        let mut reader = BufReader::new(Cursor::new(input));
        let mut output = Vec::new();
        let result = serve_on(&mut reader, &mut output, root);
        let mut frames = Vec::new();
        let mut cursor = BufReader::new(Cursor::new(output));
        while let Some(frame) = read_frame(&mut cursor).unwrap() {
            frames.push(frame);
        }
        (frames, result)
    }

    #[test]
    fn the_worker_reads_writes_and_lists_inside_the_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // The directory is created first because the local backend resolves a
        // write target through its parent, so a call that went through the
        // tools is only made against a directory that exists.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let (frames, result) = run(
            &[
                json!({ "op": "write", "path": "sub/hello.txt", "content": "hi" }),
                json!({ "op": "read", "path": "sub/hello.txt" }),
                json!({ "op": "list", "path": "sub" }),
            ],
            root,
        );
        result.unwrap();
        assert_eq!(frames.len(), 3);
        assert!(frames[0].get("ok").is_some());
        assert_eq!(frames[1]["ok"]["content"], "hi");
        assert_eq!(frames[2]["ok"][0]["name"], "hello.txt");
        assert_eq!(frames[2]["ok"][0]["kind"], "file");
    }

    #[test]
    fn the_worker_refuses_a_path_outside_the_root() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        let (frames, result) =
            run(&[json!({ "op": "read", "path": target.to_string_lossy() })], dir.path());
        result.unwrap();
        assert_eq!(frames[0]["error"]["kind"], "outside");
    }

    #[test]
    fn the_worker_refuses_a_write_into_git_hooks() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
        let (frames, result) = run(
            &[json!({ "op": "write", "path": ".git/hooks/pre-commit", "content": "x" })],
            dir.path(),
        );
        result.unwrap();
        assert_eq!(frames[0]["error"]["kind"], "protected");
    }

    #[test]
    fn the_worker_reports_a_file_past_the_read_cap() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("big.txt"), vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        let (frames, result) = run(&[json!({ "op": "read", "path": "big.txt" })], dir.path());
        result.unwrap();
        assert_eq!(frames[0]["error"]["kind"], "too_large");
    }

    #[test]
    fn shutdown_answers_then_stops_reading() {
        let dir = TempDir::new().unwrap();
        let (frames, result) = run(&[json!({ "op": "shutdown" })], dir.path());
        result.unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].get("ok").is_some());
    }

    #[test]
    fn ping_names_the_root_and_an_unknown_operation_is_a_protocol_error() {
        let dir = TempDir::new().unwrap();
        let (frames, result) = run(
            &[
                json!({ "op": "ping" }),
                json!({ "op": "remove-everything", "path": "." }),
                json!({ "path": "." }),
            ],
            dir.path(),
        );
        result.unwrap();
        assert_eq!(frames[0]["ok"]["root"], dir.path().to_string_lossy().as_ref());
        assert_eq!(frames[1]["error"]["kind"], "protocol");
        assert!(
            frames[1]["error"]["message"].as_str().unwrap().contains("remove-everything"),
            "{}",
            frames[1]
        );
        // A request with no operation at all names the empty string it read.
        assert_eq!(frames[2]["error"]["kind"], "protocol");
    }

    #[test]
    fn every_operation_that_fails_is_answered_with_its_error_kind() {
        let dir = TempDir::new().unwrap();
        // A file in the place a directory would have to be: the write cannot
        // create its parent through it.
        std::fs::write(dir.path().join("blocker.txt"), "x").unwrap();
        let (frames, result) = run(
            &[
                json!({ "op": "read", "path": "absent.txt" }),
                json!({ "op": "write", "path": "blocker.txt/leaf.txt", "content": "x" }),
                json!({ "op": "list", "path": "absent" }),
            ],
            dir.path(),
        );
        result.unwrap();
        // The failure is the filesystem's, and the message keeps the OS text
        // rather than the client's "IO error: IO error:" doubling.
        for frame in &frames {
            assert_eq!(frame["error"]["kind"], "io", "{frame}");
            assert!(frame["error"]["message"].as_str().unwrap().contains("os error"), "{frame}");
        }
    }

    #[test]
    fn a_request_round_trips_through_a_frame_and_eof_ends_the_stream() {
        let value = json!({ "op": "ping", "n": 1 });
        let mut framed = Vec::new();
        write_frame(&mut framed, &value).unwrap();
        let mut reader = BufReader::new(Cursor::new(framed));
        assert_eq!(read_frame(&mut reader).unwrap(), Some(value));
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_length_outside_the_protocol_bounds_is_refused() {
        for length in [0u32, MAX_FRAME_BYTES as u32 + 1] {
            let input = length.to_le_bytes().to_vec();
            let error = read_frame(&mut BufReader::new(Cursor::new(input))).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "length {length}");
        }
    }

    #[test]
    fn a_header_that_ends_early_is_a_broken_pipe_not_an_empty_one() {
        let error = read_frame(&mut BufReader::new(Cursor::new(vec![1u8]))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_frame_that_is_not_json_is_refused() {
        let body = b"not json";
        let mut input = (body.len() as u32).to_le_bytes().to_vec();
        input.extend_from_slice(body);
        let error = read_frame(&mut BufReader::new(Cursor::new(input))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not JSON"), "{error}");
    }

    #[test]
    fn a_response_past_the_cap_is_never_written() {
        let mut output = Vec::new();
        let oversized = json!({ "ok": "x".repeat(MAX_FRAME_BYTES) });
        let error = write_frame(&mut output, &oversized).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(output.is_empty(), "a refused frame left bytes on the pipe");
    }

    #[test]
    fn the_worker_copy_is_refreshed_only_when_the_sidecar_says_it_is_stale() {
        let dir = TempDir::new().unwrap();
        let worker_dir = dir.path().join("worker");
        let source_len = std::fs::metadata(std::env::current_exe().unwrap()).unwrap().len();
        let staged = stage_worker(&worker_dir).expect("the copy is made");
        assert_eq!(std::fs::metadata(&staged).unwrap().len(), source_len);

        // A matching sidecar keeps the copy, even when something replaced its
        // bytes: the record is a size and a modification time, so it says
        // "this was copied from that executable", not "these bytes are it".
        std::fs::write(&staged, b"marker").unwrap();
        stage_worker(&worker_dir).expect("a matching copy is kept");
        assert_eq!(std::fs::read(&staged).unwrap(), b"marker");

        // A sidecar that does not match the executable copies again.
        std::fs::write(worker_dir.join("yaoyao-worker.meta"), "0:0").unwrap();
        let staged = stage_worker(&worker_dir).expect("a stale copy is replaced");
        assert_eq!(std::fs::metadata(&staged).unwrap().len(), source_len);
    }

    #[test]
    fn a_worker_error_keeps_its_kind_and_the_path_it_was_asked_for() {
        let root = Path::new("C:\\workspace");
        let answer =
            |kind: &str, message: &str| json!({ "error": { "kind": kind, "message": message } });
        assert!(matches!(
            parse_response(&answer("outside", "nope"), "a.txt", root),
            Err(ExecutionEnvironmentError::PathOutsideWorkspace { .. })
        ));
        assert!(matches!(
            parse_response(&answer("too_large", "big"), "a.txt", root),
            Err(ExecutionEnvironmentError::FileTooLarge { .. })
        ));
        assert!(matches!(
            parse_response(&answer("protected", "hook"), "a.txt", root),
            Err(ExecutionEnvironmentError::ProtectedPath(_))
        ));
        assert!(matches!(
            parse_response(&answer("not_permitted", "read-only"), "a.txt", root),
            Err(ExecutionEnvironmentError::NotPermitted { .. })
        ));
        assert!(matches!(
            parse_response(&answer("unavailable", "no worker"), "a.txt", root),
            Err(ExecutionEnvironmentError::BackendUnavailable(reason)) if reason == "no worker"
        ));
        assert!(matches!(
            parse_response(&answer("io", "absent"), "a.txt", root),
            Err(ExecutionEnvironmentError::Io(error)) if error.to_string() == "absent"
        ));
        // An answer with no message at all still has to say something.
        assert!(matches!(
            parse_response(&json!({ "error": {} }), "a.txt", root),
            Err(ExecutionEnvironmentError::Io(error))
                if error.to_string().contains("without a message")
        ));
        assert!(parse_response(&json!({ "ok": [1] }), "a.txt", root).is_ok());
    }
}
