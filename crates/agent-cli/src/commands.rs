//! Commands with their presentation removed.
//!
//! The `run_*` functions in this crate are the CLI's *presentation*: they turn a
//! result into text or a `--json` document and pick a [`Verdict`]. The functions
//! here are the same commands without that last step, so a second front end —
//! the web GUI — can call them and render its own shapes without parsing the
//! CLI's own output back out of a string.
//!
//! Nothing in this module prints, and nothing here writes a different event than
//! the CLI would: each function is the body the corresponding `run_*` used to
//! inline, moved verbatim. The split is a seam, not a behaviour change, and the
//! existing CLI tests are the proof: they call `run_*` and must pass unchanged.
//!
//! [`Verdict`]: crate::Verdict

use agent_kernel::approval::{ApprovalAuthority, ApprovalError, ApprovalService};
use agent_kernel::inbox::{Inbox, InboxError};
use agent_kernel::intent::IntentError;
use agent_kernel::intent_adjudication::AdjudicationStatus;
use agent_kernel::scheduler::{RunReport, Scheduler, SchedulerError};
use agent_protocol::events::Actor;
use agent_protocol::ids::{ApprovalId, InboxItemId, IntentId, RunId, SessionId, StepId, TurnId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::intent::IntentStatus;
use agent_protocol::permissions::{ApprovalReply, ApprovalScope};
use agent_protocol::status::{CompletionStatus, DeliveryStatus, RunStatus, VerificationStatus};
use chrono::{DateTime, Utc};

/// Why a command could not be carried out.
///
/// Every variant is a refusal to *start* or to *write*: the command did not
/// happen, which is the same thing the CLI reports on stderr with exit `2`.
/// A command that ran and produced a bad answer is not an error here — that is
/// the answer, and the front end decides how to show it.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    Inbox(#[from] InboxError),
    #[error(transparent)]
    Intent(#[from] IntentError),
    #[error(transparent)]
    Approval(#[from] ApprovalError),
    /// The command's own argument is wrong. Nothing was written.
    #[error("{0}")]
    Invalid(String),
}

impl CommandError {
    /// True when the refusal is "another caller is already driving this
    /// session".
    ///
    /// The distinction a front end needs: every other failure says this
    /// request could not be carried out, while this one says it arrived at a
    /// session somebody else is turning and will work when they are done. The
    /// CLI exits `2` either way; the GUI answers `409` for this one alone.
    pub fn is_busy(&self) -> bool {
        matches!(self, CommandError::Scheduler(SchedulerError::SessionBusy { .. }))
    }
}

/// What starting a turn did.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnStart {
    /// A turn ran to its end (which may be a yield for approval).
    Ran(RunReport),
    /// Nothing ran: a run is already active, or there was no pending input.
    NotStarted,
    /// The message was a stop. Nothing new was queued to run; what was waiting
    /// was withdrawn and, if a run was in flight, an interrupt was asked for.
    Stopped(StopReceipt),
}

/// What a stop withdrew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReceipt {
    /// Queue items taken out by the stop, in queue order.
    pub withdrawn: Vec<InboxItemId>,
    /// The instructions behind those items, where the log names one.
    pub cancelled: Vec<IntentId>,
    /// The interrupt queued for a run in flight, when there was one.
    pub interrupt: Option<InboxItemId>,
}

impl StopReceipt {
    /// Whether the stop actually stopped something.
    ///
    /// A stop with nothing queued and no run is a command that found nothing to
    /// do — `agent interrupt` answers the same state the same way (exit `2`).
    pub fn stopped_anything(&self) -> bool {
        !self.withdrawn.is_empty() || self.interrupt.is_some()
    }
}

