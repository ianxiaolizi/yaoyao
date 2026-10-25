//! The seam between the HTTP layer and the process that can actually run work.
//!
//! `agent-web` knows how to speak HTTP and how to fold a session log. It does not
//! know how to assemble a model, a sandbox or an MCP server — that is
//! `apps/agent`'s job, and keeping it there is what stops the GUI from growing a
//! second, subtly different runtime. A [`Backend`] is the small set of verbs the
//! page may ask for; the binary implements it over the same command functions
//! the terminal uses.
//!
//! Live text is pushed through a [`LiveSink`] instead of being part of a return
//! value. A turn can take minutes, and the HTTP response that accepted it has
//! already gone out; the deltas need somewhere to go while it runs. The sink is
//! a broadcast channel so an SSE connection that starts a moment later still
//! joins the same run, and a connection that is gone costs nothing but a failed
//! `send`.

use agent_kernel::model_selection::ModelChoice;
use agent_protocol::ids::SessionId;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// One request for a person's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow this one call.
    Once,
    /// Allow this kind of call until the turn ends.
    Turn,
    /// Allow this kind of call for the rest of the session.
    Session,
    /// Refuse this call, and let the turn continue with the refusal.
    Deny,
}

impl Decision {
    /// Read the word the API accepts.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "once" => Some(Self::Once),
            "turn" => Some(Self::Turn),
            "session" => Some(Self::Session),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// The word used in the durable reply and in the response body.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Turn => "turn",
            Self::Session => "session",
            Self::Deny => "deny",
        }
    }

    /// The state an approval row ends in after this decision.
    pub fn approval_state(self) -> &'static str {
        match self {
            Self::Deny => "denied",
            Self::Once | Self::Turn | Self::Session => "granted",
        }
    }
}

/// What kind of text a live chunk carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    /// Assistant prose.
    Text,
    /// The model's exposed reasoning.
    Reasoning,
    /// A note from the host, not from the model.
    Notice,
}

/// A preview that never enters the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveChunk {
    pub kind: LiveKind,
    pub text: String,
    /// The step the text belongs to, when the host knows it. The loop's live
    /// events do not carry a step id today, so this is usually `None`; the page
    /// places the text at the live tail instead of inventing a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

impl LiveChunk {
    pub fn text(text: impl Into<String>) -> Self {
        Self { kind: LiveKind::Text, text: text.into(), step: None }
    }

    pub fn reasoning(text: impl Into<String>) -> Self {
        Self { kind: LiveKind::Reasoning, text: text.into(), step: None }
    }

    pub fn notice(text: impl Into<String>) -> Self {
        Self { kind: LiveKind::Notice, text: text.into(), step: None }
    }
}

/// The channel a running turn publishes previews on.
pub type LiveSink = broadcast::Sender<LiveChunk>;

/// Why a command could not be carried out.
///
/// This is deliberately not an HTTP status: the same refusal can be a `409` on
/// one endpoint and a `500` on another. The handler owns that mapping; the
/// backend owns the reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct BackendError {
    pub code: String,
    pub message: String,
}

impl BackendError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self::new("busy", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    pub fn not_pending(message: impl Into<String>) -> Self {
        Self::new("approval_not_pending", message)
    }
}

/// The verbs the GUI may perform.
///
/// A backend is expected to serialize work on one session itself: the web layer
/// can keep another request out while a turn is visibly running, but it cannot
/// see a resume that is already inside the kernel. Implementations should hold
/// one lock per session across the whole operation.
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    /// Queue `text` as a turn and drive it to its first stopping point.
    async fn run_turn(
        &self,
        session: &SessionId,
        text: &str,
        live: &LiveSink,
    ) -> Result<(), BackendError>;

    /// Record a decision and carry the yielded turn forward.
    async fn settle_approval(
        &self,
        session: &SessionId,
        approval_id: &str,
        decision: Decision,
        live: &LiveSink,
    ) -> Result<(), BackendError>;

    /// Carry a yielded turn forward without a fresh decision.
    ///
    /// Used when the wait ended on its own (its pending approval expired).
    /// `Resume` reads the durable status and records the denial it derives, then
    /// queues the continuation; the caller does not get to invent a decision.
    async fn resume(&self, session: &SessionId, live: &LiveSink) -> Result<(), BackendError>;

    /// Derive a new session from a closed turn boundary.
    ///
    /// `Ok(None)` means the log has no ready boundary; that is an answer, not a
    /// failure. The caller gets the child id so it can open it immediately.
    async fn fork(
        &self,
        session: &SessionId,
        at: Option<&str>,
        reason: &str,
    ) -> Result<Option<SessionId>, BackendError>;

    /// Queue a steering note for the turn in flight.
    async fn steer(&self, session: &SessionId, text: &str) -> Result<(), BackendError>;

    /// Ask the turn in flight to stop. `Ok(false)` means there was nothing to
    /// stop; that is an answer, not a failure.
    async fn interrupt(&self, session: &SessionId) -> Result<bool, BackendError>;

    /// Choose the provider and model this session's next turns should use.
    ///
    /// Not a turn: nothing runs, and a turn already in flight keeps the provider
    /// it was assembled with. The answer is the choice as it was recorded, so
    /// the page can label itself from the log instead of from what it hoped it
    /// selected.
    async fn set_model(
        &self,
        session: &SessionId,
        provider: &str,
        model: Option<&str>,
    ) -> Result<ModelChoice, BackendError>;

    /// Remove a session's log for good.
    ///
    /// Goes through the driver rather than straight to the store because the
    /// driver is what knows a session may have a live stack behind it — MCP child
    /// processes that belong to this session and must not outlive it — and
    /// because only the driver can tell that a turn of its own is running and
    /// refuse.
    async fn delete_session(&self, session: &SessionId) -> Result<(), BackendError>;
}
