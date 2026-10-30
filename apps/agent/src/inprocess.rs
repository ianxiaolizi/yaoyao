//! The in-process backend: a child that is a turn this build runs itself.
//!
//! Every other backend starts a program and speaks a protocol to it. This one
//! runs the child as a second session on the same store, driven by the same
//! scheduler that drives the parent — which is what makes "a child's history is
//! a fork of the parent's" true rather than a claim in a header: the history the
//! delegation forked is the history the child's own turn reads (CONTRACT §7
//! #133).
//!
//! ## What a child may do
//!
//! A child turn is **read-only**. Its surface holds the tools whose side effect
//! is `pure` or `read` and nothing else, and the delegation tools are not on it
//! at all — they are assembled beside the built-ins, not in them — so a child
//! cannot delegate in turn. That is not a policy the child is asked to respect;
//! it is the surface it is given.
//!
//! The reason is the one thing an in-process child cannot have: a person. A
//! child that stopped to ask would be asking nobody, and its turn would stand
//! paused in a session no one is watching. So the surface is narrowed to what
//! needs no consent, and the policy the gateway consults is narrowed with it:
//! [`never_asking`] keeps the parent's unconditional *allows* and turns every
//! question the parent would have stopped to ask into a refusal. Where the
//! parent's policy would stop and ask, the child's says no.
//!
//! ## What the kernel can say about it
//!
//! The budget a request names is enforced by the scheduler and the loop that
//! carry it — not by a program's cooperation — and the deadline is enforced
//! here, by interrupting the child's turn at its next step boundary
//! ([`Inbox::interrupt`](agent_kernel::inbox::Inbox::interrupt)), which is how
//! a run is asked to stop everywhere else. [`SubagentProvider::supported`]
//! claims exactly those two and refuses `tool_allowlist`, `model_route` and
//! `output_schema` at negotiation, before the child's history exists: a backend
//! that accepted a schema and dropped it would hand back an answer that looks
//! conformant and is not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent_kernel::policy::{PolicyEngine, PolicyRule};
use agent_kernel::scheduler::Scheduler;
use agent_ports::agent_loop::StopReason;
use agent_ports::subagent::{
    SubagentError, SubagentFeature, SubagentFeatures, SubagentHandle, SubagentOutcome,
    SubagentProvider, SubagentRequest,
};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
};
use agent_protocol::events::Actor;
use agent_protocol::ids::{SessionId, TurnId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::permissions::{Capability, PolicyDecision};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, timeout};

/// How long a child of this backend may run when the configuration states no
/// ceiling.
///
/// The same number the stdio backend uses: a child with no deadline is a run
/// nothing bounds, and "no ceiling configured" must not mean that.
pub const DEFAULT_CEILING: Duration = Duration::from_secs(300);

/// How long an interrupt is given to end the child's turn before the stop is
/// reported as unconfirmed.
///
/// The child is only ever interruptible at a step boundary, so the bound has to
/// cover one step: a model request already in flight completes first. Past this
/// the backend says so rather than claiming a stop it has not seen.
const STOP_BOUND: Duration = Duration::from_secs(10);

/// Why this backend asked a child to stop, when it asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// The parent's `cancel`.
    Cancelled,
    /// The child's deadline passed.
    Deadline,
}

/// The in-process backend: one scheduler, and a child per spawn.
///
/// The scheduler is installed once, at the end of assembly, because a child's
/// run is built from its parent's and the parent's cannot exist before the tool
/// surface this backend is part of. Until then the backend is on the surface but
/// every spawn refuses — fail closed, because a delegation that silently did
/// nothing is worse than one that says why it cannot.
pub struct InProcessSubagentProvider {
    name: String,
    /// The longest a child may live. A request may ask for less, never more.
    ceiling: Duration,
    /// The run a child executes on: the parent's scheduler with the child's loop
    /// in place of the parent's. Filled once, at the end of assembly.
    template: OnceLock<Arc<Scheduler>>,
    children: Mutex<HashMap<String, ChildState>>,
}

struct ChildState {
    cancel: mpsc::Sender<()>,
    outcome: watch::Receiver<Option<SubagentOutcome>>,
}

