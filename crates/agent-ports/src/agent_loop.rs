//! AgentLoop trait — the seam for how a turn progresses.
//!
//! The kernel's Scheduler owns turn/run lifecycle, single-active-run
//! enforcement, the step budget, and when a turn advances; an AgentLoop decides
//! what happens *inside* one step. The loop returns durable events and the
//! Scheduler appends them, so the loop never touches the EventStore directly.
//!
//! A turn is opened once and then advanced one step at a time, which is what
//! makes the boundary between two steps a place the kernel can act: input the
//! user injects while the turn runs reaches the loop there.

use std::any::Any;

use agent_protocol::events::EventDraft;
use agent_protocol::ids::{InboxItemId, RunId, SessionId, StepId, TurnId};
use agent_protocol::instructions::RunInstructions;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What a loop needs to open one turn.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub run_id: RunId,
    /// The inbox item payload that started this turn, e.g. `{"text": "..."}`.
    pub user_input: serde_json::Value,
    /// What this run recorded as its instructions, if the caller had a source.
    ///
    /// It reaches the loop here because `run/started` — which is where the
    /// snapshot becomes durable — is appended only after `begin_turn` returns.
    /// The loop places the message the way it places this turn's own input:
    /// with a uid derived from the run, so the fold that reads the log later
    /// rebuilds the same message rather than a copy of it.
    pub instructions: Option<RunInstructions>,
    /// What the world looked like when this turn opened.
    ///
    /// `None` is a real state: a harness that has no clock, queue, or previous
    /// turn to speak of. The loop then places no state or task layer rather
    /// than inventing "0 queued" or a made-up time. The Scheduler, which has
    /// all of these facts, always supplies them.
    pub state: Option<TurnStateFacts>,
    /// What this run can do, as the host assembled it.
    ///
    /// `None` means the caller has no capability facts — a harness that built
    /// no sandbox — and the loop places no capability layer rather than telling
    /// the model it can do nothing. The Scheduler supplies them when the app
    /// layer configured a stack.
    pub capability: Option<TurnCapabilityFacts>,
    /// The session's plan ledger as of this turn's opening.
    ///
    /// `None` means the caller keeps no ledger at all and the loop places no
    /// plan section. `Some(Plan::default())` is a caller that does keep one
    /// and has nothing recorded yet — the loop then says so, which is what
    /// tells the model the ledger exists and it may write to it.
    pub plan: Option<agent_protocol::plan::Plan>,
    /// The session's judgment record as of this turn's opening.
    ///
    /// Same convention as `plan`: `None` means the caller keeps no record and
    /// the loop places no judgment section; `Some(Judgment::default())` is a
    /// caller that keeps one and has nothing recorded yet.
    pub judgment: Option<agent_protocol::judgment::Judgment>,
    /// A failure from an earlier turn that this turn opens on.
    ///
    /// Only a turn resumed after an approval supplies one: the held call runs
    /// in its own turn, may fail there, and the turn that then answers is a
    /// new one. The caller reads the failure back from the log so the loop's
    /// gate covers it, instead of the resumed turn opening as if nothing had
    /// happened. `None` for every ordinary turn.
    pub pending_failure: Option<TurnFailureFacts>,
}

/// A tool failure the turn before this one left unreconciled.
///
/// The loop re-checks any reconcile against `tool`, so the record it reads
/// back must name the call that actually failed rather than a convenient one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailureFacts {
    pub call_id: String,
    pub tool: String,
    pub detail: String,
}

/// Facts about the moment a turn opens.
///
/// These are not history: they describe the world around the conversation,
/// which a trimmed transcript cannot be trusted to carry. The loop renders
/// them into the per-turn state layer and task layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateFacts {
    /// The turn's opening time, on the boundary the run started at.
    pub started_at: DateTime<Utc>,
    /// Turn-starting inputs still queued behind this one, not counting the
    /// item this turn is opening on. Steering is not counted: it enters this
    /// turn as its own user message rather than waiting for a later turn.
    pub queued_inputs: usize,
    /// Whether this turn was opened by a queued request or by a schedule.
    pub input_kind: TurnInputKind,
    /// How the most recent turn that ended ended, if any turn has ended.
    pub previous_turn: Option<TurnEnding>,
    /// The decision word of the approval this turn continues, when it opened
    /// on a resume (`granted`, `denied`, ...). `None` for every ordinary turn.
    ///
    /// It rides the inbox item rather than the log because the item is what
    /// carries `resumed: true` already; the posture needs the word, not the
    /// settlement.
    pub resumed: Option<String>,
    /// The most steps this run may take, from the run's configuration.
    pub max_steps: u32,
}

/// What opened a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputKind {
    /// An inbox item the user submitted.
    User,
    /// A scheduled item whose due time arrived.
    Scheduled,
}

