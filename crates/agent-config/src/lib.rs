//! agent-config: the configuration layers, and how they resolve into one
//! profile.
//!
//! The crate reads no files. Each layer has a `from_toml_str`, and the binary
//! that knows where the files live calls it — the rule that external I/O
//! belongs at the edge is also what makes every rule below testable without a
//! filesystem, a home directory, or an environment.
//!
//! What it produces is two answers, because configuration answers two
//! different questions and a system that conflates them gets one of them
//! wrong: what this run is *authorized* to do, and what happens when a tool
//! *asks*. See [`resolve`] for how the layers combine and where authority
//! stops moving down the stack.

#![deny(unsafe_code)]

pub mod layers;
pub mod resolve;

pub use layers::{
    BudgetSpec, DecisionsSpec, Disposition, LayaSpec, ManagedPolicy, McpServer, McpSpec, ModelSpec,
    PluginsSpec, PolicySpec, Profile, ProfileDecisions, SubagentSpec, ToolsSpec, UserConfig,
    WorkspaceConfig, WorkspaceTools, parse_duration_ms,
};
pub use resolve::{
    Directive, Layers, McpServerConfig, McpTransport, ResolvedLaya, ResolvedPolicy,
    ResolvedProfile, SubagentConfig, resolve,
};

/// Why a configuration could not be resolved.
///
/// Every variant is a refusal with a message a person can act on. None of them
/// is a warning: a configuration that says one thing while the run does
/// another is worse than a run that does not start, because it is the kind of
/// difference nobody notices.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{layer} could not be parsed: {message}")]
    Parse { layer: String, message: String },

    #[error("'{0}' is not a profile this configuration defines")]
    UnknownProfile(String),

    #[error("profiles extend each other in a circle: {lineage}")]
    CircularExtends { lineage: String },

    #[error(
        "profile '{profile}' requires {capabilities:?}, which the user configuration does not \
         authorize; a profile may select inside that set, not add to it"
    )]
    UnauthorizedCapability { profile: String, capabilities: Vec<String> },

    #[error(
        "profile '{profile}' enables plugin '{plugin}', which the user configuration does not \
         trust; add it to the user configuration's `plugins` to authorize it"
    )]
    UntrustedPlugin { profile: String, plugin: String },

    #[error("profile '{profile}' both enables and disables plugin '{plugin}'")]
    PluginEnabledAndDisabled { profile: String, plugin: String },

    #[error(
        "plugin '{plugin}' is enabled but no layer defines the MCP server '{server}'; add \
         [mcp.servers.{server}] to the user configuration"
    )]
    UndefinedMcpServer { plugin: String, server: String },

    #[error("{layer} defines MCP server '{server}', which cannot be used: {detail}")]
    InvalidMcpServer { layer: String, server: String, detail: String },

    #[error(
        "MCP server '{server}' requires {capabilities:?}, which this run is not authorized for; \
         every tool under a server inherits its capability claim"
    )]
    UnauthorizedMcpCapability { server: String, capabilities: Vec<String> },

    #[error(
        "plugin '{plugin}' is enabled but no layer defines the subagent provider '{provider}'; \
         add [subagents.{provider}] to the user configuration"
    )]
    UndefinedSubagent { plugin: String, provider: String },

    #[error("{layer} defines subagent provider '{provider}', which cannot be used: {detail}")]
    InvalidSubagent { layer: String, provider: String, detail: String },

    #[error(
        "subagent provider '{provider}' requires {capabilities:?}, which this run is not \
         authorized for; delegating is something the parent does, and a child does not inherit \
         the parent's permissions"
    )]
    UnauthorizedSubagentCapability { provider: String, capabilities: Vec<String> },

    #[error("{layer} is not supported: {detail}")]
    Unsupported { layer: String, detail: String },

    /// A limit set to zero. Zero is not "no limit" — that is the field being
    /// absent — it is a run that stops before it starts (CFG-4).
    #[error("{layer} sets a limit that cannot be run: {detail}")]
    InvalidBudget { layer: String, detail: String },

    #[error(
        "profile '{profile}' enables the Laya sidecar, but no layer defines it; add \
         [decisions.laya] to the user configuration"
    )]
    UndefinedLaya { profile: String },

    #[error("{layer} defines the Laya sidecar, which cannot be used: {detail}")]
    InvalidLaya { layer: String, detail: String },

    /// The file's `schema_version` is newer than this build reads, older with no
    /// migration, or not a version at all (YA-67). Kept distinct from
    /// [`Self::Parse`] so the reader can tell "the file is for another build"
    /// from "the file is misspelled" — the distinction YA-67 found missing.
    #[error("{layer} has an unsupported schema version: {source}")]
    Version {
        layer: String,
        #[source]
        source: agent_protocol::versioning::VersionError,
    },
}

