//! The non-Windows stub: every entry point refuses.
//!
//! Refusing here, rather than not compiling, keeps `AGENT_SANDBOX=app-container`
//! a clean `BackendUnavailable` on a host that has no AppContainer instead of a
//! build error in a crate a portable workspace still lists.

use std::path::PathBuf;

use agent_ports::process_launcher::{LaunchError, ProcessLauncher, RunningProcess, SpawnRequest};

use crate::{SetupError, SetupReport, Status};

/// A launcher that cannot be constructed off Windows.
pub struct AppContainerLauncher;

impl AppContainerLauncher {
    pub fn new(
        _profile: &str,
        _workspace_root: impl Into<PathBuf>,
        _sandbox_root: impl Into<PathBuf>,
        _network: bool,
    ) -> Result<Self, LaunchError> {
        Err(unavailable())
    }
}

impl ProcessLauncher for AppContainerLauncher {
    fn spawn(&self, _request: SpawnRequest) -> Result<Box<dyn RunningProcess>, LaunchError> {
        Err(unavailable())
    }
}

/// The job object off Windows: nothing to assign and nothing to close.
///
/// Not a refusal, because the child it guards is still started and still stopped
/// by the provider that owns it; what is missing is only the kernel's last line
/// of defence for a parent that ends without running destructors — the gap the
/// stdio backend's `kill_tree` already records for this platform.
pub struct KillOnCloseJob;

impl KillOnCloseJob {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    pub fn attach(&self, _pid: u32) -> std::io::Result<()> {
        Ok(())
    }
}

fn unavailable() -> LaunchError {
    LaunchError::BackendUnavailable(
        "the AppContainer execution backend exists only on Windows".to_string(),
    )
}

pub fn setup(_profile: &str, _readonly: &[PathBuf]) -> Result<SetupReport, SetupError> {
    Err(SetupError::new("the AppContainer execution backend exists only on Windows"))
}

pub fn status(_profile: &str) -> Result<Status, SetupError> {
    Err(SetupError::new("the AppContainer execution backend exists only on Windows"))
}