/// The execution boundary and reach this run was assembled with.
///
/// These are host facts, not permissions: the policy decides whether a tool may
/// run, while this says what the backend would allow at all. The capability
/// layer states them before the model tries something the backend will refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCapabilityFacts {
    pub sandbox: crate::execution_environment::SandboxMode,
    /// Whether a command run through the sandbox may reach the network.
    pub network_allowed: bool,
}

/// How a turn that ended ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnding {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    /// Paused on a decision. A session in this state cannot start another
    /// turn until `resume` settles the call, but the fold still reports it
    /// rather than pretending the turn vanished.
    Yielded,
}

/// Text the user injected between two steps of a turn already in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringInput {
    /// The inbox item this text came from, so the message the loop builds can
    /// carry an identity a rebuild from the log would produce again.
    pub item_id: InboxItemId,
    pub text: String,
}

/// A turn in progress, as the loop that owns it sees it.
///
/// Opaque to the kernel on purpose. The Scheduler owns *when* a turn advances;
/// it has no business reading the conversation, the route, or the tool manifest
/// that the turn carries from one step to the next. Threading those through the
/// kernel would make it a second, silent owner of loop state — and the first
/// thing to drift out of step with the loop's own copy.
pub struct TurnState(Box<dyn Any + Send + Sync>);

impl TurnState {
    pub fn new<T: Any + Send + Sync>(state: T) -> Self {
        Self(Box::new(state))
    }

    /// The loop's own state, or `None` if this turn belongs to another loop.
    pub fn get_mut<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        self.0.downcast_mut::<T>()
    }
}

impl std::fmt::Debug for TurnState {
    /// Deliberately says nothing about the contents: a state that printed its
    /// conversation would put model-visible text into every debug log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnState(..)")
    }
}

/// A turn that has been opened but has not taken its first step.
#[derive(Debug)]
pub struct TurnStart {
    pub state: TurnState,
    /// Turn-level events produced before any step exists — the route decision
    /// is taken here, and it belongs to the turn rather than to a step.
    pub events: Vec<EventDraft>,
}

/// What one step produced.
#[derive(Debug)]
pub struct StepOutcome {
    /// The step that just ran, so the caller can name the boundary after it.
    pub step_id: StepId,
    pub events: Vec<EventDraft>,
    /// `Some` when the turn is over, with the reason the loop gives. `None`
    /// means the turn could continue — whether it does is the caller's call,
    /// because the step budget is the caller's policy, not the loop's.
    pub stop: Option<StopReason>,
    pub final_content: Option<String>,
}

/// Why a turn stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    FinalAnswer,
    /// The step budget ran out. The loop never stops for this reason — the
    /// budget belongs to the caller, and the kernel's `Ending` translates an
    /// exhausted budget into this reason (YA-48).
    MaxSteps,
    NeedsUserInput,
    /// A model attempt failed and the turn could not continue. The attempt's
    /// failure event is part of the turn outcome, not an error the caller drops.
    FatalError,
    /// The attempt was cancelled, or the user interrupted the turn at a step
    /// boundary. M5 reaches the first when an adapter aborts its own request and
    /// the second through `Inbox::interrupt`.
    UserCancelled,
}

/// Port for how a turn advances. Implementations are assembled at the app layer.
#[async_trait::async_trait]
pub trait AgentLoop: Send + Sync {
    /// Open a turn: derive history, choose a route, freeze the tool set. No
    /// model request has been made when this returns.
    ///
    /// A failure here is the one that must not consume the caller's input: the
    /// turn never started, so nothing about it may be durable.
    async fn begin_turn(&self, ctx: &TurnContext) -> Result<TurnStart, AgentLoopError>;

    /// Advance the turn by exactly one step.
    ///
    /// `steering` is what the user injected at the boundary this step begins
    /// at; it is empty for a step no one steered. The loop decides how the text
    /// enters the conversation, and records that it did.
    async fn step(
        &self,
        turn: &mut TurnState,
        steering: &[SteeringInput],
    ) -> Result<StepOutcome, AgentLoopError>;
}

/// Errors from an AgentLoop implementation.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("model provider error: {0}")]
    Model(String),
    /// The turn could not start because its history could not be derived. No
    /// event is written: nothing was attempted, and the caller's input stays
    /// pending rather than being consumed by a turn that never ran.
    #[error("context engine error: {0}")]
    Context(#[from] crate::context_engine::ContextEngineError),
    #[error("loop payload error: {0}")]
    Payload(#[from] serde_json::Error),
    /// The turn state handed back was not produced by this loop. Fail closed
    /// rather than start a step on a conversation this loop cannot see.
    #[error("turn state does not belong to this loop")]
    StateNotOwned,
}
