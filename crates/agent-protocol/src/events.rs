//! Event drafts, stored events, event type namespace, and live events.

use crate::ids::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Who produced an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Agent,
    Tool,
    Plugin,
    System,
    Scheduler,
}

/// A namespaced event type, such as `"turn/started"` or `"model/attempt.failed"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventType(pub String);

impl EventType {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Core lifecycle event type names. Plugin and future events may extend this
/// namespace with their own strings; unknown types are handled via `ignorable`.
impl EventType {
    pub const SESSION_STARTED: &'static str = "session/started";
    pub const SESSION_FORKED: &'static str = "session/forked";
    /// The configuration a run executes under, recorded once as the run opens
    /// Written before `run/started`, because the configuration was
    /// resolved before there was a run to open.
    ///
    /// A reader compares two runs by this snapshot rather than by the config
    /// files, which may since have changed — and a fork copies it into its
    /// header rather than re-deriving it, for the same reason.
    pub const PROFILE_RESOLVED: &'static str = "profile/resolved";
    pub const RUN_STARTED: &'static str = "run/started";
    pub const RUN_YIELDED: &'static str = "run/yielded";
    pub const RUN_COMPLETED: &'static str = "run/completed";
    pub const RUN_FAILED: &'static str = "run/failed";
    pub const RUN_CANCELLED: &'static str = "run/cancelled";
    pub const RUN_INTERRUPTED: &'static str = "run/interrupted";
    pub const TURN_STARTED: &'static str = "turn/started";
    pub const TURN_STEERED: &'static str = "turn/steered";
    pub const TURN_YIELDED: &'static str = "turn/yielded";
    /// A yielded turn was carried forward once its wait ended (an approval was
    /// decided, an input arrived). Distinct from `turn/started`, because the
    /// turn being opened is not the one that waited, and distinct from a
    /// terminal event, because the turn that waited never reached an outcome of
    /// its own — it stopped and was picked up again.
    pub const TURN_RESUMED: &'static str = "turn/resumed";
    pub const TURN_COMPLETED: &'static str = "turn/completed";
    pub const TURN_FAILED: &'static str = "turn/failed";
    pub const TURN_INTERRUPTED: &'static str = "turn/interrupted";
    pub const STEP_STARTED: &'static str = "step/started";
    pub const STEP_COMPLETED: &'static str = "step/completed";
    pub const STEP_FAILED: &'static str = "step/failed";
    pub const STEP_INTERRUPTED: &'static str = "step/interrupted";
    /// The inbox lifecycle this build writes: an item is appended, then claimed
    /// or discarded — those three and no others. `inbox/promoted`,
    /// `inbox/updated` and `inbox/wakeup` were declared here from the start and
    /// nothing ever wrote them (YA-38); a wakeup is an *item kind*
    /// (`InboxKind::Wakeup`, written by `agent approve`), not an event of its
    /// own, and `InboxItemStatus` has no `Promoted` for a promotion event to
    /// move anything to.
    pub const INBOX_APPENDED: &'static str = "inbox/appended";
    pub const INBOX_CLAIMED: &'static str = "inbox/claimed";
    pub const INBOX_DISCARDED: &'static str = "inbox/discarded";
    pub const TURN_CANCELLED: &'static str = "turn/cancelled";
    /// The tool definitions this turn offers, frozen once before the first
    /// request. `model/request.prepared` says which manifest was sent; this
    /// event says exactly what was in it (descriptions and schemas included),
    /// which the manifest id alone cannot answer once the process is gone.
    pub const MODEL_TOOLS_FROZEN: &'static str = "model/tools.frozen";
    pub const MODEL_REQUEST_PREPARED: &'static str = "model/request.prepared";
    pub const MODEL_ATTEMPT_STARTED: &'static str = "model/attempt.started";
    pub const MODEL_ATTEMPT_FAILED: &'static str = "model/attempt.failed";
    pub const MODEL_ATTEMPT_CANCELLED: &'static str = "model/attempt.cancelled";
    pub const MODEL_RESPONSE_COMPLETED: &'static str = "model/response.completed";
    /// Live-only stream types. They carry no durable seq and never take part in
    /// replay; only the settlement event above becomes durable truth.
    pub const MODEL_STREAM_STARTED: &'static str = "model/stream.started";
    pub const MODEL_STREAM_DELTA: &'static str = "model/stream.delta";
    pub const MODEL_STREAM_TOOL_CALL_DELTA: &'static str = "model/stream.tool_call_delta";
    pub const MODEL_STREAM_REASONING_DELTA: &'static str = "model/stream.reasoning_delta";
    pub const TOOL_STARTED: &'static str = "tool/started";
    pub const TOOL_COMPLETED: &'static str = "tool/completed";
    pub const TOOL_FAILED: &'static str = "tool/failed";
    pub const TOOL_CANCELLED: &'static str = "tool/cancelled";
    pub const TOOL_INTERRUPTED: &'static str = "tool/interrupted";
    pub const TOOL_OUTCOME_UNKNOWN: &'static str = "tool/outcome_unknown";
    /// A call the model proposed that was never dispatched: there is no
    /// `tool/started` for it, so it provably did not run and re-issuing it is
    /// not a retry. Every other unsettled code says the opposite — something
    /// may have happened that the log does not know about.
    pub const TOOL_NOT_STARTED: &'static str = "tool/not_started";
    /// The session's plan, replaced in full. Written by the loop's own
    /// `plan.update` tool — one event per accepted call, carrying the plan it
    /// recorded. It never reaches a provider; the next turn reads it back.
    pub const PLAN_UPDATED: &'static str = "plan/updated";
    /// The session's judgment record, replaced in full by the merged snapshot.
    /// Written by the loop's own `judgment.update` tool — one event per accepted
    /// stage, carrying the whole record after the merge. Like the plan it never
    /// reaches a provider; the next turn reads it back.
    pub const JUDGMENT_UPDATED: &'static str = "judgment/updated";
    /// The loop refused to end the turn on a final answer: a real failure had
    /// not been reconciled, a recorded retry had not been made, or the answer
    /// restated a read from an earlier turn without looking again. It is also
    /// written when a step was held instead of run — an identical call about to
    /// run a third time. The payload names the call involved and carries the
    /// text the model was given instead of ending.
    pub const JUDGMENT_REQUIRED: &'static str = "judgment/required";
    /// A model request asking for a handover summary of older history. Written
    /// by the loop at the first step of a turn, before the normal request.
    /// It records the exact messages being summarized by reference, so the
    /// summary can be audited against the log it replaced.
    pub const CONTEXT_COMPACTION_STARTED: &'static str = "context/compaction.started";
    /// The compaction attempt failed or produced no usable text. No history
    /// changed: the turn proceeds under the ordinary trim policy.
    pub const CONTEXT_COMPACTION_FAILED: &'static str = "context/compaction.failed";
    /// The summary that replaces a prefix of the settled history, in full.
    ///
    /// It carries the summary text and the references it stands in for. The
    /// model-context projection removes those messages and puts this one in
    /// their place; the summarized messages stay in the log, so the original
    /// is never lost — only moved out of the model's window.
    pub const CONTEXT_COMPACTED: &'static str = "context/compacted";
    pub const TOOL_RESULT_TRUNCATED: &'static str = "tool/result.truncated";
    pub const TOOL_RESULT_REDACTED: &'static str = "tool/result.redacted";
    pub const TOOL_RESULT_ARTIFACT_OFFLOADED: &'static str = "tool/result.artifact_offloaded";
    pub const APPROVAL_REQUESTED: &'static str = "approval/requested";
    pub const APPROVAL_GRANTED: &'static str = "approval/granted";
    pub const APPROVAL_DENIED: &'static str = "approval/denied";
    pub const APPROVAL_EXPIRED: &'static str = "approval/expired";
    pub const APPROVAL_CANCELLED: &'static str = "approval/cancelled";
    /// One fast-decision call's trace. An observation, never an input
    /// to anything in this build: the shadow mode is what makes that true.
    pub const DECISION_TRACE: &'static str = "decision/trace";
    /// A plugin was registered and its contributions are now part of the tool
    /// surface.
    pub const PLUGIN_LOADED: &'static str = "plugin/loaded";
    /// A registered plugin was withdrawn; everything it contributed is gone.
    pub const PLUGIN_UNLOADED: &'static str = "plugin/unloaded";
    /// A registered plugin stopped working. Distinct from a registration that
    /// was *refused* for want of authorization: a refusal is a decision not to
    /// load, made by configuration, and appends nothing at all.
    pub const PLUGIN_FAILED: &'static str = "plugin/failed";
    /// A child agent was started for a delegation. Appended to the
    /// **parent's** log: the child's own history lives in the child's session and
    /// says what the child did, while this says that the parent asked.
    pub const SUBAGENT_SPAWNED: &'static str = "subagent/spawned";
    /// A child agent ended with an answer, or with less than it was asked for
    /// (`status: "partial"` in the payload — a child's own report, never this
    /// build's guess at how far it got).
    ///
    /// The type is the family and the payload's `status` is the precise word, so
    /// that a reader asking the coarse question ("did it come back?") does not
    /// have to know every terminal status this vocabulary may grow.
    pub const SUBAGENT_COMPLETED: &'static str = "subagent/completed";
    /// A child agent could not do the job, or ran past its deadline
    /// (`status: "timed_out"`).
    pub const SUBAGENT_FAILED: &'static str = "subagent/failed";
    /// The parent asked a child to stop and it stopped (`status: "cancelled"`),
    /// or this build cannot say whether it stopped
    /// (`status: "cancel_uncertain"`).
    ///
    /// Kept apart from `subagent/failed` for the reason `turn/cancelled` is kept
    /// apart from `turn/failed`: a cancellation the parent itself asked for is
    /// not a failure, and folding the two together would make "did anything go
    /// wrong?" answer yes for a decision that went exactly as intended. That
    /// makes this a fourth subagent name beyond the three
    /// (`spawned` / `completed` / `failed`) it was first planned with; the reason it is
    /// spelled out here is that the alternative is a status a reader has to
    /// reinterpret.
    pub const SUBAGENT_CANCELLED: &'static str = "subagent/cancelled";