/// What the layers resolve to, or why they do not.
pub type Result<T> = std::result::Result<T, ConfigError>;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::model::{ModelRoute, ModelTier};
    use agent_protocol::permissions::{Capability, PolicyDecision};

    /// The layers a test is about, with everything else empty.
    fn no_layers() -> Layers {
        Layers::default()
    }

    fn route() -> ModelRoute {
        ModelRoute::new(ModelTier::Main, "echo", "echo")
    }

    fn caps(names: &[&str]) -> Vec<Capability> {
        names.iter().map(|name| Capability::new(*name)).collect()
    }

    #[test]
    fn no_configuration_at_all_is_the_built_in_defaults() {
        // Not a gap to be filled but a configuration in its own right: the
        // policy and capability defaults are the ones every command had before
        // profiles existed. The budget is the one deliberate exception: a run
        // that names no window now ships with the regular working set, because
        // "send the whole history" is not a safe product default.
        let resolved = resolve(no_layers()).unwrap();
        assert_eq!(resolved.profile_id, "default");
        assert_eq!(resolved.model, None);
        assert!(resolved.capability_set.is_empty(), "no layer authorized anything");
        assert!(!resolved.policy.replaces_defaults, "the built-in rules still apply");
        assert!(resolved.policy.rules.is_empty());
        assert_eq!(resolved.budget.max_steps, 8);
        assert!(!resolved.policy_version.is_empty(), "the policy is identifiable either way");
    }

    #[test]
    fn a_run_that_names_no_window_gets_the_shipped_working_set() {
        let budget = resolve(no_layers()).unwrap().budget;
        assert_eq!(budget.max_input_tokens, Some(8_000));
        assert_eq!(budget.reserved_output_tokens, Some(2_000));
        assert_eq!(budget.compact_at_percent, Some(75));
    }

    #[test]
    fn a_profile_that_only_sets_limits_still_gets_the_working_set() {
        // Step and tool-call limits say nothing about the window; tightening
        // them must not turn the shipped working set off.
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
[profiles.p]
budget = { max_steps = 4, max_tool_calls = 6 }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());
        let budget = resolve(layers).unwrap().budget;
        assert_eq!(budget.max_steps, 4);
        assert_eq!(budget.max_tool_calls, Some(6));
        assert_eq!(budget.max_input_tokens, Some(8_000));
        assert_eq!(budget.compact_at_percent, Some(75));
    }

    #[test]
    fn an_explicit_window_opts_out_of_the_shipped_reservation() {
        // The old small-window configurations stay valid: naming only a window
        // must not inherit a 2,000-token reservation it never asked for.
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
[profiles.p]
budget = { max_input_tokens = 1000 }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());
        let budget = resolve(layers).unwrap().budget;
        assert_eq!(budget.max_input_tokens, Some(1000));
        assert_eq!(budget.reserved_output_tokens, None);
        assert_eq!(budget.compact_at_percent, None);
    }

    #[test]
    fn a_profile_composes_and_the_cli_chooses_it() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "fs.write"]

[profiles.base]
model = { main = "small" }
budget = { max_steps = 4 }

[profiles.coding]
extends = "base"
model = { main = "big" }
tools = { allow = ["fs.read"] }
policy = { shell = "ask" }
"#,
        )
        .unwrap();
        layers.profile_override = Some("coding".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(resolved.profile_id, "coding");
        assert_eq!(resolved.model.as_deref(), Some("big"), "the child names the model");
        assert_eq!(resolved.budget.max_steps, 4, "and inherits what it does not name");
        assert_eq!(resolved.capability_set, caps(&["fs.read"]).into_iter().collect());
        assert!(resolved.policy.replaces_defaults);
        assert!(resolved.policy.overrides.contains(&Directive {
            decision: PolicyDecision::Ask,
            capability: Capability::new(Capability::PROCESS_SPAWN),
        }));
    }

    #[test]
    fn a_profile_may_not_add_to_what_the_user_authorized() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read"]

[profiles.reach]
tools = { allow = ["fs.read", "git.push"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("reach".to_string());

        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::UnauthorizedCapability {
                profile: "reach".to_string(),
                capabilities: vec!["git.push".to_string()],
            }
        );
    }

    #[test]
    fn managed_deny_beats_every_layer_and_says_so() {
        let mut layers = no_layers();
        layers.managed = ManagedPolicy::from_toml_str("deny = [\"git.push\"]\n").unwrap();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "git.push"]

