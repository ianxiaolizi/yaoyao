//! PluginRegistry — reversible plugin registration behind a registration-time
//! capability gate.
//!
//! Two questions are asked of a plugin, at two different times, and this module
//! is what keeps them apart:
//!
//!   * *May this plugin run at all?* Asked **once**, at registration, against the
//!     authorized capability set. A plugin that requires anything it was not
//!     granted is refused before its contributions are constructed and before any
//!     event is written.
//!   * *May this particular call proceed?* Asked **per call**, by the
//!     `PolicyEngine` behind `GuardedToolProvider`. Registration is not a
//!     substitute for that check and pre-approves nothing: a plugin holding a
//!     capability for one tool still has every call judged on its own arguments
//!     and path.
//!
//! Registration is reversible. `register` returns a [`PluginHandle`]; disabling it
//! withdraws everything the plugin contributed and records that it did. A plugin
//! is never merely hidden, because "the tool is gone from the surface but the log
//! still says it is registered" is a state no reader can reason about.
//!
//! The registry is the authority on provenance: tools reached through it report
//! [`ToolSource::Plugin`] whatever the contributed provider says about itself. A
//! plugin does not get to describe where it came from.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock, Weak};

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, PluginId, SchemaVersion, SessionId};
use agent_protocol::permissions::{Capability, CapabilitySet};
use agent_protocol::plugin::{PluginLifecycleState, PluginManifest, PluginStateChange};
use chrono::Utc;

/// What a plugin actually contributes at runtime, as opposed to the kinds its
/// manifest declares.
///
/// The manifest's `PluginContributions` is a declaration a reader can log; this is
/// the live value the registry holds. They are separate types because one is
/// serializable and the other is not, and merging them would drag a trait object
/// into a payload meant for the event log.
#[derive(Default)]
pub struct PluginRuntime {
    /// The tools this plugin contributes, if it contributes any. `None` for a
    /// plugin whose manifest declares no tool surface — which is not a
    /// registration failure: not every plugin is a tool plugin.
    pub tool_provider: Option<Arc<dyn ToolProvider>>,
}

/// The registration table and its generation counter.
#[derive(Default)]
struct RegistryState {
    /// How many changes the plugin surface has seen. Every successful
    /// registration and every withdrawal advances it by one; a registration that
    /// could not be recorded does not.
    generation: u64,
    /// Registered plugins by id. Ordered by id, so the tool surface this
    /// produces comes out in the same order every run.
    plugins: BTreeMap<PluginId, RegisteredPlugin>,
}

struct RegisteredPlugin {
    manifest: PluginManifest,
    /// The capabilities in effect, sorted and deduplicated, as recorded in the
    /// lifecycle event.
    capabilities: Vec<Capability>,
    runtime: PluginRuntime,
    generation: u64,
}

/// The plugin registry for one process.
///
/// It holds the shared event store because a registration that is not recorded
/// did not happen: the in-memory table and the log move together or not at all.
pub struct PluginRegistry {
    store: Arc<dyn EventStore>,
    state: RwLock<RegistryState>,
    /// Tool names the run already has, which no plugin may take.
    ///
    /// These are the built-in tools — and the delegation tool when the run has
    /// one — because a plugin offering `fs.read` would be a plugin whose tool
    /// the surface never reaches: the composite routes a call to the first
    /// provider that claims the name, and the built-ins are composed first. The
    /// name would be offered to the model twice and one of the two would be
    /// dead. Refusing the registration is the answer that does not depend on
    /// which order they were composed in.
    reserved_tools: BTreeSet<String>,
}

/// A handle to one registration, from which the plugin can be withdrawn.
///
/// It is a handle, not a shared reference to the plugin: holding one keeps no
/// side effect alive, and dropping it withdraws nothing. Withdrawal is an
/// explicit act with a record, which is the point of having a lifecycle at all.
#[derive(Debug)]
pub struct PluginHandle {
    registry: Weak<PluginRegistry>,
    plugin_id: PluginId,
    /// The session the registration was recorded against. Its withdrawal is
    /// recorded against the same one, so a plugin cannot appear to have been
    /// loaded in one session and unloaded in another.
    session_id: SessionId,
    /// The generation this handle was issued at. A handle whose registration has
    /// already been withdrawn cannot withdraw whatever registration came after
    /// it, because it is not the same registration.
    generation: u64,
}

/// Errors from plugin registration and withdrawal.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error(
        "plugin '{plugin_id}' requires capabilities that are not authorized: {}",
        .missing.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
    )]
    Unauthorized { plugin_id: PluginId, missing: Vec<Capability> },
    #[error("plugin '{plugin_id}' is already registered")]
    AlreadyRegistered { plugin_id: PluginId },
    #[error(
        "plugin '{plugin_id}' contributes tool '{tool_name}', which plugin '{owned_by}' already owns"
    )]
    ToolNameConflict { plugin_id: PluginId, tool_name: String, owned_by: PluginId },
    #[error(
        "plugin '{plugin_id}' contributes tool '{tool_name}', which is already a built-in tool in \
         this run; a built-in name is not a plugin's to take"
    )]
    BuiltinNameTaken { plugin_id: PluginId, tool_name: String },
    #[error("plugin '{plugin_id}' is not registered")]
    UnknownPlugin { plugin_id: PluginId },
    #[error(
        "handle for '{plugin_id}' names registration generation {generation}, which is no longer current"
    )]
    StaleHandle { plugin_id: PluginId, generation: u64 },
    #[error("the plugin registry has been dropped")]
    RegistryGone,
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("payload error: {0}")]
    Payload(#[from] serde_json::Error),
    /// The manifest was written for a manifest schema this build does not read
    /// (YA-21). Refused before the capability gate: an unreadable claim must
    /// not be judged as if it had been read.
    #[error("plugin '{plugin_id}' has an unsupported manifest schema version: {source}")]
    Version {
        plugin_id: PluginId,
        #[source]
        source: agent_protocol::versioning::VersionError,
    },
}

