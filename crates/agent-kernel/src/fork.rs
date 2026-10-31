//! Fork — derive a new session from a closed turn boundary of another (M5).
//!
//! A fork copies an *inclusive event prefix* of the source session into a new
//! session, whose first event records where it came from. The source is read
//! and never written: a fork is a new log that starts out saying the same
//! things, not an edit to the one it came from.
//!
//! ## What makes a boundary
//!
//! A boundary is the terminal event of a turn that **ended** — `turn/completed`,
//! `turn/failed`, `turn/cancelled`, `turn/interrupted`. Nothing is cut in the
//! middle of a turn, because the child would inherit half a step whose other
//! half is in the model's head rather than in either log.
//!
//! A turn waiting on a decision (`turn/yielded`) is deliberately **not** a
//! boundary, and that is the one place this deviates from a plain prefix copy.
//! Two reasons, and they are the same reason twice:
//!
//! * The child would inherit a call the model proposed and nothing answered.
//!   Its `resume` would read that as a wait it is entitled to settle — and
//!   settling a granted call *runs it*. The parent is usually about to run the
//!   same call, so a fork would turn one side effect into two.
//! * Neither reference implementation has this state to copy. DSH forks at an
//!   open (in-flight) step and repairs it; Codex snapshots an `Interrupted`
//!   point. A paused-and-persisted turn is yaoyao's own shape, so there is no
//!   precedent to borrow and no reason to guess.
//!
//! ## What the child's tail is allowed to be
//!
//! An *interrupted* tail — an open run/turn/step, or a `tool/started` that never
//! came back — is repaired rather than refused, which is what DSH does (it
//! closes the open step and turn with reason `forked`). The repair is not
//! written here: [`Recovery`] is run over the child, so the closers are the same
//! `turn/interrupted` and `tool/outcome_unknown` facts a crashed session gets.
//! Reusing those codes is the point — "the log cannot say whether it ran" is
//! true of the child for the same reason it is true of the parent, while a
//! fork-flavoured sentence would be this module's account of someone else's log.
//!
//! The line between the two: **what the parent can still settle, the child must
//! not settle for it.**

use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::tool_provider::ToolProvider;
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId, TurnId};
use agent_protocol::tool::APPROVAL_PENDING_OUTCOME;
use chrono::Utc;

use crate::recovery::{Recovery, RecoveryError, RecoveryReport};

/// Derives new sessions from a source session's closed turn boundaries.
pub struct Fork {
    store: Arc<dyn EventStore>,
    /// Repairs the child's inherited tail with the same rules a crashed session
    /// gets. Built here rather than passed in because the repair is not
    /// optional: a child whose log ends on an open step is not a session.
    recovery: Recovery,
}

/// What one fork did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkOutcome {
    /// The child was created. Its `session_id` is the caller's handle on it.
    Forked {
        session_id: SessionId,
        parent: SessionId,
        fork_at_event_id: EventId,
        fork_at_turn_id: TurnId,
        /// Events copied from the parent, not counting the `session/forked`
        /// header. This is the count that distinguishes what the child
        /// inherited from what it did itself.
        inherited_events: usize,
        /// The closers written into the child's copy of an interrupted tail.
        /// All-false when the tail was already clean.
        repaired: RecoveryReport,
    },
    /// Nothing was written, and nothing in the source changed. `reason` says
    /// what stood in the way.
    NoBoundary { reason: String },
}

/// Errors from Fork operations.
#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("recovery error: {0}")]
    Recovery(#[from] RecoveryError),
}

impl Fork {
    pub fn new(store: Arc<dyn EventStore>, tools: Arc<dyn ToolProvider>) -> Self {
        Self { recovery: Recovery::new(store.clone(), tools), store }
    }

    /// Copy `source`'s events up to a closed turn boundary into a new session.
    ///
    /// `at` picks the boundary turn; `None` takes the source's latest one.
    /// `reason` is recorded for whoever reads the lineage later, not interpreted
    /// here — this module has no vocabulary for why a fork happened.
    pub async fn fork(
        &self,
        source: &SessionId,
        at: Option<&TurnId>,
        reason: &str,
    ) -> Result<ForkOutcome, ForkError> {
        let events = self.store.read_stream(source, EventSeq::ZERO, usize::MAX).await?;

        let boundary = match boundary(&events, at) {
            Ok(boundary) => boundary,
            Err(reason) => return Ok(ForkOutcome::NoBoundary { reason }),
        };
        let prefix = &events[..=boundary.index];
        if let Some(reason) = inherited_wait(prefix) {
            return Ok(ForkOutcome::NoBoundary { reason });
        }

        let child = SessionId::new();
        let mut drafts = Vec::with_capacity(prefix.len() + 1);
        drafts.push(header(source, &child, &boundary, prefix, reason));
        drafts.extend(prefix.iter().map(|stored| inherit(stored, &child)));
        self.store.append(&child, EventSeq::ZERO, drafts).await?;

        // The child now holds the parent's events and nothing else; whatever is
        // still open at its tail is the parent's interrupted tail, and Recovery
        // closes it the way it closes a crashed session's.
        let repaired = self.recovery.recover(&child).await?;

        Ok(ForkOutcome::Forked {
            session_id: child,
            parent: source.clone(),
            fork_at_event_id: boundary.event_id,
            fork_at_turn_id: boundary.turn_id,
            inherited_events: prefix.len(),
            repaired,
        })
    }
}

