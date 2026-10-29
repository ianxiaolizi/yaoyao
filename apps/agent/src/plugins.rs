//! The plugin surface, as the application sees it: what is configured, what the
//! log says happened, and the controls that change it at a safe boundary.
//!
//! ## Why this is two halves that do not talk to each other
//!
//! Turning a plugin off is a change to a **running** process, and the command
//! that asks for it is a **different** process — the operator types
//! `agent plugin disable` at a shell while `agent run` is in the middle of a
//! turn somewhere else. Nothing here can reach into that other process's memory,
//! so the command does not try: it appends a control item to the session's Inbox
//! ([`disable`], [`enable`]) and the process that is actually driving the
//! session applies it at its next turn boundary ([`PluginControl`]).
//!
//! That split is the whole design. It also fixes what [`list`] may claim: it can
//! read the configuration file, the session log, and the queue — all durable,
//! all shared — and it cannot read another process's live registry. The output
//! says so rather than guessing.
//!
//! ## The boundary is not a nicety
//!
//! A turn opens with a frozen tool manifest: the set of tools the model will be
//! offered for that whole turn, decided once. Applying a withdrawal mid-turn
//! would leave the manifest offering a tool the registry no longer has — an
//! offer that is guaranteed to fail when taken up. So the change waits for the
//! safe boundary, which is where `Scheduler` drains controls: after the
//! turn that was in flight, before the next one opens.
//!
//! ## A disable is a standing decision, not only a live withdrawal
//!
//! A withdrawal recorded at a boundary would otherwise last exactly as long as
//! the process that made it: the next `agent run` starts from the configuration
//! and registers the plugin all over again, so the operator's change would be
//! undone by doing nothing at all. What makes it stick is that later processes
//! read the **log** before they register — a plugin whose last lifecycle event
//! is the operator's withdrawal is not started ([`suppressed`],
//! [`register_configured`]). [`enable`] ends that with a registration, which is
//! the same rule read the other way, so nothing has to be kept in sync.
//!
//! Which is exactly why a log that cannot be read holds **everything** back
//! ([`Suppressed::Unreadable`]): a failed read is not a log that says nothing
//! was withdrawn, and letting it stand in for one would make the standing
//! decision the one thing about this design that a disk error can undo.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use agent_cli::{CliOutput, json_output};
use agent_config::{McpTransport, ResolvedProfile};
use agent_kernel::inbox::Inbox;
use agent_kernel::plugin::{PluginError, PluginHandle, PluginRegistry, PluginRuntime};
use agent_kernel::resource_guard::{ResourceGuard, ResourceGuardError};
use agent_ports::artifact_store::ArtifactStore;
use agent_ports::control::{ControlError, ControlHandler, ControlOutcome, ControlRequest};
use agent_ports::event_store::EventStore;
use agent_protocol::events::EventType;
use agent_protocol::ids::{EventSeq, PluginId, SessionId};
use agent_protocol::permissions::{Capability, CapabilitySet};
use agent_protocol::plugin::{
    PluginCapabilities, PluginContributions, PluginLifecycleState, PluginManifest,
    PluginStateChange, PluginToolSource,
};
use agent_tools_mcp::{
    Backoff, HttpSpec, McpConnector, McpToolProvider, RmcpConnector, StdioSpec, TransportSpec,
};
use serde_json::Value;

/// The plugin API version this build implements.
///
/// Declared rather than negotiated: there is one plugin door in this build, and
/// the version is recorded in every lifecycle event so that a reader comparing
/// two runs has the claim in the log rather than in whichever binary wrote it.
pub const PLUGIN_API: &str = "1";

/// The control name that withdraws a registered plugin.
///
/// A string rather than something a caller constructs: a control is named on the
/// wire, and the name is what a handler advertises. Spelling it once here keeps
/// the writer and the reader from drifting.
pub const PLUGIN_DISABLE: &str = "plugin.disable";

/// The control name that registers a configured plugin again.
pub const PLUGIN_ENABLE: &str = "plugin.enable";

/// The word [`PluginHandle::disable`] writes as the reason for a withdrawal.
///
/// Read back as a standing decision rather than as one event among many, which
/// is why it is spelled here as well as in the kernel: these two are the ends of
/// one agreement, and a change on either side that the other did not hear about
/// would silently stop keeping a plugin off.
const DISABLED: &str = "disabled";

/// One MCP server a run's configuration enabled, in the shape registration
/// needs it.
///
/// Kept by the control rather than re-read from the configuration, because
/// `plugin.enable` has to start the server the configuration already described,
/// and the resolved server is the only thing holding those words at the moment
/// it is asked. A value rather than a TOML section: by the time one is here,
/// which of the two transports it is has been decided.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub name: String,
    pub version: String,
    pub requires: Vec<Capability>,
    pub timeout_ms: Option<u64>,
    pub transport: McpTransport,
}

/// The MCP servers a resolved configuration enables, in name order.
pub fn server_specs(resolved: &ResolvedProfile) -> Vec<ServerSpec> {
    resolved
        .mcp_servers
        .iter()
        .map(|server| {
            let id = format!("mcp.{}", server.name);
            ServerSpec {
                name: server.name.clone(),
                version: resolved
                    .plugin_versions
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "*".to_string()),
                requires: server.requires.clone(),
                timeout_ms: server.timeout_ms,
                transport: server.transport.clone(),
            }
        })
        .collect()
}

/// Whether the log's last word about this plugin is the operator's withdrawal.
fn is_disabled(change: &PluginStateChange) -> bool {
    change.state == PluginLifecycleState::Unregistered && change.reason.as_deref() == Some(DISABLED)
}

/// The plugins this session has turned off, as a later process reads them.
///
/// One sentence decides it: the *last* thing the log says about a plugin is a
/// withdrawal the operator asked for. Anything after it — a registration from
/// [`enable`], or from a run that started before the disable — takes it off the
/// list, which is why the same rule both keeps a plugin out and lets one back
/// in, and why there is no second place for the answer to disagree with itself.
///
/// Only the operator's word counts. A plugin that *failed* was withdrawn too,
/// and a server that was down may be answering again by the next run, so a
/// failure is not a standing decision and keeps nothing off.
pub async fn suppressed(
    store: &dyn EventStore,
    session: &SessionId,
) -> Result<BTreeSet<String>, String> {
    Ok(lifecycle(store, session)
        .await?
        .into_iter()
        .filter(|(_, change)| is_disabled(change))
        .map(|(id, _)| id)
        .collect())
}

