//! The configuration layers, as they are written on disk.
//!
//! Three files, three authorities:
//!
//! ```text
//! managed policy   what the operator refuses, absolutely
//! user config      what is authorized, and the profiles that compose it
//! workspace config what this project narrows
//! ```
//!
//! They are separate types rather than one type with optional fields, because
//! the difference between them is the point. A project directory that could
//! write `capabilities` or `profiles` would be a project directory that could
//! authorize itself, and `#[serde(deny_unknown_fields)]` is what makes that a
//! parse error instead of a silent no-op.
//!
//! Nothing here reads a file. Parsing is `from_toml_str`, and the layer that
//! knows where files live is the app binary — the rule that external I/O
//! belongs at the edge is the same rule that keeps this crate testable without
//! a filesystem.

use std::collections::BTreeMap;

use crate::{ConfigError, Result};
use agent_protocol::versioning::{self, Version, VersionError, formats};
use serde::Deserialize;

/// The system layer: what no user, project, profile, or approval can enable.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedPolicy {
    /// Capabilities refused absolutely.
    #[serde(default)]
    pub deny: Vec<String>,
    /// MCP servers the operator defines.
    ///
    /// Here as well as in the user layer because a server entry names a program
    /// to run: the administrator is the one authority that can pin a server
    /// against a user redefining the same name.
    pub mcp: Option<McpSpec>,
    /// Subagent providers the operator defines.
    ///
    /// Here for the reason the servers are: an entry names a program to run, and
    /// a delegated child is a process the parent did not otherwise have to
    /// authorize. The administrator is the one authority that can pin one
    /// against a user redefining the same name.
    pub subagents: Option<BTreeMap<String, SubagentSpec>>,
    /// Decision backends the operator defines.
    ///
    /// Here for the reason the servers are: an entry names an endpoint the
    /// conversation's restricted state is sent to, and the administrator is the
    /// one authority that can pin it against a user pointing the same name
    /// elsewhere.
    pub decisions: Option<DecisionsSpec>,
}

impl ManagedPolicy {
    pub fn from_toml_str(text: &str) -> Result<Self> {
        parse("managed policy", text)
    }
}

/// The user layer: what is authorized, and the scenarios that compose it.
///
/// `capabilities` is the ceiling. Everything below — a profile, a project, a
/// command line — narrows it and cannot add to it, which is why the field is
/// here and not in a profile.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    /// Capabilities the user authorizes, by name (e.g. `fs.read`).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Plugins the user trusts, as `id` or `id@version`.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// Scenario compositions, by name.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// MCP servers this user defines.
    pub mcp: Option<McpSpec>,
    /// Subagent providers this user defines.
    pub subagents: Option<BTreeMap<String, SubagentSpec>>,
    /// Decision backends this user defines.
    pub decisions: Option<DecisionsSpec>,
    /// Long-term memory, as this user turns it on.
    ///
    /// Here rather than in a profile or a project for the same reason
    /// `capabilities` is: the store is one file per data directory that every
    /// session in it reads, so turning it on is a decision about the machine,
    /// not about one scenario — and a repository must not be able to make its
    /// own memories appear.
    pub memory: Option<MemorySpec>,
}

/// The `[memory]` section: whether runs on this machine offer the model the
/// long-term memory tools.
///
/// It is not a capability. `memory.write` is what a call is gated by; this says
/// whether the tools are on the surface at all. Off by default: a store that
/// outlives every session is something a person turns on, not something a run
/// assumes on their behalf.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySpec {
    /// Whether `memory.recall`, `memory.remember` and `memory.forget` are
    /// offered to the model. The operator's own `agent memory …` verbs are
    /// always available: a person typing a command is the authority here.
    #[serde(default)]
    pub enabled: bool,
}

impl UserConfig {
    pub fn from_toml_str(text: &str) -> Result<Self> {
        parse("user config", text)
    }

    /// Every trusted plugin, as `(id, version)`; a bare id is trusted at `*`.
    pub fn trusted_plugins(&self) -> Vec<(String, String)> {
        self.plugins.iter().map(|entry| split_trust(entry)).collect()
    }

    /// Whether the user layer defines a profile by this name.
    pub fn defines(&self, profile: &str) -> bool {
        self.profiles.contains_key(profile)
    }
}

