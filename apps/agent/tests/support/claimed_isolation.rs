//! A backend that runs on this machine and reports that it isolates.
//!
//! Not a product backend, and never reachable from the binary: `stack`'s
//! `environment_for` builds a real AppContainer or refuses, and nothing in
//! `apps/agent/src` constructs this type. It exists for the cases whose subject
//! is the **decision chain** — a call that asks, an answer that resumes it, a
//! refusal that stops the turn — and which therefore need a backend the
//! isolation floor admits (CONTRACT §7 #179).
//!
//! The alternative would be to run those cases on a real AppContainer, which
//! makes them depend on a machine having run the one-time setup: a suite that
//! is green here and skipped in CI is not evidence about the chain. The stage B
//! plan (§9) asks for exactly this split — a fake-backend case beside the
//! ignored real-run command — so what this type claims is the *mode*, and every
//! case built on it is about something other than the OS boundary. The
//! boundary itself has its own cases, and its own `#[ignore]`d real run.
//!
//! It runs real commands through a real local backend. That is the honest
//! part: the cases that use it are about what happens to a call before and
//! around execution, and a hand-written runner would have made those cases
//! measure the runner.

use std::path::Path;

use agent_ports::execution_environment::{
    CommandOutput, CommandSpec, DirEntry, ExecutionEnvironment, ExecutionEnvironmentError,
    SandboxMode, ShellInvocation,
};
use agent_protocol::permissions::Capability;
use agent_tools_fs::LocalExecutionEnvironment;

/// A local backend that answers `WorkspaceSandbox` to the one question the
/// isolation floor asks.
pub struct ClaimedIsolation {
    inner: LocalExecutionEnvironment,
    mode: SandboxMode,
}

impl ClaimedIsolation {
    /// The backend a run has when the machine would confine it.
    pub fn app_container(workspace_root: impl Into<std::path::PathBuf>) -> Self {
        let root: std::path::PathBuf = workspace_root.into();
        Self { inner: LocalExecutionEnvironment::new(&root), mode: SandboxMode::WorkspaceSandbox }
    }
}

impl ExecutionEnvironment for ClaimedIsolation {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn mode(&self) -> SandboxMode {
        self.mode
    }

    fn advertised_capabilities(&self) -> Vec<Capability> {
        // The interior's backend is what runs, so its list is the one that is
        // true. It is reported under the claimed mode, and for these modes the
        // two lists are the same.
        self.inner.advertised_capabilities()
    }

    fn shell_invocation(&self, command: &str) -> ShellInvocation {
        // The host shell, because the interior is the host shell. A case that
        // wants PowerShell's grammar is a case about the container, and it
        // needs the container.
        self.inner.shell_invocation(command)
    }

    fn shell_language(&self) -> Option<&'static str> {
        self.inner.shell_language()
    }

    fn read_file(&self, path: &str) -> Result<String, ExecutionEnvironmentError> {
        self.inner.read_file(path)
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), ExecutionEnvironmentError> {
        self.inner.write_file(path, content)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, ExecutionEnvironmentError> {
        self.inner.list_dir(path)
    }

    fn run_command(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecutionEnvironmentError> {
        self.inner.run_command(spec)
    }
}

/// The backend a case uses when its subject is the decision chain.
///
/// A function so each case says why it is using one, at the call site.
pub fn isolating(workspace_root: &Path) -> std::sync::Arc<dyn ExecutionEnvironment> {
    std::sync::Arc::new(ClaimedIsolation::app_container(workspace_root))
}
