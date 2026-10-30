//! The stack a run executes on, assembled in one place.
//!
//! This used to be the body of `main.rs`'s `assemble`, where nothing outside the
//! binary could reach it. The three acceptance suites (`m4`, `m5`, `m6`) each
//! assembled their own copy of the wiring and described it as "the way
//! `apps/agent` assembles one" — a claim no test could check, so the product's
//! assembly could change and every acceptance criterion would still pass. A copy
//! of an assembly is not an assembly. This is the real one, and the tests call
//! it.
//!
//! Two things are injected rather than decided here, and each is one the process
//! environment would otherwise answer:
//!
//! - the **model** ([`SelectedProvider`]), because a test's endpoint is a
//!   scripted transport, not a URL in the environment;
//! - the **advisor** ([`Decisions`]), because the claim "a shadow decision
//!   touches nothing the model sees" is only meaningful next to the run that has
//!   no shadow at all.
//!
//! Everything else is built here and only here: the store, the filesystem
//! boundary, the policy, the tool surface and the plugin registry on it, the
//! guarded gateway, the context engine, the live bus, the scheduler, recovery,
//! resume and fork.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_config::{McpTransport, ResolvedProfile};
use agent_context::ProjectionContextEngine;
use agent_index_sqlite::{MemoryToolProvider, SqliteMemoryIndex};
use agent_kernel::approval::ApprovalService;
use agent_kernel::fork::Fork;
use agent_kernel::guarded::GuardedToolProvider;
use agent_kernel::live_bus::InMemoryLiveEventBus;
use agent_kernel::plugin::{PluginRegistry, RegistryToolProvider};
use agent_kernel::recovery::Recovery;
use agent_kernel::resource_guard::ResourceGuard;
use agent_kernel::resume::Resume;
use agent_kernel::scheduler::{Scheduler, SchedulerError};
use agent_kernel::tools::CompositeToolProvider;
use agent_loop::DefaultAgentLoop;
use agent_memory_sqlite::{MemoryFactsToolProvider, SqliteMemoryStore};
use agent_ports::agent_loop::TurnCapabilityFacts;
use agent_ports::artifact_store::ArtifactStore;
use agent_ports::control::ControlHandler;
use agent_ports::decision::DecisionAdvisor;
use agent_ports::execution_environment::{ExecutionEnvironment, SandboxMode};
use agent_ports::memory_index::MemoryIndex;
use agent_ports::memory_store::MemoryStore;
use agent_ports::model_router::ModelRouter;
use agent_ports::process_launcher::ProcessLauncher;
use agent_ports::tool_gateway::ToolGateway;
use agent_ports::tool_provider::ToolProvider;
use agent_protocol::ids::SessionId;
use agent_protocol::permissions::{Capability, PolicyDecision, SecretRef, Workspace};
use agent_protocol::redactor::{HighEntropy, Redactor};
use agent_store_artifact::FsArtifactStore;
use agent_store_jsonl::JsonlEventStore;
use agent_store_secret::EnvSecretProvider;
use agent_tools_fs::{FsToolProvider, LocalExecutionEnvironment, SandboxedFileEnvironment};
use agent_tools_git::GitToolProvider;

use crate::config;
use crate::decisions;
use crate::instructions::WorkspaceInstructions;
use crate::plugins;
use crate::providers::{self, ProviderConfigError, SelectedProvider};
use crate::sandbox;
use crate::subagents;

/// What the assembly needs that it cannot decide for itself.
///
/// The four owned values are the caller's answers to "which run, where, under
/// what configuration". The two optional ones are the injections above; `None`
/// means "decide it the way the binary does".
pub struct Inputs {
    pub session: SessionId,
    /// Where the filesystem boundary is rooted.
    pub workspace_root: PathBuf,
    /// Where the event log and the artifact store live.
    pub data_dir: PathBuf,
    pub resolved: ResolvedProfile,
    pub model: Option<SelectedProvider>,
    pub decisions: Decisions,
    /// The execution backend this run selected. Read from the host environment
    /// by the caller ([`crate::sandbox`]), not from the project configuration:
    /// a workspace says what it needs, not how much of the machine it gets.
    pub sandbox: SandboxMode,
    /// Whether a sandboxed command may reach the network. The same kind of host
    /// decision as `sandbox`, and ignored by the modes that have no network
    /// controls of their own.
    pub network_allowed: bool,
    /// Whether the loop runs the thinking pipeline at all. The binary passes
    /// true; [`judgment_arm`] then reads `AGENT_JUDGMENT` to pick the default
    /// deterministic gates, the full ledger, or neither.
    /// A test whose subject is another boundary states the premise here.
    pub judgment: bool,
}

/// Which advisor the loop runs with.
///
/// [`Decisions::Unobserved`] is a stack the binary never builds — the product
/// always assembles an advisor, even when no sidecar is configured, and that
/// advisor answers from the rule baseline. It is here because a test that claims
/// the shadow changed nothing needs the run that has no shadow to compare it
/// against, and because "the loop runs with no advisor" is a state the loop
/// itself has always had.
pub enum Decisions {
    /// Assemble the router and the advisor from the resolved configuration.
    FromConfiguration,
    /// No advisor at all: nothing observes the run's choices and no decision
    /// trace is written. The router still routes.
    Unobserved,
}

