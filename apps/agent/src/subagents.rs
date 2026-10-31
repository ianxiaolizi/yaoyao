//! `subagent.run`, `subagent.wait` and `subagent.status` — delegation as tools
//! the parent can call.
//!
//! Everything below the tools already exists by the time this module is reached:
//! the port says how a child is started and how its ending is heard, the kernel
//! says a child's history is a fork and that the parent's log records the
//! delegation, and a backend runs the child — a program this build starts
//! (`agent-subagent-stdio`) or a turn it runs itself ([`crate::inprocess`]).
//! What is left is the one
//! thing none of them can decide — **what a model's tool call means** — and that
//! is assembly policy, so it lives here.
//!
//! ## Two shapes for one call, chosen by who can still read the answer
//!
//! A delegation ends twice over: the call that started it returns, and the child
//! ends. A caller that runs *one* call and then stops — `agent tool`,
//! `agent approve` — has no second reader in this process, so its call waits and
//! the child's ending **is** the answer ([`Delegation::Await`]). A caller running
//! a turn does have a second reader: the next step. That is what lets a turn
//! start a child and keep working while it runs, and what lets one step start
//! more than one ([`Delegation::Background`]).
//!
//! ## What a background call leaves behind
//!
//! The call returns a receipt — the child's session id, which is the child's
//! whole identity — and a task this process owns waits for the child and writes
//! the settlement to the parent's log as soon as it arrives. The log is the
//! record: [`WAIT_TOOL_NAME`] collects an ending (and blocks until there is
//! one), [`STATUS_TOOL_NAME`] reports what this session has delegated and how
//! each child ended without waiting for anything.
//!
//! That settlement is written while the parent's turn may also be appending to
//! the same log — the first writer in this build that is not the turn itself.
//! Two consequences show up in other files and are deliberate: the kernel's
//! delegation and the turn's journal both append at the log's tail
//! (`EventStore::append_at_tail`) rather than reading a sequence and spending
//! it, and every command drains what it started before it exits
//! ([`Children::drain`], called from `support::watch`) so a child is never left
//! with its process gone and its ending unrecorded.
//!
//! ## A child can be a step's worker
//!
//! A call may name a step of the session's plan (`step`), which turns the graph
//! from something the model reads into something that decides what may start:
//! the step has to exist, still be open, and have every dependency done, and a
//! step one worker already carries refuses a second. The check reads the log at
//! the moment of the call, not the turn's opening, so a plan written earlier in
//! the same turn governs. What it does *not* do is finish the step: the ending
//! is reported against the step in the next turn's task layer, and marking it
//! done stays the model's act — a child saying it is finished is not the same
//! fact as the step being finished.
//!
//! ## What the call costs, and who agrees to it
//!
//! The tool declares `subagent.spawn` and it is the only thing standing between
//! a configuration and a process being started for it: the default policy
//! advertises the capability and grants nothing, so a profile that wants to
//! delegate has to say so, and one that says `ask` gets the ordinary approval
//! flow. Nothing else about the delegation is negotiated — the child does not
//! inherit the parent's permissions, which is why the capability is
//! asked about the *parent's* act of delegating and not about what the child
//! will do. The two collection tools are held to the same capability: a run that
//! may not delegate has no delegation to read either, and the policy engine
//! refuses a tool that declares nothing as readily as one whose rules forbid it.
//!
//! ## The child is a process this build starts, and it is recorded as a session
//!
//! A backend is a program named by the configuration, and the child's history is
//! a fork of the parent's — so a delegation produces two durable things: events
//! in the parent's log (`subagent/spawned`, then the settlement) and a session
//! of its own whose log opens by saying the objective it was forked with. The
//! tool touches neither directly. It asks the kernel driver to, which is the
//! point of the driver existing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_config::{SubagentConfig, SubagentKind};
use agent_kernel::policy::PolicyEngine;
use agent_kernel::resource_guard::ResourceGuard;
use agent_kernel::scheduler::Scheduler;
use agent_kernel::subagent::{DelegationError, SubagentDelegation};
use agent_ports::event_store::EventStore;
use agent_ports::subagent::{
    SubagentError, SubagentHandle, SubagentOutcome, SubagentProvider, SubagentRequest,
    SubagentStatus,
};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource, ToolStatus,
};
use agent_protocol::events::EventType;
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::permissions::{Capability, PolicyDecision};
use agent_protocol::plan::{PlanItemStatus, WorkerState};
use agent_subagent_stdio::StdioSubagentProvider;
use serde::Deserialize;
use serde_json::json;

use crate::inprocess::InProcessSubagentProvider;
use crate::subagent_serve;

/// The name a model calls this by.
pub const TOOL_NAME: &str = "subagent.run";
/// The name a model collects a background child's ending with.
pub const WAIT_TOOL_NAME: &str = "subagent.wait";
/// The name a model asks "what did I delegate, and how did it end" with.
pub const STATUS_TOOL_NAME: &str = "subagent.status";

/// The capability all three tools are held to.
///
/// One capability for the family, not one per verb: a run that may not start a
/// child has no child of its own to read, and a policy author who wants the
/// collection to be narrower than the spawning has no vocabulary to say that
/// with — so the honest reading is that the grant is the family's.
const REQUIRED_CAPABILITY: &str = Capability::SUBAGENT_SPAWN;

/// What a model reads when deciding whether to delegate.
///
/// The provider list is written in because it is the one thing about this tool
/// that the configuration decides, and a model that has to guess a backend name
/// will guess wrong — or, worse, call the tool with a name that exists in its
/// imagination and read the refusal as the task having failed.
const RUN_DESCRIPTION: &str = "Delegate one self-contained objective to a child agent. The child \
     runs as its own session, forked from this session's most recent completed turn, and does not \
     inherit this session's permissions or tools; what it may do is the named backend's terms, not \
     this session's. This call returns as soon as \
     the child is started: the answer is `subagent `<`child `<`session_id>`, and the child's ending \
     arrives later. Use `subagent.wait` to wait for that ending, or `subagent.status` to see what \
     is running. Use it for work that is separable from the current step, especially work you can \
     do while going on to something else, and call it more than once in one step to run children in \
     parallel. When the plan names the step this child carries, pass its `step` id: a step the plan \
     has not reached refuses a worker, and a step one worker already carries refuses a second.";

/// What a model reads when deciding whether to wait.
const WAIT_DESCRIPTION: &str = "Wait for a child started by `subagent.run` in this run and return \
     how it ended — its answer, or why it could not finish. Blocks until the child ends, so only \
     call it when the result is what you need next. Name the child by its session id; with no id \
     given it waits for the one child this run has open.";

