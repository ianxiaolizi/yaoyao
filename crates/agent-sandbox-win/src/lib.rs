//! agent-sandbox-win: the Windows AppContainer process launcher.
//!
//! This is the one crate in the workspace that is allowed `unsafe` code
//! (`STYLE.md` §1). Containment is the product here, and Windows offers no safe
//! surface for a job object, a security-capability attribute list, or a DACL
//! edit. Every `unsafe` block carries a `// SAFETY:` line saying which API's
//! contract it is upholding; `undocumented_unsafe_blocks` is denied at the crate
//! root so a block without one fails the build.
//!
//! The crate implements [`agent_ports::process_launcher::ProcessLauncher`] and
//! nothing else. The workspace boundary, the command deadline, the output cap
//! and the environment allowlist stay in `agent-tools-fs`; moving them here
//! would give the second backend its own copy of the rules, which is how the
//! two copies start to disagree.
//!
//! Off Windows the crate is a refusal stub: constructing the launcher fails with
//! `BackendUnavailable`, so a selection can never fall through to an unconfined
//! process.

#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::fmt;

/// The profile name a run uses unless it is configured otherwise.
///
/// One name for the machine: the SID is derived from it, the ACLs are granted to
/// that SID, and `agent sandbox setup` is idempotent against it.
pub const PROFILE_NAME: &str = "yaoyao.Agent.Sandbox";

/// What `agent sandbox setup` did.
#[derive(Debug, Clone)]
pub struct SetupReport {
    pub profile: String,
    pub sid: String,
    /// False when the profile already existed; setup is idempotent.
    pub created: bool,
    /// Whether the package SID can read and write the null device.
    pub nul_granted: bool,
    /// The directories that were granted read-and-execute, as spelled.
    pub granted: Vec<String>,
}

/// What the machine currently offers the backend.
///
/// Read-only: deriving a SID and reading a DACL touch nothing.
#[derive(Debug, Clone)]
pub struct Status {
    pub profile: String,
    pub sid: String,
    /// Whether the null-device grant `setup` installs is present. Setup is the
    /// only thing that puts it there, but the grant does not survive a reboot:
    /// the kernel resets the null device's security descriptor at boot. Absence
    /// therefore means "not in place", not necessarily "never run".
    pub nul_granted: bool,
}

/// Why setup or a status probe could not answer.
#[derive(Debug, Clone)]
pub struct SetupError(String);

impl SetupError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetupError {}

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{AppContainerLauncher, KillOnCloseJob, setup, status};

#[cfg(not(windows))]
mod unsupported;
#[cfg(not(windows))]
pub use unsupported::{AppContainerLauncher, KillOnCloseJob, setup, status};