impl InProcessSubagentProvider {
    /// A backend that will run children under `ceiling` (or [`DEFAULT_CEILING`]).
    pub fn new(name: impl Into<String>, ceiling: Option<Duration>) -> Self {
        Self {
            name: name.into(),
            ceiling: ceiling.unwrap_or(DEFAULT_CEILING),
            template: OnceLock::new(),
            children: Mutex::new(HashMap::new()),
        }
    }

    /// Hand this backend the run its children execute on.
    ///
    /// Returns whether the install landed: a second install would silently
    /// replace the run the first children were started on, and the first
    /// children's turns would still be executing on it.
    pub fn install(&self, template: Arc<Scheduler>) -> bool {
        self.template.set(template).is_ok()
    }

    /// The deadline this request will actually be held to.
    ///
    /// A ceiling rather than a default, exactly as the stdio backend reads it: a
    /// request may ask for less and never for more.
    fn effective_timeout(&self, request: &SubagentRequest) -> Duration {
        match request.timeout_ms {
            // Zero is read as "unset": a child given no time at all is never
            // what a caller meant.
            Some(0) | None => self.ceiling,
            Some(ms) => Duration::from_millis(ms).min(self.ceiling),
        }
    }

    /// The child table, or the error a poisoned lock means.
    ///
    /// Read through no poison, for the reason the stdio backend gives: a table
    /// that answers `Unknown` for a child that is there cannot be told apart
    /// from one that answers truly.
    fn children(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, ChildState>>, SubagentError> {
        self.children.lock().map_err(|_| SubagentError::Lost {
            provider: self.name.clone(),
            message: "a panic elsewhere in the process poisoned the child table".to_string(),
        })
    }

    fn state(
        &self,
        handle: &SubagentHandle,
    ) -> Result<(mpsc::Sender<()>, watch::Receiver<Option<SubagentOutcome>>), SubagentError> {
        self.children()?
            .get(handle.child_session_id.as_str())
            .map(|state| (state.cancel.clone(), state.outcome.clone()))
            .ok_or_else(|| SubagentError::Unknown {
                provider: self.name.clone(),
                child_session_id: handle.child_session_id.clone(),
            })
    }
}

#[async_trait::async_trait]
impl SubagentProvider for InProcessSubagentProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn supported(&self) -> SubagentFeatures {
        [SubagentFeature::Budget, SubagentFeature::Timeout].into_iter().collect()
    }

    async fn spawn(
        &self,
        child_session_id: &SessionId,
        parent_turn_id: Option<&TurnId>,
        request: SubagentRequest,
    ) -> Result<SubagentHandle, SubagentError> {
        // Negotiation first, and it costs nothing: a request this backend cannot
        // honour never reaches the point where a child's turn is queued.
        let missing = self.supported().missing_from(&request.requested_features());
        if !missing.is_empty() {
            return Err(SubagentError::Unsupported {
                provider: self.name.clone(),
                features: missing.into_iter().collect(),
            });
        }

        let Some(template) = self.template.get().cloned() else {
            return Err(SubagentError::Spawn {
                child_session_id: child_session_id.clone(),
                message: "this process assembled no run for in-process children".to_string(),
            });
        };
        // The parent's terms with the request's budget: a request that states
        // none keeps the budget the profile gave the run, which is the only
        // stated limit there is.
        let scheduler = match &request.budget {
            Some(budget) => Arc::new(template.rebudgeted(*budget)),
            None => template,
        };
        let deadline_in = self.effective_timeout(&request);

        // The objective is the child's first turn. Queued before the child table
        // is touched, so a failure here is a child that never started rather
        // than one whose entry nobody can find.
        let mut text = request.objective.clone();
        if !request.context_refs.is_empty() {
            text.push_str(&format!(
                "\n\nThe session that delegated this named: {}",
                request.context_refs.join(", ")
            ));
        }
        scheduler
            .inbox()
            .append(child_session_id, InboxKind::NextTurn, json!({ "text": text }), 0, Actor::Agent)
            .await
            .map_err(|error| SubagentError::Spawn {
                child_session_id: child_session_id.clone(),
                message: format!("its first turn could not be queued: {error}"),
            })?;

        let mut children = self.children()?;
        let (cancel, cancelled) = mpsc::channel(1);
        let (outcomes, outcome) = watch::channel(None);
        tokio::spawn(supervise(
            scheduler,
            child_session_id.clone(),
            deadline_in,
            cancelled,
            outcomes,
        ));
        children.insert(child_session_id.to_string(), ChildState { cancel, outcome });

        Ok(SubagentHandle::new(
            self.name.clone(),
            child_session_id.clone(),
            parent_turn_id.cloned(),
        ))
    }