impl PluginRegistry {
    /// Build an empty registry over `store`.
    ///
    /// Nothing is reserved yet. A run that has built-in tools says so with
    /// [`with_reserved_tools`], and a registry that was told nothing refuses
    /// nothing — which is the right default for a test that wants to watch a
    /// plugin take a name, and the wrong one for a process, which is why
    /// assembly always passes what it has.
    ///
    /// [`with_reserved_tools`]: Self::with_reserved_tools
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            store,
            state: RwLock::new(RegistryState::default()),
            reserved_tools: BTreeSet::new(),
        }
    }

    /// Name the tools this run already has, which no plugin may take.
    ///
    /// Taken as a builder rather than a constructor argument because the set is
    /// a property of the run, not of the registry: a registry built without one
    /// is a registry that has been told nothing about its surface, and that is
    /// a legitimate thing for a test to construct.
    pub fn with_reserved_tools(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.reserved_tools = names.into_iter().collect();
        self
    }

    /// How many changes the plugin surface has seen.
    ///
    /// Two reads of this bracket a turn: a surface built at generation N is what
    /// the next turn sees, and a plugin withdrawn after N cannot reach into a
    /// turn whose `ToolManifest` was frozen at N.
    pub fn generation(&self) -> u64 {
        self.read_state().generation
    }

    /// Whether a plugin is currently registered.
    pub fn is_registered(&self, plugin_id: &PluginId) -> bool {
        self.read_state().plugins.contains_key(plugin_id)
    }

    /// The ids of the registered plugins, in a stable order.
    pub fn registered_ids(&self) -> Vec<PluginId> {
        self.read_state().plugins.keys().cloned().collect()
    }

    /// Register a plugin, gated on `authorized`.
    ///
    /// The gate is asked first and answers everything else: if
    /// `manifest.capabilities.requires` is not a subset of `authorized`, this
    /// returns `Unauthorized` and **nothing else happens** — no contribution is
    /// constructed, no tool appears, no event is appended. A refusal is a
    /// decision not to load, taken from configuration; it is not something that
    /// happened to a session, and the session log must not say otherwise.
    ///
    /// The same rule covers the other three refusals: a duplicate id, a tool
    /// name another plugin already owns, and a tool name the run already has as
    /// a built-in all leave the registry exactly as it was.
    pub async fn register(
        self: &Arc<Self>,
        session_id: &SessionId,
        manifest: PluginManifest,
        runtime: PluginRuntime,
        authorized: &CapabilitySet,
    ) -> Result<PluginHandle, PluginError> {
        // Version first: every other refusal describes a claim this build
        // understood, and a manifest from a future build has not been read yet.
        agent_protocol::versioning::check(
            &manifest.schema_version,
            &agent_protocol::versioning::formats::PLUGIN_MANIFEST,
        )
        .map_err(|source| PluginError::Version { plugin_id: manifest.id.clone(), source })?;

        let missing = authorized.missing_from(&manifest.capabilities.requires);
        if !missing.is_empty() {
            return Err(PluginError::Unauthorized {
                plugin_id: manifest.id.clone(),
                missing: missing.into_iter().cloned().collect(),
            });
        }

        let capabilities = sorted_capabilities(&manifest.capabilities.requires);

        let (generation, previous_generation) = {
            let mut state = self.write_state();
            if state.plugins.contains_key(&manifest.id) {
                return Err(PluginError::AlreadyRegistered { plugin_id: manifest.id.clone() });
            }
            // Two plugins offering the same tool name would be resolved by
            // whichever the composite happens to ask first — an ordering nobody
            // chose deciding which implementation a model actually reaches.
            // Refusing the second is the only answer that stays true when the
            // ordering changes. A built-in name is the same problem with the
            // order already decided, so it is refused before the question is
            // even about another plugin.
            if let Some(provider) = runtime.tool_provider.as_ref() {
                for definition in provider.definitions() {
                    if self.reserved_tools.contains(&definition.name) {
                        return Err(PluginError::BuiltinNameTaken {
                            plugin_id: manifest.id.clone(),
                            tool_name: definition.name,
                        });
                    }
                    if let Some((owner, _)) =
                        state.plugins.iter().find(|(_, p)| p.owns_tool(&definition.name))
                    {
                        return Err(PluginError::ToolNameConflict {
                            plugin_id: manifest.id.clone(),
                            tool_name: definition.name,
                            owned_by: owner.clone(),
                        });
                    }
                }
            }
            let previous = state.generation;
            state.generation += 1;
            let generation = state.generation;
            state.plugins.insert(
                manifest.id.clone(),
                RegisteredPlugin {
                    manifest: manifest.clone(),
                    capabilities: capabilities.clone(),
                    runtime,
                    generation,
                },
            );
            (generation, previous)
        };

        let change = PluginStateChange {
            plugin_id: manifest.id.clone(),
            version: manifest.version.clone(),
            plugin_api: manifest.plugin_api.clone(),
            state: PluginLifecycleState::Registered,
            capabilities,
            generation,
            reason: None,
        };
        if let Err(error) = self.record(session_id, EventType::PLUGIN_LOADED, &change).await {
            // A registration that could not be recorded did not happen. The entry
            // goes and the generation goes back with it, so a later
            // `generation()` never reports a change the log does not have.
            //
            // The rollback only undoes *this* call's bump. `record` awaited, so
            // another registration may have landed in between; rewinding
            // unconditionally would erase that one's change too and leave the
            // counter below a generation the log already carries. Rewinding only
            // when the counter is still ours keeps the invariant one event per
            // increment. (YA-36)
            let mut state = self.write_state();
            state.plugins.remove(&manifest.id);
            if state.generation == generation {
                state.generation = previous_generation;
            }
            return Err(error);
        }

        Ok(PluginHandle {
            registry: Arc::downgrade(self),
            plugin_id: manifest.id,
            session_id: session_id.clone(),
            generation,
        })
    }

    /// The tools contributed by every registered plugin.
    ///
    /// Provenance is stamped here rather than trusted from the plugin: whatever
    /// the contributed provider says its tools' source is, a caller reaching a
    /// tool through the registry is reaching a tool that arrived through this
    /// registry. The one thing a manifest may choose is *which* kind of plugin
    /// door it came through — `Plugin` or `Mcp` — and only those two, because
    /// the vocabulary of `PluginToolSource` does not contain the others.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let state = self.read_state();
        let mut definitions = Vec::new();
        for plugin in state.plugins.values() {
            let Some(provider) = plugin.runtime.tool_provider.as_ref() else {
                continue;
            };
            let source = plugin.manifest.contributions.tool_source.into();
            definitions
                .extend(provider.definitions().into_iter().map(|d| ToolDefinition { source, ..d }));
        }
        definitions
    }

    /// The side-effect class of a tool a plugin contributes.
    pub fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        let state = self.read_state();
        state.plugins.values().find_map(|p| p.side_effect(tool_name))
    }

    /// Run a tool a plugin contributes.
    ///
    /// The provider is cloned out of the table and the read lock released before
    /// the call, so a long-running tool cannot block a registration waiting to
    /// write.
    pub async fn invoke(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolProviderError> {
        let provider = {
            let state = self.read_state();
            state.plugins.values().find_map(|p| p.owner_of(&invocation.tool_name)).ok_or_else(
                || ToolProviderError::UnknownTool {
                    tool: invocation.tool_name.clone(),
                    // Read off the guard already held: asking the registry for
                    // its definitions here would take the lock a second time.
                    available: state
                        .plugins
                        .values()
                        .filter_map(|p| p.runtime.tool_provider.as_ref())
                        .flat_map(|provider| provider.definitions())
                        .map(|d| d.name)
                        .collect(),
                },
            )?
        };
        provider.invoke(invocation).await
    }

    /// Withdraw a registration. Reached through [`PluginHandle`].
    async fn withdraw(
        &self,
        session_id: &SessionId,
        plugin_id: &PluginId,
        expected_generation: u64,
        state: PluginLifecycleState,
        reason: Option<String>,
    ) -> Result<PluginStateChange, PluginError> {
        // Both halves — the table and the log — move together or not at all, the
        // same rule `register` follows. The entry is *cloned into* the event
        // rather than consumed by it, because a withdrawal that could not be
        // recorded has to be put back: the log is the authority on what is
        // registered, and a registry that reports a plugin gone while the log
        // still has it is the one disagreement this type exists to prevent.
        let (change, withdrawn, previous_generation) = {
            let mut registry = self.write_state();
            let Some(current) = registry.plugins.get(plugin_id) else {
                return Err(PluginError::UnknownPlugin { plugin_id: plugin_id.clone() });
            };
            if current.generation != expected_generation {
                return Err(PluginError::StaleHandle {
                    plugin_id: plugin_id.clone(),
                    generation: expected_generation,
                });
            }
            let previous = registry.generation;
            let withdrawn = registry
                .plugins
                .remove(plugin_id)
                .expect("the entry was read under this same lock");
            registry.generation += 1;
            let change = PluginStateChange {
                plugin_id: plugin_id.clone(),
                version: withdrawn.manifest.version.clone(),
                plugin_api: withdrawn.manifest.plugin_api.clone(),
                state,
                capabilities: withdrawn.capabilities.clone(),
                generation: registry.generation,
                reason,
            };
            (change, withdrawn, previous)
        };

        let event_type = match state {
            PluginLifecycleState::Failed => EventType::PLUGIN_FAILED,
            _ => EventType::PLUGIN_UNLOADED,
        };
        if let Err(error) = self.record(session_id, event_type, &change).await {
            // A withdrawal that could not be recorded did not happen either. The
            // entry goes back and the generation goes back with it, so a later
            // `generation()` never reports a change the log does not have — and
            // the plugin's tools stay on the surface, which is where a log that
            // still lists the plugin says they are.
            //
            // Like `register`, the rewind undoes only this call's bump: `record`
            // awaited, so another lifecycle change may have landed in between
            // and rewinding past it would erase its event's generation. (YA-36)
            let mut registry = self.write_state();
            registry.plugins.insert(plugin_id.clone(), withdrawn);
            if registry.generation == change.generation {
                registry.generation = previous_generation;
            }
            return Err(error);
        }
        Ok(change)
    }

    /// Append one lifecycle event at the next sequence of `session_id`.
    async fn record(
        &self,
        session_id: &SessionId,
        event_type: &str,
        change: &PluginStateChange,
    ) -> Result<(), PluginError> {
        // The read and the append are one step: another process on this session
        // can move the sequence between them, and an append computed from a
        // stale read is refused. (YA-36)
        let _lease = self.store.lease(session_id).await?;
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        let next = events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO);
        let draft = EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::current(),
            session_id: session_id.clone(),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::System,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::to_value(change)?,
            metadata: serde_json::json!({}),
            ignorable: false,
        };
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(())
    }

    /// The registry table, read.
    ///
    /// A poisoned lock means a panic was raised while the table was held — the
    /// assertions taken under this very lock, in the withdrawal below, are one
    /// way that could happen. Taking the panic again here would turn that one
    /// fault into a permanent one: every later read *and* write of the registry
    /// would panic too, and a run would lose its whole plugin surface to a
    /// thread that died somewhere else. The poison is read through instead, as
    /// the kernel's live bus reads its own state — the mutations guarded here
    /// are single map operations with no user code between them, so the table is
    /// as consistent as the last call that finished. (YA-24)
    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, RegistryState> {
        self.state.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The registry table, written — read through a poison for the reason
    /// `read_state` gives just above.
    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, RegistryState> {
        self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl PluginHandle {
    /// The id of the plugin this handle registered.
    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// The generation this registration produced.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Withdraw the plugin: its contributions leave the tool surface and a
    /// `plugin/unloaded` event records that they did.
    ///
    /// The state recorded is `Unregistered`, because that is the fact the
    /// registry knows — the plugin is no longer registered. "Disabled" is the
    /// same event told from the operator's side, so it goes in the reason rather
    /// than competing with the state for the same slot.
    pub async fn disable(&self) -> Result<PluginStateChange, PluginError> {
        self.registry()?
            .withdraw(
                &self.session_id,
                &self.plugin_id,
                self.generation,
                PluginLifecycleState::Unregistered,
                Some("disabled".to_string()),
            )
            .await
    }

    /// Record that the plugin stopped working, and withdraw it.
    ///
    /// A failed plugin is not left contributing tools: whatever it was doing, it
    /// is not doing it correctly, and a surface that keeps offering its tools is
    /// offering tools nobody can vouch for.
    pub async fn fail(&self, reason: impl Into<String>) -> Result<PluginStateChange, PluginError> {
        let reason = reason.into();
        self.registry()?
            .withdraw(
                &self.session_id,
                &self.plugin_id,
                self.generation,
                PluginLifecycleState::Failed,
                Some(reason),
            )
            .await
    }

    fn registry(&self) -> Result<Arc<PluginRegistry>, PluginError> {
        self.registry.upgrade().ok_or(PluginError::RegistryGone)
    }
}

impl RegisteredPlugin {
    /// Whether this plugin's provider answers for `tool_name`.
    fn owns_tool(&self, tool_name: &str) -> bool {
        let Some(provider) = self.runtime.tool_provider.as_ref() else {
            return false;
        };
        provider.side_effect(tool_name).is_some() || has_definition(provider, tool_name)
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        let provider = self.runtime.tool_provider.as_ref()?;
        provider.side_effect(tool_name)
    }

    /// The provider to call for `tool_name`, cloned out of the table.
    fn owner_of(&self, tool_name: &str) -> Option<Arc<dyn ToolProvider>> {
        if self.owns_tool(tool_name) { self.runtime.tool_provider.clone() } else { None }
    }
}

fn has_definition(provider: &Arc<dyn ToolProvider>, tool_name: &str) -> bool {
    provider.definitions().iter().any(|d| d.name == tool_name)
}

/// A plugin's capabilities in a stable order, without duplicates.
///
/// The manifest's order is the packager's; the log's order is the reader's. Two
/// manifests that require the same set must record the same bytes.
fn sorted_capabilities(required: &[Capability]) -> Vec<Capability> {
    let unique: BTreeSet<&Capability> = required.iter().collect();
    unique.into_iter().cloned().collect()
}

/// The registered plugins' tools, as one provider.
///
/// This is what assembly composes next to the built-in families so that a turn
/// sees one tool surface. It reads the registry on every call rather than caching
/// a snapshot, which is what makes a registration or a withdrawal land on the
/// *next* turn's manifest and not on the one already frozen.
pub struct RegistryToolProvider {
    registry: Arc<PluginRegistry>,
}

impl RegistryToolProvider {
    pub fn new(registry: Arc<PluginRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl ToolProvider for RegistryToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.registry.tool_definitions()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.registry.side_effect(tool_name)
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        self.registry.invoke(invocation).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use agent_ports::event_store::{AppendReceipt, EventSubscription, FlushReceipt, SessionLease};
    use agent_protocol::events::StoredEvent;
    use agent_protocol::plugin::{PluginCapabilities, PluginContributions, PluginToolSource};
    use agent_protocol::tool::ToolSource;
    use agent_testkit::{FakeEventStore, FakeToolProvider};

    fn registry() -> (Arc<PluginRegistry>, Arc<FakeEventStore>, SessionId) {
        let store = Arc::new(FakeEventStore::new());
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        (registry, store, SessionId::from_string("plugin-session"))
    }

    fn manifest(id: &str, requires: &[&str]) -> PluginManifest {
        PluginManifest {
            schema_version: agent_protocol::versioning::formats::PLUGIN_MANIFEST,
            id: PluginId::from_string(id),
            version: "0.1.0".to_string(),
            plugin_api: "1".to_string(),
            capabilities: PluginCapabilities {
                requires: requires.iter().map(|c| Capability::new(*c)).collect(),
            },
            contributions: PluginContributions {
                tool_provider: true,
                ..PluginContributions::default()
            },
            config_schema: None,
        }
    }

    fn runtime(tools: &[(&str, ToolSideEffect)]) -> PluginRuntime {
        let mut provider = FakeToolProvider::new();
        for (name, side_effect) in tools {
            provider = provider.with_tool(name, "ok", *side_effect);
        }
        PluginRuntime { tool_provider: Some(Arc::new(provider)) }
    }

    fn authorized(names: &[&str]) -> CapabilitySet {
        names.iter().map(|n| Capability::new(*n)).collect()
    }

    async fn event_types(store: &FakeEventStore, sid: &SessionId) -> Vec<String> {
        store
            .read_stream(sid, EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event.event_type.to_string())
            .collect()
    }

    /// Registration and withdrawal each read the log and append at it as one
    /// step. The store refuses an append that arrives without a lease, so both
    /// only pass while the pair is covered. (YA-36)
    #[tokio::test]
    async fn register_and_withdraw_hold_the_lease_across_their_read_and_append() {
        let store = Arc::new(FakeEventStore::new().requiring_lease());
        let registry = Arc::new(PluginRegistry::new(store));
        let sid = SessionId::from_string("plugin-session");

        let handle = registry
            .register(
                &sid,
                manifest("leased", &[]),
                runtime(&[("leased.echo", ToolSideEffect::Pure)]),
                &authorized(&[]),
            )
            .await
            .expect("the registration is recorded under the lease");
        handle.disable().await.expect("the withdrawal is recorded under the lease");

        assert!(!registry.registered_ids().contains(&PluginId::from_string("leased")));
    }

    #[tokio::test]
    async fn a_manifest_from_a_future_schema_is_refused_before_the_capability_gate() {
        let (registry, _store, sid) = registry();
        let mut future = manifest("future", &[Capability::FS_READ]);
        future.schema_version = agent_protocol::versioning::Version::parse("99").unwrap();

        // The capability gate would refuse this too (nothing is authorized), so
        // the test also pins which refusal wins: the claim was never read, so it
        // cannot be judged.
        let error = registry
            .register(&sid, future, runtime(&[]), &authorized(&[]))
            .await
            .expect_err("a future manifest is not this build's manifest");
        // The direction matters: "your manifest is newer than this build" is a
        // different instruction to the reader than "older, with no migration".
        assert!(
            matches!(
                error,
                PluginError::Version {
                    source: agent_protocol::versioning::VersionError::Future { .. },
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("99"), "the error names the claim: {error}");
        assert!(registry.registered_ids().is_empty(), "nothing may be registered");
    }

    #[tokio::test]
    async fn a_manifest_without_a_schema_version_is_read_as_current() {
        // A manifest authored before the field existed has today's shape. The
        // registry must not turn the field's introduction into a flag day.
        let (registry, _store, sid) = registry();
        let mut json = serde_json::to_value(manifest("old-plugin", &[])).unwrap();
        json.as_object_mut().unwrap().remove("schema_version");
        let old: PluginManifest = serde_json::from_value(json).expect("an old manifest parses");
        assert_eq!(old.schema_version, agent_protocol::versioning::formats::PLUGIN_MANIFEST);

        registry
            .register(&sid, old, runtime(&[]), &authorized(&[]))
            .await
            .expect("a pre-versioning manifest registers normally");
    }

    #[tokio::test]
    async fn an_authorized_plugin_registers_and_contributes_tools() {
        let (registry, store, sid) = registry();

        let handle = registry
            .register(
                &sid,
                manifest("git-tools", &[Capability::FS_READ]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &authorized(&[Capability::FS_READ]),
            )
            .await
            .unwrap();

        assert_eq!(handle.plugin_id(), &PluginId::from_string("git-tools"));
        assert!(registry.is_registered(&PluginId::from_string("git-tools")));
        assert_eq!(registry.generation(), 1);
        assert_eq!(
            registry.tool_definitions().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["git.blame"]
        );
        assert_eq!(event_types(&store, &sid).await, vec![EventType::PLUGIN_LOADED.to_string()]);
    }

    #[tokio::test]
    async fn an_unauthorized_plugin_is_refused_with_no_side_effects() {
        let (registry, store, sid) = registry();

        let error = registry
            .register(
                &sid,
                manifest("greedy", &[Capability::FS_READ, Capability::NETWORK_REQUEST]),
                runtime(&[("net.get", ToolSideEffect::Network)]),
                &authorized(&[Capability::FS_READ]),
            )
            .await
            .unwrap_err();

        // The refusal names what was missing, so an operator is not left
        // diffing two lists by hand.
        assert!(
            matches!(&error, PluginError::Unauthorized { missing, .. }
                if missing == &vec![Capability::new(Capability::NETWORK_REQUEST)]),
            "{error:?}"
        );
        // Zero side effects: no tool, no event, no generation advance.
        assert!(registry.tool_definitions().is_empty());
        assert_eq!(registry.generation(), 0);
        assert!(event_types(&store, &sid).await.is_empty());
    }

    #[tokio::test]
    async fn an_empty_authorized_set_refuses_everything_that_asks_for_anything() {
        let (registry, store, sid) = registry();

        assert!(matches!(
            registry
                .register(
                    &sid,
                    manifest("git-tools", &[Capability::FS_READ]),
                    runtime(&[("git.blame", ToolSideEffect::Read)]),
                    &CapabilitySet::new(),
                )
                .await,
            Err(PluginError::Unauthorized { .. })
        ));

        // The other half of fail-closed: a plugin that requires *nothing* is
        // still admissible when nothing is authorized, because it asked for
        // nothing it was not given.
        assert!(
            registry
                .register(
                    &sid,
                    manifest("quiet", &[]),
                    PluginRuntime::default(),
                    &CapabilitySet::new(),
                )
                .await
                .is_ok()
        );
        assert_eq!(event_types(&store, &sid).await.len(), 1);
    }

    #[tokio::test]
    async fn a_duplicate_id_is_refused() {
        let (registry, store, sid) = registry();
        let authorized = authorized(&[Capability::FS_READ]);

        registry
            .register(
                &sid,
                manifest("git-tools", &[Capability::FS_READ]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &authorized,
            )
            .await
            .unwrap();
        let error = registry
            .register(
                &sid,
                manifest("git-tools", &[Capability::FS_READ]),
                runtime(&[("git.other", ToolSideEffect::Read)]),
                &authorized,
            )
            .await
            .unwrap_err();

        assert!(matches!(error, PluginError::AlreadyRegistered { .. }), "{error:?}");
        // The refused second registration changed nothing: one event, one tool.
        assert_eq!(event_types(&store, &sid).await.len(), 1);
        assert_eq!(registry.tool_definitions().len(), 1);
        assert_eq!(registry.generation(), 1);
    }

    #[tokio::test]
    async fn a_second_plugin_cannot_claim_a_tool_name_already_owned() {
        let (registry, _, sid) = registry();
        let authorized = authorized(&[Capability::FS_READ]);

        registry
            .register(
                &sid,
                manifest("first", &[Capability::FS_READ]),
                runtime(&[("shared.tool", ToolSideEffect::Read)]),
                &authorized,
            )
            .await
            .unwrap();
        let error = registry
            .register(
                &sid,
                manifest("second", &[Capability::FS_READ]),
                runtime(&[("shared.tool", ToolSideEffect::Destructive)]),
                &authorized,
            )
            .await
            .unwrap_err();

        assert!(
            matches!(&error, PluginError::ToolNameConflict { tool_name, owned_by, .. }
                if tool_name == "shared.tool" && owned_by == &PluginId::from_string("first")),
            "{error:?}"
        );
        assert_eq!(registry.tool_definitions().len(), 1);
        // Whose classification stands is not decided by order: the owner kept
        // its own.
        assert_eq!(registry.side_effect("shared.tool"), Some(ToolSideEffect::Read));
    }

    #[tokio::test]
    async fn a_plugin_cannot_take_a_builtin_tool_name() {
        let store = Arc::new(FakeEventStore::new());
        // The registry is told what the run already has, the way assembly tells
        // it: derived from the built-in providers rather than written down here.
        let registry = Arc::new(
            PluginRegistry::new(store.clone()).with_reserved_tools(["fs.read".to_string()]),
        );
        let sid = SessionId::from_string("plugin-session");

        let error = registry
            .register(
                &sid,
                manifest("greedy", &[Capability::FS_READ]),
                runtime(&[("fs.read", ToolSideEffect::Read)]),
                &authorized(&[Capability::FS_READ]),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(&error, PluginError::BuiltinNameTaken { tool_name, .. }
                if tool_name == "fs.read"),
            "{error:?}"
        );
        // Refused means nothing happened: no tool, no event, no generation.
        assert!(registry.tool_definitions().is_empty());
        assert!(!registry.is_registered(&PluginId::from_string("greedy")));
        assert_eq!(registry.generation(), 0);
        assert!(event_types(&store, &sid).await.is_empty());

        // The same plugin under a name of its own is fine — the refusal is
        // about the name, not about the plugin.
        assert!(
            registry
                .register(
                    &sid,
                    manifest("greedy", &[Capability::FS_READ]),
                    runtime(&[("greedy.read", ToolSideEffect::Read)]),
                    &authorized(&[Capability::FS_READ]),
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn disabling_withdraws_the_tools_and_records_the_change() {
        let (registry, store, sid) = registry();
        let handle = registry
            .register(
                &sid,
                manifest("git-tools", &[Capability::FS_READ]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &authorized(&[Capability::FS_READ]),
            )
            .await
            .unwrap();
        let after_register = registry.generation();

        let change = handle.disable().await.unwrap();

        assert_eq!(change.state, PluginLifecycleState::Unregistered);
        assert_eq!(change.reason.as_deref(), Some("disabled"));
        // The generation advanced once for the registration and once for the
        // withdrawal — the counter a turn boundary compares against.
        assert_eq!(change.generation, after_register + 1);
        assert_eq!(registry.generation(), after_register + 1);
        assert!(registry.tool_definitions().is_empty());
        assert!(!registry.is_registered(&PluginId::from_string("git-tools")));
        assert_eq!(
            event_types(&store, &sid).await,
            vec![EventType::PLUGIN_LOADED.to_string(), EventType::PLUGIN_UNLOADED.to_string()]
        );
    }

    #[tokio::test]
    async fn a_tool_that_went_away_is_no_longer_reachable() {
        let (registry, _, sid) = registry();
        let handle = registry
            .register(
                &sid,
                manifest("git-tools", &[Capability::FS_READ]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &authorized(&[Capability::FS_READ]),
            )
            .await
            .unwrap();
        let invocation = ToolInvocation::proposed("git.blame", serde_json::json!({}));
        assert!(registry.invoke(&invocation).await.is_ok());

        handle.disable().await.unwrap();

        // Withdrawn means gone, not hidden: a call to it is an unknown tool.
        assert!(matches!(
            registry.invoke(&invocation).await,
            Err(ToolProviderError::UnknownTool { .. })
        ));
    }

    #[tokio::test]
    async fn a_withdrawn_handle_cannot_withdraw_a_later_registration() {
        let (registry, _, sid) = registry();
        let authorized = authorized(&[Capability::FS_READ]);
        let first = registry
            .register(
                &sid,
                manifest("git-tools", &[Capability::FS_READ]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &authorized,
            )
            .await
            .unwrap();
        first.disable().await.unwrap();
        registry
            .register(
                &sid,
                manifest("git-tools", &[Capability::FS_READ]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &authorized,
            )
            .await
            .unwrap();

        // The old handle names a generation that is gone. Letting it through
        // would withdraw a registration it never made.
        assert!(matches!(first.disable().await, Err(PluginError::StaleHandle { .. })));
        assert!(registry.is_registered(&PluginId::from_string("git-tools")));
        assert_eq!(registry.tool_definitions().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_plugin_stops_contributing_and_is_recorded_as_failed() {
        let (registry, store, sid) = registry();
        let handle = registry
            .register(
                &sid,
                manifest("flaky", &[]),
                runtime(&[("flaky.tool", ToolSideEffect::Write)]),
                &CapabilitySet::new(),
            )
            .await
            .unwrap();

        let change = handle.fail("upstream refused").await.unwrap();

        assert_eq!(change.state, PluginLifecycleState::Failed);
        assert_eq!(change.reason.as_deref(), Some("upstream refused"));
        assert!(registry.tool_definitions().is_empty());
        assert_eq!(
            event_types(&store, &sid).await,
            vec![EventType::PLUGIN_LOADED.to_string(), EventType::PLUGIN_FAILED.to_string()]
        );
    }

    #[tokio::test]
    async fn provenance_is_stamped_by_the_registry_not_the_plugin() {
        let (registry, _, sid) = registry();
        registry
            .register(
                &sid,
                manifest("git-tools", &[]),
                // The fake provider calls its tools `Builtin`; through the
                // registry they are a plugin's tools, and the registry says so.
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &CapabilitySet::new(),
            )
            .await
            .unwrap();

        assert_eq!(registry.tool_definitions()[0].source, ToolSource::Plugin);
    }

    #[tokio::test]
    async fn a_manifest_declaring_mcp_stamps_its_tools_mcp() {
        let (registry, _, sid) = registry();
        let mut mcp_manifest = manifest("mcp.files", &[]);
        mcp_manifest.contributions.tool_source = PluginToolSource::Mcp;

        registry
            .register(
                &sid,
                mcp_manifest,
                runtime(&[("mcp__files__read", ToolSideEffect::Read)]),
                &CapabilitySet::new(),
            )
            .await
            .unwrap();

        // The provider called its tool whatever it liked; the registry read the
        // declared kind and said `Mcp`, because that is the door it came in by.
        assert_eq!(registry.tool_definitions()[0].source, ToolSource::Mcp);
    }

    #[tokio::test]
    async fn the_registry_provider_presents_the_registrys_current_surface() {
        let (registry, _, sid) = registry();
        let provider = RegistryToolProvider::new(registry.clone());
        assert!(provider.definitions().is_empty());

        registry
            .register(
                &sid,
                manifest("git-tools", &[]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &CapabilitySet::new(),
            )
            .await
            .unwrap();

        // The same provider object, asked again: the surface moves with the
        // registry, which is what lets a change land on the next turn rather
        // than on the manifest already frozen.
        assert_eq!(
            provider.definitions().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["git.blame"]
        );
        assert_eq!(provider.side_effect("git.blame"), Some(ToolSideEffect::Read));
        assert!(
            provider
                .invoke(&ToolInvocation::proposed("git.blame", serde_json::json!({})))
                .await
                .is_ok()
        );
        assert!(
            provider
                .invoke(&ToolInvocation::proposed("nope", serde_json::json!({})))
                .await
                .is_err()
        );
    }

    /// A store that reads through to a fake and can be told to refuse appends.
    ///
    /// The only way to reach `register`'s rollback path honestly: short of a
    /// store that refuses, every registration succeeds, and the invariant that a
    /// failed record leaves nothing behind would go untested.
    struct RefusingAppendStore {
        inner: Arc<FakeEventStore>,
        refuse: AtomicBool,
    }

    impl RefusingAppendStore {
        fn new(inner: Arc<FakeEventStore>) -> Self {
            Self { inner, refuse: AtomicBool::new(false) }
        }
    }

    #[async_trait::async_trait]
    impl EventStore for RefusingAppendStore {
        async fn append(
            &self,
            session_id: &SessionId,
            expected_next_seq: EventSeq,
            drafts: Vec<EventDraft>,
        ) -> Result<AppendReceipt, EventStoreError> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err(EventStoreError::SequenceConflict {
                    expected: expected_next_seq,
                    actual: expected_next_seq,
                });
            }
            self.inner.append(session_id, expected_next_seq, drafts).await
        }

        async fn lease(
            &self,
            session_id: &SessionId,
        ) -> Result<Box<dyn SessionLease>, EventStoreError> {
            self.inner.lease(session_id).await
        }

        async fn flush(&self, session_id: &SessionId) -> Result<FlushReceipt, EventStoreError> {
            self.inner.flush(session_id).await
        }

        async fn read_stream(
            &self,
            session_id: &SessionId,
            from_seq: EventSeq,
            limit: usize,
        ) -> Result<Vec<StoredEvent>, EventStoreError> {
            self.inner.read_stream(session_id, from_seq, limit).await
        }

        async fn subscribe(
            &self,
            session_id: &SessionId,
            from_seq: EventSeq,
        ) -> Result<Box<dyn EventSubscription>, EventStoreError> {
            self.inner.subscribe(session_id, from_seq).await
        }
    }

    #[tokio::test]
    async fn a_registration_that_cannot_be_recorded_does_not_happen() {
        let inner = Arc::new(FakeEventStore::new());
        let store = Arc::new(RefusingAppendStore::new(inner.clone()));
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let sid = SessionId::from_string("plugin-session");

        store.refuse.store(true, Ordering::SeqCst);
        let error = registry
            .register(
                &sid,
                manifest("git-tools", &[]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &CapabilitySet::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, PluginError::Store(_)), "{error:?}");
        // Nothing registered, nothing recorded, and the generation did not move:
        // the surface is exactly what it was before the attempt.
        assert!(!registry.is_registered(&PluginId::from_string("git-tools")));
        assert_eq!(registry.generation(), 0);
        assert!(registry.tool_definitions().is_empty());
        assert!(inner.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().is_empty());

        // With the store working again the same registration succeeds, at
        // generation 1 — not 2.
        store.refuse.store(false, Ordering::SeqCst);
        registry
            .register(
                &sid,
                manifest("git-tools", &[]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &CapabilitySet::new(),
            )
            .await
            .unwrap();
        assert_eq!(registry.generation(), 1);
    }

    /// A store that holds one `append` open until the test releases it, so a
    /// second registration can land *inside* the first one's `record`. The
    /// in-process kernel never interleaves this way; the point is to pin what
    /// the rollback does if it ever did. The held append then answers with the
    /// conflict a store that was overtaken would.
    struct GatedStore {
        inner: Arc<FakeEventStore>,
        /// Signalled when the held append is entered. Taken by the first one.
        reached: Mutex<Option<futures_channel::oneshot::Sender<()>>>,
        /// Awaited before the held append answers. Taken by the first one.
        release: Mutex<Option<futures_channel::oneshot::Receiver<()>>>,
        armed: AtomicBool,
    }

    impl GatedStore {
        fn new(inner: Arc<FakeEventStore>) -> Self {
            Self {
                inner,
                reached: Mutex::new(None),
                release: Mutex::new(None),
                armed: AtomicBool::new(false),
            }
        }

        /// Hand the test the two ends it drives: arm the gate, and take what it
        /// signals with and what it waits on.
        fn gate(
            &self,
        ) -> (futures_channel::oneshot::Receiver<()>, futures_channel::oneshot::Sender<()>)
        {
            let (reached_tx, reached_rx) = futures_channel::oneshot::channel();
            let (release_tx, release_rx) = futures_channel::oneshot::channel();
            *self.reached.lock().unwrap() = Some(reached_tx);
            *self.release.lock().unwrap() = Some(release_rx);
            self.armed.store(true, Ordering::SeqCst);
            (reached_rx, release_tx)
        }
    }

    #[async_trait::async_trait]
    impl EventStore for GatedStore {
        async fn append(
            &self,
            session_id: &SessionId,
            expected_next_seq: EventSeq,
            drafts: Vec<EventDraft>,
        ) -> Result<AppendReceipt, EventStoreError> {
            if self.armed.swap(false, Ordering::SeqCst) {
                if let Some(reached) = self.reached.lock().unwrap().take() {
                    let _ = reached.send(());
                }
                // The guard is dropped before the await, so this future stays
                // `Send` — it is boxed into the store's `dyn` slot.
                let release = self.release.lock().unwrap().take();
                if let Some(release) = release {
                    let _ = release.await;
                }
                return Err(EventStoreError::SequenceConflict {
                    expected: expected_next_seq,
                    actual: expected_next_seq,
                });
            }
            self.inner.append(session_id, expected_next_seq, drafts).await
        }

        async fn lease(
            &self,
            session_id: &SessionId,
        ) -> Result<Box<dyn SessionLease>, EventStoreError> {
            self.inner.lease(session_id).await
        }

        async fn flush(&self, session_id: &SessionId) -> Result<FlushReceipt, EventStoreError> {
            self.inner.flush(session_id).await
        }

        async fn read_stream(
            &self,
            session_id: &SessionId,
            from_seq: EventSeq,
            limit: usize,
        ) -> Result<Vec<StoredEvent>, EventStoreError> {
            self.inner.read_stream(session_id, from_seq, limit).await
        }

        async fn subscribe(
            &self,
            session_id: &SessionId,
            from_seq: EventSeq,
        ) -> Result<Box<dyn EventSubscription>, EventStoreError> {
            self.inner.subscribe(session_id, from_seq).await
        }
    }

    /// A registration that fails to record must put back only *its own* bump.
    /// If another registration landed while it was writing, rewinding the
    /// counter would erase that one's event's generation — the log would carry
    /// a lifecycle change the registry says never happened. (YA-36)
    #[tokio::test]
    async fn a_failed_registration_does_not_rewind_a_later_registration() {
        let inner = Arc::new(FakeEventStore::new());
        let store = Arc::new(GatedStore::new(inner.clone()));
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let sid = SessionId::from_string("plugin-session");
        let (reached, release) = store.gate();

        let first = {
            let registry = registry.clone();
            let sid = sid.clone();
            tokio::spawn(async move {
                registry
                    .register(
                        &sid,
                        manifest("first", &[]),
                        runtime(&[("first.tool", ToolSideEffect::Read)]),
                        &CapabilitySet::new(),
                    )
                    .await
            })
        };

        // The first registration is now inside its append, at generation 1.
        reached.await.expect("the first append reached the gate");

        // A second registration lands entirely while the first is held open.
        registry
            .register(
                &sid,
                manifest("second", &[]),
                runtime(&[("second.tool", ToolSideEffect::Read)]),
                &CapabilitySet::new(),
            )
            .await
            .expect("the second registration is recorded");

        release.send(()).expect("the held append is still waiting");
        let error = first.await.unwrap().unwrap_err();
        assert!(matches!(error, PluginError::Store(_)), "{error:?}");

        // The first took its entry back but left the counter where the second
        // put it: generation 2, and the second still registered.
        assert_eq!(registry.generation(), 2);
        assert!(registry.is_registered(&PluginId::from_string("second")));
        assert!(!registry.is_registered(&PluginId::from_string("first")));
    }

    #[tokio::test]
    async fn a_withdrawal_that_cannot_be_recorded_does_not_happen() {
        let inner = Arc::new(FakeEventStore::new());
        let store = Arc::new(RefusingAppendStore::new(inner.clone()));
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let sid = SessionId::from_string("plugin-session");

        let handle = registry
            .register(
                &sid,
                manifest("git-tools", &[]),
                runtime(&[("git.blame", ToolSideEffect::Read)]),
                &CapabilitySet::new(),
            )
            .await
            .unwrap();

        store.refuse.store(true, Ordering::SeqCst);
        let error = handle.disable().await.unwrap_err();
        assert!(matches!(error, PluginError::Store(_)), "{error:?}");

        // The withdrawal did not happen either: the entry is back, its tool is
        // still on the surface, the generation did not move, and the log holds
        // nothing but the registration. Dropping the entry here would leave the
        // registry reporting a plugin gone while the log still lists it — a state
        // no reader can reason about, which is the whole point of the pairing.
        assert!(registry.is_registered(&PluginId::from_string("git-tools")));
        assert_eq!(registry.generation(), 1);
        assert_eq!(
            registry.tool_definitions().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["git.blame"]
        );
        assert_eq!(event_types(&inner, &sid).await, vec![EventType::PLUGIN_LOADED.to_string()]);

        // The handle is not consumed by the attempt: withdrawing again once the
        // log works is the same withdrawal, and lands at generation 2 rather than
        // 3 — the failed attempt left no mark on the counter.
        store.refuse.store(false, Ordering::SeqCst);
        let change = handle.disable().await.unwrap();
        assert_eq!(change.generation, 2);
        assert!(!registry.is_registered(&PluginId::from_string("git-tools")));
        assert_eq!(
            event_types(&inner, &sid).await,
            vec![EventType::PLUGIN_LOADED.to_string(), EventType::PLUGIN_UNLOADED.to_string()]
        );
    }

    /// A panic while the registry's lock is held poisons it. The registry must
    /// keep answering afterwards: taking the poison again would let one
    /// thread's death take the whole plugin surface down with it, for the rest
    /// of the run. (YA-24)
    #[test]
    fn a_poisoned_registry_lock_does_not_stop_the_registry() {
        let (registry, _store, _sid) = registry();

        // Poison the lock the way any panic under it would: hold the guard,
        // then die.
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = registry.write_state();
            panic!("a thread died while holding the registry lock");
        }));
        assert!(died.is_err(), "the premise: the lock is poisoned now");

        // Every one of these panicked before the fix, and the registry still
        // answers with the truth it held before the panic.
        assert_eq!(registry.generation(), 0);
        assert!(!registry.is_registered(&PluginId::from_string("git-tools")));
        assert!(registry.registered_ids().is_empty());
    }
}