    /// Every core event type, in one place.
    ///
    /// A fold has to tell three cases apart: types it consumes, types it knows
    /// about but has no use for (skip), and types it has never heard of (fail
    /// closed, unless the event is `ignorable`). Without this table the middle
    /// case turns into a hand-maintained match list that silently rots.
    pub const ALL: &'static [&'static str] = &[
        Self::SESSION_STARTED,
        Self::SESSION_FORKED,
        Self::PROFILE_RESOLVED,
        Self::RUN_STARTED,
        Self::RUN_YIELDED,
        Self::RUN_COMPLETED,
        Self::RUN_FAILED,
        Self::RUN_CANCELLED,
        Self::RUN_INTERRUPTED,
        Self::TURN_STARTED,
        Self::TURN_STEERED,
        Self::TURN_YIELDED,
        Self::TURN_RESUMED,
        Self::TURN_COMPLETED,
        Self::TURN_FAILED,
        Self::TURN_INTERRUPTED,
        Self::STEP_STARTED,
        Self::STEP_COMPLETED,
        Self::STEP_FAILED,
        Self::STEP_INTERRUPTED,
        Self::INBOX_APPENDED,
        Self::INBOX_CLAIMED,
        Self::INBOX_DISCARDED,
        Self::TURN_CANCELLED,
        Self::MODEL_TOOLS_FROZEN,
        Self::MODEL_REQUEST_PREPARED,
        Self::MODEL_ATTEMPT_STARTED,
        Self::MODEL_ATTEMPT_FAILED,
        Self::MODEL_ATTEMPT_CANCELLED,
        Self::MODEL_RESPONSE_COMPLETED,
        Self::MODEL_STREAM_STARTED,
        Self::MODEL_STREAM_DELTA,
        Self::MODEL_STREAM_TOOL_CALL_DELTA,
        Self::MODEL_STREAM_REASONING_DELTA,
        Self::TOOL_STARTED,
        Self::TOOL_COMPLETED,
        Self::TOOL_FAILED,
        Self::TOOL_CANCELLED,
        Self::TOOL_INTERRUPTED,
        Self::TOOL_OUTCOME_UNKNOWN,
        Self::TOOL_NOT_STARTED,
        Self::PLAN_UPDATED,
        Self::JUDGMENT_UPDATED,
        Self::JUDGMENT_REQUIRED,
        Self::CONTEXT_COMPACTION_STARTED,
        Self::CONTEXT_COMPACTION_FAILED,
        Self::CONTEXT_COMPACTED,
        Self::TOOL_RESULT_TRUNCATED,
        Self::TOOL_RESULT_REDACTED,
        Self::TOOL_RESULT_ARTIFACT_OFFLOADED,
        Self::APPROVAL_REQUESTED,
        Self::APPROVAL_GRANTED,
        Self::APPROVAL_DENIED,
        Self::APPROVAL_EXPIRED,
        Self::APPROVAL_CANCELLED,
        Self::DECISION_TRACE,
        Self::PLUGIN_LOADED,
        Self::PLUGIN_UNLOADED,
        Self::PLUGIN_FAILED,
        Self::SUBAGENT_SPAWNED,
        Self::SUBAGENT_COMPLETED,
        Self::SUBAGENT_FAILED,
        Self::SUBAGENT_CANCELLED,
    ];

