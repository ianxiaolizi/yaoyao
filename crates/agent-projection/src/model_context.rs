//! The model-context projection: the transcript the model is shown.
//!
//! Model history is derived, never hand-maintained. This fold walks the durable
//! log and produces the ordered messages that make up a session's conversation,
//! so that "what the model saw" and "what the log says" are the same thing —
//! the failure mode it exists to prevent is a model seeing something that was
//! never recorded.
//!
//! Two consequences are deliberate:
//!
//! - Only settled facts contribute. A failed or cancelled model attempt leaves
//!   no assistant message, and a tool outcome only answers a call the
//!   log already contains.
//! - Message uids come from durable ids (`ModelMessage::user_for_turn` and
//!   friends), so a message rebuilt here is the *same* message the loop sent,
//!   not a copy with a new identity.
//!
//! The invariant runs in both directions. A transcript *shorter* than the log is
//! the same defect as one longer than it — a model handed a history that omits
//! the answer it gave and the calls it proposed reads as if it never spoke. So
//! an event this fold knows, whose payload it cannot read, is an error
//! (`ApplyError::MalformedPayload`) rather than a silent skip (YA-04; YA-69
//! closed the three optional reads left over from it — the inbox payload, the
//! turn's item, and a steering note — keeping only the half that has a
//! representable "nothing to say").

use std::collections::BTreeMap;

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{AttemptId, InboxItemId, RunId};
use agent_protocol::inbox::InboxItem;
use agent_protocol::instructions::RunInstructions;
use agent_protocol::model::{ModelMessage, ModelResponse, Usage};
use agent_protocol::tool::{APPROVAL_PENDING_OUTCOME, ToolStatus};
use serde::{Deserialize, Serialize};

use crate::definition::{ApplyError, ProjectionDefinition};

/// A tool call the model proposed that the log has not settled yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OpenCall {
    tool_name: String,
    tool_call_id: String,
}

/// The instructions of the latest run, as the transcript remembers them.
///
/// Only the text is kept: the message is rebuilt with [`ModelMessage::system_for_run`],
/// so its uid comes from the same durable id the loop used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SystemNotice {
    run_id: RunId,
    text: String,
}

/// Folded model-visible transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelContextState {
    /// User text by inbox item id, held until `turn/started` names the item.
    /// Placement matters: an item appended while a turn is in flight belongs to
    /// a later turn, and appending its message at append time would put it in
    /// the middle of the turn that is still running.
    inbox_text: BTreeMap<String, String>,
    /// Proposed tool calls with no settled outcome yet.
    open_calls: Vec<OpenCall>,
    /// The transcript, in order.
    messages: Vec<ModelMessage>,
    /// What the latest run recorded as its instructions, if it recorded any.
    /// The view puts it at the front; a later run replaces it rather than
    /// adding a second system message.
    #[serde(default)]
    system: Option<SystemNotice>,
    /// The provider's own counts for the most recent settled request. Kept
    /// because the compaction trigger is measured against real usage: an
    /// estimate of the derived messages alone undercounts a request by the
    /// stable head, the per-turn layers, and the tool definitions. A provider
    /// that reports nothing leaves `None`, and the estimate remains the
    /// fallback — never a zero that would read as "an empty request".
    #[serde(default)]
    last_usage: Option<Usage>,
}

impl ModelContextState {
    /// The provider's own counts for the most recent request it reported
    /// usage for, if any.
    pub fn last_usage(&self) -> Option<Usage> {
        self.last_usage
    }
}

/// The model-context projection definition.
pub struct ModelContextProjection;

impl ModelContextProjection {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ModelContextProjection {
    fn default() -> Self {
        Self
    }
}

impl ProjectionDefinition for ModelContextProjection {
    type State = ModelContextState;
    type View = Vec<ModelMessage>;

    fn key(&self) -> &str {
        "model-context"
    }

    fn state_schema(&self) -> &str {
        "model-context/0.3.0"
    }

    fn state_version(&self) -> u64 {
        // 2: the latest run's instructions are part of the folded state.
        // 3: the latest reported usage is part of the state, so the compaction
        // trigger can be measured on real input tokens.
        3
    }

    fn init(&self) -> Self::State {
        ModelContextState::default()
    }

