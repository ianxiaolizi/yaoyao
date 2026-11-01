//! Resume — the path from a paused turn back to a running one (M5).
//!
//! A turn that needs a decision stops at `turn/yielded` and stays stopped until
//! someone answers. It cannot be continued *in place*: the loop's in-memory
//! `TurnState` is not in the log and never will be, so "the rest of that turn"
//! is not something a new process can hold. What the log does hold is everything
//! the turn had done and the one thing it was waiting for, and that turns out to
//! be enough to pick the work up.
//!
//! Resume does two things, in this order:
//!
//! 1. **Settle the held call.** A grant re-dispatches it with the arguments the
//!    model originally proposed; a refusal records that it did not run. Either
//!    way the call gets an outcome — and so does every call the turn proposed
//!    but never reached, because an assistant message proposing calls that
//!    nothing answers is not a history any provider will accept back.
//! 2. **Carry the turn forward.** Close the step it stopped in, mark the turn
//!    resumed, and queue the continuation. The next `run_pending` then opens a
//!    *new* run and turn, which re-reads the transcript — including the tool
//!    result written in step 1 — and carries on from there.
//!
//! ## Running a side effect exactly once
//!
//! This is the property the feature exists for, and it rests on four things
//! that are easy to get subtly wrong:
//!
//! * **The whole sequence runs under the session's lease.** Every argument below
//!   is about one process doing the right thing; the lease is what keeps a
//!   second process out of the middle of it. Without it, two resumes both read
//!   the same unanswered call, both run it, and the losing one then finds the
//!   log has moved — a side effect produced with no record of having produced
//!   it. The lease is held from the first read to the last append because the
//!   dangerous part is not the write, it is the *act*.
//! * **A granted call runs through [`GuardedToolProvider::execute_approved`]**,
//!   not through the gateway's ordinary path. The policy question was already
//!   asked and answered; asking it again would raise a *second* approval for a
//!   decision that exists, and the session would wait on a queue nobody drains.
//! * **A call is only settled once.** "Waiting" is a property of the call's
//!   *latest* outcome, so the moment resume writes a real outcome the older
//!   `approval_pending` stops describing the call, and a second resume finds it
//!   settled and runs nothing. That is also what catches the process that died
//!   between the outcome and the turn it belonged to: the outcome is there, so
//!   the call is not re-run, and only the carrying-forward is repeated.
//! * **The dispatch is written before the call runs.** A crash in between
//!   leaves a `tool/started` with no outcome, which Recovery marks
//!   `tool/outcome_unknown` — and that marker, not the older `approval_pending`,
//!   is then the call's latest outcome. So a crash can cost a possibly-lost side
//!   effect; it cannot produce a duplicated one.
//!
//! Nothing here re-derives a *result*. Whatever the call returns is recorded the
//! way the loop records it, and the model reads it back out of the transcript.

use std::sync::Arc;

use agent_ports::event_store::{DriverRecord, EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{
    ApprovalId, EventId, EventSeq, RunId, SchemaVersion, SessionId, StepId, TurnId,
};
use agent_protocol::inbox::InboxKind;
use agent_protocol::model::ModelResponse;
use agent_protocol::permissions::ApprovalStatus;
use agent_protocol::redactor::Redactor;
use agent_protocol::tool::{
    APPROVAL_DENIED_OUTCOME, APPROVAL_PENDING_OUTCOME, ToolInvocation, ToolResult, ToolStatus,
};
use chrono::Utc;

use crate::approval::{ApprovalError, ApprovalService};
use crate::guarded::GuardedToolProvider;
use crate::inbox::{DiscardCause, Inbox, InboxError};
use crate::lifecycle;

/// Carries a yielded turn forward once its wait has ended.
pub struct Resume {
    store: Arc<dyn EventStore>,
    approvals: Arc<ApprovalService>,
    tools: Arc<GuardedToolProvider>,
    inbox: Inbox,
}

/// What one resume pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// No turn is waiting on anything. Nothing was written.
    NothingToResume,
    /// A decision is still outstanding. Nothing was written; calling resume
    /// again after the decision settles the call.
    Waiting { approval_id: ApprovalId, tool_name: String },
    /// The wait ended and the turn was carried forward.
    Settled { turn_id: TurnId, tool_name: String, disposition: Disposition },
    /// A turn is waiting, but not on anything this build can settle. Nothing
    /// was written and nothing was run.
    Unresumable { turn_id: TurnId, reason: String },
}