/// Why a configured server did not reach the tool surface.
///
/// Three different failures, each with a sentence that names it: a server that
/// did not answer is a fact about the world, a registry that would not take it
/// is a fact about this run, and a working directory the boundary refuses is a
/// fact about the configuration. An operator fixes the first by looking outside
/// and the last by editing the file they wrote.
#[derive(Debug)]
pub enum RegisterError {
    /// The server was started and did not answer.
    Unavailable(String),
    /// It answered, and the registry refused the registration.
    Refused(PluginError),
    /// Its configured working directory is not one the workspace boundary
    /// admits, so the program was never started.
    WorkingDirectory(ResourceGuardError),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(f, "is not available: {error}"),
            Self::Refused(error) => write!(f, "could not be registered: {error}"),
            Self::WorkingDirectory(error) => write!(f, "is not started: {error}"),
        }
    }
}

/// Start one server and register it, in that order.
///
/// **Started before registered**, because a registration is a claim that
/// something is there: putting a name on the surface with no session behind it
/// would make the log say a plugin is loaded while its tools are unreachable.
///
/// The artifact store is the run's, not this server's. An MCP tool can answer
/// with a block that has no text form — an image, an audio clip — and the
/// adapter puts those there and names them in the result, so the model is told
/// something is there rather than handed the word `[image]`.
pub async fn register_server(
    registry: &Arc<PluginRegistry>,
    session: &SessionId,
    spec: &ServerSpec,
    capabilities: &CapabilitySet,
    guard: &ResourceGuard,
    artifacts: &Arc<dyn ArtifactStore>,
) -> Result<PluginHandle, RegisterError> {
    let transport =
        transport_spec(&spec.transport, guard).map_err(RegisterError::WorkingDirectory)?;
    let connector = RmcpConnector::new(spec.name.clone(), transport, Arc::clone(artifacts));
    let provider = Arc::new(McpToolProvider::new(
        spec.name.clone(),
        spec.requires.clone(),
        Arc::clone(&connector) as Arc<dyn McpConnector>,
        spec.timeout_ms,
        Backoff::default(),
    ));
    if let Err(error) = provider.start().await {
        return Err(RegisterError::Unavailable(error.to_string()));
    }
    // The pump outlives this call and holds the provider with it, so a
    // `list_changed` the server sends later still reaches the surface.
    provider.spawn_resync_listener(connector.tool_list_changed());

    let id = format!("mcp.{}", spec.name);
    let manifest = PluginManifest {
        schema_version: agent_protocol::versioning::formats::PLUGIN_MANIFEST,
        id: PluginId::from_string(&id),
        // The version the user trusted, which is what the run's configuration
        // agreed to — not what the server says about itself.
        version: spec.version.clone(),
        plugin_api: PLUGIN_API.to_string(),
        capabilities: PluginCapabilities { requires: spec.requires.clone() },
        contributions: PluginContributions {
            tool_provider: true,
            // The manifest picks the door; the registry still stamps the tools
            // with whatever kind this says, and `Builtin` is not in the
            // vocabulary it may say.
            tool_source: PluginToolSource::Mcp,
            ..PluginContributions::default()
        },
        config_schema: None,
    };

    registry
        .register(session, manifest, PluginRuntime { tool_provider: Some(provider) }, capabilities)
        .await
        .map_err(RegisterError::Refused)
}

/// A resolved server's transport, in the vocabulary the adapter speaks.
///
/// Two crates' words for the same thing, mapped at the one place that knows
/// both: the configuration describes a transport, and the adapter opens it.
///
/// A stdio server's configured `cwd` is a path in a configuration file, and a
/// path in a configuration file is relative to the workspace root — never to the
/// process's working directory, which is what `Command::current_dir` would
/// otherwise resolve a bare name against. It is also held to the same boundary
/// as any path a tool is handed, so a server cannot be launched outside the
/// workspace by configuration alone. The path handed to the adapter is the one
/// the guard admitted, so the boundary and the launch agree.
fn transport_spec(
    transport: &McpTransport,
    guard: &ResourceGuard,
) -> Result<TransportSpec, ResourceGuardError> {
    match transport {
        McpTransport::Stdio { command, args, env, cwd } => Ok(TransportSpec::Stdio(StdioSpec {
            command: command.clone(),
            args: args.clone(),
            env: env.clone(),
            cwd: match cwd {
                Some(cwd) => Some(guard.resolve(cwd)?.display().to_string()),
                None => None,
            },
        })),
        McpTransport::Http { url, headers } => {
            Ok(TransportSpec::Http(HttpSpec { url: url.clone(), headers: headers.clone() }))
        }
    }
}

/// What a run's configuration produced: the handles it kept, and the lines a
/// person needs to read about what it could not do.
pub struct Registered {
    pub handles: HashMap<PluginId, PluginHandle>,
    pub notes: Vec<String>,
}

/// Which plugins this run holds off the surface.
///
/// Two ways to know, and the second is why this is a type rather than a set. A
/// log that cannot be read is a fact about **this run**, not evidence that
/// nothing was turned off: handing [`register_configured`] an empty set there
/// would start every plugin the operator had withdrawn, letting a failed read
/// eat a standing decision. So the unreadable case holds **everything** back,
/// and its note says as much — `plugin enable` is the wrong remedy for a log
/// nobody can read, and a note that sent the operator there would be worse than
/// no note at all.
#[derive(Debug)]
pub enum Suppressed {
    /// The log answered: these are the plugins whose last word is the
    /// operator's withdrawal.
    ByTheLog(BTreeSet<String>),
    /// The log could not be read, so nothing is being taken as enabled.
    Unreadable,
}

/// Start and register the servers this configuration enabled, except the ones
/// this session has turned off.
///
/// A server that cannot be reached is reported and skipped, not fatal: a
/// third-party server being down is not a reason to refuse to start the run, and
/// its tools are simply not among those the model can call.
///
/// A suppressed plugin is skipped *before* it is started. This process has no
/// business spawning a program it is about to withdraw, and a disable that cost
/// a process start and a `plugin/loaded` event every run would be a strange kind
/// of off. [`Suppressed::Unreadable`] takes the same road for every server at
/// once: an answer nobody could read is not an answer.
pub async fn register_configured(
    registry: &Arc<PluginRegistry>,
    session: &SessionId,
    servers: &[ServerSpec],
    capabilities: &CapabilitySet,
    suppressed: &Suppressed,
    guard: &ResourceGuard,
    artifacts: &Arc<dyn ArtifactStore>,
) -> Registered {
    let mut handles = HashMap::new();
    let mut notes = Vec::new();
    for spec in servers {
        let id = format!("mcp.{}", spec.name);
        match suppressed {
            Suppressed::ByTheLog(off) if off.contains(&id) => {
                notes.push(format!(
                    "plugin '{id}' is disabled in session '{}'; `agent plugin enable {id}` \
                     registers it again",
                    session.as_str()
                ));
                continue;
            }
            Suppressed::Unreadable => {
                notes.push(format!(
                    "plugin '{id}' is not started: this session's log could not be read, so nothing \
                     is being taken as enabled"
                ));
                continue;
            }
            Suppressed::ByTheLog(_) => {}
        }
        match register_server(registry, session, spec, capabilities, guard, artifacts).await {
            Ok(handle) => {
                handles.insert(PluginId::from_string(&id), handle);
            }
            Err(error) => notes.push(format!("mcp server '{}' {error}", spec.name)),
        }
    }
    Registered { handles, notes }
}