    fn apply(&self, state: &Self::State, event: &StoredEvent) -> Result<Self::State, ApplyError> {
        let mut next = state.clone();
        match event.event.event_type.as_str() {
            EventType::INBOX_APPENDED => {
                // The payload *is* a serialized `InboxItem` and nothing else
                // (`agent-kernel/src/inbox.rs:64` writes `to_value(&item)`), so
                // one that will not deserialize is an anomaly — a hand-edited
                // line, a record from another version — not an item with
                // nothing to say. Whether the *item* carries `text` is a
                // different question with a representable "no" (a wakeup or an
                // interrupt has none), and that half stays lenient. Splitting
                // the two is the point of YA-69: the old code read both the
                // same way, so a payload it could not parse was silently
                // treated as an item with no text.
                let item: InboxItem = serde_json::from_value(event.event.payload.clone())
                    .map_err(|e| ApplyError::malformed(event, format!("inbox item: {e}")))?;
                if let Some(text) = item.payload.get("text").and_then(|v| v.as_str()) {
                    next.inbox_text.insert(item.item_id.to_string(), text.to_string());
                }
            }

            EventType::RUN_STARTED => {
                // The absence of `instructions` is legal: a log written before
                // runs carried them, or a harness whose scheduler had no
                // source. It means this run recorded no prompt, so the
                // transcript shows none — the same "nothing to say" shape the
                // inbox payload keeps for an item without text.
                //
                // A field that *is* there and cannot be read is an anomaly: it
                // is this build's own write shape, and skipping it would hand
                // the model a prompt it was never given (YA-04).
                next.system = match event.event.payload.get("instructions") {
                    Some(raw) => {
                        let instructions: RunInstructions = serde_json::from_value(raw.clone())
                            .map_err(|e| {
                                ApplyError::malformed(event, format!("instructions: {e}"))
                            })?;
                        let run_id = event.event.run_id.clone().ok_or_else(|| {
                            ApplyError::malformed(event, "instructions without a run_id")
                        })?;
                        Some(SystemNotice { run_id, text: instructions.text })
                    }
                    None => None,
                };
            }

            EventType::TURN_STARTED => {
                // Both fields are written on every `turn/started`
                // (`agent-kernel/src/scheduler.rs:192-200`), so either being
                // unreadable is an anomaly rather than a turn with nothing to
                // say. What *is* representable is the item having no text: it
                // was never recorded, so there is no message to emit — a
                // different fact from failing to read one.
                let item_id = event
                    .event
                    .payload
                    .get("inbox_item")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ApplyError::malformed(event, "no inbox_item"))?;
                let turn_id = event
                    .event
                    .turn_id
                    .as_ref()
                    .ok_or_else(|| ApplyError::malformed(event, "no turn_id"))?;
                // Taking the entry means an item starts at most one turn:
                // replaying the same item cannot duplicate its message.
                if let Some(text) = next.inbox_text.remove(item_id) {
                    next.messages.push(ModelMessage::user_for_turn(turn_id, text));
                }
            }

            EventType::TURN_STEERED => {
                // Steering becomes a user message exactly where the loop put
                // it: at the step boundary the scheduler read it, which is
                // after the transcript so far. Its uid is (turn, item), so a
                // rebuild produces the same message the model was actually
                // shown rather than a new one that merely says the same thing.
                //
                // All three fields are written every time
                // (`agent-kernel/src/scheduler.rs:385-397`); `text` may be the
                // empty string, which is a value and not an absence. An
                // unreadable one is an anomaly: dropping the message would hand
                // the model a transcript it was never shown, one message
                // shorter — the same defect the module doc names (YA-69).
                let item_id = event
                    .event
                    .payload
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ApplyError::malformed(event, "no item_id"))?;
                let text = event
                    .event
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ApplyError::malformed(event, "no text"))?;
                let turn_id = event
                    .event
                    .turn_id
                    .as_ref()
                    .ok_or_else(|| ApplyError::malformed(event, "no turn_id"))?;
                next.messages.push(ModelMessage::user_for_steering(
                    turn_id,
                    &InboxItemId::from_string(item_id),
                    text,
                ));
            }

