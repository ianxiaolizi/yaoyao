//! `subagent.run`, `subagent.wait` and `subagent.status` — delegation as tools
//! the parent can call.
//!
//! Everything below the tools already exists by the time this module is reached:
//! the port says how a child is started and how its ending is heard, the kernel
//! says a child's history is a fork and that the parent's log records the
//! delegation, and the stdio backend runs the process. What is left is the one
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

use agent_config::SubagentConfig;
use agent_kernel::resource_guard::ResourceGuard;
use agent_kernel::subagent::SubagentDelegation;
use agent_ports::event_store::EventStore;
use agent_ports::subagent::{
    SubagentHandle, SubagentOutcome, SubagentProvider, SubagentRequest, SubagentStatus,
};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource, ToolStatus,
};
use agent_protocol::events::EventType;
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::permissions::Capability;
use agent_protocol::plan::{PlanItemStatus, WorkState};
use agent_subagent_stdio::StdioSubagentProvider;
use serde::Deserialize;
use serde_json::json;

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
     runs as its own process and its own session, forked from this session's most recent completed \
     turn, and does not inherit this session's permissions or tools. This call returns as soon as \
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
}

impl SubagentToolProvider {
    /// Build one delegation per configured backend.
    ///
    /// `tools` is the surface a fork repairs an inherited history with — the
    /// built-in family, deliberately not the surface this tool is part of: a
    /// composite cannot hold itself, and repairing a fork has no question about
    /// delegation in it.
    pub fn new(
        store: Arc<dyn EventStore>,
        tools: Arc<dyn ToolProvider>,
        parent: SessionId,
        configured: &[SubagentConfig],
        children: Arc<Children>,
        mode: Delegation,
    ) -> Self {
        let delegations = configured
            .iter()
            .map(|config| {
                let provider: Arc<dyn SubagentProvider> = Arc::new(backend(config));
                (
                    config.provider.clone(),
                    Arc::new(SubagentDelegation::new(store.clone(), tools.clone(), provider)),
                )
            })
            .collect();
        Self { parent, store, children, mode, delegations }
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
pub fn admit_backends(
    configured: &[SubagentConfig],
    guard: &ResourceGuard,
) -> (Vec<SubagentConfig>, Vec<String>) {
    let mut admitted = Vec::new();
    let mut notes = Vec::new();
    for config in configured {
        match &config.cwd {
            None => admitted.push(config.clone()),
            Some(cwd) => match guard.resolve(cwd) {
                Ok(path) => {
                    let mut config = config.clone();
                    config.cwd = Some(path.display().to_string());
                    admitted.push(config);
                }
                Err(error) => {
                    notes.push(format!(
                        "subagent provider '{}' is not started: {error}",
                        config.provider
                    ));
                }
            },
        }
    }
    (admitted, notes)
}

/// One configured backend, as the stdio adapter.
///
/// The ceiling is the configured one and never absent: a child with no deadline
/// is a process the parent cannot bound, and the adapter's own default is what
/// this build calls "no ceiling configured" — not what "no ceiling" means.
fn backend(config: &SubagentConfig) -> StdioSubagentProvider {
    let mut provider = StdioSubagentProvider::new(config.provider.clone(), config.command.clone())
        .with_args(config.args.clone())
        .with_env(config.env.clone());
    if let Some(cwd) = &config.cwd {
        provider = provider.with_cwd(PathBuf::from(cwd));
    }
    if let Some(ceiling_ms) = config.ceiling_ms {
        provider = provider.with_ceiling(Duration::from_millis(ceiling_ms));
    }
    provider
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
            .find(|worker| worker.step == step && worker.state == WorkState::Running)
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
            .map_err(|error| refused(TOOL_NAME, error.to_string()))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventId, SchemaVersion, TurnId};
    use agent_protocol::permissions::Workspace;
    use std::path::Path;

    /// A backend that names a program nothing here starts.
    ///
    /// These tests are about which backends a run keeps and where their working
    /// directory lands, and no command written here is ever executed — the
    /// adapter is not built, only the configuration list is filtered.
    fn config(provider: &str, cwd: Option<&str>) -> SubagentConfig {
        SubagentConfig {
            provider: provider.to_string(),
            command: "unused-by-this-test".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: cwd.map(str::to_string),
            requires: Vec::new(),
            ceiling_ms: None,
        }
    }

    fn guard(dir: &Path) -> ResourceGuard {
        ResourceGuard::new(Workspace {
            workspace_root: dir.display().to_string(),
            writable_roots: Vec::new(),
            external_directories: Vec::new(),
        })
    }

    #[test]
    fn a_relative_backend_directory_resolves_against_the_workspace_root() {
        // A `cwd` in a configuration file is relative to the workspace root, not
        // to wherever the process was launched — the child would otherwise be
        // started in the launcher's directory, which is the defect this method
        // exists to close. It is resolved to an absolute path here so the adapter
        // never has to know the workspace exists.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let guard = guard(&root);

        // The premise: the process is not sitting in the workspace, so nothing
        // resolves to the right place by accident.
        assert_ne!(std::env::current_dir().unwrap(), root.canonicalize().unwrap());

        let (admitted, notes) = admit_backends(&[config("one", Some("sub"))], &guard);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(admitted.len(), 1);
        assert_eq!(
            admitted[0].cwd.as_deref(),
            Some(root.canonicalize().unwrap().join("sub").to_str().unwrap())
        );
    }

    #[test]
    fn a_backend_whose_directory_escapes_the_workspace_is_skipped_with_a_note() {
        // The boundary is the same one every path a tool is handed is held to, so
        // a backend cannot be started outside the workspace by configuration
        // alone — and the run says so rather than starting it in the wrong place.
        let dir = tempfile::tempdir().unwrap();
        let guard = guard(dir.path());

        let (admitted, notes) = admit_backends(&[config("one", Some("../outside"))], &guard);
        assert!(admitted.is_empty(), "a backend outside the workspace was admitted");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("one"), "{:?}", notes[0]);
        assert!(notes[0].contains("not started"), "{:?}", notes[0]);
        assert!(notes[0].contains("path outside workspace"), "{:?}", notes[0]);
    }