/// What a model reads when deciding whether to look.
const STATUS_DESCRIPTION: &str = "Report every child this session has delegated to: the backend, \
     the objective, and how each one ended or that it is still running. Never waits. Use it to \
     check on a child without blocking, or to find the id `subagent.wait` wants.";

/// Whether a call waits for the child it started.
///
/// The distinction is not about the tool's usefulness but about who is left to
/// read the answer: a process that runs one call and exits has no later step, so
/// a receipt would be the last thing anyone saw of that child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delegation {
    /// The call's answer is the child's ending. For a caller with no later step.
    Await,
    /// The call returns a receipt; the ending is collected later, by
    /// `subagent.wait` or by the drain before the process exits.
    Background,
}

/// The arguments a model may pass to `subagent.run`, refused if it passes
/// anything else.
///
/// `deny_unknown_fields` is doing real work here: a tool argument that is
/// quietly ignored is a model believing it asked for something. A misspelled
/// `timeout_ms` would otherwise leave a child that runs for its provider's
/// ceiling while the caller thinks it set a deadline.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    /// Which configured backend runs the child.
    provider: String,
    /// What the child is asked to do, in the parent's words.
    objective: String,
    #[serde(default)]
    context_refs: Vec<String>,
    #[serde(default)]
    tool_allowlist: Vec<String>,
    #[serde(default)]
    model_route: Option<agent_protocol::model::ModelRoute>,
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    budget: Option<agent_protocol::profile::RunBudget>,
    /// The plan step this child carries, if the caller named one.
    #[serde(default)]
    step: Option<String>,
}

impl RunArgs {
    fn into_request(self) -> SubagentRequest {
        SubagentRequest {
            objective: self.objective,
            context_refs: self.context_refs,
            tool_allowlist: self.tool_allowlist,
            model_route: self.model_route,
            budget: self.budget,
            output_schema: self.output_schema,
            timeout_ms: self.timeout_ms,
            step: self.step,
        }
    }
}

/// The arguments `subagent.wait` accepts.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    /// The child to wait for. `None` means "the one this run has open".
    #[serde(default)]
    child_session_id: Option<String>,
}

/// A child this process started and has not finished collecting.
///
/// One entry per delegation, kept from the moment the call returns until the
/// ending has been recorded and the handle released. The entry is what
/// `subagent.wait` waits on and what `subagent.status` asks "is it still
/// running?" — a child that was dropped from here early would be unobservable
/// in the one place a model can look.
pub struct Child {
    parent: SessionId,
    handle: SubagentHandle,
    delegation: Arc<SubagentDelegation>,
    /// What the backend said when the child ended; `None` until then.
    ending: tokio::sync::watch::Receiver<Option<Ending>>,
    /// What this process has already done about that ending.
    done: tokio::sync::Mutex<Done>,
}

/// How a child ended, as far as this process knows.
#[derive(Clone, Debug)]
pub enum Ending {
    /// The child ended with this outcome.
    Outcome(SubagentOutcome),
    /// The backend could not say how the child ended. Nothing can be recorded
    /// as a settlement, because there is no ending to record.
    Unsaid(String),
}

#[derive(Default)]
struct Done {
    recorded: bool,
    disposed: bool,
}

impl Child {
    /// The child's session, which is its whole identity.
    pub fn id(&self) -> &SessionId {
        &self.handle.child_session_id
    }

    /// How the child ended, waiting until it has.
    pub async fn ending(&self) -> Ending {
        let mut ending = self.ending.clone();
        loop {
            if let Some(ending) = self.ending_now() {
                return ending;
            }
            // A closed channel means the task that was waiting for the child is
            // gone without publishing — only the runtime shutting down under
            // this process can do that, and the honest answer is that nothing is
            // known rather than that the child never ended.
            if ending.changed().await.is_err() {
                return Ending::Unsaid("the wait for this child was cut short".to_string());
            }
        }
    }

    /// The ending if it has already arrived, without waiting for one.
    pub fn ending_now(&self) -> Option<Ending> {
        self.ending.borrow().clone()
    }

    /// Record the ending and release the child, once.
    ///
    /// Both halves are once-only: two settlements of one child would be two
    /// endings in the log for one child, and a second dispose is a call the
    /// backend promises is a no-op but has no reason to be given. Failures are
    /// returned as notes rather than raised — the child did its job, and
    /// bookkeeping going wrong afterwards is not the task's outcome.
    pub async fn finish(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let ending = self.ending().await;
        let mut done = self.done.lock().await;
        if !done.recorded {
            match &ending {
                Ending::Outcome(outcome) => {
                    match self
                        .delegation
                        .record_settlement(&self.parent, &self.handle, outcome)
                        .await
                    {
                        Ok(()) => done.recorded = true,
                        Err(error) => notes.push(format!(
                            "subagent {}: its ending could not be recorded: {error}",
                            self.id()
                        )),
                    }
                }
                Ending::Unsaid(message) => {
                    // Nothing to write: a settlement is a fact about how the
                    // child ended, and this is the absence of one.
                    done.recorded = true;
                    notes.push(format!(
                        "subagent {}: the backend could not say how it ended: {message}",
                        self.id()
                    ));
                }
            }
        }
        if !done.disposed {
            done.disposed = true;
            if let Err(error) = self.delegation.dispose(&self.handle).await {
                notes.push(format!(
                    "subagent {}: its handle could not be released: {error}",
                    self.id()
                ));
            }
        }
        notes
    }
}

/// The children this process started and has not collected.
///
/// Shared by the tool provider (which starts them and answers about them) and
/// the command that owns the process (which drains them before exiting), which
/// is why it is an `Arc` in the runtime rather than a field of the tool.
#[derive(Default)]
pub struct Children {
    started: Mutex<BTreeMap<SessionId, Arc<Child>>>,
}

