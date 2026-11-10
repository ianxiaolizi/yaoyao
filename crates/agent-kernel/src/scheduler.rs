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
    AgentLoop, AgentLoopError, EventSink, Journal, SteeringInput, TurnCapabilityFacts, TurnContext,
    TurnFailureFacts, TurnInputKind, TurnStart, TurnStateFacts,
};
use agent_ports::control::ControlHandler;
use agent_ports::event_store::{DriverRecord, EventStore, EventStoreError, SessionLease};
use agent_ports::instructions::{InstructionError, InstructionSource};
use agent_ports::intent_adjudication::IntentAdjudicator;
use agent_ports::security_reviewer::SecurityReviewer;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventSeq, InboxItemId, RunId, SessionId, StepId, TurnId};
use agent_protocol::inbox::{InboxItem, InboxKind};
use agent_protocol::profile::{ResolvedProfileSnapshot, RunBudget, Spent};
use chrono::{DateTime, Utc};

use crate::approval::{ApprovalError, ApprovalService};
use crate::inbox::{DiscardCause, Due, Inbox, InboxError};
use crate::judgment;
use crate::lifecycle;
use crate::plan;
use crate::session_lifecycle;
use crate::state;

mod accessors;
mod completion;
mod controls;
mod draft;
mod endings;
mod journal;
mod rebuilt;
mod report;
mod security;
mod seq;
mod snapshot;
mod subject;

pub(crate) use draft::draft;
use endings::{Ending, Gate};
use journal::{SessionJournal, close_failed_turn};
use report::Finished;
pub use report::RunReport;

/// Drives runs: claims input, enforces a single active run, and settles turns.
pub struct Scheduler {
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) inbox: Inbox,
    loop_: Arc<dyn AgentLoop>,
    max_steps: Option<u32>,
    /// The rest of the run's budget. `max_steps` above is the optional binding
    /// step limit; this carries the limits the loop enforces inside a step and
    /// the ones recorded in a budget stop.
    budget: RunBudget,
    /// The profile's completion criterion, when it named one. The loop runs it
    /// at step boundaries; the scheduler only carries it from the resolved
    /// profile into every turn it opens.
    completion: Option<agent_protocol::profile::CompletionSpec>,
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
    pub(crate) adjudicator: Option<Arc<dyn IntentAdjudicator>>,
    /// Who reviews the L2/L3 actions of a session before its answer is handed
    /// over (B9). `None` is not consent: a high-risk task then cannot complete.
    security: Option<Arc<dyn SecurityReviewer>>,
}