/// Submit a direct message, then drive whatever turn it queued.
///
/// The append is a separate fact from the run: if the run cannot start, the
/// text is still in the inbox, exactly as `agent ask` leaves it today. The
/// instruction is recorded to the intent ledger beside its inbox item, so a
/// later reader can tell which request a given answer belongs to — and a
/// message the rule layer reads as a stop never becomes a turn at all.
pub async fn start_turn(
    scheduler: &Scheduler,
    session_id: &SessionId,
    text: &str,
    now: DateTime<Utc>,
) -> Result<TurnStart, CommandError> {
    let submitted = scheduler.inbox().submit_user_message(session_id, text, now).await?;
    if !submitted.route.queues_a_turn() {
        return Ok(TurnStart::Stopped(StopReceipt {
            withdrawn: submitted.displaced,
            cancelled: submitted.cancelled,
            interrupt: submitted.item.map(|item| item.item_id),
        }));
    }

    let report = scheduler.run_pending(session_id, now).await?;
    Ok(match report {
        Some(report) => TurnStart::Ran(report),
        None => TurnStart::NotStarted,
    })
}

/// A queued steering note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerReceipt {
    /// The inbox item the note became.
    pub item_id: InboxItemId,
    /// The turn the note was addressed to, or `None` for "whatever comes next".
    pub expected_turn: Option<TurnId>,
    /// The text, once trimmed and stored.
    pub text: String,
}

/// Queue text for the turn already in flight.
///
/// The target turn is read from the durable lifecycle rather than from process
/// state, so this works from a process that is not driving the run. When no turn
/// is open the note is addressed to no one in particular and lands on the next
/// turn's first step.
pub async fn steer(
    scheduler: &Scheduler,
    session_id: &SessionId,
    text: &str,
) -> Result<SteerReceipt, CommandError> {
    if text.trim().is_empty() {
        return Err(CommandError::Invalid("steer needs text: agent steer \"<text>\"".to_string()));
    }
    let expected_turn = scheduler.lifecycle(session_id).await?.open_turn;
    let item = scheduler.inbox().steer(session_id, text, expected_turn.as_ref()).await?;
    Ok(SteerReceipt { item_id: item.item_id, expected_turn, text: text.to_string() })
}

/// What an interrupt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// The request is queued; the process driving the session applies it at the
    /// next step boundary.
    Queued(InboxItemId),
    /// Nothing was in flight and nothing was waiting, so nothing was queued.
    NothingToInterrupt,
}

/// Ask the run in flight to stop at its next step boundary.
///
/// The state check comes first and is the whole point of this being more than a
/// one-line append: an interrupt appended while nothing runs would kill the
/// *next* turn before its first step. Both `open_run` and `yielded_turn` are
/// read because a paused turn has no open run but is exactly the thing somebody
/// means to interrupt.
pub async fn interrupt(
    scheduler: &Scheduler,
    session_id: &SessionId,
) -> Result<InterruptOutcome, CommandError> {
    let lifecycle = scheduler.lifecycle(session_id).await?;
    if lifecycle.open_run.is_none() && lifecycle.yielded_turn.is_none() {
        return Ok(InterruptOutcome::NothingToInterrupt);
    }

    let item = scheduler.inbox().interrupt(session_id, "user_interrupt").await?;
    Ok(InterruptOutcome::Queued(item.item_id))
}

/// How wide a grant reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// This one call.
    Once,
    /// Every call until the next turn ends.
    Turn,
    /// Every call for the rest of the session.
    Session,
    /// The session scope, *and* every other pending request of the same kind.
    Always,
}

impl Grant {
    /// Read the scope a person typed: `once`, `turn`, `session`, `always`.
    pub fn from_name(name: &str) -> Result<Self, CommandError> {
        match name {
            "once" => Ok(Self::Once),
            "turn" => Ok(Self::Turn),
            "session" => Ok(Self::Session),
            "always" => Ok(Self::Always),
            other => Err(CommandError::Invalid(format!(
                "unknown scope '{other}'; expected once|turn|session|always"
            ))),
        }
    }

    /// The scope a single reply carries. `Always` is `Session` on the wire: the
    /// wide form is a *cascade*, not a fourth scope.
    fn scope(self) -> ApprovalScope {
        match self {
            Self::Once => ApprovalScope::AllowOnce,
            Self::Turn => ApprovalScope::AllowForTurn,
            Self::Session | Self::Always => ApprovalScope::AllowForSession,
        }
    }
}

/// The decisions that were recorded, in the order they were settled.
#[derive(Debug, Clone)]
pub struct ApprovalReceipt {
    pub replies: Vec<ApprovalReply>,
}

