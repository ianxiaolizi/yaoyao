//! Subagent delegation — the kernel's side of the delegation protocol.
//!
//! [`agent_ports::subagent`] says how to start a child and how to hear how it
//! ended. This module says what the rest of the system needs in order for that
//! to be a *fact about a session*: where the child's history comes from, what
//! the parent's log records, and how a result the parent is not currently
//! waiting for gets back to it.
//!
//! ## A child's history is a fork
//!
//! A delegation does not invent a child session; it forks one (M5's
//! [`Fork`]). That is the whole reason the design says a child inherits the
//! parent's context: the inheritance is not a summary someone composed,
//! it is the parent's own events, copied up to a closed turn, with the child's
//! own header saying where they came from. The objective becomes the fork's
//! reason, so the child's log opens by saying what it was asked to do.
//!
//! Because a fork needs a closed turn to copy up to, a parent that has never
//! finished a turn cannot delegate — [`DelegationError::NoBoundary`]. That is
//! not a limitation to work around: a half-finished turn is exactly the tail
//! Fork exists to refuse, and a child started from one would inherit a history
//! whose own ending the parent has not decided yet.
//!
//! ## How a result gets back
//!
//! [`SubagentDelegation::settle`] waits for the child and records the outcome.
//! The caller that is *standing there* gets the outcome — which is the only way
//! today, and the reason the assembled binary's delegation tool blocks.
//!
//! The other half of delegation — a parent that has *ended* its turn, with the
//! child's answer waiting for it as input when a later run picks up — is **not
//! built**. There used to be a `deliver` here that appended the outcome as an
//! `external_result` Inbox item, and nothing ever selected that kind: the item
//! would sit pending forever, and `status` would report a task nobody was ever
//! going to do. It was removed rather than kept as a documented placeholder
//! (YA-40). When a run loop that ends its turn instead of blocking exists, both
//! ends — the writer and the selector — get written together, and the payload's
//! shape is that loop's to decide, not this file's to guess.
//!
//! ## What is recorded, and by whom
//!
//! `subagent/spawned` is appended by [`Actor::Agent`]: the parent's own act.
//! The settlement is appended by [`Actor::System`]: news arriving from outside
//! the parent's own run. Both go in the **parent's** log — the child's log says
//! what the child did, and only the parent's says that the parent asked.

use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::subagent::{
    SubagentError, SubagentFeatures, SubagentHandle, SubagentOutcome, SubagentProvider,
    SubagentRequest, SubagentStatus,
};
use agent_ports::tool_provider::ToolProvider;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId, TurnId};
use chrono::Utc;
use serde_json::json;

use crate::fork::{Fork, ForkError, ForkOutcome};

