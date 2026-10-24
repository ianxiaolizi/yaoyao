//! Scheduler — claims inbox items, enforces single-active-run, drives the
//! AgentLoop, and settles turn/run lifecycle.
//!
//! The Scheduler is stateless: every decision folds the event stream, so a
//! restarted process sees the same open/active state as a long-running one.
//! It owns an Inbox and an AgentLoop port. What the app layer injects is the
//! store and the loop: the Inbox is built from that same store rather than
//! passed in, because the items a run claims and the log every decision folds
//! are one stream.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use agent_ports::agent_loop::{
    AgentLoop, AgentLoopError, SteeringInput, StopReason, TurnCapabilityFacts, TurnContext,
    TurnFailureFacts, TurnInputKind, TurnStart, TurnStateFacts,
};
use agent_ports::control::{ControlError, ControlHandler, ControlOutcome, ControlRequest};
use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::instructions::{InstructionError, InstructionSource};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{
    EventId, EventSeq, InboxItemId, RunId, SchemaVersion, SessionId, StepId, TurnId,
};
use agent_protocol::inbox::{InboxItem, InboxKind};
use agent_protocol::profile::ResolvedProfileSnapshot;
use chrono::{DateTime, Utc};

use crate::approval::{ApprovalError, ApprovalService};
use crate::inbox::{DiscardCause, Due, Inbox, InboxError};
use crate::judgment;
use crate::lifecycle;
use crate::plan;

/// Drives runs: claims input, enforces a single active run, and settles turns.
pub struct Scheduler {
    store: Arc<dyn EventStore>,
    inbox: Inbox,
    loop_: Arc<dyn AgentLoop>,
    max_steps: u32,
    /// The configuration this session's runs execute under, when the app layer
    /// resolved one. `None` is a real state — a harness or a tool that has no
    /// configuration to speak of — and it means the run records no snapshot
    /// rather than recording a made-up one.
    profile: Option<ResolvedProfileSnapshot>,
    /// What can be changed about the running system, and how. Empty is a real
    /// state: a harness that assembled nothing has nothing to apply a control
    /// to, and every control is then refused by name rather than ignored.
    controls: Vec<Arc<dyn ControlHandler>>,
    /// Where each run's instructions come from, when the app layer has a
    /// source. Resolved once per run rather than once per process: a long
    /// session must see an edited instruction file on its next turn, and a
    /// scheduler with none records no instructions rather than inventing them.
    instructions: Option<Arc<dyn InstructionSource>>,
    /// What the run can do, when the app layer assembled a boundary. `None` is
    /// a real state for harnesses that built no sandbox; the loop then places
    /// no capability layer rather than describing a boundary that does not
    /// exist.
    capability: Option<TurnCapabilityFacts>,
}

/// What one driven turn produced.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub stop_reason: StopReason,
    pub final_content: Option<String>,
}

/// Errors from Scheduler operations.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("inbox error: {0}")]
    Inbox(#[from] InboxError),
    #[error("agent loop error: {0}")]
    Loop(#[from] AgentLoopError),
    /// The run's instructions could not be assembled. Nothing has been written
    /// and the item is still pending: this is a run that did not start, not a
    /// turn that failed.
    #[error("instructions: {0}")]
    Instructions(#[from] InstructionError),
    #[error("approval error: {0}")]
    Approval(#[from] ApprovalError),
    #[error("payload error: {0}")]
    Payload(#[from] serde_json::Error),
    #[error(
        "turn {turn_id} is waiting on a decision; settle it with `resume` before starting another"
    )]
    AwaitingDecision { turn_id: TurnId },
    #[error(
        "control '{control}' is claimed by two handlers, '{first}' and '{second}'; a control name \
         has to reach exactly one implementation, or which one answers turns on assembly order"
    )]
    DuplicateControl { control: String, first: String, second: String },
    #[error(
        "two control handlers are both named '{name}'; every refusal is recorded under that name, \
         so the record could not say who answered"
    )]
    DuplicateHandlerName { name: String },
}

impl Scheduler {
    pub fn new(store: Arc<dyn EventStore>, loop_: Arc<dyn AgentLoop>, max_steps: u32) -> Self {
        let inbox = Inbox::new(store.clone());
        Self {
            store,
            inbox,
            loop_,
            max_steps,
            profile: None,
            controls: Vec::new(),
            instructions: None,
            capability: None,
        }
    }

    /// Tell the scheduler what the run's execution boundary can do.
    ///
    /// Set by the app layer, which chose the sandbox and the network policy.
    /// The scheduler hands these to every turn it opens; it does not infer them
    /// from the profile, because the sandbox is a host decision and not a
    /// project one (CONTRACT §7 #34).
    pub fn with_capability(mut self, capability: TurnCapabilityFacts) -> Self {
        self.capability = Some(capability);
        self
    }

    /// Give this scheduler the handlers that can apply a control.
    ///
    /// Set by the app layer, and only there: what a control needs is whatever
    /// the app built and therefore still holds — a plugin registry, a handle to
    /// a registration. A scheduler with none is not broken; it is one that can
    /// apply nothing, and it says so by refusing every control by name.
    ///
    /// # A name reaches one handler and no other
    ///
    /// Routing asks the handlers in the order they were passed and takes the
    /// first claimant (`apply_controls`), so a second claimant is not an
    /// implementation that lost a vote — it is one nothing can ever reach, while
    /// which handler answers the operator turns on nothing but the order
    /// assembly happened to pass them in. `CompositeToolProvider` refuses that
    /// shape on the tool surface for the same reason, and a surface whose meaning
    /// depends on assembly order is not one anybody chose; this is the control
    /// half of it. So the handlers are checked here, when the scheduler is built,
    /// rather than when a control arrives. (YA-46)
    ///
    /// The handlers' own [`name`]s are checked too. That string is the subject of
    /// every sentence the record writes about a refusal — "`plugin` refused it:
    /// …" — so two implementations answering to one name leave the record unable
    /// to say who answered.
    ///
    /// [`name`]: ControlHandler::name
    pub fn with_control_handlers(
        mut self,
        handlers: Vec<Arc<dyn ControlHandler>>,
    ) -> Result<Self, SchedulerError> {
        let mut claimed: BTreeMap<String, String> = BTreeMap::new();
        let mut named: BTreeSet<String> = BTreeSet::new();
        for handler in &handlers {
            let name = handler.name().to_string();
            if !named.insert(name.clone()) {
                return Err(SchedulerError::DuplicateHandlerName { name });
            }
            for control in handler.handles() {
                // One handler naming a control twice still leaves one
                // implementation behind the name, so it is not a collision —
                // only a second handler standing behind it is.
                if claimed.get(&control).is_some_and(|first| first == &name) {
                    continue;
                }
                if let Some(first) = claimed.insert(control.clone(), name.clone()) {
                    return Err(SchedulerError::DuplicateControl { control, first, second: name });
                }
            }
        }
        self.controls = handlers;
        Ok(self)
    }

    /// Record `snapshot` as `profile/resolved` when a run opens.
    ///
    /// Set by the app layer, which is the only place that reads configuration
    /// files. A scheduler without one still runs; what it cannot do is tell a
    /// later reader what configuration it was running under.
    pub fn with_profile_snapshot(mut self, snapshot: ResolvedProfileSnapshot) -> Self {
        self.profile = Some(snapshot);
        self
    }

    /// Ask `source` for the instructions of every run this scheduler opens.
    ///
    /// Set by the app layer, and resolved per run rather than here: what
    /// changed since the last turn is a fact only the source can check. A
    /// failure is a run that did not start, so the call sits before the turn is
    /// opened and before anything is written.
    pub fn with_instructions(mut self, source: Arc<dyn InstructionSource>) -> Self {
        self.instructions = Some(source);
        self
    }

