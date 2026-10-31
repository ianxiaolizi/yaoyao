//! agent-tools-fs: concrete `fs.read` / `fs.write` / `shell.run` tools plus a
//! workspace-confined local execution backend.
//!
//! This is an adapter crate: it depends only on `agent-ports` and
//! `agent-protocol`. The local backend confines reads/writes to the workspace
//! root and refuses command execution in read-only mode; the sandboxed backends
//! arrive through [`agent_ports::process_launcher::ProcessLauncher`] and are
//! assembled in `apps/agent`.

#![deny(unsafe_code)]

mod env;
mod provider;
mod python_env;
mod worker;

pub use env::{
    LocalExecutionEnvironment, MAX_FILE_BYTES, MAX_OUTPUT_BYTES, StdProcessLauncher,
    app_container_shell, app_container_shell_supports_and_and,
};
pub use provider::{FsToolProvider, SHELL_TIMEOUT_MS};
pub use worker::{SandboxedFileEnvironment, WORKER_COMMAND, serve as serve_worker};
