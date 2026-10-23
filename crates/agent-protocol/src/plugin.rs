//! Plugin manifest, lifecycle vocabulary, and the payload of a plugin
//! lifecycle event.
//!
//! Everything here is a serializable value: what a plugin *declares*, and the
//! durable record of what happened to it. The runtime half — the `ToolProvider`
//! a plugin actually contributes, and the registry that holds it — is not
//! protocol and lives in `agent_kernel::plugin`. A manifest is safe to log,
//! hash, and compare; a live handle is none of those things.

use crate::ids::PluginId;
use crate::permissions::Capability;
use crate::tool::ToolSource;
use crate::versioning::Version;
use serde::{Deserialize, Serialize};

/// What a plugin asks for and offers, before anything is constructed.
///
/// The manifest is the *claim*; the registry is what decides whether the claim
/// is honored. A manifest is authored by the plugin's packager and is therefore
/// untrusted input — nothing here is believed on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// The manifest shape's schema version (YA-21).
    ///
    /// Absent means current: manifests were authored before this field existed,
    /// and their shape is the one this build reads. The registry checks this
    /// before anything else, so a manifest from a newer build is refused as a
    /// version mismatch rather than half-read as a set of unknown fields.
    #[serde(default = "crate::versioning::current_plugin_manifest")]
    pub schema_version: Version,
    pub id: PluginId,
    /// The plugin's own version, distinct from the API version it targets.
    pub version: String,
    /// The plugin API version this plugin was written against. A
    /// mismatch is a compatibility question, answered by the loader, not here.
    pub plugin_api: String,
    pub capabilities: PluginCapabilities,
    pub contributions: PluginContributions,
    /// Name of the config schema this plugin's settings are validated against,
    /// if it takes settings at all.
    pub config_schema: Option<String>,
}

/// The capabilities a plugin requires to run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCapabilities {
    /// Every capability the plugin needs. Registration fails unless all of
    /// these are authorized — the whole list, not a best-effort subset, because
    /// a plugin running without a capability it declared is a plugin whose
    /// behavior nobody described.
    pub requires: Vec<Capability>,
}

/// What kinds of thing a plugin contributes.
///
/// Declared as kinds rather than as live values: this is the part that is
/// serializable, so it can be logged next to the plugin's capabilities and
/// compared across runs. **That record is this type's whole job.** Nothing here
/// is a switch the runtime reads to decide what to do — what the runtime
/// actually holds is `agent_kernel::plugin::PluginRuntime`, a separate type
/// because a trait object cannot go into a payload.
///
/// Which half of the declaration is *honored* today, field by field — the
/// distinction matters, because "the manifest says so" is not yet the same as
/// "it happens":
///
/// * [`Self::tool_source`] — read and used: the registry stamps contributed
///   tools with it (`agent-kernel/src/plugin.rs:320`).
/// * [`Self::tool_provider`], [`Self::observer`], [`Self::decision_backend`] —
///   **recorded and not enforced** (YA-40). A manifest may declare an observer
///   or a decision backend and the run will neither provide nor refuse one; the
///   declaration is kept so that an operator reading the log can see what the
///   plugin claimed. Nothing here is compared against what `register` was
///   handed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginContributions {
    /// Declared-only (YA-40): the runtime's tool surface is whatever
    /// `PluginRuntime::tool_provider` holds, and `register` never compares the
    /// two. A manifest that declares this and hands over nothing still
    /// registers.
    pub tool_provider: bool,
    /// Which source kind the contributed tools report. Constrained to
    /// [`PluginToolSource`] on purpose — see there. This one is read.
    #[serde(default)]
    pub tool_source: PluginToolSource,
    /// Declared-only (YA-40): no run in this build hosts an observer, and
    /// nothing refuses a manifest that declares one.
    pub observer: bool,
    /// Declared-only (YA-40): the decision backends a run attaches come from
    /// configuration (`agent-config`'s `[laya]` section, into
    /// `ResolvedProfileSnapshot::decision_backends`), which is a different
    /// mechanism with a different lifetime — a plugin cannot attach one.
    pub decision_backend: bool,
}

/// The source kind a plugin's tools are stamped with when they are reached
/// through the registry.
///
/// The registry is the authority on provenance: a contributed provider does not
/// get to say where it came from. But "the registry always says `Plugin`" is one
/// rule too strong — it would make [`ToolSource::Mcp`] unreachable, and mangle a
/// distinction the tool model already draws. So the *manifest* names the kind,
/// and the vocabulary here is deliberately short: `Plugin` and `Mcp` are the two
/// ways a tool can arrive through this door. `Builtin` is stamped by the built-in
/// providers themselves; a manifest that wants to look built-in cannot say so,
/// because the word is not in this enum. A skill loader does not exist in this
/// build, so there is no `Skill` source to claim either — if one is ever built,
/// it adds its own source together with the producer that stamps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginToolSource {
    #[default]
    Plugin,
    Mcp,
}

impl From<PluginToolSource> for ToolSource {
    fn from(kind: PluginToolSource) -> Self {
        match kind {
            PluginToolSource::Plugin => ToolSource::Plugin,
            PluginToolSource::Mcp => ToolSource::Mcp,
        }
    }
}