/// The project layer: it may choose a profile and narrow, never widen.
///
/// It has no `capabilities`, no `profiles`, no `mcp`, and no `subagents`, and
/// `deny_unknown_fields` makes writing any of them a refusal rather than a rule
/// that quietly does nothing. The last two are the sharpest: such an entry names
/// a program to run, so a repository that could write one could run code of its
/// choosing in every session opened inside it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// The profile to compose. Must be one the user layer defines.
    pub profile: Option<String>,
    /// What this project narrows.
    pub tools: Option<WorkspaceTools>,
    /// Limits for this project. A budget is a normal setting, not a
    /// permission: it merges the way normal settings merge, so a
    /// profile composed on top of this layer still has the last word.
    pub budget: Option<BudgetSpec>,
}

impl WorkspaceConfig {
    pub fn from_toml_str(text: &str) -> Result<Self> {
        parse("workspace config", text)
    }
}

/// The half of `[tools]` a project is allowed to write: refusals only.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTools {
    /// Capabilities this project refuses, whatever the profile allows.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// A scenario composition.
///
/// Not a prompt: what a profile composes is which model serves the turn, what
/// the tools are allowed to do, how much budget the run has, and which plugins
/// are enabled.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The profile this one is composed on top of. Fields it names override
    /// the parent's, one field at a time.
    pub extends: Option<String>,
    pub model: Option<ModelSpec>,
    pub tools: Option<ToolsSpec>,
    pub policy: Option<PolicySpec>,
    pub budget: Option<BudgetSpec>,
    pub plugins: Option<PluginsSpec>,
    /// Which decision backends this scenario runs with.
    pub decisions: Option<ProfileDecisions>,
}

/// The decision backends a scenario turns on or off.
///
/// A scenario may only *turn one off*. A profile that could name an endpoint
/// would be a configuration file pointing the conversation's restricted state
/// at somewhere of its choosing, which is the same hazard as a project naming
/// an MCP server command — so the endpoints live in the layers that authorize,
/// and this says which of those authorized backends this scenario uses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDecisions {
    /// Whether this scenario uses the Laya sidecar. `None` leaves the
    /// definition's answer, which is "yes, since a layer defined it".
    pub laya: Option<bool>,
}

/// The decision backends a layer defines.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionsSpec {
    /// The local fast-decision sidecar.
    pub laya: Option<LayaSpec>,
}

/// The Laya sidecar, as a layer spells it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayaSpec {
    /// Where the sidecar is reachable. Must be loopback; the adapter refuses
    /// anything else when it is built, because a decision sidecar is a local
    /// artifact and a remote one would be a side channel.
    pub url: String,
    /// The checkpoint to name. Absent lets the server route.
    pub model: Option<String>,
    /// The `answer_confidence` below which an answer reads as no opinion.
    pub threshold: Option<f64>,
}

/// Which model serves which tier.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub main: Option<String>,
    pub fast: Option<String>,
    pub thinking: Option<String>,
}

/// What a profile's tools may do.
///
/// The names are capabilities, not tool names: `fs.read`, `process.spawn`,
/// `network.request`. What a *tool* needs is declared by the tool — a profile
/// decides what may be granted, and the gateway asks about the specific call.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsSpec {
    /// The whole of what this profile permits, when it says anything at all.
    ///
    /// A profile that lists capabilities gets exactly those and nothing else:
    /// an unlisted capability falls through to the engine's fail-closed deny,
    /// which is what makes this a whitelist rather than a longer allow list.
    /// *Listing* is the operative word — this list is what decides whether the
    /// built-in rules still apply at all.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Capabilities refused on top of whatever the profile permits.
    ///
    /// That is the built-in defaults when this profile lists no `allow` of its
    /// own: writing only a `deny` narrows the defaults, it does not replace
    /// them. The alternative reading — a `deny`-only section meaning "permit
    /// nothing else" — turns the most cautious thing a user can write into the
    /// widest refusal (CONTRACT.md §7 row 3, YA-10).
    #[serde(default)]
    pub deny: Vec<String>,
}

/// A small set of named knobs, expanded to the same capability rules
/// `[tools]` writes directly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    /// What spawning a process requires (`process.spawn`).
    pub shell: Option<Disposition>,
    /// What reaching the network requires (`network.request`).
    pub network: Option<Disposition>,
    /// Whether writing inside the workspace is permitted (`fs.write`).
    pub workspace_write: Option<bool>,
}

/// A rule's disposition, as configuration spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Allow,
    Ask,
    Deny,
}

