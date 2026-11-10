//! Which execution sandbox this run starts on.
//!
//! The choice belongs to the **host**, not to the project: it is read from
//! `AGENT_SANDBOX` and deliberately not from a workspace configuration file. A
//! project can describe what its tools need; it cannot decide how much of the
//! machine this process is allowed to reach on its behalf. That is why
//! `WorkspaceConfig` refuses a `sandbox` key rather than silently ignoring one.
//!
//! The values are the four modes of
//! [`agent_ports::execution_environment::SandboxMode`]. Selecting a backend this
//! build does not have is a refusal, never a fallback: a run that asked for a
//! confinement and silently got a local process would be exactly the promise
//! this setting exists to make honestly. The refusal happens where the backend
//! is built ([`crate::stack`]), because that is the only place that can say
//! *why* it is unavailable; this module only decides what was asked for.
//!
//! Network is a second host decision, `AGENT_SANDBOX_NETWORK`, and it defaults
//! to `deny`. A sandbox that reaches the network by default is a sandbox that
//! only governs the filesystem, which is not what the name promises. The
//! explicit `internet` value grants AppContainer's broad `internetClient`
//! capability; it is not an origin allowlist and only works on the AppContainer
//! backend. The older `allow` spelling stays refused (CONTRACT §7 #179): an
//! origin allowlist bounds the requests this process makes, not the traffic of
//! a command it starts. The controlled `web.request` tool remains the
//! read-only, origin-allowlisted way out.

use agent_ports::execution_environment::SandboxMode;

/// The variable that names this run's execution backend.
pub const ENV_SANDBOX: &str = "AGENT_SANDBOX";

/// The variable that says whether a sandboxed command may reach the network.
pub const ENV_NETWORK: &str = "AGENT_SANDBOX_NETWORK";

/// Why the named backend or policy cannot be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    #[error(
        "unknown execution sandbox {value:?}; expected one of: read-only, local, app-container, docker"
    )]
    Unknown { value: String },
    #[error(
        "unknown network policy {value:?}; expected one of: deny (the default), internet; the older allow spelling is refused"
    )]
    UnknownNetwork { value: String },
    /// `AGENT_SANDBOX_NETWORK=allow` asks for a reach this build cannot bound.
    ///
    /// A command started by a backend reaches the network as the invoking user
    /// does; an origin allowlist is enforced on requests this process makes,
    /// not on the traffic of a program it started. Granting the flag would read
    /// as "the command may reach these hosts" while meaning "every host", so it
    /// is refused until an egress proxy exists to make the promise true.
    #[error(
        "AGENT_SANDBOX_NETWORK=allow is refused in this build: a command's own traffic cannot be \
         held to an origin allowlist, so allowing it would mean every host rather than the ones \
         configured; read the web through the controlled `web.request` tool instead"
    )]
    NetworkNotBoundable,
}

/// The backend `AGENT_SANDBOX` names.
///
/// Unset or empty means [`SandboxMode::LocalProcess`], the documented default:
/// a run that says nothing gets the backend every previous run had.
pub fn from_environment() -> Result<SandboxMode, SandboxError> {
    selected(std::env::var(ENV_SANDBOX).ok().as_deref())
}

/// The backend a value names. Split from [`from_environment`] so the decision
/// is testable without touching the process environment.
pub fn selected(value: Option<&str>) -> Result<SandboxMode, SandboxError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("local") => Ok(SandboxMode::LocalProcess),
        Some("read-only") => Ok(SandboxMode::ReadOnly),
        Some("app-container") => Ok(SandboxMode::WorkspaceSandbox),
        Some("docker") => Ok(SandboxMode::Container),
        Some(other) => Err(SandboxError::Unknown { value: other.to_string() }),
    }
}

/// Whether a sandboxed command may reach the network.
///
/// Read for every mode, not only the sandboxed ones: a typo must be reported
/// whatever backend is selected, or it would surface the first time someone
/// switched backends.
pub fn network_from_environment() -> Result<bool, SandboxError> {
    network(std::env::var(ENV_NETWORK).ok().as_deref())
}

/// Whether the host said anything about the network at all.
///
/// `deny` is the default, so "the policy has no effect here" is only worth
/// telling an operator who asked for it: the unset case would otherwise put a
/// line on every run's stderr.
pub fn network_is_explicit() -> bool {
    std::env::var(ENV_NETWORK).map(|value| !value.trim().is_empty()).unwrap_or(false)
}