/// How the held call ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// The grant was applied and the call ran to this outcome.
    Ran { status: ToolStatus },
    /// The decision was negative (denied, expired, cancelled). The call did not
    /// run, and the model is told so.
    Denied { status: ApprovalStatus },
    /// The grant was applied, the call was dispatched, and the gateway refused
    /// it — an unknown tool, a path outside the workspace. The refusal is
    /// recorded; the call did not run.
    Rejected { reason: String },
    /// A previous resume already recorded this call's outcome. Nothing was run
    /// again: the log cannot say whether that outcome came from a call that
    /// finished, and guessing is how side effects get duplicated.
    AlreadySettled,
}

impl Disposition {
    /// The word that goes into `turn/resumed`, for a reader that wants to know
    /// what ended the wait without replaying the whole settlement.
    fn label(&self) -> &'static str {
        match self {
            Self::Ran { .. } | Self::Rejected { .. } => "granted",
            Self::Denied { status } => status_text(*status),
            Self::AlreadySettled => "already_settled",
        }
    }
}

/// Errors from Resume operations.
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("approval error: {0}")]
    Approval(#[from] ApprovalError),
    #[error("inbox error: {0}")]
    Inbox(#[from] InboxError),
    /// Another caller is driving the session. Nothing was read or written and
    /// no tool ran: the resume did not get far enough to settle anything, so
    /// answering the wait again later is simply trying again.
    #[error("session {session_id} is already being driven ({})", holder.as_ref().map_or("holder did not record itself".to_string(), DriverRecord::to_string))]
    SessionBusy { session_id: SessionId, holder: Option<DriverRecord> },
}

/// The call a paused turn stopped on.
#[derive(Debug, Clone)]
struct HeldCall {
    tool_name: String,
    tool_call_id: String,
    step_id: StepId,
    run_id: Option<RunId>,
    turn_id: TurnId,
    /// The wait, when there still is one. `None` means a previous resume
    /// already wrote this call's outcome and died before carrying the turn
    /// forward: the wait ended and the news never got out.
    wait: Option<Wait>,
}

/// What the call is waiting for.
#[derive(Debug, Clone)]
struct Wait {
    approval_id: ApprovalId,
    /// The arguments the model proposed. Kept so a granted call can be
    /// re-dispatched without asking the model to say all of it again.
    arguments: serde_json::Value,
}

impl Resume {
    pub fn new(
        store: Arc<dyn EventStore>,
        approvals: Arc<ApprovalService>,
        tools: Arc<GuardedToolProvider>,
    ) -> Self {
        Self { inbox: Inbox::new(store.clone()), store, approvals, tools }
    }

    /// Settle a yielded turn's held call and queue the continuation.
    ///
    /// `continuation` becomes the user message of the next turn. It is written
    /// here rather than left to the caller so that "the wait is over" and
    /// "there is something to run" are one step: a caller that settled a turn
    /// without queueing anything would leave a session nobody can move forward.
    pub async fn resume(
        &self,
        session_id: &SessionId,
        continuation: &str,
    ) -> Result<ResumeOutcome, ResumeError> {
        // The driver claim comes first. The write lease below keeps a second
        // *writer* out, but it waits: a process that is mid-turn in another
        // terminal holds nothing here yet, and a resume that waited for it
        // would be waiting to answer a wait that driver is about to settle
        // itself. The claim says so instead, and says who.
        let _driver = match self.store.driver_lease(session_id).await {
            Ok(lease) => lease,
            Err(EventStoreError::ConcurrentDriver { session_id, holder }) => {
                return Err(ResumeError::SessionBusy { session_id, holder });
            }
            Err(other) => return Err(ResumeError::Store(other)),
        };

        // Hold the session's lease across the whole sequence — the read that
        // finds the wait, the run that answers it, and the appends that record
        // it. A granted call's dispatch is appended before the call runs, so the
        // log alone survives a crash; what it does not survive is a second
        // process doing the same read while this one is still between the two.
        // That process would find the same unanswered call and run it too, and
        // the one that appended second would leave a real effect with no record
        // of it. The lease is what keeps that process out.
        let _lease = self.store.lease(session_id).await?;

        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        let lc = lifecycle::fold(&events);

        let Some(turn) = lc.yielded_turn.clone() else {
            return Ok(ResumeOutcome::NothingToResume);
        };

        // Reading the wait comes first and writes nothing: every way of not
        // being able to resume ends here, before anything has run.
        let held = match classify(&events, &turn) {
            Ok(held) => held,
            Err(reason) => return Ok(ResumeOutcome::Unresumable { turn_id: turn, reason }),
        };
        let unreached = match unreached_calls(session_id, &events, &held) {
            Ok(drafts) => drafts,
            Err(reason) => return Ok(ResumeOutcome::Unresumable { turn_id: turn, reason }),
        };

        let (disposition, mut drafts) = match &held.wait {
            Some(wait) => {
                let Some((disposition, settled)) = self.settle(session_id, &held, wait).await?
                else {
                    return Ok(ResumeOutcome::Waiting {
                        approval_id: wait.approval_id.clone(),
                        tool_name: held.tool_name.clone(),
                    });
                };
                (disposition, settled)
            }
            // The call is answered already; what is missing is the rest.
            None => (Disposition::AlreadySettled, Vec::new()),
        };

        let label = disposition.label();
        drafts.extend(unreached);
        drafts.extend(carry_forward(session_id, &lc, &held, label));

        let next = next_seq(&self.store, session_id).await?;
        self.store.append(session_id, next, drafts).await?;

        // The continuation goes in its own append, after the settlement. The
        // other order would let a crash leave an item behind a wait the log
        // still says is open — and the retry would queue the text a second time.
        let continuation = if continuation.trim().is_empty() {
            continuation_text(&events, &held, &disposition)
        } else {
            continuation.to_string()
        };
        // `resumed` marks the turn this item opens as the continuation of an
        // interrupted one: the held call has now run, and if it failed that
        // failure belongs to a turn that has already closed. The Scheduler
        // reads it back from the log for exactly these items. `decision` is the
        // same word `turn/resumed` carries: the continuation's text may be the
        // person's own "go ahead", which says nothing about what just happened.
        self.inbox
            .append(
                session_id,
                InboxKind::NextTurn,
                serde_json::json!({
                    "text": continuation,
                    "resumed": true,
                    "decision": disposition.label(),
                }),
                0,
                Actor::User,
            )
            .await?;

        // Last, because it is the only step that can be safely repeated: a crash
        // before it leaves the wakeup pending, which is where it already was.
        self.spend_wakeups(session_id).await?;

        Ok(ResumeOutcome::Settled { turn_id: turn, tool_name: held.tool_name.clone(), disposition })
    }

    /// Mark the wakeups this resume acted on as spent.
    ///
    /// `approve` and `deny` record the decision and then queue a `wakeup` so the
    /// session gets picked back up. Nothing *selects* a wakeup — it is a signal,
    /// not work: `next_turn`, `steering`, and `pending_interrupt` each look for
    /// their own kind — so without this it stays pending forever and `status`
    /// reports an item no run will ever take.
    ///
    /// A wakeup is only ever written next to a decision, so one whose approval
    /// is no longer pending has been acted on and is spent. This is deliberately
    /// not a rule the `Scheduler` applies: "the decision has been carried out"
    /// is a judgment only the thing that carried it out can make.
    async fn spend_wakeups(&self, session_id: &SessionId) -> Result<(), ResumeError> {
        for item in self.inbox.pending(session_id).await? {
            if item.kind != InboxKind::Wakeup {
                continue;
            }
            // A wakeup with no string `approval_id` cannot be matched to a
            // decision, and leaving it pending would keep a `status` entry
            // alive for work nothing will ever take. Discard it with the
            // reason, so the bad payload is visible rather than merely gone.
            let Some(approval_id) = item.payload.get("approval_id").and_then(|v| v.as_str()) else {
                self.inbox
                    .discard(
                        session_id,
                        &item.item_id,
                        DiscardCause::Unreadable,
                        Some("wakeup payload has no string `approval_id`"),
                    )
                    .await?;
                continue;
            };
            // A decision that cannot be read is not a decision to throw away.
            let Ok(status) =
                self.approvals.status(session_id, &ApprovalId::from_string(approval_id)).await
            else {
                continue;
            };
            if status == ApprovalStatus::Pending {
                continue;
            }
            self.inbox.discard(session_id, &item.item_id, DiscardCause::Spent, None).await?;
        }
        Ok(())
    }

    /// Answer the held call, or report that the answer is not in yet.
    ///
    /// `Ok(None)` means the approval is still pending. Nothing was written and
    /// nothing was run.
    ///
    /// A granted call is the one case that writes *before* it acts: the dispatch
    /// goes to the log in an append of its own and only then does the call run,
    /// so a crash in between leaves a `tool/started` with no outcome for
    /// [`Recovery`](crate::recovery::Recovery) to mark. That marker, not the
    /// older `approval_pending`, is then the call's latest outcome — which is
    /// what stops the next resume from reading it as a wait that is still open
    /// and running the call a second time.
    ///
    /// Everything else in a resume is appended afterwards as one batch, and can
    /// be, because all of it is recomputable from the log. A side effect is not.
    async fn settle(
        &self,
        session_id: &SessionId,
        held: &HeldCall,
        wait: &Wait,
    ) -> Result<Option<(Disposition, Vec<EventDraft>)>, ResumeError> {
        let status = self.approvals.status(session_id, &wait.approval_id).await?;
        match status {
            ApprovalStatus::Pending => Ok(None),

            ApprovalStatus::Granted => {
                let next = next_seq(&self.store, session_id).await?;
                self.store
                    .append(session_id, next, vec![dispatch_draft(session_id, held, wait)])
                    .await?;
                // The dispatch is on disk but not yet *durable*: an append sits
                // in the page cache and is lost to a power cut, and this is the
                // one place in the product where a real side effect follows an
                // append. Lose the dispatch, and the next resume reads the call
                // as never dispatched and runs it again — a repeated real
                // effect, which is the whole window C4.3 exists to close. So
                // flush here, before the call runs, and treat a failed flush as
                // a reason *not* to run: a call whose dispatch cannot be made to
                // outlive the machine is a call we must not make. This is what
                // makes `run_approved`'s "already durable by the time this runs"
                // true rather than aspirational (C4.4).
                self.store.flush(session_id).await?;
                let (drafts, disposition) = self.run_approved(session_id, held, wait).await;
                Ok(Some((disposition, drafts)))
            }

            status @ (ApprovalStatus::Denied
            | ApprovalStatus::Expired
            | ApprovalStatus::Cancelled) => {
                let content = match status {
                    ApprovalStatus::Denied => {
                        "The user denied this tool call, so it was not executed. Do not retry it \
                         as proposed — ask the user how to proceed."
                            .to_string()
                    }
                    other => format!(
                        "This tool call was not executed: its approval is {}. Ask the user \
                         before trying again.",
                        status_text(other)
                    ),
                };
                let payload = serde_json::json!({
                    "tool_name": held.tool_name,
                    "provider_tool_call_id": held.tool_call_id,
                    "status": ToolStatus::Failed,
                    "content": content,
                    // The call did not run: it is a decision, not an
                    // observation. Marked here so the judgment gate does not
                    // read a denial as a failure waiting to be reconciled.
                    "outcome": APPROVAL_DENIED_OUTCOME,
                    "approval_id": wait.approval_id,
                    "approval_status": status,
                });
                let draft = tool_draft(session_id, held, EventType::TOOL_FAILED, payload);
                Ok(Some((Disposition::Denied { status }, vec![draft])))
            }
        }
    }

    /// Run an approved call and record what came back — the second phase.
    ///
    /// The dispatch is already durable by the time this runs (see [`settle`]):
    /// the caller wrote it in its own append, so a crash here leaves a
    /// `tool/started` with no outcome rather than a call nobody can tell ran.
    /// Everything this returns is appended afterwards, in one batch with the
    /// rest of the settlement, because every draft in it can be recomputed from
    /// the log; the call cannot.
    ///
    /// [`settle`]: Self::settle
    async fn run_approved(
        &self,
        session_id: &SessionId,
        held: &HeldCall,
        wait: &Wait,
    ) -> (Vec<EventDraft>, Disposition) {
        let invocation = ToolInvocation::from_tool_call(
            held.tool_call_id.clone(),
            held.tool_name.clone(),
            wait.arguments.clone(),
        );

        match self.tools.execute_approved(session_id, &wait.approval_id, &invocation).await {
            Ok(result) => recorded_result(session_id, held, result),
            // The call was approved, so this is not a policy refusal: it is the
            // gateway saying the call could not be run as asked. Same shape the
            // loop records for a gateway error, because it is the same fact.
            Err(error) => {
                let reason = error.to_string();
                let draft = tool_draft(
                    session_id,
                    held,
                    EventType::TOOL_FAILED,
                    serde_json::json!({
                        "tool_name": held.tool_name,
                        "provider_tool_call_id": held.tool_call_id,
                        "status": ToolStatus::Failed,
                        "content": reason,
                    }),
                );
                (vec![draft], Disposition::Rejected { reason })
            }
        }
    }
}

/// The continuation a resume queues when the caller supplied none.
///
/// "Continue the task" names nothing, and it lands beside a state layer that
/// also names nothing. A small model answers by repeating the nearest text —
/// the state layer itself — and the loop records that as a final answer
/// (observed 2026-10-21 with qwen2.5-3b: 5 of 6 resumed runs). This names the
/// call, what became of it, and the request it was serving, so the new turn
/// starts from the work rather than from the summary of the wait.
fn continuation_text(events: &[StoredEvent], held: &HeldCall, disposition: &Disposition) -> String {
    let call = match &held.wait {
        Some(wait) => {
            format!("`{}` with {}", held.tool_name, one_line(&wait.arguments.to_string(), 120))
        }
        None => format!("`{}`", held.tool_name),
    };
    let outcome = match disposition {
        Disposition::Ran { .. } => "has run; its result is the tool message above",
        Disposition::Denied { .. } => "was denied and did not run",
        Disposition::Rejected { .. } => "was refused by the gateway and did not run",
        Disposition::AlreadySettled => "had already been settled and was not run again",
    };
    let mut text = format!("The approval for {call} ended the wait: it {outcome}.");
    match last_request(events) {
        Some(request) => {
            text.push_str(&format!(" The user's request was: \"{}\".", one_line(&request, 200)));
            text.push_str(
                " Answer that request from the result in the history; do not restate this \
                 message or the state summary.",
            );
        }
        None => text.push_str(
            " Continue the task from the result in the history; do not restate this message \
             or the state summary.",
        ),
    }
    text
}

/// The newest request text in the log — what the user asked for before the wait.
///
/// A steer counts: it is the most recent instruction, and the one the next turn
/// should be answering.
fn last_request(events: &[StoredEvent]) -> Option<String> {
    events
        .iter()
        .filter(|stored| stored.event.event_type.as_str() == EventType::INBOX_APPENDED)
        .filter_map(|stored| {
            // The item's own payload nests under `payload`: the envelope says
            // which item was queued, the inner object is what it carries.
            stored
                .event
                .payload
                .get("payload")
                .and_then(|item| item.get("text"))
                .and_then(|text| text.as_str())
        })
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .next_back()
}

/// Flatten to one line and cut at a character bound, marking the cut.
fn one_line(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let cut: String = flat.chars().take(limit).collect();
    format!("{cut}…")
}

/// Read the log for the call a paused turn stopped on.
///
/// `Err` is a reason the wait cannot be settled by this build — reported, not
/// fatal, and nothing is written.
fn classify(events: &[StoredEvent], turn: &TurnId) -> Result<HeldCall, String> {
    let mut starts: Vec<Started> = Vec::new();
    let mut outcomes: Vec<Outcome> = Vec::new();

    for stored in events {
        let event = &stored.event;
        if event.turn_id.as_ref() != Some(turn) {
            continue;
        }
        let Some(tool_call_id) = call_id(event) else {
            continue;
        };
        match event.event_type.as_str() {
            EventType::TOOL_STARTED => {
                // The first dispatch for a call is the model's proposal, and
                // its arguments are the ones to run. Later dispatches are
                // resumes of it, and must not replace what was first asked.
                if !starts.iter().any(|s| s.tool_call_id == tool_call_id) {
                    starts.push(Started {
                        tool_call_id,
                        tool_name: tool_name(event),
                        arguments: event.payload.get("arguments").cloned(),
                        step_id: event.step_id.clone(),
                        run_id: event.run_id.clone(),
                    });
                }
            }
            EventType::TOOL_COMPLETED
            | EventType::TOOL_FAILED
            | EventType::TOOL_CANCELLED
            | EventType::TOOL_INTERRUPTED
            | EventType::TOOL_OUTCOME_UNKNOWN
            | EventType::TOOL_NOT_STARTED => {
                outcomes.push(Outcome { tool_call_id, payload: event.payload.clone() });
            }
            _ => {}
        }
    }

    // Waiting is a property of a call's *latest* outcome. That is what makes a
    // settlement stick: once a resume has written a real outcome, the older
    // `approval_pending` no longer describes the call, and a second resume
    // cannot mistake it for an unanswered wait and run it again.
    let waiting =
        starts.iter().find(|call| latest(&outcomes, &call.tool_call_id).is_some_and(is_wait));
    if let Some(call) = waiting {
        let approval_id = latest(&outcomes, &call.tool_call_id)
            .and_then(|o| approval_id(&o.payload))
            .ok_or_else(|| {
                format!("tool '{}' is waiting on an approval the log does not name", call.tool_name)
            })?;
        let arguments = call.arguments.clone().ok_or_else(|| {
            format!(
                "tool '{}' is waiting on a decision but its arguments are not in the log, so \
                     it cannot be run again as proposed",
                call.tool_name
            )
        })?;
        return Ok(HeldCall {
            tool_name: call.tool_name.clone(),
            tool_call_id: call.tool_call_id.clone(),
            step_id: call.step_id.clone().ok_or_else(|| {
                format!(
                    "tool '{}' was dispatched with no step to record its outcome in",
                    call.tool_name
                )
            })?,
            run_id: call.run_id.clone(),
            turn_id: turn.clone(),
            wait: Some(Wait { approval_id, arguments }),
        });
    }

    // No live wait. A call that was held and now has some other outcome means a
    // previous resume settled it and did not get to carry the turn forward.
    let settled = starts
        .iter()
        .find(|call| outcomes.iter().any(|o| o.tool_call_id == call.tool_call_id && is_wait(o)));
    if let Some(call) = settled {
        return Ok(HeldCall {
            tool_name: call.tool_name.clone(),
            tool_call_id: call.tool_call_id.clone(),
            step_id: call.step_id.clone().ok_or_else(|| {
                format!(
                    "tool '{}' was dispatched with no step to record its outcome in",
                    call.tool_name
                )
            })?,
            run_id: call.run_id.clone(),
            turn_id: turn.clone(),
            wait: None,
        });
    }

    Err("the turn yielded but the log holds no call it was waiting on".to_string())
}

/// The text the model is shown for a call that ran.
fn recorded_result(
    session_id: &SessionId,
    held: &HeldCall,
    result: ToolResult,
) -> (Vec<EventDraft>, Disposition) {
    let refs = artifact_refs(&result);
    let mut drafts = Vec::new();

    // What happened to the result on its way here, recorded before the result
    // itself: these are the facts that say what follows may not be all of it.
    // Same three events, in the same order, that the loop writes.
    if !refs.is_empty() {
        drafts.push(tool_draft(
            session_id,
            held,
            EventType::TOOL_RESULT_ARTIFACT_OFFLOADED,
            serde_json::json!({
                "tool_name": held.tool_name,
                "provider_tool_call_id": held.tool_call_id,
                "artifact_refs": refs,
            }),
        ));
    }
    if result.truncated {
        drafts.push(tool_draft(
            session_id,
            held,
            EventType::TOOL_RESULT_TRUNCATED,
            serde_json::json!({
                "tool_name": held.tool_name,
                "provider_tool_call_id": held.tool_call_id,
            }),
        ));
    }
    if result.redaction_metadata.redacted {
        drafts.push(tool_draft(
            session_id,
            held,
            EventType::TOOL_RESULT_REDACTED,
            serde_json::json!({
                "tool_name": held.tool_name,
                "provider_tool_call_id": held.tool_call_id,
                "patterns": result.redaction_metadata.patterns,
            }),
        ));
    }
    if !result.redaction_metadata.warnings.is_empty() {
        drafts.push(tool_draft(
            session_id,
            held,
            EventType::TOOL_RESULT_HIGH_ENTROPY,
            serde_json::json!({
                "tool_name": held.tool_name,
                "provider_tool_call_id": held.tool_call_id,
                "warnings": result.redaction_metadata.warnings,
            }),
        ));
    }

    let event_type = match result.status {
        ToolStatus::Completed => EventType::TOOL_COMPLETED,
        ToolStatus::Failed => EventType::TOOL_FAILED,
        ToolStatus::Cancelled => EventType::TOOL_CANCELLED,
    };
    drafts.push(tool_draft(
        session_id,
        held,
        event_type,
        serde_json::json!({
            "tool_name": held.tool_name,
            "provider_tool_call_id": held.tool_call_id,
            "status": result.status,
            "content": result.content,
            "artifact_refs": refs,
            "truncated": result.truncated,
        }),
    ));

    (drafts, Disposition::Ran { status: result.status })
}

/// Answer every call the turn proposed that it never reached.
///
/// Exact rather than a guess, because of how a turn stops: the loop returns at
/// the first call that needs a decision, the whole turn appends as one batch,
/// and the dispatch is written per call — so a proposed call with no
/// `tool/started` provably never ran. That is why these carry `tool/not_started`
/// and not `tool/outcome_unknown`: re-issuing one is not a retry, and telling
/// the model they are knowable only as "maybe" would teach it to distrust a call
/// that certainly did not happen.
fn unreached_calls(
    session_id: &SessionId,
    events: &[StoredEvent],
    held: &HeldCall,
) -> Result<Vec<EventDraft>, String> {
    let Some(proposed) = proposed_calls(events, held) else {
        return Err(
            "the model response the stopped step came from is not in the log, so the calls it \
             proposed cannot be accounted for"
                .to_string(),
        );
    };
    Ok(proposed
        .into_iter()
        .filter(|call| call.tool_call_id != held.tool_call_id)
        // Dispatched but unsettled is Recovery's business, not resume's:
        // settling it here as "never started" would be a lie about a call that
        // ran. Recovery runs before resume for exactly this reason.
        .filter(|call| !dispatched(events, &call.tool_call_id))
        .map(|call| {
            tool_draft(
                session_id,
                held,
                EventType::TOOL_NOT_STARTED,
                serde_json::json!({
                    "tool_name": call.tool_name,
                    "provider_tool_call_id": call.tool_call_id,
                    "content": "This tool call was never dispatched: the turn stopped at another \
                                call before reaching it, so nothing ran. Propose it again if it \
                                is still needed.",
                }),
            )
        })
        .collect())
}

/// Close the step the turn stopped in, and mark the turn resumed.
///
/// `turn/resumed` is what clears the wait, so this is the batch that makes the
/// session runnable again — written together with the settlement, so there is no
/// moment where the log says the wait is over while the call it waited for is
/// still unanswered.
fn carry_forward(
    session_id: &SessionId,
    lc: &lifecycle::Lifecycle,
    held: &HeldCall,
    label: &str,
) -> Vec<EventDraft> {
    let mut drafts = Vec::new();
    if let Some(step) = lc.open_step.clone() {
        drafts.push(draft(
            session_id,
            EventType::STEP_COMPLETED,
            serde_json::json!({ "resumed": true }),
            Actor::Scheduler,
            lc.open_run.as_ref(),
            Some(&held.turn_id),
            Some(&step),
        ));
    }
    drafts.push(draft(
        session_id,
        EventType::TURN_RESUMED,
        serde_json::json!({
            "tool_name": held.tool_name,
            "provider_tool_call_id": held.tool_call_id,
            "decision": label,
        }),
        Actor::Scheduler,
        lc.open_run.as_ref(),
        Some(&held.turn_id),
        None,
    ));
    drafts
}

/// A dispatch the loop recorded for a call, with what resume needs from it.
struct Started {
    tool_call_id: String,
    tool_name: String,
    /// `None` when the log's `tool/started` recorded no arguments at all —
    /// which is not the same as recording `null`, and is reason enough not to
    /// run the call again.
    arguments: Option<serde_json::Value>,
    step_id: Option<StepId>,
    run_id: Option<RunId>,
}

/// An outcome the log recorded for a call.
struct Outcome {
    tool_call_id: String,
    payload: serde_json::Value,
}

/// A tool call the model proposed.
struct Call {
    tool_call_id: String,
    tool_name: String,
}

/// Whether an outcome is a wait rather than a result.
///
/// The `outcome` field is the mark; the type is checked too, because the mark
/// on a call that reached a *terminal* outcome would be a log that contradicts
/// itself, and reading the mark alone would build a wait out of it.
fn is_wait(outcome: &Outcome) -> bool {
    outcome.payload.get("outcome").and_then(|v| v.as_str()) == Some(APPROVAL_PENDING_OUTCOME)
        && outcome.payload.get("status").and_then(|v| v.as_str()) == Some("failed")
}

/// The latest outcome recorded for a call.
fn latest<'a>(outcomes: &'a [Outcome], tool_call_id: &str) -> Option<&'a Outcome> {
    outcomes.iter().rev().find(|o| o.tool_call_id == tool_call_id)
}