impl Children {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start the wait for a child that has already been spawned.
    ///
    /// The wait runs on its own task because the whole point of the background
    /// shape is that the caller's turn does not. What the task does with the
    /// result is publish it: recording and releasing belong to whoever collects
    /// it (a `subagent.wait` call, or the drain), so that the parent's log is
    /// only written from a place that knows the turn is not appending to it —
    /// see [`Child::finish`] and `EventStore::append_at_tail`.
    pub fn started(
        &self,
        delegation: Arc<SubagentDelegation>,
        parent: SessionId,
        handle: &SubagentHandle,
    ) -> Arc<Child> {
        let (sender, ending) = tokio::sync::watch::channel(None);
        let child = Arc::new(Child {
            parent,
            handle: handle.clone(),
            delegation: delegation.clone(),
            ending,
            done: tokio::sync::Mutex::new(Done::default()),
        });
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(handle.child_session_id.clone(), child.clone());

        let waiting = child.clone();
        tokio::spawn(async move {
            let ending = match waiting.delegation.wait(&waiting.handle).await {
                Ok(outcome) => Ending::Outcome(outcome),
                Err(error) => Ending::Unsaid(error.to_string()),
            };
            // A receiver that has gone is a process that stopped caring; the
            // send is the last thing this task does either way.
            let _ = sender.send(Some(ending));
        });
        child
    }

    /// The child with this id, if this process started it.
    pub fn get(&self, child: &SessionId) -> Option<Arc<Child>> {
        self.started.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(child).cloned()
    }

    /// Every child this process started, in a stable order.
    pub fn open(&self) -> Vec<Arc<Child>> {
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Wait for every child, record each ending, and release each handle.
    ///
    /// Called once, as the process is about to exit: a child whose progress no
    /// longer exists is a child whose ending nobody will write, and the parent's
    /// log would be left saying a delegation began and then nothing. The wait is
    /// bounded by the children themselves (each backend enforces its ceiling),
    /// which is the same bound a blocking `subagent.run` call has always had.
    pub async fn drain(&self) -> Vec<String> {
        let children = self.open();
        let mut notes = Vec::new();
        for child in children {
            notes.extend(child.finish().await);
        }
        self.started.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
        notes
    }
}

/// The delegation tools for one parent session.
///
/// Bound to the session it was assembled for, because the port's `invoke` is
/// handed an invocation and no session: the tool provider is not what decides
/// which session a call belongs to, the gateway is, and this build has one
/// session per command. A tool that took the parent session from its arguments
/// would let a model name someone else's.
pub struct SubagentToolProvider {
    parent: SessionId,
    /// Read by `subagent.status`, which reports what the session's log says
    /// rather than what this process remembers: a child started by an earlier
    /// command is still this session's delegation.
    store: Arc<dyn EventStore>,
    children: Arc<Children>,
    mode: Delegation,
    /// One delegation per configured backend, keyed by the name a caller asks
    /// for. Keyed rather than listed because the caller *names* the backend: a
    /// single provider would have to decide which one an unqualified call meant,
    /// and that answer would change with the configuration.
    delegations: BTreeMap<String, Arc<SubagentDelegation>>,
    /// The backends that need a run handed to them at the end of assembly
    /// ([`crate::inprocess`]). Kept beside the delegations because the
    /// delegation is what a call goes through and this is what makes the backend
    /// behind it able to start anything.
    in_process: Vec<Arc<InProcessSubagentProvider>>,
    /// Configured backends that could not be built, and why. Said out loud by
    /// the caller rather than dropped in silence: a run whose configuration
    /// names a backend the model can never call has a difference between what
    /// it says and what it does, and only a note can close it.
    notes: Vec<String>,
}

impl SubagentToolProvider {
    /// Build one delegation per configured backend.
    ///
    /// `tools` is the surface a fork repairs an inherited history with — the
    /// built-in family, deliberately not the surface this tool is part of: a
    /// composite cannot hold itself, and repairing a fork has no question about
    /// delegation in it.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn EventStore>,
        tools: Arc<dyn ToolProvider>,
        parent: SessionId,
        configured: &[SubagentConfig],
        children: Arc<Children>,
        mode: Delegation,
        facts: &SelfFacts,
    ) -> Self {
        let mut in_process: Vec<Arc<InProcessSubagentProvider>> = Vec::new();
        let mut delegations = BTreeMap::new();
        let mut notes = Vec::new();
        for config in configured {
            match backend(config, facts) {
                Ok((provider, runs_in_this_process)) => {
                    if let Some(provider) = runs_in_this_process {
                        in_process.push(provider);
                    }
                    delegations.insert(
                        config.provider.clone(),
                        Arc::new(SubagentDelegation::new(store.clone(), tools.clone(), provider)),
                    );
                }
                Err(reason) => notes.push(format!(
                    "subagent provider '{}' is not started: {reason}",
                    config.provider
                )),
            }
        }
        Self { parent, store, children, mode, delegations, in_process, notes }
    }

    /// The backends this run was configured with but could not start.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Hand every in-process backend the run its children execute on.
    ///
    /// Done once, at the end of assembly, because that run is built from the
    /// parent's scheduler — which is assembled after the tool surface this
    /// provider is on. Until this lands, each in-process backend refuses every
    /// `subagent.run` with the reason (`inprocess`), rather than starting a child
    /// nothing can execute.
    pub fn install(&self, template: Arc<Scheduler>) {
        for provider in &self.in_process {
            provider.install(template.clone());
        }
    }

    /// Whether this run has a backend that executes children itself.
    pub fn has_in_process(&self) -> bool {
        !self.in_process.is_empty()
    }

    /// The backends this run can delegate to, in name order.
    pub fn providers(&self) -> Vec<&str> {
        self.delegations.keys().map(String::as_str).collect()
    }

    /// The names this provider implements, in the order the model sees them.
    fn offered(&self) -> Vec<String> {
        vec![TOOL_NAME.to_string(), WAIT_TOOL_NAME.to_string(), STATUS_TOOL_NAME.to_string()]
    }
}

/// What the parent knows about itself that a `self` child has to be told.
///
/// A `self` backend runs **this binary**, so the child's deployment facts are
/// the parent's — but they travel as arguments rather than as an environment or
/// a configuration file, because a child that read either could widen its own
/// permissions (CONTRACT §7 #136). This is the list of everything it is told,
/// and it is deliberately short enough to read: the program, where the session
/// lives, how to reach a model, where the work may happen, and the parent's
/// policy — the last only so that what a child is granted can be intersected
/// with what its parent allows outright.
pub struct SelfFacts {
    /// The binary to start. `None` when this process cannot say what it is,
    /// which is a refusal for a `self` backend rather than a guess at a path.
    pub program: Option<PathBuf>,
    pub workspace_root: PathBuf,
    pub data_dir: PathBuf,
    /// The provider kind, as `AGENT_MODEL_PROVIDER` names it.
    pub provider_kind: String,
    /// The model the parent's route asks for.
    pub model: Option<String>,
    /// The endpoint, when the provider kind needs one and one is configured.
    pub base_url: Option<String>,
    pub network_allowed: bool,
    /// `on`, `gates` or `off`: the arm the parent runs, handed down so a child's
    /// turn is judged the way its parent's is.
    pub judgment: &'static str,
    /// The parent's policy. A capability it allows **outright** is one a child
    /// may be given; one it would stop and ask about is refused, because a child
    /// that stopped would be asking nobody.
    pub policy: PolicyEngine,
}

