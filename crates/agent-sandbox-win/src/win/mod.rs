//! The Windows implementation.
//!
//! The launcher's construction is the fail-closed point: a profile SID that
//! cannot be derived (or a machine whose setup has not been run) stops the
//! run here, before any command is handed to it. There is no variant of this
//! type that falls back to an unconfined process.

mod acl;
mod cmdline;
mod env_block;
mod grant_record;
mod process;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use agent_ports::process_launcher::{
    LaunchError, ProcessLauncher, RunningProcess, SpawnRequest, StdinMode,
};

use crate::{SetupError, SetupReport, Status};

use acl::{Capabilities, Sid};

/// A launcher that starts every command inside the AppContainer profile.
pub struct AppContainerLauncher {
    profile: String,
    sid: Sid,
    /// `None` when network access was denied: the process is created without the
    /// capability, not with an empty one that some future edit could fill.
    capabilities: Option<Capabilities>,
    workspace_root: PathBuf,
    /// Where the record of a completed workspace grant lives. One file, next to
    /// the sandbox-owned directories.
    sandbox_root: PathBuf,
    home: PathBuf,
    temp: PathBuf,
    cargo_home: PathBuf,
    /// Where the sandboxed file worker is staged. It must be granted with the
    /// other sandbox directories, or the container cannot execute the copy.
    worker: PathBuf,
    rustup_home: Option<PathBuf>,
    /// The workspace and sandbox grants are applied once, on the first spawn.
    /// Assembly happens for commands that never run anything (`status`,
    /// `export`), and a recursive ACL pass over a large workspace is not
    /// something a read-only command should pay for.
    grants: OnceLock<Result<(), String>>,
}

impl AppContainerLauncher {
    /// Derive the profile SID and prepare the sandbox-owned directories.
    ///
    /// `workspace_root` is granted when the first command starts, not here: the
    /// grant walks the tree and is only worth doing if something will run.
    pub fn new(
        profile: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
        sandbox_root: impl Into<PathBuf>,
        network: bool,
    ) -> Result<Self, LaunchError> {
        let profile = profile.into();
        let sid = Sid::derive(&profile).map_err(|error| {
            LaunchError::BackendUnavailable(format!(
                "{error}; run `agent sandbox setup` once from an elevated prompt"
            ))
        })?;
        // The NUL grant is the one setup artifact that can be checked without
        // elevation, and without it git and the MSVC toolchain cannot run in
        // the container at all. Refusing here turns "setup was never run" into
        // a refusal to start instead of a confusing ACL error from the first
        // command that happens to touch the device.
        match acl::null_device_granted(sid.as_ptr()) {
            Ok(true) => {}
            Ok(false) => {
                return Err(LaunchError::BackendUnavailable(
                    "the AppContainer setup is not in place: the null-device grant is \
                     missing (Windows resets it at every boot); run `agent sandbox setup` \
                     from an elevated prompt"
                        .to_string(),
                ));
            }
            Err(error) => {
                return Err(LaunchError::BackendUnavailable(format!(
                    "could not verify the AppContainer setup: {error}"
                )));
            }
        }
        let workspace_root = workspace_root.into();
        if !workspace_root.is_dir() {
            return Err(LaunchError::BackendUnavailable(format!(
                "the workspace root {} is not a directory",
                workspace_root.display()
            )));
        }
        let sandbox_root = sandbox_root.into();
        let home = sandbox_root.join("home");
        let temp = sandbox_root.join("temp");
        let cargo_home = sandbox_root.join("cargo-home");
        let worker = sandbox_root.join("worker");
        // `work` is where a command that wants scratch space outside the
        // workspace is pointed by the environment; it is not the cwd.
        for directory in
            [&sandbox_root, &home, &temp, &cargo_home, &worker, &sandbox_root.join("work")]
        {
            std::fs::create_dir_all(directory).map_err(LaunchError::Io)?;
        }

        let capabilities = if network {
            Some(Capabilities::internet_client().map_err(|error| {
                LaunchError::BackendUnavailable(format!(
                    "network access was selected but the capability could not be derived: {error}"
                ))
            })?)
        } else {
            None
        };

        Ok(Self {
            profile,
            sid,
            capabilities,
            workspace_root,
            sandbox_root,
            home,
            temp,
            cargo_home,
            worker,
            rustup_home: rustup_home(),
            grants: OnceLock::new(),
        })
    }

