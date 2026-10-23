//! ControlHandler — one change to the running system, applied at a safe
//! boundary.
//!
//! Reload here is not hot replacement. The design names the boundaries at which
//! configuration may change — after a turn, a step, a run, a settled provider
//! request — and says outright that compiled-in Rust trait implementations are
//! not safely replaceable at all. So this port is deliberately shaped as "one
//! change, applied once, at a boundary" rather than as a live registry something
//! can mutate at any moment. A caller cannot ask for a change to take effect
//! now, because "now" is the one moment at which it must not.
//!
//! ## Why the queue is the Inbox
//!
//! A control arrives from wherever input arrives — another process, a later
//! command, a person — and the Inbox is already this system's durable queue for
//! that. It is also already drained at the boundary where a change can be
//! applied without tearing a turn's batch of events apart. Nothing here defines
//! a second queue.
//!
//! ## The two ways a control does not take effect
//!
//! **Nobody handles the name.** The scheduler asks every handler what it
//! handles ([`ControlHandler::handles`], fail closed by default) and refuses a
//! name no one claims, by name — the same rule the subagent wire uses: unknown
//! *data* is tolerated, a disagreement about *meaning* is not. A control left
//! pending because no one recognised it would be a command that looks accepted
//! and does nothing.
//!
//! **A handler recognises it and says no.** That is [`ControlOutcome::Refused`]:
//! a decision, taken deliberately, with a reason. It is not an error, and it is
//! not silence — a control that claimed a plugin which is not registered has to
//! come back as a sentence, not as an item that quietly stopped being pending.

use agent_protocol::ids::{InboxItemId, SessionId};
use serde_json::Value;

/// One control, as it was asked for.
///
/// The name is a string rather than an enum because the set of names is open:
/// a newer writer may append a control this version has never heard of, and the
/// answer to that must be a refusal naming it, not a deserialization failure.
/// What this version does check is that the name is one somebody claims.
#[derive(Debug, Clone)]
pub struct ControlRequest {
    /// The session the item was appended to. A control acts on a session's
    /// runtime, and the one it arrived on is the only one it may touch.
    pub session_id: SessionId,
    /// The item that carried it. Goes into the record, so "who asked" and "what
    /// happened" can be joined by a reader who was not there.
    pub item_id: InboxItemId,
    /// What the item asks for, e.g. `plugin.disable`.
    pub control: String,
    /// The rest of the item's payload. Read by the handler that claims the
    /// name; nothing here interprets it.
    pub payload: Value,
}

/// What applying a control did.
///
/// Both answers are outcomes, not errors: a refusal is a handler doing its job.
/// The distinction from [`ControlError`] matters because they are recorded
/// differently — a refusal is a decision about a request, an error is the
/// request not being answerable at all.
#[derive(Debug, Clone)]
pub enum ControlOutcome {
    /// It happened; `detail` says what, in one line, for the record.
    Applied { detail: String },
    /// It did not happen, and the reason is worth reading.
    Refused { reason: String },
}

/// Why a control could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("control '{control}' could not be applied: {message}")]
    Failed { control: String, message: String },
}

/// Port for applying one change at a safe boundary.
///
/// Implementations are assembled by the application, because only the
/// application knows what it built and therefore holds the handles a change
/// needs. A change to the plugin surface is the first of these; the other
/// reloadable things the design names (instructions, skills, a rebuilt tool
/// manifest) belong here too, and each adds a name rather than a second port.
#[async_trait::async_trait]
pub trait ControlHandler: Send + Sync {
    /// What this handler is called in records and refusals.
    fn name(&self) -> &str;

    /// Which control names this handler applies.
    ///
    /// Fail closed by default, for the reason every seam in this crate is: a
    /// handler that has not said it handles something is taken not to, so
    /// forgetting to declare a name disables nothing that was working.
    fn handles(&self) -> Vec<String> {
        Vec::new()
    }

    /// Apply `request`, or say why not.
    ///
    /// Called at a boundary, once per item: an implementation is not asked
    /// whether it wants to be called again later, and a request it has already
    /// answered is not re-offered. It therefore must not be depended on for
    /// anything that has to happen repeatedly.
    async fn apply(&self, request: ControlRequest) -> Result<ControlOutcome, ControlError>;
}
