//! ContextEngine trait — the seam for deriving model-visible history.
//!
//! A model may only see what is already durable, so the messages of a request
//! are not assembled by hand: they are derived from the session log. This port
//! is how the loop asks for that history without knowing where it comes from —
//! the loop depends on this trait, never on a projection or a store.
//!
//! What is derived here is the *settled* prefix of a session: everything up to
//! the caller's own in-flight turn. The Scheduler writes `turn/started` only
//! after the loop returns (so a loop failure leaves the inbox item pending), so
//! the current turn's input is the caller's to place.

use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::model::{MessageRole, ModelMessage};
use serde::{Deserialize, Serialize};

/// What a derivation did to fit the history into the configured window.
///
/// Reported alongside the messages rather than inferred by the loop: only the
/// engine knows what it dropped, and a request should record that number even
/// when nothing was dropped. All-zero with `None` limits is the "no window
/// configured" state — the measured estimate is still filled in, because it is
/// the value that tells an operator how close a session is to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextUsage {
    /// Messages from the settled prefix that were left out of `messages`.
    pub dropped_messages: usize,
    /// Estimated tokens of the messages that were kept.
    pub estimated_tokens: usize,
    /// The configured ceiling, if one was.
    pub max_input_tokens: Option<u32>,
    /// The part of the ceiling set aside for the answer, if a policy was set.
    pub reserved_output_tokens: Option<u32>,
}

/// One kind of content in the model-visible context.
///
/// The split follows what a reader needs to answer "is this structure doing its
/// job": run instructions, the per-turn notice, conversation, tool traffic, and
/// the fixed per-message framing. The instructions are one category because
/// they are still one message: the four-layer frame (cognition, capability,
/// state, task) is not separated in the log yet. When it is, this enum grows a
/// category per layer rather than a second counting rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextCategoryKind {
    /// The leading `run/started` instructions.
    Instructions,
    /// The per-turn budget reminder (#40), which is not history.
    Notice,
    /// A system message that is neither of the two above.
    SystemOther,
    /// User-authored text.
    UserText,
    /// Assistant-authored text.
    AssistantText,
    /// Text carried by a tool-role message.
    ToolText,
    /// A tool proposal.
    ToolCalls,
    /// A tool outcome that succeeded.
    ToolResultsOk,
    /// A tool outcome that failed.
    ToolResultsFailed,
    /// The fixed tokens every message pays for its own framing.
    Framing,
}

impl ContextCategoryKind {
    /// Every category, in report order. The report always carries all of them,
    /// including the empty ones, so a consumer can read a fixed shape.
    pub const ALL: [Self; 10] = [
        Self::Instructions,
        Self::Notice,
        Self::SystemOther,
        Self::UserText,
        Self::AssistantText,
        Self::ToolText,
        Self::ToolCalls,
        Self::ToolResultsOk,
        Self::ToolResultsFailed,
        Self::Framing,
    ];

    /// The wire and report string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::Notice => "notice",
            Self::SystemOther => "system-other",
            Self::UserText => "user-text",
            Self::AssistantText => "assistant-text",
            Self::ToolText => "tool-text",
            Self::ToolCalls => "tool-calls",
            Self::ToolResultsOk => "tool-results-ok",
            Self::ToolResultsFailed => "tool-results-failed",
            Self::Framing => "framing",
        }
    }

    /// Its position in [`Self::ALL`].
    pub fn index(self) -> usize {
        match self {
            Self::Instructions => 0,
            Self::Notice => 1,
            Self::SystemOther => 2,
            Self::UserText => 3,
            Self::AssistantText => 4,
            Self::ToolText => 5,
            Self::ToolCalls => 6,
            Self::ToolResultsOk => 7,
            Self::ToolResultsFailed => 8,
            Self::Framing => 9,
        }
    }
}

/// What one category cost.
///
/// `messages` counts the messages that carry at least one block of this kind;
/// one message can carry several kinds (text plus a tool call), so these counts
/// may sum past the message total. `blocks` counts the blocks themselves. The
/// token counts are the trimmer's estimate, block by block, so the categories
/// plus [`ContextCategoryKind::Framing`] add up to the derivation's estimate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCategoryUsage {
    pub kind: ContextCategoryKind,
    pub messages: usize,
    pub blocks: usize,
    pub estimated_tokens: usize,
}

/// One of the largest messages in the context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargestMessage {
    pub message_uid: String,
    pub role: MessageRole,
    /// The distinct content kinds this message carries, in report order.
    pub kinds: Vec<ContextCategoryKind>,
    pub estimated_tokens: usize,
    /// A one-line, character-bounded preview. Never the whole message.
    pub preview: String,
}