/// The tool calls the model proposed in the step the held call belongs to.
///
/// Read from the response the model actually gave, so "proposed" means what it
/// proposed and not what the loop got round to dispatching.
fn proposed_calls(events: &[StoredEvent], held: &HeldCall) -> Option<Vec<Call>> {
    let response = events
        .iter()
        .rev()
        .find(|s| {
            s.event.event_type.as_str() == EventType::MODEL_RESPONSE_COMPLETED
                && s.event.step_id.as_ref() == Some(&held.step_id)
                && s.event.turn_id.as_ref() == Some(&held.turn_id)
        })?
        .event
        .payload
        .get("response")?;
    let response = serde_json::from_value::<ModelResponse>(response.clone()).ok()?;
    Some(
        response
            .tool_calls()
            .into_iter()
            .map(|c| Call { tool_call_id: c.tool_call_id, tool_name: c.tool_name })
            .collect(),
    )
}

/// Whether the log shows a dispatch for this call anywhere in the session.
fn dispatched(events: &[StoredEvent], tool_call_id: &str) -> bool {
    events.iter().any(|s| {
        s.event.event_type.as_str() == EventType::TOOL_STARTED
            && call_id(&s.event).as_deref() == Some(tool_call_id)
    })
}

async fn next_seq(
    store: &Arc<dyn EventStore>,
    session_id: &SessionId,
) -> Result<EventSeq, ResumeError> {
    let events = store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
    Ok(events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO))
}