/// The configured backends whose working directory the boundary admits, with the
/// ones it does not dropped and the reason for each returned.
///
/// A backend's `cwd` is a path in a configuration file, and a path in a
/// configuration file is relative to the workspace root — never to the process's
/// working directory, which is what the child would otherwise inherit. It is
/// also held to the same boundary as any path a tool is handed, so a backend
/// cannot be started outside the workspace by configuration alone. Resolving it
/// here rather than in the adapter keeps that boundary in the one layer that
/// holds the workspace root, and hands the adapter an absolute path it can use
/// without knowing the workspace exists.
///
/// A backend the boundary refuses is **skipped, not fatal**: like a server that
/// does not answer it is reported and left off the surface, so a misconfigured
/// provider does not stop a run that has others — and a run whose every backend
/// was refused ends up with no delegation tool at all, which is what "no backend
/// this run can reach" should look like.
///
/// A backend is also refused when this run's execution backend cannot supply what
/// it needs. Every kind needs `subagent.spawn`, because that is the capability a
/// `subagent.run` call is checked against and a backend admitted without it would
/// be a tool the model can see and never use. On top of that a stdio child *is* a
/// process, so it needs `process.spawn` as well; an in-process child is a turn in
/// this process and asks the sandbox for nothing more, which is the one kind of
/// delegation a read-only run can have (CONTRACT §7 #133).
pub fn admit_backends(
    configured: &[SubagentConfig],
    guard: &ResourceGuard,
    advertised: &[Capability],
) -> (Vec<SubagentConfig>, Vec<String>) {
    let delegates = advertised.contains(&Capability::new(Capability::SUBAGENT_SPAWN));
    let can_spawn = advertised.contains(&Capability::new(Capability::PROCESS_SPAWN));
    let mut admitted = Vec::new();
    let mut notes = Vec::new();
    for config in configured {
        if !delegates {
            notes.push(format!(
                "subagent provider '{}' is not started: the execution sandbox provides no {}",
                config.provider,
                Capability::SUBAGENT_SPAWN
            ));
            continue;
        }
        if config.kind.is_a_process() && !can_spawn {
            notes.push(format!(
                "subagent provider '{}' is not started: the execution sandbox provides no {}",
                config.provider,
                Capability::PROCESS_SPAWN
            ));
            continue;
        }
        match admitted_paths(config, guard) {
            Ok(config) => admitted.push(config),
            Err(error) => {
                notes.push(format!(
                    "subagent provider '{}' is not started: {error}",
                    config.provider
                ));
            }
        }
    }
    (admitted, notes)
}

/// A backend with its configured paths resolved, or the reason it was refused.
///
/// Two kinds of path, one rule: a `cwd` is where the child *runs* and a `scope`
/// is where it may *write*, and both are written in a configuration file
/// relative to the workspace root. Resolving them here means the child is handed
/// absolute paths it can compare against without knowing where the workspace is,
/// and that a prefix landing outside the parent's writable face is refused at
/// admission rather than at the child's first write.
///
/// The `scope` check is `write`: it is the parent asking its own boundary whether
/// it could write there, which is the only thing that makes a child's write an
/// extension of the parent's authority rather than a new one.
fn admitted_paths(
    config: &SubagentConfig,
    guard: &ResourceGuard,
) -> Result<SubagentConfig, String> {
    let mut resolved = config.clone();
    if let Some(cwd) = &config.cwd {
        resolved.cwd =
            Some(guard.resolve(cwd).map_err(|error| error.to_string())?.display().to_string());
    }
    if !resolved.scope.is_empty() {
        let mut scopes = Vec::with_capacity(resolved.scope.len());
        for prefix in &resolved.scope {
            guard.check(prefix, true).map_err(|error| format!("its scope {prefix:?}: {error}"))?;
            let path =
                guard.resolve(prefix).map_err(|error| format!("its scope {prefix:?}: {error}"))?;
            scopes.push(path.display().to_string());
        }
        resolved.scope = scopes;
    }
    Ok(resolved)
}

/// One configured backend, as the adapter its `kind` names, and — only for the
/// kind that runs here — the in-process provider as itself.
///
/// The second half exists because the in-process adapter is not complete when it
/// is built: the assembly has to hand it the run after the tool surface exists
/// ([`SubagentToolProvider::install`]). A stdio backend needs nothing further, so
/// its second half is `None`.
type Built = (Arc<dyn SubagentProvider>, Option<Arc<InProcessSubagentProvider>>);

/// The ceiling is the configured one and never absent: a child with no deadline
/// is a child the parent cannot bound, and the adapter's own default is what this
/// build calls "no ceiling configured" — not what "no ceiling" means.
///
/// The `Err` is a backend that is configured but cannot be started, and the
/// reason is said out loud by the caller ([`SubagentToolProvider::notes`]): a
/// backend that vanished between "the run has one" and "the model can call it"
/// would otherwise be a refusal nobody can explain.
fn backend(config: &SubagentConfig, facts: &SelfFacts) -> Result<Built, String> {
    match config.kind {
        SubagentKind::InProcess => {
            let ceiling = config.ceiling_ms.map(Duration::from_millis);
            let provider =
                Arc::new(InProcessSubagentProvider::new(config.provider.clone(), ceiling));
            Ok((provider.clone(), Some(provider)))
        }
        SubagentKind::SelfHosted => self_backend(config, facts).map(|provider| (provider, None)),
        SubagentKind::Stdio => {
            // A stdio backend with no program has nothing to run. Resolution
            // refuses the shape, so this is unreachable through configuration —
            // and answered by leaving the backend off the surface rather than by
            // starting an empty one.
            let command = config
                .command
                .as_ref()
                .filter(|command| !command.trim().is_empty())
                .ok_or_else(|| "it names no command".to_string())?;
            let mut provider = StdioSubagentProvider::new(config.provider.clone(), command.clone())
                .with_args(config.args.clone())
                .with_env(config.env.clone());
            if let Some(cwd) = &config.cwd {
                provider = provider.with_cwd(PathBuf::from(cwd));
            }
            if let Some(ceiling_ms) = config.ceiling_ms {
                provider = provider.with_ceiling(Duration::from_millis(ceiling_ms));
            }
            Ok((Arc::new(provider), None))
        }
    }
}

