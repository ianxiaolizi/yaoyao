//! ModelRouter port — the seam for choosing which model serves a turn.
//!
//! Routing is a decision made *before* an attempt, from facts the caller
//! already has: the task, the context revision, what the request needs. It is
//! not an attempt and it has no I/O, so the trait is synchronous — making it
//! `async` would be pretending a rule table is a network call.
//!
//! `route` cannot fail. A router that can refuse to produce a route would make
//! "the router is unavailable" a way for a turn to die, and there is always a
//! defensible answer: the default model. Degradation belongs in the table the
//! implementation holds, not in the caller's error path.

use agent_protocol::ids::{EventSeq, SessionId, TurnId};
use serde::{Deserialize, Serialize};

pub use agent_protocol::model::{ModelBudget, ModelCapability, ModelRoute, ModelTier};

/// What kind of work a turn is (the "task type").
///
/// An open set: a profile may name a task this build has no constant for, and
/// the router is free to fall back to its default route.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskClass(pub String);

impl TaskClass {
    /// A user talking to the agent.
    pub const INTERACTIVE: &'static str = "interactive";
    /// Condensing history or a document.
    pub const SUMMARIZE: &'static str = "summarize";
    /// Reviewing a change.
    pub const REVIEW: &'static str = "review";

    /// Create a task class from its wire string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a router is asked to route.
///
/// This is the router's whole input. Note what is *not* here: message content.
/// A router picks a model from the shape of the request, so a profile or a
/// budget cannot leak the conversation into a routing decision — and a
/// fast-decision backend that wants content reads it from the state the caller
/// builds, which is a deliberate act rather than a default.
#[derive(Debug, Clone)]
pub struct RouteRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub task: TaskClass,
    /// The log position the turn's history was derived from. The same
    /// [`EventSeq`] the context engine reports, not a bare number: one log
    /// position should have one type wherever it crosses a port.
    pub context_revision: EventSeq,
    /// How many messages the first request will carry.
    pub message_count: usize,
    /// What the attempt will require of the provider.
    pub required_capabilities: Vec<ModelCapability>,
    pub budget: ModelBudget,
}

/// Port for choosing a route.
pub trait ModelRouter: Send + Sync {
    /// The route for one turn. Always succeeds; see the module note.
    fn route(&self, request: &RouteRequest) -> ModelRoute;
}