/// What a run may spend.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSpec {
    pub max_steps: Option<u32>,
    pub max_tool_calls: Option<u32>,
    /// A duration such as `"30m"`, `"90s"`, or `"2h"`.
    pub wall_clock_timeout: Option<String>,
    /// The most estimated input tokens one request may carry. Omit to send the
    /// settled history whole.
    pub max_input_tokens: Option<u32>,
    /// How much of `max_input_tokens` to reserve for the answer. Defaults to
    /// zero; must be smaller than `max_input_tokens` when both are set.
    pub reserved_output_tokens: Option<u32>,
    /// The most completion tokens one request may ask the provider for. Omit
    /// for the shipped default (8,000); independent of the window, and it is
    /// what the request's `max_tokens` carries.
    pub max_output_tokens: Option<u32>,
    /// The share of the usable window at which older history is summarized
    /// before a request is sent. Omit to keep the trimmer as the only
    /// protection. Requires `max_input_tokens`; 1..=99.
    pub compact_at_percent: Option<u32>,
}

/// Which plugins a scenario turns on.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsSpec {
    /// Plugins this profile needs. Each must be one the user layer trusts.
    #[serde(default)]
    pub enable: Vec<String>,
    /// Plugins this profile turns off even though the user trusts them.
    #[serde(default)]
    pub disable: Vec<String>,
}

/// The MCP servers a layer defines.
///
/// Keyed by server name, which is also the half of a plugin id that follows
/// `mcp.`: `[mcp.servers.files]` is the server a profile enables as `mcp.files`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSpec {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServer>,
}

/// One MCP server, as a layer spells it.
///
/// Two transports are expressible and exactly one must be named: a stdio server
/// by `command` (with `args`, `env`, `cwd`), or a Streamable HTTP server by
/// `url` (with `headers`). Naming both, naming neither, or writing a field that
/// belongs to the transport that was not chosen is a refusal, not a setting that
/// quietly does nothing — [`crate::resolve`] is where the shape is checked, so
/// that the error can name the server and the layer that wrote it.
///
/// Every field is optional here because the two shapes share one type; which of
/// them a server is, is decided by the check rather than by the parser.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// Capabilities every tool this server offers requires.
    ///
    /// MCP tools declare none of their own, so the server entry is the only
    /// place the claim can come from — and every tool the server lists inherits
    /// it, which is what makes "this server may do X" answerable before any of
    /// its tools has been seen.
    #[serde(default)]
    pub requires: Vec<String>,
    /// How long one call under this server may take, e.g. `"90s"`.
    pub timeout: Option<String>,
    /// What this run does with a tool whose declaration it cannot enforce:
    /// `"strict"` (the default) refuses the server, `"warn"` registers it and
    /// says so. See [`crate::SchemaPolicy`].
    pub schema: Option<String>,

    /// The program to run, for a stdio server.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,

    /// The endpoint to reach, for a Streamable HTTP server.
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// One subagent provider, as a layer spells it.
///
/// Keyed under `subagents` by provider name, which is also the half of a plugin
/// id that follows `subagent.`: `[subagents.fake]` is the provider a profile
/// enables as `subagent.fake`. There is no level between the section and the
/// name — unlike `[mcp.servers.<name>]` — because the entry is the whole of what
/// there is to say about it.
///
/// Two shapes, and `kind` says which. Until CONTRACT §7 #133 this comment said
/// a child was *always* a program this build starts, so a field for any other
/// way of reaching one was not expressible. That was true of the only backend
/// there was, and it made the model's delegation surface unreachable in
/// practice: no program in this build speaks the child wire, and an external
/// program is never told which forked session it is meant to continue. An
/// `in-process` child is a turn this build runs itself in that session, so it is
/// a shape the entry has to be able to name.
///
/// Which fields belong to which shape is a question with one wrong answer —
/// silently ignoring a field — so [`crate::resolve`] refuses the mixtures rather
/// than dropping half of an entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentSpec {
    /// Capabilities a delegation through this provider requires.
    ///
    /// A child does not inherit the parent's permissions, and what a
    /// delegation asks of the parent's policy is the delegation itself: this is
    /// the claim that question is answered against, and the only place it can
    /// be stated, because a delegated child's own tool calls are governed where
    /// this build does not — another process, or a surface restricted before the
    /// child sees it.
    #[serde(default)]
    pub requires: Vec<String>,
    /// The longest a child may live, e.g. `"5m"`.
    ///
    /// A ceiling, not a default: a request may ask for less and never for more,
    /// which is what makes the party who wrote this file the party who decides
    /// what a child may cost. Absent leaves the provider's own default.
    pub timeout: Option<String>,

    /// How this build reaches a child: `"stdio"` (absent), `"in-process"`, or
    /// `"self"`.
    ///
    /// Absent means `stdio`, because every definition written before this field
    /// existed was one, and a configuration that never named a kind must keep
    /// resolving to the thing it already meant.
    pub kind: Option<String>,

    /// The program to run. Required for `stdio`, and refused for the kinds that
    /// run this binary: a program named beside a provider that does not start
    /// one is an entry that says two conflicting things.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,

    /// Where a `self` child may write, as workspace-relative directory
    /// prefixes.
    ///
    /// Naming a scope is what makes an entry a **work** provider rather than a
    /// **read-and-answer** one: the child's surface gains the write tools
    /// inside these directories and nothing outside them. Without it the child
    /// is read-only, which is the cheaper and safer default — a delegation that
    /// can write has to say where.
    ///
    /// Refused for the other kinds: a `stdio` child's writes happen in a
    /// program this build builds no boundary for, and an `in-process` child has
    /// no write surface at all.
    #[serde(default)]
    pub scope: Vec<String>,
}