/// The backend that runs **this binary** as the child (CONTRACT §7 #136).
///
/// Everything the child learns is on its command line: which session it
/// continues, where the workspace and the store are, how to reach a model, what
/// it is granted, and where it may write. Two of those are decided here rather
/// than passed through from the configuration:
///
///   * **the grants** are the capabilities a child's surface could use,
///     intersected with the capabilities this parent's policy allows outright —
///     a capability the parent would stop and ask about is not granted, because
///     a child that stopped would be asking nobody;
///   * **the scopes** were resolved at admission, so the child is handed the very
///     directories the parent's own boundary agreed it could write.
///
/// The credential is the one thing that does not travel as an argument. It is the
/// name this backend's `env` table spelled out (`secret://env/…`), resolved here
/// and admitted to the child's environment under the one-name exception
/// `agent_ports::process_env` documents; a name that did not resolve is not
/// passed at all, and the child says it has no key rather than starting without
/// one and failing at the first request.
fn self_backend(
    config: &SubagentConfig,
    facts: &SelfFacts,
) -> Result<Arc<dyn SubagentProvider>, String> {
    let program = facts.program.as_ref().ok_or_else(|| {
        "this process cannot say which binary it is, so a `self` child cannot be started"
            .to_string()
    })?;

    let read_only = config.scope.is_empty();
    let surface = subagent_serve::surface_capabilities(&facts.workspace_root, read_only);
    let grants = grants_within(&facts.policy, surface);
    if grants.is_empty() {
        return Err(
            "the parent's policy allows none of what this child could use, so every call would \
             only ever be refused"
                .to_string(),
        );
    }

    let grants_line =
        grants.iter().map(|grant| grant.as_str().to_string()).collect::<Vec<_>>().join(",");
    let (env, admitted) = resolve_env(&config.env);

    // The facts, rendered once and cloned per spawn: only the child's session
    // id differs between one delegation and the next.
    let mut base = vec![
        "--workspace".to_string(),
        facts.workspace_root.display().to_string(),
        "--data-dir".to_string(),
        facts.data_dir.display().to_string(),
        "--provider".to_string(),
        facts.provider_kind.clone(),
        "--judgment".to_string(),
        facts.judgment.to_string(),
        "--network".to_string(),
        if facts.network_allowed { "allow" } else { "deny" }.to_string(),
        "--grants".to_string(),
        grants_line,
    ];
    if let Some(model) = &facts.model {
        base.push("--model".to_string());
        base.push(model.clone());
    }
    if let Some(base_url) = &facts.base_url {
        base.push("--base-url".to_string());
        base.push(base_url.clone());
    }
    // Already absolute, already checked against the parent's writable face.
    for scope in &config.scope {
        base.push("--scope".to_string());
        base.push(scope.clone());
    }

    let mut provider =
        StdioSubagentProvider::new(config.provider.clone(), program.display().to_string())
            .with_env(env)
            .with_admitted_agent_env(admitted)
            .with_kill_on_parent_close()
            .with_cwd(facts.workspace_root.clone())
            .with_identity_args(move |child, _turn| {
                let mut argv = vec!["subagent".to_string(), subagent_serve::SERVE.to_string()];
                argv.extend(base.iter().cloned());
                argv.push("--child-session".to_string());
                argv.push(child.to_string());
                argv
            });
    if let Some(ceiling_ms) = config.ceiling_ms {
        provider = provider.with_ceiling(Duration::from_millis(ceiling_ms));
    }
    Ok(Arc::new(provider))
}

/// The capabilities of `surface` this policy allows outright, in the order the
/// surface declares them.
///
/// The question is asked with no context and no grants — [`PolicyEngine::evaluate`]'s
/// one context-free reading — which is exactly "would the parent let this happen
/// without asking anyone", the only authority a child can be given.
fn grants_within(policy: &PolicyEngine, surface: Vec<Capability>) -> Vec<Capability> {
    let mut granted: Vec<Capability> = Vec::new();
    for capability in surface {
        if granted.contains(&capability) {
            continue;
        }
        if policy.evaluate(std::slice::from_ref(&capability), &[]) == PolicyDecision::Allow {
            granted.push(capability);
        }
    }
    granted
}

/// A backend's configured environment, with `secret://env/NAME` resolved, and the
/// agent-namespace names that are therefore allowed to reach the child.
///
/// The reference is resolved here rather than in the child because the child
/// never sees the parent's environment: it gets the allowlist plus these names.
/// A reference whose variable is unset drops the pair — passing it unresolved
/// would hand the child a literal string as its credential.
fn resolve_env(configured: &[(String, String)]) -> (Vec<(String, String)>, Vec<String>) {
    let mut pairs = Vec::new();
    let mut admitted = Vec::new();
    for (name, value) in configured {
        let value = match value.strip_prefix("secret://env/") {
            Some(variable) => match std::env::var(variable) {
                Ok(value) => value,
                Err(_) => continue,
            },
            None => value.clone(),
        };
        if name.to_ascii_uppercase().starts_with("AGENT_") {
            admitted.push(name.clone());
        }
        pairs.push((name.clone(), value));
    }
    (pairs, admitted)
}