    /// The scheduler's Inbox, for submitting inputs to this session.
    pub fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    /// The current open-entity lifecycle for this session.
    pub async fn lifecycle(
        &self,
        session_id: &SessionId,
    ) -> Result<lifecycle::Lifecycle, SchedulerError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(lifecycle::fold(&events))
    }

    /// Whether a run is currently open for this session.
    pub async fn has_active_run(&self, session_id: &SessionId) -> Result<bool, SchedulerError> {
        Ok(self.lifecycle(session_id).await?.open_run.is_some())
    }

    /// Drive the next pending `next_turn` item, if any and if no run is active.
    ///
    /// The turn runs one step at a time, and the inbox is drained at every step
    /// boundary: input the user injects while the turn is in flight reaches the
    /// loop there, and never interrupts the step in progress.
    ///
    /// The claim, the run/turn open events, the steps' events, the steering
    /// events, and the turn/run terminal events are appended in one batch. That
    /// is deliberate: a turn that dies part-way through consumes nothing, so a
    /// step boundary is a place to inject work into, not a place to commit from.
    ///
    /// A session paused at `turn/yielded` is refused rather than served: the
    /// call it stopped on is still unanswered, so opening a turn now would leave
    /// it unanswered for good — and the transcript would be one no provider
    /// accepts, a proposed tool call with an unrelated user message behind it.
    /// The answer to that state is `resume`, which settles the call first.
    ///
    /// A stop arriving *while* the session is held is the one thing this path
    /// has to handle itself. Nothing else consumes an interrupt on a held
    /// session — the interrupt is only read inside the step boundary of a
    /// running turn, and there is no running turn here — so a request the person
    /// has walked away from would stand until it expired on its own. Cancelling
    /// it here settles the wait the same way `expired` does, and the refusal
    /// below still stands: the turn is not carried forward until `resume` runs.
    ///
    /// `now` is the instant the turn opens on. It is not used to decide whether
    /// anything is due — a `NextTurn` item is always ready — but it is the
    /// time the turn's state layer reports to the model, so it is an input
    /// rather than a clock read here (CONTRACT §7 #20).
    pub async fn run_pending(
        &self,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Option<RunReport>, SchedulerError> {
        if self.gate(session_id).await? == Gate::Busy {
            return Ok(None);
        }

        // Between two runs is a boundary, and it comes before the turn
        // below so that what a control changes is what the turn that follows it
        // is built from — never the turn it arrived during. Applying controls
        // here also keeps their events out of any turn's batch: a turn's events
        // are appended as one, and a change made *between* turns is not part of
        // one.
        self.apply_controls(session_id).await?;

        let Some(item) = self.inbox.next_turn(session_id).await? else {
            return Ok(None);
        };
        Ok(Some(self.run_item(session_id, item, now).await?))
    }

    /// Drive the earliest scheduled item that is due at `now`, if any.
    ///
    /// Same gate as [`run_pending`](Self::run_pending): a held session is
    /// refused (and a pending interrupt there is cancelled), and an open run
    /// means the item waits. At most one turn is started per call; a queue of
    /// due items drains one `run_due` at a time, so a caller that loops can
    /// observe each turn between them.
    ///
    /// A scheduled item whose payload cannot be read is discarded with the
    /// reason and the search continues, so a bad item cannot block the queue.
    ///
    /// # This is at-least-once
    ///
    /// The item is claimed inside the run's terminal batch. A process that
    /// dies before that batch lands leaves the item pending, and the next
    /// `run_due` runs it again from the top — model calls and tool side effects
    /// in the failed attempt may happen twice. There is no exactly-once
    /// guarantee to be had here: the terminal batch is the only record that
    /// the work finished, so anything that stops short of it is retried.
    pub async fn run_due(
        &self,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Option<RunReport>, SchedulerError> {
        if self.gate(session_id).await? == Gate::Busy {
            return Ok(None);
        }
        self.apply_controls(session_id).await?;

        loop {
            match self.inbox.next_due(session_id, now).await? {
                Due::None => return Ok(None),
                Due::Unreadable { item, reason } => {
                    self.inbox
                        .discard(session_id, &item.item_id, DiscardCause::Unreadable, Some(&reason))
                        .await?;
                }
                Due::Due(item) => return Ok(Some(self.run_item(session_id, item, now).await?)),
            }
        }
    }

    /// The lifecycle gate both run entry points share.
    ///
    /// A yielded session is an error, not a "no": the call it stopped on is
    /// still unanswered, and opening a turn now would leave it unanswered for
    /// good. A stop arriving while the session is held is settled here, because
    /// nothing else consumes an interrupt on a session with no running turn.
    /// An open run is merely busy — the caller has nothing to report and no
    /// item is consumed.
    async fn gate(&self, session_id: &SessionId) -> Result<Gate, SchedulerError> {
        let lc = self.lifecycle(session_id).await?;
        if let Some(turn_id) = lc.yielded_turn.clone() {
            if self.inbox.pending_interrupt(session_id).await?.is_some() {
                ApprovalService::new(self.store.clone())
                    .cancel_pending(session_id, "user_interrupt")
                    .await?;
            }
            return Err(SchedulerError::AwaitingDecision { turn_id });
        }
        Ok(if lc.open_run.is_some() { Gate::Busy } else { Gate::Ready })
    }

    /// Drive one claimed item through a complete turn.
    ///
    /// The item is not consumed here: its claim is part of the terminal batch,
    /// so a turn that cannot finish leaves it pending.
    async fn run_item(
        &self,
        session_id: &SessionId,
        item: InboxItem,
        now: DateTime<Utc>,
    ) -> Result<RunReport, SchedulerError> {
        let run_id = RunId::new();
        let turn_id = TurnId::new();
        // Read before anything is durable: instructions that cannot be
        // assembled are a run that did not start, so the item stays pending and
        // no turn opens over a prompt this build could not compose.
        let instructions = match &self.instructions {
            Some(source) => Some(source.for_run()?),
            None => None,
        };

        // What the world looks like at the boundary this turn opens on. Read
        // before anything is durable, for the same reason the instructions
        // are: a state layer that could not be assembled is a turn that did
        // not start, and the item must stay pending.
        //
        // The queued count excludes the item being run — it is still pending
        // until the terminal batch — and counts only inputs that will start a
        // turn of their own. Steering is not one of those: it is injected as a
        // user message inside this turn, so counting it would claim work is
        // waiting when the model is about to be handed it.
        let queued_inputs = self
            .inbox
            .pending(session_id)
            .await?
            .into_iter()
            .filter(|pending| pending.item_id != item.item_id)
            .filter(|pending| matches!(&pending.kind, InboxKind::NextTurn | InboxKind::Scheduled))
            .count();
        let history = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        let previous_turn = lifecycle::last_turn_ending(&history).map(|(_turn_id, ending)| ending);
        // The plan ledger as of this boundary. `Some` even when nothing has
        // been recorded: the Scheduler keeps the ledger, and "none recorded
        // yet" is a fact worth placing, unlike a caller that has no ledger at
        // all (`None`).
        let plan = plan::current(&history)?.unwrap_or_default();
        // The judgment record, on the same terms: the Scheduler keeps the
        // ledger, and "none recorded yet" is a fact worth placing.
        let judgment = judgment::current(&history)?.unwrap_or_default();
        // A resume continuation opens on a turn that already closed: the held
        // call ran and may have failed there. The loop has no memory of that
        // turn, so the failure is read back from the log and handed to it.
        let pending_failure =
            if item.payload.get("resumed").and_then(|value| value.as_bool()).unwrap_or(false) {
                judgment::pending_failure(&history).map(|pending| TurnFailureFacts {
                    call_id: pending.call_id,
                    tool: pending.tool,
                    detail: pending.detail,
                })
            } else {
                None
            };
        let state = TurnStateFacts {
            started_at: now,
            queued_inputs,
            input_kind: match &item.kind {
                InboxKind::Scheduled => TurnInputKind::Scheduled,
                _ => TurnInputKind::User,
            },
            previous_turn,
            max_steps: self.max_steps,
        };
        let ctx = TurnContext {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            run_id: run_id.clone(),
            user_input: item.payload.clone(),
            instructions: instructions.clone(),
            state: Some(state),
            capability: self.capability.clone(),
            plan: Some(plan),
            judgment: Some(judgment),
            pending_failure,
        };

        // Opening the turn comes before the claim for the same reason the loop
        // used to run before it: a turn that could not start must leave the item
        // pending rather than consume it.
        let TurnStart { mut state, events: opening } = self.loop_.begin_turn(&ctx).await?;

        let mut drafts = Vec::new();
        drafts.push(self.inbox.claim_draft(session_id, &item.item_id, Some(&run_id)).await?);
        // What this run is, before it is a run. The snapshot belongs to the run
        // and not to the session: a session can be driven by several processes
        // over its life, and the later ones may be configured differently.
        if let Some(profile) = &self.profile {
            drafts.push(draft(
                session_id,
                EventType::PROFILE_RESOLVED,
                serde_json::to_value(profile)?,
                Actor::Scheduler,
                Some(&run_id),
                None,
                None,
            ));
        }
        drafts.push(draft(
            session_id,
            EventType::RUN_STARTED,
            match &instructions {
                Some(instructions) => serde_json::json!({ "instructions": instructions }),
                None => serde_json::json!({}),
            },
            Actor::Scheduler,
            Some(&run_id),
            None,
            None,
        ));
        drafts.push(draft(
            session_id,
            EventType::TURN_STARTED,
            serde_json::json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
            Some(&run_id),
            Some(&turn_id),
            None,
        ));
        drafts.extend(opening);

        let mut steps = 0u32;
        let mut last_step: Option<StepId> = None;
        // What this turn has already taken out of the queue, whether it went to
        // a step or was discarded as unreadable. The records are in `drafts`, not
        // in the log — the whole turn appends in one batch — so the store still
        // reports these items as pending, and the next boundary would read them
        // again. Being consumed by the turn is a fact only the turn can know
        // until it settles.
        let mut consumed: HashSet<InboxItemId> = HashSet::new();
        let ending = loop {
            if steps >= self.max_steps {
                break Ending::Budget;
            }
            // An interrupt ends the turn at the boundary rather than inside a
            // step: what it stops is the next step, not the one already running.
            if let Some(interrupt) = self.inbox.pending_interrupt(session_id).await? {
                drafts.push(
                    self.inbox.claim_draft(session_id, &interrupt.item_id, Some(&run_id)).await?,
                );
                break Ending::Interrupted { item_id: interrupt.item_id };
            }

            let steering = self
                .drain_steering(
                    session_id,
                    &run_id,
                    &turn_id,
                    last_step.as_ref(),
                    &mut consumed,
                    &mut drafts,
                )
                .await?;
            let outcome = self.loop_.step(&mut state, &steering).await?;
            drafts.extend(outcome.events);
            last_step = Some(outcome.step_id);
            steps += 1;

            if let Some(reason) = outcome.stop {
                break Ending::Loop { reason, final_content: outcome.final_content };
            }
        };

        let (stop_reason, final_content, turn_terminal, run_terminal, turn_payload) =
            ending.settle();
        drafts.push(draft(
            session_id,
            turn_terminal,
            turn_payload,
            Actor::Scheduler,
            Some(&run_id),
            Some(&turn_id),
            None,
        ));
        drafts.push(draft(
            session_id,
            run_terminal,
            serde_json::json!({}),
            Actor::Scheduler,
            Some(&run_id),
            None,
            None,
        ));

        // The terminal batch is the run's last write. The sequence is read from
        // the log and appended at it as one step — another process on this
        // session moves it otherwise, and the append is refused. Held only for
        // these two calls, not across the turn. (YA-36)
        let _lease = self.store.lease(session_id).await?;
        let next = self.next_seq(session_id).await?;
        self.store.append(session_id, next, drafts).await?;

        Ok(RunReport { run_id, turn_id, stop_reason, final_content })
    }

    /// Apply the changes waiting at this boundary, one at a time, in order.
    ///
    /// Each item is decided before it is recorded, because the record is the
    /// decision: a control that took effect is *claimed* — the queue's word for
    /// "taken out, and not by a run" — and one that did not is *discarded with
    /// the reason*. Recording first and deciding after would make a refused
    /// control look exactly like an applied one to anyone reading the log.
    ///
    /// The order matters for the failure it leaves behind: the handler acts
    /// before the claim is written, so a process that dies in between leaves the
    /// item pending and the change already made. Applying it again is then
    /// harmless — the second attempt refuses, because there is nothing left to
    /// do — which is why this is safe to run on every boundary rather than
    /// exactly once.
    async fn apply_controls(&self, session_id: &SessionId) -> Result<(), SchedulerError> {
        for item in self.inbox.controls(session_id).await? {
            let control = item
                .payload
                .get("control")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let handler = self.controls.iter().find(|h| h.handles().contains(&control));

            // The three ways a control can fail to take effect are different
            // facts — nobody claimed it, a handler declined, a handler broke —
            // and the port says they are recorded differently. The cause is
            // what carries that distinction into the log; the sentence beside
            // it is for the person reading (C-1).
            let refusal = match handler {
                None => Some((DiscardCause::Unhandled, self.nothing_handles(&control))),
                Some(handler) => {
                    let request = ControlRequest {
                        session_id: session_id.clone(),
                        item_id: item.item_id.clone(),
                        control: control.clone(),
                        payload: item.payload.clone(),
                    };
                    match handler.apply(request).await {
                        Ok(ControlOutcome::Applied { .. }) => None,
                        Ok(ControlOutcome::Refused { reason }) => Some((
                            DiscardCause::Refused,
                            format!("{} refused it: {reason}", handler.name()),
                        )),
                        Err(ControlError::Failed { message, .. }) => Some((
                            DiscardCause::Failed,
                            format!("{} could not apply it: {message}", handler.name()),
                        )),
                    }
                }
            };

            match refusal {
                // Applied: the change is already made and recorded by whoever
                // made it. What is left is to take the item out of the queue.
                None => {
                    // Claim and append are one step, like every other writer
                    // here: the item is re-read to build the claim, and the
                    // append must land on the log that read saw. (YA-36)
                    let _lease = self.store.lease(session_id).await?;
                    let draft = self.inbox.claim_draft(session_id, &item.item_id, None).await?;
                    let next = self.next_seq(session_id).await?;
                    self.store.append(session_id, next, vec![draft]).await?;
                }
                Some((cause, reason)) => {
                    self.inbox.discard(session_id, &item.item_id, cause, Some(&reason)).await?;
                }
            }
        }
        Ok(())
    }

    /// What to say when nobody claims a control.
    ///
    /// A refusal that only said "no" would send the operator to read the source;
    /// naming the handlers is what lets them see that the control they sent is
    /// one this build does not have — or that it has none at all.
    fn nothing_handles(&self, control: &str) -> String {
        let handled: Vec<String> = self
            .controls
            .iter()
            .flat_map(|h| h.handles())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if handled.is_empty() {
            format!("no control '{control}' can be applied: this run registered no handlers")
        } else {
            format!(
                "no handler claims control '{control}'; this run handles: {}",
                handled.join(", ")
            )
        }
    }

    /// Claim what the user injected for this turn, and describe it to the loop.
    ///
    /// Items addressed to a different turn stay pending: a note that arrived
    /// early is not a note that is wrong, and injecting it here would be the
    /// race `expected_turn_id` exists to prevent.
    ///
    /// `consumed` is what keeps a note from being injected twice: it records the
    /// items this turn has already taken, which the log cannot yet say.
    async fn drain_steering(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        turn_id: &TurnId,
        after_step: Option<&StepId>,
        consumed: &mut HashSet<InboxItemId>,
        drafts: &mut Vec<EventDraft>,
    ) -> Result<Vec<SteeringInput>, SchedulerError> {
        let mut inputs = Vec::new();
        for item in self.inbox.steering(session_id).await? {
            if consumed.contains(&item.item_id) {
                continue;
            }
            let addressed_to = item.payload.get("expected_turn_id").and_then(|v| v.as_str());
            if addressed_to.is_some_and(|expected| expected != turn_id.as_str()) {
                continue;
            }
            // A steering item without a string `text` is not a note to inject:
            // injecting the empty string would hand the model a message the
            // caller never wrote, and consuming it would hide the bad payload.
            // Discard it in this turn's batch with a reason a log reader can
            // act on. (YA-55)
            let Some(text) = item.payload.get("text").and_then(|v| v.as_str()) else {
                drafts.push(
                    self.inbox
                        .discard_draft(
                            session_id,
                            &item.item_id,
                            DiscardCause::Unreadable,
                            Some("steering payload has no string `text`"),
                        )
                        .await?,
                );
                consumed.insert(item.item_id.clone());
                continue;
            };
            drafts.push(self.inbox.claim_draft(session_id, &item.item_id, Some(run_id)).await?);
            // No `step_id`: nothing is running at a boundary, and naming the
            // step that just ended would say this happened *during* it.
            drafts.push(draft(
                session_id,
                EventType::TURN_STEERED,
                serde_json::json!({
                    "item_id": item.item_id,
                    "text": text,
                    "after_step_id": after_step,
                }),
                Actor::Scheduler,
                Some(run_id),
                Some(turn_id),
                None,
            ));
            consumed.insert(item.item_id.clone());
            inputs.push(SteeringInput { item_id: item.item_id, text: text.to_string() });
        }
        Ok(inputs)
    }

    async fn next_seq(&self, session_id: &SessionId) -> Result<EventSeq, SchedulerError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO))
    }
}