            EventType::MODEL_RESPONSE_COMPLETED => {
                // This is the one event that carries what the assistant said.
                // The loop writes both fields on every settled attempt
                // (`agent-loop/src/default.rs:344-358`), so a payload missing
                // either — or carrying a response this build cannot read — is an
                // anomaly (a hand-edited line, a record from another version, a
                // fragment spliced onto a record by a bad append), not a shape
                // to skip. Skipping it drops the answer *and* the tool calls it
                // proposed: the tool results that follow would then answer
                // nothing, and the model would be shown a conversation in which
                // it never spoke. Fail closed (YA-04).
                let attempt_id = event
                    .event
                    .payload
                    .get("attempt_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ApplyError::malformed(event, "no attempt_id"))?;
                let response = event
                    .event
                    .payload
                    .get("response")
                    .ok_or_else(|| ApplyError::malformed(event, "no response"))?;
                let response: ModelResponse = serde_json::from_value(response.clone())
                    .map_err(|e| ApplyError::malformed(event, format!("response: {e}")))?;

                // The usage is the provider's own count for this request. A
                // later response that reports none leaves the last reported
                // one in place: "the provider said nothing" must not erase
                // what it said before, and the trigger keeps a real measure
                // rather than dropping back to an estimate.
                if let Some(usage) = response.usage {
                    next.last_usage = Some(usage);
                }
                next.messages.push(ModelMessage::assistant_for_attempt(
                    &AttemptId::from_string(attempt_id),
                    response.content_blocks.clone(),
                ));
                for block in &response.content_blocks {
                    if let Some((tool_call_id, tool_name, _)) = block.as_tool_use() {
                        next.open_calls.push(OpenCall {
                            tool_name: tool_name.to_string(),
                            tool_call_id: tool_call_id.to_string(),
                        });
                    }
                }
            }

            EventType::TOOL_COMPLETED
            | EventType::TOOL_FAILED
            | EventType::TOOL_CANCELLED
            | EventType::TOOL_INTERRUPTED
            | EventType::TOOL_OUTCOME_UNKNOWN
            | EventType::TOOL_NOT_STARTED => apply_tool_outcome(&mut next, event),

            EventType::CONTEXT_COMPACTED => {
                // The summary replaces exactly the messages the event names,
                // and it replaces them where they were: at the front of the
                // conversation. The originals remain in the log and in earlier
                // events; they only leave the model's window.
                //
                // Every field is written by this build's loop, so one that will
                // not read is an anomaly rather than an empty summary (YA-04).
                // A ref that no longer matches anything is the same kind of
                // anomaly: the fold would be applying a summary to a
                // transcript it does not describe.
                let attempt_id = event
                    .event
                    .payload
                    .get("attempt_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ApplyError::malformed(event, "no attempt_id"))?;
                let summary = event
                    .event
                    .payload
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ApplyError::malformed(event, "no summary"))?;
                let summarized = event
                    .event
                    .payload
                    .get("summarized")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| ApplyError::malformed(event, "no summarized"))?;
                // The summary takes the place of the earliest message it
                // replaces. For a turn-start compaction that is the front of
                // the conversation, as before; for the loop's in-flight
                // compaction it is inside the turn that was running, and
                // placing it at the front would put a summary of newer traffic
                // ahead of the older history it does not describe.
                let mut insert_at: Option<usize> = None;
                for reference in summarized {
                    let uid = reference
                        .get("message_uid")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ApplyError::malformed(event, "summarized without uid"))?;
                    let Some(index) = next
                        .messages
                        .iter()
                        .position(|message| message.message_uid.as_str() == uid)
                    else {
                        return Err(ApplyError::malformed(
                            event,
                            format!("summarized message {uid} is not in the transcript"),
                        ));
                    };
                    insert_at = Some(insert_at.map_or(index, |at| at.min(index)));
                    next.messages.remove(index);
                }
                let at = insert_at.unwrap_or(0);
                next.messages.insert(
                    at,
                    ModelMessage::summary_for(
                        &AttemptId::from_string(attempt_id),
                        summary.to_string(),
                    ),
                );
                // The deterministic ledger, when this compaction wrote one,
                // sits immediately after the summary: together they stand in
                // for the removed messages, and a later compaction finds the
                // ledger here to carry its lines forward. An event from before
                // the ledger existed simply has no such key.
                let mut after = at;
                if let Some(ledger) = event.event.payload.get("ledger") {
                    let text = ledger
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ApplyError::malformed(event, "ledger without text"))?;
                    next.messages.insert(
                        at + 1,
                        ModelMessage::ledger_for(
                            &AttemptId::from_string(attempt_id),
                            text.to_string(),
                        ),
                    );
                    after += 1;
                }
                // The verbatim record of the user's words follows the ledger,
                // on the same terms: a rebuild carries exactly the text the
                // loop recorded, and a payload that cannot be read is an
                // anomaly rather than an empty record.
                if let Some(usertext) = event.event.payload.get("usertext") {
                    let text = usertext
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ApplyError::malformed(event, "usertext without text"))?;
                    next.messages.insert(
                        after + 1,
                        ModelMessage::usertext_for(
                            &AttemptId::from_string(attempt_id),
                            text.to_string(),
                        ),
                    );
                    after += 1;
                }
                // The failures record follows the user's words, on the same
                // terms. It exists because the ledger carries only its tail
                // into the next compaction; a rebuild must restore this text
                // exactly, or a later compaction would carry something the loop
                // never sent.
                if let Some(failures) = event.event.payload.get("failures") {
                    let text = failures
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ApplyError::malformed(event, "failures without text"))?;
                    next.messages.insert(
                        after + 1,
                        ModelMessage::failures_for(
                            &AttemptId::from_string(attempt_id),
                            text.to_string(),
                        ),
                    );
                }
            }

            // Known types with nothing to say about the transcript — lifecycle,
            // inbox bookkeeping, approvals, live stream events. They are
            // skipped rather than treated as unknown, because the engine fails
            // closed on an unknown required event and every one of these is
            // ordinary traffic.
            _ if event.event.event_type.is_core() => {}

            _ => return Err(ApplyError::UnknownEventType(event.event.event_type.clone())),
        }
        Ok(next)
    }

    fn view(&self, state: &Self::State) -> Self::View {
        match &state.system {
            Some(notice) => {
                let mut view = Vec::with_capacity(state.messages.len() + 1);
                view.push(ModelMessage::system_for_run(&notice.run_id, notice.text.clone()));
                view.extend(state.messages.iter().cloned());
                view
            }
            None => state.messages.clone(),
        }
    }
}

