//! The Transcript projection: a session log as a conversation.
//!
//! This is a third fold beside [`crate::SessionProjection`] and
//! [`crate::ModelContextProjection`], and it exists for the same reason the
//! other two do: the log is the fact, and a view of it is derived, never stored
//! (C1.3). The difference is the audience. Session state answers "what is open",
//! model context answers "what does the model see next", and this one answers
//! "what happened, in the order a person would read it".
//!
//! Items are **upserted by id**, not appended blindly:
//!
//! - a tool starts as `running` and ends `completed`, `failed`, `refused` or
//!   `waiting` (held for approval) — one item, updated in place;
//! - an approval starts `pending` and ends granted, denied, expired or
//!   cancelled — one item;
//! - a turn's answer may be written by the model response first and replaced by
//!   the turn's `final_content`, which is the authoritative text.
//!
//! The fold does not truncate and does not format. A projection that shortened
//! content would be inventing a view the log cannot reproduce; truncation is the
//! presentation layer's job (the same rule [`crate::session`] follows).

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{ApprovalId, EventSeq, StepId, TurnId};
use agent_protocol::inbox::{InboxItem, InboxKind};
use agent_protocol::model::{ContentBlock, ModelResponse, Usage};
use agent_protocol::tool::{
    APPROVAL_DENIED_OUTCOME, APPROVAL_PENDING_OUTCOME, GATE_REFUSED_OUTCOME,
    GATEWAY_REFUSED_OUTCOME,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::definition::{ApplyError, ProjectionDefinition};

/// What one line of the conversation is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemBody {
    /// Something the person asked for.
    User {
        text: String,
        /// The inbox item this came from. Used to attach the message to the
        /// turn that later claims it; not part of the wire shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inbox_item: Option<String>,
    },
    /// A steering note injected at a step boundary.
    Note { text: String },
    /// One tool call.
    Action {
        tool: String,
        arguments: serde_json::Value,
        status: ActionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        /// How long the call took, once it is over. Computed from the two
        /// events' record times, not written by the loop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// When the call started, so the completion can subtract. Internal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<DateTime<Utc>>,
    },
    /// A request for a person's decision.
    Approval {
        approval_id: ApprovalId,
        tool: String,
        /// The already-redacted summary the request carried.
        summary: String,
        state: ApprovalState,
    },
    /// What the model said.
    Answer { text: String },
    /// How a turn ended.
    TurnEnd {
        ending: TurnEnding,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}

/// Where a tool call stands.
///
/// `Waiting` is not a failure: a call held for approval did not run and will be
/// carried forward by `resume` (C1.4). `Refused` is the other side of the same
/// distinction — the gateway or a kernel gate said no before the world was
/// touched (C3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Running,
    Completed,
    Failed,
    Refused,
    Waiting,
    Cancelled,
}

/// Where a request for a decision stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Granted,
    Denied,
    Expired,
    Cancelled,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnding {
    /// It ran to an answer.
    Answered,
    /// The step budget ran out.
    MaxSteps,
    /// It stopped for a person (an approval).
    Yielded,
    /// It broke.
    Failed,
    /// A person stopped it.
    Cancelled,
}

/// One line of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptItem {
    /// Stable identity for upserts, e.g. `a:<provider_tool_call_id>`.
    pub id: String,
    /// The event that first created the item; ordering is by first appearance.
    pub seq: EventSeq,
    pub turn_id: Option<TurnId>,
    pub step_id: Option<StepId>,
    pub body: ItemBody,
}

/// The folded transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptState {
    pub items: Vec<TranscriptItem>,
    /// The last event this fold applied.
    pub head: EventSeq,
    /// The most recent usage the model reported. Kept because a turn's terminal
    /// event does not carry it; the last attempt in the turn is the one the
    /// turn cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<Usage>,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self { items: Vec::new(), head: EventSeq::ZERO, last_usage: None }
    }
}

/// The client-facing view.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptView {
    pub items: Vec<TranscriptItem>,
    pub head: EventSeq,
}

/// The Transcript projection definition.
pub struct TranscriptProjection;