/// The policy a value names. Unset means deny; `internet` is broad and opt-in;\n/// the older `allow` spelling is refused.
///
/// The flag is still parsed for every mode, so a typo — and the one value this
/// build cannot honour — is reported now rather than the first time someone
/// switches backends. `deny` stays the answer an unset variable gets, so a run
/// that says nothing keeps the promise it has always had.
pub fn network(value: Option<&str>) -> Result<bool, SandboxError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("deny") => Ok(false),
        Some("internet") => Ok(true),
        Some("allow") => Err(SandboxError::NetworkNotBoundable),
        Some(other) => Err(SandboxError::UnknownNetwork { value: other.to_string() }),
    }
}

/// The name the backend is configured under, for messages that name it.
pub fn label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::LocalProcess => "local",
        SandboxMode::WorkspaceSandbox => "app-container",
        SandboxMode::Container => "docker",
    }
}

/// The display name of the network policy, for the doctor.
pub fn network_label(allowed: bool) -> &'static str {
    if allowed {
        "internet (broad; AppContainer internetClient; not site-allowlisted)"
    } else {
        "deny"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_that_names_nothing_gets_the_local_backend() {
        assert_eq!(selected(None).unwrap(), SandboxMode::LocalProcess);
        assert_eq!(selected(Some("")).unwrap(), SandboxMode::LocalProcess);
        assert_eq!(selected(Some("  ")).unwrap(), SandboxMode::LocalProcess);
        assert_eq!(selected(Some("local")).unwrap(), SandboxMode::LocalProcess);
    }

    #[test]
    fn read_only_is_selectable() {
        assert_eq!(selected(Some("read-only")).unwrap(), SandboxMode::ReadOnly);
    }

    #[test]
    fn the_two_sandboxed_backends_are_selectable() {
        // Selectable is not the same as usable: whether a backend can be built
        // is answered by `stack::environment_for`, which refuses loudly. What
        // is pinned here is that the name reaches the mode rather than being
        // turned away at parse time with no explanation of what is missing.
        assert_eq!(selected(Some("app-container")).unwrap(), SandboxMode::WorkspaceSandbox);
        assert_eq!(selected(Some("docker")).unwrap(), SandboxMode::Container);
    }

    #[test]
    fn a_misspelled_backend_is_a_refusal_not_a_fallback() {
        // A typo must stop the run: a command that asked for confinement and
        // got a local shell would be worse than one that did not start.
        assert!(matches!(selected(Some("read_only")), Err(SandboxError::Unknown { .. })));
        assert!(matches!(selected(Some("sandbox")), Err(SandboxError::Unknown { .. })));
    }

    #[test]
    fn network_defaults_to_deny_and_refuses_unknown_values() {
        assert!(!network(None).unwrap());
        assert!(!network(Some("")).unwrap());
        assert!(!network(Some("deny")).unwrap());
        assert!(matches!(network(Some("yes")), Err(SandboxError::UnknownNetwork { .. })));
    }

    #[test]
    fn allowing_the_sandboxed_network_is_refused_rather_than_ignored() {
        // The one value this build cannot honour is an error, not a silent
        // `deny`: a host that asked for a reach and was quietly given less
        // would go on believing a command could reach what it named.
        let error = network(Some("allow")).expect_err("allow cannot be bounded here");
        assert!(matches!(error, SandboxError::NetworkNotBoundable));
        assert!(error.to_string().contains("web.request"), "{error}");
        // The same answer spelled with surrounding whitespace, which is what an
        // environment variable routinely carries.
        assert!(matches!(network(Some(" allow ")), Err(SandboxError::NetworkNotBoundable)));
    }

    #[test]
    fn internet_is_explicit_while_allow_stays_refused() {
        assert!(network(Some("internet")).unwrap());
        assert!(!network(Some(" deny ")).unwrap());
        assert!(matches!(network(Some("allow")), Err(SandboxError::NetworkNotBoundable)));
        assert!(network_label(true).contains("broad"));
    }

    #[test]
    fn every_mode_has_a_configuration_name() {
        assert_eq!(label(SandboxMode::ReadOnly), "read-only");
        assert_eq!(label(SandboxMode::LocalProcess), "local");
        assert_eq!(label(SandboxMode::WorkspaceSandbox), "app-container");
        assert_eq!(label(SandboxMode::Container), "docker");
    }
}
