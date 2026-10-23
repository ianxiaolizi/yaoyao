//! Where the configuration comes from, and what it turns into.
//!
//! `agent-config` resolves layers and reads no files; this is the edge that
//! knows where the layers live. Three sources, in the order authority runs:
//!
//! - the **managed policy**, named by `AGENT_MANAGED_POLICY` — set by whoever
//!   administers the machine, and the one layer nothing below it can undo;
//! - the **user configuration**, named by `AGENT_USER_CONFIG` — the capabilities
//!   and plugins this person has authorized at all;
//! - the **project configuration**, `<workspace>/.agent/config.toml` — written
//!   by whoever owns the repository, and the layer that must not be able to
//!   authorize itself.
//!
//! The path is read from the environment rather than derived from a home
//! directory so that a test, a container, and a person can all point the same
//! binary at a configuration without a code path apiece.

use std::path::Path;

use agent_cli::args::Args;
use agent_config::{
    ConfigError, Layers, ManagedPolicy, ResolvedProfile, UserConfig, WorkspaceConfig, resolve,
};
use agent_kernel::policy::{PolicyEngine, PolicyRule, default_policy_with};
use agent_protocol::permissions::{Capability, PolicyDecision};

/// Where the machine-wide policy lives.
pub const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// Where the user's configuration lives.
pub const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";

/// Where a project's configuration lives, relative to the workspace root.
pub const WORKSPACE_CONFIG: &str = ".agent/config.toml";

/// Why the configuration could not be assembled.
///
/// A configuration error stops the command. Every other kind of error in this
/// binary is a command that failed at something; this is a run that was never
/// authorized to begin, and running it anyway would produce a log that says the
/// session executed under a configuration no one agreed to.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{path} could not be read: {source}")]
    Read { path: String, source: std::io::Error },

    /// The file was read but its contents were refused.
    ///
    /// `ConfigError::Parse` already carries the line, column, and offending
    /// snippet, but it names the layer ("user config"), not the file — and the
    /// layer's location is exactly what the reader of the error does not know
    /// (CFG-6). Wrapping here is the edge's job: `agent-config` reads no files
    /// and must not pretend to.
    #[error("{path} could not be used: {source}")]
    Parse { path: String, source: ConfigError },

    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Read the three layers and resolve them.
pub fn load(workspace_root: &str, args: &Args) -> Result<ResolvedProfile, LoadError> {
    let mut layers = Layers {
        // The command line is the last layer and the narrowest one in time: it
        // selects, and it cannot grant. A `--model` reaches no capability, and
        // a `--profile` is still checked against the user layer's ceiling.
        profile_override: args.profile.clone(),
        model_override: args.model.clone(),
        ..Layers::default()
    };

    if let Some(path) = env_path(ENV_MANAGED_POLICY) {
        layers.managed = layer(&path, ManagedPolicy::from_toml_str)?;
    }
    if let Some(path) = env_path(ENV_USER_CONFIG) {
        layers.user = layer(&path, UserConfig::from_toml_str)?;
    }

    let project = Path::new(workspace_root).join(WORKSPACE_CONFIG);
    // Absent is the normal case, not a failure: most directories are not
    // projects, and an agent run in one is a run under the defaults.
    if project.exists() {
        layers.workspace = layer(&project.display().to_string(), WorkspaceConfig::from_toml_str)?;
    }

    Ok(resolve(layers)?)
}

/// The policy this run's configuration describes, over what its backend
/// actually advertises.
///
/// Two halves, from two questions. `replaces_defaults` says whether the
/// configuration wrote its own rules; when it did not, the built-in rule set
/// still applies. The sandbox set is neither — it is what the bound execution
/// backend can do, so it is present whatever the configuration says, and a
/// capability granted in a configuration but absent here fails closed. It is
/// an argument rather than a constant because a read-only run or a
/// confinement backend provides less, and a rule that could not be enforced
/// must not read as granted.
pub fn policy_engine(profile: &ResolvedProfile, advertised: &[Capability]) -> PolicyEngine {
    let base = if profile.policy.replaces_defaults {
        PolicyEngine::new().with_sandbox(advertised.iter().cloned())
    } else {
        default_policy_with(advertised)
    };

    base.with_managed_deny(profile.policy.managed_deny.iter().cloned())
        .with_rules(profile.policy.rules.iter().map(rule))
        .with_override_rules(profile.policy.overrides.iter().map(rule))
}