/// Errors from Scheduler operations.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("inbox error: {0}")]
    Inbox(#[from] InboxError),
    #[error("intent error: {0}")]
    Intent(#[from] crate::intent::IntentError),
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
    /// Another caller holds the session's driver claim: it is running a turn
    /// there. Not a store failure — nothing was read or written — and not
    /// `Gate::Busy`, which means an open run in the log: this one is about a
    /// driver that has not written anything yet.
    #[error("session {session_id} is already being driven ({})", holder.as_ref().map_or("holder did not record itself".to_string(), DriverRecord::to_string))]
    SessionBusy { session_id: SessionId, holder: Option<DriverRecord> },
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
        // The profile's budget is what the run executes under, including its
        // optional step ceiling. Keeping a separately supplied limit here would
        // let the recorded profile and the scheduler enforce different numbers.
        self.budget = snapshot.budget.clone();
        self.max_steps = self.budget.max_steps;
        self.completion = snapshot.completion.clone();
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
        // Before the gate and before anything is read. The gate answers "is a
        // run open *in the log*", and two callers that have not written yet
        // both answer "no" — that is the race the driver claim exists for, and
        // it has to be closed before the reads that race, not after.
        let _driver = self.drive(session_id).await?;
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
        // Once per boundary: an `unclear` message's relation to the queued work.
        self.adjudicate_pending(session_id).await?;

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
        // Same claim as `run_pending`, for the same reason: a scheduled item is
        // just as easy for two drivers to find due at once.
        let _driver = self.drive(session_id).await?;
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

    /// Claim the right to drive this session until the returned guard drops.
    ///
    /// The two run entry points hold it for the whole call — the reads, the
    /// model and tool work, and the terminal batch — because every one of those
    /// steps is something a second driver must not be doing at the same time.
    /// A refusal is a [`SessionBusy`](SchedulerError::SessionBusy) and not an
    /// `Ok(None)`: "another caller is running this session" is a fact the
    /// caller has to be told, while `Ok(None)` already means "nothing to do".
    async fn drive(&self, session_id: &SessionId) -> Result<Box<dyn SessionLease>, SchedulerError> {
        self.store.driver_lease(session_id).await.map_err(|error| match error {
            EventStoreError::ConcurrentDriver { session_id, holder } => {
                SchedulerError::SessionBusy { session_id, holder }
            }
            other => SchedulerError::Store(other),
        })
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
        // The run's wall clock, measured on the monotonic clock: it times a
        // limit, so it is not a timestamp and does not belong in the log. The
        // loop enforces the deadline inside a step; this is the number written
        // beside a budget stop.
        let run_started = std::time::Instant::now();
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
        // A session created before this build wrote `session/started` on its
        // first use still has to get one, or the lifecycle fold says a session
        // that has been running turns never began. It is written at the opening
        // of its next turn, which is the first moment this session is used
        // again; a session that already says it started is left alone.
        let unstarted = !session_lifecycle::has_started(&history);
        let previous_turn = lifecycle::last_turn_ending(&history).map(|(_turn_id, ending)| ending);
        // The plan ledger as of this boundary. `Some` even when nothing has
        // been recorded: the Scheduler keeps the ledger, and "none recorded
        // yet" is a fact worth placing, unlike a caller that has no ledger at
        // all (`None`). The workers are folded from the same history: the plan
        // is what the model said it is doing, and they are which steps a child
        // is already carrying.
        let (plan, workers) =
            (plan::current(&history)?.unwrap_or_default(), plan::workers(&history));
        // The judgment record, on the same terms: the Scheduler keeps the
        // ledger, and "none recorded yet" is a fact worth placing.
        let judgment = judgment::current(&history)?.unwrap_or_default();
        // The work state the model last recorded, read the same way: the
        // Scheduler keeps it, so `Some` even when nothing was written, and the
        // loop says "none recorded yet" rather than opening a turn that seems
        // to have forgotten.
        let work = state::current(&history)?.unwrap_or_default();
        // A resume continuation opens on a turn that already closed: the held
        // call ran and may have failed there. The loop has no memory of that
        // turn, so the failure is read back from the log and handed to it.
        let resumed =
            item.payload.get("resumed").and_then(|value| value.as_bool()).unwrap_or(false);
        let pending_failure = if resumed {
            judgment::pending_failure(&history).map(|pending| TurnFailureFacts {
                call_id: pending.call_id,
                tool: pending.tool,
                detail: pending.detail,
            })
        } else {
            None
        };
        // The continuation's words are often the person's own "go ahead", which
        // reads as no cue at all; the decision the resume recorded is the fact
        // the posture can come from.
        let decision = resumed
            .then(|| item.payload.get("decision").and_then(|value| value.as_str()))
            .flatten()
            .map(str::to_string);
        // The permission subject is the intent this turn answers. A resumed
        // continuation inherits the active intent, so the held call and the
        // following work keep one identity.
        let authorization_subject = subject::for_turn(session_id, &item, resumed, &history)?;

        let state = TurnStateFacts {
            started_at: now,
            queued_inputs,
            input_kind: match &item.kind {
                InboxKind::Scheduled => TurnInputKind::Scheduled,
                _ => TurnInputKind::User,
            },
            previous_turn,
            resumed: decision,
            max_steps: self.max_steps,
            budget: self.budget.clone(),
            completion: self.completion.clone(),
        };
        // The turn's journal: the same log every other write goes to, committed
        // and flushed at the points where a fact has to outlive the process.
        let journal = Arc::new(SessionJournal::new(self.store.clone(), session_id.clone()));
        let ctx = TurnContext {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            run_id: run_id.clone(),
            authorization_subject,
            user_input: item.payload.clone(),
            journal: Journal::new(journal.clone()),
            instructions: instructions.clone(),
            state: Some(state),
            capability: self.capability.clone(),
            plan: Some(plan),
            workers,
            work: Some(work),
            judgment: Some(judgment),
            pending_failure,
        };

        // Opening the turn comes before the claim for the same reason the loop
        // used to run before it: a turn that could not start must leave the item
        // pending rather than consume it.
        let TurnStart { mut state, events: opening } = self.loop_.begin_turn(&ctx).await?;

        // The turn boundary, made durable before a single step runs: the run
        // exists from here on, and a process that dies now leaves an open run
        // for recovery to close instead of a session with nothing to show.
        // The item is deliberately *not* claimed here — see the terminal batch.
        let mut pending: Vec<EventDraft> = Vec::new();
        if unstarted {
            pending.push(session_lifecycle::started_draft(session_id, Actor::Scheduler));
        }
        // What this run is, before it is a run. The snapshot belongs to the run
        // and not to the session: a session can be driven by several processes
        // over its life, and the later ones may be configured differently.
        if let Some(profile) = &self.profile {
            pending.push(draft(
                session_id,
                EventType::PROFILE_RESOLVED,
                serde_json::to_value(profile)?,
                Actor::Scheduler,
                Some(&run_id),
                None,
                None,
            ));
        }
        pending.push(draft(
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
        // What this turn is an answer *to*, written beside what it consumed.
        // Why the binding sits on `turn/started` rather than on the terminal
        // payload is recorded with the payload itself.
        let answered_intent = crate::inbox::item_intent_id(&item);
        let turn_started_payload = report::turn_started_payload(&item, answered_intent.as_ref());
        pending.push(draft(
            session_id,
            EventType::TURN_STARTED,
            turn_started_payload,
            Actor::Scheduler,
            Some(&run_id),
            Some(&turn_id),
            None,
        ));
        pending.extend(opening);
        journal.commit(std::mem::take(&mut pending)).await?;

        let mut steps = 0u32;
        let mut last_step: Option<StepId> = None;
        // What the last completed step reported spending. A step that refused
        // to open at the boundary never ran, so it leaves this as the previous
        // step left it.
        let mut spent = Spent::default();
        // What this turn has already taken out of the queue, whether it went to
        // a step or was discarded as unreadable. Being consumed by the turn is a
        // fact only the turn can know, so it is tracked across steps even though
        // each boundary now writes what it produced.
        let mut consumed: HashSet<InboxItemId> = HashSet::new();
        let ending = loop {
            if self.max_steps.is_some_and(|max_steps| steps >= max_steps) {
                break Ending::Budget;
            }
            // An interrupt ends the turn at the boundary rather than inside a
            // step: what it stops is the next step, not the one already running.
            if let Some(interrupt) = self.inbox.pending_interrupt(session_id).await? {
                pending.push(
                    self.inbox.claim_draft(session_id, &interrupt.item_id, Some(&run_id)).await?,
                );
                journal.commit(std::mem::take(&mut pending)).await?;
                break Ending::Interrupted { item_id: interrupt.item_id };
            }

            let steering = self
                .drain_steering(
                    session_id,
                    &run_id,
                    &turn_id,
                    last_step.as_ref(),
                    &mut consumed,
                    &mut pending,
                )
                .await?;
            // What the boundary took out of the queue is durable before the
            // step that consumes it: a `turn/steered` written after the message
            // it introduced would date the claim later than the turn's use of
            // it, and a reader sorting by sequence would see a request carry
            // text whose arrival is recorded as still to come.
            journal.commit(std::mem::take(&mut pending)).await?;
            let outcome = match self.loop_.step(&mut state, &steering).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    // The run is already durable, so an error here cannot be
                    // answered by writing nothing: the turn has to be closed
                    // with the fact that it failed, or the next caller finds an
                    // open run and waits for recovery to guess what happened.
                    close_failed_turn(&journal, &run_id, &turn_id, pending).await;
                    return Err(SchedulerError::Loop(error));
                }
            };
            spent = outcome.spent;
            pending.extend(outcome.events);
            last_step = Some(outcome.step_id);
            steps += 1;
            // The step boundary: what it produced is durable before the next
            // step starts, so a crash between two steps loses at most the step
            // that was in flight.
            journal.commit(std::mem::take(&mut pending)).await?;

            if let Some(reason) = outcome.stop {
                break Ending::Loop { reason, final_content: outcome.final_content };
            }
        };

        let (stop_reason, final_content, turn_terminal, run_terminal, mut turn_payload) =
            ending.settle();
        // The review runs before any terminal; it returns the log as it then stands (B9).
        let (stop_reason, so_far) = self
            .review_before_claim(
                (session_id, &run_id, &turn_id, stop_reason),
                (&mut pending, &journal, &mut turn_payload),
            )
            .await?;
        // Read from the counters the terminal payload will carry, and before
        // `spent` moves into `Finished`: the parked record's percentage has to
        // describe the same run the spend does.
        let parked_percent = crate::parking::percent_of(
            u32::try_from(journal.steps()).unwrap_or(u32::MAX),
            self.max_steps,
            spent.total_tokens(),
            self.budget.max_total_tokens,
        );
        let finished = Finished::now(
            spent,
            journal.steps(),
            journal.calls(),
            run_started,
            self.budget.cache_hit_weight_percent.unwrap_or(10),
        );
        finished.decorate(&mut turn_payload, endings::is_limit(&stop_reason));
        // The run's terminal event carries the same spend. `turn/completed` and
        // `run/completed` are both terminal records readers can join to, and a
        // run-level audit must not have to walk back to find what it cost.
        let mut run_payload = serde_json::json!({ "stop_reason": &stop_reason });
        finished.decorate(&mut run_payload, false);
        completion::note_claims(
            &history,
            &so_far,
            &stop_reason,
            final_content.as_deref(),
            (session_id, &run_id, &turn_id),
            &mut pending,
        );
        // A limit ending is a pause, and this is what it paused with. Written
        // in the same batch as the terminal events, so a crash cannot leave a
        // yielded turn with nothing saying where it stopped. The record is built
        // from the log's own plan, state and ledger — not asked of the model,
        // which is the party that just ran out of budget (CONTRACT §7 #173).
        if endings::is_limit(&stop_reason) {
            let parked = crate::parking::state_of(&so_far, stop_reason.as_str(), parked_percent);
            pending.push(draft(
                session_id,
                EventType::BUDGET_PARKED,
                serde_json::json!({ "parked": parked }),
                Actor::Scheduler,
                Some(&run_id),
                Some(&turn_id),
                None,
            ));
        }
        // The item is claimed only now: the claim is the fact "this input has
        // been answered", and a turn that never reached its end has not
        // answered it. Writing the claim at the opening would consume the
        // person's message on a crash that produced no answer at all — the one
        // failure worse than doing the work twice, and the one no event in the
        // vocabulary can undo.
        pending.push(self.inbox.claim_draft(session_id, &item.item_id, Some(&run_id)).await?);
        pending.push(draft(
            session_id,
            turn_terminal,
            turn_payload,
            Actor::Scheduler,
            Some(&run_id),
            Some(&turn_id),
            None,
        ));
        pending.push(draft(
            session_id,
            run_terminal,
            run_payload,
            Actor::Scheduler,
            Some(&run_id),
            None,
            None,
        ));

        // The run's last write, and the one that makes the turn's ending
        // durable rather than merely accepted.
        journal.commit(std::mem::take(&mut pending)).await?;

        Ok(finished.report(run_id, turn_id, stop_reason, final_content, answered_intent))
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
}
#[cfg(test)]
mod tests;