/// The shape of one derived context, measured but not changed.
///
/// This is a read model over [`ContextUsage`] and the messages it describes. It
/// answers "what is in the context, how much is each part, how much room is
/// left, and what are the biggest items" without inventing a second estimate:
/// every number comes from the same `estimate_message` the trimmer uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsageReport {
    /// Messages in the derived context, as handed to the model.
    pub message_count: usize,
    /// Settled messages the configured window left out.
    pub dropped_messages: usize,
    /// The trimmer's estimate for the messages that remain.
    pub estimated_tokens: usize,
    pub max_input_tokens: Option<u32>,
    pub reserved_output_tokens: Option<u32>,
    /// `max_input_tokens - reserved_output_tokens`, when a window is set.
    pub budget_tokens: Option<usize>,
    /// Room left under the usable budget; zero when the newest group overflowed.
    pub remaining_tokens: Option<usize>,
    /// Estimated share of the usable budget, as a whole percent.
    pub percent_used: Option<u32>,
    pub categories: Vec<ContextCategoryUsage>,
    pub largest_messages: Vec<LargestMessage>,
}

/// Which part of a request a compaction rewrites.
///
/// The engine proposes settled history once per turn; the loop may compact its
/// own in-flight step traffic once when a turn outgrows the window on its own.
/// The scope is recorded so a reader can tell the two apart in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionScope {
    /// History from turns that have already settled.
    Settled,
    /// This turn's own step traffic, kept in flight.
    InFlight,
}

impl CompactionScope {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::InFlight => "in-flight",
        }
    }
}

/// Older history the engine proposes summarizing.
///
/// The engine decides *what* to summarize because only it sees the untrimmed
/// view and owns the estimate; the loop decides *whether* to ask, because only
/// it can make the model request. A candidate is a proposal, not a fact: a
/// failed or empty summary leaves the history exactly as it was.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionCandidate {
    /// The whole atomic groups in the prefix, oldest first. They are the input
    /// to the summary request; a tool call and its result are never split
    /// across the boundary.
    pub messages: Vec<ModelMessage>,
    /// The trimmer's estimate for `messages`.
    pub estimated_tokens: usize,
    /// The estimate that remains once `messages` are replaced by the summary.
    pub retained_tokens: usize,
    /// The usable budget the proposal was made against.
    pub budget_tokens: usize,
    /// The provider's own input count for the request that crossed the trigger,
    /// when it reported one. `None` means the proposal was made on the estimate
    /// alone — the trigger fired on a provider with no usage reporting, or the
    /// loop proposed a compaction of its own in-flight traffic.
    pub observed_input_tokens: Option<u32>,
    /// Whether the proposal covers settled history or this turn's in-flight
    /// traffic.
    pub scope: CompactionScope,
}

/// Model-visible history derived from durable state.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedContext {
    /// Messages belonging to settled turns, in event order.
    pub messages: Vec<ModelMessage>,
    /// The durable log position the messages were derived from. It is the
    /// request's `context_revision`: a request can be traced back to the exact
    /// log prefix it saw ("record the source and version").
    ///
    /// An empty log has no position, and this is then [`EventSeq::ZERO`]. That
    /// is a stated value, not a default: a derivation over nothing is a real
    /// outcome, and the README-level contract "revision names the prefix the
    /// request saw" stays true at zero.
    pub revision: EventSeq,
    /// What the derivation kept and dropped.
    pub usage: ContextUsage,
    /// The provider's own input count for the most recent settled request in
    /// the session, when the log records one. It is what the compaction
    /// trigger is measured against: an estimate describes only the messages
    /// this derivation produced, while a real request also carries the stable
    /// head, the per-turn layers, and the tool definitions.
    pub observed_input_tokens: Option<u32>,
    /// A proposal to summarize older history, when a compaction trigger is
    /// configured and the trigger measure has reached it. `None` is the whole
    /// story for an engine with no trigger: there is nothing to ask the model.
    pub compaction: Option<CompactionCandidate>,
}

/// Errors from a ContextEngine implementation.
#[derive(Debug, thiserror::Error)]
pub enum ContextEngineError {
    /// Derivation failed. The message is deliberately opaque: the port must not
    /// name the mechanism behind it (a projection, a store, a cache).
    #[error("context derivation failed: {0}")]
    Derivation(String),
}

/// Port for deriving the model's history from durable state.
#[async_trait::async_trait]
pub trait ContextEngine: Send + Sync {
    /// Derive the settled history of one session.
    async fn derive(&self, session_id: &SessionId) -> Result<DerivedContext, ContextEngineError>;
}