/// Parse one layer, naming it in the error so the message says which file.
///
/// The version is read from the document itself before the typed parse, because
/// a future version's file can otherwise fail as an "unknown field" or — worse,
/// if it added no field — succeed silently. Reading it first is what lets the
/// refusal say "version", which is the whole of YA-67.
///
/// `schema_version` is removed from the value before `try_into` so the layer
/// structs keep their `deny_unknown_fields` answer to every *other* unknown key
/// without gaining a field that exists only to be discarded. Absent means the
/// current version: these files predate the field, and a record written by a
/// build that had no version to stamp is a record of today's shape, not a
/// failure.
fn parse<T: serde::de::DeserializeOwned>(layer: &str, text: &str) -> Result<T> {
    let mut value: toml::Value = toml::from_str(text)
        .map_err(|e| ConfigError::Parse { layer: layer.to_string(), message: e.to_string() })?;

    let version = match value.as_table_mut().and_then(|table| table.remove("schema_version")) {
        None => formats::CONFIG,
        Some(toml::Value::String(text)) => Version::parse(&text)
            .map_err(|source| ConfigError::Version { layer: layer.to_string(), source })?,
        Some(other) => {
            return Err(ConfigError::Version {
                layer: layer.to_string(),
                source: VersionError::Malformed {
                    found: format!("{other:?}"),
                    detail: "`schema_version` must be a string such as \"1\"".to_string(),
                },
            });
        }
    };
    versioning::check(&version, &formats::CONFIG)
        .map_err(|source| ConfigError::Version { layer: layer.to_string(), source })?;

    value
        .try_into()
        .map_err(|e| ConfigError::Parse { layer: layer.to_string(), message: e.to_string() })
}

/// Split `id@version`; a bare id is an unpinned trust.
fn split_trust(entry: &str) -> (String, String) {
    match entry.split_once('@') {
        Some((id, version)) => (id.to_string(), version.to_string()),
        None => (entry.to_string(), "*".to_string()),
    }
}