/// Grant a pending approval, then wake the turn that was waiting on it.
///
/// The wakeup is one item per settled approval, not one for the id that was
/// named: `always` settles a *kind* of request, and each settled request is a
/// decision the paused turn may be waiting on.
pub async fn grant_approval(
    approvals: &ApprovalService,
    inbox: &Inbox,
    session_id: &SessionId,
    approval_id: &str,
    grant: Grant,
    actor: &str,
    authority: &ApprovalAuthority,
) -> Result<ApprovalReceipt, CommandError> {
    let approval_id = ApprovalId::from_string(approval_id);
    let replies = match grant {
        Grant::Always => approvals.grant_always(session_id, &approval_id, actor, authority).await?,
        scope => {
            vec![approvals.grant(session_id, &approval_id, scope.scope(), actor, authority).await?]
        }
    };

    for reply in &replies {
        inbox
            .append(
                session_id,
                InboxKind::Wakeup,
                serde_json::json!({
                    "approval_id": reply.approval_id.as_str(),
                    "decision": reply.decision,
                    "scope": reply.scope,
                }),
                0,
                Actor::User,
            )
            .await?;
    }

    Ok(ApprovalReceipt { replies })
}

/// Deny a pending approval, then wake the turn that was waiting on it.
///
/// Unless `only` is set the denial cascades to every other pending approval in
/// the session: a person who says no has stopped trusting what the session is
/// attempting, and the remaining questions would otherwise keep asking them.
pub async fn deny_approval(
    approvals: &ApprovalService,
    inbox: &Inbox,
    session_id: &SessionId,
    approval_id: &str,
    only: bool,
    actor: &str,
) -> Result<ApprovalReceipt, CommandError> {
    let approval_id = ApprovalId::from_string(approval_id);
    let replies = if only {
        vec![approvals.deny(session_id, &approval_id, actor).await?]
    } else {
        approvals.deny_cascade(session_id, &approval_id, actor).await?
    };

    for reply in &replies {
        inbox
            .append(
                session_id,
                InboxKind::Wakeup,
                serde_json::json!({
                    "approval_id": reply.approval_id.as_str(),
                    "decision": reply.decision,
                }),
                0,
                Actor::User,
            )
            .await?;
    }

    Ok(ApprovalReceipt { replies })
}

/// A session's lifecycle, as `agent status` reads it.
///
/// `PartialEq` only: [`AdjudicationStatus`] carries the model's stated
/// confidence, and a float has no `Eq` to derive.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReport {
    pub open_run: Option<RunId>,
    pub open_turn: Option<TurnId>,
    pub open_step: Option<StepId>,
    /// Inbox items that are still pending, of any kind.
    pub pending: usize,
    /// The most recent instruction the session recorded, if any.
    ///
    /// A session whose logs predate the intent ledger answers `None` — that is
    /// "this log has no intents", not "the user asked for nothing".
    pub latest_intent_id: Option<IntentId>,
    /// The instruction the most recently settled turn answered.
    ///
    /// `None` when that turn is not bound to a recorded intent — it predates
    /// the ledger, or it answered a wakeup rather than a request.
    pub answered_intent_id: Option<IntentId>,
    /// Whether the answer on the table predates the instruction still standing.
    ///
    /// True when the newest instruction is still [`Active`] and a *different*
    /// instruction is the one the last turn answered: the session is working
    /// through something newer than the answer a reader would otherwise take
    /// for current. That is the 2026-11-01 shape in one boolean.
    ///
    /// [`Active`]: agent_protocol::intent::IntentStatus::Active
    pub stale_answer: bool,
    /// Queued items whose instruction no longer stands.
    ///
    /// A pending item bound to an intent that was superseded, withdrawn or
    /// discharged is work nobody is waiting for any more. The write path
    /// discards such items in the same append that displaces them, so this list
    /// is empty on any log this version wrote — it exists for a log written
    /// before that, where a reader has to be able to say "this queue entry is
    /// stale" rather than run it.
    pub stale_items: Vec<InboxItemId>,
    /// The most recent intent adjudication, when the session has one.
    ///
    /// `None` covers two real states this build cannot tell apart from the log
    /// alone — no adjudicator was configured, and every message the rules could
    /// route — and both mean the same thing to a reader: nothing was decided by
    /// a model here.
    pub intent_adjudication: Option<AdjudicationStatus>,
    /// What the session's latest run is doing; `None` when it never opened one.
    pub run_status: Option<RunStatus>,
    /// Whether the person received an answer to the latest request.
    pub delivery_status: DeliveryStatus,
    /// Whether the work has been checked, and whether the check is still fresh.
    pub verification_status: VerificationStatus,
    /// The derived completion claim. `None` means "nothing to claim" — not
    /// "complete": see [`agent_protocol::status::CompletionStatus`].
    pub completion_status: Option<CompletionStatus>,
    /// The instruction that still stands and is the newest of those.
    ///
    /// `None` means the session has no standing instruction: every one it
    /// recorded was answered, superseded or withdrawn, or its log predates the
    /// ledger. The newest instruction is [`Self::latest_intent_id`] even when
    /// it no longer stands.
    pub active_intent_id: Option<IntentId>,
    /// The instructions whose queue entries are still waiting, in queue order.
    ///
    /// A pending item with no instruction behind it (a wakeup, a log written
    /// before the ledger) contributes nothing: the list is about work a person
    /// asked for and nobody has picked up.
    pub pending_intents: Vec<IntentId>,
}