/// Where the fork is cut, in the source's own terms.
struct Boundary {
    /// Index into the source's events of the boundary's terminal event.
    index: usize,
    turn_id: TurnId,
    event_id: EventId,
    seq: EventSeq,
}

/// The event types that end a turn for good.
///
/// `turn/yielded` is not among them: it ends the turn's *running*, not its
/// business. See this module's header for why that distinction decides whether
/// a fork is allowed at all.
fn ends_the_turn(event_type: &str) -> bool {
    matches!(
        event_type,
        EventType::TURN_COMPLETED
            | EventType::TURN_FAILED
            | EventType::TURN_CANCELLED
            | EventType::TURN_INTERRUPTED
    )
}

/// Pick the boundary, or say why no boundary can be picked.
///
/// `Err` is a reason for the caller to show a person — every one of them is a
/// state with an action attached, not a failure of this module.
fn boundary(events: &[StoredEvent], at: Option<&TurnId>) -> Result<Boundary, String> {
    let Some(at) = at else {
        let index = events
            .iter()
            .rposition(|stored| ends_the_turn(stored.event.event_type.as_str()))
            .ok_or_else(|| "this session has no finished turn to fork at".to_string())?;
        return at_index(events, index);
    };

    if let Some(index) = events.iter().rposition(|stored| {
        stored.event.turn_id.as_ref() == Some(at) && ends_the_turn(stored.event.event_type.as_str())
    }) {
        return at_index(events, index);
    }

    if !events.iter().any(|stored| stored.event.turn_id.as_ref() == Some(at)) {
        return Err(format!("turn {at} is not in this session"));
    }
    if waiting(events, at) {
        return Err(format!(
            "turn {at} is waiting on a decision, so it has not ended; a fork cannot inherit a \
             wait the child would be entitled to settle — answer it (approve or deny, then \
             resume) or fork at an earlier turn"
        ));
    }
    Err(format!(
        "turn {at} has not finished; a fork boundary is an end, not a place in the middle of a turn"
    ))
}

/// Whether `turn` yielded and nothing has carried it forward since.
fn waiting(events: &[StoredEvent], turn: &TurnId) -> bool {
    let mut waiting = false;
    for stored in events.iter().filter(|s| s.event.turn_id.as_ref() == Some(turn)) {
        match stored.event.event_type.as_str() {
            EventType::TURN_YIELDED => waiting = true,
            EventType::TURN_RESUMED => waiting = false,
            _ => {}
        }
    }
    waiting
}

/// Cut the prefix at `terminal`, reaching through the run terminal that belongs
/// to the same batch.
///
/// A turn's end is not one line. The scheduler writes the turn's terminal and
/// then its run's terminal as one append, and the run's line carries the run id
/// but not the turn's. Stopping between them would hand the child a batch with
/// its second half missing — a run that nothing closed — which [`Recovery`]
/// would then "repair" into a `run/interrupted` for a run that in fact ended
/// normally. That is a false fact of exactly the kind the turn-level rule above
/// exists to prevent, so the boundary reaches through the run terminal and
/// stops at the first event that is not one.
fn at_index(events: &[StoredEvent], terminal: usize) -> Result<Boundary, String> {
    let run_id = events[terminal].event.run_id.clone();
    let mut index = terminal;
    while let Some(next) = events.get(index + 1) {
        let ends_this_run = run_id.is_some()
            && next.event.run_id == run_id
            && matches!(
                next.event.event_type.as_str(),
                EventType::RUN_COMPLETED
                    | EventType::RUN_FAILED
                    | EventType::RUN_CANCELLED
                    | EventType::RUN_INTERRUPTED
                    | EventType::RUN_YIELDED
            );
        if !ends_this_run {
            break;
        }
        index += 1;
    }

    let stored = &events[index];
    // A terminal event for a turn that no event ever opened would be a log
    // contradicting itself. Nothing in this build writes one, but a log is
    // input, and `turn_id` is what the boundary is *named* by: minting a fresh
    // id here would write a name the source never had into the child's lineage,
    // making the fork point untraceable back to the parent. Refuse instead —
    // the caller gets a reason, and the log stays the only authority on what
    // the boundary was called.
    let Some(turn_id) = events[terminal].event.turn_id.clone() else {
        return Err(
            "the boundary event has no turn id; a fork cannot name a turn the source never named"
                .to_string(),
        );
    };
    Ok(Boundary { index, turn_id, event_id: stored.event.event_id.clone(), seq: stored.seq })
}