    /// Whether this type is one of the core types above.
    pub fn is_core(&self) -> bool {
        Self::ALL.contains(&self.0.as_str())
    }
}

/// A producer-authored event, before the store assigns a sequence number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDraft {
    pub event_id: EventId,
    pub event_type: EventType,
    pub schema_version: SchemaVersion,
    pub session_id: SessionId,
    /// Present when the event belongs to a specific run/turn/step; `None` for
    /// session- and inbox-level events that occur outside any run.
    pub run_id: Option<RunId>,
    pub turn_id: Option<TurnId>,
    pub step_id: Option<StepId>,
    pub actor: Actor,
    pub created_at: DateTime<Utc>,
    pub causation_id: Option<EventId>,
    pub correlation_id: Option<String>,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    /// Whether an unknown event type may be skipped by projections. Unknown
    /// required (`ignorable == false`) events must fail closed instead.
    pub ignorable: bool,
}

/// An event persisted by the store, with its assigned seq and record time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub seq: EventSeq,
    pub recorded_at: DateTime<Utc>,
    pub event: EventDraft,
}

/// A UI-only event that never enters the durable log or advances the replay cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveEvent {
    pub live_seq: u64,
    pub event_type: EventType,
    pub session_id: SessionId,
    pub created_at: DateTime<Utc>,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(event_type: &str) -> EventDraft {
        EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::from_string("0.1.0"),
            session_id: SessionId::new(),
            run_id: Some(RunId::new()),
            turn_id: Some(TurnId::new()),
            step_id: Some(StepId::new()),
            actor: Actor::System,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::json!({"key": "value"}),
            metadata: serde_json::json!({}),
            ignorable: false,
        }
    }

    #[test]
    fn event_draft_round_trips_through_serde() {
        let original = draft(EventType::TURN_STARTED);
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: EventDraft = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original.event_id, decoded.event_id);
        assert_eq!(original.event_type, decoded.event_type);
        assert_eq!(original.schema_version, decoded.schema_version);
        assert_eq!(original.session_id, decoded.session_id);
        assert_eq!(original.actor, decoded.actor);
        assert_eq!(original.ignorable, decoded.ignorable);
    }

    #[test]
    fn stored_event_round_trips_with_seq_and_recorded_at() {
        let stored = StoredEvent {
            seq: EventSeq::new(7),
            recorded_at: Utc::now(),
            event: draft(EventType::STEP_COMPLETED),
        };
        let json = serde_json::to_string(&stored).expect("serialize");
        let decoded: StoredEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.seq, EventSeq::new(7));
        assert_eq!(decoded.event.event_type.as_str(), "step/completed");
    }

    #[test]
    fn the_core_event_table_is_well_formed() {
        let mut seen = std::collections::BTreeSet::new();
        for ty in EventType::ALL {
            assert!(ty.contains('/'), "core event type is not namespaced: {ty}");
            assert!(seen.insert(*ty), "core event type listed twice: {ty}");
        }
        // The table exists so a fold can skip known-but-irrelevant events; an
        // empty one would make every event look unknown.
        assert!(EventType::ALL.len() > 40);
        assert!(EventType::new(EventType::TOOL_COMPLETED).is_core());
        assert!(!EventType::new("plugin/whatever").is_core());
    }

    #[test]
    fn event_type_is_transparent_string() {
        let ty = EventType::new("custom/plugin-event");
        let json = serde_json::to_string(&ty).expect("serialize");
        assert_eq!(json, "\"custom/plugin-event\"");
        let decoded: EventType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, ty);
    }

    #[test]
    fn event_seq_increments_monotonically() {
        assert_eq!(EventSeq::ZERO.as_u64(), 0);
        assert_eq!(EventSeq::ZERO.next(), EventSeq::new(1));
        assert_eq!(EventSeq::new(41).next(), EventSeq::new(42));
    }

    #[test]
    fn live_event_has_no_durable_seq_or_event_id() {
        let live = LiveEvent {
            live_seq: 3,
            event_type: EventType::new("ui/model.delta"),
            session_id: SessionId::new(),
            created_at: Utc::now(),
            payload: serde_json::json!({"delta": "abc"}),
            metadata: serde_json::json!({}),
        };
        let json = serde_json::to_string(&live).expect("serialize");
        let decoded: LiveEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.live_seq, 3);
        assert_eq!(decoded.event_type.as_str(), "ui/model.delta");
    }
}