/// What the session's lifecycle says about starting a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// No run is open and nothing is held.
    Ready,
    /// A run is already open; the caller should report nothing and consume
    /// nothing.
    Busy,
}

/// How a turn stopped.
///
/// Three endings, because two of them are decisions the Scheduler makes rather
/// than facts the loop reports, and collapsing them into a `StopReason` would
/// lose that: an exhausted step budget and a user interrupt are not things the
/// loop decided, they are things that were done to the turn.
enum Ending {
    /// The loop said the turn was over.
    Loop { reason: StopReason, final_content: Option<String> },
    /// The step budget ran out. The budget is the Scheduler's policy, so only
    /// the Scheduler can report this ending.
    Budget,
    /// The user interrupted the turn at a step boundary.
    Interrupted { item_id: InboxItemId },
}

impl Ending {
    /// The stop reason, the final content, the two terminal event types, and
    /// the terminal payload.
    #[allow(clippy::type_complexity)]
    fn settle(
        &self,
    ) -> (StopReason, Option<String>, &'static str, &'static str, serde_json::Value) {
        let reason = match self {
            Ending::Loop { reason, .. } => reason.clone(),
            Ending::Budget => StopReason::MaxSteps,
            Ending::Interrupted { .. } => StopReason::UserCancelled,
        };
        let final_content = match self {
            Ending::Loop { final_content, .. } => final_content.clone(),
            _ => None,
        };
        let (turn_event, run_event) = match &reason {
            StopReason::FinalAnswer | StopReason::MaxSteps => {
                (EventType::TURN_COMPLETED, EventType::RUN_COMPLETED)
            }
            StopReason::NeedsUserInput => (EventType::TURN_YIELDED, EventType::RUN_YIELDED),
            StopReason::FatalError => (EventType::TURN_FAILED, EventType::RUN_FAILED),
            StopReason::UserCancelled => (EventType::TURN_CANCELLED, EventType::RUN_CANCELLED),
        };
        // An interrupt names its cause, because `turn/cancelled` alone cannot
        // say whether the user stopped the turn or an adapter abandoned an
        // attempt. The paths that predate the interrupt keep their exact
        // payload — the cancelled turn's shape is not this change's to alter.
        let payload = match self {
            Ending::Interrupted { item_id } => serde_json::json!({
                "cause": "user_interrupt",
                "inbox_item": item_id,
            }),
            _ => match &final_content {
                Some(content) => serde_json::json!({ "final_content": content }),
                None => serde_json::json!({}),
            },
        };
        (reason, final_content, turn_event, run_event, payload)
    }
}