/// Why the stack could not be assembled.
///
/// Every variant is a refusal to start. None of them is a run that failed at
/// something: an assembly that could not be completed has no run to report on,
/// which is why the binary turns all of them into `EXIT_DID_NOT_RUN`.
#[derive(Debug, thiserror::Error)]
pub enum StackError {
    /// Two providers contributed the same tool name.
    #[error("tool surface: {0}")]
    ToolSurface(String),
    /// No provider could be built for the configured model.
    #[error("model provider: {0}")]
    Model(#[from] ProviderConfigError),
    /// The configured decision backend this build will not honor.
    #[error("decision backend: {0}")]
    DecisionBackend(#[from] decisions::AssembleError),
    /// The control handlers this build hands the scheduler do not agree on who
    /// applies what — two of them claim one control name, or answer to one name.
    #[error("control handlers: {0}")]
    ControlHandlers(#[from] SchedulerError),
    /// The selected backend is not in this build (or its prerequisites are
    /// missing). A refusal to run, never a reason to fall back to a less
    /// confined one.
    #[error("execution sandbox {mode} is unavailable: {reason}")]
    SandboxUnavailable { mode: &'static str, reason: String },
    /// `AGENT_JUDGMENT` names an arm this build does not know. Fail closed:
    /// a typo must not silently pick the expensive pipeline or the baseline.
    #[error("judgment mode '{0}' is not one of `on`, `gates` or `off`")]
    JudgmentMode(String),
    /// `AGENT_REDACT_HIGH_ENTROPY` names a policy this build does not know.
    /// Fail closed for the same reason: a typo must not quietly turn the
    /// detector off, nor start mangling results the reader asked for.
    #[error("redaction mode '{0}' is not one of `off`, `warn` or `redact`")]
    RedactionMode(String),
}

/// Which part of the thinking pipeline a run gets.
///
/// The arms exist for the evaluation and for audit runs: `Gates` is the
/// product default (the deterministic refusals, without the ledger's tool and
/// advice), `Full` adds the ledger, and `Off` is the baseline that predates
/// the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JudgmentArm {
    Full,
    Gates,
    Off,
}

/// The secrets a tool argument may name, from `AGENT_TOOL_SECRETS`.
///
/// A comma-separated list of environment variable names. Host configuration
/// rather than project configuration, for the same reason the sandbox mode and
/// the model key are: a workspace says what it needs, not what the machine is
/// willing to hand it. Sorted and deduplicated so the capability layer reads
/// the same on every run that names the same set. Empty or unset means this run
/// resolves no references at all.
fn tool_secret_refs(raw: Option<&str>) -> Vec<SecretRef> {
    let mut names: Vec<String> = raw
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names.into_iter().map(|name| SecretRef { source: "env".to_string(), name }).collect()
}

/// The high-entropy layer's setting, from `AGENT_REDACT_HIGH_ENTROPY`.
///
/// Host configuration, like the sandbox mode: whether to report or replace a
/// token nobody taught the redactor is a property of the machine and the
/// workspace being read, not of the workspace itself. Unset is `warn` — the
/// detector reports in the tool result's metadata and leaves the text alone.
fn high_entropy_from(raw: Option<&str>) -> Result<HighEntropy, StackError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(HighEntropy::Warn),
        Some(value) if value.eq_ignore_ascii_case("off") => Ok(HighEntropy::Off),
        Some(value) if value.eq_ignore_ascii_case("warn") => Ok(HighEntropy::Warn),
        Some(value) if value.eq_ignore_ascii_case("redact") => Ok(HighEntropy::Redact),
        Some(other) => Err(StackError::RedactionMode(other.to_string())),
    }
}

/// Read the arm from `AGENT_JUDGMENT` (default `gates`).
///
/// `enabled` is the caller's premise; `false` forces `Off` without reading the
/// environment, which is what a test that disables the pipeline means.
fn judgment_arm(enabled: bool) -> Result<JudgmentArm, StackError> {
    if !enabled {
        return Ok(JudgmentArm::Off);
    }
    match std::env::var("AGENT_JUDGMENT").ok().as_deref().map(str::trim) {
        None | Some("") => Ok(JudgmentArm::Gates),
        Some(value) if value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("full") => {
            Ok(JudgmentArm::Full)
        }
        Some(value) if value.eq_ignore_ascii_case("gates") => Ok(JudgmentArm::Gates),
        Some(value) if value.eq_ignore_ascii_case("off") => Ok(JudgmentArm::Off),
        Some(other) => Err(StackError::JudgmentMode(other.to_string())),
    }
}

/// The assembled stack: one event store, one tool surface, and the kernel
/// services built over them.
///
/// A struct rather than a tuple because the commands are not interchangeable
/// readers of the same thing — `resume` needs the gateway's concrete type, and a
/// five-wide tuple makes every command site a line of underscores.
pub struct Runtime {
    pub scheduler: Scheduler,
    pub recovery: Recovery,
    pub resume: Resume,
    pub fork: Fork,
    pub approvals: Arc<ApprovalService>,
    /// The concrete guarded provider, not `dyn ToolGateway`: resuming a granted
    /// call goes around the policy question, and only this type can do that.
    pub gateway: Arc<GuardedToolProvider>,
    /// Kept so that `--events` can read back what a command wrote. Nothing else
    /// in the binary reaches for the store directly.
    pub store: Arc<JsonlEventStore>,
    /// The concrete bus, not `dyn LiveEventBus`: watching live output means
    /// subscribing, and subscribing is not something the loop's view of the bus
    /// needs to offer.
    pub live: Arc<InMemoryLiveEventBus>,
    /// The registry the surface reads on every call, so that a plugin
    /// withdrawn mid-run stops being offered. Held here because a caller that
    /// wants to register one has to reach the same object the gateway does.
    pub registry: Arc<PluginRegistry>,
    /// The tool surface the gateway guards, for callers that need to ask what
    /// is on it.
    pub tools: Arc<dyn ToolProvider>,
    /// The run's one artifact store: the MCP adapter and the gateway's own
    /// output offload both write here.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// The children this process started and has not collected.
    ///
    /// Held here rather than inside the delegation tool because two callers
    /// share it: the tool starts a child and answers questions about it, and the
    /// command that owns the process drains it before exiting
    /// (`support::watch`). Handing the wrapper this object is also what keeps
    /// the drain from needing `Runtime: Sync` — it clones one `Arc`, not the
    /// whole stack.
    pub children: Arc<subagents::Children>,
}

/// The execution backend the selected mode names, or a refusal.
///
/// Fail closed: a mode this build has no backend for stops assembly. Falling
/// back to the local process would run the tools under a boundary nobody
/// selected, which is exactly the confusion a named sandbox exists to remove.
fn environment_for(
    mode: SandboxMode,
    workspace_root: &Path,
    data_dir: &Path,
    network_allowed: bool,
) -> Result<Arc<dyn ExecutionEnvironment>, StackError> {
    match mode {
        SandboxMode::ReadOnly => Ok(Arc::new(LocalExecutionEnvironment::read_only(workspace_root))),
        SandboxMode::LocalProcess => {
            // The flag is parsed for every mode so a typo is caught, but this
            // backend has nothing to enforce it with: say so once, rather than
            // let a `deny` read as a boundary that is not there.
            if let Some(notice) =
                network_notice(mode, network_allowed, sandbox::network_is_explicit())
            {
                eprintln!("{notice}");
            }
            Ok(Arc::new(LocalExecutionEnvironment::new(workspace_root)))
        }
        SandboxMode::WorkspaceSandbox => {
            // The launcher is built here, before anything else: deriving the
            // package SID, checking that the machine's one-time setup ran, and
            // creating the sandbox's own directories are all refusals that must
            // happen before a store is opened. The workspace ACL grant is
            // deferred to the first spawn, because a command that never runs one
            // should not pay for a tree walk.
            let launcher = agent_sandbox_win::AppContainerLauncher::new(
                agent_sandbox_win::PROFILE_NAME,
                workspace_root,
                data_dir.join("sandbox"),
                network_allowed,
            )
            .map_err(|error| StackError::SandboxUnavailable {
                mode: sandbox::label(mode),
                reason: error.to_string(),
            })?;
            let launcher: Arc<dyn ProcessLauncher> = Arc::new(launcher);
            // The file operations go through the worker staged under the
            // sandbox root; the copy has to be made before the first spawn,
            // which is why the environment stages it here at assembly. The
            // launcher grants this directory with the rest of the sandbox, so
            // the container may execute it.
            let worker_dir = data_dir.join("sandbox").join("worker");
            Ok(Arc::new(SandboxedFileEnvironment::new(
                workspace_root,
                launcher,
                network_allowed,
                worker_dir,
            )))
        }
        SandboxMode::Container => Err(StackError::SandboxUnavailable {
            mode: sandbox::label(mode),
            reason: "this build ships no Docker backend".to_string(),
        }),
    }
}

/// The warning a host gets when its network policy has nothing to enforce it.
///
/// Only the local backend qualifies: it runs commands as the user, so a
/// command reaches the network whatever `AGENT_SANDBOX_NETWORK` says. And only
/// an explicit setting is worth answering — `deny` is the default, so warning
/// on the unset case would put a line on every run's stderr. Returned rather
/// than printed so the decision can be tested without capturing stderr.
fn network_notice(
    mode: SandboxMode,
    network_allowed: bool,
    explicit: bool,
) -> Option<&'static str> {
    match (mode, network_allowed, explicit) {
        (SandboxMode::LocalProcess, false, true) => Some(
            "sandbox: the local backend cannot restrict the network; \
             AGENT_SANDBOX_NETWORK=deny has no effect here",
        ),
        _ => None,
    }
}