/// A wait sitting inside the prefix the child would inherit.
///
/// The turn-level check in [`boundary`] catches the ordinary form of this — a
/// yielded turn, which is what a held call looks like from above. This is the
/// same state stated as the fact underneath it, and it is a guard rather than a
/// path this build produces: no writer here records an answerless call without
/// also yielding the turn it is in. It is checked anyway, because the cost of
/// being wrong is a duplicated side effect in a *different* session, where the
/// child's `resume` would run the call and the parent would never hear of it.
fn inherited_wait(prefix: &[StoredEvent]) -> Option<String> {
    for (index, stored) in prefix.iter().enumerate() {
        if stored.event.payload.get("outcome").and_then(|v| v.as_str())
            != Some(APPROVAL_PENDING_OUTCOME)
        {
            continue;
        }
        // An outcome is only the last word on its call if nothing later in the
        // prefix replies to the same call. A resume writes a second dispatch and
        // a real outcome over the wait, and after that the history is ordinary.
        let id = stored.event.payload.get("provider_tool_call_id").and_then(|v| v.as_str());
        let answered_later = prefix.iter().skip(index + 1).any(|later| {
            later.event.payload.get("provider_tool_call_id").and_then(|v| v.as_str()) == id
                && later.event.payload.get("outcome").and_then(|v| v.as_str())
                    != Some(APPROVAL_PENDING_OUTCOME)
        });
        if !answered_later {
            let tool = stored.event.payload.get("tool_name").and_then(|v| v.as_str());
            return Some(format!(
                "the inherited history holds a call{} that is still waiting on an approval, so \
                 the child could run it; answer the wait in this session or fork at an earlier \
                 turn",
                tool.map(|t| format!(" to '{t}'")).unwrap_or_default()
            ));
        }
    }
    None
}

/// The child's first event: what it is, and where it came from.
///
/// `inherited_event_count` is what lets a reader tell the child's own events
/// from the ones it inherited, without knowing anything about the parent — the
/// child's log is the only thing a later reader has.
fn header(
    source: &SessionId,
    child: &SessionId,
    boundary: &Boundary,
    prefix: &[StoredEvent],
    reason: &str,
) -> EventDraft {
    draft(
        child,
        EventType::SESSION_FORKED,
        serde_json::json!({
            "parent_session_id": source.as_str(),
            "fork_at_event_id": boundary.event_id.as_str(),
            "fork_at_turn_id": boundary.turn_id.as_str(),
            "fork_at_seq": boundary.seq.as_u64(),
            "fork_reason": reason,
            "inherited_event_count": prefix.len(),
            "profile_snapshot": profile_snapshot(prefix),
        }),
        Actor::System,
    )
}

/// The configuration the inherited history was running under.
///
/// Copied from the run's own `profile/resolved` rather than re-derived from
/// config, which may have moved on since — and which is not even available to
/// a reader looking at the child's log alone.
///
/// A prefix produced before the snapshot existed has none, so the fallback
/// records what such a history *can* still say about itself: the model route
/// it last ran on, read off `model/attempt.started`. The result is a partial
/// snapshot whose missing fields mean "not recorded", which is the truth;
/// inventing the rest from today's config would be a made-up fact.
fn profile_snapshot(prefix: &[StoredEvent]) -> serde_json::Value {
    let resolved = prefix
        .iter()
        .rposition(|stored| stored.event.event_type.as_str() == EventType::PROFILE_RESOLVED);
    if let Some(index) = resolved {
        return prefix[index].event.payload.clone();
    }

    let route = prefix
        .iter()
        .rposition(|stored| stored.event.event_type.as_str() == EventType::MODEL_ATTEMPT_STARTED)
        .and_then(|index| prefix[index].event.payload.get("route").cloned());
    match route {
        Some(route) => serde_json::json!({ "model_route": route }),
        None => serde_json::Value::Null,
    }
}