/// Read a session's lifecycle from the durable log.
pub async fn status(
    scheduler: &Scheduler,
    session_id: &SessionId,
) -> Result<StatusReport, CommandError> {
    let lifecycle = scheduler.lifecycle(session_id).await?;
    let pending = scheduler.inbox().pending(session_id).await?;
    let ledger = scheduler.intents();
    // One read of the ledger answers every intent question below; a second read
    // could see a state the first did not and report a session from two logs.
    let intents = ledger.all(session_id).await?;
    let latest = intents.last();
    let answered_intent_id = ledger.answered(session_id).await?;
    let latest_intent_id = latest.map(|intent| intent.intent_id.clone());
    // An instruction that is still standing and is not the one the last turn
    // answered means the answer on the table is older than the work. An
    // instruction that was superseded, withdrawn or discharged is not owed an
    // answer, so it cannot make an older one stale.
    let stale_answer = latest.is_some_and(|intent| intent.status == IntentStatus::Active)
        && answered_intent_id.is_some()
        && answered_intent_id != latest_intent_id;
    let active_intent_id = intents
        .iter()
        .rev()
        .find(|intent| intent.status == IntentStatus::Active)
        .map(|intent| intent.intent_id.clone());
    let standings: std::collections::HashMap<IntentId, IntentStatus> =
        intents.into_iter().map(|intent| (intent.intent_id, intent.status)).collect();
    // The instructions behind the items still waiting, in queue order. The
    // write path binds one instruction to one item, so this is a de-duplication
    // for a queue that was written by two callers rather than a merge.
    let mut pending_intents: Vec<IntentId> = Vec::new();
    for item in &pending {
        let Some(intent_id) = agent_kernel::inbox::item_intent_id(item) else {
            continue;
        };
        if !pending_intents.contains(&intent_id) {
            pending_intents.push(intent_id);
        }
    }
    let stale_items = pending
        .iter()
        .filter(|item| match agent_kernel::inbox::item_intent_id(item) {
            Some(intent_id) => standings.get(&intent_id) != Some(&IntentStatus::Active),
            None => false,
        })
        .map(|item| item.item_id.clone())
        .collect();
    let intent_adjudication = scheduler.intent_adjudication(session_id).await?;
    let axes = scheduler.status_axes(session_id).await?;
    Ok(StatusReport {
        open_run: lifecycle.open_run,
        open_turn: lifecycle.open_turn,
        open_step: lifecycle.open_step,
        pending: pending.len(),
        latest_intent_id,
        answered_intent_id,
        stale_answer,
        stale_items,
        intent_adjudication,
        run_status: axes.run_status,
        delivery_status: axes.delivery_status,
        verification_status: axes.verification_status,
        completion_status: axes.completion_status,
        active_intent_id,
        pending_intents,
    })
}