    /// The profile this launcher runs under (diagnostics).
    pub fn profile(&self) -> &str {
        &self.profile
    }

    fn ensure_grants(&self) -> Result<(), String> {
        self.grants
            .get_or_init(|| {
                self.grant_workspace_once()?;
                for directory in [&self.home, &self.temp, &self.cargo_home, &self.worker] {
                    acl::grant_tree(directory, self.sid.as_ptr(), acl::MODIFY).map_err(
                        |error| format!("sandbox directory {}: {error}", directory.display()),
                    )?;
                }
                Ok(())
            })
            .clone()
    }

    /// Grant the workspace tree unless a record says it has already been done.
    ///
    /// The walk is what makes a workspace that predates the ACE readable from
    /// inside the container; files created since then inherit the ACE, so one
    /// walk is enough until something outside the rules moves a tree in. That
    /// costs about two seconds on a 20k-file workspace and used to be paid by
    /// the first command of every process. See `grant_record` for what the
    /// record is bound to and what a stale one does.
    fn grant_workspace_once(&self) -> Result<(), String> {
        let sid = self.sid.to_string().map_err(|error| format!("the package SID: {error}"))?;
        let Some(machine) = std::env::var("COMPUTERNAME").ok().filter(|name| !name.is_empty())
        else {
            // Without the machine name a record cannot tell a copy of this
            // workspace from this workspace; walk rather than guess.
            return acl::grant_tree(&self.workspace_root, self.sid.as_ptr(), acl::MODIFY)
                .map_err(|error| format!("workspace {}: {error}", self.workspace_root.display()));
        };
        let record = grant_record::GrantRecord {
            workspace: &self.workspace_root,
            sid: &sid,
            machine: &machine,
            access: acl::MODIFY,
        };
        let path = self.sandbox_root.join("workspace-grant");
        if grant_record::is_current(&path, &record) {
            return Ok(());
        }
        acl::grant_tree(&self.workspace_root, self.sid.as_ptr(), acl::MODIFY)
            .map_err(|error| format!("workspace {}: {error}", self.workspace_root.display()))?;
        // A record that cannot be written costs the next process a walk, never
        // a missing grant, so the failure is not worth failing the command.
        let _ = grant_record::write(&path, &record);
        Ok(())
    }
}

impl ProcessLauncher for AppContainerLauncher {
    fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn RunningProcess>, LaunchError> {
        self.ensure_grants().map_err(LaunchError::BackendUnavailable)?;

        // The sandbox's own directories replace the ones the host would name.
        // The child never sees the user's real profile: a command that writes
        // to `%USERPROFILE%` writes into the sandbox, and one that reads it
        // finds nothing the operator stored there.
        let mut overrides: Vec<(&str, OsString)> = vec![
            ("HOME", self.home.clone().into_os_string()),
            ("USERPROFILE", self.home.clone().into_os_string()),
            ("TEMP", self.temp.clone().into_os_string()),
            ("TMP", self.temp.clone().into_os_string()),
            ("CARGO_HOME", self.cargo_home.clone().into_os_string()),
        ];
        if let Some(rustup) = &self.rustup_home {
            // Read-only by ACL: the toolchain is visible, `rustup update` is
            // not. `process_env` strips `RUSTUP_HOME` from the inherited set
            // (its directory can hold credentials), so naming it here is the
            // only way the shims find their toolchains.
            overrides.push(("RUSTUP_HOME", rustup.clone().into_os_string()));
        }

        let environment = env_block::build(&request.env, &overrides);
        let command_line = cmdline::build(&request.program, &request.args);
        let capability_entries = self.capabilities.as_ref().map(Capabilities::entries);
        let handle = process::spawn(
            self.sid.as_ptr(),
            capability_entries.as_deref(),
            &environment,
            &command_line,
            &request.cwd,
            request.stdin == StdinMode::Pipe,
        )?;
        Ok(Box::new(handle))
    }
}