/// The configured servers this run's sandbox can actually reach.
///
/// A stdio server needs a child process; an HTTP one needs the network. The
/// skip is said out loud: a server that is never started is otherwise
/// invisible until someone wonders why its tools are missing.
fn reachable_servers(
    servers: Vec<plugins::ServerSpec>,
    advertised: &[Capability],
) -> Vec<plugins::ServerSpec> {
    servers
        .into_iter()
        .filter(|server| {
            let needed = match &server.transport {
                McpTransport::Stdio { .. } => Capability::PROCESS_SPAWN,
                McpTransport::Http { .. } => Capability::NETWORK_REQUEST,
            };
            let available = advertised.contains(&Capability::new(needed));
            if !available {
                eprintln!(
                    "plugins: {} not started: the execution sandbox provides no {needed}",
                    server.name
                );
            }
            available
        })
        .collect()
}

/// Assemble the full stack over a single shared event store.
///
/// The configuration arrives already resolved: reading it (and refusing to start
/// when it does not resolve) belongs to the caller, because only the caller
/// knows whether there is a process to stop.
///
/// A delegation started through this assembly is **background**: every product
/// command that runs turns takes this, because a turn has a later step to read
/// the child's ending in. The two callers that run one call and stop —
/// `agent tool`, `agent approve`, `agent resume` — say so themselves through
/// [`assemble_with_delegation`].
pub async fn assemble(inputs: Inputs) -> Result<Runtime, StackError> {
    assemble_with_delegation(inputs, subagents::Delegation::Background).await
}

