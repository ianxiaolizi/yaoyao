//! The GUI's driver: the same commands as the terminal, one cached stack per
//! session.
//!
//! The HTTP layer must not assemble a model, a sandbox or a plugin by itself.
//! That is why [`WebBackend`] lives in the binary rather than in `agent-web`: it
//! owns the one assembly (`stack::assemble`) and the one cache of the runtimes it
//! produced. A turn requested from the page and a turn requested with `agent ask`
//! therefore write the same events, and an approval settles through the same
//! `ApprovalService` and `Resume` the CLI uses.
//!
//! Two locks live here, and neither is a new authority:
//!
//! - one `async` mutex per session, held across a turn or a resume, so this
//!   process cannot start two of them. It is a fast path, not the rule: the
//!   rule is the driver claim the Scheduler and `Resume` take, which is what
//!   keeps *another process* from starting a second turn on the same session;
//! - the store's own lease, opened by `Resume`, which is what keeps another
//!   process out while a granted call runs.
//!
//! The runtime cache exists because MCP servers are child processes. Assembling
//! once per request would restart them on every click. It is bounded by the
//! sessions a person actually opens in one server lifetime, not by the log.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_config::ResolvedProfile;
use agent_kernel::model_selection::{self, ModelChoice};
use agent_ports::event_store::EventStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::live_event_bus::{LiveEventBus, LiveEventSubscription};
use agent_protocol::events::Actor;
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::model::ModelStreamEvent;
use agent_web::{Backend, BackendError, Decision, LiveChunk, LiveSink};
use futures_util::FutureExt;
use tokio::sync::Mutex;

use crate::stack::{self, Decisions, Runtime};

/// Classify a command failure for the page.
///
/// One refusal is not this request's failure: another caller is already
/// driving the session. The page shows that as "busy" — the same answer a
/// second `agent ask` gets on the terminal — rather than as a broken server,
/// and the message names the other driver's pid so a person can go and look.
fn failure(error: agent_cli::commands::CommandError) -> BackendError {
    match error {
        agent_cli::commands::CommandError::Scheduler(
            busy @ agent_kernel::scheduler::SchedulerError::SessionBusy { .. },
        ) => BackendError::busy(busy.to_string()),
        other => BackendError::internal(other.to_string()),
    }
}

/// A resume's failure, keeping "somebody else is driving" a class of its own.
fn resume_failure(
    context: &str,
    error: agent_kernel::resume::ResumeError,
) -> agent_cli::commands::CommandError {
    match error {
        agent_kernel::resume::ResumeError::SessionBusy { session_id, holder } => {
            agent_cli::commands::CommandError::Scheduler(
                agent_kernel::scheduler::SchedulerError::SessionBusy { session_id, holder },
            )
        }
        other => agent_cli::commands::CommandError::Invalid(format!("{context}: {other}")),
    }
}

/// What every session's stack shares.
///
/// The resolved configuration is cloned per session, not shared: the scheduler
/// freezes it into each run's profile snapshot, and two sessions may open at
/// different times while the files on disk change. The host decisions (sandbox,
/// network, judgment arm) are fixed when the server starts, which is what makes
/// the page's `/api/meta` answer true for every turn it accepts.
#[derive(Clone)]
pub struct WebInputs {
    pub workspace_root: PathBuf,
    pub data_dir: PathBuf,
    pub resolved: ResolvedProfile,
    pub sandbox: SandboxMode,
    pub network_allowed: bool,
    pub judgment: bool,
    /// The provider `agent web --provider` named, or the environment's default.
    /// Kept as a name rather than an assembled provider so every session's
    /// stack builds its own client from the same decision.
    pub provider: String,
    /// The log a session's own model choice is read from. The choice is a
    /// decision about the next turn and lives in the session's log, so the
    /// server only has to know how to read it back.
    pub store: Arc<dyn EventStore>,
}