/// One-time, elevated setup: create the profile, grant the null device, and
/// make the toolchain directories readable to the package SID.
///
/// `readonly` are paths the caller names (a compiler install, a git install).
/// They must exist: a typo that silently granted nothing would turn into a
/// command failing later for a reason nobody could connect back to setup.
pub fn setup(profile: &str, readonly: &[PathBuf]) -> Result<SetupReport, SetupError> {
    let (sid, created) = acl::create_or_derive(profile)
        .map_err(|error| SetupError::new(format!("profile {profile:?}: {error}")))?;
    let sid_string = sid
        .to_string()
        .map_err(|error| SetupError::new(format!("profile {profile:?}: {error}")))?;

    acl::grant_null_device(sid.as_ptr()).map_err(|error| {
        SetupError::new(format!(
            "the null-device grant failed: {error} (setup needs an elevated prompt; \
             this is the grant that lets git and the MSVC toolchain run in the container)"
        ))
    })?;

    // Toolchain directories that belong to the user: read and execute only.
    // Missing ones are skipped — not every machine has cargo or rustup — but an
    // explicitly named path that is missing is an error.
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(cargo_bin) = cargo_bin() {
        paths.push(cargo_bin);
    }
    if let Some(rustup) = rustup_home() {
        paths.push(rustup);
    }
    for path in readonly {
        if !path.exists() {
            return Err(SetupError::new(format!(
                "the read-only path {} does not exist",
                path.display()
            )));
        }
        paths.push(path.clone());
    }
    paths.sort();
    paths.dedup();

    let mut granted = Vec::new();
    for path in paths {
        acl::grant_tree(&path, sid.as_ptr(), acl::READ_EXECUTE).map_err(|error| {
            SetupError::new(format!("the grant on {} failed: {error}", path.display()))
        })?;
        granted.push(path.display().to_string());
    }

    Ok(SetupReport {
        profile: profile.to_string(),
        sid: sid_string,
        created,
        nul_granted: true,
        granted,
    })
}

/// Read-only readiness probe for the doctor.
pub fn status(profile: &str) -> Result<Status, SetupError> {
    let sid = Sid::derive(profile)
        .map_err(|error| SetupError::new(format!("profile {profile:?}: {error}")))?;
    let sid_string = sid
        .to_string()
        .map_err(|error| SetupError::new(format!("profile {profile:?}: {error}")))?;
    let nul_granted = acl::null_device_granted(sid.as_ptr()).map_err(|error| {
        SetupError::new(format!("could not read the null-device DACL: {error}"))
    })?;
    Ok(Status { profile: profile.to_string(), sid: sid_string, nul_granted })
}

/// `%CARGO_HOME%\bin`, or the user's default `%USERPROFILE%\.cargo\bin`.
///
/// The `bin` directory only: the parent holds `credentials.toml`, which the
/// sandbox has no reason to see.
fn cargo_bin() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("CARGO_HOME") {
        let candidate = PathBuf::from(configured).join("bin");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    let profile = std::env::var_os("USERPROFILE")?;
    let candidate = PathBuf::from(profile).join(".cargo").join("bin");
    candidate.is_dir().then_some(candidate)
}

/// `%RUSTUP_HOME%`, or the user's default `%USERPROFILE%\.rustup`.
fn rustup_home() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("RUSTUP_HOME") {
        let candidate = PathBuf::from(configured);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    let profile = std::env::var_os("USERPROFILE")?;
    let candidate = PathBuf::from(profile).join(".rustup");
    candidate.is_dir().then_some(candidate)
}

/// A UTF-16 string with its terminator, for an API that takes `PCWSTR`.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The system message for a Win32 error code.
pub(crate) fn win32_message(code: u32) -> String {
    io::Error::from_raw_os_error(code as i32).to_string()
}

/// The message for an HRESULT, unwrapping the Win32 code when there is one.
pub(crate) fn hresult_message(hr: i32) -> String {
    let code = hr as u32;
    let facility = (code >> 16) & 0x1FFF;
    if facility == 7 {
        format!("0x{code:08x}: {}", win32_message(code & 0xFFFF))
    } else {
        format!("0x{code:08x}: Windows error {}", code & 0xFFFF)
    }
}

/// The calling thread's last error, rendered.
pub(crate) fn last_error() -> String {
    // SAFETY: reading the calling thread's last-error value has no preconditions.
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    win32_message(code)
}