impl TranscriptProjection {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TranscriptProjection {
    fn default() -> Self {
        Self::new()
    }
}

/// Insert or replace an item, keeping the order of first appearance.
fn upsert(
    state: &mut TranscriptState,
    id: String,
    seq: EventSeq,
    turn: Option<TurnId>,
    step: Option<StepId>,
    body: ItemBody,
) {
    if let Some(existing) = state.items.iter_mut().find(|item| item.id == id) {
        // The first seq is the item's place in the timeline; a later update
        // must not move it after the rows it was already before.
        existing.body = body;
        if existing.turn_id.is_none() {
            existing.turn_id = turn;
        }
        if existing.step_id.is_none() {
            existing.step_id = step;
        }
        return;
    }
    state.items.push(TranscriptItem { id, seq, turn_id: turn, step_id: step, body });
}

/// The text blocks of a response, in order.
///
/// Tool-use blocks are proposals the action items already carry; only text is
/// what the model *said*.
fn response_text(response: &ModelResponse) -> String {
    response
        .content_blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The id of a user message, keyed by the event that queued it.
fn user_id(seq: EventSeq) -> String {
    format!("u:{}", seq.as_u64())
}

/// The id of an answer, keyed by the turn it answers.
fn answer_id(turn: &TurnId) -> String {
    format!("ans:{}", turn.as_str())
}

/// The id of a turn's ending.
fn turn_end_id(turn: &TurnId) -> String {
    format!("end:{}", turn.as_str())
}

impl ProjectionDefinition for TranscriptProjection {
    type State = TranscriptState;
    type View = TranscriptView;

    fn key(&self) -> &str {
        "transcript"
    }

    fn state_schema(&self) -> &str {
        "transcript/0.1.0"
    }

    fn state_version(&self) -> u64 {
        1
    }

    fn init(&self) -> Self::State {
        TranscriptState::default()
    }

    fn apply(&self, state: &Self::State, event: &StoredEvent) -> Result<Self::State, ApplyError> {
        let mut next = state.clone();
        next.head = event.seq;

        match event.event.event_type.as_str() {
            EventType::INBOX_APPENDED => {
                let item: InboxItem = serde_json::from_value(event.event.payload.clone())
                    .map_err(|e| ApplyError::malformed(event, e))?;
                let text = item
                    .payload
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                match item.kind {
                    InboxKind::NextTurn | InboxKind::Scheduled if !text.trim().is_empty() => {
                        upsert(
                            &mut next,
                            user_id(event.seq),
                            event.seq,
                            None,
                            None,
                            ItemBody::User {
                                text,
                                inbox_item: Some(item.item_id.as_str().to_string()),
                            },
                        );
                    }
                    // Steering is a sentence said to the turn, not a new
                    // request; it reads as a note in place. The turn it was
                    // addressed to is not known until `turn/steered`.
                    InboxKind::NextStep if !text.trim().is_empty() => {
                        upsert(
                            &mut next,
                            format!("note:{}", event.seq.as_u64()),
                            event.seq,
                            None,
                            None,
                            ItemBody::Note { text },
                        );
                    }
                    _ => {}
                }
            }

            EventType::TURN_STARTED => {
                // The message that opened this turn was appended before the
                // turn existed. Link it now, so a reader can group the turn.
                let inbox_id = event.event.payload.get("inbox_item").and_then(|v| v.as_str());
                if let Some(inbox_id) = inbox_id {
                    let turn = event.event.turn_id.clone();
                    if let Some(message) = next.items.iter_mut().find(|item| {
                        matches!(&item.body, ItemBody::User { inbox_item: Some(id), .. } if id == inbox_id)
                    }) {
                        message.turn_id = turn;
                    }
                }
            }

            EventType::TURN_STEERED => {
                let text = event
                    .event
                    .payload
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    upsert(
                        &mut next,
                        format!("steer:{}", event.seq.as_u64()),
                        event.seq,
                        event.event.turn_id.clone(),
                        None,
                        ItemBody::Note { text: text.to_string() },
                    );
                }
            }

            EventType::MODEL_RESPONSE_COMPLETED => {
                let response: ModelResponse = event
                    .event
                    .payload
                    .get("response")
                    .cloned()
                    .ok_or_else(|| ApplyError::malformed(event, "no response"))
                    .and_then(|value| {
                        serde_json::from_value(value).map_err(|e| ApplyError::malformed(event, e))
                    })?;
                if let Some(usage) = response.usage {
                    next.last_usage = Some(usage);
                }
                let text = response_text(&response);
                if !text.trim().is_empty()
                    && let Some(turn) = event.event.turn_id.clone()
                {
                    let id = answer_id(&turn);
                    let combined = match next.items.iter().find(|item| item.id == id) {
                        Some(TranscriptItem {
                            body: ItemBody::Answer { text: previous }, ..
                        }) if !previous.is_empty() => {
                            format!("{previous}\n\n{text}")
                        }
                        _ => text,
                    };
                    upsert(
                        &mut next,
                        id,
                        event.seq,
                        Some(turn),
                        event.event.step_id.clone(),
                        ItemBody::Answer { text: combined },
                    );
                }
            }

            EventType::TOOL_STARTED => {
                let tool = payload_str(event, "tool_name")?;
                let call_id = payload_str(event, "provider_tool_call_id")?;
                let arguments = event
                    .event
                    .payload
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                upsert(
                    &mut next,
                    format!("a:{call_id}"),
                    event.seq,
                    event.event.turn_id.clone(),
                    event.event.step_id.clone(),
                    ItemBody::Action {
                        tool,
                        arguments,
                        status: ActionStatus::Running,
                        content: None,
                        duration_ms: None,
                        started_at: Some(event.recorded_at),
                    },
                );
            }

            EventType::TOOL_COMPLETED
            | EventType::TOOL_FAILED
            | EventType::TOOL_CANCELLED
            | EventType::TOOL_INTERRUPTED
            | EventType::TOOL_NOT_STARTED => {
                let tool = payload_str(event, "tool_name")?;
                let call_id = payload_str(event, "provider_tool_call_id")?;
                let outcome = event.event.payload.get("outcome").and_then(|v| v.as_str());
                let status = match event.event.event_type.as_str() {
                    EventType::TOOL_COMPLETED => ActionStatus::Completed,
                    EventType::TOOL_CANCELLED | EventType::TOOL_INTERRUPTED => {
                        ActionStatus::Cancelled
                    }
                    EventType::TOOL_NOT_STARTED => ActionStatus::Refused,
                    _ => match outcome {
                        // Held for approval: it did not run and will be carried
                        // forward, so it is neither a failure nor a refusal
                        // (C1.4).
                        Some(APPROVAL_PENDING_OUTCOME) => ActionStatus::Waiting,
                        Some(APPROVAL_DENIED_OUTCOME)
                        | Some(GATE_REFUSED_OUTCOME)
                        | Some(GATEWAY_REFUSED_OUTCOME) => ActionStatus::Refused,
                        _ => ActionStatus::Failed,
                    },
                };
                let id = format!("a:{call_id}");
                let started_at = next
                    .items
                    .iter()
                    .find(|item| item.id == id)
                    .and_then(|item| match &item.body {
                        ItemBody::Action { started_at, .. } => *started_at,
                        _ => None,
                    })
                    .unwrap_or(event.recorded_at);
                let duration_ms = (event.recorded_at - started_at).num_milliseconds().max(0) as u64;
                let arguments = next
                    .items
                    .iter()
                    .find(|item| item.id == id)
                    .and_then(|item| match &item.body {
                        ItemBody::Action { arguments, .. } => Some(arguments.clone()),
                        _ => None,
                    })
                    .unwrap_or(serde_json::Value::Null);
                let content = event
                    .event
                    .payload
                    .get("content")
                    .and_then(|value| value.as_str())
                    .map(|text| text.to_string());
                upsert(
                    &mut next,
                    id,
                    event.seq,
                    event.event.turn_id.clone(),
                    event.event.step_id.clone(),
                    ItemBody::Action {
                        tool,
                        arguments,
                        status,
                        content,
                        duration_ms: Some(duration_ms),
                        started_at: Some(started_at),
                    },
                );
            }

            EventType::APPROVAL_REQUESTED => {
                let request: agent_protocol::permissions::ApprovalRequest =
                    serde_json::from_value(event.event.payload.clone())
                        .map_err(|e| ApplyError::malformed(event, e))?;
                upsert(
                    &mut next,
                    format!("ap:{}", request.approval_id.as_str()),
                    event.seq,
                    event.event.turn_id.clone(),
                    event.event.step_id.clone(),
                    ItemBody::Approval {
                        approval_id: request.approval_id.clone(),
                        tool: request.tool_name,
                        summary: request.args_summary,
                        state: ApprovalState::Pending,
                    },
                );
            }

            EventType::APPROVAL_GRANTED
            | EventType::APPROVAL_DENIED
            | EventType::APPROVAL_EXPIRED
            | EventType::APPROVAL_CANCELLED => {
                // The three writers do not share a payload shape: a decision
                // carries the reply, and an expiry records only the id it
                // closed. Reading the id from the envelope's payload and the
                // state from the event type keeps both readable without a
                // second guess.
                let approval_id = event
                    .event
                    .payload
                    .get("approval_id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| ApplyError::malformed(event, "no approval_id"))?;
                let state = match event.event.event_type.as_str() {
                    EventType::APPROVAL_GRANTED => ApprovalState::Granted,
                    EventType::APPROVAL_DENIED => ApprovalState::Denied,
                    EventType::APPROVAL_EXPIRED => ApprovalState::Expired,
                    _ => ApprovalState::Cancelled,
                };
                let id = format!("ap:{approval_id}");
                if let Some(existing) = next.items.iter_mut().find(|item| item.id == id)
                    && let ItemBody::Approval { state: current, .. } = &mut existing.body
                {
                    *current = state;
                }
            }

            EventType::TURN_YIELDED
            | EventType::TURN_COMPLETED
            | EventType::TURN_FAILED
            | EventType::TURN_CANCELLED
            | EventType::TURN_INTERRUPTED => {
                let Some(turn) = event.event.turn_id.clone() else {
                    return Ok(next);
                };
                let stop_reason = event
                    .event
                    .payload
                    .get("stop_reason")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string());
                let ending = match event.event.event_type.as_str() {
                    EventType::TURN_COMPLETED => match stop_reason.as_deref() {
                        Some("max_steps") => TurnEnding::MaxSteps,
                        _ => TurnEnding::Answered,
                    },
                    EventType::TURN_YIELDED => TurnEnding::Yielded,
                    EventType::TURN_FAILED => TurnEnding::Failed,
                    _ => TurnEnding::Cancelled,
                };

                // A completed turn's `final_content` is the answer, not a hint:
                // it replaces whatever the model response wrote, because the
                // turn is the thing that decided what the answer was.
                if let Some(content) =
                    event.event.payload.get("final_content").and_then(|value| value.as_str())
                {
                    upsert(
                        &mut next,
                        answer_id(&turn),
                        event.seq,
                        Some(turn.clone()),
                        None,
                        ItemBody::Answer { text: content.to_string() },
                    );
                }

                let usage = next.last_usage;
                upsert(
                    &mut next,
                    turn_end_id(&turn),
                    event.seq,
                    Some(turn),
                    None,
                    ItemBody::TurnEnd { ending, stop_reason, usage },
                );
            }

            // Core traffic this fold has nothing to say about — the model's
            // request bookkeeping, reason nodes, policy, stream deltas, inbox
            // claims. Skipped as ordinary traffic, exactly as the session fold
            // does; a fold that failed on every core type could not read a real
            // session (C2.5 keeps the two refusals apart).
            _ if event.event.event_type.is_core() => {}

            _ => return Err(ApplyError::UnknownEventType(event.event.event_type.clone())),
        }

        Ok(next)
    }

    fn view(&self, state: &Self::State) -> Self::View {
        TranscriptView { items: state.items.clone(), head: state.head }
    }
}

/// A required string field of an event payload.
fn payload_str(event: &StoredEvent, key: &str) -> Result<String, ApplyError> {
    event
        .event
        .payload
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| ApplyError::malformed(event, format!("no {key}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventId, SchemaVersion, SessionId};
    use agent_protocol::inbox::InboxItemStatus;
    use agent_protocol::permissions::{
        ApprovalDecision, ApprovalReply, ApprovalRequest, ApprovalScope, ApprovalStatus, Capability,
    };
    use serde_json::json;

    fn stored(
        seq: u64,
        event_type: &str,
        turn: Option<TurnId>,
        step: Option<StepId>,
        payload: serde_json::Value,
    ) -> StoredEvent {
        let now = Utc::now();
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: now,
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::current(),
                session_id: SessionId::from_string("transcript-test"),
                run_id: None,
                turn_id: turn,
                step_id: step,
                actor: Actor::Agent,
                created_at: now,
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: json!({}),
                ignorable: false,
            },
        }
    }

    fn apply(state: &TranscriptState, event: &StoredEvent) -> TranscriptState {
        TranscriptProjection::new().apply(state, event).unwrap()
    }

    fn user_text(item: &TranscriptItem) -> Option<&str> {
        match &item.body {
            ItemBody::User { text, .. } => Some(text),
            _ => None,
        }
    }

    #[test]
    fn a_queued_prompt_becomes_a_user_item_and_the_turn_claims_it() {
        let turn = TurnId::from_string("t1");
        let item = InboxItem {
            item_id: agent_protocol::ids::InboxItemId::from_string("i1"),
            session_id: SessionId::from_string("transcript-test"),
            kind: InboxKind::NextTurn,
            payload: json!({ "text": "先看一下 agent-cli" }),
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };

        let state = apply(
            &TranscriptProjection::new().init(),
            &stored(0, EventType::INBOX_APPENDED, None, None, serde_json::to_value(&item).unwrap()),
        );
        assert_eq!(state.items.len(), 1);
        assert_eq!(user_text(&state.items[0]), Some("先看一下 agent-cli"));
        assert!(state.items[0].turn_id.is_none(), "the turn did not exist yet");

        let state = apply(
            &state,
            &stored(
                1,
                EventType::TURN_STARTED,
                Some(turn.clone()),
                None,
                json!({ "inbox_item": item.item_id }),
            ),
        );
        assert_eq!(state.items[0].turn_id, Some(turn), "the message now belongs to the turn");
    }

    #[test]
    fn a_draft_answer_is_replaced_by_the_turns_final_content() {
        let turn = TurnId::from_string("t1");
        let usage = Usage { input_tokens: Some(10), output_tokens: Some(4), ..Usage::default() };
        let response = ModelResponse { usage: Some(usage), ..ModelResponse::text("draft") };

        let state = apply(
            &TranscriptProjection::new().init(),
            &stored(
                2,
                EventType::MODEL_RESPONSE_COMPLETED,
                Some(turn.clone()),
                Some(StepId::from_string("s1")),
                json!({ "attempt_id": "a1", "response": response }),
            ),
        );
        assert_eq!(
            state.items[0].body,
            ItemBody::Answer { text: "draft".to_string() },
            "the response's text is what the turn said so far"
        );

        let state = apply(
            &state,
            &stored(
                3,
                EventType::TURN_COMPLETED,
                Some(turn),
                None,
                json!({ "final_content": "final" }),
            ),
        );
        let answer = state.items.iter().find(|item| item.id == "ans:t1").unwrap();
        assert_eq!(answer.body, ItemBody::Answer { text: "final".to_string() });
        let ending = state.items.iter().find(|item| item.id == "end:t1").unwrap();
        match &ending.body {
            ItemBody::TurnEnd { ending, usage: end_usage, .. } => {
                assert_eq!(*ending, TurnEnding::Answered);
                assert_eq!(end_usage.expect("the last attempt's usage").input_tokens, Some(10));
            }
            other => panic!("expected a turn end, got {other:?}"),
        }
    }

    #[test]
    fn a_held_call_is_waiting_and_a_gateway_refusal_is_refused() {
        let turn = TurnId::from_string("t1");
        let step = StepId::from_string("s1");
        let mut state = TranscriptProjection::new().init();

        for (call, seq) in [("c1", 10), ("c2", 20), ("c3", 30)] {
            state = apply(
                &state,
                &stored(
                    seq,
                    EventType::TOOL_STARTED,
                    Some(turn.clone()),
                    Some(step.clone()),
                    json!({
                        "tool_name": "shell.run",
                        "provider_tool_call_id": call,
                        "arguments": { "command": "cargo test" },
                    }),
                ),
            );
        }

        state = apply(
            &state,
            &stored(
                11,
                EventType::TOOL_FAILED,
                Some(turn.clone()),
                Some(step.clone()),
                json!({
                    "tool_name": "shell.run",
                    "provider_tool_call_id": "c1",
                    "status": "failed",
                    "content": "approval required",
                    "outcome": APPROVAL_PENDING_OUTCOME,
                    "approval_id": "ap-1",
                }),
            ),
        );
        state = apply(
            &state,
            &stored(
                21,
                EventType::TOOL_FAILED,
                Some(turn.clone()),
                Some(step.clone()),
                json!({
                    "tool_name": "shell.run",
                    "provider_tool_call_id": "c2",
                    "status": "failed",
                    "content": "denied by policy",
                    "outcome": GATEWAY_REFUSED_OUTCOME,
                }),
            ),
        );
        state = apply(
            &state,
            &stored(
                31,
                EventType::TOOL_FAILED,
                Some(turn),
                Some(step),
                json!({
                    "tool_name": "shell.run",
                    "provider_tool_call_id": "c3",
                    "status": "failed",
                    "content": "exit code 1",
                }),
            ),
        );

        let status = |id: &str| match &state.items.iter().find(|i| i.id == id).unwrap().body {
            ItemBody::Action { status, .. } => *status,
            other => panic!("expected an action, got {other:?}"),
        };
        assert_eq!(status("a:c1"), ActionStatus::Waiting, "held is not failed");
        assert_eq!(status("a:c2"), ActionStatus::Refused, "a refusal is not a failure");
        assert_eq!(status("a:c3"), ActionStatus::Failed);
    }

    #[test]
    fn an_approval_becomes_pending_then_granted() {
        let request = ApprovalRequest {
            approval_id: agent_protocol::ids::ApprovalId::from_string("ap-1"),
            session_id: SessionId::from_string("transcript-test"),
            capability: Capability::new(Capability::PROCESS_SPAWN),
            tool_name: "shell.run".to_string(),
            args_summary: "cargo test".to_string(),
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
        };
        let state = apply(
            &TranscriptProjection::new().init(),
            &stored(
                1,
                EventType::APPROVAL_REQUESTED,
                Some(TurnId::from_string("t1")),
                Some(StepId::from_string("s1")),
                serde_json::to_value(&request).unwrap(),
            ),
        );
        let state = apply(
            &state,
            &stored(
                2,
                EventType::APPROVAL_GRANTED,
                Some(TurnId::from_string("t1")),
                Some(StepId::from_string("s1")),
                serde_json::to_value(ApprovalReply {
                    approval_id: request.approval_id.clone(),
                    decision: ApprovalDecision::Grant,
                    scope: ApprovalScope::AllowOnce,
                    actor: "user".to_string(),
                    created_at: Utc::now(),
                })
                .unwrap(),
            ),
        );

        let item = state.items.iter().find(|item| item.id == "ap:ap-1").unwrap();
        match &item.body {
            ItemBody::Approval { state, summary, .. } => {
                assert_eq!(*state, ApprovalState::Granted);
                assert_eq!(summary, "cargo test");
            }
            other => panic!("expected an approval, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_payload_is_refused_rather_than_skipped() {
        let event = stored(0, EventType::INBOX_APPENDED, None, None, json!({ "not": "an item" }));
        let error = TranscriptProjection::new().apply(&TranscriptProjection::new().init(), &event);
        assert!(
            matches!(error, Err(ApplyError::MalformedPayload { .. })),
            "a known event we cannot read must fail closed: {error:?}"
        );
    }

    #[test]
    fn an_unknown_non_core_event_is_refused() {
        let event = stored(0, "custom/unknown", None, None, json!({}));
        let error = TranscriptProjection::new().apply(&TranscriptProjection::new().init(), &event);
        assert!(matches!(error, Err(ApplyError::UnknownEventType(_))), "unexpected: {error:?}");
    }
}