/// Where a plugin is in its lifecycle.
///
/// The full sequence is `Discovered → ManifestValidated → DependencyResolved →
/// CapabilityAuthorized → Constructed → Registered → Active → Degraded / Disabled
/// / Failed → Unregistered`. This build reaches the tail of it: an in-process
/// plugin is handed to the registry already validated and constructed, so a
/// successful registration records `Registered` in one step rather than
/// replaying the stages it passed through, and every later state is a change the
/// registry makes and records itself. The earlier states are declared because
/// they are the vocabulary a loader will use, and inventing a second set of
/// names for them later is how two files come to disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginLifecycleState {
    Discovered,
    ManifestValidated,
    DependencyResolved,
    CapabilityAuthorized,
    Constructed,
    Registered,
    Active,
    Degraded,
    Disabled,
    Failed,
    Unregistered,
}

/// The payload of `plugin/loaded`, `plugin/unloaded`, and `plugin/failed`.
///
/// One shape for all three so a fold reads a plugin's history without three
/// deserialization paths, and so a reader comparing "what was loaded" with
/// "what was unloaded" is comparing like with like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStateChange {
    pub plugin_id: PluginId,
    pub version: String,
    pub plugin_api: String,
    pub state: PluginLifecycleState,
    /// The capabilities in effect for this plugin, sorted. Not "what it asked
    /// for": a plugin whose request was refused is never recorded at all, so at
    /// every state that reaches the log the granted set and the required set are
    /// the same set, and recording both would be recording one fact twice.
    pub capabilities: Vec<Capability>,
    /// The registry generation this change produced or ended. The number orders
    /// plugin changes against each other and is what a frozen `ToolManifest`
    /// can be compared against.
    pub generation: u64,
    /// Why, when the change is not a plain success — a failed plugin's error, or
    /// the cause of an unload. `None` for a registration that simply worked.
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        PluginManifest {
            schema_version: crate::versioning::formats::PLUGIN_MANIFEST,
            id: PluginId::from_string("git-tools"),
            version: "0.3.0".to_string(),
            plugin_api: "1".to_string(),
            capabilities: PluginCapabilities {
                requires: vec![Capability::new(Capability::FS_READ)],
            },
            contributions: PluginContributions {
                tool_provider: true,
                observer: true,
                ..PluginContributions::default()
            },
            config_schema: Some("git-tools.config.v1".to_string()),
        }
    }

    #[test]
    fn a_manifest_round_trips_through_serde() {
        let original = manifest();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: PluginManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_manifest_serializes_its_id_as_a_plain_string() {
        // The id is what config files and event payloads key on, so it has to be
        // the string itself rather than a wrapper object.
        let json = serde_json::to_value(manifest()).unwrap();
        assert_eq!(json["id"], "git-tools");
        assert_eq!(json["capabilities"]["requires"][0], "fs.read");
    }

    #[test]
    fn a_manifest_defaults_to_the_plugin_tool_source() {
        // The plain case — an in-process plugin — needs no field at all, and a
        // contribution payload written before the field existed reads the same
        // way: as a plugin.
        assert_eq!(PluginToolSource::default(), PluginToolSource::Plugin);
        assert_eq!(ToolSource::from(PluginToolSource::default()), ToolSource::Plugin);
        let decoded: PluginContributions = serde_json::from_value(serde_json::json!({
            "tool_provider": true,
            "observer": false,
            "decision_backend": false,
        }))
        .unwrap();
        assert_eq!(decoded.tool_source, PluginToolSource::Plugin);
    }

    #[test]
    fn the_source_kind_maps_to_the_tool_model_and_serializes_snake_case() {
        assert_eq!(ToolSource::from(PluginToolSource::Mcp), ToolSource::Mcp);
        assert_eq!(serde_json::to_string(&PluginToolSource::Mcp).unwrap(), "\"mcp\"");
        assert_eq!(serde_json::to_string(&PluginToolSource::Plugin).unwrap(), "\"plugin\"");
    }

    #[test]
    fn lifecycle_states_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&PluginLifecycleState::CapabilityAuthorized).unwrap(),
            "\"capability_authorized\""
        );
        assert_eq!(
            serde_json::to_string(&PluginLifecycleState::Unregistered).unwrap(),
            "\"unregistered\""
        );
    }

    #[test]
    fn a_state_change_round_trips_with_an_explicit_no_reason() {
        let change = PluginStateChange {
            plugin_id: PluginId::from_string("git-tools"),
            version: "0.3.0".to_string(),
            plugin_api: "1".to_string(),
            state: PluginLifecycleState::Registered,
            capabilities: vec![Capability::new(Capability::FS_READ)],
            generation: 1,
            reason: None,
        };
        let json = serde_json::to_string(&change).unwrap();
        let decoded: PluginStateChange = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, change);
        // A missing reason is `null`, not an absent field: a reader must be able
        // to tell "no reason was given" from "this payload predates the field".
        assert_eq!(serde_json::to_value(&change).unwrap()["reason"], serde_json::Value::Null);
    }
}