/// Fold one terminal tool event into a tool message.
///
/// The call is matched by `provider_tool_call_id` when the event carries one.
/// The name fallback is for logs written before the loop recorded an id — it is
/// the only identity such an event has, and a name cannot tell two calls to the
/// same tool apart. Removal is what makes the first settlement win: a second
/// outcome for the same call answers nothing and is dropped rather than
/// reported twice.
///
/// A *wait* is not a settlement (see `APPROVAL_PENDING_OUTCOME`): the call stays
/// open so that the answer `resume` writes is the one that reaches the model.
fn apply_tool_outcome(state: &mut ModelContextState, event: &StoredEvent) {
    let payload = &event.event.payload;
    if payload.get("outcome").and_then(|v| v.as_str()) == Some(APPROVAL_PENDING_OUTCOME) {
        return;
    }
    let tool_name = payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or_default();
    let call_id = payload.get("provider_tool_call_id").and_then(|v| v.as_str());

    let position = call_id
        .and_then(|id| state.open_calls.iter().position(|c| c.tool_call_id == id))
        .or_else(|| state.open_calls.iter().rposition(|c| c.tool_name == tool_name));
    let Some(position) = position else {
        return;
    };
    let call = state.open_calls.remove(position);

    // A tool outcome without a step has no message identity to reuse, so there
    // is no message the loop could have sent that this would rebuild.
    let Some(step_id) = event.event.step_id.as_ref() else {
        return;
    };

    let status: Option<ToolStatus> =
        payload.get("status").and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_error = status != Some(ToolStatus::Completed);
    let content = tool_outcome_text(payload);

    state.messages.push(ModelMessage::tool_for_call(
        step_id,
        call.tool_call_id,
        call.tool_name,
        content,
        is_error,
    ));
}