fn draft(
    session_id: &SessionId,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
    run_id: Option<&RunId>,
    turn_id: Option<&TurnId>,
    step_id: Option<&StepId>,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session_id.clone(),
        run_id: run_id.cloned(),
        turn_id: turn_id.cloned(),
        step_id: step_id.cloned(),
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
    use agent_loop::DefaultAgentLoop;
    use agent_ports::agent_loop::{StepOutcome, TurnState};
    use agent_ports::execution_environment::SandboxMode;
    use agent_ports::instructions::{InstructionError, InstructionSource};
    use agent_ports::model_provider::{
        ModelCapability, ModelProvider, ModelProviderError, ModelRequest, ModelResponse,
        ModelStreamSink,
    };
    use agent_ports::tool_provider::ToolSideEffect;
    use agent_protocol::events::StoredEvent;
    use agent_protocol::ids::InboxItemId;
    use agent_protocol::inbox::InboxKind;
    use agent_protocol::instructions::RunInstructions;
    use agent_protocol::model::{ContentBlock, ModelMessage};
    use agent_testkit::{
        FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine,
        one_model_router,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn scheduler() -> (Scheduler, Arc<FakeEventStore>, SessionId) {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("hello")]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = Arc::new(DefaultAgentLoop::new(
            model,
            gateway,
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let scheduler = Scheduler::new(store.clone(), loop_, 8);
        let sid = SessionId::from_string("sched-session");
        (scheduler, store, sid)
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

    #[tokio::test]
    async fn run_pending_drives_a_turn_to_completion() {
        let (scheduler, store, sid) = scheduler();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(report.stop_reason, StopReason::FinalAnswer);
        assert_eq!(report.final_content.as_deref(), Some("hello"));

        assert_eq!(
            event_types(&store, &sid).await,
            vec![
                "inbox/appended",
                "inbox/claimed",
                "run/started",
                "turn/started",
                "model/tools.frozen",
                "reason/recorded",
                "reason/recorded",
                "reason/recorded",
                "step/started",
                "model/request.prepared",
                "model/attempt.started",
                "model/response.completed",
                "reason/recorded",
                "step/completed",
                "turn/completed",
                "run/completed",
            ]
        );
        assert!(!scheduler.has_active_run(&sid).await.unwrap());
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());
    }

    /// The run's last write is appended at the sequence read from the log, and
    /// the lease spans both. The store refuses an append that arrives without
    /// one, so this passes only while the terminal batch is covered. (YA-36)
    #[tokio::test]
    async fn a_runs_final_batch_is_appended_under_the_session_lease() {
        let store = Arc::new(FakeEventStore::new().requiring_lease());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("hello")]));
        let scheduler = scheduler_over(store, model, 8);
        let sid = SessionId::from_string("sched-session");
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(report.stop_reason, StopReason::FinalAnswer);
    }

    /// A control applied at a boundary claims its item and appends at the
    /// sequence it reads, as one step — so it too only lands under the lease.
    /// (YA-36)
    #[tokio::test]
    async fn an_applied_control_is_recorded_under_the_session_lease() {
        let store = Arc::new(FakeEventStore::new().requiring_lease());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("unused")]));
        let handler = Arc::new(RecordingControl::applying(&["test.retire"]));
        let scheduler = scheduler_over(store, model, 8)
            .with_control_handlers(vec![handler.clone()])
            .expect("one handler claims one name");
        let sid = SessionId::from_string("control-session");
        scheduler
            .inbox
            .append(
                &sid,
                InboxKind::Control,
                serde_json::json!({ "control": "test.retire" }),
                0,
                Actor::User,
            )
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap();
        assert_eq!(handler.applied(), 1);
    }

    #[tokio::test]
    async fn run_pending_with_no_items_returns_none() {
        let (scheduler, _, sid) = scheduler();
        assert!(scheduler.run_pending(&sid, Utc::now()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn single_active_run_skips_new_work() {
        let (scheduler, store, sid) = scheduler();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        // Seed an open run directly, so the gate must skip the pending item.
        let run_id = RunId::new();
        let next = scheduler.next_seq(&sid).await.unwrap();
        store
            .append(
                &sid,
                next,
                vec![draft(
                    &sid,
                    EventType::RUN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run_id),
                    None,
                    None,
                )],
            )
            .await
            .unwrap();

        assert!(scheduler.run_pending(&sid, Utc::now()).await.unwrap().is_none());
        // The item must still be pending (nothing was claimed).
        assert_eq!(scheduler.inbox.pending(&sid).await.unwrap().len(), 1);
    }

    /// A loop that hands the turn back to the user every time it is asked for a
    /// step. The turn state is `()`, which is all this double needs to carry.
    struct YieldLoop;

    #[async_trait::async_trait]
    impl AgentLoop for YieldLoop {
        async fn begin_turn(&self, _ctx: &TurnContext) -> Result<TurnStart, AgentLoopError> {
            Ok(TurnStart { state: TurnState::new(()), events: vec![] })
        }

        async fn step(
            &self,
            _turn: &mut TurnState,
            _steering: &[SteeringInput],
        ) -> Result<StepOutcome, AgentLoopError> {
            Ok(StepOutcome {
                step_id: StepId::new(),
                events: vec![],
                stop: Some(StopReason::NeedsUserInput),
                final_content: None,
            })
        }
    }

    #[tokio::test]
    async fn needs_user_input_yields_turn_and_run() {
        let store = Arc::new(FakeEventStore::new());
        let scheduler = Scheduler::new(store.clone(), Arc::new(YieldLoop), 8);
        let sid = SessionId::from_string("yield-session");
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "?"}), 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(report.stop_reason, StopReason::NeedsUserInput);

        let types = event_types(&store, &sid).await;
        assert!(types.contains(&"turn/yielded".to_string()));
        assert!(types.contains(&"run/yielded".to_string()));
        // A yielded run is no longer active, so a subsequent run may proceed.
        assert!(!scheduler.has_active_run(&sid).await.unwrap());
    }

    #[tokio::test]
    async fn a_fatal_turn_fails_the_run_instead_of_completing_it() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("fatal-session");
        // The provider declares no capabilities, so the loop's capability check
        // fails the attempt before any request goes out.
        let model = Arc::new(
            FakeModelProvider::new(vec![FakeModelProvider::answer("never sent")])
                .with_capabilities(vec![]),
        );
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = Arc::new(DefaultAgentLoop::new(
            model,
            gateway,
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let scheduler = Scheduler::new(store.clone(), loop_, 8);
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(report.stop_reason, StopReason::FatalError);

        let types = event_types(&store, &sid).await;
        assert!(types.contains(&"model/attempt.failed".to_string()), "{types:?}");
        assert!(types.contains(&"turn/failed".to_string()), "{types:?}");
        assert!(types.contains(&"run/failed".to_string()), "{types:?}");
        assert!(!types.contains(&"turn/completed".to_string()), "{types:?}");
        // A failed run is no longer active, so later work can proceed.
        assert!(!scheduler.has_active_run(&sid).await.unwrap());
    }

    #[test]
    fn inbox_item_id_round_trips_in_claim_payload() {
        // Guard the claim payload key shape that fold_inbox relies on.
        let id = InboxItemId::from_string("abc");
        let payload = serde_json::json!({ "item_id": id });
        let decoded: InboxItemId =
            serde_json::from_value(payload.get("item_id").cloned().unwrap()).unwrap();
        assert_eq!(decoded.as_str(), "abc");
    }

    fn snapshot() -> ResolvedProfileSnapshot {
        ResolvedProfileSnapshot {
            profile_id: "coding".to_string(),
            config_hash: String::new(),
            plugin_versions: std::collections::BTreeMap::new(),
            policy_version: "deadbeef".to_string(),
            decision_backends: Vec::new(),
            model_route: agent_protocol::model::ModelRoute::new(
                agent_protocol::model::ModelTier::Main,
                "echo",
                "echo",
            ),
            capability_set: agent_protocol::permissions::CapabilitySet::new(),
            budget: agent_protocol::profile::RunBudget::default(),
        }
        .sealed()
    }

    /// What a run is, before it is a run.
    ///
    /// The snapshot belongs to the run, not to the session: a session can be
    /// driven by several processes over its life and the later ones may be
    /// configured differently, so "which run was this?" is the only question
    /// this event can answer.
    #[tokio::test]
    async fn a_configured_run_records_the_configuration_it_started_under() {
        let (scheduler, store, sid) = scheduler();
        let scheduler = scheduler.with_profile_snapshot(snapshot());
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({ "text": "hi" }), 0, Actor::User)
            .await
            .unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let index = events
            .iter()
            .position(|e| e.event.event_type.as_str() == EventType::PROFILE_RESOLVED)
            .expect("the run recorded its configuration");

        assert_eq!(
            events[index + 1].event.event_type.as_str(),
            EventType::RUN_STARTED,
            "the configuration is recorded before the run it belongs to opens"
        );
        assert_eq!(events[index].event.payload["profile_id"], "coding");
        assert_eq!(
            events[index].event.run_id,
            events[index + 1].event.run_id,
            "it is a fact about that run"
        );
        assert!(events[index].event.turn_id.is_none(), "and not about a turn");
    }

    #[tokio::test]
    async fn a_run_with_no_resolved_configuration_records_none() {
        // A scheduler without one is a real state — a harness, or a command
        // that has no configuration to speak of. It writes nothing, rather than
        // recording a snapshot nobody resolved.
        let (scheduler, store, sid) = scheduler();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({ "text": "hi" }), 0, Actor::User)
            .await
            .unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        let types = event_types(&store, &sid).await;
        assert!(!types.contains(&EventType::PROFILE_RESOLVED.to_string()), "{types:?}");
    }

    // -----------------------------------------------------------------------
    // Step-boundary steering (M5 criterion 3)
    // -----------------------------------------------------------------------

    /// A scheduler over `store` and whatever model the test is about, with the
    /// one tool a two-step turn needs.
    fn scheduler_over(
        store: Arc<FakeEventStore>,
        model: Arc<dyn ModelProvider>,
        max_steps: u32,
    ) -> Scheduler {
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = Arc::new(DefaultAgentLoop::new(
            model,
            gateway,
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        Scheduler::new(store, loop_, max_steps)
    }

    /// A two-step turn: the model calls a tool, then answers.
    ///
    /// The model is returned so a test can read the requests it was sent — the
    /// model's own view of the conversation is where "the steering reached the
    /// next step" has to be shown.
    fn two_step_scheduler() -> (Scheduler, Arc<FakeEventStore>, Arc<FakeModelProvider>, SessionId) {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("done"),
        ]));
        let scheduler = scheduler_over(store.clone(), model.clone(), 8);
        (scheduler, store, model, SessionId::from_string("steer-session"))
    }

    /// The words the model was shown, message by message, in order.
    ///
    /// System messages are skipped: they are the frame around the conversation
    /// (instructions, the per-turn state and task layers, the context notice),
    /// while these tests are about what the user and the tools said.
    ///
    /// Text and tool outcomes are what carry words; a tool proposal carries none
    /// and so contributes nothing. The result reads as "what was said to the
    /// model, and when".
    fn shown_text(messages: &[ModelMessage]) -> Vec<String> {
        messages
            .iter()
            .filter(|m| m.role != agent_protocol::model::MessageRole::System)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                        ContentBlock::ToolUse { .. } => None,
                    })
                    .collect::<String>()
            })
            .filter(|t| !t.is_empty())
            .collect()
    }

    /// The one `turn/steered` event in the log, and where it sits in the chain.
    async fn steer_event(store: &FakeEventStore, sid: &SessionId) -> (StoredEvent, usize) {
        let events = store.read_stream(sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let position = events
            .iter()
            .position(|e| e.event.event_type.as_str() == EventType::TURN_STEERED)
            .expect("a steer event");
        (events[position].clone(), position)
    }

    #[tokio::test]
    async fn steering_queued_before_the_turn_joins_its_first_step() {
        let (scheduler, store, model, sid) = two_step_scheduler();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();
        // Unaddressed, because no turn has been opened yet: "steer whatever turn
        // is running" is all a caller can mean at this point. The turn boundary
        // takes both queues, so the note joins the turn's first step rather than
        // waiting for a turn that may be a long time coming.
        let steer_item = scheduler.inbox.steer(&sid, "also check the tests", None).await.unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        let turn_id = report.turn_id.clone();

        let requests = model.requests();
        assert_eq!(requests.len(), 2, "one request per step");
        assert_eq!(shown_text(&requests[0].messages), vec!["hi", "also check the tests"]);

        // Not merely the same words: the same message. Its uid is (turn, item),
        // so the log and the loop agree on its identity and a rebuild rebuilds
        // instead of producing a lookalike.
        let expected =
            ModelMessage::user_for_steering(&turn_id, &steer_item.item_id, "also check the tests");
        assert_eq!(requests[0].messages.last(), Some(&expected));

        // The log places it at the turn boundary, before any step ran — so there
        // is no step to name as the one it came after.
        let (event, position) = steer_event(&store, &sid).await;
        let types = event_types(&store, &sid).await;
        assert_eq!(types[position - 1], EventType::INBOX_CLAIMED, "{types:?}");
        assert_eq!(types[position + 1], EventType::REASON_RECORDED, "{types:?}");
        assert_eq!(types[position + 2], EventType::STEP_STARTED, "{types:?}");
        let turn_started = types
            .iter()
            .position(|kind| kind == EventType::TURN_STARTED)
            .expect("the turn opened before anything else");
        assert!(turn_started < position, "the steer landed before the turn opened: {types:?}");
        assert!(event.event.payload["after_step_id"].is_null(), "{:?}", event.event.payload);
        assert!(event.event.step_id.is_none(), "nothing runs at a boundary");

        // The note is consumed, not left pending to be replayed.
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn steering_addressed_to_another_turn_stays_pending() {
        let (scheduler, store, model, sid) = two_step_scheduler();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();
        // A note written for a turn that is not this one. Injecting it would be
        // the race `expected_turn_id` exists to prevent, and dropping it would
        // lose the user's words — so it waits.
        scheduler
            .inbox
            .steer(&sid, "for a later turn", Some(&TurnId::from_string("some-other-turn")))
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        let requests = model.requests();
        assert_eq!(shown_text(&requests[0].messages), vec!["hi"]);
        assert_eq!(shown_text(&requests[1].messages), vec!["hi", "hello"]);
        let types = event_types(&store, &sid).await;
        assert!(!types.contains(&EventType::TURN_STEERED.to_string()), "{types:?}");

        let pending = scheduler.inbox.pending(&sid).await.unwrap();
        assert_eq!(pending.len(), 1, "the note waits for its turn");
        assert_eq!(pending[0].kind, InboxKind::NextStep);
    }

    #[tokio::test]
    async fn steering_without_a_string_text_is_discarded_not_injected() {
        let (scheduler, store, model, sid) = two_step_scheduler();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();
        // A steering item whose payload has no string under `text`. It must not
        // become a message: injecting the empty string would put words in the
        // user's mouth, and claiming it would hide the bad payload.
        let malformed = scheduler
            .inbox
            .append(
                &sid,
                InboxKind::NextStep,
                serde_json::json!({"expected_turn_id": null}),
                0,
                Actor::User,
            )
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        let requests = model.requests();
        // The frame is expected (state and task layers); the point is that the
        // malformed note did not become a third message.
        assert_eq!(
            requests[0]
                .messages
                .iter()
                .filter(|m| m.role != agent_protocol::model::MessageRole::System)
                .count(),
            1,
            "{:?}",
            requests[0].messages
        );
        assert_eq!(shown_text(&requests[0].messages), vec!["hi"]);

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let types: Vec<String> = events.iter().map(|e| e.event.event_type.to_string()).collect();
        assert!(!types.contains(&EventType::TURN_STEERED.to_string()), "{types:?}");

        let discarded = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .expect("the malformed item is discarded, not silently consumed");
        assert_eq!(discarded.event.payload["item_id"], serde_json::json!(malformed.item_id));
        let reason = discarded.event.payload["reason"].as_str().expect("a readable reason");
        assert!(reason.contains("text"), "{reason}");

        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());
    }

    /// What someone else does to the inbox while this process is waiting on the
    /// model — a second process, in effect. In-process the only place a test can
    /// get a word in is inside the call that is being awaited, because
    /// `run_pending` yields nowhere else.
    enum WhileModelThinks {
        Steer {
            text: &'static str,
            expected_turn_id: Option<TurnId>,
        },
        Interrupt,
        /// A change to the runtime, arriving from outside while a turn is in
        /// flight. It is not a word to the turn, and nothing may act on it
        /// inside the turn it arrived in.
        Control {
            control: &'static str,
        },
    }

    /// A model that, on its first attempt only, lets someone else write the
    /// inbox first.
    struct MeanwhileModel {
        inner: Arc<FakeModelProvider>,
        inbox: Inbox,
        session_id: SessionId,
        action: WhileModelThinks,
        acted: AtomicBool,
        landed: Mutex<Option<InboxItemId>>,
    }

    impl MeanwhileModel {
        fn new(
            store: Arc<FakeEventStore>,
            session_id: &SessionId,
            inner: Arc<FakeModelProvider>,
            action: WhileModelThinks,
        ) -> Self {
            Self {
                inner,
                inbox: Inbox::new(store),
                session_id: session_id.clone(),
                action,
                acted: AtomicBool::new(false),
                landed: Mutex::new(None),
            }
        }

        async fn act(&self) {
            if self.acted.swap(true, Ordering::SeqCst) {
                return;
            }
            let item = match &self.action {
                WhileModelThinks::Steer { text, expected_turn_id } => {
                    self.inbox.steer(&self.session_id, *text, expected_turn_id.as_ref()).await
                }
                WhileModelThinks::Interrupt => self.inbox.interrupt(&self.session_id, "stop").await,
                WhileModelThinks::Control { control } => {
                    self.inbox.control(&self.session_id, *control, serde_json::json!({})).await
                }
            }
            .expect("the inbox takes it");
            if let Ok(mut landed) = self.landed.lock() {
                *landed = Some(item.item_id);
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for MeanwhileModel {
        async fn stream(
            &self,
            request: &ModelRequest,
            sink: &dyn ModelStreamSink,
        ) -> Result<ModelResponse, ModelProviderError> {
            self.act().await;
            self.inner.stream(request, sink).await
        }

        fn capabilities(&self) -> Vec<ModelCapability> {
            self.inner.capabilities()
        }

        fn provider_id(&self) -> String {
            self.inner.provider_id()
        }
    }

    #[tokio::test]
    async fn a_note_that_arrives_while_a_step_runs_reaches_the_next_step() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("midflight-session");
        // Three steps, so there is a boundary *after* the one that takes the
        // note. That is what makes this the regression test for injecting it
        // twice: the claim is still only in the turn's un-appended batch, so the
        // store keeps reporting the item as pending until the turn settles.
        let inner = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("done"),
        ]));
        let model = Arc::new(MeanwhileModel::new(
            store.clone(),
            &sid,
            inner.clone(),
            WhileModelThinks::Steer { text: "and the tests", expected_turn_id: None },
        ));
        let scheduler = scheduler_over(store.clone(), model.clone(), 8);
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        // It landed during step 1 and stayed out of it: the step already running
        // was not re-planned around a note that arrived mid-flight.
        let requests = inner.requests();
        assert_eq!(requests.len(), 3, "the turn ran to the model's own answer");
        assert_eq!(shown_text(&requests[0].messages), vec!["hi"]);
        assert_eq!(shown_text(&requests[1].messages), vec!["hi", "hello", "and the tests"]);
        // Once, not once per boundary after it.
        assert_eq!(
            shown_text(&requests[2].messages),
            vec!["hi", "hello", "and the tests", "hello"]
        );
        let item_id = model.landed.lock().unwrap().clone().expect("the note landed");
        let expected = ModelMessage::user_for_steering(&report.turn_id, &item_id, "and the tests");
        assert_eq!(requests[1].messages.last(), Some(&expected));
        assert_eq!(
            requests[2].messages.iter().filter(|m| **m == expected).count(),
            1,
            "the note is injected once per turn, however many boundaries follow"
        );

        // And the log says where it went in: after the step that had ended, not
        // during it.
        let (event, position) = steer_event(&store, &sid).await;
        let types = event_types(&store, &sid).await;
        assert_eq!(types.iter().filter(|t| *t == EventType::TURN_STEERED).count(), 1, "{types:?}");
        assert_eq!(
            types[position - 2..position + 3],
            [
                EventType::STEP_COMPLETED,
                EventType::INBOX_CLAIMED,
                EventType::TURN_STEERED,
                EventType::REASON_RECORDED,
                EventType::STEP_STARTED,
            ],
            "{types:?}"
        );
        let ended = types[..position].iter().filter(|t| *t == EventType::STEP_COMPLETED).count();
        assert_eq!(ended, 1);
        assert!(event.event.payload["after_step_id"].is_string(), "{:?}", event.event.payload);
    }

    #[tokio::test]
    async fn an_interrupt_ends_the_turn_at_the_next_boundary() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("interrupt-session");
        // The model is scripted to keep proposing the same tool call, so nothing
        // it does can end this turn: whatever stops it, it is not the model.
        let inner = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::tool_call(
            "echo",
            serde_json::json!({}),
        )]));
        let model = Arc::new(MeanwhileModel::new(
            store.clone(),
            &sid,
            inner.clone(),
            WhileModelThinks::Interrupt,
        ));
        let scheduler = scheduler_over(store.clone(), model, 8);
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        // The step in flight finished — an interrupt stops the *next* step, it
        // does not tear down the one already running — and then the turn
        // ended, one step in.
        assert_eq!(report.stop_reason, StopReason::UserCancelled);
        assert_eq!(inner.requests().len(), 1, "the step in flight ran; no second step started");
        let types = event_types(&store, &sid).await;
        assert_eq!(
            types.iter().filter(|t| *t == EventType::STEP_COMPLETED).count(),
            1,
            "{types:?}"
        );
        assert!(types.contains(&EventType::STEP_STARTED.to_string()), "{types:?}");
        assert!(!types.contains(&EventType::TURN_COMPLETED.to_string()), "{types:?}");
        assert!(!types.contains(&EventType::RUN_COMPLETED.to_string()), "{types:?}");

        // A cancelled turn names its cause: `turn/cancelled` alone could not tell
        // the user's interrupt from an adapter abandoning an attempt.
        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let cancelled = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::TURN_CANCELLED)
            .expect("the turn was cancelled");
        assert_eq!(cancelled.event.payload["cause"], "user_interrupt");
        assert!(cancelled.event.payload["inbox_item"].is_string(), "{:?}", cancelled.event.payload);

        // The interrupt was consumed, and the run is closed rather than open.
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());
        assert!(!scheduler.has_active_run(&sid).await.unwrap());
    }

    #[tokio::test]
    async fn an_exhausted_step_budget_completes_the_turn() {
        // The budget is the Scheduler's policy, so only the Scheduler can report
        // this ending. The model here would call tools forever: it never stops on
        // its own, which is why something outside the loop has to.
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("budget-session");
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::tool_call(
            "echo",
            serde_json::json!({}),
        )]));
        let scheduler = scheduler_over(store.clone(), model.clone(), 2);
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        assert_eq!(report.stop_reason, StopReason::MaxSteps);
        assert!(report.final_content.is_none());
        assert_eq!(model.requests().len(), 2, "the budget counts steps, not requests");
        let types = event_types(&store, &sid).await;
        assert!(types.contains(&EventType::TURN_COMPLETED.to_string()), "{types:?}");
        assert!(types.contains(&EventType::RUN_COMPLETED.to_string()), "{types:?}");
        assert!(!scheduler.has_active_run(&sid).await.unwrap());
    }

    #[tokio::test]
    async fn a_turn_nobody_steers_gains_no_steering_events() {
        // The invariant the whole change has to preserve: steering is opt-in. A
        // turn with an empty steering queue is the turn this code drove before
        // there was such a queue, down to the order of the log.
        let (scheduler, store, _, sid) = two_step_scheduler();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        assert_eq!(
            event_types(&store, &sid).await,
            [
                EventType::INBOX_APPENDED,
                EventType::INBOX_CLAIMED,
                EventType::RUN_STARTED,
                EventType::TURN_STARTED,
                EventType::MODEL_TOOLS_FROZEN,
                "reason/recorded",
                "reason/recorded",
                "reason/recorded",
                EventType::STEP_STARTED,
                EventType::MODEL_REQUEST_PREPARED,
                EventType::MODEL_ATTEMPT_STARTED,
                EventType::MODEL_RESPONSE_COMPLETED,
                "reason/recorded",
                EventType::TOOL_STARTED,
                EventType::TOOL_COMPLETED,
                "reason/recorded",
                EventType::STEP_COMPLETED,
                EventType::STEP_STARTED,
                EventType::MODEL_REQUEST_PREPARED,
                EventType::MODEL_ATTEMPT_STARTED,
                EventType::MODEL_RESPONSE_COMPLETED,
                "reason/recorded",
                EventType::STEP_COMPLETED,
                EventType::TURN_COMPLETED,
                EventType::RUN_COMPLETED,
            ]
        );
    }

    /// A control handler that records what it was asked and answers as told.
    struct RecordingControl {
        /// What this handler is called in the record. Varies because the
        /// scheduler refuses two handlers answering to one name, and a test has
        /// to be able to say which of two colliding handlers is which.
        name: String,
        names: Vec<String>,
        seen: Mutex<Vec<String>>,
        applied: AtomicUsize,
        /// When set, every request comes back `Refused` with this reason.
        refuse: Option<String>,
        /// When set, every request comes back as a handler failure.
        fail: Option<String>,
    }

    impl RecordingControl {
        fn applying(names: &[&str]) -> Self {
            Self {
                name: "recording".to_string(),
                names: names.iter().map(|n| n.to_string()).collect(),
                seen: Mutex::new(Vec::new()),
                applied: AtomicUsize::new(0),
                refuse: None,
                fail: None,
            }
        }

        fn failing(names: &[&str], message: &str) -> Self {
            Self { fail: Some(message.to_string()), ..Self::applying(names) }
        }

        fn named(name: &str, names: &[&str]) -> Self {
            Self { name: name.to_string(), ..Self::applying(names) }
        }

        fn refusing(names: &[&str], reason: &str) -> Self {
            Self { refuse: Some(reason.to_string()), ..Self::applying(names) }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn applied(&self) -> usize {
            self.applied.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ControlHandler for RecordingControl {
        fn name(&self) -> &str {
            &self.name
        }

        fn handles(&self) -> Vec<String> {
            self.names.clone()
        }

        async fn apply(&self, request: ControlRequest) -> Result<ControlOutcome, ControlError> {
            self.seen.lock().unwrap().push(request.control.clone());
            match (&self.refuse, &self.fail) {
                (_, Some(message)) => {
                    Err(ControlError::Failed { control: request.control, message: message.clone() })
                }
                (Some(reason), None) => Ok(ControlOutcome::Refused { reason: reason.clone() }),
                (None, None) => {
                    self.applied.fetch_add(1, Ordering::SeqCst);
                    Ok(ControlOutcome::Applied { detail: format!("{} applied", request.control) })
                }
            }
        }
    }

    /// Where `item_id`'s claim sits in the log, and the whole stream it sits in.
    async fn claim_of(
        store: &FakeEventStore,
        sid: &SessionId,
        item_id: &InboxItemId,
    ) -> (Vec<StoredEvent>, usize) {
        let events = store.read_stream(sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let position = events
            .iter()
            .position(|e| {
                e.event.event_type.as_str() == EventType::INBOX_CLAIMED
                    && e.event.payload["item_id"] == serde_json::json!(item_id)
            })
            .expect("the item was claimed");
        (events, position)
    }

    #[tokio::test]
    async fn a_control_that_arrives_mid_turn_waits_for_the_boundary_after_it() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("control-session");
        let inner = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("done"),
        ]));
        // The control lands during the model call of step 1 — inside the turn,
        // from outside the process. That is the only place a second writer can
        // get a word in, and therefore the only place this can be tested.
        let model = Arc::new(MeanwhileModel::new(
            store.clone(),
            &sid,
            inner.clone(),
            WhileModelThinks::Control { control: "test.retire" },
        ));
        let handler = Arc::new(RecordingControl::applying(&["test.retire"]));
        let scheduler = scheduler_over(store.clone(), model.clone(), 8)
            .with_control_handlers(vec![handler.clone()])
            .expect("one handler claims one name");
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        // The turn ran to its own answer and never looked at the control: a
        // change to the runtime is applied at a boundary, and the boundary is
        // not inside a turn.
        assert_eq!(inner.requests().len(), 2, "the turn ran to the model's own answer");
        assert_eq!(handler.applied(), 0, "a control was applied inside a turn");
        assert!(handler.seen().is_empty(), "a handler was even asked inside a turn");

        // Which is to say it is still there to be found, not taken and dropped.
        let pending = scheduler.inbox.pending(&sid).await.unwrap();
        assert_eq!(pending.len(), 1, "the control left the queue without being applied");
        assert_eq!(pending[0].kind, InboxKind::Control);
        let control_item = pending[0].item_id.clone();
        assert!(
            !event_types(&store, &sid).await.contains(&EventType::INBOX_DISCARDED.to_string()),
            "nothing was discarded"
        );

        // The next boundary — which is the start of the next run, before the
        // turn it precedes opens — takes it.
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "again"}), 0, Actor::User)
            .await
            .unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a second run");

        assert_eq!(handler.applied(), 1, "applied once, at the boundary");
        assert_eq!(handler.seen(), vec!["test.retire".to_string()]);
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty(), "taken out of the queue");

        // And the record says when it happened: after the turn that was in
        // flight, before the turn it preceded. A reader can tell a change made
        // *between* two turns from one made during either.
        let (events, claim) = claim_of(&store, &sid, &control_item).await;
        assert!(
            events[claim].event.payload["run_id"].is_null(),
            "no run took it: a control belongs to the session, not to a turn"
        );
        let second_turn = events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.event.event_type.as_str() == EventType::TURN_STARTED)
            .map(|(i, _)| i)
            .nth(1)
            .expect("a second turn");
        assert!(claim < second_turn, "the control was applied after the turn it preceded");
        let turn_one_ended = events
            .iter()
            .position(|e| e.event.event_type.as_str() == EventType::TURN_COMPLETED)
            .expect("the first turn ended");
        assert!(turn_one_ended < claim, "the control was applied inside the turn it arrived in");
    }

    #[tokio::test]
    async fn a_control_is_applied_even_when_no_turn_is_waiting() {
        // A change belongs to the session, not to a turn: an operator who turns
        // a plugin off while nothing is running should not have to start a turn
        // to make it so. `None` still means "no turn to run".
        let (scheduler, store, _model, sid) = two_step_scheduler();
        let handler = Arc::new(RecordingControl::applying(&["test.retire"]));
        let scheduler = scheduler
            .with_control_handlers(vec![handler.clone()])
            .expect("one handler claims one name");
        let item =
            scheduler.inbox.control(&sid, "test.retire", serde_json::json!({})).await.unwrap();

        assert!(
            scheduler.run_pending(&sid, Utc::now()).await.unwrap().is_none(),
            "there was no turn to run"
        );

        assert_eq!(handler.applied(), 1);
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());
        let (events, claim) = claim_of(&store, &sid, &item.item_id).await;
        assert!(
            !events.iter().any(|e| e.event.event_type.as_str() == EventType::TURN_STARTED),
            "applying a control is not a reason to open a turn"
        );
        assert_eq!(claim, events.len() - 1, "and nothing followed it");
    }

    #[tokio::test]
    async fn a_control_nobody_handles_is_refused_by_name() {
        // Fail closed, and say so. Leaving it pending would be a command that
        // looks accepted and does nothing; dropping it silently would be worse.
        let (scheduler, store, _model, sid) = two_step_scheduler();
        let handler = Arc::new(RecordingControl::applying(&["some.other.control"]));
        let scheduler = scheduler
            .with_control_handlers(vec![handler.clone()])
            .expect("one handler claims one name");
        scheduler.inbox.control(&sid, "test.retire", serde_json::json!({})).await.unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap();

        assert!(handler.seen().is_empty(), "a handler was asked about a name it does not claim");
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty(), "it is not left pending");

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let refused = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .expect("the refusal is recorded");
        let reason = refused.event.payload["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("test.retire"), "the refusal names it: {reason}");
        assert!(
            reason.contains("some.other.control"),
            "and says what is handled instead: {reason}"
        );
    }

    #[tokio::test]
    async fn a_control_a_handler_refuses_leaves_the_queue_with_its_reason() {
        let (scheduler, store, _model, sid) = two_step_scheduler();
        let handler =
            Arc::new(RecordingControl::refusing(&["test.retire"], "no such plugin 'one'"));
        let scheduler = scheduler
            .with_control_handlers(vec![handler.clone()])
            .expect("one handler claims one name");
        let item = scheduler
            .inbox
            .control(&sid, "test.retire", serde_json::json!({"plugin_id": "one"}))
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap();

        // Asked once, and its answer was taken as the end of the matter: a
        // refusal that stayed pending would be indistinguishable from a
        // boundary that never came.
        assert_eq!(handler.seen(), vec!["test.retire".to_string()]);
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let discarded = events
            .iter()
            .find(|e| {
                e.event.event_type.as_str() == EventType::INBOX_DISCARDED
                    && e.event.payload["item_id"] == serde_json::json!(&item.item_id)
            })
            .expect("the refusal is recorded against the item that asked");
        let reason = discarded.event.payload["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("no such plugin 'one'"),
            "the handler's own words survive: {reason}"
        );
        assert!(reason.contains("recording"), "and which handler said them: {reason}");
    }

    /// Two handlers claiming one control are refused when the scheduler is
    /// built — not when a control arrives, and not silently.
    ///
    /// Routing asks the handlers in the order they were passed and takes the
    /// first claimant, so without this the answer to "who applies
    /// `test.retire`" would turn on nothing but assembly order, and the second
    /// handler would be one nothing could ever reach. The tool surface refuses
    /// the same shape in `CompositeToolProvider` for the same reason. (YA-46)
    #[tokio::test]
    async fn two_handlers_claiming_one_control_are_refused_at_assembly() {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("unused")]));
        let first = Arc::new(RecordingControl::named("alpha", &["test.retire"]));
        let second = Arc::new(RecordingControl::named("beta", &["test.retire"]));

        let Err(err) = scheduler_over(store, model, 8).with_control_handlers(vec![first, second])
        else {
            panic!("two handlers claim one name");
        };

        match err {
            SchedulerError::DuplicateControl { control, first, second } => {
                assert_eq!(control, "test.retire");
                assert_eq!(first, "alpha");
                assert_eq!(second, "beta");
            }
            other => panic!("expected a duplicate-control refusal, got {other}"),
        }
    }

    /// Two handlers answering to one name are refused as well.
    ///
    /// Every refusal is written down as "`name` refused it", so one name for two
    /// implementations is a record that cannot say who answered. The controls
    /// here are disjoint on purpose: this refusal is about the name alone.
    #[tokio::test]
    async fn two_handlers_answering_to_one_name_are_refused() {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("unused")]));
        let first = Arc::new(RecordingControl::named("recording", &["test.retire"]));
        let second = Arc::new(RecordingControl::named("recording", &["test.other"]));

        let Err(err) = scheduler_over(store, model, 8).with_control_handlers(vec![first, second])
        else {
            panic!("two handlers answer to one name");
        };

        match err {
            SchedulerError::DuplicateHandlerName { name } => assert_eq!(name, "recording"),
            other => panic!("expected a duplicate handler name, got {other}"),
        }
    }

    /// Where the check stops: one handler naming a control twice.
    ///
    /// A name repeated inside one handler's own list still leaves one
    /// implementation behind it, so nothing about routing is ambiguous and
    /// nothing is refused — and the control still works, which is the half a
    /// refusal test cannot show.
    #[tokio::test]
    async fn one_handler_naming_a_control_twice_is_not_a_collision() {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("unused")]));
        let handler = Arc::new(RecordingControl::applying(&["test.retire", "test.retire"]));
        let scheduler = scheduler_over(store, model, 8)
            .with_control_handlers(vec![handler.clone()])
            .expect("one handler is one implementation, however often it says so");

        let sid = SessionId::from_string("control-session");
        scheduler.inbox.control(&sid, "test.retire", serde_json::json!({})).await.unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap();

        assert_eq!(handler.applied(), 1);
    }

    /// Several handlers with their own names and their own controls assemble:
    /// the check is about collisions, not about there being more than one.
    #[tokio::test]
    async fn handlers_with_their_own_names_and_controls_assemble() {
        let store = Arc::new(FakeEventStore::new().requiring_lease());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("unused")]));
        let plugins = Arc::new(RecordingControl::named("plugins", &["plugin.disable"]));
        let skills = Arc::new(RecordingControl::named("skills", &["skill.reload"]));
        let scheduler = scheduler_over(store.clone(), model, 8)
            .with_control_handlers(vec![plugins, skills])
            .expect("two handlers, four distinct names");

        // The surface is the pair, not whichever one answers first: a name
        // neither claims is refused with both of their names in the reason.
        let sid = SessionId::from_string("control-session");
        scheduler.inbox.control(&sid, "nobody.claims.this", serde_json::json!({})).await.unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap();

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let reason = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .map(|e| e.event.payload["reason"].as_str().unwrap_or_default().to_string())
            .expect("the unclaimed control is discarded with a reason");
        assert!(reason.contains("plugin.disable"), "{reason}");
        assert!(reason.contains("skill.reload"), "{reason}");
    }

    /// The three ways a control fails to take effect are three facts, and the
    /// log says which one it was instead of leaving the reader to parse prose.
    ///
    /// `discarded` is one status for all of them; `cause` is what keeps "nobody
    /// claimed it", "a handler declined", and "a handler broke" distinguishable
    /// to anything but a human reading `reason` (C-1).
    #[tokio::test]
    async fn the_three_ways_a_control_fails_are_told_apart_in_the_log() {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("unused")]));
        let refuser = Arc::new(RecordingControl {
            name: "refuser".to_string(),
            ..RecordingControl::refusing(&["test.refuse"], "not today")
        });
        let breaker = Arc::new(RecordingControl {
            name: "breaker".to_string(),
            ..RecordingControl::failing(&["test.fail"], "the disk said no")
        });
        let scheduler = scheduler_over(store.clone(), model, 8)
            .with_control_handlers(vec![refuser, breaker])
            .expect("two handlers with distinct names and controls");

        let sid = SessionId::from_string("control-session");
        scheduler.inbox.control(&sid, "test.refuse", serde_json::json!({})).await.unwrap();
        scheduler.inbox.control(&sid, "test.fail", serde_json::json!({})).await.unwrap();
        scheduler.inbox.control(&sid, "test.unhandled", serde_json::json!({})).await.unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap();

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let discarded: Vec<(&str, &str)> = events
            .iter()
            .filter(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .map(|e| {
                (
                    e.event.payload["cause"].as_str().expect("every discard has a cause"),
                    e.event.payload["reason"].as_str().unwrap_or_default(),
                )
            })
            .collect();

        assert_eq!(discarded.len(), 3, "{discarded:?}");
        let cause_of = |control: &str| {
            discarded.iter().find(|(_, reason)| reason.contains(control)).map(|(cause, _)| *cause)
        };
        assert_eq!(cause_of("not today"), Some("refused"), "{discarded:?}");
        assert_eq!(cause_of("the disk said no"), Some("failed"), "{discarded:?}");
        assert_eq!(cause_of("no handler claims"), Some("unhandled"), "{discarded:?}");
    }

    /// A payload the scheduler cannot write down is an error, not a panic.
    ///
    /// The scheduler records the configuration a run executes under as
    /// `profile/resolved`, and a payload it cannot serialize is a run it cannot
    /// describe. `serde_json::to_value` on a `ResolvedProfileSnapshot` cannot
    /// actually fail — every field is a string or a number — so this test pins
    /// the *plumbing* the call site now uses (YA-30): the error travels through
    /// `SchedulerError::Payload` via `?` instead of a panic. The call site is
    /// unreachable with that type, and this says so rather than pretending a
    /// profile that fails to serialize can be built.
    // -----------------------------------------------------------------------
    // Scheduled turns
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_due_drives_a_scheduled_turn_and_claims_it_once() {
        let store = Arc::new(FakeEventStore::new().requiring_lease());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("woken")]));
        let scheduler = scheduler_over(store.clone(), model.clone(), 8);
        let sid = SessionId::from_string("schedule-session");
        let due = Utc::now() - chrono::Duration::seconds(1);
        scheduler.inbox.schedule(&sid, due, "wake up", 0, Actor::User).await.unwrap();

        let report = scheduler.run_due(&sid, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(report.stop_reason, StopReason::FinalAnswer);
        assert_eq!(report.final_content.as_deref(), Some("woken"));
        assert_eq!(shown_text(&model.requests()[0].messages), vec!["wake up"]);

        let types = event_types(&store, &sid).await;
        assert_eq!(
            types.iter().filter(|t| t.as_str() == EventType::INBOX_CLAIMED).count(),
            1,
            "the item is claimed once, in the terminal batch: {types:?}"
        );
        assert!(types.contains(&EventType::TURN_COMPLETED.to_string()), "{types:?}");
        assert!(scheduler.inbox.scheduled(&sid).await.unwrap().is_empty());
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_due_with_nothing_due_returns_none() {
        let (scheduler, _, sid) = scheduler();
        assert!(scheduler.run_due(&sid, Utc::now()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn run_due_leaves_a_future_item_pending() {
        let (scheduler, store, sid) = scheduler();
        scheduler
            .inbox
            .schedule(&sid, Utc::now() + chrono::Duration::hours(1), "later", 0, Actor::User)
            .await
            .unwrap();

        let now = Utc::now();
        assert!(scheduler.run_due(&sid, now).await.unwrap().is_none());
        assert_eq!(scheduler.inbox.scheduled(&sid).await.unwrap().len(), 1);
        assert_eq!(event_types(&store, &sid).await, vec![EventType::INBOX_APPENDED]);
    }

    #[tokio::test]
    async fn run_due_does_not_start_work_while_a_run_is_open() {
        let (scheduler, store, sid) = scheduler();
        scheduler
            .inbox
            .schedule(&sid, Utc::now() - chrono::Duration::seconds(1), "wake", 0, Actor::User)
            .await
            .unwrap();

        let run_id = RunId::new();
        let next = scheduler.next_seq(&sid).await.unwrap();
        store
            .append(
                &sid,
                next,
                vec![draft(
                    &sid,
                    EventType::RUN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run_id),
                    None,
                    None,
                )],
            )
            .await
            .unwrap();

        assert!(scheduler.run_due(&sid, Utc::now()).await.unwrap().is_none());
        assert_eq!(scheduler.inbox.scheduled(&sid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_due_refuses_a_held_session_and_leaves_the_item_pending() {
        let store = Arc::new(FakeEventStore::new());
        let scheduler = Scheduler::new(store, Arc::new(YieldLoop), 8);
        let sid = SessionId::from_string("held-schedule-session");
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "?"}), 0, Actor::User)
            .await
            .unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a turn that yields");
        scheduler
            .inbox
            .schedule(&sid, Utc::now() - chrono::Duration::seconds(1), "wake", 0, Actor::User)
            .await
            .unwrap();

        let error = scheduler.run_due(&sid, Utc::now()).await.unwrap_err();
        assert!(matches!(error, SchedulerError::AwaitingDecision { .. }), "{error}");
        assert_eq!(scheduler.inbox.scheduled(&sid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_due_discards_an_unreadable_item_and_runs_the_next_one() {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let scheduler = scheduler_over(store.clone(), model, 8);
        let sid = SessionId::from_string("bad-schedule-session");
        let bad = scheduler
            .inbox
            .append(
                &sid,
                InboxKind::Scheduled,
                serde_json::json!({ "due_at": "yesterday", "text": "unreadable" }),
                0,
                Actor::User,
            )
            .await
            .unwrap();
        scheduler
            .inbox
            .schedule(&sid, Utc::now() - chrono::Duration::seconds(1), "readable", 0, Actor::User)
            .await
            .unwrap();

        let report = scheduler.run_due(&sid, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(report.stop_reason, StopReason::FinalAnswer);

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let discarded = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .expect("the bad item is discarded, not left to block the queue");
        assert_eq!(discarded.event.payload["item_id"], serde_json::json!(bad.item_id));
        assert_eq!(discarded.event.payload["cause"], "unreadable");
        assert!(
            discarded.event.payload["reason"].as_str().unwrap_or_default().contains("due_at"),
            "{:?}",
            discarded.event.payload
        );
        assert!(scheduler.inbox.pending(&sid).await.unwrap().is_empty());
    }

    /// Instructions a run gets, fixed by the test.
    struct FixedInstructions(RunInstructions);

    impl InstructionSource for FixedInstructions {
        fn for_run(&self) -> Result<RunInstructions, InstructionError> {
            Ok(self.0.clone())
        }
    }

    /// Instructions that cannot be assembled, as a broken workspace would have.
    struct BrokenInstructions;

    impl InstructionSource for BrokenInstructions {
        fn for_run(&self) -> Result<RunInstructions, InstructionError> {
            Err(InstructionError::NotUtf8 { path: "AGENTS.md".to_string() })
        }
    }

    fn instructions() -> RunInstructions {
        RunInstructions {
            text: "follow these".to_string(),
            project_file: Some("AGENTS.md".to_string()),
            builtin: "test/1".to_string(),
        }
    }

    #[tokio::test]
    async fn the_run_records_the_instructions_it_executed_under() {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("hello")]));
        let scheduler = scheduler_over(store.clone(), model.clone(), 8)
            .with_instructions(Arc::new(FixedInstructions(instructions())));
        let sid = SessionId::from_string("instructions-session");
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let started = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::RUN_STARTED)
            .expect("a run/started");
        assert_eq!(started.event.payload["instructions"]["text"], "follow these");
        assert_eq!(started.event.payload["instructions"]["builtin"], "test/1");
        // And what the model was actually sent is the same prompt, leading the
        // request, so the log and the request agree about what it was told.
        let sent = &model.requests()[0].messages[0];
        assert_eq!(sent.role, agent_protocol::model::MessageRole::System);
        assert_eq!(sent.content, vec![ContentBlock::text("follow these")]);
    }

    #[tokio::test]
    async fn instructions_that_cannot_be_assembled_mean_the_run_never_started() {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("unused")]));
        let scheduler =
            scheduler_over(store.clone(), model, 8).with_instructions(Arc::new(BrokenInstructions));
        let sid = SessionId::from_string("broken-instructions-session");
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        let error =
            scheduler.run_pending(&sid, Utc::now()).await.expect_err("the run must not start");
        assert!(matches!(error, SchedulerError::Instructions(_)), "{error}");

        // Nothing was written and nothing was consumed: the next attempt (or the
        // next process, after the file is fixed) finds the item still there.
        assert_eq!(event_types(&store, &sid).await, vec!["inbox/appended"]);
        assert_eq!(scheduler.inbox.pending(&sid).await.unwrap().len(), 1);
    }

    /// A loop that records the context each turn was opened with and answers
    /// at once. Enough to see what the Scheduler tells the loop about the
    /// world around a turn.
    struct RecordingLoop {
        seen: Mutex<Vec<TurnContext>>,
    }

    impl RecordingLoop {
        fn new() -> Arc<Self> {
            Arc::new(Self { seen: Mutex::new(Vec::new()) })
        }

        fn contexts(&self) -> Vec<TurnContext> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentLoop for RecordingLoop {
        async fn begin_turn(&self, ctx: &TurnContext) -> Result<TurnStart, AgentLoopError> {
            self.seen.lock().unwrap().push(ctx.clone());
            Ok(TurnStart { state: TurnState::new(()), events: vec![] })
        }

        async fn step(
            &self,
            _turn: &mut TurnState,
            _steering: &[SteeringInput],
        ) -> Result<StepOutcome, AgentLoopError> {
            Ok(StepOutcome {
                step_id: StepId::new(),
                events: vec![],
                stop: Some(StopReason::FinalAnswer),
                final_content: Some(String::from("done")),
            })
        }
    }

    #[tokio::test]
    async fn a_turn_is_opened_with_the_state_behind_it_and_the_previous_turn_s_ending() {
        let store = Arc::new(FakeEventStore::new());
        let loop_ = RecordingLoop::new();
        let scheduler = Scheduler::new(store.clone(), loop_.clone(), 8);
        let sid = SessionId::from_string("state-session");

        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "first"}), 0, Actor::User)
            .await
            .unwrap();
        scheduler
            .inbox
            .append(
                &sid,
                InboxKind::NextTurn,
                serde_json::json!({"text": "second"}),
                0,
                Actor::User,
            )
            .await
            .unwrap();
        scheduler
            .inbox
            .schedule(&sid, Utc::now() + chrono::Duration::hours(1), "later", 0, Actor::User)
            .await
            .unwrap();
        scheduler.inbox.steer(&sid, "a note", None).await.unwrap();

        let now = Utc::now();
        scheduler.run_pending(&sid, now).await.unwrap().expect("a run");
        let first = loop_.contexts().remove(0).state.expect("the scheduler supplies facts");

        // Two turn-starting inputs wait behind this turn: the second request
        // and the future schedule. Steering is not one of them — it enters
        // this turn as a user message.
        assert_eq!(first.queued_inputs, 2);
        assert_eq!(first.input_kind, TurnInputKind::User);
        assert_eq!(first.previous_turn, None, "no turn has ended yet");
        assert_eq!(first.max_steps, 8);
        assert_eq!(first.started_at, now, "the turn reports the instant it was handed");

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        let second = loop_.contexts().remove(1).state.expect("the scheduler supplies facts");
        assert_eq!(second.queued_inputs, 1, "only the schedule is still queued");
        assert_eq!(
            second.previous_turn,
            Some(agent_ports::agent_loop::TurnEnding::Completed),
            "the turn just finished is the previous one"
        );
    }

    /// An approval holds a call, the turn yields, the resumed call fails in a
    /// turn of its own, and the turn that answers is a new one. The failure is
    /// read back from the log for exactly the items `resume` marks.
    #[tokio::test]
    async fn a_resumed_turn_opens_on_the_failure_the_turn_before_it_left() {
        let store = Arc::new(FakeEventStore::new());
        let loop_ = RecordingLoop::new();
        let scheduler = Scheduler::new(store.clone(), loop_.clone(), 8);
        let sid = SessionId::from_string("resumed-session");
        let failed = draft(
            &sid,
            EventType::TOOL_FAILED,
            serde_json::json!({
                "tool_name": "shell.run",
                "provider_tool_call_id": "call-1",
                "status": "failed",
                "content": "exit code 1: no such file\nsecond line",
            }),
            Actor::Tool,
            None,
            None,
            None,
        );
        store.append(&sid, EventSeq::ZERO, vec![failed]).await.unwrap();

        scheduler
            .inbox
            .append(
                &sid,
                InboxKind::NextTurn,
                serde_json::json!({"text": "continue", "resumed": true}),
                0,
                Actor::User,
            )
            .await
            .unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        let ctx = loop_.contexts().remove(0);
        assert_eq!(
            ctx.pending_failure,
            Some(TurnFailureFacts {
                call_id: "call-1".to_string(),
                tool: "shell.run".to_string(),
                detail: "exit code 1: no such file".to_string(),
            })
        );
    }

    /// An ordinary turn opens clean: a failure in the log belongs to the turns
    /// that saw it, not to the next request a user sends.
    #[tokio::test]
    async fn an_ordinary_turn_does_not_inherit_a_past_failure() {
        let store = Arc::new(FakeEventStore::new());
        let loop_ = RecordingLoop::new();
        let scheduler = Scheduler::new(store.clone(), loop_.clone(), 8);
        let sid = SessionId::from_string("ordinary-session");
        let failed = draft(
            &sid,
            EventType::TOOL_FAILED,
            serde_json::json!({
                "tool_name": "shell.run",
                "provider_tool_call_id": "call-1",
                "status": "failed",
                "content": "exit code 1: no such file",
            }),
            Actor::Tool,
            None,
            None,
            None,
        );
        store.append(&sid, EventSeq::ZERO, vec![failed]).await.unwrap();

        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();
        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");

        assert_eq!(loop_.contexts().remove(0).pending_failure, None);
    }

    #[tokio::test]
    async fn a_scheduled_turn_says_how_it_was_opened() {
        let store = Arc::new(FakeEventStore::new());
        let loop_ = RecordingLoop::new();
        let scheduler = Scheduler::new(store.clone(), loop_.clone(), 8);
        let sid = SessionId::from_string("scheduled-session");
        scheduler
            .inbox
            .schedule(&sid, Utc::now() - chrono::Duration::seconds(1), "wake", 0, Actor::User)
            .await
            .unwrap();

        scheduler.run_due(&sid, Utc::now()).await.unwrap().expect("a run");
        let state = loop_.contexts().remove(0).state.expect("the scheduler supplies facts");

        assert_eq!(state.input_kind, TurnInputKind::Scheduled);
        assert_eq!(state.queued_inputs, 0, "the running item is not queued behind itself");
    }

    /// The boundary the host assembled reaches the loop unchanged, and a
    /// scheduler that was handed none sends none rather than inventing one.
    #[tokio::test]
    async fn a_turn_is_opened_with_the_capability_facts_the_host_assembled() {
        let loop_ = RecordingLoop::new();
        let facts =
            TurnCapabilityFacts { sandbox: SandboxMode::WorkspaceSandbox, network_allowed: false };
        let scheduler = Scheduler::new(Arc::new(FakeEventStore::new()), loop_.clone(), 8)
            .with_capability(facts.clone());
        let sid = SessionId::from_string("capability-session");
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(loop_.contexts()[0].capability, Some(facts));

        let bare = Scheduler::new(Arc::new(FakeEventStore::new()), loop_.clone(), 8);
        let other = SessionId::from_string("bare-session");
        bare.inbox
            .append(&other, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();
        bare.run_pending(&other, Utc::now()).await.unwrap().expect("a run");
        assert_eq!(loop_.contexts()[1].capability, None);
    }

    /// The plan the session's log already holds reaches the next turn. A
    /// ledger with nothing recorded is `Some(empty)`, not `None`: the
    /// Scheduler knows the ledger exists, and saying "none recorded yet" is
    /// different from a harness that has no ledger at all.
    #[tokio::test]
    async fn a_turn_is_opened_with_the_plan_the_log_already_holds() {
        let store = Arc::new(FakeEventStore::new());
        let loop_ = RecordingLoop::new();
        let scheduler = Scheduler::new(store.clone(), loop_.clone(), 8);
        let sid = SessionId::from_string("plan-session");
        store
            .append(
                &sid,
                EventSeq::ZERO,
                vec![draft(
                    &sid,
                    EventType::PLAN_UPDATED,
                    serde_json::json!({
                        "provider_tool_call_id": "call_1",
                        "plan": {
                            "goal": "carry on",
                            "items": [{ "text": "next", "status": "doing" }],
                        },
                    }),
                    Actor::Agent,
                    None,
                    None,
                    None,
                )],
            )
            .await
            .unwrap();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
        let plan = loop_.contexts().remove(0).plan.expect("the scheduler keeps a ledger");
        assert_eq!(plan.goal, "carry on");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].status, agent_protocol::plan::PlanItemStatus::Doing);

        let unplanned = SessionId::from_string("unplanned-session");
        scheduler
            .inbox
            .append(
                &unplanned,
                InboxKind::NextTurn,
                serde_json::json!({"text": "hi"}),
                0,
                Actor::User,
            )
            .await
            .unwrap();
        scheduler.run_pending(&unplanned, Utc::now()).await.unwrap().expect("a run");
        // `contexts()` hands out a copy of what was seen, so both turns are
        // still there even though the first was removed from the local value.
        assert_eq!(
            loop_.contexts()[1].plan,
            Some(agent_protocol::plan::Plan::default()),
            "a known ledger with nothing recorded is Some(empty), not None"
        );
    }

    fn stored(seq: u64, event_type: &str, turn_id: &TurnId) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: draft(
                &SessionId::from_string("s"),
                event_type,
                serde_json::json!({}),
                Actor::Scheduler,
                None,
                Some(turn_id),
                None,
            ),
        }
    }

    #[test]
    fn the_last_turn_ending_is_the_last_terminal_event() {
        use agent_ports::agent_loop::TurnEnding;

        let t1 = TurnId::from_string("t1");
        let t2 = TurnId::from_string("t2");
        let t3 = TurnId::from_string("t3");
        let mut events = vec![
            stored(0, EventType::TURN_STARTED, &t1),
            stored(1, EventType::TURN_FAILED, &t1),
            stored(2, EventType::TURN_STARTED, &t2),
            stored(3, EventType::TURN_CANCELLED, &t2),
        ];
        assert_eq!(lifecycle::last_turn_ending(&events), Some((t2.clone(), TurnEnding::Cancelled)));

        // A turn that started and never ended is not reported as the previous
        // turn: recovery will close it, and until then it has no ending.
        events.push(stored(4, EventType::TURN_STARTED, &t3));
        assert_eq!(lifecycle::last_turn_ending(&events), Some((t2, TurnEnding::Cancelled)));
    }

    #[test]
    fn a_payload_that_cannot_be_written_down_is_an_error_not_a_panic() {
        use serde::ser::Error as _;

        struct Unserializable;
        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(S::Error::custom("no text form"))
            }
        }

        let error = SchedulerError::from(serde_json::to_value(Unserializable).unwrap_err());
        assert!(matches!(error, SchedulerError::Payload(_)));
        assert!(error.to_string().starts_with("payload error:"), "{error}");
        assert!(error.to_string().contains("no text form"), "{error}");
    }
}