#[async_trait::async_trait]
impl ToolProvider for SubagentToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let backends = self.providers().join(", ");
        vec![
            ToolDefinition {
                name: TOOL_NAME.to_string(),
                description: format!(
                    "{RUN_DESCRIPTION} Backends this run can delegate to: {backends}."
                ),
                // Builtin, and deliberately not `Plugin`: nothing loaded this
                // tool's shape from anywhere. The *program* a child runs is
                // configured, but what a delegation is — its arguments, its
                // result — is this binary's own port, unlike an MCP tool whose
                // shape a server declares.
                source: ToolSource::Builtin,
                // A child runs a program that can write files, commit, and call
                // out. Nothing about a delegation is reversible, so it is the
                // strictest class the vocabulary has rather than a judgement
                // about this adapter.
                side_effect: ToolSideEffect::Destructive,
                capability_requirements: vec![Capability::new(REQUIRED_CAPABILITY)],
                // A second call starts a second child: the objective is the
                // same, the session is new, and the work happens twice.
                idempotent: false,
                // The deadline belongs to the request and to the configured
                // ceiling, which the backend enforces. A framework timeout here
                // would be a second, shorter bound whose expiry nobody recorded
                // as a fact about the child.
                timeout_ms: None,
                input_schema: self.run_schema(),
            },
            ToolDefinition {
                name: WAIT_TOOL_NAME.to_string(),
                description: WAIT_DESCRIPTION.to_string(),
                source: ToolSource::Builtin,
                // Waiting changes nothing; what it collects was already decided
                // by the child.
                side_effect: ToolSideEffect::Pure,
                capability_requirements: vec![Capability::new(REQUIRED_CAPABILITY)],
                // Waiting twice for the same ending returns the same ending, and
                // the settlement is recorded once (see `Child::finish`).
                idempotent: true,
                // No framework timeout: the child's own ceiling is the bound,
                // and a shorter one here would turn "still working" into "failed
                // for being asked about too early".
                timeout_ms: None,
                input_schema: self.wait_schema(),
            },
            ToolDefinition {
                name: STATUS_TOOL_NAME.to_string(),
                description: STATUS_DESCRIPTION.to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Pure,
                capability_requirements: vec![Capability::new(REQUIRED_CAPABILITY)],
                idempotent: true,
                timeout_ms: None,
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
            },
        ]
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        match tool_name {
            TOOL_NAME => Some(ToolSideEffect::Destructive),
            WAIT_TOOL_NAME | STATUS_TOOL_NAME => Some(ToolSideEffect::Pure),
            _ => None,
        }
    }

    fn path_arguments(&self, _tool_name: &str) -> Vec<String> {
        // Nothing here names a path a person could type; a backend's working
        // directory came from the configuration and was checked at assembly.
        Vec::new()
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        match invocation.tool_name.as_str() {
            TOOL_NAME => self.run(invocation).await,
            WAIT_TOOL_NAME => self.wait(invocation).await,
            STATUS_TOOL_NAME => self.status().await,
            other => Err(ToolProviderError::UnknownTool {
                tool: other.to_string(),
                available: self.offered(),
            }),
        }
    }
}

impl SubagentToolProvider {
    /// Whether the plan says `step` may start a worker now.
    ///
    /// Read from the log at the moment of the call rather than from the turn's
    /// opening: a plan written earlier in the same turn is the one that
    /// governs, and a second worker on a step the first one is carrying is the
    /// mistake this check exists for. Every refusal names what to do instead,
    /// because the caller is a model that can act on it.
    async fn admit_step(&self, step: &str) -> Result<(), ToolProviderError> {
        let events =
            self.store.read_stream(&self.parent, EventSeq::ZERO, usize::MAX).await.map_err(
                |error| refused(TOOL_NAME, format!("cannot read the session log: {error}")),
            )?;
        let plan = agent_kernel::plan::current(&events)
            .map_err(|error| {
                refused(TOOL_NAME, format!("the session's plan will not read: {error}"))
            })?
            .unwrap_or_default();
        let known: Vec<&str> = plan.items.iter().filter_map(|item| item.id.as_deref()).collect();
        if known.is_empty() {
            return Err(refused(
                TOOL_NAME,
                format!(
                    "`step` is {step:?}, but this session's plan names no step ids; give the \
                     step an `id` in plan.update, or drop `step`"
                ),
            ));
        }
        let Some(item) = plan.items.iter().find(|item| item.id.as_deref() == Some(step)) else {
            return Err(refused(
                TOOL_NAME,
                format!(
                    "`step` is {step:?}, which is not a step in this session's plan; the ids are: {}",
                    known.join(", ")
                ),
            ));
        };
        if let Some(worker) = agent_kernel::plan::workers(&events)
            .into_iter()
            .find(|worker| worker.step == step && worker.state == WorkerState::Running)
        {
            return Err(refused(
                TOOL_NAME,
                format!(
                    "{step:?} already has a worker: subagent {}. Collect it with {WAIT_TOOL_NAME} \
                     before starting another — two children on one step is the same work done \
                     twice.",
                    worker.child
                ),
            ));
        }
        match item.status {
            PlanItemStatus::Done => {
                return Err(refused(
                    TOOL_NAME,
                    format!("{step:?} is already done; a worker is for a step that still has work"),
                ));
            }
            PlanItemStatus::Doing => {
                return Err(refused(
                    TOOL_NAME,
                    format!(
                        "{step:?} is already doing; a worker is for a step nobody has started — \
                         call plan.update to say what that work is, or drop `step`"
                    ),
                ));
            }
            PlanItemStatus::Todo => {}
        }
        let waiting: Vec<&str> = item
            .depends_on
            .iter()
            .filter(|dep| {
                !plan.items.iter().any(|other| {
                    other.id.as_deref() == Some(dep.as_str())
                        && other.status == PlanItemStatus::Done
                })
            })
            .map(String::as_str)
            .collect();
        if !waiting.is_empty() {
            return Err(refused(
                TOOL_NAME,
                format!(
                    "{step:?} waits on {}, which is not done; starting a worker now would work \
                     on a step the plan has not reached",
                    waiting.join(", ")
                ),
            ));
        }
        Ok(())
    }

    /// `subagent.run`: start a child, and either wait for it or hand back a
    /// receipt naming it.
    async fn run(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let args: RunArgs =
            serde_json::from_value(invocation.arguments.clone()).map_err(|error| {
                refused(TOOL_NAME, format!("the arguments are not a subagent request: {error}"))
            })?;

        let Some(delegation) = self.delegations.get(&args.provider) else {
            return Err(refused(
                TOOL_NAME,
                format!(
                    "'{}' is not a backend this run has; it has: {}",
                    args.provider,
                    self.providers().join(", ")
                ),
            ));
        };

        // Naming a step is asking the plan to admit the work: the graph is the
        // ordering, and a step whose dependencies are not done is one the plan
        // says has not been reached. Read before the clock because a refused
        // call starts no child at all.
        if let Some(step) = &args.step {
            self.admit_step(step).await?;
        }

        // The clock covers the delegation and not the argument parsing: what a
        // caller wants to know is what the child cost, and the child starts at
        // the fork.
        let started = Instant::now();

        // Held past the request because the receipt names the step, and the
        // request is consumed by the delegation.
        let step = args.step.clone();

        // `None` is the parent's most recent closed turn, which is the only
        // boundary a fork will copy up to: a child delegated mid-turn inherits
        // what the parent has finished, not what it is in the middle of.
        let handle = delegation
            .delegate(&self.parent, None, args.into_request())
            .await
            .map_err(|error| refused(TOOL_NAME, delegation_refusal(&error)))?;

        match self.mode {
            Delegation::Await => {
                let outcome = delegation
                    .settle(&self.parent, &handle)
                    .await
                    .map_err(|error| refused(TOOL_NAME, error.to_string()))?;
                let mut content = describe(&handle.child_session_id, &outcome);
                // The handle is released as soon as its ending is known,
                // because this caller has no path that would ask about the child
                // again. A failure here is reported in the content rather than
                // as the call failing: the child did its job.
                if let Err(error) = delegation.dispose(&handle).await {
                    content.push_str(&format!(
                        "\n(the child's handle could not be released: {error})"
                    ));
                }
                Ok(ToolResult {
                    status: status_of(outcome.status()),
                    content,
                    artifacts: Vec::new(),
                    truncated: false,
                    facts: Default::default(),
                    // This provider runs a child; it does not decide policy. The
                    // gateway that handed it the call stamps the decision.
                    policy: None,
                    redaction_metadata: Default::default(),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                })
            }
            Delegation::Background => {
                let child = self.children.started(delegation.clone(), self.parent.clone(), &handle);
                Ok(ToolResult {
                    status: ToolStatus::Completed,
                    content: receipt(child.id(), step.as_deref()),
                    artifacts: Vec::new(),
                    truncated: false,
                    facts: Default::default(),
                    policy: None,
                    redaction_metadata: Default::default(),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                })
            }
        }
    }