/// The text the model is shown for a tool outcome.
///
/// Every branch is a function of durable payload fields, so a rebuild produces
/// the same sentence the model was given. `reason` is the one case that renders
/// rather than copies: recovery's synthesized outcomes say `safe_to_retry`, and
/// a bare flag is not something to hand a model as a tool result.
fn tool_outcome_text(payload: &serde_json::Value) -> String {
    if let Some(content) = payload.get("content").and_then(|v| v.as_str()) {
        return content.to_string();
    }
    // Older logs recorded a gateway error text under `error`; it is the same
    // sentence that was put in the message at the time.
    if let Some(error) = payload.get("error").and_then(|v| v.as_str()) {
        return error.to_string();
    }
    if let Some(reason) = payload.get("reason").and_then(|v| v.as_str()) {
        return format!("tool call did not complete: {reason}");
    }
    "tool call did not complete".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId, TurnId};
    use agent_protocol::inbox::{InboxItemStatus, InboxKind};
    use agent_protocol::model::{ContentBlock, FinishReason, MessageRole};
    use chrono::Utc;

    /// A stored `model/response.completed` at `seq` carrying `payload`.
    fn response_completed(seq: u64, payload: serde_json::Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(EventType::MODEL_RESPONSE_COMPLETED),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("corrupt"),
                run_id: None,
                turn_id: None,
                step_id: None,
                actor: Actor::Agent,
                created_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    /// The payload a settled attempt writes: an attempt id and the response.
    fn settled(response: ModelResponse) -> serde_json::Value {
        serde_json::json!({ "attempt_id": "attempt-1", "response": response })
    }

    /// The response the fold refuses, with the reason it gives.
    fn refusal(state: Result<ModelContextState, ApplyError>) -> (EventType, EventSeq, String) {
        match state {
            Err(ApplyError::MalformedPayload { event_type, seq, reason }) => {
                (event_type, seq, reason)
            }
            other => panic!("a payload the fold cannot read must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_response_this_build_cannot_read_is_refused_not_skipped() {
        let definition = ModelContextProjection::new();
        let stored = response_completed(7, serde_json::json!({ "attempt_id": "a", "response": 5 }));

        let (event_type, seq, reason) = refusal(definition.apply(&definition.init(), &stored));

        // Refusing is the point: folding this as "nothing to say" would hand back
        // a transcript missing the assistant turn entirely, and the model would
        // read its own settled answer as never having been given.
        assert_eq!(event_type.as_str(), EventType::MODEL_RESPONSE_COMPLETED);
        assert_eq!(seq, EventSeq::new(7));
        assert!(reason.contains("response"), "the reason must name the field: {reason}");
    }

    #[test]
    fn a_response_missing_what_the_loop_always_writes_is_refused() {
        let definition = ModelContextProjection::new();
        // Absent, and present-but-not-a-string: both mean the record cannot be
        // read as the loop wrote it, and neither is a shape to fold past.
        let payloads = [
            serde_json::json!({ "response": ModelResponse::text("hi") }),
            serde_json::json!({ "attempt_id": 5, "response": ModelResponse::text("hi") }),
        ];

        for payload in payloads {
            let (_, _, reason) =
                refusal(definition.apply(&definition.init(), &response_completed(1, payload)));
            assert!(reason.contains("attempt_id"), "the reason must name the field: {reason}");
        }
    }

    #[test]
    fn a_settled_response_still_folds_with_its_calls_open() {
        let definition = ModelContextProjection::new();
        let response = ModelResponse {
            content_blocks: vec![
                ContentBlock::text("looking"),
                ContentBlock::tool_use("call-1", "fs.read", serde_json::json!({ "path": "a" })),
            ],
            usage: None,
            finish_reason: FinishReason::new(FinishReason::STOP),
            provider_metadata: serde_json::Value::Null,
        };

        let state = definition
            .apply(&definition.init(), &response_completed(3, settled(response)))
            .expect("an event written the way the loop writes it must fold");

        let view = definition.view(&state);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].message_uid.as_str(), "msg/assistant/attempt-1");
        // The call is open, so whatever terminal event follows it answers a call
        // the transcript contains rather than arriving alone.
        assert_eq!(
            state.open_calls,
            vec![OpenCall { tool_name: "fs.read".to_string(), tool_call_id: "call-1".to_string() }]
        );
    }

    /// A stored event of `event_type` carrying `payload`, optionally named to a
    /// turn.
    fn event(
        event_type: &str,
        seq: u64,
        payload: serde_json::Value,
        turn: Option<TurnId>,
    ) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("corrupt"),
                run_id: None,
                turn_id: turn,
                step_id: None,
                actor: Actor::Scheduler,
                created_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    /// The payload `inbox/append` writes: the item, serialized the way the
    /// writer serializes it.
    fn appended(item_id: &str, kind: InboxKind, inner: serde_json::Value) -> serde_json::Value {
        let item = InboxItem {
            item_id: InboxItemId::from_string(item_id),
            session_id: SessionId::from_string("corrupt"),
            kind,
            payload: inner,
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };
        serde_json::to_value(&item).expect("an item serializes the way the writer writes it")
    }

    /// One half of YA-69's split. The payload of `inbox/appended` *is* an item
    /// (`agent-kernel/src/inbox.rs:64`), so one that is not an item at all is an
    /// anomaly, not an item with nothing to say. The old code read it with
    /// `if let Ok(item)`, so this payload was silently dropped.
    #[test]
    fn an_inbox_payload_that_is_not_an_item_is_refused() {
        let definition = ModelContextProjection::new();
        let folded = definition.apply(
            &definition.init(),
            &event(EventType::INBOX_APPENDED, 0, serde_json::json!({ "kind": "next-turn" }), None),
        );

        let (event_type, seq, reason) = refusal(folded);
        assert_eq!(event_type.as_str(), EventType::INBOX_APPENDED);
        assert_eq!(seq, EventSeq::new(0));
        assert!(
            reason.contains("inbox item"),
            "the reason must say what could not be read: {reason}"
        );
    }

    /// The other half, and the reason the two must be told apart: an item that
    /// carries no text is a *representable* outcome — a wakeup has nothing to
    /// say — and refusing it would call ordinary traffic corruption. This is the
    /// guard against a fix that fails closed on both halves.
    #[test]
    fn an_item_with_no_text_is_ordinary_traffic() {
        let definition = ModelContextProjection::new();
        let opened = definition
            .apply(
                &definition.init(),
                &event(
                    EventType::INBOX_APPENDED,
                    0,
                    appended("w", InboxKind::Wakeup, serde_json::json!({})),
                    None,
                ),
            )
            .expect("an item with nothing to say folds like any other");
        assert!(opened.inbox_text.is_empty(), "and contributes no text");

        // The turn it opens then emits no message rather than failing.
        let folded = definition
            .apply(
                &opened,
                &event(
                    EventType::TURN_STARTED,
                    1,
                    serde_json::json!({ "inbox_item": "w" }),
                    Some(TurnId::new()),
                ),
            )
            .expect("a turn for an item with no text is not an error");
        assert!(definition.view(&folded).is_empty());
    }

    #[test]
    fn a_turn_started_without_its_item_is_refused() {
        let definition = ModelContextProjection::new();
        let folded = definition.apply(
            &definition.init(),
            &event(EventType::TURN_STARTED, 0, serde_json::json!({}), Some(TurnId::new())),
        );

        let (_, _, reason) = refusal(folded);
        assert!(reason.contains("inbox_item"), "{reason}");
    }

    #[test]
    fn a_turn_steered_without_its_text_is_refused() {
        let definition = ModelContextProjection::new();
        let folded = definition.apply(
            &definition.init(),
            &event(
                EventType::TURN_STEERED,
                0,
                serde_json::json!({ "item_id": "n", "after_step_id": "s" }),
                Some(TurnId::new()),
            ),
        );

        let (_, _, reason) = refusal(folded);
        assert!(reason.contains("text"), "{reason}");
    }

    /// A stored `run/started` carrying `payload`, named to `run`.
    fn run_started(seq: u64, run: &RunId, payload: serde_json::Value) -> StoredEvent {
        let mut stored = event(EventType::RUN_STARTED, seq, payload, None);
        stored.event.run_id = Some(run.clone());
        stored
    }

    fn instructions_payload(text: &str) -> serde_json::Value {
        serde_json::json!({
            "instructions": RunInstructions {
                text: text.to_string(),
                project_file: Some(PROJECT_FILE.to_string()),
                builtin: "test/1".to_string(),
            }
        })
    }

    const PROJECT_FILE: &str = "AGENTS.md";

    #[test]
    fn the_run_instructions_lead_the_transcript() {
        let definition = ModelContextProjection::new();
        let run = RunId::from_string("run-1");
        let state = definition
            .apply(&definition.init(), &run_started(0, &run, instructions_payload("be nice")))
            .expect("instructions written the way the scheduler writes them must fold");

        let view = definition.view(&state);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].role, MessageRole::System);
        // The uid is the run's, so this is the same message the loop placed
        // before `run/started` existed rather than a copy of it.
        assert_eq!(view[0].message_uid.as_str(), "msg/system/run-1");
        assert_eq!(view[0].content, vec![ContentBlock::text("be nice")]);
    }

    #[test]
    fn a_later_run_replaces_the_instructions_instead_of_adding_a_second_system_message() {
        let definition = ModelContextProjection::new();
        let first = RunId::from_string("run-1");
        let second = RunId::from_string("run-2");
        let state = definition
            .apply(&definition.init(), &run_started(0, &first, instructions_payload("first")))
            .unwrap();
        let state = definition
            .apply(&state, &run_started(1, &second, instructions_payload("second")))
            .unwrap();

        let view = definition.view(&state);
        assert_eq!(view.len(), 1, "a request has one system message");
        assert_eq!(view[0].message_uid.as_str(), "msg/system/run-2");
        assert_eq!(view[0].content, vec![ContentBlock::text("second")]);
    }

    #[test]
    fn a_run_without_instructions_leaves_none_in_the_transcript() {
        let definition = ModelContextProjection::new();
        let run = RunId::from_string("run-1");
        let state = definition
            .apply(&definition.init(), &run_started(0, &run, instructions_payload("first")))
            .unwrap();
        // A scheduler with no source writes `run/started` with no field at all.
        let state = definition.apply(&state, &run_started(1, &run, serde_json::json!({}))).unwrap();

        assert!(definition.view(&state).is_empty());
    }

    #[test]
    fn unreadable_instructions_are_refused_not_skipped() {
        let definition = ModelContextProjection::new();
        let run = RunId::from_string("run-1");
        let (_, _, reason) = refusal(definition.apply(
            &definition.init(),
            &run_started(0, &run, serde_json::json!({ "instructions": 5 })),
        ));
        assert!(reason.contains("instructions"), "{reason}");
    }

    #[test]
    fn instructions_without_a_run_are_refused() {
        let definition = ModelContextProjection::new();
        // `event` leaves `run_id` empty; `run/started` without one cannot name
        // the message's uid, so it cannot rebuild what the model was shown.
        let (_, _, reason) = refusal(definition.apply(
            &definition.init(),
            &event(EventType::RUN_STARTED, 0, instructions_payload("x"), None),
        ));
        assert!(reason.contains("run_id"), "{reason}");
    }

    #[test]
    fn a_compaction_replaces_the_named_messages_with_its_summary_at_the_front() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let second = ModelMessage::user_for_turn(&TurnId::from_string("t2"), "second");
        let state = ModelContextState {
            messages: vec![first.clone(), second.clone()],
            ..Default::default()
        };

        let state = definition
            .apply(
                &state,
                &event(
                    EventType::CONTEXT_COMPACTED,
                    1,
                    serde_json::json!({
                        "attempt_id": "attempt-7",
                        "summary": "we talked about first",
                        "summarized": [{
                            "message_uid": first.message_uid.as_str(),
                            "role": "user",
                            "hash": "h",
                        }],
                    }),
                    None,
                ),
            )
            .expect("a compaction written by this build must fold");

        let view = definition.view(&state);
        assert_eq!(view.len(), 2);
        assert_eq!(view[0].message_uid.as_str(), "msg/summary/attempt-7");
        assert_eq!(view[0].role, MessageRole::System);
        assert_eq!(view[0].content, vec![ContentBlock::text("we talked about first")]);
        assert_eq!(view[1], second, "the retained message is untouched");
    }

    #[test]
    fn a_ledger_follows_the_summary_it_was_recorded_with() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let state = ModelContextState { messages: vec![first.clone()], ..Default::default() };

        let state = definition
            .apply(
                &state,
                &event(
                    EventType::CONTEXT_COMPACTED,
                    1,
                    serde_json::json!({
                        "attempt_id": "attempt-7",
                        "summary": "the goal",
                        "ledger": {
                            "message_uid": "msg/ledger/attempt-7",
                            "view_version": "ledger/1",
                            "text": "1. fs.read {\"path\":\"a\"} -> ok: text",
                        },
                        "summarized": [{ "message_uid": first.message_uid.as_str() }],
                    }),
                    None,
                ),
            )
            .expect("a compaction with a ledger folds");

        let view = definition.view(&state);
        assert_eq!(view.len(), 2);
        assert_eq!(view[0].message_uid.as_str(), "msg/summary/attempt-7");
        assert_eq!(view[1].message_uid.as_str(), "msg/ledger/attempt-7");
        assert_eq!(view[1].role, MessageRole::System);
        assert!(view[1].text_content().contains("fs.read"));
    }

    #[test]
    fn a_user_record_follows_the_ledger_it_was_recorded_with() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let state = ModelContextState { messages: vec![first.clone()], ..Default::default() };

        let state = definition
            .apply(
                &state,
                &event(
                    EventType::CONTEXT_COMPACTED,
                    1,
                    serde_json::json!({
                        "attempt_id": "attempt-7",
                        "summary": "the goal",
                        "ledger": {
                            "message_uid": "msg/ledger/attempt-7",
                            "view_version": "ledger/1",
                            "text": "1. fs.read -> ok: text",
                        },
                        "usertext": {
                            "message_uid": "msg/usertext/attempt-7",
                            "view_version": "usertext/1",
                            "text": "Recorded user messages: never edit tests/spec.rs",
                        },
                        "summarized": [{ "message_uid": first.message_uid.as_str() }],
                    }),
                    None,
                ),
            )
            .expect("a compaction with a user record folds");

        let view = definition.view(&state);
        assert_eq!(view.len(), 3);
        assert_eq!(view[0].message_uid.as_str(), "msg/summary/attempt-7");
        assert_eq!(view[1].message_uid.as_str(), "msg/ledger/attempt-7");
        assert_eq!(view[2].message_uid.as_str(), "msg/usertext/attempt-7");
        assert_eq!(view[2].role, MessageRole::System);
        assert!(view[2].text_content().contains("never edit tests/spec.rs"));
    }

    #[test]
    fn a_failures_record_follows_the_user_record_it_was_recorded_with() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let state = ModelContextState { messages: vec![first.clone()], ..Default::default() };

        let state = definition
            .apply(
                &state,
                &event(
                    EventType::CONTEXT_COMPACTED,
                    1,
                    serde_json::json!({
                        "attempt_id": "attempt-7",
                        "summary": "the goal",
                        "ledger": {
                            "message_uid": "msg/ledger/attempt-7",
                            "view_version": "ledger/2",
                            "text": "1. shell.run -> failed: exit code 101",
                        },
                        "usertext": {
                            "message_uid": "msg/usertext/attempt-7",
                            "view_version": "usertext/1",
                            "text": "Recorded user messages: never edit tests/spec.rs",
                        },
                        "failures": {
                            "message_uid": "msg/failures/attempt-7",
                            "view_version": "failures/1",
                            "text": "Recorded failures: expected Ok(111), got Ok(74)",
                        },
                        "summarized": [{ "message_uid": first.message_uid.as_str() }],
                    }),
                    None,
                ),
            )
            .expect("a compaction with a failures record folds");

        let view = definition.view(&state);
        assert_eq!(view.len(), 4);
        assert_eq!(view[0].message_uid.as_str(), "msg/summary/attempt-7");
        assert_eq!(view[1].message_uid.as_str(), "msg/ledger/attempt-7");
        assert_eq!(view[2].message_uid.as_str(), "msg/usertext/attempt-7");
        assert_eq!(view[3].message_uid.as_str(), "msg/failures/attempt-7");
        assert_eq!(view[3].role, MessageRole::System);
        assert!(view[3].text_content().contains("expected Ok(111), got Ok(74)"));
    }

    #[test]
    fn a_failures_record_without_text_is_refused_rather_than_skipped() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let state = ModelContextState { messages: vec![first.clone()], ..Default::default() };
        let (_, _, reason) = refusal(definition.apply(
            &state,
            &event(
                EventType::CONTEXT_COMPACTED,
                1,
                serde_json::json!({
                    "attempt_id": "attempt-7",
                    "summary": "the goal",
                    "failures": { "view_version": "failures/1" },
                    "summarized": [{ "message_uid": first.message_uid.as_str() }],
                }),
                None,
            ),
        ));
        assert!(reason.contains("failures"), "{reason}");
    }

    #[test]
    fn a_user_record_without_text_is_refused_rather_than_skipped() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let state = ModelContextState { messages: vec![first.clone()], ..Default::default() };
        let (_, _, reason) = refusal(definition.apply(
            &state,
            &event(
                EventType::CONTEXT_COMPACTED,
                1,
                serde_json::json!({
                    "attempt_id": "attempt-7",
                    "summary": "the goal",
                    "usertext": { "view_version": "usertext/1" },
                    "summarized": [{ "message_uid": first.message_uid.as_str() }],
                }),
                None,
            ),
        ));
        assert!(reason.contains("usertext"), "{reason}");
    }

    #[test]
    fn a_ledger_without_text_is_refused_rather_than_skipped() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let state = ModelContextState { messages: vec![first.clone()], ..Default::default() };
        let (_, _, reason) = refusal(definition.apply(
            &state,
            &event(
                EventType::CONTEXT_COMPACTED,
                1,
                serde_json::json!({
                    "attempt_id": "attempt-7",
                    "summary": "the goal",
                    "ledger": { "view_version": "ledger/1" },
                    "summarized": [{ "message_uid": first.message_uid.as_str() }],
                }),
                None,
            ),
        ));
        assert!(reason.contains("ledger"), "{reason}");
    }

    #[test]
    fn a_second_compaction_replaces_the_first_summary() {
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let state = ModelContextState { messages: vec![first.clone()], ..Default::default() };
        let state = definition
            .apply(
                &state,
                &event(
                    EventType::CONTEXT_COMPACTED,
                    1,
                    serde_json::json!({
                        "attempt_id": "attempt-7",
                        "summary": "one",
                        "summarized": [{ "message_uid": first.message_uid.as_str() }],
                    }),
                    None,
                ),
            )
            .unwrap();
        let state = definition
            .apply(
                &state,
                &event(
                    EventType::CONTEXT_COMPACTED,
                    2,
                    serde_json::json!({
                        "attempt_id": "attempt-8",
                        "summary": "one and two",
                        "summarized": [{ "message_uid": "msg/summary/attempt-7" }],
                    }),
                    None,
                ),
            )
            .unwrap();

        let view = definition.view(&state);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].message_uid.as_str(), "msg/summary/attempt-8");
        assert_eq!(view[0].content, vec![ContentBlock::text("one and two")]);
    }

    #[test]
    fn an_in_flight_compaction_puts_the_summary_where_its_messages_were() {
        // A turn-start compaction replaces the front of the conversation, but
        // the loop also compacts a turn's own step traffic mid-turn. That
        // summary must land where those messages were: at the front it would
        // describe newer traffic from ahead of the older history it does not
        // cover.
        let definition = ModelContextProjection::new();
        let first = ModelMessage::user_for_turn(&TurnId::from_string("t1"), "first");
        let step = ModelMessage::assistant_for_attempt(
            &AttemptId::from_string("a1"),
            vec![ContentBlock::text("working")],
        );
        let later = ModelMessage::user_for_steering(
            &TurnId::from_string("t1"),
            &InboxItemId::from_string("i1"),
            "same turn, steered",
        );
        let last = ModelMessage::user_for_turn(&TurnId::from_string("t2"), "next turn");
        let state = ModelContextState {
            messages: vec![first.clone(), step.clone(), later.clone(), last.clone()],
            ..Default::default()
        };
        let state = definition
            .apply(
                &state,
                &event(
                    EventType::CONTEXT_COMPACTED,
                    1,
                    serde_json::json!({
                        "attempt_id": "attempt-9",
                        "summary": "what the steps did",
                        "summarized": [
                            { "message_uid": step.message_uid.as_str() },
                            { "message_uid": later.message_uid.as_str() },
                        ],
                    }),
                    None,
                ),
            )
            .unwrap();

        let view = definition.view(&state);
        assert_eq!(view.len(), 3);
        assert_eq!(view[0], first);
        assert_eq!(view[1].message_uid.as_str(), "msg/summary/attempt-9");
        assert_eq!(view[1].content, vec![ContentBlock::text("what the steps did")]);
        assert_eq!(view[2], last);
    }

    #[test]
    fn the_latest_reported_usage_is_kept_across_a_response_that_reports_none() {
        let definition = ModelContextProjection::new();
        let mut measured = ModelResponse::text("answer");
        measured.usage =
            Some(Usage { input_tokens: Some(900), output_tokens: Some(12), ..Usage::default() });
        let state = definition
            .apply(&definition.init(), &response_completed(1, settled(measured)))
            .unwrap();
        assert_eq!(state.last_usage().and_then(|usage| usage.input_tokens), Some(900));

        // A provider that reports nothing must not erase the last number it
        // did report: the trigger keeps a real measure instead of silently
        // falling back to the estimate.
        let state = definition
            .apply(&state, &response_completed(2, settled(ModelResponse::text("again"))))
            .unwrap();
        assert_eq!(state.last_usage().and_then(|usage| usage.input_tokens), Some(900));
    }

    #[test]
    fn a_compaction_naming_a_message_it_does_not_hold_is_refused() {
        let definition = ModelContextProjection::new();
        let (_, _, reason) = refusal(definition.apply(
            &definition.init(),
            &event(
                EventType::CONTEXT_COMPACTED,
                1,
                serde_json::json!({
                    "attempt_id": "attempt-7",
                    "summary": "ghost",
                    "summarized": [{ "message_uid": "msg/user/missing" }],
                }),
                None,
            ),
        ));
        assert!(reason.contains("not in the transcript"), "{reason}");
    }
}