/// One inherited event, as the child's own.
///
/// The `event_id` survives, because it is the identity of a fact as it happened
/// and the lineage is only traceable if it does; `session_id` does not, because
/// after inheritance the line genuinely belongs to the child's log, and a line
/// claiming to belong to another session is false. `created_at` and the
/// run/turn/step ids are part of the fact and stay as they were.
fn inherit(stored: &StoredEvent, child: &SessionId) -> EventDraft {
    let mut draft = stored.event.clone();
    draft.session_id = child.clone();
    draft
}

fn draft(
    session_id: &SessionId,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session_id.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use agent_loop::DefaultAgentLoop;
    use agent_ports::tool_gateway::ToolGateway;
    use agent_ports::tool_provider::ToolSideEffect;
    use agent_protocol::ids::{RunId, StepId};
    use agent_protocol::model::ModelResponse;
    use agent_protocol::profile::ResolvedProfileSnapshot;
    use agent_testkit::{
        FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine,
        one_model_router,
    };

    /// A session with a real loop over a fake store, so the log a fork reads is
    /// the log commands actually write.
    struct Harness {
        store: Arc<FakeEventStore>,
        scheduler: Scheduler,
        fork: Fork,
        session_id: SessionId,
    }

    fn harness(model: Vec<ModelResponse>) -> Harness {
        build(Arc::new(FakeModelProvider::new(model)), None, None)
    }

    /// A harness whose every tool call is held for approval, so the turn yields.
    fn harness_yielding() -> Harness {
        build(
            Arc::new(FakeModelProvider::new(vec![FakeModelProvider::tool_call(
                "deploy",
                serde_json::json!({}),
            )])),
            Some("appr-1"),
            None,
        )
    }

    /// A harness whose runs record a resolved configuration, as the assembled
    /// binary's do.
    fn harness_with_profile(snapshot: ResolvedProfileSnapshot) -> Harness {
        build(
            Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")])),
            None,
            Some(snapshot),
        )
    }

    /// `held` makes every call come back awaiting an approval, which is how a
    /// turn is made to yield without a policy engine or an approval service.
    fn build(
        model: Arc<FakeModelProvider>,
        held: Option<&str>,
        profile: Option<ResolvedProfileSnapshot>,
    ) -> Harness {
        let store = Arc::new(FakeEventStore::new());
        let tools: Arc<dyn ToolProvider> = Arc::new(FakeToolProvider::new().with_tool(
            "deploy",
            "done",
            ToolSideEffect::Destructive,
        ));
        let gateway: Arc<dyn ToolGateway> = match held {
            Some(approval_id) => {
                Arc::new(FakeToolGateway::requiring_approval(tools.clone(), approval_id))
            }
            None => Arc::new(FakeToolGateway::new(tools.clone())),
        };
        let loop_ = Arc::new(DefaultAgentLoop::new(
            model,
            gateway,
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let scheduler = match profile {
            Some(snapshot) => {
                Scheduler::new(store.clone(), loop_, 8).with_profile_snapshot(snapshot)
            }
            None => Scheduler::new(store.clone(), loop_, 8),
        };
        Harness {
            fork: Fork::new(store.clone(), tools),
            scheduler,
            store,
            session_id: SessionId::from_string("fork-source"),
        }
    }

    impl Harness {
        /// Answer one prompt, producing a finished turn.
        async fn ask(&self, text: &str) {
            self.scheduler
                .inbox()
                .append(
                    &self.session_id,
                    agent_protocol::inbox::InboxKind::NextTurn,
                    serde_json::json!({ "text": text }),
                    0,
                    Actor::User,
                )
                .await
                .unwrap();
            self.scheduler.run_pending(&self.session_id, Utc::now()).await.unwrap().expect("a run");
        }

        async fn log(&self, session_id: &SessionId) -> Vec<StoredEvent> {
            self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await.unwrap()
        }

        async fn types(&self, session_id: &SessionId) -> Vec<String> {
            self.log(session_id).await.into_iter().map(|e| e.event.event_type.to_string()).collect()
        }

        /// The turn of the source's latest finished turn.
        async fn boundary_of(&self) -> TurnId {
            self.log(&self.session_id)
                .await
                .iter()
                .rev()
                .find(|e| ends_the_turn(e.event.event_type.as_str()))
                .and_then(|e| e.event.turn_id.clone())
                .expect("the premise: the source has a finished turn")
        }
    }

    fn forked(outcome: ForkOutcome) -> (SessionId, TurnId, usize) {
        match outcome {
            ForkOutcome::Forked { session_id, fork_at_turn_id, inherited_events, .. } => {
                (session_id, fork_at_turn_id, inherited_events)
            }
            other => panic!("expected a fork, got {other:?}"),
        }
    }

    fn no_boundary(outcome: ForkOutcome) -> String {
        match outcome {
            ForkOutcome::NoBoundary { reason } => reason,
            other => panic!("expected no boundary, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fork_copies_the_prefix_and_answers_for_its_lineage() {
        let h = harness(vec![FakeModelProvider::answer("done")]);
        h.ask("do the thing").await;
        let parent_log = h.log(&h.session_id).await;
        let boundary = h.boundary_of().await;

        let (child, at_turn, inherited) =
            forked(h.fork.fork(&h.session_id, Some(&boundary), "try another way").await.unwrap());
        assert_eq!(at_turn, boundary);
        assert_eq!(inherited, parent_log.len(), "the whole prefix was inherited");

        let child_log = h.log(&child).await;
        assert_eq!(child_log.len(), parent_log.len() + 1, "header plus the prefix, nothing else");

        // The header says where the child came from, and where the cut was.
        let head = &child_log[0];
        assert_eq!(head.event.event_type.as_str(), EventType::SESSION_FORKED);
        assert_eq!(head.event.session_id, child);
        assert_eq!(head.event.actor, Actor::System);
        assert_eq!(head.event.payload["parent_session_id"], h.session_id.as_str());
        assert_eq!(head.event.payload["fork_at_turn_id"], boundary.as_str());
        assert_eq!(head.event.payload["fork_reason"], "try another way");
        assert_eq!(head.event.payload["inherited_event_count"], serde_json::json!(inherited));
        let terminal = parent_log.last().expect("a finished turn");
        assert_eq!(head.event.payload["fork_at_event_id"], terminal.event.event_id.as_str());
        assert_eq!(head.event.payload["fork_at_seq"], serde_json::json!(terminal.seq.as_u64()));

        // The inherited events are the parent's, in order, under the child's name.
        for (offset, parent_event) in parent_log.iter().enumerate() {
            let copied = &child_log[offset + 1];
            assert_eq!(copied.seq, EventSeq::new(offset as u64 + 1), "seq is the child's own");
            assert_eq!(
                copied.event.event_id, parent_event.event.event_id,
                "an inherited fact keeps its identity"
            );
            assert_eq!(copied.event.payload, parent_event.event.payload);
            assert_eq!(copied.event.created_at, parent_event.event.created_at);
            assert_eq!(copied.event.session_id, child, "but the line belongs to the child's log");
        }

        // The source did not move.
        assert_eq!(h.types(&h.session_id).await, types_of(&parent_log));
    }

    #[tokio::test]
    async fn the_header_carries_the_resolved_configuration_its_history_ran_under() {
        let snapshot = ResolvedProfileSnapshot {
            profile_id: "coding".to_string(),
            config_hash: String::new(),
            plugin_versions: std::collections::BTreeMap::from([(
                "mcp.foo".to_string(),
                "1.2.0".to_string(),
            )]),
            policy_version: "deadbeef".to_string(),
            decision_backends: Vec::new(),
            model_route: agent_protocol::model::ModelRoute::new(
                agent_protocol::model::ModelTier::Main,
                "echo",
                "echo",
            ),
            model_routes: std::collections::BTreeMap::new(),
            memory_enabled: false,
            capability_set: [agent_protocol::permissions::Capability::new(
                agent_protocol::permissions::Capability::FS_READ,
            )]
            .into_iter()
            .collect(),
            budget: agent_protocol::profile::RunBudget::default(),
            model_capabilities: std::collections::BTreeMap::new(),
            budget_sources: std::collections::BTreeMap::new(),
            completion: None,
        }
        .sealed();

        let h = harness_with_profile(snapshot);
        h.ask("do it").await;
        let recorded = h
            .log(&h.session_id)
            .await
            .into_iter()
            .find(|e| e.event.event_type.as_str() == EventType::PROFILE_RESOLVED)
            .map(|e| e.event.payload)
            .expect("the run recorded the configuration it started under");

        let (child, ..) = forked(h.fork.fork(&h.session_id, None, "carry on").await.unwrap());
        let head = &h.log(&child).await[0].event;

        // The child gets the configuration itself, not a re-resolution of it:
        // the config files a reader would consult may have moved on, and the
        // child's log is all a reader of the child has.
        assert_eq!(head.payload["profile_snapshot"], recorded);
        assert_eq!(head.payload["profile_snapshot"]["profile_id"], "coding");
        assert_eq!(head.payload["profile_snapshot"]["plugin_versions"]["mcp.foo"], "1.2.0");
    }

    #[tokio::test]
    async fn a_history_with_no_recorded_configuration_says_what_it_can() {
        // The harness in `harness` resolves no configuration, which is also the
        // shape of every log written before snapshots existed: the header then
        // records the route the history last ran on and leaves the rest absent,
        // rather than filling it in from today's config.
        let h = harness(vec![FakeModelProvider::answer("done")]);
        h.ask("do it").await;
        let route = h
            .log(&h.session_id)
            .await
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::MODEL_ATTEMPT_STARTED)
            .map(|e| e.event.payload["route"].clone())
            .expect("the premise: a model was called");

        let (child, ..) = forked(h.fork.fork(&h.session_id, None, "switch models").await.unwrap());
        let head = &h.log(&child).await[0].event;
        assert_eq!(head.payload["profile_snapshot"]["model_route"], route);
        assert!(
            head.payload["profile_snapshot"]["config_hash"].is_null(),
            "what was not recorded stays unrecorded: {}",
            head.payload["profile_snapshot"]
        );
    }

    #[tokio::test]
    async fn forking_the_same_boundary_twice_yields_two_independent_children() {
        let h = harness(vec![FakeModelProvider::answer("done")]);
        h.ask("do it").await;
        let boundary = h.boundary_of().await;

        let (a, ..) = forked(h.fork.fork(&h.session_id, Some(&boundary), "one").await.unwrap());
        let (b, ..) = forked(h.fork.fork(&h.session_id, Some(&boundary), "two").await.unwrap());

        assert_ne!(a, b, "each fork is its own session");
        let (a_log, b_log) = (h.log(&a).await, h.log(&b).await);
        assert_eq!(a_log.len(), b_log.len(), "same content");
        assert_ne!(a_log[0].event.event_id, b_log[0].event.event_id, "separate facts");
        assert_eq!(a_log[0].event.payload["fork_reason"], "one");
        assert_eq!(b_log[0].event.payload["fork_reason"], "two");
    }

    #[tokio::test]
    async fn the_child_ends_at_the_named_boundary_not_at_the_latest_one() {
        let h =
            harness(vec![FakeModelProvider::answer("first"), FakeModelProvider::answer("second")]);
        h.ask("one").await;
        let first_boundary = h.boundary_of().await;
        h.ask("two").await;
        assert_ne!(first_boundary, h.boundary_of().await, "the premise: two finished turns");

        let (child, at_turn, inherited) =
            forked(h.fork.fork(&h.session_id, Some(&first_boundary), "early").await.unwrap());
        assert_eq!(at_turn, first_boundary);

        // The child stops at the first turn's terminal event — the second turn
        // is not in it at all.
        let child_types = h.types(&child).await;
        assert_eq!(child_types.iter().filter(|t| *t == EventType::TURN_STARTED).count(), 1);
        assert_eq!(child_types.iter().filter(|t| *t == EventType::TURN_COMPLETED).count(), 1);
        assert!(inherited < h.log(&h.session_id).await.len(), "the later turn was left behind");
    }

    #[tokio::test]
    async fn a_session_with_no_finished_turn_has_no_boundary() {
        let h = harness(vec![FakeModelProvider::answer("unused")]);
        // Nothing has run: the session is empty.
        let reason = no_boundary(h.fork.fork(&h.session_id, None, "nowhere").await.unwrap());
        assert!(reason.contains("no finished turn"), "{reason}");
        assert!(h.log(&h.session_id).await.is_empty(), "nothing was written");

        // A turn that is open is not a boundary either.
        let turn = TurnId::new();
        seed(
            &h.store,
            &h.session_id,
            vec![
                draft(
                    &h.session_id,
                    EventType::TURN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                )
                .with_turn(&turn),
            ],
        )
        .await;
        let reason =
            no_boundary(h.fork.fork(&h.session_id, Some(&turn), "mid-turn").await.unwrap());
        assert!(reason.contains("has not finished"), "{reason}");
        assert_eq!(h.types(&h.session_id).await.len(), 1, "the source is untouched");
    }

    #[tokio::test]
    async fn a_boundary_without_a_turn_id_is_refused_not_given_a_new_name() {
        let h = harness(vec![FakeModelProvider::answer("unused")]);
        // A terminal event the source never named. This build does not write
        // one, but a log is input: minting a fresh id here would put a turn
        // name in the child's lineage that appears nowhere in the parent.
        seed(
            &h.store,
            &h.session_id,
            vec![draft(
                &h.session_id,
                EventType::TURN_COMPLETED,
                serde_json::json!({}),
                Actor::Scheduler,
            )],
        )
        .await;

        let reason = no_boundary(h.fork.fork(&h.session_id, None, "anyway").await.unwrap());
        assert!(reason.contains("no turn id"), "{reason}");
        assert_eq!(h.types(&h.session_id).await.len(), 1, "nothing was inherited or written");
    }

    #[tokio::test]
    async fn a_turn_waiting_on_a_decision_is_not_a_boundary() {
        let h = harness_yielding();
        h.ask("deploy it").await;
        let boundary = h
            .log(&h.session_id)
            .await
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::TURN_YIELDED)
            .and_then(|e| e.event.turn_id.clone())
            .expect("the premise: the turn yielded");
        let before = h.types(&h.session_id).await;

        // Named explicitly, and by default: a waiting turn is not an end, so it
        // cannot be the latest finished turn either.
        let named = no_boundary(
            h.fork.fork(&h.session_id, Some(&boundary), "during the wait").await.unwrap(),
        );
        assert!(named.contains("waiting on a decision"), "{named}");
        let by_default =
            no_boundary(h.fork.fork(&h.session_id, None, "during the wait").await.unwrap());
        assert!(by_default.contains("no finished turn"), "{by_default}");

        // The point of refusing: the held call is not inherited into a session
        // that would run it. And the source is untouched.
        assert_eq!(h.types(&h.session_id).await, before);
    }

    #[tokio::test]
    async fn an_interrupted_tail_is_closed_in_the_child_and_not_in_the_source() {
        let h = harness(vec![FakeModelProvider::answer("unused")]);
        // A turn that ended while a call was still out: the shape a crash
        // between a dispatch and its outcome leaves behind. The turn is
        // finished, so it is a boundary; the call and the step are not.
        let turn = TurnId::new();
        seed(&h.store, &h.session_id, interrupted_tail(&h.session_id, &turn)).await;
        let parent_types = h.types(&h.session_id).await;

        let ForkOutcome::Forked { session_id: child, repaired, inherited_events, .. } =
            h.fork.fork(&h.session_id, Some(&turn), "after the crash").await.unwrap()
        else {
            panic!("expected a fork");
        };

        // The child's tail is closed with the codes a crashed session gets.
        assert_eq!(repaired.tools_outcome_unknown, 1, "the dispatched call is answered");
        assert!(repaired.closed_step, "the step it was dispatched in is closed");
        assert!(repaired.closed_run, "the run it belonged to is closed");
        assert!(!repaired.closed_turn, "the turn had already ended");
        assert_eq!(repaired.deferred_to_resume, None);

        let child_log = h.log(&child).await;
        let closer = child_log
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::TOOL_OUTCOME_UNKNOWN)
            .expect("the call is settled in the child");
        assert_eq!(closer.event.payload["provider_tool_call_id"], "call-1");
        assert!(
            child_log.iter().any(|e| e.event.event_type.as_str() == EventType::STEP_INTERRUPTED)
        );
        // The closers are the child's own, so the inherited count still says
        // exactly how much of it came from the parent.
        assert_eq!(child_log.len(), inherited_events + 1 + 3);

        // And the source keeps its dangling tail: a fork borrows, it does not
        // tidy up someone else's log.
        assert_eq!(h.types(&h.session_id).await, parent_types);
    }

    #[tokio::test]
    async fn a_turn_that_is_not_in_the_session_is_not_a_boundary() {
        let h = harness(vec![FakeModelProvider::answer("done")]);
        h.ask("do it").await;

        let reason = no_boundary(
            h.fork.fork(&h.session_id, Some(&TurnId::new()), "elsewhere").await.unwrap(),
        );
        assert!(reason.contains("not in this session"), "{reason}");
    }

    #[tokio::test]
    async fn a_call_left_waiting_in_the_inherited_history_blocks_the_fork() {
        // The guard stated as the fact itself: a call with no answer, and no
        // yield to make it visible as one. Nothing in this build writes that
        // pair — the loop yields the turn whenever it holds a call — so the log
        // is seeded. It is checked anyway, because the failure it prevents (a
        // child running a call the parent still means to answer) lands in a
        // session the parent never hears about.
        let h = harness(vec![FakeModelProvider::answer("unused")]);
        let turn = TurnId::new();
        seed(&h.store, &h.session_id, held_call(&h.session_id, &turn, false)).await;

        let reason = no_boundary(h.fork.fork(&h.session_id, Some(&turn), "anyway").await.unwrap());
        assert!(reason.contains("still waiting on an approval"), "{reason}");
        assert!(reason.contains("deploy"), "the reason names the call: {reason}");
    }

    #[tokio::test]
    async fn a_wait_that_a_later_resume_answered_does_not_block_the_fork() {
        // The same shape, one answer later: the resume settled the call. Now the
        // wait is over and the history is ordinary — a guard that read the
        // `approval_pending` without looking for what came after it would refuse
        // a perfectly good boundary.
        let h = harness(vec![FakeModelProvider::answer("unused")]);
        let turn = TurnId::new();
        seed(&h.store, &h.session_id, held_call(&h.session_id, &turn, true)).await;

        let (_, at_turn, inherited) =
            forked(h.fork.fork(&h.session_id, Some(&turn), "after the answer").await.unwrap());
        assert_eq!(at_turn, turn);
        assert_eq!(inherited, 6, "the whole seeded history came across");
    }

    /// A finished turn whose step and tool call were left open — what a crash
    /// between a dispatch and its outcome leaves, plus the turn that ended.
    fn interrupted_tail(session_id: &SessionId, turn: &TurnId) -> Vec<EventDraft> {
        let run = RunId::new();
        let step = StepId::new();
        let at = |draft: EventDraft| {
            let mut draft = draft.with_run(&run).with_turn(turn);
            draft.step_id = Some(step.clone());
            draft
        };
        vec![
            draft(session_id, EventType::RUN_STARTED, serde_json::json!({}), Actor::Scheduler)
                .with_run(&run),
            draft(session_id, EventType::TURN_STARTED, serde_json::json!({}), Actor::Scheduler)
                .with_run(&run)
                .with_turn(turn),
            at(draft(session_id, EventType::STEP_STARTED, serde_json::json!({}), Actor::Scheduler)),
            at(draft(
                session_id,
                EventType::TOOL_STARTED,
                serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call-1" }),
                Actor::Tool,
            )),
            draft(session_id, EventType::TURN_COMPLETED, serde_json::json!({}), Actor::Scheduler)
                .with_run(&run)
                .with_turn(turn),
        ]
    }

    /// A turn that ended with its one call held for approval, and an answer for
    /// it when `answered`.
    fn held_call(session_id: &SessionId, turn: &TurnId, answered: bool) -> Vec<EventDraft> {
        let mut drafts = vec![
            draft(session_id, EventType::TURN_STARTED, serde_json::json!({}), Actor::Scheduler)
                .with_turn(turn),
            draft(
                session_id,
                EventType::TOOL_STARTED,
                serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call-1" }),
                Actor::Tool,
            )
            .with_turn(turn),
            draft(
                session_id,
                EventType::TOOL_FAILED,
                serde_json::json!({
                    "tool_name": "deploy",
                    "provider_tool_call_id": "call-1",
                    "outcome": APPROVAL_PENDING_OUTCOME,
                    "approval_id": "appr-1",
                }),
                Actor::Tool,
            )
            .with_turn(turn),
        ];
        if answered {
            // What a resume writes: a second dispatch, and the real outcome.
            drafts.push(
                draft(
                    session_id,
                    EventType::TOOL_STARTED,
                    serde_json::json!({
                        "tool_name": "deploy",
                        "provider_tool_call_id": "call-1",
                        "resumed": true,
                    }),
                    Actor::Tool,
                )
                .with_turn(turn),
            );
            drafts.push(
                draft(
                    session_id,
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({
                        "tool_name": "deploy",
                        "provider_tool_call_id": "call-1",
                        "status": "completed",
                        "content": "done",
                    }),
                    Actor::Tool,
                )
                .with_turn(turn),
            );
        }
        drafts.push(
            draft(session_id, EventType::TURN_COMPLETED, serde_json::json!({}), Actor::Scheduler)
                .with_turn(turn),
        );
        drafts
    }

    fn types_of(events: &[StoredEvent]) -> Vec<String> {
        events.iter().map(|e| e.event.event_type.to_string()).collect()
    }

    /// Envelope ids for logs this build does not produce.
    trait WithIds {
        fn with_turn(self, turn: &TurnId) -> Self;
        fn with_run(self, run: &RunId) -> Self;
    }

    impl WithIds for EventDraft {
        fn with_turn(mut self, turn: &TurnId) -> Self {
            self.turn_id = Some(turn.clone());
            self
        }

        fn with_run(mut self, run: &RunId) -> Self {
            self.run_id = Some(run.clone());
            self
        }
    }

    async fn seed(store: &FakeEventStore, session_id: &SessionId, drafts: Vec<EventDraft>) {
        store.append(session_id, EventSeq::ZERO, drafts).await.unwrap();
    }
}