    async fn wait(&self, handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError> {
        let (_, mut outcome) = self.state(handle)?;
        settle(&mut outcome).await
    }

    async fn cancel(&self, handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError> {
        let (sender, mut outcome) = self.state(handle)?;
        // A child that has already ended is not asked to stop: the honest answer
        // to "make it stop" is how it stopped.
        if outcome.borrow().is_none() {
            // A closed channel means the supervisor is already finishing, which
            // means an outcome is on its way — waiting for it is the answer.
            let _ = sender.send(()).await;
        }
        settle(&mut outcome).await
    }

    async fn dispose(&self, handle: &SubagentHandle) -> Result<(), SubagentError> {
        let state = self.children()?.remove(handle.child_session_id.as_str());
        // Idempotent: the only reason to dispose twice is not knowing whether
        // the first call landed.
        let Some(state) = state else {
            return Ok(());
        };
        if state.outcome.borrow().is_none() {
            let _ = state.cancel.send(()).await;
            let mut outcome = state.outcome.clone();
            // Bounded, and the entry is released whether or not the stop is
            // confirmed: reporting an unconfirmed stop is `cancel`'s job.
            let _ = timeout(STOP_BOUND, settle(&mut outcome)).await;
        }
        Ok(())
    }
}

/// Wait until the child has ended, and say how.
///
/// The supervisor publishes one outcome and then stops, so a waiter either gets
/// that value or outlives the supervisor — and a supervisor that vanished
/// without a value is itself a fact about the child.
async fn settle(
    outcome: &mut watch::Receiver<Option<SubagentOutcome>>,
) -> Result<SubagentOutcome, SubagentError> {
    loop {
        let current = (*outcome.borrow()).clone();
        if let Some(outcome) = current {
            return Ok(outcome);
        }
        if outcome.changed().await.is_err() {
            return Ok(SubagentOutcome::Failed {
                message: "the child's supervisor stopped without saying how it ended".to_string(),
                side_effects: Vec::new(),
            });
        }
    }
}

/// Own one child from the turn it was queued with to the moment it ends.
async fn supervise(
    scheduler: Arc<Scheduler>,
    child: SessionId,
    deadline_in: Duration,
    mut cancelled: mpsc::Receiver<()>,
    outcomes: watch::Sender<Option<SubagentOutcome>>,
) {
    let run = scheduler.run_pending(&child, crate::clock::now());
    tokio::pin!(run);

    // The child's own end wins ties: a run that finished as the parent reached
    // for the brake is reported as itself rather than relabelled as a stop.
    let asked = tokio::select! {
        biased;
        report = &mut run => {
            let _ = outcomes.send(Some(ended(report, &child, None, deadline_in)));
            return;
        }
        _ = cancelled.recv() => Asked::Cancelled,
        _ = sleep(deadline_in) => Asked::Deadline,
    };

    // The request goes where the child's turn reads it — its own inbox, at the
    // next step boundary. A failed append is not fatal below: the run is still
    // given its bound, and an unconfirmed stop is what gets reported.
    let reason = match asked {
        Asked::Cancelled => "the parent asked this child to stop",
        Asked::Deadline => "the child's deadline passed",
    };
    let recorded = scheduler.inbox().interrupt(&child, reason).await.is_ok();

    let outcome = match timeout(STOP_BOUND, &mut run).await {
        Ok(report) => ended(report, &child, Some(asked), deadline_in),
        Err(_) => unconfirmed(asked, &child, recorded),
    };
    let _ = outcomes.send(Some(outcome));
}

/// What the parent is told when the child did not end inside the bound.
///
/// Whether the stop landed is exactly what the two variants say apart: a
/// cancellation this backend could not watch finish is `CancelledUncertain`, and
/// a deadline is `TimedOut` whose message says whether the turn is still open —
/// the difference a caller needs before reusing anything the child touched.
fn unconfirmed(asked: Asked, child: &SessionId, recorded: bool) -> SubagentOutcome {
    let why = if recorded {
        format!(
            "the interrupt was recorded but its turn did not end within {} s",
            STOP_BOUND.as_secs()
        )
    } else {
        "the interrupt could not be recorded".to_string()
    };
    match asked {
        Asked::Cancelled => SubagentOutcome::CancelledUncertain {
            message: format!("{why}; the child's turn may still be open in session {child}"),
        },
        Asked::Deadline => SubagentOutcome::TimedOut {
            message: format!(
                "the child's deadline passed and it did not stop: {why}; its turn is still open \
                 in session {child}"
            ),
        },
    }
}

/// How a child's turn ended, as the parent is told it.
///
/// `asked` is what this backend did, and it only becomes the answer when it is
/// what ended the turn: a child that answered a moment before the brake was
/// reached answered.
fn ended(
    report: Result<
        Option<agent_kernel::scheduler::RunReport>,
        agent_kernel::scheduler::SchedulerError,
    >,
    child: &SessionId,
    asked: Option<Asked>,
    deadline_in: Duration,
) -> SubagentOutcome {
    let report = match report {
        Ok(Some(report)) => report,
        Ok(None) => {
            return SubagentOutcome::Failed {
                message: format!(
                    "the child's turn ran nothing: session {child} had no pending work when this \
                     backend looked"
                ),
                side_effects: Vec::new(),
            };
        }
        Err(error) => {
            return SubagentOutcome::Failed {
                message: format!("the child's turn could not be run: {error}"),
                side_effects: Vec::new(),
            };
        }
    };
    match (asked, &report.stop_reason) {
        (Some(Asked::Cancelled), StopReason::UserCancelled) => SubagentOutcome::Cancelled,
        (Some(Asked::Deadline), StopReason::UserCancelled) => SubagentOutcome::TimedOut {
            message: format!(
                "the child was stopped after its deadline ({} ms) and ended at its next step \
                 boundary",
                deadline_in.as_millis()
            ),
        },
        _ => describe_report(&report, child),
    }
}

/// How a finished child turn reads to its parent.
///
/// Every ending that is not an answer is `Failed`, and the message says which
/// limit or fault it was and what the parent can do about it. `Partial` is
/// deliberately never produced: the port reserves it for a child that says it
/// did part of the job, and a parent that reads the child's stop reason still
/// cannot know how much of the work was done.
pub(crate) fn describe_report(
    report: &agent_kernel::scheduler::RunReport,
    child: &SessionId,
) -> SubagentOutcome {
    let said = report.final_content.clone().filter(|text| !text.trim().is_empty());
    let limit = |what: &str, advice: &str| {
        let mut message = format!("the child {what} before it finished. {advice}");
        if let Some(text) = &said {
            message.push_str(&format!(
                " What it had said is the beginning of an answer, not one: {text}"
            ));
        }
        SubagentOutcome::Failed { message, side_effects: Vec::new() }
    };
    match report.stop_reason {
        StopReason::FinalAnswer => SubagentOutcome::Completed {
            summary: said
                .unwrap_or_else(|| format!("the child finished without saying anything ({child})")),
            result: None,
        },
        StopReason::MaxSteps => limit(
            "used all its steps",
            "Give the delegation a larger `budget.max_steps` if the work needs more.",
        ),
        StopReason::MaxToolCalls => limit(
            "used its whole tool-call allowance",
            "Give the delegation a larger `budget.max_tool_calls` if the work needs more.",
        ),
        StopReason::WallClockExceeded => limit(
            "ran out of its own wall-clock budget",
            "Give the delegation a larger `budget.wall_clock_ms` if the work needs longer.",
        ),
        StopReason::MaxOutputTokens => limit(
            "was cut off at its output-token limit",
            "Give the delegation a larger `budget.max_output_tokens` if the answer needs more.",
        ),
        StopReason::ContentFiltered => SubagentOutcome::Failed {
            message: "the provider withheld the child's answer (content filter); there is no \
                      answer to read"
                .to_string(),
            side_effects: Vec::new(),
        },
        StopReason::NeedsUserInput => SubagentOutcome::Failed {
            message: format!(
                "the child stopped waiting on an approval nobody can answer — a child turn is \
                 read-only and asks nothing; its turn is still open in session {child}, and \
                 `agent resume` settles it"
            ),
            side_effects: Vec::new(),
        },
        StopReason::UserCancelled => SubagentOutcome::Cancelled,
        StopReason::FatalError => {
            let mut message = "the child's attempt failed".to_string();
            if let Some(text) = &said {
                message.push_str(&format!(": {text}"));
            }
            SubagentOutcome::Failed { message, side_effects: Vec::new() }
        }
    }
}

/// Whether a tool of this class belongs on a child's surface.
pub fn is_read_only(effect: ToolSideEffect) -> bool {
    matches!(effect, ToolSideEffect::Pure | ToolSideEffect::Read)
}

/// A surface with only the read-only tools on it.
///
/// The filter is the child's boundary, and it is deliberately a *surface*
/// rather than a call-time check: a write tool is not offered, so it is not
/// known to the gateway, so a call naming it is an ordinary unknown-tool refusal
/// that the model can read and act on. The `invoke` arm repeats the answer for a
/// caller that reaches this provider without the gateway, which this build does
/// not do — but a provider whose implementation disagreed with its own
/// definitions would be a boundary with a hole in it.
pub struct ReadOnlySurface {
    inner: Arc<dyn ToolProvider>,
}

impl ReadOnlySurface {
    pub fn new(inner: Arc<dyn ToolProvider>) -> Self {
        Self { inner }
    }

    fn offered(&self) -> Vec<String> {
        self.definitions().into_iter().map(|definition| definition.name).collect()
    }
}

#[async_trait::async_trait]
impl ToolProvider for ReadOnlySurface {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|definition| is_read_only(definition.side_effect))
            .collect()
    }

