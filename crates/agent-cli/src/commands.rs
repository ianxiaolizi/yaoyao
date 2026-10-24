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

use agent_kernel::approval::{ApprovalError, ApprovalService};
use agent_kernel::inbox::{Inbox, InboxError};
use agent_kernel::scheduler::{RunReport, Scheduler, SchedulerError};
use agent_protocol::events::Actor;
use agent_protocol::ids::{ApprovalId, InboxItemId, RunId, SessionId, StepId, TurnId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::permissions::{ApprovalReply, ApprovalScope};
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
    Approval(#[from] ApprovalError),
    /// The command's own argument is wrong. Nothing was written.
    #[error("{0}")]
    Invalid(String),
}

/// What starting a turn did.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnStart {
    /// A turn ran to its end (which may be a yield for approval).
    Ran(RunReport),
    /// Nothing ran: a run is already active, or there was no pending input.
    NotStarted,
}

/// Queue text for a turn and drive it.
///
/// The append is a separate fact from the run: if the run cannot start, the
/// text is still in the inbox, exactly as `agent ask` leaves it today.
pub async fn start_turn(
    scheduler: &Scheduler,
    session_id: &SessionId,
    text: &str,
    now: DateTime<Utc>,
) -> Result<TurnStart, CommandError> {
    scheduler
        .inbox()
        .append(
            session_id,
            InboxKind::NextTurn,
            serde_json::json!({ "text": text }),
            0,
            Actor::User,
        )
        .await?;

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
) -> Result<ApprovalReceipt, CommandError> {
    let approval_id = ApprovalId::from_string(approval_id);
    let replies = match grant {
        Grant::Always => approvals.grant_always(session_id, &approval_id, actor).await?,
        scope => vec![approvals.grant(session_id, &approval_id, scope.scope(), actor).await?],
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub open_run: Option<RunId>,
    pub open_turn: Option<TurnId>,
    pub open_step: Option<StepId>,
    /// Inbox items that are still pending, of any kind.
    pub pending: usize,
}

/// Read a session's lifecycle from the durable log.
pub async fn status(
    scheduler: &Scheduler,
    session_id: &SessionId,
) -> Result<StatusReport, CommandError> {
    let lifecycle = scheduler.lifecycle(session_id).await?;
    let pending = scheduler.inbox().pending(session_id).await?;
    Ok(StatusReport {
        open_run: lifecycle.open_run,
        open_turn: lifecycle.open_turn,
        open_step: lifecycle.open_step,
        pending: pending.len(),
    })
}