    /// `subagent.wait`: collect one child's ending, recording it when it is the
    /// first collection.
    async fn wait(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let args: WaitArgs =
            serde_json::from_value(invocation.arguments.clone()).map_err(|error| {
                refused(WAIT_TOOL_NAME, format!("the arguments are not a subagent wait: {error}"))
            })?;

        let child = match &args.child_session_id {
            Some(id) => self
                .children
                .get(&SessionId::from_string(id))
                .ok_or_else(|| refused(WAIT_TOOL_NAME, format!("{} {}", self.waitable(), id)))?,
            None => {
                let open = self.children.open();
                match open.len() {
                    1 => open[0].clone(),
                    0 => {
                        return Err(refused(
                            WAIT_TOOL_NAME,
                            "this run has no child to wait for; `subagent.status` shows what this \
                             session delegated in earlier runs",
                        ));
                    }
                    _ => {
                        // Naming what there is, rather than picking one: the
                        // caller asked an ambiguous question, and choosing for it
                        // would make the wrong answer look like the right one.
                        let ids: Vec<String> =
                            open.iter().map(|child| child.id().to_string()).collect();
                        return Err(refused(
                            WAIT_TOOL_NAME,
                            format!(
                                "this run has {} children; name which one to wait for: {}",
                                ids.len(),
                                ids.join(", ")
                            ),
                        ));
                    }
                }
            }
        };

        let started = Instant::now();
        let ending = child.ending().await;
        // Recording is what makes the ending survive this process; releasing the
        // handle is what stops a backend holding an entry per child.
        let notes = child.finish().await;
        let outcome = match ending {
            Ending::Outcome(outcome) => outcome,
            Ending::Unsaid(message) => {
                return Err(refused(
                    WAIT_TOOL_NAME,
                    format!(
                        "subagent {}: the backend could not say how it ended: {message}",
                        child.id()
                    ),
                ));
            }
        };

        let mut content = describe(child.id(), &outcome);
        for note in notes {
            content.push_str(&format!("\n({note})"));
        }
        Ok(ToolResult {
            status: status_of(outcome.status()),
            content,
            artifacts: Vec::new(),
            truncated: false,
            facts: Default::default(),
            policy: None,
            redaction_metadata: Default::default(),
            duration_ms: Some(started.elapsed().as_millis() as u64),
        })
    }

    /// `subagent.status`: what this session has delegated, and where each child
    /// stands.
    ///
    /// Read from the log, not from this process's memory: a child delegated by
    /// an earlier command is still a fact about this session, and a status that
    /// forgot it would be a status that reads as "you never delegated".
    async fn status(&self) -> Result<ToolResult, ToolProviderError> {
        let events =
            self.store.read_stream(&self.parent, EventSeq::ZERO, usize::MAX).await.map_err(
                |error| refused(STATUS_TOOL_NAME, format!("cannot read the session log: {error}")),
            )?;

        let mut rows: Vec<Row> = Vec::new();
        for stored in events {
            let event_type = stored.event.event_type.as_str();
            if !event_type.starts_with("subagent/") {
                continue;
            }
            let payload = &stored.event.payload;
            let child = payload["child_session_id"].as_str().unwrap_or("?").to_string();
            match event_type {
                EventType::SUBAGENT_SPAWNED => rows.push(Row {
                    child,
                    provider: payload["provider"].as_str().unwrap_or("?").to_string(),
                    objective: payload["request"]["objective"].as_str().unwrap_or("").to_string(),
                    ending: None,
                }),
                _ => {
                    let ended = settlement(payload);
                    // Newest unresolved spawn of this child: a session can
                    // delegate to the same backend twice, and each child is its
                    // own row.
                    if let Some(row) =
                        rows.iter_mut().rev().find(|row| row.child == child && row.ending.is_none())
                    {
                        row.ending = Some(ended);
                    }
                }
            }
        }

        let mut content = if rows.is_empty() {
            "this session has delegated to no child.".to_string()
        } else {
            let mut lines = vec!["children this session has delegated to:".to_string()];
            for row in rows {
                let ending = self.where_it_stands(&row);
                let objective = if row.objective.is_empty() {
                    String::new()
                } else {
                    format!(" — \"{}\"", row.objective)
                };
                lines.push(format!("- {} ({}) {ending}{objective}", row.child, row.provider));
            }
            lines.join("\n")
        };
        if is_empty_report(&content) {
            content.push_str(" `subagent.run` starts one.");
        }
        Ok(ToolResult {
            status: ToolStatus::Completed,
            content,
            artifacts: Vec::new(),
            truncated: false,
            facts: Default::default(),
            policy: None,
            redaction_metadata: Default::default(),
            duration_ms: None,
        })
    }

    /// Where one child stands, for a row with no settlement in the log.
    fn where_it_stands(&self, row: &Row) -> String {
        if let Some(ending) = &row.ending {
            return ending.clone();
        }
        match self.children.get(&SessionId::from_string(&row.child)) {
            // The ending is known here and the log has not been told yet: the
            // collection that records it has not happened.
            Some(child) => match child.ending_now() {
                Some(Ending::Outcome(outcome)) => {
                    format!("{} (not yet recorded)", outcome.describe())
                }
                Some(Ending::Unsaid(message)) => format!("ended, unrecorded: {message}"),
                None => "running".to_string(),
            },
            None => "no ending recorded".to_string(),
        }
    }