/// One cached runtime per session, plus the lock that serializes its work.
///
/// The cache is what keeps an MCP server from being restarted on every click.
/// It is bounded by idle time rather than by a count: a person may keep several
/// sessions open, and the one they are not looking at is the one whose child
/// processes should go.
pub struct WebBackend {
    inputs: WebInputs,
    runtimes: Mutex<HashMap<String, (Arc<Runtime>, Instant)>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl WebBackend {
    pub fn new(inputs: WebInputs) -> Self {
        Self { inputs, runtimes: Mutex::new(HashMap::new()), sessions: Mutex::new(HashMap::new()) }
    }

    /// Start the idle-runtime reaper on the current tokio runtime.
    ///
    /// A weak reference means the loop ends when the server does, rather than
    /// keeping the backend (and its children) alive from a detached task.
    pub fn start_reaper(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(backend) = weak.upgrade() else {
                    break;
                };
                backend.evict_idle().await;
            }
        });
    }

    /// Drop runtimes no operation has touched for [`IDLE_RUNTIME`].
    ///
    /// A runtime still held by a turn survives in that turn's `Arc`; removing
    /// the cache entry only means the next command assembles a fresh one. That
    /// is the right trade for a child process that would otherwise live until
    /// the terminal closes.
    async fn evict_idle(&self) {
        let now = Instant::now();
        let mut runtimes = self.runtimes.lock().await;
        let stale: Vec<String> = runtimes
            .iter()
            .filter(|(_, (_, last_used))| now.duration_since(*last_used) >= IDLE_RUNTIME)
            .map(|(session, _)| session.clone())
            .collect();
        let dropped: Vec<(Arc<Runtime>, Instant)> =
            stale.iter().filter_map(|session| runtimes.remove(session)).collect();
        // Drop outside the map lock: a Runtime owns child processes whose
        // destructors may wait, and the next request should not queue behind
        // that while the cache is otherwise free to read.
        drop(runtimes);
        drop(dropped);
    }

    /// The per-session serialization point.
    async fn session_lock(&self, session: &SessionId) -> Arc<Mutex<()>> {
        let mut sessions = self.sessions.lock().await;
        sessions
            .entry(session.as_str().to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// The model this session is set to use.
    ///
    /// Its own most recent choice wins; a session that never made one runs on
    /// the decision the server started with. Reading the log for this is cheap
    /// next to assembling a stack, and it happens only when a runtime is built.
    async fn model_for(&self, session: &SessionId) -> Result<ModelChoice, BackendError> {
        let events = self
            .inputs
            .store
            .read_stream(session, EventSeq::ZERO, usize::MAX)
            .await
            .map_err(|error| BackendError::internal(error.to_string()))?;
        let chosen = model_selection::current(&events)
            .map_err(|error| BackendError::internal(format!("model selection: {error}")))?;
        Ok(chosen.unwrap_or_else(|| {
            ModelChoice::new(self.inputs.provider.clone(), self.inputs.resolved.model.clone())
        }))
    }

    /// Assemble this session's stack once, and reuse it while it stays warm.
    async fn runtime(&self, session: &SessionId) -> Result<Arc<Runtime>, BackendError> {
        // Held across assembly on purpose: two requests for the same session
        // must not both spawn a server or both build an approval service.
        let mut runtimes = self.runtimes.lock().await;
        if let Some((runtime, last_used)) = runtimes.get_mut(session.as_str()) {
            *last_used = Instant::now();
            return Ok(runtime.clone());
        }

        // Read only when a stack is actually being built: a warm session must
        // not pay a full log read on every turn for a choice that cannot have
        // changed its cached runtime anyway (`set_model` drops the entry).
        let choice = self.model_for(session).await?;
        let inputs = self.inputs.clone();
        let model = crate::providers::select_kind(&choice.provider, choice.model.as_deref())
            .map_err(|error| BackendError::internal(error.to_string()))?;
        let built = stack::assemble(stack::Inputs {
            session: session.clone(),
            workspace_root: inputs.workspace_root,
            data_dir: inputs.data_dir,
            resolved: inputs.resolved,
            model: Some(model),
            decisions: Decisions::FromConfiguration,
            sandbox: inputs.sandbox,
            network_allowed: inputs.network_allowed,
            judgment: inputs.judgment,
        })
        .await
        .map_err(|error| BackendError::internal(error.to_string()))?;

        let runtime = Arc::new(built);
        runtimes.insert(session.as_str().to_string(), (runtime.clone(), Instant::now()));
        Ok(runtime)
    }
}

#[async_trait::async_trait]
impl Backend for WebBackend {
    async fn run_turn(
        &self,
        session: &SessionId,
        text: &str,
        live: &LiveSink,
    ) -> Result<(), BackendError> {
        let lock = self.session_lock(session).await;
        let _guard = lock.lock().await;
        let runtime = self.runtime(session).await?;

        if let Err(error) = runtime.recovery.recover(session).await {
            return Err(BackendError::internal(format!("recovery failed: {error}")));
        }

        let subscription = runtime
            .live
            .subscribe()
            .await
            .map_err(|error| BackendError::internal(error.to_string()))?;
        let turn =
            agent_cli::commands::start_turn(&runtime.scheduler, session, text, crate::clock::now());
        let outcome = drive(turn, subscription, live).await;
        match outcome {
            Ok(agent_cli::commands::TurnStart::Ran(_)) => Ok(()),
            Ok(agent_cli::commands::TurnStart::NotStarted) => Err(BackendError::busy(
                "a run is already active, or the prompt was claimed elsewhere",
            )),
            Err(error) => Err(failure(error)),
        }
    }

    async fn settle_approval(
        &self,
        session: &SessionId,
        approval_id: &str,
        decision: Decision,
        live: &LiveSink,
    ) -> Result<(), BackendError> {
        let lock = self.session_lock(session).await;
        let _guard = lock.lock().await;
        let runtime = self.runtime(session).await?;

        if let Err(error) = runtime.recovery.recover(session).await {
            return Err(BackendError::internal(format!("recovery failed: {error}")));
        }
        let subscription = runtime
            .live
            .subscribe()
            .await
            .map_err(|error| BackendError::internal(error.to_string()))?;

        let work = async {
            match decision {
                Decision::Deny => {
                    agent_cli::commands::deny_approval(
                        runtime.approvals.as_ref(),
                        runtime.scheduler.inbox(),
                        session,
                        approval_id,
                        false,
                        "user",
                    )
                    .await?;
                }
                Decision::Once | Decision::Turn | Decision::Session => {
                    let grant = match decision {
                        Decision::Once => agent_cli::commands::Grant::Once,
                        Decision::Turn => agent_cli::commands::Grant::Turn,
                        _ => agent_cli::commands::Grant::Session,
                    };
                    agent_cli::commands::grant_approval(
                        runtime.approvals.as_ref(),
                        runtime.scheduler.inbox(),
                        session,
                        approval_id,
                        grant,
                        "user",
                    )
                    .await?;
                }
            }

            let resumed = runtime.resume.resume(session, "").await.map_err(|error| {
                resume_failure(
                    "the approval was recorded, but the turn could not be resumed",
                    error,
                )
            })?;
            match resumed {
                agent_kernel::resume::ResumeOutcome::Settled { .. } => {
                    // The resumed turn is a real turn: drive it to its next
                    // stopping point. A failure here is the run's, not the
                    // approval's; the decision is already durable.
                    runtime.scheduler.run_pending(session, crate::clock::now()).await.map_err(
                        |error| {
                            agent_cli::commands::CommandError::Invalid(format!(
                                "the approval was recorded, but the continuation did not run: \
                                 {error}"
                            ))
                        },
                    )?;
                    Ok(())
                }
                // Another request of the same kind is still waiting; the
                // person will see its card and answer it next.
                agent_kernel::resume::ResumeOutcome::Waiting { .. } => Ok(()),
                agent_kernel::resume::ResumeOutcome::NothingToResume => {
                    Err(agent_cli::commands::CommandError::Invalid(
                        "the approval was recorded, but no yielded turn was waiting".to_string(),
                    ))
                }
                agent_kernel::resume::ResumeOutcome::Unresumable { turn_id, reason } => {
                    Err(agent_cli::commands::CommandError::Invalid(format!(
                        "the approval was recorded, but turn {turn_id} could not be resumed: \
                         {reason}"
                    )))
                }
            }
        };

        match drive(work, subscription, live).await {
            Ok(()) => Ok(()),
            Err(error) => {
                if error.is_busy() {
                    return Err(BackendError::busy(error.to_string()));
                }
                let message = error.to_string();
                if message.contains("no yielded turn")
                    || message.contains("no longer waiting")
                    || message.contains("approval")
                {
                    Err(BackendError::not_pending(message))
                } else {
                    Err(BackendError::internal(message))
                }
            }
        }
    }

    async fn resume(&self, session: &SessionId, live: &LiveSink) -> Result<(), BackendError> {
        let lock = self.session_lock(session).await;
        let _guard = lock.lock().await;
        let runtime = self.runtime(session).await?;

        if let Err(error) = runtime.recovery.recover(session).await {
            return Err(BackendError::internal(format!("recovery failed: {error}")));
        }
        let subscription = runtime
            .live
            .subscribe()
            .await
            .map_err(|error| BackendError::internal(error.to_string()))?;

        let work = async {
            let resumed =
                runtime.resume.resume(session, "").await.map_err(|error| {
                    resume_failure("the yielded turn could not be resumed", error)
                })?;
            match resumed {
                agent_kernel::resume::ResumeOutcome::Settled { .. } => {
                    runtime.scheduler.run_pending(session, crate::clock::now()).await.map_err(
                        |error| {
                            agent_cli::commands::CommandError::Invalid(format!(
                                "the expired wait was settled, but the continuation did not run: \
                                 {error}"
                            ))
                        },
                    )?;
                    Ok(())
                }
                agent_kernel::resume::ResumeOutcome::Waiting { .. } => Ok(()),
                agent_kernel::resume::ResumeOutcome::NothingToResume => {
                    Err(agent_cli::commands::CommandError::Invalid(
                        "nothing to resume: no turn is waiting on a decision".to_string(),
                    ))
                }
                agent_kernel::resume::ResumeOutcome::Unresumable { turn_id, reason } => {
                    Err(agent_cli::commands::CommandError::Invalid(format!(
                        "turn {turn_id} could not be resumed: {reason}"
                    )))
                }
            }
        };

        match drive(work, subscription, live).await {
            Ok(()) => Ok(()),
            Err(error) => Err(failure(error)),
        }
    }

    async fn fork(
        &self,
        session: &SessionId,
        at: Option<&str>,
        reason: &str,
    ) -> Result<Option<SessionId>, BackendError> {
        let runtime = self.runtime(session).await?;
        let (_, child) = agent_cli::run_fork(&runtime.fork, session, at, reason, true)
            .await
            .map_err(BackendError::internal)?;
        Ok(child)
    }

    async fn steer(&self, session: &SessionId, text: &str) -> Result<(), BackendError> {
        // No session lock: the whole point is to reach a turn that is already
        // holding it. The command reads the turn id from the log and appends an
        // inbox item; the kernel applies it at the next step boundary.
        let runtime = self.runtime(session).await?;
        agent_cli::commands::steer(&runtime.scheduler, session, text)
            .await
            .map(|_| ())
            .map_err(|error| BackendError::internal(error.to_string()))
    }

    async fn interrupt(&self, session: &SessionId) -> Result<bool, BackendError> {
        let runtime = self.runtime(session).await?;
        let outcome = agent_cli::commands::interrupt(&runtime.scheduler, session)
            .await
            .map_err(|error| BackendError::internal(error.to_string()))?;
        Ok(matches!(outcome, agent_cli::commands::InterruptOutcome::Queued(_)))
    }

    async fn set_model(
        &self,
        session: &SessionId,
        provider: &str,
        model: Option<&str>,
    ) -> Result<ModelChoice, BackendError> {
        // Build it once before writing it down. A stored choice this process
        // cannot honour would break every later turn on the session, so an
        // unknown provider or an unreadable endpoint is refused here, where the
        // person can still pick another one.
        crate::providers::select_kind(provider, model)
            .map_err(|error| BackendError::new("invalid_model", error.to_string()))?;

        let choice = ModelChoice::new(provider, model.map(str::to_string));
        model_selection::select(self.inputs.store.clone(), session, choice.clone(), Actor::User)
            .await
            .map_err(|error| {
                BackendError::internal(format!("the model choice could not be recorded: {error}"))
            })?;

        // The cached stack was assembled around the old provider. Dropping the
        // entry makes the next turn build a new one; a turn already in flight
        // holds its own `Arc` and finishes on the model it started with.
        self.runtimes.lock().await.remove(session.as_str());
        Ok(choice)
    }

    async fn delete_session(&self, session: &SessionId) -> Result<(), BackendError> {
        // The session lock is tried, not taken: a delete that waited for a
        // ten-minute turn would look like a hung button, and a delete that
        // removed the log under a running turn would leave that turn appending
        // to a session nobody can see. Refusing is the honest answer, and the
        // page can say so.
        let lock = self.session_lock(session).await;
        let Ok(_guard) = lock.try_lock() else {
            return Err(BackendError::busy("a turn is running in this session"));
        };

        // Drop the stack before the log: it owns this session's MCP children,
        // and they must not keep running for a session that no longer exists.
        // Its file handles are what would otherwise keep the directory from
        // being removed on Windows.
        let dropped = self.runtimes.lock().await.remove(session.as_str());
        drop(dropped);

        self.inputs.store.delete_session(session).await.map_err(|error| {
            BackendError::internal(format!("the session log could not be removed: {error}"))
        })?;
        self.sessions.lock().await.remove(session.as_str());
        Ok(())
    }
}

/// Watch a turn's live text while it runs, then drain what was already queued.
///
/// The final answer is durable and will arrive through the log even if the last
/// delta is lost here; this is a preview, not a fact. The drain matters because
/// a turn can return in the same poll that published its last chunk, and
/// dropping the queue would make the page's text jump backwards.
async fn drive<F, T>(future: F, mut live: Box<dyn LiveEventSubscription>, sink: &LiveSink) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(future);
    let output = loop {
        tokio::select! {
            output = &mut future => break output,
            event = live.next() => match event {
                Some(event) => publish(&event, sink),
                None => {
                    // The bus is gone; the durable log still has the answer.
                    return future.await;
                }
            }
        }
    };

    while let Some(Some(event)) = live.next().now_or_never() {
        publish(&event, sink);
    }
    output
}

/// Turn one live event into the page's preview shape.
fn publish(event: &agent_protocol::events::LiveEvent, sink: &LiveSink) {
    let chunk = match serde_json::from_value::<ModelStreamEvent>(event.payload.clone()) {
        Ok(ModelStreamEvent::TextDelta { text }) => LiveChunk::text(text),
        Ok(ModelStreamEvent::ReasoningDelta { text }) => LiveChunk::reasoning(text),
        _ => return,
    };
    // A sender with no SSE receiver is not an error: the page may have been
    // closed, and the durable items will be read by the next connection.
    let _ = sink.send(chunk);
}

/// How long a session's stack may sit unused before its child processes are
/// allowed to go. Long enough to switch back to a session and continue, short
/// enough that an afternoon of browsing does not leave every MCP server alive.
const IDLE_RUNTIME: Duration = Duration::from_secs(600);