/// Errors from delegation.
///
/// The first two variants are the ones a caller can act on, and both mean
/// **nothing was written and nothing was started**: a refusal costs no events.
/// Everything else is a failure of something that had already begun, and the
/// errors that follow it name what is left behind.
#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    /// The backend cannot honour what the request asked for. No child session
    /// was created and no process was started.
    #[error("subagent backend '{provider}' cannot honour what the request asked for: {features}")]
    Unsupported { provider: String, features: SubagentFeatures },
    /// The parent has no closed turn to fork a child's history from. Nothing
    /// was written.
    #[error("no closed turn to fork a child from: {reason}")]
    NoBoundary { reason: String },
    /// A child was reported to have ended, in a state that is not an ending.
    ///
    /// Unreachable with today's [`SubagentOutcome`], whose every variant is
    /// terminal. It exists so that a vocabulary which later grows a
    /// non-terminal outcome produces a refusal rather than a log entry that
    /// says a child stopped when it did not.
    #[error("child '{provider}/{child_session_id}' is not in a terminal state")]
    NotTerminal { provider: String, child_session_id: SessionId },
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("fork error: {0}")]
    Fork(#[from] ForkError),
    /// A failure from the backend itself. [`SubagentError::Spawn`] names the
    /// child it could not start, which is how a caller finds the forked history
    /// that a failed spawn leaves behind.
    #[error("subagent backend error: {0}")]
    Backend(#[from] SubagentError),
}

/// Delegation for one parent session: fork a child's history, start it, and
/// record what happened.
pub struct SubagentDelegation {
    store: Arc<dyn EventStore>,
    fork: Fork,
    provider: Arc<dyn SubagentProvider>,
}

impl SubagentDelegation {
    /// Wire a delegation over a store, the tools a fork's recovery needs, and a
    /// backend.
    ///
    /// `tools` is what [`Fork`] hands to recovery, which asks each inherited
    /// open call's `side_effect` to decide how to close it. That is a question
    /// about the parent's history, not about delegation, so it is the caller's
    /// to answer — the delegation does not invent a tool surface for it.
    pub fn new(
        store: Arc<dyn EventStore>,
        tools: Arc<dyn ToolProvider>,
        provider: Arc<dyn SubagentProvider>,
    ) -> Self {
        Self { fork: Fork::new(store.clone(), tools), store, provider }
    }

    /// The backend this delegation starts children on.
    pub fn provider(&self) -> &Arc<dyn SubagentProvider> {
        &self.provider
    }

    /// Start a child for `request`, forked from `parent`'s closed turn `at` (`None`
    /// takes the parent's latest). Returns the handle that names the child.
    ///
    /// The order is a contract, not an implementation detail:
    ///
    /// 1. **Negotiate.** What the request asks for is read off the request and
    ///    compared with what the backend says it can honour. A backend that
    ///    cannot do it never sees a process started, and the parent's log never
    ///    mentions a delegation — that is what "failure to negotiate costs
    ///    nothing" means.
    /// 2. **Fork.** The child's history comes into being, and its session id —
    ///    which is the child's whole identity — is assigned by the fork. The
    ///    backend cannot be asked to start a child before the child's session
    ///    exists, because the session id is what it is told to run under.
    /// 3. **Start.** The backend spawns the child.
    /// 4. **Record.** `subagent/spawned` is appended to the parent.
    ///
    /// Steps 2 and 3 are not atomic, and cannot be: a fork is durable and there
    /// is no unfork. So a spawn that fails leaves the child's session on disk
    /// holding a copied prefix and nothing else — a residue the error names by
    /// session id, so it can be found. Step 4 goes the other way: a child whose
    /// spawn could not be recorded is a child nobody owns, so the child is
    /// stopped before the store error is returned rather than left running
    /// unwritten.
    pub async fn delegate(
        &self,
        parent: &SessionId,
        at: Option<&TurnId>,
        request: SubagentRequest,
    ) -> Result<SubagentHandle, DelegationError> {
        let missing = self.provider.supported().missing_from(&request.requested_features());
        if !missing.is_empty() {
            return Err(DelegationError::Unsupported {
                provider: self.provider.name().to_string(),
                features: missing.into_iter().collect(),
            });
        }

        let (child_session_id, fork_at_turn_id) =
            match self.fork.fork(parent, at, &request.objective).await? {
                ForkOutcome::Forked { session_id, fork_at_turn_id, .. } => {
                    (session_id, fork_at_turn_id)
                }
                ForkOutcome::NoBoundary { reason } => {
                    return Err(DelegationError::NoBoundary { reason });
                }
            };

        let handle =
            self.provider.spawn(&child_session_id, Some(&fork_at_turn_id), request.clone()).await?;

        let recorded = self
            .append(
                parent,
                EventType::SUBAGENT_SPAWNED,
                json!({
                    "child_session_id": handle.child_session_id,
                    "provider": handle.provider,
                    "parent_turn_id": handle.parent_turn_id,
                    "fork_at_turn_id": fork_at_turn_id,
                    "request": request,
                }),
                Actor::Agent,
            )
            .await;

        match recorded {
            Ok(()) => Ok(handle),
            Err(error) => {
                // The child is running and the parent's log does not say so.
                // Stopping it is the only way back to a state where everything
                // running is written down; whether the stop itself worked is
                // not something this can report, because the store failure is
                // the fact the caller needs.
                let _ = self.provider.dispose(&handle).await;
                Err(error)
            }
        }
    }

    /// Wait for the child to end, record how it ended, and return that.
    ///
    /// A child that ends badly is still a child that ended: the outcome is
    /// returned rather than raised, and the only errors are the ones that mean
    /// the outcome could not be established or recorded.
    pub async fn settle(
        &self,
        parent: &SessionId,
        handle: &SubagentHandle,
    ) -> Result<SubagentOutcome, DelegationError> {
        let outcome = self.provider.wait(handle).await?;
        self.record_settlement(parent, handle, &outcome).await?;
        Ok(outcome)
    }

    /// Ask the child to stop, record how it ended, and return that.
    ///
    /// Recorded through the same path as [`Self::settle`], because "the parent
    /// cancelled this child" is a settlement too, and the reason a log has
    /// `subagent/cancelled` as a word of its own is that a reader looking for
    /// trouble should not find a cancellation the parent asked for.
    pub async fn cancel(
        &self,
        parent: &SessionId,
        handle: &SubagentHandle,
    ) -> Result<SubagentOutcome, DelegationError> {
        let outcome = self.provider.cancel(handle).await?;
        self.record_settlement(parent, handle, &outcome).await?;
        Ok(outcome)
    }

    /// Release everything the backend holds for the child.
    ///
    /// Writes nothing: recycling a handle is not a fact about the child, and a
    /// log entry for it would give a reader a second thing called "the
    /// end" — after `subagent/completed` already said which one it was. A
    /// child still running when it is disposed is stopped (see the port).
    pub async fn dispose(&self, handle: &SubagentHandle) -> Result<(), SubagentError> {
        self.provider.dispose(handle).await
    }

    /// Append the settlement to the parent's log, under the event type that
    /// names the family the outcome belongs to.
    ///
    /// The event type is coarse and the payload is precise: a reader asking
    /// "did anything go wrong?" matches on the type, and a reader asking
    /// "exactly how did it end?" reads `outcome.status`.
    async fn record_settlement(
        &self,
        parent: &SessionId,
        handle: &SubagentHandle,
        outcome: &SubagentOutcome,
    ) -> Result<(), DelegationError> {
        let event_type = match outcome.status() {
            SubagentStatus::Completed | SubagentStatus::Partial => EventType::SUBAGENT_COMPLETED,
            SubagentStatus::Failed | SubagentStatus::TimedOut => EventType::SUBAGENT_FAILED,
            SubagentStatus::Cancelled | SubagentStatus::CancelledUncertain => {
                EventType::SUBAGENT_CANCELLED
            }
            SubagentStatus::Running => {
                return Err(DelegationError::NotTerminal {
                    provider: handle.provider.clone(),
                    child_session_id: handle.child_session_id.clone(),
                });
            }
        };

        self.append(
            parent,
            event_type,
            json!({
                "child_session_id": handle.child_session_id,
                "provider": handle.provider,
                "parent_turn_id": handle.parent_turn_id,
                "outcome": outcome,
            }),
            Actor::System,
        )
        .await
    }

    /// Append one event to `session`, at the sequence the log is actually at.
    async fn append(
        &self,
        session: &SessionId,
        event_type: &str,
        payload: serde_json::Value,
        actor: Actor,
    ) -> Result<(), DelegationError> {
        // Read and append as one step — see `Recovery::recover` for why. When a
        // resume holds the lease already this only bumps a depth counter, so a
        // delegated write under an approved call is not a second acquisition.
        // (YA-36)
        let _lease = self.store.lease(session).await?;
        let events = self.store.read_stream(session, EventSeq::ZERO, usize::MAX).await?;
        let next = events.last().map(|event| event.seq.next()).unwrap_or(EventSeq::ZERO);
        let draft = EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::current(),
            session_id: session.clone(),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: json!({}),
            ignorable: false,
        };
        self.store.append(session, next, vec![draft]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use agent_loop::DefaultAgentLoop;
    use agent_ports::tool_gateway::ToolGateway;
    use agent_ports::tool_provider::ToolSideEffect;
    use agent_protocol::events::StoredEvent;
    use agent_protocol::inbox::InboxKind;
    use agent_testkit::{
        FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine,
        one_model_router,
    };

    use crate::scheduler::Scheduler;

    /// A backend that answers from a script, and remembers what it was asked.
    ///
    /// It implements the real port with no process behind it: what the
    /// delegation does with a child is the subject here, and a real child
    /// process would only add a reason for the test to be flaky.
    struct FakeSubagent {
        name: String,
        supports: SubagentFeatures,
        /// What `wait` returns.
        outcome: Mutex<Option<SubagentOutcome>>,
        /// What `cancel` returns. `None` means `Cancelled`.
        cancel_outcome: Mutex<Option<SubagentOutcome>>,
        /// When set, `spawn` fails with this message.
        spawn_failure: Option<String>,
        spawned: Mutex<Vec<SubagentHandle>>,
        disposed: Mutex<Vec<SubagentHandle>>,
    }

    impl FakeSubagent {
        fn new(supports: SubagentFeatures, outcome: SubagentOutcome) -> Self {
            Self {
                name: "fake".to_string(),
                supports,
                outcome: Mutex::new(Some(outcome)),
                cancel_outcome: Mutex::new(None),
                spawn_failure: None,
                spawned: Mutex::new(Vec::new()),
                disposed: Mutex::new(Vec::new()),
            }
        }

        fn spawns(&self) -> usize {
            self.spawned.lock().expect("not poisoned").len()
        }

        fn disposals(&self) -> usize {
            self.disposed.lock().expect("not poisoned").len()
        }
    }

    #[async_trait::async_trait]
    impl SubagentProvider for FakeSubagent {
        fn name(&self) -> &str {
            &self.name
        }

        fn supported(&self) -> SubagentFeatures {
            self.supports.clone()
        }

        async fn spawn(
            &self,
            child_session_id: &SessionId,
            parent_turn_id: Option<&TurnId>,
            _request: SubagentRequest,
        ) -> Result<SubagentHandle, SubagentError> {
            if let Some(message) = &self.spawn_failure {
                return Err(SubagentError::Spawn {
                    child_session_id: child_session_id.clone(),
                    message: message.clone(),
                });
            }
            let handle = SubagentHandle::new(
                self.name.clone(),
                child_session_id.clone(),
                parent_turn_id.cloned(),
            );
            self.spawned.lock().expect("not poisoned").push(handle.clone());
            Ok(handle)
        }

        async fn wait(&self, _handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError> {
            Ok(self.outcome.lock().expect("not poisoned").clone().expect("the test set an outcome"))
        }

        async fn cancel(&self, _handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError> {
            Ok(self
                .cancel_outcome
                .lock()
                .expect("not poisoned")
                .clone()
                .unwrap_or(SubagentOutcome::Cancelled))
        }

        async fn dispose(&self, handle: &SubagentHandle) -> Result<(), SubagentError> {
            self.disposed.lock().expect("not poisoned").push(handle.clone());
            Ok(())
        }
    }

    /// A parent session with a real loop over a fake store, so the turn a fork
    /// copies is a turn the loop actually closed.
    struct Harness {
        store: Arc<FakeEventStore>,
        delegation: SubagentDelegation,
        backend: Arc<FakeSubagent>,
        parent: SessionId,
        scheduler: Scheduler,
    }

    fn every_feature() -> SubagentFeatures {
        [
            agent_ports::subagent::SubagentFeature::ToolAllowlist,
            agent_ports::subagent::SubagentFeature::ModelRoute,
            agent_ports::subagent::SubagentFeature::OutputSchema,
            agent_ports::subagent::SubagentFeature::Budget,
            agent_ports::subagent::SubagentFeature::Timeout,
        ]
        .into_iter()
        .collect()
    }

    fn harness(supports: SubagentFeatures, outcome: SubagentOutcome) -> Harness {
        harness_over(Arc::new(FakeEventStore::new()), supports, outcome)
    }

    fn harness_over(
        store: Arc<FakeEventStore>,
        supports: SubagentFeatures,
        outcome: SubagentOutcome,
    ) -> Harness {
        let tools: Arc<dyn ToolProvider> =
            Arc::new(FakeToolProvider::new().with_tool("echo", "ok", ToolSideEffect::Pure));
        let gateway: Arc<dyn ToolGateway> = Arc::new(FakeToolGateway::new(tools.clone()));
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let loop_ = Arc::new(DefaultAgentLoop::new(
            model,
            gateway,
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let scheduler = Scheduler::new(store.clone(), loop_, 8);
        let backend = Arc::new(FakeSubagent::new(supports, outcome));
        let delegation = SubagentDelegation::new(store.clone(), tools, backend.clone());
        let parent = SessionId::from_string("parent");
        Harness { store, delegation, backend, parent, scheduler }
    }

    impl Harness {
        /// Give the parent one finished turn, which is what a fork needs.
        async fn finish_a_turn(&self) {
            self.scheduler
                .inbox()
                .append(
                    &self.parent,
                    InboxKind::NextTurn,
                    json!({ "text": "do something" }),
                    0,
                    Actor::User,
                )
                .await
                .expect("append input");
            self.scheduler
                .run_pending(&self.parent, Utc::now())
                .await
                .expect("run pending")
                .expect("a turn ran");
        }

        async fn log(&self, session: &SessionId) -> Vec<StoredEvent> {
            self.store.read_stream(session, EventSeq::ZERO, usize::MAX).await.expect("readable log")
        }

        async fn types(&self, session: &SessionId) -> Vec<String> {
            self.log(session)
                .await
                .into_iter()
                .map(|event| event.event.event_type.to_string())
                .collect()
        }

        /// The `subagent/*` payloads the parent recorded, newest last.
        async fn subagent_payloads(&self) -> Vec<(String, serde_json::Value)> {
            self.log(&self.parent)
                .await
                .into_iter()
                .filter(|event| event.event.event_type.as_str().starts_with("subagent/"))
                .map(|event| (event.event.event_type.to_string(), event.event.payload.clone()))
                .collect()
        }
    }

    fn delegating(objective: &str) -> SubagentRequest {
        SubagentRequest::new(objective)
    }

    #[tokio::test]
    async fn a_parent_with_no_closed_turn_cannot_delegate() {
        // A fork needs a closed turn to copy up to. A parent that has never
        // finished one has nothing to hand a child, and the delegation says so
        // instead of starting a child with a history whose own ending is
        // undecided.
        let harness = harness(every_feature(), SubagentOutcome::Cancelled);
        let before = harness.log(&harness.parent).await.len();

        let refused = harness
            .delegation
            .delegate(&harness.parent, None, delegating("look at the thing"))
            .await
            .expect_err("a parent with no closed turn delegates nothing");

        assert!(matches!(refused, DelegationError::NoBoundary { .. }), "{refused}");
        assert_eq!(
            harness.log(&harness.parent).await.len(),
            before,
            "a refusal must write nothing"
        );
        assert_eq!(harness.backend.spawns(), 0, "the backend must not have been asked to start");
    }

    #[tokio::test]
    async fn a_request_the_backend_cannot_honour_is_refused_before_anything_is_written() {
        // The design's example, end to end through the kernel: a backend that
        // cannot be held to an output schema must refuse rather than run the
        // child and hand back an answer that is not schema-shaped.
        let harness =
            harness([agent_ports::subagent::SubagentFeature::Timeout].into_iter().collect(), {
                SubagentOutcome::Completed { summary: "done".into(), result: None }
            });
        harness.finish_a_turn().await;
        let before = harness.log(&harness.parent).await.len();

        let request = SubagentRequest {
            output_schema: Some(json!({ "type": "object" })),
            ..delegating("summarize")
        };
        let refused = harness
            .delegation
            .delegate(&harness.parent, None, request)
            .await
            .expect_err("an unsupported request is refused");

        match refused {
            DelegationError::Unsupported { ref features, .. } => assert_eq!(
                features.iter().collect::<Vec<_>>(),
                vec![agent_ports::subagent::SubagentFeature::OutputSchema]
            ),
            other => panic!("expected a refusal naming the feature: {other}"),
        }
        assert_eq!(
            harness.log(&harness.parent).await.len(),
            before,
            "a refusal must write nothing"
        );
        assert_eq!(harness.backend.spawns(), 0, "no child may be started for a refused request");
        assert!(
            harness.types(&harness.parent).await.iter().all(|t| !t.starts_with("subagent/")),
            "a refused delegation leaves no trace in the parent's log"
        );
    }

    #[tokio::test]
    async fn delegating_forks_the_child_from_the_parents_closed_turn() {
        // The inheritance the design claims is not a summary someone composed: it is
        // the parent's own events, and the child's header says so.
        let harness = harness(every_feature(), {
            SubagentOutcome::Completed { summary: "answered".into(), result: None }
        });
        harness.finish_a_turn().await;

        let handle = harness
            .delegation
            .delegate(&harness.parent, None, delegating("answer the question"))
            .await
            .expect("the delegation starts");

        assert_ne!(handle.child_session_id, harness.parent);
        assert_eq!(handle.provider, "fake");
        assert!(handle.parent_turn_id.is_some(), "a forked child belongs to the turn it inherited");

        let child = harness.log(&handle.child_session_id).await;
        let header = child.first().expect("the child's log opens with its header");
        assert_eq!(header.event.event_type.as_str(), EventType::SESSION_FORKED);
        assert_eq!(header.event.payload["parent_session_id"], json!(harness.parent.as_str()));
        assert_eq!(header.event.payload["fork_reason"], json!("answer the question"));
        assert_eq!(
            header.event.payload["fork_at_turn_id"],
            json!(handle.parent_turn_id.as_ref().expect("set above").as_str())
        );
        assert!(
            header.event.payload["inherited_event_count"].as_u64().unwrap_or(0) > 0,
            "the child inherited nothing: {}",
            header.event.payload
        );
    }

    /// Recording the delegation reads the parent's log and appends at it as one
    /// step. The store refuses an unleased append to the parent — but not to the
    /// child, which a fork writes at seq 0 having read nothing — so this passes
    /// only while the parent's read → append is covered. (YA-36)
    #[tokio::test]
    async fn recording_a_delegation_holds_the_parents_lease_across_its_read_and_append() {
        let harness = harness_over(
            Arc::new(FakeEventStore::new().requiring_lease_for(&SessionId::from_string("parent"))),
            every_feature(),
            SubagentOutcome::Completed { summary: "answered".into(), result: None },
        );
        harness.finish_a_turn().await;

        harness
            .delegation
            .delegate(&harness.parent, None, delegating("answer the question"))
            .await
            .expect("the delegation is recorded under the parent's lease");

        assert_eq!(harness.subagent_payloads().await.len(), 1);
    }

    #[tokio::test]
    async fn the_parents_log_records_the_delegation_it_asked_for() {
        let harness = harness(every_feature(), {
            SubagentOutcome::Completed { summary: "answered".into(), result: None }
        });
        harness.finish_a_turn().await;

        let handle = harness
            .delegation
            .delegate(&harness.parent, None, delegating("answer the question"))
            .await
            .expect("the delegation starts");

        let recorded = harness.subagent_payloads().await;
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        let (event_type, payload) = &recorded[0];
        assert_eq!(event_type, EventType::SUBAGENT_SPAWNED);
        assert_eq!(payload["child_session_id"], json!(handle.child_session_id.as_str()));
        assert_eq!(payload["provider"], json!("fake"));
        assert_eq!(payload["request"]["objective"], json!("answer the question"));

        // The act is the parent's own, not news from outside: that is what
        // makes it readable as "the parent decided to delegate" rather than
        // "something happened to the parent".
        let spawned = harness
            .log(&harness.parent)
            .await
            .into_iter()
            .find(|event| event.event.event_type.as_str() == EventType::SUBAGENT_SPAWNED)
            .expect("recorded above");
        assert_eq!(spawned.event.actor, Actor::Agent);
    }

    #[tokio::test]
    async fn settling_records_the_word_the_child_ended_with() {
        // The event type is the family a reader routes on; the payload's status
        // is the precise word. Both are checked, because a mapping that put
        // `timed_out` under `completed` would still satisfy a check on either
        // one alone.
        let cases = [
            (
                SubagentOutcome::Completed { summary: "done".into(), result: None },
                EventType::SUBAGENT_COMPLETED,
                SubagentStatus::Completed,
            ),
            (
                SubagentOutcome::Partial { summary: "half".into() },
                EventType::SUBAGENT_COMPLETED,
                SubagentStatus::Partial,
            ),
            (
                SubagentOutcome::Failed { message: "broke".into(), side_effects: Vec::new() },
                EventType::SUBAGENT_FAILED,
                SubagentStatus::Failed,
            ),
            (
                SubagentOutcome::TimedOut { message: "too slow".into() },
                EventType::SUBAGENT_FAILED,
                SubagentStatus::TimedOut,
            ),
            (SubagentOutcome::Cancelled, EventType::SUBAGENT_CANCELLED, SubagentStatus::Cancelled),
            (
                SubagentOutcome::CancelledUncertain { message: "unconfirmed".into() },
                EventType::SUBAGENT_CANCELLED,
                SubagentStatus::CancelledUncertain,
            ),
        ];

        for (outcome, event_type, status) in cases {
            let harness = harness(every_feature(), outcome.clone());
            harness.finish_a_turn().await;
            let handle = harness
                .delegation
                .delegate(&harness.parent, None, delegating("something"))
                .await
                .expect("the delegation starts");

            let settled =
                harness.delegation.settle(&harness.parent, &handle).await.expect("settled");
            assert_eq!(settled, outcome, "settle returns what the child reported");

            let recorded = harness.subagent_payloads().await;
            assert_eq!(recorded.len(), 2, "{recorded:?}");
            let (settlement_type, payload) = &recorded[1];
            assert_eq!(settlement_type, event_type, "for {}", outcome.describe());
            assert_eq!(
                payload["outcome"]["status"],
                json!(serde_json::to_value(status).expect("a status serializes")),
                "for {}",
                outcome.describe()
            );
            assert_eq!(payload["child_session_id"], json!(handle.child_session_id.as_str()));
        }
    }

    #[tokio::test]
    async fn a_cancellation_the_parent_asked_for_is_not_recorded_as_a_failure() {
        // The reason `subagent/cancelled` exists as a fourth name: a reader
        // asking "did anything go wrong?" must not be answered yes by a
        // decision that went exactly as intended.
        let harness = harness(every_feature(), SubagentOutcome::Cancelled);
        harness.finish_a_turn().await;
        let handle = harness
            .delegation
            .delegate(&harness.parent, None, delegating("something"))
            .await
            .expect("the delegation starts");

        let cancelled =
            harness.delegation.cancel(&harness.parent, &handle).await.expect("the child stops");
        assert_eq!(cancelled, SubagentOutcome::Cancelled);

        let types = harness.types(&harness.parent).await;
        assert!(types.contains(&EventType::SUBAGENT_CANCELLED.to_string()), "{types:?}");
        assert!(!types.contains(&EventType::SUBAGENT_FAILED.to_string()), "{types:?}");
    }

    #[tokio::test]
    async fn a_spawn_that_fails_names_the_child_whose_history_was_left_behind() {
        // The fork is durable and there is no unfork, so this order leaves a
        // residue. What matters is that it is findable: the error carries the
        // child's session id, and the child's log is really there.
        let mut harness = harness(every_feature(), SubagentOutcome::Cancelled);
        harness.finish_a_turn().await;
        let backend = Arc::new(FakeSubagent {
            spawn_failure: Some("the interpreter was not found".to_string()),
            ..FakeSubagent::new(every_feature(), SubagentOutcome::Cancelled)
        });
        harness.delegation = SubagentDelegation::new(
            harness.store.clone(),
            Arc::new(FakeToolProvider::new().with_tool("echo", "ok", ToolSideEffect::Pure))
                as Arc<dyn ToolProvider>,
            backend,
        );

        let error = harness
            .delegation
            .delegate(&harness.parent, None, delegating("something"))
            .await
            .expect_err("a spawn that could not start is an error");

        let DelegationError::Backend(SubagentError::Spawn { child_session_id, .. }) = &error else {
            panic!("expected the backend's spawn failure: {error}");
        };
        assert!(
            error.to_string().contains(child_session_id.as_str()),
            "the child must be named so the residue can be found: {error}"
        );

        let residue = harness.log(child_session_id).await;
        assert_eq!(
            residue.first().map(|event| event.event.event_type.as_str()),
            Some(EventType::SESSION_FORKED),
            "the forked history is what a failed spawn leaves behind"
        );
        assert!(
            harness.types(&harness.parent).await.iter().all(|t| !t.starts_with("subagent/")),
            "the parent must not claim a delegation that was never recorded"
        );
    }

    #[tokio::test]
    async fn disposing_a_child_writes_nothing() {
        // Recycling a handle is not a fact about the child. A log entry for it
        // would give a reader a second thing called "the end", after the
        // settlement already said which one it was.
        let harness = harness(every_feature(), {
            SubagentOutcome::Completed { summary: "done".into(), result: None }
        });
        harness.finish_a_turn().await;
        let handle = harness
            .delegation
            .delegate(&harness.parent, None, delegating("something"))
            .await
            .expect("the delegation starts");
        harness.delegation.settle(&harness.parent, &handle).await.expect("settled");
        let before = harness.log(&harness.parent).await.len();

        harness.delegation.dispose(&handle).await.expect("disposing is fine");

        assert_eq!(harness.log(&harness.parent).await.len(), before);
        assert_eq!(harness.backend.disposals(), 1);
    }
}