/// Parse a duration such as `"30m"`.
///
/// Only what the design's example writes: a whole number and one unit. A
/// richer parser would accept `"1h30m"` and a calendar's worth of edge cases,
/// and the budget does not need them.
///
/// `layer` names the section the value came from and travels into the error.
/// It is a parameter because this parser is used by budgets, MCP server
/// timeouts, and subagent ceilings: hardcoding one section made an invalid
/// timeout in `[mcp.servers.files]` report itself as a budget error.
pub fn parse_duration_ms(layer: &str, text: &str) -> Result<u64> {
    let trimmed = text.trim();
    let (digits, unit) =
        trimmed.split_at(trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(trimmed.len()));
    let amount: u64 = digits.parse().map_err(|_| ConfigError::Unsupported {
        layer: layer.to_string(),
        detail: format!("'{text}' is not a duration"),
    })?;
    let scale = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        other => {
            return Err(ConfigError::Unsupported {
                layer: layer.to_string(),
                detail: format!("'{other}' is not a duration unit; use ms, s, m, or h"),
            });
        }
    };
    amount.checked_mul(scale).ok_or_else(|| ConfigError::Unsupported {
        layer: layer.to_string(),
        detail: format!("'{text}' is too long a duration"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_off_unless_the_user_layer_says_otherwise() {
        // Off is the default, and the section is a user-layer fact: a project
        // config that wrote it is refused as an unknown key, which is what
        // `deny_unknown_fields` on `WorkspaceConfig` already does for every
        // grant it does not own.
        let absent = UserConfig::from_toml_str("").expect("an empty configuration");
        assert!(absent.memory.is_none());

        let on = UserConfig::from_toml_str("[memory]\nenabled = true\n").expect("the section");
        assert_eq!(on.memory.map(|memory| memory.enabled), Some(true));

        let refused = WorkspaceConfig::from_toml_str("[memory]\nenabled = true\n");
        assert!(refused.is_err(), "a project may not turn the store on");
    }

    #[test]
    fn a_memory_section_with_a_typo_is_a_refusal_not_a_default() {
        let error = UserConfig::from_toml_str("[memory]\nEnable = true\n")
            .expect_err("a misspelled key must not read as off");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn a_user_config_declares_capabilities_and_profiles() {
        let config = UserConfig::from_toml_str(
            r#"
capabilities = ["fs.read", "fs.write"]
plugins = ["mcp.foo@1.2.0", "mcp.bar"]

[profiles.coding]
extends = "base"

[profiles.coding.model]
main = "deepseek-chat"

[profiles.coding.tools]
allow = ["fs.read"]

[profiles.coding.policy]
shell = "ask"
workspace_write = true

[profiles.coding.budget]
max_steps = 32
wall_clock_timeout = "30m"

[profiles.coding.plugins]
enable = ["mcp.foo"]
"#,
        )
        .unwrap();

        assert!(config.defines("coding"));
        assert_eq!(config.profiles["coding"].extends.as_deref(), Some("base"));
        let profile = &config.profiles["coding"];
        assert_eq!(profile.model.as_ref().unwrap().main.as_deref(), Some("deepseek-chat"));
        assert_eq!(profile.tools.as_ref().unwrap().allow, vec!["fs.read"]);
        assert_eq!(profile.policy.as_ref().unwrap().shell, Some(Disposition::Ask));
        assert_eq!(profile.budget.as_ref().unwrap().max_steps, Some(32));
        assert_eq!(profile.plugins.as_ref().unwrap().enable, vec!["mcp.foo"]);

        // A pinned trust keeps its version; a bare id is trusted at any of them.
        assert_eq!(
            config.trusted_plugins(),
            vec![
                ("mcp.foo".to_string(), "1.2.0".to_string()),
                ("mcp.bar".to_string(), "*".to_string()),
            ]
        );
    }

    #[test]
    fn a_project_may_not_authorize_itself() {
        // The trust boundary, enforced by the shape of the type rather than by
        // a check somebody has to remember to write. Each of these is a project
        // config trying to give itself something it may not have.
        for text in [
            r#"capabilities = ["git.push"]"#,
            "[profiles.mine]\n",
            "[tools]\nallow = [\"fs.write\"]\n",
            // The sharpest of the three: a project that could name a server
            // command could run a program of its choosing in every session
            // opened in it.
            "[mcp.servers.evil]\ncommand = \"anything\"\n",
            "[mcp.servers.evil]\nurl = \"https://example.test/mcp\"\n",
            // A subagent provider is the same hazard again: it names a program
            // the delegation runs.
            "[subagents.evil]\ncommand = \"anything\"\n",
            // And the same for a decision backend: an endpoint the state is
            // sent to is no more a project's to name than a program to run.
            "[decisions.laya]\nurl = \"http://127.0.0.1:8000\"\n",
        ] {
            let error = WorkspaceConfig::from_toml_str(text).unwrap_err();
            assert!(
                matches!(error, ConfigError::Parse { .. }),
                "{text} should be refused, got {error}"
            );
        }
    }

    #[test]
    fn a_project_may_narrow() {
        let config = WorkspaceConfig::from_toml_str(
            r#"
profile = "coding"

[tools]
deny = ["process.spawn"]

[budget]
max_steps = 4
"#,
        )
        .unwrap();
        assert_eq!(config.profile.as_deref(), Some("coding"));
        assert_eq!(config.tools.unwrap().deny, vec!["process.spawn"]);
        assert_eq!(config.budget.unwrap().max_steps, Some(4));
    }

    #[test]
    fn a_typo_is_a_refusal_rather_than_a_line_that_does_nothing() {
        let error = UserConfig::from_toml_str("capabilites = [\"fs.read\"]\n").unwrap_err();
        match error {
            ConfigError::Parse { layer, message } => {
                assert_eq!(layer, "user config");
                assert!(message.contains("capabilites"), "{message}");
            }
            other => panic!("expected a parse error, got {other}"),
        }
    }

    #[test]
    fn a_user_config_declares_subagent_providers() {
        let config = UserConfig::from_toml_str(
            r#"
plugins = ["subagent.fake"]

[subagents.fake]
command = "fake-subagent"
args = ["--mode", "result"]
timeout = "5m"
requires = ["process.spawn"]
"#,
        )
        .unwrap();

        let fake = &config.subagents.as_ref().unwrap()["fake"];
        assert_eq!(
            fake.kind.as_deref(),
            None,
            "an entry that names no kind is a program this build starts"
        );
        assert_eq!(fake.command.as_deref(), Some("fake-subagent"));
        assert_eq!(fake.args, vec!["--mode", "result"]);
        assert_eq!(fake.timeout.as_deref(), Some("5m"));
        assert_eq!(fake.requires, vec!["process.spawn"]);

        // The same rule the rest of the file follows: a field nobody reads is a
        // field somebody meant.
        let typo =
            UserConfig::from_toml_str("[subagents.fake]\ncommand = \"fake\"\ntimeout_ms = 5000\n");
        assert!(matches!(typo, Err(ConfigError::Parse { .. })), "{typo:?}");
    }

    #[test]
    fn durations_are_a_number_and_one_unit() {
        assert_eq!(parse_duration_ms("budget", "30m").unwrap(), 1_800_000);
        assert_eq!(parse_duration_ms("budget", "90s").unwrap(), 90_000);
        assert_eq!(parse_duration_ms("budget", "2h").unwrap(), 7_200_000);
        assert_eq!(parse_duration_ms("budget", "500ms").unwrap(), 500);
        assert!(parse_duration_ms("budget", "soon").is_err());
        assert!(parse_duration_ms("budget", "30 minutes").is_err());
    }

    #[test]
    fn every_layer_reads_the_current_schema_version() {
        // `1` and `1.0.0` are one version; both spellings are accepted, and the
        // key does not trip `deny_unknown_fields` because it is removed before
        // the typed parse.
        let user =
            UserConfig::from_toml_str("schema_version = \"1\"\ncapabilities = [\"fs.read\"]\n")
                .expect("the current version is readable");
        assert_eq!(user.capabilities, vec!["fs.read"]);

        let workspace = WorkspaceConfig::from_toml_str("schema_version = \"1.0.0\"\n")
            .expect("a spelled-out current version is readable");
        assert!(workspace.profile.is_none());

        let managed = ManagedPolicy::from_toml_str("schema_version = \"1\"\ndeny = [\"net\"]\n")
            .expect("managed policy reads the same way");
        assert_eq!(managed.deny, vec!["net"]);
    }

    #[test]
    fn a_config_from_a_future_schema_is_refused_as_a_version_problem() {
        let error = UserConfig::from_toml_str("schema_version = \"99\"\ncapabilities = []\n")
            .expect_err("a future config is not this build's config");
        match error {
            ConfigError::Version { layer, source } => {
                assert_eq!(layer, "user config");
                assert!(
                    matches!(source, VersionError::Future { .. }),
                    "the refusal names a version, not a syntax: {source}"
                );
                assert!(source.to_string().contains("99"), "{source}");
            }
            other => panic!("wrong error kind: {other}"),
        }
    }

    #[test]
    fn a_config_from_an_unmigratable_older_schema_is_refused_explicitly() {
        let error = WorkspaceConfig::from_toml_str("schema_version = \"0.9\"\n")
            .expect_err("there is no migration from 0.9");
        assert!(
            matches!(error, ConfigError::Version { source: VersionError::NoMigration { .. }, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_schema_version_that_is_not_a_string_is_refused_rather_than_coerced() {
        let error = ManagedPolicy::from_toml_str("schema_version = 1\n")
            .expect_err("TOML integers and version strings are different facts");
        assert!(
            matches!(error, ConfigError::Version { source: VersionError::Malformed { .. }, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_config_without_a_version_is_the_current_shape() {
        // Every file written before the field existed is this case, and must
        // keep parsing; "absent" is the compatibility decision, not a gap.
        let config = UserConfig::from_toml_str("capabilities = [\"fs.read\"]\n").unwrap();
        assert_eq!(config.capabilities, vec!["fs.read"]);
    }
}