/// Assemble the stack, stating what a `subagent.run` call in it means.
///
/// The choice is the caller's rather than the configuration's because it is a
/// fact about the caller, not about the workspace: the same configuration driven
/// by a turn delegates in the background, and driven by `agent tool` it waits.
/// See [`subagents::Delegation`] for why the distinction is "who can still read
/// the answer" rather than a performance knob.
pub async fn assemble_with_delegation(
    inputs: Inputs,
    delegation: subagents::Delegation,
) -> Result<Runtime, StackError> {
    let Inputs {
        session,
        workspace_root,
        data_dir,
        resolved,
        model,
        decisions,
        sandbox,
        network_allowed,
        judgment,
    } = inputs;

    let store = Arc::new(JsonlEventStore::new(&data_dir));

    let approvals = Arc::new(ApprovalService::new(store.clone()).with_session_scope(true));

    // The backend is chosen before anything is built on it, and a backend this
    // build cannot supply stops assembly. Nothing below decides to use a less
    // confined one instead: that would run the tools under a boundary nobody
    // selected.
    let env = environment_for(sandbox, &workspace_root, &data_dir, network_allowed)?;
    // What the gateway may resolve when a tool argument names a reference. The
    // same list is what the model is told it may name, one fact read once.
    let tool_secrets = tool_secret_refs(std::env::var("AGENT_TOOL_SECRETS").ok().as_deref());
    // One redactor for the whole assembly: the loop and the tool boundary must
    // not disagree about whether a long mixed token is reported or replaced.
    let redactor = Redactor::new().with_high_entropy(high_entropy_from(
        std::env::var("AGENT_REDACT_HIGH_ENTROPY").ok().as_deref(),
    )?);
    // What this backend actually provides, read once before the environment is
    // handed out. The policy and the plugin surface both ask this question and
    // must get the same answer.
    let advertised = env.advertised_capabilities();
    // The policy is decided over what this run's backend actually advertises,
    // not over what the build can do in principle: a read-only run must refuse
    // a configured fs.write as unavailable rather than let a rule grant it.
    // Built here rather than beside the gateway because the tool surface asks
    // it one question too — which of its tools this run could never call — so a
    // note about the surface and the gate decide from one engine, not two.
    let policy = config::policy_engine(&resolved, &advertised);
    // The file boundary, built once, before anything that starts a process: the
    // configured working directory of a plugin or a backend is a path in a
    // configuration file and goes through the same guard as a path a tool is
    // handed. Building it here rather than next to the tool gateway lets both
    // surfaces below hold it to the boundary they already promise.
    let guard = ResourceGuard::new(Workspace {
        workspace_root: workspace_root.display().to_string(),
        writable_roots: vec![],
        external_directories: vec![],
    });
    // One tool surface over the built-in families and whatever the plugin
    // registry has been given, all on the same execution backend — so the file
    // boundary, the command deadline, and the output cap are the same ones
    // whichever tool is called.
    //
    // Everything that can refuse the run is built *before* a plugin is started
    // (AA-07). Starting an MCP server spawns a child process, and a later
    // refusal would leave that child with no owner: the binary ends the process
    // on assembly failure, and `std::process::exit` runs no destructors. The
    // order below is therefore: build and refuse first, start processes last.
    // What the log says about a plugin decides whether it is started at all —
    // a plugin the operator turned off at a previous boundary is a standing
    // decision, and the only thing that can hold it is the session log.
    //
    // The built-in families are built first, before any plugin is on the
    // surface, because registering one means asking whether the name it brings
    // is already the run's. The names are read off the providers themselves
    // rather than written down here, so the reserved list cannot drift from
    // what the built-ins actually offer.
    let fs_tools: Arc<dyn ToolProvider> = Arc::new(FsToolProvider::new(env));
    // The git tools read the repository in process, so they take the root
    // itself rather than the execution backend. Under the AppContainer
    // backend a spawned git cannot obtain a working directory at all; a reader
    // that already has one has no such problem.
    let git_tools: Arc<dyn ToolProvider> = Arc::new(GitToolProvider::new(workspace_root.clone()));
    // The searchable index the model gets as `memory.search`: the same
    // disposable read model `agent memory search` builds, over this session's
    // own log and bound to this session. Built here, beside the tools, so the
    // model's view and the operator's view are one index rather than two
    // definitions of "this session's history".
    let memory_index: Arc<dyn MemoryIndex> =
        Arc::new(SqliteMemoryIndex::open(store.clone(), data_dir.clone()));
    let memory_tools: Arc<dyn ToolProvider> =
        Arc::new(MemoryToolProvider::new(memory_index, session.clone()));
    // The built-in families are listed once. `reserved` is read off this list
    // and the composite is assembled from it, so a family cannot be added to
    // one and forgotten in the other.
    let mut builtin_families: Vec<Arc<dyn ToolProvider>> = vec![fs_tools, git_tools, memory_tools];
    // Long-term memory is off unless the user configuration turned it on, and
    // when it is off no provider is built at all: a tool that is not on the
    // surface cannot be called by name, which is stronger than one that refuses.
    // The store is the data directory's one `memory.db`, the same file the
    // `agent memory` verbs write — the model and the person share it.
    if resolved.memory_enabled {
        // Whether *writes* are permitted is a policy question, and the gateway
        // answers it by dropping any tool whose capabilities every call would
        // fail. It is asked here only to say out loud what the model was not
        // offered: a recall that works beside a remember that cannot is a
        // difference a person should not have to discover at call time.
        let write = Capability::new(Capability::MEMORY_WRITE);
        if policy.evaluate(std::slice::from_ref(&write), &[]) == PolicyDecision::Deny {
            eprintln!(
                "memory: long-term memory is on, but {write} is refused in this run \
                 (see the policy and the sandbox); only memory.recall is offered"
            );
        }
        let facts: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::open(data_dir.clone()));
        builtin_families.push(Arc::new(MemoryFactsToolProvider::new(
            facts,
            session.clone(),
            Arc::new(crate::clock::now),
        )));
    }
    let mut reserved: BTreeSet<String> = builtin_families
        .iter()
        .flat_map(|provider| provider.definitions())
        .map(|definition| definition.name)
        .collect();
    // The loop's plan tool is not a provider's, so it is not in the list above,
    // but it is on the wire: a plugin that claimed the name would be offering a
    // tool the loop intercepts before any provider sees it.
    reserved.insert(agent_protocol::plan::TOOL_NAME.to_string());
    // The configured backends, held to the workspace boundary the same way a
    // plugin's working directory is: a relative `cwd` resolves against the
    // workspace root, not the process, and one that lands outside is refused. A
    // refused backend is said out loud and left off the surface. It is resolved
    // *here*, before the reservation below, because whether the run has a
    // delegation tool at all depends on how many backends survived.
    //
    // A backend is a child process, so a run whose sandbox provides no
    // `process.spawn` has none: admitting one would offer the model a tool that
    // can only ever fail, and the refusal would arrive at call time rather than
    // here, where it can still be read before the run starts.
    let can_spawn = advertised.contains(&Capability::new(Capability::PROCESS_SPAWN));
    let (subagents_config, subagent_notes) = if can_spawn {
        subagents::admit_backends(&resolved.subagents, &guard)
    } else {
        let notes = if resolved.subagents.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "{} backend(s) not admitted: the execution sandbox provides no {}",
                resolved.subagents.len(),
                Capability::PROCESS_SPAWN
            )]
        };
        (Vec::new(), notes)
    };
    for note in &subagent_notes {
        eprintln!("subagents: {note}");
    }
    // The delegation tools are the run's too, but only in a run that has one:
    // reserving the names for a run with no backend would refuse a plugin names
    // nothing else is using. All three or none — `subagent.wait` with no
    // `subagent.run` is a tool whose every call must refuse, and a plugin
    // offering a *different* `subagent.wait` beside the run's own would be the
    // one thing worse than no name at all.
    if !subagents_config.is_empty() {
        reserved.insert(subagents::TOOL_NAME.to_string());
        reserved.insert(subagents::WAIT_TOOL_NAME.to_string());
        reserved.insert(subagents::STATUS_TOOL_NAME.to_string());
    }
    // One artifact store for the run, built here because two things reach for
    // it and they are built in this order: the MCP adapter, which offloads a
    // content block it cannot carry as text — an image, an audio clip — and the
    // gateway above every provider, which offloads output too large to sit in
    // the model's context. One store gives one answer to "where did that output
    // go?": a blob saved under a server's tool is the same kind of thing as one
    // saved by the kernel's own offload, and a reader holding a locator does
    // not have to know which of them wrote it.
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(FsArtifactStore::new(data_dir.join("artifacts")));
    let registry = Arc::new(PluginRegistry::new(store.clone()).with_reserved_tools(reserved));
    let servers = reachable_servers(plugins::server_specs(&resolved), &advertised);
    let suppressed = match plugins::suppressed(store.as_ref(), &session).await {
        Ok(suppressed) => plugins::Suppressed::ByTheLog(suppressed),
        Err(error) => {
            // Not fatal — refusing to start the run over a log that cannot be
            // read would be a bigger failure than the one being avoided — but
            // not open either. An empty set says "nothing was withdrawn", which
            // is the one thing a failed read cannot say, so the unreadable case
            // holds every configured plugin back instead.
            eprintln!("session log: {error}");
            plugins::Suppressed::Unreadable
        }
    };
    // A surface with one name on it twice is refused here rather than routed
    // by accident: which provider a call reached would otherwise turn on the
    // order of this list. The registry keeps the same promise for a plugin
    // registered later, so the surface cannot drift into a duplicate after it
    // has been built.
    builtin_families
        .push(Arc::new(RegistryToolProvider::new(registry.clone())) as Arc<dyn ToolProvider>);
    let builtins: Arc<dyn ToolProvider> = Arc::new(
        CompositeToolProvider::new(builtin_families)
            .map_err(|e| StackError::ToolSurface(e.to_string()))?,
    );
    // The surface, in two stages, because the delegation is built on the
    // built-ins rather than beside them. A fork repairs an inherited history by
    // asking each open call's `side_effect`, and the delegation's own fork has
    // no question about delegation in it — while a composite that held itself
    // cannot be constructed at all. So the built-ins are assembled first, the
    // delegation is given that, and the surface a caller sees is both.
    //
    // A run with no backend the boundary admitted has no delegation tool:
    // `subagent.run` with nothing behind it could only ever refuse, and a tool a
    // model can see but never use is worse than one that is not there. The
    // admitted list is what counts here, so a run whose every configured backend
    // was refused is a run with no delegation tool rather than one whose only
    // tool can name backends the guard already turned down.
    //
    // The registry of background children is built here, beside the tool that
    // fills it, and kept on the runtime so that the command owning this process
    // can drain it before exiting: a child whose process disappears with the
    // parent is a delegation the log says began and never ended.
    let children = Arc::new(subagents::Children::new());
    let tools: Arc<dyn ToolProvider> = if subagents_config.is_empty() {
        builtins.clone()
    } else {
        Arc::new(
            CompositeToolProvider::new(vec![
                builtins.clone(),
                Arc::new(subagents::SubagentToolProvider::new(
                    store.clone(),
                    builtins.clone(),
                    session.clone(),
                    &subagents_config,
                    children.clone(),
                    delegation,
                )),
            ])
            .map_err(|e| StackError::ToolSurface(e.to_string()))?,
        )
    };

    // One step's output is bounded by the window the run actually has: a
    // result larger than the working set cannot be compacted away afterwards,
    // because the newest group is kept whole. The gateway also decides what the
    // model is offered (`model_definitions`): the surface minus every tool this
    // policy could refuse for every possible call.
    let gateway = Arc::new(
        GuardedToolProvider::new(
            tools.clone(),
            policy,
            approvals.clone(),
            guard.clone(),
            artifacts.clone(),
        )
        .with_offload_limit(resolved.budget.tool_output_limit_bytes())
        .with_redactor(redactor.clone())
        .with_secrets(Arc::new(EnvSecretProvider::new(
            tool_secrets.iter().map(|secret| secret.name.clone()),
        ))),
    );

    // Routing and shadow decisions are assembled here, next to the provider they
    // are built from, and nowhere else decides which model a run talks to.
    let selected = match model {
        Some(selected) => selected,
        None => providers::select(resolved.model.as_deref())?,
    };
    // The tiers the configuration named, pointed at the provider this run
    // selected. Every entry differs from `main` only in its model name: this
    // build speaks to one provider kind per run, and a route that named another
    // kind would be a claim nothing here can honor.
    let routes = providers::route_table(&selected.route, &resolved.model_tiers);
    let (router, advisor): (Arc<dyn ModelRouter>, Option<Arc<dyn DecisionAdvisor>>) =
        match decisions {
            Decisions::FromConfiguration => {
                let router = providers::router(selected.route.clone(), &routes);
                let (router, advisor) =
                    decisions::assemble(Arc::new(router), resolved.laya.clone())?;
                (router, Some(advisor))
            }
            // No advisor, but still a router: the loop is always given one, and a
            // run without an observation is a run whose choices are still routed.
            Decisions::Unobserved => {
                (Arc::new(providers::router(selected.route.clone(), &routes)), None)
            }
        };
    // Which advisors this run has is said out loud when there is something to
    // say. The snapshot records the same list, but a run that attached a
    // sidecar that is down would otherwise only be visible in the trace of a
    // turn that already happened.
    for label in &resolved.decision_backends {
        eprintln!("decision backend: {label}");
    }

    // Only now is anything started. Every construction above can refuse the
    // run, and all of them have already had their chance — a child spawned
    // before them would outlive an assembly error (AA-07). The servers are
    // registered before the surface is handed to the gateway's callers, so the
    // first turn sees them.
    //
    // The handles come back and are kept. They are what makes a plugin
    // withdrawable *during* a run rather than only by editing a file and
    // restarting: the registry has always returned one per registration, and
    // this is the layer that stops throwing them away.
    let registered = plugins::register_configured(
        &registry,
        &session,
        &servers,
        &resolved.capability_set,
        &suppressed,
        &guard,
        &artifacts,
    )
    .await;
    for note in &registered.notes {
        eprintln!("plugins: {note}");
    }
    let handles = registered.handles;

    // Model deltas are published as live events, and `--events` subscribes to
    // them for the duration of a command: a delta is shown as it arrives and
    // never becomes a durable fact.
    let live_bus = Arc::new(InMemoryLiveEventBus::new(256));
    // History comes from the session log, never from a message array the loop
    // keeps. The engine is built here because this is the only layer allowed to
    // see both the loop's port and the projection behind it.
    //
    // A window is applied only when the configuration names one. A reservation
    // without a window is not a policy — there is nothing to reserve from — so
    // it is ignored rather than translated into a made-up ceiling.
    let engine = ProjectionContextEngine::new(store.clone(), &data_dir);
    let context = Arc::new(match resolved.budget.max_input_tokens {
        Some(max_input_tokens) => engine.with_context_policy(agent_context::trim::ContextPolicy {
            max_input_tokens,
            reserved_output_tokens: resolved.budget.reserved_output_tokens.unwrap_or(0),
            // Resolution refuses a trigger without a window, so this is only
            // ever `Some` alongside a usable budget.
            compact_at_percent: resolved.budget.compact_at_percent,
        }),
        None => engine,
    });
    let suite: Arc<dyn ToolGateway> = gateway.clone();
    let mut loop_ = DefaultAgentLoop::new(selected.provider, suite, context, router)
        .with_live_bus(live_bus.clone())
        .with_redactor(redactor);
    if let Some(advisor) = advisor {
        loop_ = loop_.with_decisions(advisor);
    }
    // The arms: `gates` (the default) keeps the deterministic refusals without
    // the ledger's tool and advice, `on` adds the ledger for audit and the
    // evaluation's full arm, and `off` is the baseline that predates the
    // pipeline. The binary passes `judgment: true`; a test may pass false to
    // state that it is not exercising the pipeline at all.
    match judgment_arm(judgment)? {
        JudgmentArm::Full => {}
        JudgmentArm::Gates => loop_ = loop_.without_ledger(),
        JudgmentArm::Off => loop_ = loop_.without_judgment(),
    }
    let loop_ = Arc::new(loop_);
    // The configuration is frozen here, now that the route is known, and handed
    // to the scheduler so that the run records what it executed under before it
    // opens.
    // The capabilities the run is authorized under, kept for the one caller that
    // needs them after the snapshot has taken the resolved configuration: a
    // plugin registered by a control at a later boundary is registered under the
    // same grant the run started with, and it is checked against the registry by
    // the same gate either way.
    let capabilities = resolved.capability_set.clone();
    let snapshot = resolved.into_snapshot(selected.route.clone(), routes.clone());
    // The controls that turn a plugin off and back on are assembled next to the
    // registry that holds them, and handed to the scheduler — which applies them
    // at a turn boundary, never inside one. A run that started no plugins still
    // gets the handler: it is what refuses `plugin.disable` *by name* instead of
    // leaving the command looking accepted while nothing can ever happen.
    //
    // The capabilities are cloned, not moved: the snapshot has just taken the
    // same set, and a grant the run recorded as its own is also what a plugin
    // registered later has to be checked against.
    let controls: Vec<Arc<dyn ControlHandler>> = vec![Arc::new(plugins::PluginControl::new(
        registry.clone(),
        session.clone(),
        servers,
        capabilities,
        guard,
        artifacts.clone(),
        handles,
    ))];
    // What the model is told before it is asked anything. Resolved per run by
    // the scheduler, so an `AGENTS.md` edited while a session is open is what
    // the next turn executes under; the root is the same one the file boundary
    // was built over, so "the workspace" is one place in both places.
    let instructions = Arc::new(WorkspaceInstructions::new(workspace_root.clone()));
    let scheduler = Scheduler::new(store.clone(), loop_, snapshot.budget.max_steps)
        .with_profile_snapshot(snapshot)
        .with_control_handlers(controls)?
        .with_instructions(instructions)
        // The boundary the host chose, told to the model before it reaches it.
        // These are the same two facts the environment was built over, not a
        // second reading of the environment.
        .with_capability(TurnCapabilityFacts { sandbox, network_allowed, tool_secrets });
    let recovery = Recovery::new(store.clone(), tools.clone());
    let resume = Resume::new(store.clone(), approvals.clone(), gateway.clone());
    // The fork closes its child's interrupted tail with the same Recovery rules,
    // so the two share one tool surface rather than each building their own.
    let fork = Fork::new(store.clone(), tools.clone());

    Ok(Runtime {
        scheduler,
        recovery,
        resume,
        fork,
        approvals,
        gateway,
        store,
        live: live_bus,
        registry,
        tools,
        artifacts,
        children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_secrets_are_a_sorted_deduplicated_env_list() {
        assert!(tool_secret_refs(None).is_empty());
        assert!(tool_secret_refs(Some("   ")).is_empty());

        let refs = tool_secret_refs(Some(" B_TOKEN , A_TOKEN,,B_TOKEN "));
        assert_eq!(
            refs,
            vec![
                SecretRef { source: "env".to_string(), name: "A_TOKEN".to_string() },
                SecretRef { source: "env".to_string(), name: "B_TOKEN".to_string() },
            ]
        );
    }

    /// A policy the selected backend cannot enforce is said out loud, once, at
    /// assembly — and only where it is true: the confinement backends do honour
    /// the flag, so they get no notice.
    #[test]
    fn a_network_policy_the_backend_cannot_enforce_is_announced() {
        assert!(network_notice(SandboxMode::LocalProcess, false, true).is_some());
        // The default `deny` says nothing: it is what every run has.
        assert!(network_notice(SandboxMode::LocalProcess, false, false).is_none());
        assert!(network_notice(SandboxMode::LocalProcess, true, true).is_none());
        assert!(network_notice(SandboxMode::WorkspaceSandbox, false, true).is_none());
        assert!(network_notice(SandboxMode::ReadOnly, false, true).is_none());
    }

    #[test]
    fn the_redaction_mode_is_warn_unless_told_otherwise_and_refuses_typos() {
        assert_eq!(high_entropy_from(None).unwrap(), HighEntropy::Warn);
        assert_eq!(high_entropy_from(Some("  ")).unwrap(), HighEntropy::Warn);
        assert_eq!(high_entropy_from(Some("OFF")).unwrap(), HighEntropy::Off);
        assert_eq!(high_entropy_from(Some("Warn")).unwrap(), HighEntropy::Warn);
        assert_eq!(high_entropy_from(Some("redact")).unwrap(), HighEntropy::Redact);

        match high_entropy_from(Some("warn-ish")) {
            Err(StackError::RedactionMode(value)) => assert_eq!(value, "warn-ish"),
            other => panic!("a typo must be refused, not defaulted: {other:?}"),
        }
    }

    #[test]
    fn a_backend_this_build_does_not_have_stops_assembly() {
        // The fallback that must never happen: a run that selected confinement
        // and got a local shell would enforce nothing while appearing to. The
        // refusal also says *why*, so the message can be acted on.
        let dir = tempfile::tempdir().unwrap();
        let error = environment_for(SandboxMode::Container, dir.path(), dir.path(), false)
            .err()
            .expect("a backend this build does not ship must not be built");
        match error {
            StackError::SandboxUnavailable { mode: "docker", reason } => {
                assert!(reason.contains("Docker"), "{reason}");
            }
            other => panic!("expected a docker refusal, got {other}"),
        }
    }

    #[test]
    fn an_available_backend_is_built_with_the_mode_it_was_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [SandboxMode::ReadOnly, SandboxMode::LocalProcess] {
            let env = environment_for(mode, dir.path(), dir.path(), false)
                .expect("this build has the backend");
            assert_eq!(env.mode(), mode);
        }
    }

    fn server(name: &str, transport: McpTransport) -> plugins::ServerSpec {
        plugins::ServerSpec {
            name: name.to_string(),
            version: "*".to_string(),
            requires: Vec::new(),
            timeout_ms: None,
            transport,
            schema: agent_config::SchemaPolicy::Strict,
        }
    }

    fn servers() -> Vec<plugins::ServerSpec> {
        vec![
            server(
                "stdio",
                McpTransport::Stdio {
                    command: "server".to_string(),
                    args: Vec::new(),
                    env: Vec::new(),
                    cwd: None,
                },
            ),
            server(
                "http",
                McpTransport::Http {
                    url: "https://example.invalid".to_string(),
                    headers: Vec::new(),
                },
            ),
        ]
    }

    #[test]
    fn a_read_only_run_starts_no_configured_server() {
        // Neither transport is reachable without a capability the read-only
        // backend lacks. Starting one anyway would spawn a child (or open a
        // socket) before the policy ever saw a tool call.
        let kept = reachable_servers(servers(), &SandboxMode::ReadOnly.advertised_capabilities());
        assert!(kept.is_empty(), "kept: {:?}", kept.iter().map(|s| &s.name).collect::<Vec<_>>());
    }

    #[test]
    fn a_local_run_keeps_both_transports() {
        let kept =
            reachable_servers(servers(), &SandboxMode::LocalProcess.advertised_capabilities());
        assert_eq!(kept.len(), 2);
    }
}