    #[test]
    fn a_backend_with_no_configured_directory_is_admitted_unchanged() {
        // No `cwd` means the child inherits the process's working directory —
        // unchanged by this pass. The inconsistency between that and the
        // workspace-root rule is the configuration's to resolve, not something a
        // run may silently rewrite.
        let dir = tempfile::tempdir().unwrap();
        let guard = guard(dir.path());

        let (admitted, notes) = admit_backends(&[config("one", None)], &guard);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].cwd, None);
    }

    /// One delegation to a backend nothing ever starts, for the argument checks
    /// below. Both refuse before the child would be forked, so the program named
    /// here is never executed.
    fn provider() -> SubagentToolProvider {
        SubagentToolProvider::new(
            Arc::new(agent_testkit::FakeEventStore::new()),
            Arc::new(agent_testkit::FakeToolProvider::new()),
            SessionId::from_string("parent"),
            &[config("fake", None)],
            Arc::new(Children::new()),
            Delegation::Await,
        )
    }

    /// The gateway refuses a name outside the backends this run has before the
    /// tool is reached, so this is the second line: a caller that bypasses it —
    /// or a declaration that stopped excluding the name — still gets a sentence
    /// naming what the run does have rather than a bare "unknown".
    #[tokio::test]
    async fn a_backend_the_run_does_not_have_is_named_with_the_ones_it_has() {
        let error = provider()
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "ghost", "objective": "x" }),
            ))
            .await
            .expect_err("a backend this run does not have was accepted");

        let message = error.to_string();
        assert!(message.contains("'ghost' is not a backend this run has"), "{message}");
        assert!(message.contains("fake"), "it says what the run has: {message}");
    }

    /// Likewise for an argument the request does not define: the schema says
    /// `additionalProperties: false`, and this is the check behind it.
    #[tokio::test]
    async fn an_argument_the_request_does_not_define_never_reaches_a_child() {
        let error = provider()
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "x", "timeout": "5s" }),
            ))
            .await
            .expect_err("an undefined argument was accepted");

        let message = error.to_string();
        assert!(message.contains("not a subagent request"), "{message}");
        assert!(message.contains("timeout_ms"), "it names the field it wanted: {message}");
    }

    #[test]
    fn one_refused_backend_does_not_take_the_others_with_it() {
        // A misconfigured provider is skipped, not fatal: a run that has a backend
        // it can reach still has one.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let guard = guard(&root);

        let (admitted, notes) = admit_backends(
            &[config("bad", Some("../outside")), config("good", Some("sub"))],
            &guard,
        );
        assert_eq!(admitted.len(), 1, "{admitted:?}");
        assert_eq!(admitted[0].provider, "good");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("bad"), "{:?}", notes[0]);
    }

    /// A backend that ends a child only when the test says so, and remembers
    /// what was done to it.
    ///
    /// The point of a background delegation is that something is still running
    /// while the caller goes on, so the test has to be able to hold a child open
    /// and then let it go — a backend that answered immediately could not tell a
    /// call that waited from one that did not.
    struct Releasable {
        released: tokio::sync::Semaphore,
        outcome: SubagentOutcome,
        disposals: Mutex<usize>,
    }

    impl Releasable {
        fn new(outcome: SubagentOutcome) -> Arc<Self> {
            Arc::new(Self {
                released: tokio::sync::Semaphore::new(0),
                outcome,
                disposals: Mutex::new(0),
            })
        }

        fn release(&self) {
            self.released.add_permits(1);
        }

        fn disposals(&self) -> usize {
            *self.disposals.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl SubagentProvider for Releasable {
        fn name(&self) -> &str {
            "fake"
        }

        async fn spawn(
            &self,
            child_session_id: &SessionId,
            parent_turn_id: Option<&TurnId>,
            _request: SubagentRequest,
        ) -> Result<SubagentHandle, agent_ports::subagent::SubagentError> {
            Ok(SubagentHandle::new("fake", child_session_id.clone(), parent_turn_id.cloned()))
        }

        async fn wait(
            &self,
            _handle: &SubagentHandle,
        ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
            self.released.acquire().await.expect("the test's semaphore is never closed").forget();
            Ok(self.outcome.clone())
        }

        async fn cancel(
            &self,
            _handle: &SubagentHandle,
        ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
            Ok(SubagentOutcome::Cancelled)
        }

        async fn dispose(
            &self,
            _handle: &SubagentHandle,
        ) -> Result<(), agent_ports::subagent::SubagentError> {
            *self.disposals.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// A store, a parent, and a registry — the three things a background
    /// delegation leaves behind, without the fork and the process.
    fn running(
        outcome: SubagentOutcome,
    ) -> (Arc<agent_testkit::FakeEventStore>, Arc<Children>, Arc<Releasable>, Arc<Child>) {
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        let child_session = SessionId::from_string("child-1");
        let backend = Releasable::new(outcome);
        let provider: Arc<dyn SubagentProvider> = backend.clone();
        let delegation = Arc::new(SubagentDelegation::new(
            store.clone(),
            Arc::new(agent_testkit::FakeToolProvider::new()),
            provider,
        ));
        let handle = SubagentHandle::new("fake", child_session, None);
        let children = Arc::new(Children::new());
        let child = children.started(delegation, SessionId::from_string("parent"), &handle);
        (store, children, backend, child)
    }

    async fn subagent_events(store: &agent_testkit::FakeEventStore) -> Vec<String> {
        store
            .read_stream(&SessionId::from_string("parent"), EventSeq::ZERO, usize::MAX)
            .await
            .expect("the parent's log")
            .into_iter()
            .filter(|event| event.event.event_type.as_str().starts_with("subagent/"))
            .map(|event| event.event.event_type.to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_collected_ending_is_recorded_once_and_the_handle_released_once() {
        // `subagent.wait` can be called twice for the same child — a model that
        // reads no answer the first time asks again — and two settlements would
        // be two endings for one child in a log that has to be able to say how
        // it ended.
        let (store, _children, backend, child) =
            running(SubagentOutcome::Completed { summary: "done".to_string(), result: None });
        backend.release();

        child.finish().await;
        child.finish().await;

        assert_eq!(
            subagent_events(store.as_ref()).await,
            vec![EventType::SUBAGENT_COMPLETED.to_string()],
            "the settlement is written once"
        );
        assert_eq!(backend.disposals(), 1, "the handle is released once");
    }

    #[tokio::test]
    async fn draining_waits_for_a_child_that_has_not_ended() {
        // The promise the exit drain makes: a command that started a child
        // leaves nothing unrecorded. It waits — bounded by the child's own
        // ceiling, which the backend enforces — rather than dropping the child
        // when the command's own work is done.
        let (store, children, backend, _child) = running(SubagentOutcome::Failed {
            message: "broke".to_string(),
            side_effects: Vec::new(),
        });

        let draining = {
            let children = children.clone();
            tokio::spawn(async move { children.drain().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!draining.is_finished(), "draining returned while a child was still running");
        assert!(
            subagent_events(store.as_ref()).await.is_empty(),
            "nothing is recorded before the child ends"
        );

        backend.release();
        let notes = draining.await.expect("the drain task");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            subagent_events(store.as_ref()).await,
            vec![EventType::SUBAGENT_FAILED.to_string()],
            "the ending is recorded, under the family the outcome belongs to"
        );
        assert_eq!(backend.disposals(), 1);
    }

    #[tokio::test]
    async fn a_backend_that_cannot_say_how_a_child_ended_is_reported_and_not_recorded() {
        // A settlement is a fact about how a child ended. When the backend cannot
        // say, there is no fact — and a log entry claiming one would be worse
        // than the absence, because nothing downstream could tell it was made up.
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        let backend: Arc<dyn SubagentProvider> = Arc::new(Silent);
        let delegation = Arc::new(SubagentDelegation::new(
            store.clone(),
            Arc::new(agent_testkit::FakeToolProvider::new()),
            backend,
        ));
        let handle = SubagentHandle::new("fake", SessionId::from_string("child-1"), None);
        let children = Arc::new(Children::new());
        let child = children.started(delegation, SessionId::from_string("parent"), &handle);

        let notes = child.finish().await;

        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("could not say how it ended"), "{notes:?}");
        assert!(subagent_events(store.as_ref()).await.is_empty(), "nothing may be recorded");
    }

    /// A backend whose `wait` fails: the child's fate is unknown to the parent.
    struct Silent;

    #[async_trait::async_trait]
    impl SubagentProvider for Silent {
        fn name(&self) -> &str {
            "fake"
        }

        async fn spawn(
            &self,
            child_session_id: &SessionId,
            parent_turn_id: Option<&TurnId>,
            _request: SubagentRequest,
        ) -> Result<SubagentHandle, agent_ports::subagent::SubagentError> {
            Ok(SubagentHandle::new("fake", child_session_id.clone(), parent_turn_id.cloned()))
        }

        async fn wait(
            &self,
            handle: &SubagentHandle,
        ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
            Err(agent_ports::subagent::SubagentError::Unknown {
                provider: "fake".to_string(),
                child_session_id: handle.child_session_id.clone(),
            })
        }

        async fn cancel(
            &self,
            _handle: &SubagentHandle,
        ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
            Ok(SubagentOutcome::Cancelled)
        }

        async fn dispose(
            &self,
            _handle: &SubagentHandle,
        ) -> Result<(), agent_ports::subagent::SubagentError> {
            Ok(())
        }
    }

    /// The wait tool, over a registry the test fills.
    fn waiting(
        children: Arc<Children>,
        store: Arc<agent_testkit::FakeEventStore>,
    ) -> SubagentToolProvider {
        SubagentToolProvider::new(
            store,
            Arc::new(agent_testkit::FakeToolProvider::new()),
            SessionId::from_string("parent"),
            &[],
            children,
            Delegation::Background,
        )
    }

    #[tokio::test]
    async fn waiting_names_the_children_it_has_when_it_is_given_one_it_does_not() {
        // A model that invents an id — or copies one from an earlier run — gets
        // told what this run can be asked about. "Unknown child" alone reads the
        // same whether the id was a typo or the child was in another process.
        let (store, children, backend, child) = running(SubagentOutcome::Cancelled);
        let provider = waiting(children, store);

        let error = provider
            .invoke(&ToolInvocation::proposed(
                WAIT_TOOL_NAME,
                json!({ "child_session_id": "not-a-child" }),
            ))
            .await
            .expect_err("an id this run does not have was accepted");

        let message = error.to_string();
        assert!(message.contains("is not a child this run started"), "{message}");
        assert!(message.contains(child.id().as_str()), "it names what it has: {message}");
        backend.release();
    }

    #[tokio::test]
    async fn waiting_with_no_id_takes_the_one_child_this_run_has_open() {
        // The common case is one delegation, and making the model copy an id it
        // was just handed is a way to lose a child: a mistyped id is a refusal,
        // not an answer.
        let (store, children, backend, child) =
            running(SubagentOutcome::Partial { summary: "two of the three".to_string() });
        backend.release();
        let provider = waiting(children, store);

        let result = provider
            .invoke(&ToolInvocation::proposed(WAIT_TOOL_NAME, json!({})))
            .await
            .expect("waiting for the only child");

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.content.contains(child.id().as_str()), "{}", result.content);
        assert!(result.content.contains("partial: two of the three"), "{}", result.content);
    }

    #[tokio::test]
    async fn waiting_with_no_id_refuses_to_choose_between_children() {
        // Two children and no name is an ambiguous question. Picking one would
        // make the wrong answer look like the right one.
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        let children = Arc::new(Children::new());
        let provider = waiting(children.clone(), store.clone());
        for id in ["child-1", "child-2"] {
            let backend = Releasable::new(SubagentOutcome::Cancelled);
            let provider_handle: Arc<dyn SubagentProvider> = backend.clone();
            let delegation = Arc::new(SubagentDelegation::new(
                store.clone(),
                Arc::new(agent_testkit::FakeToolProvider::new()),
                provider_handle,
            ));
            let handle = SubagentHandle::new("fake", SessionId::from_string(id), None);
            children.started(delegation, SessionId::from_string("parent"), &handle);
            backend.release();
        }

        let error = provider
            .invoke(&ToolInvocation::proposed(WAIT_TOOL_NAME, json!({})))
            .await
            .expect_err("an ambiguous wait was answered");

        let message = error.to_string();
        assert!(message.contains("2 children"), "{message}");
        assert!(message.contains("child-1") && message.contains("child-2"), "{message}");
    }

    #[tokio::test]
    async fn status_reads_the_sessions_own_log_including_children_from_earlier_runs() {
        // The report is about the session, not about this process: a child
        // delegated by a previous command is still a fact about the session, and
        // a status that forgot it would read as "you never delegated".
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        let parent = SessionId::from_string("parent");
        append(
            &store,
            &parent,
            EventType::SUBAGENT_SPAWNED,
            json!({
                "child_session_id": "child-1",
                "provider": "fake",
                "request": { "objective": "read the notes" },
            }),
        )
        .await;
        append(
            &store,
            &parent,
            EventType::SUBAGENT_COMPLETED,
            json!({
                "child_session_id": "child-1",
                "provider": "fake",
                "outcome": { "status": "completed", "summary": "read them" },
            }),
        )
        .await;

        let provider = waiting(Arc::new(Children::new()), store);
        let result = provider
            .invoke(&ToolInvocation::proposed(STATUS_TOOL_NAME, json!({})))
            .await
            .expect("a status report");

        assert!(
            result.content.contains("child-1 (fake) completed: read them"),
            "{}",
            result.content
        );
        assert!(result.content.contains("read the notes"), "{}", result.content);
        assert_eq!(result.status, ToolStatus::Completed);
    }

    #[tokio::test]
    async fn status_says_so_when_the_session_has_delegated_nothing() {
        let provider =
            waiting(Arc::new(Children::new()), Arc::new(agent_testkit::FakeEventStore::new()));
        let result = provider
            .invoke(&ToolInvocation::proposed(STATUS_TOOL_NAME, json!({})))
            .await
            .expect("a status report");
        assert!(result.content.contains("delegated to no child"), "{}", result.content);
        assert!(
            result.content.contains("subagent.run"),
            "it says how to start one: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn status_reports_a_child_that_has_ended_but_not_been_recorded() {
        // The window between a child ending and someone collecting it is real:
        // the status report is the only place a model can see it, and calling it
        // "running" would be wrong in the direction that matters.
        let (store, children, backend, child) =
            running(SubagentOutcome::Completed { summary: "done".to_string(), result: None });
        append(
            &store,
            &SessionId::from_string("parent"),
            EventType::SUBAGENT_SPAWNED,
            json!({
                "child_session_id": child.id().as_str(),
                "provider": "fake",
                "request": { "objective": "do the thing" },
            }),
        )
        .await;
        backend.release();
        // Let the background task publish the ending.
        while child.ending_now().is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let provider = waiting(children, store);
        let result = provider
            .invoke(&ToolInvocation::proposed(STATUS_TOOL_NAME, json!({})))
            .await
            .expect("a status report");

        assert!(
            result.content.contains("completed: done (not yet recorded)"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_name_this_provider_does_not_implement_names_the_ones_it_does() {
        let provider =
            waiting(Arc::new(Children::new()), Arc::new(agent_testkit::FakeEventStore::new()));
        let error = provider
            .invoke(&ToolInvocation::proposed("subagent.cancel", json!({})))
            .await
            .expect_err("an unimplemented name was accepted");
        let message = error.to_string();
        assert!(message.contains(TOOL_NAME), "{message}");
        assert!(message.contains(WAIT_TOOL_NAME), "{message}");
        assert!(message.contains(STATUS_TOOL_NAME), "{message}");
    }

    #[test]
    fn the_collection_tools_are_held_to_the_capability_that_allows_delegating() {
        // A run that may not delegate has no delegation of its own to read, and
        // a tool that declares nothing is refused by the policy engine as surely
        // as one whose rules forbid it — so all three name the same capability
        // rather than leaving it to the gateway to notice.
        let provider =
            waiting(Arc::new(Children::new()), Arc::new(agent_testkit::FakeEventStore::new()));
        let definitions = provider.definitions();
        assert_eq!(
            definitions.len(),
            3,
            "{:?}",
            definitions.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
        for definition in &definitions {
            assert_eq!(definition.capability_requirements.len(), 1);
            assert_eq!(definition.capability_requirements[0].as_str(), REQUIRED_CAPABILITY);
        }
        // The collection verbs touch nothing, and say so: the boundary treats
        // "no answer" as a write.
        assert_eq!(provider.side_effect(WAIT_TOOL_NAME), Some(ToolSideEffect::Pure));
        assert_eq!(provider.side_effect(STATUS_TOOL_NAME), Some(ToolSideEffect::Pure));
        assert_eq!(provider.side_effect(TOOL_NAME), Some(ToolSideEffect::Destructive));
    }

    async fn append(
        store: &agent_testkit::FakeEventStore,
        session: &SessionId,
        event_type: &str,
        payload: serde_json::Value,
    ) {
        let seq = store
            .read_stream(session, EventSeq::ZERO, usize::MAX)
            .await
            .expect("readable")
            .last()
            .map(|event| event.seq.next())
            .unwrap_or(EventSeq::ZERO);
        let draft = EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::current(),
            session_id: session.clone(),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::System,
            created_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: json!({}),
            ignorable: false,
        };
        store.append(session, seq, vec![draft]).await.expect("an append");
    }

    /// A provider over a log this test fills, for the admission checks.
    ///
    /// Nothing here ever reaches a child: every case below is decided before
    /// the fork, which is the point of admitting a step at all.
    fn admitting(store: Arc<agent_testkit::FakeEventStore>) -> SubagentToolProvider {
        SubagentToolProvider::new(
            store,
            Arc::new(agent_testkit::FakeToolProvider::new()),
            SessionId::from_string("parent"),
            &[config("fake", None)],
            Arc::new(Children::new()),
            Delegation::Background,
        )
    }

    async fn plan_of(store: &agent_testkit::FakeEventStore, plan: serde_json::Value) {
        append(
            store,
            &SessionId::from_string("parent"),
            EventType::PLAN_UPDATED,
            json!({ "plan": plan }),
        )
        .await;
    }

    /// The step two of the tests below share: open, and waiting on `read`.
    fn a_graph() -> serde_json::Value {
        json!({
            "goal": "ship it",
            "items": [
                { "text": "read the failing test", "status": "done", "id": "read" },
                { "text": "fix the tokenizer", "status": "todo", "id": "fix", "depends_on": ["read"] },
                { "text": "run the suite", "status": "todo", "id": "test", "depends_on": ["fix"] },
            ],
        })
    }

    #[tokio::test]
    async fn a_step_is_refused_while_the_step_it_waits_on_is_open() {
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        plan_of(store.as_ref(), a_graph()).await;

        let error = admitting(store)
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "run it", "step": "test" }),
            ))
            .await
            .expect_err("a step the plan has not reached was admitted");

        let message = error.to_string();
        assert!(message.contains("waits on fix"), "{message}");
        assert!(message.contains("not done"), "{message}");
    }

    #[tokio::test]
    async fn a_step_one_worker_already_carries_refuses_a_second() {
        // Two children on one step is the same work done twice, and the second
        // one is invisible in the task layer: the fold keeps the first child's
        // entry and the second only looks like a delegation.
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        plan_of(store.as_ref(), a_graph()).await;
        append(
            store.as_ref(),
            &SessionId::from_string("parent"),
            EventType::SUBAGENT_SPAWNED,
            json!({
                "child_session_id": "child-1",
                "provider": "fake",
                "request": { "objective": "fix it", "step": "fix" },
            }),
        )
        .await;

        let error = admitting(store)
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "fix it again", "step": "fix" }),
            ))
            .await
            .expect_err("a step with a worker was given another");

        let message = error.to_string();
        assert!(message.contains("already has a worker: subagent child-1"), "{message}");
    }

    #[tokio::test]
    async fn a_step_the_plan_does_not_name_lists_the_ones_it_does() {
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        plan_of(store.as_ref(), a_graph()).await;

        let error = admitting(store)
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "x", "step": "deploy" }),
            ))
            .await
            .expect_err("a step the plan does not have was admitted");

        let message = error.to_string();
        assert!(message.contains("read, fix, test"), "{message}");
    }

    #[tokio::test]
    async fn naming_a_step_in_a_session_with_no_plan_says_to_write_one() {
        let store = Arc::new(agent_testkit::FakeEventStore::new());

        let error = admitting(store)
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "x", "step": "fix" }),
            ))
            .await
            .expect_err("a step was admitted with no plan at all");

        let message = error.to_string();
        assert!(message.contains("names no step ids"), "{message}");
        assert!(message.contains("plan.update"), "{message}");
    }

    #[tokio::test]
    async fn a_step_that_is_done_or_already_doing_is_not_a_worker_s_step() {
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        plan_of(
            store.as_ref(),
            json!({
                "goal": "ship it",
                "items": [
                    { "text": "read", "status": "done", "id": "read" },
                    { "text": "fix", "status": "doing", "id": "fix" },
                ],
            }),
        )
        .await;
        let provider = admitting(store);

        for (step, expected) in [("read", "already done"), ("fix", "already doing")] {
            let error = provider
                .invoke(&ToolInvocation::proposed(
                    TOOL_NAME,
                    json!({ "provider": "fake", "objective": "x", "step": step }),
                ))
                .await
                .expect_err("a step with no work left was given a worker");
            assert!(error.to_string().contains(expected), "{step}: {error}");
        }
    }

    #[tokio::test]
    async fn a_ready_step_passes_admission_and_the_request_carries_it() {
        // The gate opening is what this checks: what happens next is the fork,
        // and this session has no closed turn for one, so the call fails there
        // rather than here — with a reason that says so.
        let store = Arc::new(agent_testkit::FakeEventStore::new());
        plan_of(store.as_ref(), a_graph()).await;
        let provider = admitting(store);

        provider.admit_step("fix").await.expect("a ready step is admitted");

        let error = provider
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "fix it", "step": "fix" }),
            ))
            .await
            .expect_err("no closed turn to fork from");
        assert!(!error.to_string().contains("already"), "the gate let it through: {error}");
    }

    #[test]
    fn a_request_that_names_a_step_carries_it_and_one_that_does_not_stays_byte_identical() {
        let named: RunArgs =
            serde_json::from_value(json!({ "provider": "fake", "objective": "x", "step": "fix" }))
                .expect("arguments");
        assert_eq!(named.into_request().step.as_deref(), Some("fix"));

        let plain: RunArgs =
            serde_json::from_value(json!({ "provider": "fake", "objective": "x" }))
                .expect("arguments");
        let request = plain.into_request();
        assert_eq!(request.step, None);
        assert!(
            !serde_json::to_string(&request).expect("serialize").contains("step"),
            "an unnamed step is not written at all"
        );
    }

    #[test]
    fn the_receipt_names_the_step_the_child_carries() {
        let child = SessionId::from_string("child-1");
        assert!(receipt(&child, Some("fix")).contains("It carries step \"fix\""));
        assert!(!receipt(&child, None).contains("carries"), "a plain receipt is unchanged");
    }
}