/// The `tool/started` a resume writes for the call it is about to run.
///
/// Written on its own, before the call runs, so that the log a crash leaves
/// behind names the call. `resumed` separates this dispatch from the model's
/// original proposal, which stays the one `classify` reads the arguments from.
fn dispatch_draft(session_id: &SessionId, held: &HeldCall, wait: &Wait) -> EventDraft {
    // Written scrubbed for the same reason the model's own proposal is: the log
    // is long-lived, and a call resumed from a log written before redaction
    // existed must not re-introduce the argument secret it held (§7 row 4).
    let (arguments, redaction) = Redactor::new().redact_value(&wait.arguments);
    let mut payload = serde_json::json!({
        "tool_name": held.tool_name,
        "provider_tool_call_id": held.tool_call_id,
        "arguments": arguments,
        "approval_id": wait.approval_id,
        "resumed": true,
    });
    if redaction.redacted {
        payload["redaction"] = serde_json::json!({
            "redacted": true,
            "patterns": redaction.patterns,
        });
    }
    tool_draft(session_id, held, EventType::TOOL_STARTED, payload)
}

fn tool_draft(
    session_id: &SessionId,
    held: &HeldCall,
    event_type: &str,
    payload: serde_json::Value,
) -> EventDraft {
    draft(
        session_id,
        event_type,
        payload,
        Actor::Tool,
        held.run_id.as_ref(),
        Some(&held.turn_id),
        Some(&held.step_id),
    )
}

#[allow(clippy::too_many_arguments)]
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

fn artifact_refs(result: &ToolResult) -> Vec<serde_json::Value> {
    result.artifacts.iter().map(|a| a.reference()).collect()
}

fn call_id(event: &EventDraft) -> Option<String> {
    event.payload.get("provider_tool_call_id").and_then(|v| v.as_str()).map(str::to_string)
}

fn tool_name(event: &EventDraft) -> String {
    event.payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("unknown").to_string()
}

fn approval_id(payload: &serde_json::Value) -> Option<ApprovalId> {
    payload.get("approval_id").and_then(|v| v.as_str()).map(ApprovalId::from_string)
}

fn status_text(status: ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Granted => "granted",
        ApprovalStatus::Denied => "denied",
        ApprovalStatus::Expired => "expired",
        ApprovalStatus::Cancelled => "cancelled",
    }
}

/// Out of line because `#[cfg(test)]` must be the last thing a production
/// file says (the boundary guards in agent-testkit assert exactly that).
#[cfg(test)]
mod tests;