/// The handles this process holds and the configuration it could register from,
/// with the two controls that change which plugins are on the surface.
///
/// The handles are the reason this type exists. Registration already returns
/// them and already carries a generation guard, so withdrawal is a call rather
/// than a new registry API; what was missing was somebody willing to keep them.
/// A process that discards its handles can only change the plugin surface by
/// being restarted with different configuration — which is what "there is no
/// product entry for this" meant.
///
/// The configuration is kept for the mirror-image reason: `plugin.enable` has to
/// register a plugin this process's assembly deliberately skipped, and the only
/// description of that server is the one the configuration resolved.
///
/// The guard is held for the same reason the servers are: an `enable` starts a
/// server the assembly never started, so it has to resolve that server's
/// configured working directory against the boundary the assembly used — and
/// the boundary is not something this type can rebuild, because it holds the
/// workspace root the run was launched with.
///
/// The artifact store is held for a blunter reason: an `enable` calls the same
/// [`register_server`] the assembly did, and that path has to hand the connector
/// somewhere to put a result block that has no text form. Passing a second store
/// here would mean output from a plugin enabled mid-run lands somewhere the
/// rest of the run cannot find it.
pub struct PluginControl {
    registry: Arc<PluginRegistry>,
    session: SessionId,
    servers: Vec<ServerSpec>,
    capabilities: CapabilitySet,
    guard: ResourceGuard,
    artifacts: Arc<dyn ArtifactStore>,
    handles: Mutex<HashMap<PluginId, Arc<PluginHandle>>>,
}

impl PluginControl {
    /// Build one over the handles registration produced and the servers the
    /// configuration named.
    pub fn new(
        registry: Arc<PluginRegistry>,
        session: SessionId,
        servers: Vec<ServerSpec>,
        capabilities: CapabilitySet,
        guard: ResourceGuard,
        artifacts: Arc<dyn ArtifactStore>,
        handles: HashMap<PluginId, PluginHandle>,
    ) -> Self {
        Self {
            registry,
            session,
            servers,
            capabilities,
            guard,
            artifacts,
            handles: Mutex::new(
                handles.into_iter().map(|(id, handle)| (id, Arc::new(handle))).collect(),
            ),
        }
    }

    /// The handles, with a poisoned lock read through rather than panicked on.
    ///
    /// Nothing here can leave the map half-written, and the alternative is a
    /// turn boundary that dies because some other thread died earlier.
    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<PluginId, Arc<PluginHandle>>> {
        self.handles.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The ids this process could withdraw, in a stable order.
    ///
    /// Sorted, because it goes into a refusal a person reads: the answer to
    /// "which plugin?" should not depend on hash order.
    fn registered(&self) -> Vec<String> {
        let handles = self.held();
        let ids: BTreeSet<&str> = handles.keys().map(PluginId::as_str).collect();
        ids.into_iter().map(str::to_string).collect()
    }

    /// The plugin ids this run's configuration enables, in the same order.
    fn configured(&self) -> Vec<String> {
        let ids: BTreeSet<String> =
            self.servers.iter().map(|spec| format!("mcp.{}", spec.name)).collect();
        ids.into_iter().collect()
    }

    /// What this process has, for a refusal to name.
    ///
    /// Configured-but-absent ids are said too. An operator who typed a name the
    /// configuration knows is asking why it is not there, and answering "no
    /// plugins" would send them to read a configuration that is not the problem
    /// — while the session log, which is, is exactly what `plugin list` reads.
    fn what_is_registered(&self) -> String {
        let known = self.registered();
        let said = if known.is_empty() {
            "this process registered no plugins".to_string()
        } else {
            format!("this process registered: {}", known.join(", "))
        };
        let absent: Vec<String> =
            self.configured().into_iter().filter(|id| !known.contains(id)).collect();
        if absent.is_empty() {
            said
        } else {
            format!(
                "{said}; the configuration enables {} and this process did not register it \
                 (`agent plugin list` says why)",
                absent.join(", ")
            )
        }
    }

    fn what_is_configured(&self) -> String {
        let known = self.configured();
        if known.is_empty() {
            "this run's configuration enables no plugins".to_string()
        } else {
            format!("this run's configuration enables: {}", known.join(", "))
        }
    }

    /// Withdraw a registered plugin.
    ///
    /// The handle is cloned out of the map before the await rather than borrowed
    /// across it: a lock held over an await is a lock held over however long the
    /// registry takes to write, and the entry is only removed once the
    /// withdrawal actually happened.
    ///
    /// The return type is `Result` for the same reason [`enable`]'s is: a
    /// *refusal* is a decision this process is entitled to make about what it
    /// will do, while a withdrawal the log would not take is a failure that must
    /// reach the exit code instead of being dressed up as a decision. Folding
    /// the two together — as this arm once did — reads a store that is down as
    /// "the operator asked for something unreasonable", which is exactly the
    /// kind of silence that lets a broken run look clean. (YA-36)
    ///
    /// [`enable`]: Self::enable
    async fn disable(&self, id: &str) -> Result<ControlOutcome, ControlError> {
        let plugin_id = PluginId::from_string(id);
        let handle = self.held().get(&plugin_id).map(Arc::clone);
        let Some(handle) = handle else {
            // Refused, not an error: an operator naming a plugin this process
            // never registered is a question with an answer, and the answer is
            // worth reading — it lists what *was* registered, so the misspelling
            // is visible. Leaving it pending instead would make the command look
            // accepted while nothing ever happened.
            return Ok(ControlOutcome::Refused {
                reason: format!("no such plugin '{id}'; {}", self.what_is_registered()),
            });
        };

        match handle.disable().await {
            Ok(change) => {
                self.held().remove(&plugin_id);
                Ok(ControlOutcome::Applied {
                    detail: format!(
                        "plugin '{id}' unregistered at generation {}",
                        change.generation
                    ),
                })
            }
            // Could not, rather than will not. A store that refused the write
            // and a registry that is gone are both the machinery failing under
            // the command: nothing was decided and nothing is left to decide, so
            // the run has to come out with a non-zero answer. (YA-36)
            Err(error @ (PluginError::Store(_) | PluginError::RegistryGone)) => {
                Err(ControlError::Failed {
                    control: PLUGIN_DISABLE.to_string(),
                    message: format!("plugin '{id}' {error}"),
                })
            }
            // The registry's own words for the rest. A stale handle is a
            // decision about a registration that has already moved on, and
            // saying so is not the same as failing — the operator needs to know
            // which, and a refusal carries that answer to them.
            Err(error) => Ok(ControlOutcome::Refused { reason: error.to_string() }),
        }
    }

    /// Register a plugin the configuration enables.
    ///
    /// The server is started here, which is the one thing this control does that
    /// the whole process did not already do at assembly — and it is why it holds
    /// the resolved servers. A name the configuration does not have is refused
    /// by name rather than attempted: there is no way to make up a transport for
    /// a server nobody described.
    async fn enable(&self, id: &str) -> Result<ControlOutcome, ControlError> {
        let plugin_id = PluginId::from_string(id);
        if self.held().contains_key(&plugin_id) {
            return Ok(ControlOutcome::Refused {
                reason: format!("plugin '{id}' is already registered in this process"),
            });
        }
        let Some(spec) = self.servers.iter().find(|spec| format!("mcp.{}", spec.name) == id) else {
            return Ok(ControlOutcome::Refused {
                reason: format!("no such plugin '{id}'; {}", self.what_is_configured()),
            });
        };

        match register_server(
            &self.registry,
            &self.session,
            spec,
            &self.capabilities,
            &self.guard,
            &self.artifacts,
        )
        .await
        {
            Ok(handle) => {
                let generation = handle.generation();
                self.held().insert(plugin_id, Arc::new(handle));
                Ok(ControlOutcome::Applied {
                    detail: format!("plugin '{id}' registered at generation {generation}"),
                })
            }
            // Tried and could not, rather than will not: the configuration
            // describes a server that did not answer. That is a failure rather
            // than a refusal because there is nothing to decide — the operator
            // may well be able to do something about it, and the reason has to
            // survive to them either way.
            Err(error) => Err(ControlError::Failed {
                control: PLUGIN_ENABLE.to_string(),
                message: format!("plugin '{id}' {error}"),
            }),
        }
    }
}

#[async_trait::async_trait]
impl ControlHandler for PluginControl {
    fn name(&self) -> &str {
        "plugins"
    }