    /// The inner class, unchanged: the surface filters what is *offered*, not
    /// what a tool is. A caller asking about a write tool gets `write`, which is
    /// what makes it absent from [`definitions`](Self::definitions).
    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.inner.side_effect(tool_name)
    }

    fn path_arguments(&self, tool_name: &str) -> Vec<String> {
        self.inner.path_arguments(tool_name)
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        match self.inner.side_effect(&invocation.tool_name) {
            Some(effect) if is_read_only(effect) => self.inner.invoke(invocation).await,
            _ => Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: self.offered(),
            }),
        }
    }
}

/// A policy that never stops to ask: what the parent allows outright, and
/// nothing else.
///
/// For a child turn, whose question nobody would hear. Every capability of every
/// tool on `surface` is asked of `parent` with no context and no grants — the
/// one question that has a context-free answer — and a capability the parent
/// answers `allow` becomes a rule here while one it would ask about becomes a
/// managed deny. The engine built from those cannot produce `ask`: a
/// disposition is either stated or absent, and absence fails closed.
///
/// Narrowing only, never widening: a rule with a path or command matcher is not
/// reproduced, so anything the parent narrowed is absent here and refused as
/// ungranted. That is the direction the fail-closed reading runs in.
pub fn never_asking(
    parent: &PolicyEngine,
    surface: &dyn ToolProvider,
    advertised: &[Capability],
) -> PolicyEngine {
    let mut allowed: Vec<Capability> = Vec::new();
    let mut denied: Vec<Capability> = Vec::new();
    for definition in surface.definitions() {
        for capability in &definition.capability_requirements {
            let bucket = match parent.evaluate(std::slice::from_ref(capability), &[]) {
                PolicyDecision::Allow => &mut allowed,
                PolicyDecision::Ask | PolicyDecision::Deny => &mut denied,
            };
            if !bucket.contains(capability) {
                bucket.push(capability.clone());
            }
        }
    }
    let mut engine =
        PolicyEngine::new().with_sandbox(advertised.iter().cloned()).with_managed_deny(denied);
    for capability in allowed {
        engine = engine.with_rules([PolicyRule::allow(capability)]);
    }
    engine
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_kernel::scheduler::RunReport;
    use agent_loop::DefaultAgentLoop;
    use agent_ports::event_store::EventStore;
    use agent_ports::tool_provider::ToolSource;
    use agent_protocol::ids::{EventSeq, RunId};
    use agent_protocol::permissions::Capability as Cap;
    use agent_protocol::profile::RunBudget;
    use agent_testkit::{
        FakeEventStore, FakeToolGateway, FakeToolProvider, NullContextEngine, one_model_router,
    };

    fn child() -> SessionId {
        SessionId::from_string("child-1")
    }

    /// A child turn that runs for real: the objective is queued in the child's
    /// own inbox, the child's loop answers it, and the parent hears the answer.
    /// This is the whole point of the backend — the child's history is the
    /// session the delegation forked, not a message handed to a program.
    #[tokio::test]
    async fn a_child_turn_runs_in_its_own_session_and_answers_its_objective() {
        let store = Arc::new(FakeEventStore::new());
        let loop_ = Arc::new(DefaultAgentLoop::new(
            Arc::new(crate::providers::EchoModelProvider),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let provider = InProcessSubagentProvider::new("inner", None);
        assert!(provider.install(Arc::new(Scheduler::new(store.clone(), loop_, 8))));

        let handle = provider
            .spawn(&child(), None, SubagentRequest::new("look at the thing"))
            .await
            .expect("the backend was given a run");
        assert_eq!(handle.child_session_id, child());
        assert_eq!(handle.provider, "inner");

        let outcome = provider.wait(&handle).await.unwrap();
        assert_eq!(
            outcome,
            SubagentOutcome::Completed {
                summary: "echo: look at the thing".to_string(),
                result: None,
            }
        );
        provider.dispose(&handle).await.unwrap();
        // The handle is released, and disposing again is a no-op rather than an
        // error: the only reason to call it twice is not knowing it landed.
        assert!(provider.state(&handle).is_err());
        provider.dispose(&handle).await.unwrap();

        // The child's own log is where the turn happened.
        let types: Vec<String> = store
            .read_stream(&child(), EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| stored.event.event_type.to_string())
            .collect();
        assert!(types.contains(&"turn/completed".to_string()), "{types:?}");
    }

    /// Fail closed: a backend on the surface with no run behind it refuses the
    /// call and says why, rather than starting a child nothing can execute.
    #[tokio::test]
    async fn a_backend_nothing_installed_refuses_to_start_a_child() {
        let provider = InProcessSubagentProvider::new("inner", None);

        match provider.spawn(&child(), None, SubagentRequest::new("x")).await {
            Err(SubagentError::Spawn { message, .. }) => {
                assert!(message.contains("assembled no run"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// What this backend cannot honour is refused at negotiation, before the
    /// child's history exists — a schema dropped silently would hand back an
    /// answer that looks conformant and is not.
    #[tokio::test]
    async fn a_feature_this_backend_does_not_have_is_refused_before_anything_starts() {
        let provider = InProcessSubagentProvider::new("inner", None);
        let request = SubagentRequest {
            output_schema: Some(json!({ "type": "object" })),
            tool_allowlist: vec!["fs.read".to_string()],
            ..SubagentRequest::new("x")
        };

        match provider.spawn(&child(), None, request).await {
            Err(SubagentError::Unsupported { features, .. }) => {
                assert!(features.contains(SubagentFeature::OutputSchema), "{features}");
                assert!(features.contains(SubagentFeature::ToolAllowlist), "{features}");
            }
            other => panic!("expected an unsupported refusal, got {other:?}"),
        }
    }

    /// The two limits this backend claims are the two it enforces: a request
    /// that gives the child one step gets a child that took one, and the ending
    /// says so rather than reading as an answer.
    #[tokio::test]
    async fn a_requests_budget_is_the_budget_the_child_runs_under() {
        let store = Arc::new(FakeEventStore::new());
        let tools = FakeToolProvider::new().with_tool("fs.read", "one line", ToolSideEffect::Read);
        let loop_ = Arc::new(DefaultAgentLoop::new(
            Arc::new(agent_testkit::FakeModelProvider::new(vec![
                agent_testkit::FakeModelProvider::tool_call("fs.read", json!({ "path": "x" })),
                agent_testkit::FakeModelProvider::answer("the answer"),
            ])),
            Arc::new(FakeToolGateway::new(Arc::new(tools))),
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let provider = InProcessSubagentProvider::new("inner", None);
        assert!(provider.install(Arc::new(Scheduler::new(store.clone(), loop_, 8))));

        let handle = provider
            .spawn(
                &child(),
                None,
                SubagentRequest {
                    budget: Some(RunBudget { max_steps: 1, ..RunBudget::default() }),
                    ..SubagentRequest::new("look at the thing")
                },
            )
            .await
            .unwrap();

        match provider.wait(&handle).await.unwrap() {
            SubagentOutcome::Failed { message, .. } => {
                assert!(message.contains("used all its steps"), "{message}");
            }
            other => panic!("a child held to one step did not answer: {other:?}"),
        }
        let stored = store.read_stream(&child(), EventSeq::ZERO, usize::MAX).await.unwrap();
        let completed = stored
            .iter()
            .find(|event| event.event.event_type.as_str() == "turn/completed")
            .expect("the turn ended");
        assert_eq!(
            completed.event.payload.pointer("/budget/steps").and_then(|v| v.as_u64()),
            Some(1),
            "the child's terminal event says what it was held to: {}",
            completed.event.payload
        );
    }

    #[test]
    fn a_child_surface_offers_only_the_read_tools() {
        let surface = ReadOnlySurface::new(Arc::new(Surface));
        let names: Vec<String> =
            surface.definitions().into_iter().map(|definition| definition.name).collect();

        assert_eq!(names, vec!["fs.read".to_string()]);
        // The class is the inner's, so a write tool is present and unfiltered —
        // which is exactly why it is not offered.
        assert_eq!(surface.side_effect("fs.write"), Some(ToolSideEffect::Write));
        assert_eq!(surface.side_effect("nobody.knows"), None);
    }

    #[tokio::test]
    async fn a_call_for_a_tool_the_surface_does_not_offer_is_refused_by_name() {
        let surface = ReadOnlySurface::new(Arc::new(Surface));
        let error = surface
            .invoke(&ToolInvocation::proposed("fs.write", json!({})))
            .await
            .expect_err("a write tool is not on this surface");

        match error {
            ToolProviderError::UnknownTool { tool, available } => {
                assert_eq!(tool, "fs.write");
                assert_eq!(available, vec!["fs.read".to_string()]);
            }
            other => panic!("expected an unknown-tool refusal, got {other}"),
        }
    }

    /// The premise of the child's policy: whatever the parent stopped to ask
    /// about, the child refuses. Not a narrower *ask* — an ask a child cannot
    /// answer is a turn paused forever.
    #[test]
    fn a_child_policy_never_asks() {
        let parent = PolicyEngine::new()
            .with_sandbox([Cap::new(Cap::FS_READ), Cap::new("test.ask")])
            .with_rules([
                PolicyRule::allow(Cap::new(Cap::FS_READ)),
                PolicyRule::ask(Cap::new("test.ask")),
            ]);
        let surface = ReadOnlySurface::new(Arc::new(Surface));

        let child_policy =
            never_asking(&parent, &surface, &[Cap::new(Cap::FS_READ), Cap::new("test.ask")]);

        assert_eq!(child_policy.evaluate(&[Cap::new(Cap::FS_READ)], &[]), PolicyDecision::Allow);
        assert_eq!(
            child_policy.evaluate(&[Cap::new("test.ask")], &[]),
            PolicyDecision::Deny,
            "an ask the parent would have stopped on is a refusal here"
        );
        // Even a grant cannot reach an ask in this engine, because there is no
        // ask for a grant to satisfy.
        assert_eq!(
            child_policy.evaluate(&[Cap::new("test.ask")], &[Cap::new("test.ask")]),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn an_answer_that_arrived_before_the_brake_is_reported_as_itself() {
        let report = RunReport {
            run_id: RunId::new(),
            turn_id: agent_protocol::ids::TurnId::new(),
            stop_reason: StopReason::FinalAnswer,
            final_content: Some("the answer".to_string()),
        };

        let outcome = ended(Ok(Some(report)), &child(), Some(Asked::Cancelled), DEFAULT_CEILING);

        assert_eq!(
            outcome,
            SubagentOutcome::Completed { summary: "the answer".to_string(), result: None }
        );
    }

    #[test]
    fn a_stop_the_parent_asked_for_is_the_answer_when_it_is_what_ended_the_turn() {
        let report = |stop_reason| RunReport {
            run_id: RunId::new(),
            turn_id: agent_protocol::ids::TurnId::new(),
            stop_reason,
            final_content: None,
        };

        assert_eq!(
            ended(
                Ok(Some(report(StopReason::UserCancelled))),
                &child(),
                Some(Asked::Cancelled),
                DEFAULT_CEILING
            ),
            SubagentOutcome::Cancelled
        );
        match ended(
            Ok(Some(report(StopReason::UserCancelled))),
            &child(),
            Some(Asked::Deadline),
            Duration::from_millis(50),
        ) {
            SubagentOutcome::TimedOut { message } => {
                assert!(message.contains("50 ms"), "{message}")
            }
            other => panic!("a deadline stop is a timeout, not {other:?}"),
        }
    }

    /// A limit is not an answer, and pretending otherwise would repeat the
    /// 2026-10-30 reading (CONTRACT §7 #131, #132) one layer up.
    #[test]
    fn a_limit_stop_is_a_failure_that_keeps_what_the_child_said() {
        let report = RunReport {
            run_id: RunId::new(),
            turn_id: agent_protocol::ids::TurnId::new(),
            stop_reason: StopReason::MaxOutputTokens,
            final_content: Some("the beginning".to_string()),
        };

        match describe_report(&report, &child()) {
            SubagentOutcome::Failed { message, .. } => {
                assert!(message.contains("output-token limit"), "{message}");
                assert!(message.contains("the beginning"), "{message}");
            }
            other => panic!("a truncated child did not answer: {other:?}"),
        }
    }

    #[test]
    fn an_unconfirmed_stop_says_which_one_it_could_not_confirm() {
        match unconfirmed(Asked::Cancelled, &child(), true) {
            SubagentOutcome::CancelledUncertain { message } => {
                assert!(message.contains("child-1"), "{message}");
            }
            other => panic!("an unwatched cancel is uncertain, not {other:?}"),
        }
        match unconfirmed(Asked::Deadline, &child(), false) {
            SubagentOutcome::TimedOut { message } => {
                assert!(message.contains("could not be recorded"), "{message}");
            }
            other => panic!("an unwatched deadline is a timeout, not {other:?}"),
        }
    }

    /// A surface with one read tool and one write tool, over no inner provider:
    /// the provider half of these tests is about what is *offered*.
    struct Surface;

    #[async_trait::async_trait]
    impl ToolProvider for Surface {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![
                ToolDefinition {
                    name: "fs.read".to_string(),
                    description: String::new(),
                    source: ToolSource::Builtin,
                    side_effect: ToolSideEffect::Read,
                    capability_requirements: vec![Cap::new(Cap::FS_READ)],
                    idempotent: true,
                    timeout_ms: None,
                    input_schema: json!({}),
                },
                ToolDefinition {
                    name: "fs.write".to_string(),
                    description: String::new(),
                    source: ToolSource::Builtin,
                    side_effect: ToolSideEffect::Write,
                    capability_requirements: vec![Cap::new(Cap::FS_READ)],
                    idempotent: false,
                    timeout_ms: None,
                    input_schema: json!({}),
                },
            ]
        }

        fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
            match tool_name {
                "fs.read" => Some(ToolSideEffect::Read),
                "fs.write" => Some(ToolSideEffect::Write),
                _ => None,
            }
        }

        async fn invoke(
            &self,
            invocation: &ToolInvocation,
        ) -> Result<ToolResult, ToolProviderError> {
            Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: Vec::new(),
            })
        }
    }
}