fn rule(directive: &agent_config::Directive) -> PolicyRule {
    let capability = directive.capability.clone();
    match directive.decision {
        PolicyDecision::Allow => PolicyRule::allow(capability),
        PolicyDecision::Ask => PolicyRule::ask(capability),
        PolicyDecision::Deny => PolicyRule::deny(capability),
    }
}

/// The path an environment variable names, if it names one and names something
/// that is there.
///
/// An empty value is treated as unset rather than as a path, because that is
/// what an empty environment variable means in practice — it is how a shell
/// clears one.
fn env_path(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(path) if !path.trim().is_empty() => Some(path),
        _ => None,
    }
}

fn read(path: impl AsRef<Path>) -> Result<String, LoadError> {
    let path = path.as_ref();
    std::fs::read_to_string(path)
        .map_err(|source| LoadError::Read { path: path.display().to_string(), source })
}

/// Read one layer and parse it, naming the file if either step fails.
fn layer<T>(
    path: &str,
    parse: impl FnOnce(&str) -> Result<T, ConfigError>,
) -> Result<T, LoadError> {
    parsed(path, &read(path)?, parse)
}

/// Parse one layer's text, attaching the file it came from to any refusal.
fn parsed<T>(
    path: &str,
    text: &str,
    parse: impl FnOnce(&str) -> Result<T, ConfigError>,
) -> Result<T, LoadError> {
    parse(text).map_err(|source| LoadError::Parse { path: path.to_string(), source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::permissions::Capability;

    #[test]
    fn a_layer_that_does_not_parse_names_the_file_and_the_line() {
        // `agent-config` names the layer, not the file — it has never seen a
        // path. The edge knows the path, so it is the edge that has to put the
        // two together (CFG-6). No filesystem here: `parsed` takes the text.
        let error = parsed(
            "C:/cfg/user.toml",
            "capabilities = [\nprofile = 3\n",
            UserConfig::from_toml_str,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("C:/cfg/user.toml"), "{message}");
        assert!(message.contains("line"), "{message} has no line number");
        assert!(message.contains("user config"), "{message} does not say which layer");
    }

    #[test]
    fn a_project_configuration_is_read_only_for_what_it_may_say() {
        // The reason this module exists is that a project can narrow and cannot
        // grant; if that ever stops being true it will be here, at the edge,
        // where the file is actually read.
        let parsed = WorkspaceConfig::from_toml_str("capabilities = [\"fs.write\"]\n");
        assert!(parsed.is_err(), "a project may not authorize a capability");
    }

    #[test]
    fn a_profile_that_only_denies_keeps_the_permissions_it_did_not_name() {
        // The end of the chain YA-10 is about: what the engine actually answers
        // a call. `resolve` alone can only promise `replaces_defaults` is false;
        // the promise that matters is that `fs.read` still reads as allowed.
        let layers = Layers {
            user: UserConfig::from_toml_str(
                r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

[profiles.careful]
tools = { deny = ["process.spawn"] }
"#,
            )
            .unwrap(),
            profile_override: Some("careful".to_string()),
            ..Layers::default()
        };

        let engine = policy_engine(
            &resolve(layers).unwrap(),
            &agent_kernel::policy::default_sandbox_capabilities(),
        );
        let decide = |name: &str| engine.evaluate(&[Capability::new(name)], &[]);

        assert_eq!(
            decide(Capability::PROCESS_SPAWN),
            PolicyDecision::Deny,
            "the denial is in force"
        );
        assert_eq!(
            decide(Capability::FS_READ),
            PolicyDecision::Allow,
            "and the default it never mentioned is untouched"
        );
    }

    #[test]
    fn a_workspace_cannot_choose_its_own_sandbox() {
        // The sandbox is the host's decision. If a project file could set it,
        // a repository could ask for less confinement than the machine's
        // operator chose — the one direction a boundary must not move.
        let error = WorkspaceConfig::from_toml_str("sandbox = \"local\"\n")
            .expect_err("a workspace that names a sandbox must not parse");
        assert!(error.to_string().contains("sandbox"), "the refusal names the key: {error}");
    }

    #[test]
    fn a_read_only_run_refuses_what_its_backend_cannot_do() {
        // The default rules allow `fs.write` and ask about `process.spawn`; a
        // read-only backend has neither, and "the sandbox cannot" outranks
        // both. The refusal names the layer, so a caller can tell a backend
        // limitation from a rule someone wrote.
        let engine = policy_engine(
            &resolve(Layers::default()).unwrap(),
            &agent_ports::execution_environment::SandboxMode::ReadOnly.advertised_capabilities(),
        );

        for capability in [Capability::FS_WRITE, Capability::PROCESS_SPAWN] {
            let capability = Capability::new(capability);
            assert_eq!(
                engine.evaluate(std::slice::from_ref(&capability), &[]),
                PolicyDecision::Deny,
                "{capability} must be refused in a read-only run"
            );
            let refusals = engine.refusals(std::slice::from_ref(&capability), None, &[]);
            assert_eq!(
                refusals,
                vec![(capability, agent_kernel::policy::Refusal::SandboxUnavailable)]
            );
        }

        assert_eq!(
            engine.evaluate(&[Capability::new(Capability::FS_READ)], &[]),
            PolicyDecision::Allow,
            "reading is what a read-only run is for"
        );
    }

    #[test]
    fn a_policy_section_leaves_the_defaults_it_did_not_mention_alone() {
        // The end of the chain for `[policy]`, the sibling of YA-10: the section
        // writes one disposition, and the defaults for everything it is silent
        // about must survive. Before this fix a no-op `shell = "ask"` also took
        // `fs.read` down to a fail-closed deny.
        let layers = Layers {
            user: UserConfig::from_toml_str(
                r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

[profiles.careful]
policy = { shell = "ask" }
"#,
            )
            .unwrap(),
            profile_override: Some("careful".to_string()),
            ..Layers::default()
        };

        let engine = policy_engine(
            &resolve(layers).unwrap(),
            &agent_kernel::policy::default_sandbox_capabilities(),
        );
        let decide = |name: &str| engine.evaluate(&[Capability::new(name)], &[]);

        assert_eq!(
            decide(Capability::PROCESS_SPAWN),
            PolicyDecision::Ask,
            "the section's own disposition is in force"
        );
        assert_eq!(
            decide(Capability::FS_READ),
            PolicyDecision::Allow,
            "and the default it never mentioned is untouched"
        );
    }

    #[test]
    fn a_policy_section_that_allows_is_not_outvoted_by_the_default_ask() {
        // Found by running the agent: `shell = "allow"` read as allowed, but
        // the run still stopped on an approval because the appended Allow tied
        // with the built-in Ask and the tie went to the stricter rule. A knob
        // that cannot do what it says is worse than no knob (CONTRACT.md §7
        // row 14), so an override must actually replace the disposition.
        let layers = Layers {
            user: UserConfig::from_toml_str(
                r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

[profiles.permissive]
policy = { shell = "allow" }
"#,
            )
            .unwrap(),
            profile_override: Some("permissive".to_string()),
            ..Layers::default()
        };

        let engine = policy_engine(
            &resolve(layers).unwrap(),
            &agent_kernel::policy::default_sandbox_capabilities(),
        );
        let decide = |name: &str| engine.evaluate(&[Capability::new(name)], &[]);

        assert_eq!(
            decide(Capability::PROCESS_SPAWN),
            PolicyDecision::Allow,
            "the section's allow must reach the engine"
        );
        assert_eq!(
            decide(Capability::FS_READ),
            PolicyDecision::Allow,
            "and the defaults it did not name are untouched"
        );
    }
}