    fn handles(&self) -> Vec<String> {
        vec![PLUGIN_DISABLE.to_string(), PLUGIN_ENABLE.to_string()]
    }

    async fn apply(&self, request: ControlRequest) -> Result<ControlOutcome, ControlError> {
        let Some(id) = request.payload.get("plugin_id").and_then(Value::as_str) else {
            return Ok(ControlOutcome::Refused {
                reason: "the control carries no 'plugin_id'".to_string(),
            });
        };

        // Routed by the name the item carries. A name that reached this far
        // without a branch is refused rather than guessed at: `handles()` is
        // what the scheduler routes on, so a name here that nobody claims is a
        // disagreement between the two, and refusing is what makes it visible.
        Ok(match request.control.as_str() {
            PLUGIN_DISABLE => self.disable(id).await?,
            PLUGIN_ENABLE => self.enable(id).await?,
            other => ControlOutcome::Refused {
                reason: format!("'{other}' is not a control this handler knows"),
            },
        })
    }
}

/// What the log says about each plugin id: its most recent lifecycle event.
///
/// The *most recent*, not all of them. A plugin registered, withdrawn and
/// registered again has one current answer, and a list showing three events
/// would leave the reader to work out which one is still true.
async fn lifecycle(
    store: &dyn EventStore,
    session: &SessionId,
) -> Result<BTreeMap<String, PluginStateChange>, String> {
    let events =
        store.read_stream(session, EventSeq::ZERO, usize::MAX).await.map_err(|e| e.to_string())?;

    let mut current = BTreeMap::new();
    for stored in events {
        let kind = stored.event.event_type.as_str();
        if !matches!(
            kind,
            EventType::PLUGIN_LOADED | EventType::PLUGIN_UNLOADED | EventType::PLUGIN_FAILED
        ) {
            continue;
        }
        // A payload this version cannot read is data, not a failure:
        // a newer writer's lifecycle event is skipped, not fatal to the list.
        let Ok(change) = serde_json::from_value::<PluginStateChange>(stored.event.payload) else {
            continue;
        };
        current.insert(change.plugin_id.as_str().to_string(), change);
    }
    Ok(current)
}

/// The note every listing ends with, because it is what this command cannot see.
///
/// The honest boundary of the answer, printed rather than assumed: a plugin
/// recorded `loaded` in the log may have been withdrawn by a process whose
/// change has not been written yet, and a process running right now may hold a
/// surface this file-based view knows nothing about.
const SCOPE_NOTE: &str = "read from configuration and the session log; another running process's live registry is not \
     visible from here";

/// The MCP plugins a resolved configuration enables, as `(id, version)` pairs.
///
/// The mapping from "a profile enables this server" to "this plugin id" lives
/// here rather than inside [`list`], so that the listing can be tested without
/// building a whole resolved configuration — and so that the id's spelling has
/// one home, next to the control that withdraws it.
pub fn configured_plugins(resolved: &ResolvedProfile) -> Vec<(String, String)> {
    resolved
        .mcp_servers
        .iter()
        .map(|server| {
            let id = format!("mcp.{}", server.name);
            let version =
                resolved.plugin_versions.get(&id).cloned().unwrap_or_else(|| "*".to_string());
            (id, version)
        })
        .collect()
}

/// `agent plugin list`: what is configured, what the log says, and what is
/// still waiting for a boundary.
///
/// Read-only, and deliberately does not start any MCP server: a management
/// command that had to bring up the very plugin it is reporting on would be
/// unable to report on one that is down.
pub async fn list(
    configured: &[(String, String)],
    store: &std::sync::Arc<dyn EventStore>,
    session: &SessionId,
    json: bool,
) -> Result<CliOutput, String> {
    let log = lifecycle(store.as_ref(), session).await?;
    let inbox = Inbox::new(store.clone());
    let pending = inbox.controls(session).await.map_err(|e| e.to_string())?;

    if json {
        let value = serde_json::json!({
            "configured": configured
                .iter()
                .map(|(id, version)| serde_json::json!({ "plugin_id": id, "version": version }))
                .collect::<Vec<_>>(),
            "log": log
                .values()
                .map(|change| serde_json::json!({
                    "plugin_id": change.plugin_id.as_str(),
                    "version": change.version,
                    "state": change.state,
                    "generation": change.generation,
                    "reason": change.reason,
                }))
                .collect::<Vec<_>>(),
            "kept_off": log
                .values()
                .filter(|change| is_disabled(change))
                .map(|change| {
                    serde_json::json!({
                        "plugin_id": change.plugin_id.as_str(),
                        "enable_with": format!("agent plugin enable {}", change.plugin_id.as_str()),
                    })
                })
                .collect::<Vec<_>>(),
            "pending": pending
                .iter()
                .map(|item| serde_json::json!({
                    "item_id": item.item_id.as_str(),
                    "control": item.payload.get("control"),
                    "plugin_id": item.payload.get("plugin_id"),
                }))
                .collect::<Vec<_>>(),
            "note": SCOPE_NOTE,
        });
        return Ok(CliOutput::Json(json_output(&value)?));
    }

    let mut lines = Vec::new();
    lines.push("configured (this profile enables):".to_string());
    if configured.is_empty() {
        lines.push("  none".to_string());
    }
    for (id, version) in configured {
        lines.push(format!("  {id}  (version {version})"));
    }
    lines.push("in the session log:".to_string());
    if log.is_empty() {
        lines.push("  nothing recorded".to_string());
    }
    for change in log.values() {
        let reason = change.reason.as_ref().map(|r| format!("  ({r})")).unwrap_or_default();
        lines.push(format!(
            "  {}  {:?}  generation {}{reason}",
            change.plugin_id.as_str(),
            change.state,
            change.generation
        ));
    }
    lines.push("waiting for the next turn boundary:".to_string());
    if pending.is_empty() {
        lines.push("  none".to_string());
    }
    for item in &pending {
        let control = item.payload.get("control").and_then(Value::as_str).unwrap_or("?");
        let target = item.payload.get("plugin_id").and_then(Value::as_str).unwrap_or("?");
        lines.push(format!("  {control} {target}  (item {})", item.item_id.as_str()));
    }
    // The answer to the operator's next question, which the sections above only
    // imply: "and will it come back on its own?" — no, and here is the command
    // that brings it back.
    for change in log.values().filter(|change| is_disabled(change)) {
        let id = change.plugin_id.as_str();
        lines.push(format!(
            "kept off: '{id}' is not registered by later runs; `agent plugin enable {id}` \
             registers it again"
        ));
    }
    lines.push(SCOPE_NOTE.to_string());
    Ok(CliOutput::Text(lines.join("\n")))
}

/// `agent plugin disable <id>`: ask for a withdrawal at the next safe boundary.
///
/// It queues and returns. It does not withdraw anything itself, and that is not
/// a shortcut: this process is not the one running the session, so its own
/// plugin surface is not the one the operator is looking at. The record of what
/// happened — `plugin/unloaded`, and which handler applied it — is written by
/// whichever process next reaches a boundary, next to the turn it precedes.
pub async fn disable(
    inbox: &Inbox,
    session: &SessionId,
    plugin_id: &str,
    reason: Option<&str>,
    json: bool,
) -> Result<CliOutput, String> {
    let mut fields = serde_json::Map::new();
    fields.insert("plugin_id".to_string(), Value::String(plugin_id.to_string()));
    if let Some(reason) = reason {
        fields.insert("reason".to_string(), Value::String(reason.to_string()));
    }
    let item = inbox
        .control(session, PLUGIN_DISABLE, Value::Object(fields))
        .await
        .map_err(|e| e.to_string())?;

    if json {
        let value = serde_json::json!({
            "queued": {
                "item_id": item.item_id.as_str(),
                "control": PLUGIN_DISABLE,
                "plugin_id": plugin_id,
                "applies_at": "the next turn boundary",
            },
        });
        return Ok(CliOutput::Json(json_output(&value)?));
    }
    Ok(CliOutput::Text(format!(
        "queued: plugin '{plugin_id}' will be disabled at the next turn boundary (item {})\n\
         this command only asks; the process driving the session applies it",
        item.item_id.as_str()
    )))
}

/// `agent plugin enable <id>`: ask for a registration at the next safe boundary.
///
/// The mirror of [`disable`], down to the road it takes — and the reason it can
/// be a mirror rather than a special case: a disable that outlives its process is
/// recorded in the log as a withdrawal, so undoing it is a registration, which is
/// something a running process already knows how to do. Nothing has to be
/// un-suppressed anywhere, and a build that never ran this command is not less
/// correct than one that did.
///
/// It does not check whether the plugin is disabled. The check belongs to the
/// process that will act: this one cannot see whether the disable was already
/// undone, and a command that refused to queue because of a guess would be worse
/// than one that queues a request the boundary can answer.
pub async fn enable(
    inbox: &Inbox,
    session: &SessionId,
    plugin_id: &str,
    json: bool,
) -> Result<CliOutput, String> {
    let mut fields = serde_json::Map::new();
    fields.insert("plugin_id".to_string(), Value::String(plugin_id.to_string()));
    let item = inbox
        .control(session, PLUGIN_ENABLE, Value::Object(fields))
        .await
        .map_err(|e| e.to_string())?;

    if json {
        let value = serde_json::json!({
            "queued": {
                "item_id": item.item_id.as_str(),
                "control": PLUGIN_ENABLE,
                "plugin_id": plugin_id,
                "applies_at": "the next turn boundary",
            },
        });
        return Ok(CliOutput::Json(json_output(&value)?));
    }
    Ok(CliOutput::Text(format!(
        "queued: plugin '{plugin_id}' will be registered at the next turn boundary (item {})\n\
         this command only asks; the process driving the session applies it",
        item.item_id.as_str()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_kernel::plugin::{PluginRegistry, PluginRuntime};
    use agent_protocol::ids::InboxItemId;
    use agent_protocol::plugin::{
        PluginCapabilities, PluginContributions, PluginLifecycleState, PluginManifest,
    };
    use agent_testkit::FakeEventStore;
    use std::sync::Arc;

    fn manifest(id: &str) -> PluginManifest {
        PluginManifest {
            schema_version: agent_protocol::versioning::formats::PLUGIN_MANIFEST,
            id: PluginId::from_string(id),
            version: "1.0.0".to_string(),
            plugin_api: "1".to_string(),
            capabilities: PluginCapabilities::default(),
            contributions: PluginContributions { observer: true, ..PluginContributions::default() },
            config_schema: None,
        }
    }

    /// One server a run's configuration could have enabled, at its shape.
    ///
    /// The transport names a program that is never started: these tests are
    /// about which plugin id the control is willing to reach for, and the one
    /// that does start a process is the acceptance suite's business.
    fn spec(name: &str) -> ServerSpec {
        ServerSpec {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            requires: Vec::new(),
            timeout_ms: None,
            transport: McpTransport::Stdio {
                command: "unused-by-this-test".to_string(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: None,
            },
        }
    }

    /// A boundary rooted at a directory nothing here reaches into.
    ///
    /// These tests are about which plugin id a control is willing to reach for,
    /// and no transport written here names a real program or a working
    /// directory. The guard is what the signatures take, not a path any of them
    /// resolves.
    fn guard() -> ResourceGuard {
        ResourceGuard::new(agent_protocol::permissions::Workspace {
            workspace_root: std::env::temp_dir().display().to_string(),
            writable_roots: Vec::new(),
            external_directories: Vec::new(),
        })
    }

    /// A store for the registration path. Nothing in these tests offloads a
    /// content block, but the path has to be handed one all the same.
    fn artifacts() -> Arc<dyn ArtifactStore> {
        Arc::new(agent_testkit::InMemoryArtifactStore::default())
    }

    /// A store that records normally until it is told to refuse, so a test can
    /// put a plugin on the surface and then take the store out from under a
    /// withdrawal of it.
    struct RefusingStore {
        inner: Arc<FakeEventStore>,
        refuse: std::sync::atomic::AtomicBool,
    }

    impl RefusingStore {
        fn new(inner: Arc<FakeEventStore>) -> Self {
            Self { inner, refuse: std::sync::atomic::AtomicBool::new(false) }
        }
    }

    #[async_trait::async_trait]
    impl EventStore for RefusingStore {
        async fn append(
            &self,
            session_id: &SessionId,
            expected_next_seq: EventSeq,
            drafts: Vec<agent_protocol::events::EventDraft>,
        ) -> Result<
            agent_ports::event_store::AppendReceipt,
            agent_ports::event_store::EventStoreError,
        > {
            if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(agent_ports::event_store::EventStoreError::SequenceConflict {
                    expected: expected_next_seq,
                    actual: expected_next_seq,
                });
            }
            self.inner.append(session_id, expected_next_seq, drafts).await
        }

        async fn lease(
            &self,
            session_id: &SessionId,
        ) -> Result<
            Box<dyn agent_ports::event_store::SessionLease>,
            agent_ports::event_store::EventStoreError,
        > {
            self.inner.lease(session_id).await
        }

        async fn driver_lease(
            &self,
            session_id: &SessionId,
        ) -> Result<
            Box<dyn agent_ports::event_store::SessionLease>,
            agent_ports::event_store::EventStoreError,
        > {
            self.inner.driver_lease(session_id).await
        }

        async fn flush(
            &self,
            session_id: &SessionId,
        ) -> Result<agent_ports::event_store::FlushReceipt, agent_ports::event_store::EventStoreError>
        {
            self.inner.flush(session_id).await
        }

        async fn read_stream(
            &self,
            session_id: &SessionId,
            from_seq: EventSeq,
            limit: usize,
        ) -> Result<
            Vec<agent_protocol::events::StoredEvent>,
            agent_ports::event_store::EventStoreError,
        > {
            self.inner.read_stream(session_id, from_seq, limit).await
        }

        async fn subscribe(
            &self,
            session_id: &SessionId,
            from_seq: EventSeq,
        ) -> Result<
            Box<dyn agent_ports::event_store::EventSubscription>,
            agent_ports::event_store::EventStoreError,
        > {
            self.inner.subscribe(session_id, from_seq).await
        }
    }

    /// A registry with one plugin registered, and the control over its handle.
    async fn one_plugin(
        id: &str,
    ) -> (Arc<PluginRegistry>, Arc<FakeEventStore>, SessionId, PluginControl) {
        let store = Arc::new(FakeEventStore::new());
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let session = SessionId::from_string("plugin-control");
        let handle = registry
            .register(
                &session,
                manifest(id),
                PluginRuntime { tool_provider: None },
                &Default::default(),
            )
            .await
            .expect("it registers");

        let mut handles = HashMap::new();
        handles.insert(PluginId::from_string(id), handle);
        let server = id.strip_prefix("mcp.").unwrap_or(id);
        let control = PluginControl::new(
            registry.clone(),
            session.clone(),
            vec![spec(server)],
            Default::default(),
            guard(),
            artifacts(),
            handles,
        );
        (registry, store, session, control)
    }

    fn request(plugin_id: &str) -> ControlRequest {
        ControlRequest {
            session_id: SessionId::from_string("plugin-control"),
            item_id: InboxItemId::from_string("item-1"),
            control: PLUGIN_DISABLE.to_string(),
            payload: serde_json::json!({ "control": PLUGIN_DISABLE, "plugin_id": plugin_id }),
        }
    }

    fn enable_request(plugin_id: &str) -> ControlRequest {
        ControlRequest {
            session_id: SessionId::from_string("plugin-control"),
            item_id: InboxItemId::from_string("item-2"),
            control: PLUGIN_ENABLE.to_string(),
            payload: serde_json::json!({ "control": PLUGIN_ENABLE, "plugin_id": plugin_id }),
        }
    }

    /// The plugins this session has turned off, as a later process reads them.
    async fn off(
        store: &dyn EventStore,
        session: &SessionId,
    ) -> std::collections::BTreeSet<String> {
        suppressed(store, session).await.expect("the log is readable")
    }

    #[tokio::test]
    async fn the_control_advertises_exactly_the_names_the_commands_write() {
        // The writer and the reader are different processes; this is the one
        // place both names are in scope at once.
        let (_, _, _, control) = one_plugin("mcp.one").await;
        assert_eq!(control.handles(), vec![PLUGIN_DISABLE.to_string(), PLUGIN_ENABLE.to_string()]);
    }

    #[tokio::test]
    async fn a_disable_written_to_the_log_keeps_the_plugin_off_in_later_processes() {
        // The whole of the durability rule. The processes that will read this
        // were not running when the operator asked, so what the log says has to
        // be enough on its own — there is nowhere else for them to look.
        let (_registry, store, session, control) = one_plugin("mcp.one").await;
        assert!(off(store.as_ref(), &session).await.is_empty(), "nothing is off yet");

        control.apply(request("mcp.one")).await.expect("applying is answerable");

        let off = off(store.as_ref(), &session).await;
        assert!(off.contains("mcp.one"), "the next process would register it again: {off:?}");
    }

    #[tokio::test]
    async fn a_log_that_cannot_be_read_registers_nothing() {
        // The unreadable half of the same rule, and the reason [`Suppressed`] is
        // a type rather than a bare set: an empty set here would say "nothing
        // was withdrawn", which is the one thing a failed read cannot say. Every
        // configured plugin waits instead, and the note says why.
        let store = Arc::new(FakeEventStore::new());
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let session = SessionId::from_string("plugin-control");

        let registered = register_configured(
            &registry,
            &session,
            &[spec("one"), spec("two")],
            &Default::default(),
            &Suppressed::Unreadable,
            &guard(),
            &artifacts(),
        )
        .await;

        assert!(
            registered.handles.is_empty(),
            "a plugin was started off a log nobody could read: {:?}",
            registered.handles
        );
        assert_eq!(registered.notes.len(), 2, "{:?}", registered.notes);
        for note in &registered.notes {
            assert!(note.contains("not started"), "{note}");
            assert!(note.contains("could not be read"), "{note}");
            // The remedy the note must not offer: nothing was withdrawn by hand,
            // so enabling the plugin would answer a question nobody asked.
            assert!(!note.contains("plugin enable"), "{note}");
        }

        // And nothing reached the log while the answer was unknown — no
        // `plugin/loaded`, no `plugin/failed`.
        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        assert!(events.is_empty(), "the surface moved off an unreadable log: {events:?}");
    }

    #[tokio::test]
    async fn a_plugin_registered_again_is_no_longer_off() {
        // `plugin enable` ends in a registration, so the same rule that keeps a
        // disabled plugin off lets a registered one back in. One rule, both
        // ways, and nothing to keep in sync.
        let (registry, store, session, control) = one_plugin("mcp.one").await;
        control.apply(request("mcp.one")).await.expect("it is withdrawn");
        assert!(off(store.as_ref(), &session).await.contains("mcp.one"));

        registry
            .register(
                &session,
                manifest("mcp.one"),
                PluginRuntime { tool_provider: None },
                &Default::default(),
            )
            .await
            .expect("it registers again");

        let off = off(store.as_ref(), &session).await;
        assert!(off.is_empty(), "a plugin that is registered is not off: {off:?}");
    }

    #[tokio::test]
    async fn a_plugin_that_failed_is_not_kept_off() {
        // Only the operator's word suppresses. A plugin that stopped working
        // may be working again by the next process, and a build that decided on
        // its own to keep it out would be a decision nobody can undo from the
        // outside — which is the thing that makes a disable worth having a
        // release for in the first place.
        let (registry, store, session, _control) = one_plugin("mcp.one").await;
        let handle = registry
            .register(
                &session,
                manifest("mcp.two"),
                PluginRuntime { tool_provider: None },
                &Default::default(),
            )
            .await
            .expect("it registers");
        handle.fail("it stopped answering").await.expect("it fails");

        let off = off(store.as_ref(), &session).await;
        assert!(!off.contains("mcp.two"), "a failure is not an operator's decision: {off:?}");
    }

    #[tokio::test]
    async fn an_enable_is_refused_by_name_when_the_configuration_does_not_have_it() {
        // Two different "no", and the answer has to tell them apart: a plugin
        // the configuration enables and this process could not start, and a
        // name the configuration never mentions — the second is a typo, and
        // saying what *is* configured is what makes it visible.
        let (_, _, _, control) = one_plugin("mcp.one").await;
        let outcome = control.apply(enable_request("mcp.two")).await.expect("answerable");

        let ControlOutcome::Refused { reason } = outcome else {
            panic!("a plugin the configuration does not have was registered");
        };
        assert!(reason.contains("no such plugin 'mcp.two'"), "{reason}");
        assert!(reason.contains("mcp.one"), "it names what is configured instead: {reason}");
    }

    #[tokio::test]
    async fn an_enable_for_something_already_registered_is_refused() {
        let (registry, _, _, control) = one_plugin("mcp.one").await;
        let outcome = control.apply(enable_request("mcp.one")).await.expect("answerable");

        let ControlOutcome::Refused { reason } = outcome else {
            panic!("enabling what is already on was treated as a change");
        };
        assert!(reason.contains("already registered"), "{reason}");
        assert!(registry.is_registered(&PluginId::from_string("mcp.one")));
    }

    #[tokio::test]
    async fn a_disable_withdraws_the_registration_it_names() {
        let (registry, store, session, control) = one_plugin("mcp.one").await;

        let outcome = control.apply(request("mcp.one")).await.expect("applying is answerable");

        assert!(
            matches!(outcome, ControlOutcome::Applied { .. }),
            "an existing plugin was not withdrawn: {outcome:?}"
        );
        assert!(!registry.is_registered(&PluginId::from_string("mcp.one")));

        // And the withdrawal is a fact in the log, with the operator's word for
        // it — the same path the configuration used, so a reader cannot tell
        // which one of the two turned it off without reading the reason.
        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        let unloaded = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::PLUGIN_UNLOADED)
            .expect("the withdrawal is recorded");
        assert_eq!(unloaded.event.payload["reason"], "disabled");
        assert_eq!(unloaded.event.payload["state"], "unregistered");
    }

    #[tokio::test]
    async fn a_disable_of_something_that_is_not_registered_is_refused_by_name() {
        let (registry, store, session, control) = one_plugin("mcp.one").await;

        let outcome = control.apply(request("mcp.two")).await.expect("applying is answerable");

        let ControlOutcome::Refused { reason } = outcome else {
            panic!("a plugin this process never registered was treated as withdrawable");
        };
        assert!(reason.contains("no such plugin 'mcp.two'"), "{reason}");
        assert!(reason.contains("mcp.one"), "the refusal lists what is registered: {reason}");

        // Nothing changed: a refusal is a decision, not a partial attempt.
        assert!(registry.is_registered(&PluginId::from_string("mcp.one")));
        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        assert!(
            !events.iter().any(|e| e.event.event_type.as_str() == EventType::PLUGIN_UNLOADED),
            "a refusal withdrew something"
        );
    }

    #[tokio::test]
    async fn a_second_disable_through_the_same_handle_is_refused_rather_than_an_error() {
        // The registry's generation guard is what makes this safe to run at
        // every boundary: a control that was already applied is answered, not
        // crashed on. The reason has to survive to the operator.
        //
        // The registry is held, not dropped: a handle is a `Weak` on it, so the
        // thing that would answer "that generation is stale" is the very thing
        // that is gone. In the assembled binary the tool surface holds it.
        let (_registry, _, _, control) = one_plugin("mcp.one").await;

        let first = control.apply(request("mcp.one")).await.expect("answerable");
        assert!(matches!(first, ControlOutcome::Applied { .. }));

        let second = control.apply(request("mcp.one")).await.expect("still answerable");
        let ControlOutcome::Refused { reason } = second else {
            panic!("a stale handle withdrew a registration that is not its own");
        };
        assert!(reason.contains("mcp.one"), "{reason}");
    }

    #[tokio::test]
    async fn a_withdrawal_the_log_will_not_take_is_a_failure_not_a_refusal() {
        // A store that is down is not a decision about the command. Reading it
        // as a refusal would answer the operator with the registry's words and
        // leave the run exiting zero on a withdrawal that never happened — the
        // silence that lets a broken run look clean. (YA-36)
        let inner = Arc::new(FakeEventStore::new());
        let store = Arc::new(RefusingStore::new(inner.clone()));
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let session = SessionId::from_string("plugin-control");
        let handle = registry
            .register(
                &session,
                manifest("mcp.one"),
                PluginRuntime { tool_provider: None },
                &Default::default(),
            )
            .await
            .expect("it registers");
        let mut handles = HashMap::new();
        handles.insert(PluginId::from_string("mcp.one"), handle);
        let control = PluginControl::new(
            registry.clone(),
            session.clone(),
            vec![spec("one")],
            Default::default(),
            guard(),
            artifacts(),
            handles,
        );

        store.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = control
            .apply(request("mcp.one"))
            .await
            .expect_err("a store that is down is a failure, not a refusal");
        assert!(matches!(error, ControlError::Failed { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_disable_names_the_plugin_it_applied() {
        // The record has to say which change happened, not merely that one did:
        // the detail line goes next to the claim in the log.
        let (_registry, _, _, control) = one_plugin("mcp.one").await;
        let ControlOutcome::Applied { detail } = control.apply(request("mcp.one")).await.unwrap()
        else {
            panic!("expected an applied outcome");
        };
        assert!(detail.contains("mcp.one"), "{detail}");
        assert!(detail.contains("generation"), "{detail}");
    }

    #[tokio::test]
    async fn a_list_says_what_it_cannot_see() {
        // The scope note is the output's honesty about its own boundary: three
        // durable sources, and not the live registry of a process that is
        // running right now.
        let store: Arc<dyn EventStore> = Arc::new(FakeEventStore::new());
        let session = SessionId::from_string("plugin-control");

        let out = list(&[], &store, &session, false).await.expect("listing works");
        let CliOutput::Text(text) = out else { panic!("expected text") };
        assert!(text.contains("not visible from here"), "{text}");
        assert!(text.contains("configured"), "{text}");
    }

    #[tokio::test]
    async fn a_list_reports_the_last_lifecycle_event_not_every_one() {
        let store = Arc::new(FakeEventStore::new());
        let session = SessionId::from_string("plugin-control");
        let registry = Arc::new(PluginRegistry::new(store.clone()));

        let handle = registry
            .register(
                &session,
                manifest("mcp.one"),
                PluginRuntime { tool_provider: None },
                &Default::default(),
            )
            .await
            .expect("it registers");
        handle.disable().await.expect("it withdraws");

        let store: Arc<dyn EventStore> = store;
        let CliOutput::Json(text) = list(&[], &store, &session, true).await.unwrap() else {
            panic!("expected json");
        };
        assert!(text.as_str().starts_with("{\n"), "one pretty object: {text}");
        let value: Value = serde_json::from_str(text.as_str()).expect("json");
        let log = value["log"].as_array().expect("a log array");

        assert_eq!(log.len(), 1, "one plugin, one current answer: {log:?}");
        assert_eq!(log[0]["state"], "unregistered");
        assert_eq!(log[0]["reason"], "disabled");
        assert_eq!(log[0]["plugin_id"], "mcp.one");
    }

    #[tokio::test]
    async fn a_disabled_plugin_is_listed_configured_and_withdrawn_at_once() {
        // The two surfaces disagree on purpose and the output keeps them apart:
        // the configuration still enables it (a file did not change), and the
        // log says a process withdrew it. Merging the two would hide exactly the
        // fact an operator is asking about.
        let store = Arc::new(FakeEventStore::new());
        let session = SessionId::from_string("plugin-control");
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let handle = registry
            .register(
                &session,
                manifest("mcp.one"),
                PluginRuntime { tool_provider: None },
                &Default::default(),
            )
            .await
            .unwrap();
        handle.disable().await.unwrap();

        let store: Arc<dyn EventStore> = store;
        let configured = vec![("mcp.one".to_string(), "1.0.0".to_string())];
        let CliOutput::Json(text) = list(&configured, &store, &session, true).await.unwrap() else {
            panic!("expected json");
        };
        assert!(text.as_str().starts_with("{\n"), "one pretty object: {text}");
        let value: Value = serde_json::from_str(text.as_str()).unwrap();
        assert_eq!(value["configured"][0]["plugin_id"], "mcp.one");
        assert_eq!(value["log"][0]["plugin_id"], "mcp.one");
        assert_eq!(value["log"][0]["state"], "unregistered");
    }

    #[tokio::test]
    async fn a_list_says_a_disabled_plugin_stays_off_for_later_runs() {
        // The operator's next question after "why is it not there?" is "does it
        // come back?", and the answer is what the note is for.
        let store = Arc::new(FakeEventStore::new());
        let session = SessionId::from_string("plugin-control");
        let registry = Arc::new(PluginRegistry::new(store.clone()));
        let handle = registry
            .register(
                &session,
                manifest("mcp.one"),
                PluginRuntime { tool_provider: None },
                &Default::default(),
            )
            .await
            .unwrap();
        handle.disable().await.unwrap();

        let store: Arc<dyn EventStore> = store;
        let configured = vec![("mcp.one".to_string(), "1.0.0".to_string())];
        let CliOutput::Text(text) = list(&configured, &store, &session, false).await.unwrap()
        else {
            panic!("expected text");
        };
        assert!(text.contains("kept off"), "{text}");
        assert!(
            text.contains("agent plugin enable mcp.one"),
            "the note says how to undo it: {text}"
        );
    }

    #[tokio::test]
    async fn an_enable_queues_an_item_under_its_own_name() {
        // Same road as a disable and a different destination: the item says
        // which change is wanted, and nothing here registers anything — this
        // process is not the one running the session.
        let store = Arc::new(FakeEventStore::new());
        let session = SessionId::from_string("plugin-control");
        let inbox = Inbox::new(store.clone());

        let CliOutput::Text(text) = enable(&inbox, &session, "mcp.one", false).await.unwrap()
        else {
            panic!("expected text");
        };
        assert!(text.contains("next turn boundary"), "{text}");

        let queued = inbox.controls(&session).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].payload["control"], PLUGIN_ENABLE);
        assert_eq!(queued[0].payload["plugin_id"], "mcp.one");
    }

    #[tokio::test]
    async fn a_disable_queues_an_item_and_withdraws_nothing_here() {
        let store = Arc::new(FakeEventStore::new());
        let session = SessionId::from_string("plugin-control");
        let inbox = Inbox::new(store.clone());

        let CliOutput::Text(text) =
            disable(&inbox, &session, "mcp.one", Some("not needed"), false).await.unwrap()
        else {
            panic!("expected text");
        };
        assert!(text.contains("next turn boundary"), "{text}");

        let queued = inbox.controls(&session).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].payload["control"], PLUGIN_DISABLE);
        assert_eq!(queued[0].payload["plugin_id"], "mcp.one");
        assert_eq!(queued[0].payload["reason"], "not needed");

        // The command wrote the queue item and nothing else. The session's own
        // start rides along with the first append into an empty log
        // (`Inbox::append`), so it is the one other event that can be here.
        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        let types: Vec<&str> = events.iter().map(|event| event.event.event_type.as_str()).collect();
        assert_eq!(
            types,
            ["session/started", "inbox/appended"],
            "a queueing command wrote more than the queue: {types:?}"
        );
    }

    #[test]
    fn the_lifecycle_state_written_by_a_withdrawal_is_the_one_a_reader_names() {
        // Guards against the enum's spelling drifting away from the string the
        // tests above compare — the log is read by people and by other builds.
        let json = serde_json::to_value(PluginLifecycleState::Unregistered).unwrap();
        assert_eq!(json, "unregistered");
    }
}