[profiles.shipping]
tools = { allow = ["git.push", "fs.read"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("shipping".to_string());

        let resolved = resolve(layers).unwrap();
        // Not an error: the operator's refusal is absolute, and a user config
        // that mentions a denied capability is not something the operator
        // should have to go and fix in every file.
        assert_eq!(resolved.capability_set, caps(&["fs.read"]).into_iter().collect());
        assert_eq!(
            resolved.notes,
            vec!["capability 'git.push' is denied by managed policy and was not granted"]
        );
        assert!(resolved.policy.managed_deny.contains(&Capability::new("git.push")));
    }

    #[test]
    fn a_project_may_narrow_what_the_profile_allows() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

[profiles.coding]
tools = { allow = ["fs.read", "fs.write", "process.spawn"] }
"#,
        )
        .unwrap();
        layers.workspace = WorkspaceConfig::from_toml_str(
            r#"
profile = "coding"

[tools]
deny = ["process.spawn"]
"#,
        )
        .unwrap();

        let resolved = resolve(layers).unwrap();
        // The refusal is a rule, applied after the profile's — so it holds even
        // though the profile permits the capability it denies.
        assert_eq!(
            resolved.policy.rules.last(),
            Some(&Directive {
                decision: PolicyDecision::Deny,
                capability: Capability::new(Capability::PROCESS_SPAWN),
            })
        );
        // And the profile's own allow rule for it is still there, unordered:
        // the engine takes the strictest disposition, so both survive.
        assert!(resolved.policy.rules.contains(&Directive {
            decision: PolicyDecision::Allow,
            capability: Capability::new(Capability::PROCESS_SPAWN),
        }));
    }

    #[test]
    fn a_profile_that_only_denies_narrows_the_defaults() {
        // The two halves of "[tools] spoke" must be the same predicate. Read as
        // a whitelist, a `deny`-only section produced the *widest* capability
        // set and the *fewest* rules — the user writes "deny one thing" and
        // gets "allow only that one" (YA-10).
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

[profiles.careful]
tools = { deny = ["process.spawn"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("careful".to_string());

        let resolved = resolve(layers).unwrap();
        assert!(
            !resolved.policy.replaces_defaults,
            "nothing was listed, so the built-in rules still stand"
        );
        assert_eq!(
            resolved.capability_set,
            caps(&["fs.read", "fs.write", "process.spawn"]).into_iter().collect(),
            "and nothing was narrowed out of what the user authorized"
        );
        assert_eq!(
            resolved.policy.rules,
            vec![Directive {
                decision: PolicyDecision::Deny,
                capability: Capability::new(Capability::PROCESS_SPAWN),
            }],
            "the denial is the only rule it adds"
        );
    }

    #[test]
    fn a_policy_section_adds_to_the_defaults_instead_of_replacing_them() {
        // The sibling of YA-10, on the other section that spoke. `[policy]`'s
        // three knobs cannot name a whole set, so "replace" is not an intent they
        // can express — and reading it as one dropped the rules the section never
        // mentioned. `shell = "ask"` is what the default already says about
        // `process.spawn`, yet it used to take the default's allow for `fs.read`
        // away with it.
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

[profiles.careful]
policy = { shell = "ask" }
"#,
        )
        .unwrap();
        layers.profile_override = Some("careful".to_string());

        let resolved = resolve(layers).unwrap();
        assert!(
            !resolved.policy.replaces_defaults,
            "the section adds a disposition; it does not become the whole set"
        );
        assert!(resolved.policy.rules.is_empty(), "it appends no ordinary rule");
        assert_eq!(
            resolved.policy.overrides,
            vec![Directive {
                decision: PolicyDecision::Ask,
                capability: Capability::new(Capability::PROCESS_SPAWN),
            }],
            "and the one override it adds is exactly the knob it wrote"
        );
    }

    #[test]
    fn a_project_can_pick_a_profile_but_not_invent_one() {
        let mut layers = no_layers();
        layers.workspace = WorkspaceConfig::from_toml_str("profile = \"mine\"\n").unwrap();
        assert_eq!(resolve(layers).unwrap_err(), ConfigError::UnknownProfile("mine".to_string()));
    }

    #[test]
    fn a_profile_may_not_enable_a_plugin_the_user_does_not_trust() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
plugins = ["mcp.trusted@1.2.0"]

[profiles.reach]
plugins = { enable = ["mcp.unknown"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("reach".to_string());
        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::UntrustedPlugin {
                profile: "reach".to_string(),
                plugin: "mcp.unknown".to_string(),
            }
        );
    }

    #[test]
    fn an_enabled_plugin_carries_the_version_it_was_trusted_at() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
plugins = ["mcp.pinned@1.2.0", "mcp.any"]

[mcp.servers.pinned]
command = "pinned-mcp"

[mcp.servers.any]
url = "https://example.test/mcp"

[profiles.coding]
plugins = { enable = ["mcp.pinned", "mcp.any"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("coding".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(resolved.plugin_versions["mcp.pinned"], "1.2.0");
        assert_eq!(resolved.plugin_versions["mcp.any"], "*", "trusted at any version");
        assert_eq!(
            resolved.mcp_servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["any", "pinned"],
            "and the enabled servers are the ones this run reaches"
        );
    }

    #[test]
    fn an_enabled_server_is_resolved_with_its_transport_and_its_claim() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "network.request"]
plugins = ["mcp.files@1.2.0", "mcp.web"]

[mcp.servers.files]
command = "files-mcp"
args = ["--root", "."]
env = { LOG = "info" }
cwd = "/tmp"
requires = ["fs.read"]
timeout = "90s"

[mcp.servers.web]
url = "https://example.test/mcp"
headers = { Authorization = "Bearer x" }
requires = []

[profiles.p]
plugins = { enable = ["mcp.files", "mcp.web"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(
            resolved.mcp_servers,
            vec![
                McpServerConfig {
                    name: "files".to_string(),
                    transport: McpTransport::Stdio {
                        command: "files-mcp".to_string(),
                        args: vec!["--root".to_string(), ".".to_string()],
                        env: vec![("LOG".to_string(), "info".to_string())],
                        cwd: Some("/tmp".to_string()),
                    },
                    requires: caps(&["fs.read"]),
                    timeout_ms: Some(90_000),
                },
                McpServerConfig {
                    name: "web".to_string(),
                    transport: McpTransport::Http {
                        url: "https://example.test/mcp".to_string(),
                        headers: vec![("Authorization".to_string(), "Bearer x".to_string())],
                    },
                    requires: vec![],
                    timeout_ms: None,
                },
            ]
        );
    }

    #[test]
    fn an_enabled_server_no_layer_defines_is_a_refusal() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
plugins = ["mcp.ghost"]

[profiles.p]
plugins = { enable = ["mcp.ghost"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::UndefinedMcpServer {
                plugin: "mcp.ghost".to_string(),
                server: "ghost".to_string(),
            }
        );
    }

    #[test]
    fn a_server_that_asks_for_what_the_run_was_not_given_is_a_refusal() {
        // The same question the registry asks at registration, asked of the file
        // instead of after a process has been started.
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read"]
plugins = ["mcp.web"]

[mcp.servers.web]
url = "https://example.test/mcp"
requires = ["network.request"]

[profiles.p]
plugins = { enable = ["mcp.web"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::UnauthorizedMcpCapability {
                server: "web".to_string(),
                capabilities: vec!["network.request".to_string()],
            }
        );
    }

    #[test]
    fn the_operators_definition_replaces_the_users_and_says_so() {
        let mut layers = no_layers();
        layers.managed =
            ManagedPolicy::from_toml_str("[mcp.servers.files]\ncommand = \"operator-mcp\"\n")
                .unwrap();
        layers.user = UserConfig::from_toml_str(
            r#"
plugins = ["mcp.files"]

[mcp.servers.files]
command = "user-mcp"

[profiles.p]
plugins = { enable = ["mcp.files"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(
            resolved.mcp_servers[0].transport,
            McpTransport::Stdio {
                command: "operator-mcp".to_string(),
                args: vec![],
                env: vec![],
                cwd: None,
            },
            "a user file does not get to point the operator's server elsewhere"
        );
        assert!(
            resolved.notes.iter().any(|note| note.contains("managed policy")),
            "{:?}",
            resolved.notes
        );
    }

    #[test]
    fn a_server_that_names_two_transports_or_none_is_a_refusal() {
        let load = |server: &str| {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!(
                "plugins = [\"mcp.files\"]\n\n[mcp.servers.files]\n{server}\n\n\
                 [profiles.p]\nplugins = {{ enable = [\"mcp.files\"] }}\n"
            ))
            .unwrap();
            layers.profile_override = Some("p".to_string());
            resolve(layers).unwrap_err()
        };

        for (server, expected) in [
            (
                "command = \"a-mcp\"\nurl = \"https://example.test/mcp\"",
                "both a `command` and a `url`",
            ),
            ("timeout = \"5s\"", "neither a `command`"),
            (
                "url = \"https://example.test/mcp\"\nargs = [\"--x\"]",
                "belong to a server reached by `command`",
            ),
            ("command = \"a-mcp\"\nheaders = { X = \"y\" }", "belong to a `url` server"),
            (
                "command = \"a-mcp\"\ntimeout = \"0s\"",
                "`timeout` is zero, which would cut every call off before it starts",
            ),
        ] {
            match load(server) {
                ConfigError::InvalidMcpServer { layer, server, detail } => {
                    assert_eq!(layer, "user config");
                    assert_eq!(server, "files");
                    assert!(detail.contains(expected), "{detail} did not mention '{expected}'");
                }
                other => panic!("expected a refusal for {server}, got {other}"),
            }
        }
    }

    #[test]
    fn an_enabled_provider_is_resolved_with_its_ceiling_and_its_claim() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["process.spawn"]
plugins = ["subagent.fake@1.0.0"]

[subagents.fake]
command = "fake-subagent"
args = ["--mode", "result"]
env = { LOG = "info" }
cwd = "/tmp"
requires = ["process.spawn"]
timeout = "5m"

[profiles.p]
plugins = { enable = ["subagent.fake"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(
            resolved.subagents,
            vec![SubagentConfig {
                provider: "fake".to_string(),
                command: "fake-subagent".to_string(),
                args: vec!["--mode".to_string(), "result".to_string()],
                env: vec![("LOG".to_string(), "info".to_string())],
                cwd: Some("/tmp".to_string()),
                requires: caps(&["process.spawn"]),
                ceiling_ms: Some(300_000),
            }]
        );
        assert_eq!(resolved.plugin_versions["subagent.fake"], "1.0.0");
    }

    #[test]
    fn an_enabled_provider_no_layer_defines_is_a_refusal() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
plugins = ["subagent.ghost"]

[profiles.p]
plugins = { enable = ["subagent.ghost"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::UndefinedSubagent {
                plugin: "subagent.ghost".to_string(),
                provider: "ghost".to_string(),
            }
        );
    }

    #[test]
    fn a_provider_that_asks_for_what_the_run_was_not_given_is_a_refusal() {
        // The claim a delegation is answered against: a child does not
        // inherit the parent's permissions. Asked of the file that says "this
        // runs a program" rather than in the middle of a turn that has already
        // forked a child.
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read"]
plugins = ["subagent.fake"]

[subagents.fake]
command = "fake-subagent"
requires = ["process.spawn"]

[profiles.p]
plugins = { enable = ["subagent.fake"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::UnauthorizedSubagentCapability {
                provider: "fake".to_string(),
                capabilities: vec!["process.spawn".to_string()],
            }
        );
    }

    #[test]
    fn the_operators_subagent_definition_replaces_the_users_and_says_so() {
        let mut layers = no_layers();
        layers.managed =
            ManagedPolicy::from_toml_str("[subagents.fake]\ncommand = \"operator-subagent\"\n")
                .unwrap();
        layers.user = UserConfig::from_toml_str(
            r#"
plugins = ["subagent.fake"]

[subagents.fake]
command = "user-subagent"

[profiles.p]
plugins = { enable = ["subagent.fake"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(
            resolved.subagents[0].command, "operator-subagent",
            "a user file does not get to point the operator's provider elsewhere"
        );
        assert!(
            resolved.notes.iter().any(|note| note.contains("managed policy")),
            "{:?}",
            resolved.notes
        );
    }

    #[test]
    fn a_provider_with_nothing_to_run_is_a_refusal() {
        let load = |provider: &str| {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!(
                "plugins = [\"subagent.fake\"]\n\n[subagents.fake]\n{provider}\n\n\
                 [profiles.p]\nplugins = {{ enable = [\"subagent.fake\"] }}\n"
            ))
            .unwrap();
            layers.profile_override = Some("p".to_string());
            resolve(layers).unwrap_err()
        };

        for (provider, expected) in [
            ("command = \"\"", "`command` is empty"),
            ("command = \"fake-subagent\"\ntimeout = \"soon\"", "not a duration"),
            ("command = \"fake-subagent\"\ntimeout = \"0s\"", "`timeout` is zero"),
        ] {
            match load(provider) {
                ConfigError::InvalidSubagent { layer, provider, detail } => {
                    assert_eq!(layer, "user config");
                    assert_eq!(provider, "fake");
                    assert!(detail.contains(expected), "{detail} did not mention '{expected}'");
                }
                other => panic!("expected a refusal for {provider}, got {other}"),
            }
        }

        // Naming no program at all never reaches resolution: the entry requires
        // one, so the file is refused while it is being read.
        assert!(matches!(
            UserConfig::from_toml_str("[subagents.fake]\nrequires = []\n"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn a_server_nobody_enabled_is_defined_but_not_reached() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
[mcp.servers.files]
command = "files-mcp"
"#,
        )
        .unwrap();

        let resolved = resolve(layers).unwrap();
        assert!(resolved.mcp_servers.is_empty(), "a definition is not a connection");
        assert!(resolved.notes.is_empty(), "and not something to report either");
    }

    #[test]
    fn an_enabled_plugin_of_neither_family_is_reported_and_not_loaded() {
        // One note, naming the plugin this build cannot start — and not naming
        // the one it can. Asking the question inside either family would have
        // forced the other family's plugins to look unstartable to it.
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
plugins = ["vendor.pack", "subagent.fake"]

[subagents.fake]
command = "fake-subagent"

[profiles.p]
plugins = { enable = ["vendor.pack", "subagent.fake"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        let resolved = resolve(layers).unwrap();
        assert!(resolved.mcp_servers.is_empty());
        assert_eq!(
            resolved.subagents.iter().map(|s| s.provider.as_str()).collect::<Vec<_>>(),
            vec!["fake"]
        );
        assert_eq!(resolved.notes.len(), 1, "{:?}", resolved.notes);
        assert!(resolved.notes[0].contains("vendor.pack"), "{:?}", resolved.notes);
        assert!(resolved.notes[0].contains("subagent."), "{:?}", resolved.notes);
    }

    #[test]
    fn extends_cannot_go_in_a_circle() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
[profiles.a]
extends = "b"

[profiles.b]
extends = "a"
"#,
        )
        .unwrap();
        layers.profile_override = Some("a".to_string());

        match resolve(layers).unwrap_err() {
            ConfigError::CircularExtends { lineage } => {
                assert_eq!(lineage, "a -> b -> a", "the circle names itself");
            }
            other => panic!("expected a circle, got {other}"),
        }
    }

    #[test]
    fn a_deny_a_parent_set_survives_into_the_child() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "fs.write"]

[profiles.base]
tools = { allow = ["fs.read"], deny = ["network.request"] }

[profiles.child]
extends = "base"
tools = { allow = ["fs.read", "fs.write"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("child".to_string());

        let resolved = resolve(layers).unwrap();
        // The child narrowed nothing and widened its allow list; the parent's
        // refusal is still there, because composing may add refusals and must
        // not drop them.
        assert!(resolved.policy.rules.contains(&Directive {
            decision: PolicyDecision::Deny,
            capability: Capability::new(Capability::NETWORK_REQUEST),
        }));
    }

    #[test]
    fn the_snapshot_carries_the_configuration_and_seals_itself() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read"]
plugins = ["mcp.foo@1.0.0"]

[mcp.servers.foo]
command = "foo-mcp"

[profiles.coding]
plugins = { enable = ["mcp.foo"] }
"#,
        )
        .unwrap();
        layers.profile_override = Some("coding".to_string());
        layers.model_override = Some("chosen-on-the-command-line".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(resolved.model.as_deref(), Some("chosen-on-the-command-line"));
        let policy_version = resolved.policy_version.clone();
        let snapshot = resolved.into_snapshot(route());

        assert_eq!(snapshot.profile_id, "coding");
        assert_eq!(snapshot.plugin_versions["mcp.foo"], "1.0.0");
        assert_eq!(snapshot.policy_version, policy_version);
        assert_eq!(snapshot.budget.max_steps, 8);
        assert!(!snapshot.config_hash.is_empty());
    }

    #[test]
    fn the_hash_moves_when_the_configuration_does() {
        // Two runs of the same config are the same run; two runs of different
        // configs must not be, or the field is decoration.
        let seal = |steps: u32, route: ModelRoute| {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!(
                "[profiles.coding]\nbudget = {{ max_steps = {steps} }}\n"
            ))
            .unwrap();
            layers.profile_override = Some("coding".to_string());
            resolve(layers).unwrap().into_snapshot(route).config_hash
        };

        assert_eq!(seal(8, route()), seal(8, route()));
        assert_ne!(seal(8, route()), seal(9, route()));
        assert_ne!(seal(8, route()), seal(8, ModelRoute::new(ModelTier::Main, "echo", "other")));
    }

    #[test]
    fn the_snapshot_records_the_route_that_ran_not_the_name_that_was_asked_for() {
        // A configuration names a model; whether that name is what served the
        // turn is the app's answer, and it can differ — a provider that does
        // not honor the request still ran *something*. What a reader needs to
        // compare two runs is what actually happened.
        let mut layers = no_layers();
        layers.user =
            UserConfig::from_toml_str("[profiles.coding]\nmodel = { main = \"asked-for\" }\n")
                .unwrap();
        layers.profile_override = Some("coding".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(resolved.model.as_deref(), Some("asked-for"));
        let snapshot = resolved.into_snapshot(ModelRoute::new(ModelTier::Main, "echo", "echo"));
        assert_eq!(snapshot.model_route.model, "echo");
    }

    #[test]
    fn the_policy_version_moves_with_the_policy_alone() {
        let with_policy = |spec: &str| {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!("[profiles.p]\n{spec}")).unwrap();
            layers.profile_override = Some("p".to_string());
            resolve(layers).unwrap().policy_version
        };

        assert_eq!(
            with_policy("policy = { shell = \"ask\" }\n"),
            with_policy("policy = { shell = \"ask\" }\n")
        );
        assert_ne!(
            with_policy("policy = { shell = \"ask\" }\n"),
            with_policy("policy = { shell = \"deny\" }\n")
        );
        assert_ne!(
            with_policy("policy = { shell = \"ask\" }\n"),
            with_policy("policy = { shell = \"ask\", network = \"deny\" }\n")
        );
    }

    #[test]
    fn the_project_lowers_the_budget_and_the_profile_still_has_the_last_word() {
        let load = |workspace: &str, profile: &str| {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!("[profiles.p]\n{profile}")).unwrap();
            layers.workspace = WorkspaceConfig::from_toml_str(workspace).unwrap();
            layers.profile_override = Some("p".to_string());
            resolve(layers).unwrap().budget
        };

        let lowered = load("[budget]\nmax_steps = 2\n", "");
        assert_eq!(lowered.max_steps, 2);

        // A budget is a normal setting, so the closer layer wins — the
        // project is not a ceiling on it the way it is on a permission.
        let overridden = load("[budget]\nmax_steps = 2\n", "budget = { max_steps = 12 }\n");
        assert_eq!(overridden.max_steps, 12);
    }

    #[test]
    fn a_defined_sidecar_is_attached_and_a_profile_can_turn_it_off() {
        let user = r#"
[decisions.laya]
url = "http://127.0.0.1:8231/v1/systemone"
model = "multilingual"
threshold = 0.6

[profiles.quiet]
decisions = { laya = false }

[profiles.loud]
decisions = { laya = true }
"#;

        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(user).unwrap();
        // A definition nobody turned off is on: the layer that wrote it is the
        // layer that authorized it.
        let resolved = resolve(layers.clone()).unwrap();
        assert_eq!(
            resolved.laya,
            Some(ResolvedLaya {
                url: "http://127.0.0.1:8231/v1/systemone".to_string(),
                model: Some("multilingual".to_string()),
                threshold: Some(0.6),
            })
        );
        assert_eq!(
            resolved.decision_backends,
            vec!["laya:http://127.0.0.1:8231/v1/systemone@multilingual".to_string()]
        );

        // The scenario that turns it off carries no sidecar and says so in its
        // snapshot, so two runs are not compared as if they were the same.
        let mut off = layers.clone();
        off.profile_override = Some("quiet".to_string());
        let resolved = resolve(off).unwrap();
        assert_eq!(resolved.laya, None);
        assert!(resolved.decision_backends.is_empty());

        let mut on = layers;
        on.profile_override = Some("loud".to_string());
        assert!(resolve(on).unwrap().laya.is_some());
    }

    #[test]
    fn a_profile_that_turns_on_a_sidecar_no_layer_defines_is_a_refusal() {
        let mut layers = no_layers();
        layers.user =
            UserConfig::from_toml_str("[profiles.p]\ndecisions = { laya = true }\n").unwrap();
        layers.profile_override = Some("p".to_string());

        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::UndefinedLaya { profile: "p".to_string() }
        );
    }

    #[test]
    fn a_sidecar_nobody_turned_on_is_not_a_gap_to_be_reported() {
        // The same shape as an MCP server nobody enabled: a definition says how
        // to reach something, not that this run reaches it.
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            "[decisions.laya]\nurl = \"http://127.0.0.1:8000\"\n\n\
             [profiles.p]\ndecisions = { laya = false }\n",
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());

        let resolved = resolve(layers).unwrap();
        assert_eq!(resolved.laya, None);
        assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);
    }

    #[test]
    fn a_sidecar_threshold_outside_the_unit_interval_is_a_refusal() {
        let mut layers = no_layers();
        layers.user = UserConfig::from_toml_str(
            "[decisions.laya]\nurl = \"http://127.0.0.1:8000\"\nthreshold = 1.5\n",
        )
        .unwrap();

        assert_eq!(
            resolve(layers).unwrap_err(),
            ConfigError::InvalidLaya {
                layer: "user config".to_string(),
                detail: "threshold 1.5 is outside 0..=1".to_string(),
            }
        );
    }

    #[test]
    fn the_operators_sidecar_replaces_the_users_and_says_so() {
        let mut layers = no_layers();
        layers.managed =
            ManagedPolicy::from_toml_str("[decisions.laya]\nurl = \"http://127.0.0.1:9000\"\n")
                .unwrap();
        layers.user =
            UserConfig::from_toml_str("[decisions.laya]\nurl = \"http://127.0.0.1:8000\"\n")
                .unwrap();

        let resolved = resolve(layers).unwrap();
        assert_eq!(
            resolved.laya.map(|laya| laya.url),
            Some("http://127.0.0.1:9000".to_string()),
            "a user file does not get to point the operator's sidecar elsewhere"
        );
        assert!(
            resolved.notes.iter().any(|note| note.contains("managed policy")),
            "{:?}",
            resolved.notes
        );
    }

    #[test]
    fn the_snapshot_records_which_decision_backends_the_run_attached() {
        let mut layers = no_layers();
        layers.user =
            UserConfig::from_toml_str("[decisions.laya]\nurl = \"http://127.0.0.1:8231\"\n")
                .unwrap();

        let resolved = resolve(layers).unwrap();
        let snapshot = resolved.into_snapshot(route());
        assert_eq!(snapshot.decision_backends, vec!["laya:http://127.0.0.1:8231".to_string()]);
    }

    #[test]
    fn a_wall_clock_timeout_is_a_duration_or_a_refusal() {
        let mut layers = no_layers();
        layers.user =
            UserConfig::from_toml_str("[profiles.p]\nbudget = { wall_clock_timeout = \"30m\" }\n")
                .unwrap();
        layers.profile_override = Some("p".to_string());
        assert_eq!(resolve(layers).unwrap().budget.wall_clock_ms, Some(1_800_000));

        let mut bad = no_layers();
        bad.user =
            UserConfig::from_toml_str("[profiles.p]\nbudget = { wall_clock_timeout = \"soon\" }\n")
                .unwrap();
        bad.profile_override = Some("p".to_string());
        assert!(matches!(resolve(bad).unwrap_err(), ConfigError::Unsupported { .. }));
    }

    #[test]
    fn a_budget_of_zero_is_refused_wherever_it_is_written() {
        // The same judgement the MCP and subagent timeouts make: zero is not a
        // tight limit, it is a run that never starts (CFG-4).
        for budget in [
            "budget = { max_steps = 0 }",
            "budget = { max_tool_calls = 0 }",
            "budget = { wall_clock_timeout = \"0s\" }",
        ] {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!("[profiles.p]\n{budget}\n")).unwrap();
            layers.profile_override = Some("p".to_string());
            match resolve(layers).unwrap_err() {
                ConfigError::InvalidBudget { layer, detail } => {
                    assert_eq!(layer, "budget");
                    assert!(detail.contains("zero"), "{detail} did not say why");
                }
                other => panic!("expected a refusal for {budget}, got {other}"),
            }
        }

        // A positive limit is still accepted, and still wins over the default.
        let mut ok = no_layers();
        ok.user = UserConfig::from_toml_str(
            "[profiles.p]\nbudget = { max_steps = 1, max_tool_calls = 1, wall_clock_timeout = \"1s\" }\n",
        )
        .unwrap();
        ok.profile_override = Some("p".to_string());
        let resolved = resolve(ok).unwrap().budget;
        assert_eq!(resolved.max_steps, 1);
        assert_eq!(resolved.max_tool_calls, Some(1));
        assert_eq!(resolved.wall_clock_ms, Some(1_000));
    }

    #[test]
    fn a_context_window_of_zero_is_refused_like_every_other_zero_limit() {
        // "No window" is the field being absent, not zero. A reservation of
        // zero is refused for the same reason: it says "reserve nothing" in the
        // one spelling that reads like a setting.
        for budget in [
            "budget = { max_input_tokens = 0 }",
            "budget = { max_input_tokens = 1000, reserved_output_tokens = 0 }",
        ] {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!("[profiles.p]\n{budget}\n")).unwrap();
            layers.profile_override = Some("p".to_string());
            match resolve(layers).unwrap_err() {
                ConfigError::InvalidBudget { layer, detail } => {
                    assert_eq!(layer, "budget");
                    assert!(detail.contains("zero"), "{detail} did not say why");
                }
                other => panic!("expected a refusal for {budget}, got {other}"),
            }
        }
    }

    #[test]
    fn a_reservation_is_checked_against_the_window_after_both_layers_have_spoken() {
        // Neither layer is wrong alone: the project names a window and the
        // profile names a reservation. Checking each layer on its own would
        // accept a pair that leaves nothing for history.
        let split = |reserved: &str| {
            let mut layers = no_layers();
            layers.workspace =
                WorkspaceConfig::from_toml_str("[budget]\nmax_input_tokens = 1000\n").unwrap();
            layers.user = UserConfig::from_toml_str(&format!(
                "[profiles.p]\nbudget = {{ reserved_output_tokens = {reserved} }}\n"
            ))
            .unwrap();
            layers.profile_override = Some("p".to_string());
            resolve(layers)
        };

        match split("1000").unwrap_err() {
            ConfigError::InvalidBudget { detail, .. } => {
                assert!(detail.contains("not smaller"), "{detail} did not say why");
            }
            other => panic!("a window of 1000 with 1000 reserved must be refused, got {other}"),
        }
        assert!(split("1001").is_err(), "a reservation larger than the window is refused too");
        let accepted = split("999").unwrap().budget;
        assert_eq!(accepted.max_input_tokens, Some(1000));
        assert_eq!(accepted.reserved_output_tokens, Some(999));
    }

    #[test]
    fn the_profile_window_overrides_the_projects() {
        let mut layers = no_layers();
        layers.workspace = WorkspaceConfig::from_toml_str(
            "[budget]\nmax_input_tokens = 1000\nreserved_output_tokens = 100\n",
        )
        .unwrap();
        layers.user = UserConfig::from_toml_str(
            "[profiles.p]\nbudget = { max_input_tokens = 500, reserved_output_tokens = 50 }\n",
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());
        let budget = resolve(layers).unwrap().budget;
        assert_eq!(budget.max_input_tokens, Some(500));
        assert_eq!(budget.reserved_output_tokens, Some(50));
    }

    #[test]
    fn a_compaction_trigger_outside_the_window_is_refused() {
        // 0 and 100 are not thresholds: the first would compact the first
        // message of every turn, the second only after the trimmer has already
        // had to run. Both are configuration mistakes, not settings.
        for percent in [0, 100, 150] {
            let mut layers = no_layers();
            layers.user = UserConfig::from_toml_str(&format!(
                "[profiles.p]\nbudget = {{ max_input_tokens = 1000, \
                 compact_at_percent = {percent} }}\n"
            ))
            .unwrap();
            layers.profile_override = Some("p".to_string());
            match resolve(layers).unwrap_err() {
                ConfigError::InvalidBudget { layer, detail } => {
                    assert_eq!(layer, "budget");
                    assert!(detail.contains("1..=99"), "{detail} did not say the range");
                }
                other => panic!("expected a refusal for {percent}%, got {other}"),
            }
        }
    }

    #[test]
    fn a_compaction_trigger_without_a_window_is_refused() {
        // A share of nothing is not a threshold. Accepting it would leave the
        // field set in the run record while no window exists to take a share of.
        let mut layers = no_layers();
        layers.user =
            UserConfig::from_toml_str("[profiles.p]\nbudget = { compact_at_percent = 85 }\n")
                .unwrap();
        layers.profile_override = Some("p".to_string());
        match resolve(layers).unwrap_err() {
            ConfigError::InvalidBudget { detail, .. } => {
                assert!(
                    detail.contains("max_input_tokens"),
                    "{detail} did not name the missing window"
                );
            }
            other => panic!("expected a refusal without a window, got {other}"),
        }
    }

    #[test]
    fn a_compaction_trigger_is_carried_and_the_profile_wins() {
        let mut layers = no_layers();
        layers.workspace = WorkspaceConfig::from_toml_str(
            "[budget]\nmax_input_tokens = 1000\ncompact_at_percent = 70\n",
        )
        .unwrap();
        assert_eq!(resolve(layers).unwrap().budget.compact_at_percent, Some(70));

        let mut layers = no_layers();
        layers.workspace = WorkspaceConfig::from_toml_str(
            "[budget]\nmax_input_tokens = 1000\ncompact_at_percent = 70\n",
        )
        .unwrap();
        layers.user = UserConfig::from_toml_str(
            "[profiles.p]\nbudget = { max_input_tokens = 500, compact_at_percent = 90 }\n",
        )
        .unwrap();
        layers.profile_override = Some("p".to_string());
        let budget = resolve(layers).unwrap().budget;
        assert_eq!(budget.max_input_tokens, Some(500));
        assert_eq!(budget.compact_at_percent, Some(90));
    }

    #[test]
    fn a_snapshot_written_before_windows_existed_still_reads_back() {
        // The rollback case: a `profile/resolved` record on disk was written by
        // a build that had no window fields. It must read back as "no window",
        // not as a parse failure that makes the whole session unreadable.
        let old = serde_json::json!({
            "max_steps": 8,
            "max_tool_calls": null,
            "wall_clock_ms": null,
        });
        let budget: agent_protocol::profile::RunBudget =
            serde_json::from_value(old).expect("an old budget reads back");
        assert_eq!(budget.max_input_tokens, None);
        assert_eq!(budget.reserved_output_tokens, None);
        assert_eq!(budget, agent_protocol::profile::RunBudget::default());
    }

    #[test]
    fn an_unset_window_is_not_written_so_the_config_hash_does_not_move() {
        // A run that sets no window must serialize exactly as one from before
        // the fields existed; otherwise every existing `config_hash` changes
        // the moment this build runs, and profiles stop comparing equal.
        let default = serde_json::to_value(agent_protocol::profile::RunBudget::default()).unwrap();
        assert_eq!(
            default,
            serde_json::json!({
                "max_steps": 8,
                "max_tool_calls": null,
                "wall_clock_ms": null,
            })
        );
    }
}