    /// The refusal a `subagent.wait` call gets when it names a child this
    /// process does not have.
    fn waitable(&self) -> String {
        let ids: Vec<String> =
            self.children.open().iter().map(|child| child.id().to_string()).collect();
        if ids.is_empty() {
            "is not a child this run started; it has started none".to_string()
        } else {
            format!("is not a child this run started; it has: {}", ids.join(", "))
        }
    }

    /// The run tool's arguments, as JSON Schema, with the backends this run
    /// actually has.
    fn run_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "enum": self.providers(),
                    "description": "Which configured backend runs the child.",
                },
                "objective": {
                    "type": "string",
                    "description": "What the child is asked to do, in your own words. It becomes \
                                    the reason recorded in the child's history, and the child \
                                    sees it as its task.",
                },
                "context_refs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "References to context the child should be given: artifact \
                                    locators, event ids. Passed through verbatim, not resolved.",
                },
                "tool_allowlist": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict which tools the child may use. Ask for it and a \
                                    backend that cannot restrict a child refuses the delegation.",
                },
                "model_route": {
                    "type": "object",
                    "description": "Pin the child to a model route.",
                },
                "output_schema": {
                    "type": "object",
                    "description": "Ask the child to answer in this shape. A backend passes it to \
                                    the child, and whether the child obeys is the child's; the \
                                    schema is a requirement the child is told, not a filter the \
                                    parent applies afterwards.",
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long the child may run, in milliseconds. A request can \
                                    shorten the configured ceiling and never extend it.",
                },
                "budget": {
                    "type": "object",
                    "description": "Bound what the child may spend. A backend that enforces no \
                                    budget refuses the delegation instead of dropping the limit.",
                },
                "step": {
                    "type": "string",
                    "description": "The id of the plan step this child carries. Optional, and \
                                    checked against the plan when given: the step must exist, \
                                    still be open, and have no dependency left undone. Naming \
                                    it is what lets the task layer say which step a worker is \
                                    working on, and what lets two ready steps run at once.",
                },
            },
            "required": ["provider", "objective"],
            "additionalProperties": false,
        })
    }

    /// The wait tool's arguments.
    fn wait_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "child_session_id": {
                    "type": "string",
                    "description": "The child to wait for, as `subagent.run` named it. Omit it \
                                    when this run has exactly one child open.",
                },
            },
            "additionalProperties": false,
        })
    }
}

/// One line of a status report, built from the session's log.
struct Row {
    child: String,
    provider: String,
    objective: String,
    /// How it ended, once a settlement for it has been seen.
    ending: Option<String>,
}

/// Whether a status report said there was nothing to report.
fn is_empty_report(content: &str) -> bool {
    content.starts_with("this session has delegated to no child")
}

/// How a settlement event reads, in the outcome's own words.
///
/// Re-parsed rather than echoed: the payload is what the kernel wrote, and a
/// report that rendered the raw JSON would be a reader's problem instead of
/// this module's. A payload this build cannot read is said to be exactly that.
fn settlement(payload: &serde_json::Value) -> String {
    match serde_json::from_value::<SubagentOutcome>(payload["outcome"].clone()) {
        Ok(outcome) => outcome.describe(),
        Err(error) => format!("an ending this build cannot read: {error}"),
    }
}

/// What the caller of a background `subagent.run` is told.
///
/// The session id is the child's identity — it is where the child's history is,
/// and it is what `subagent.wait` and `subagent.status` name it by. Saying that
/// the call did not wait is not decoration: a model that reads a receipt as an
/// answer will carry on believing it has the result.
fn receipt(child: &SessionId, step: Option<&str>) -> String {
    let carrying = match step {
        Some(step) => format!(" It carries step {step:?} of this session's plan."),
        None => String::new(),
    };
    format!(
        "subagent {child}: started and running in the background.{carrying}\n\
         Collect its ending with {WAIT_TOOL_NAME} (child_session_id: \"{child}\"); \
         {STATUS_TOOL_NAME} shows it without waiting."
    )
}

/// How a child's ending reads as a tool call's status.
///
/// The same three groups the parent's log uses for the settlement event, because
/// a caller reading both should not have to reconcile two vocabularies: a
/// partial answer is a completed call that did not finish the job, and a
/// delegation the parent cancelled is neither of the two failure words.
fn status_of(status: SubagentStatus) -> ToolStatus {
    match status {
        SubagentStatus::Completed | SubagentStatus::Partial => ToolStatus::Completed,
        SubagentStatus::Failed | SubagentStatus::TimedOut => ToolStatus::Failed,
        SubagentStatus::Cancelled | SubagentStatus::CancelledUncertain => ToolStatus::Cancelled,
        // Unreachable: `settle` returns an outcome, and every outcome is
        // terminal. A tool is the wrong place to refuse a modelling error, so
        // the strictest reading — the call did not do what it was for — is the
        // one that fails closed.
        SubagentStatus::Running => ToolStatus::Failed,
    }
}

/// What the caller is told, in the child's own terms plus what the child was.
///
/// The session id is carried because it is the child's identity: it is where the
/// child's history is, and it is what the parent's log names in
/// `subagent/spawned`. Without it, "the child failed" is a fact a reader cannot
/// follow up.
fn describe(child: &SessionId, outcome: &SubagentOutcome) -> String {
    let mut text = format!("subagent {child}: {}", outcome.describe());
    match outcome {
        SubagentOutcome::Completed { result: Some(result), .. } => {
            text.push_str(&format!("\n{result}"));
        }
        SubagentOutcome::Failed { side_effects, .. } if !side_effects.is_empty() => {
            text.push_str("\nIt says it changed: ");
            text.push_str(&side_effects.join("; "));
        }
        _ => {}
    }
    text
}

/// A refusal in the vocabulary the gateway passes on.
fn refused(tool: &str, message: impl Into<String>) -> ToolProviderError {
    ToolProviderError::Failed { tool: tool.to_string(), message: message.into() }
}

/// What a caller is told when the delegation itself is refused.
///
/// The backend's message names what it cannot honour and stops there. A model
/// reading only that cannot tell "this backend has no such knob" from "you are
/// not allowed to ask", and the live endpoint answered the ambiguity by
/// abandoning the delegation rather than retrying without it — one call away
/// from the child it wanted. So a refusal that never started anything says so,
/// and says the request can be made again without the feature.
fn delegation_refusal(error: &DelegationError) -> String {
    match error {
        DelegationError::Unsupported { .. }
        | DelegationError::Backend(SubagentError::Unsupported { .. }) => format!(
            "{error}. Nothing was started and no child session exists: the same objective can be \
             delegated again without the feature(s) it names."
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests;
