//! Slice A3 — the kernel half of the intent adjudicator.
//!
//! The rule layer ([`crate::adjudicate`]) covers control words and nothing
//! else. A message it cannot route is queued untouched and marked `unclear` in
//! its item payload; this module is what that marker is resolved by, **once**,
//! at the boundary just before the next turn would run.
//!
//! Three separations are the point of the slice:
//!
//! - **The model suggests; the kernel decides.** [`decide`] is a pure function
//!   over one [`AdjudicationOutcome`](agent_ports::intent_adjudication::AdjudicationOutcome).
//!   A low confidence, a widened scope, a request for confirmation, a
//!   schedule with no time, a failure, or an abstention all end in the same
//!   place: no side effect and one clarification question. The model's
//!   `reason` is never an authorization.
//! - **The suggestion and the decision are recorded apart.**
//!   `intent/adjudication.completed` carries both, in separate fields, so a
//!   reader can tell a misclassification from a bad policy. When nothing usable
//!   came back, `intent/adjudication.unclear` says so instead — a different
//!   answer to "what do I do now?", not a `completed` with a null decision.
//! - **Effects land in one append, at a safe boundary.** The decision event,
//!   the discards it causes, and the intent records that go with them are one
//!   batch: no reader can see a queue emptied by a decision that was never
//!   written, or a decision whose effects have not landed.
//!
//! What this slice deliberately does not do:
//!
//! - **No merge window.** The design asks for a 500–1000 ms window that folds
//!   consecutive short messages into one batch. A deferral like that needs a
//!   timer the scheduler does not own, and pretending to merge by looking at
//!   the queue once is not the same thing. One call per boundary, over the
//!   messages already queued, is what this build does; the deviation is
//!   recorded in CONTRACT §7 #165.
//! - **No task versions.** `preserve`/`invalidate` and `target_task_id` are
//!   recorded and not acted on: there is no task revision to attach them to
//!   until A4/A5. The queue effects below are the ones this build can honestly
//!   apply.
//! - **No side effect on doubt.** An `unclear` verdict leaves the queue exactly
//!   as it was — the message keeps its turn, and the person gets the
//!   clarification in `status` and in the answer the turn produces.

use std::collections::HashSet;
use std::sync::Arc;

use agent_ports::intent_adjudication::{
    AdjudicationInput, AdjudicationOutcome, AdjudicationRelation, AdjudicationReport, RecentLine,
};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, InboxItemId, IntentId, SessionId};
use agent_protocol::inbox::{InboxItem, InboxKind};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::inbox::{DiscardCause, ROUTE, item_intent_id};
use crate::scheduler::{Scheduler, SchedulerError, draft};

/// The confidence below which a suggestion is not acted on.
///
/// The design is explicit that this is *not* the model's self-report alone: it
/// is one of several checks (scope, confirmation, and the relation itself) and
/// the kernel takes the most conservative answer among them. Calibration is
/// what would make this number meaningful; until then it is deliberately high
/// enough that a guess is not enough.
pub const MIN_CONFIDENCE: f64 = 0.6;

/// The trigger recorded on a request: the rules left this message `unclear`.
pub const TRIGGER_UNCLEAR_ROUTE: &str = "unclear_route";

/// The one question a person is asked when nothing usable came back.
///
/// Deterministic on purpose: a clarification the model wrote would be the model
/// speaking for the kernel at the moment the kernel decided not to trust it.
pub fn clarification_question() -> String {
    "我不确定这条消息是要继续手上的任务、改写它，还是停下。你是想让我继续，还是换一件事？"
        .to_string()
}

/// What the kernel decided to do about one suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Leave the queue alone; the message is a continuation.
    Continue,
    /// The message replaces what was queued around it.
    Supersede,
    /// The message withdraws what was queued around it.
    Cancel,
    /// Nothing usable came back: no side effect, one question.
    Unclear,
}

impl Decision {
    /// The word written in `intent/adjudication.completed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Supersede => "supersede",
            Self::Cancel => "cancel",
            Self::Unclear => "unclear",
        }
    }
}

/// A decision and the reason the kernel reached it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    pub decision: Decision,
    /// A stable label: `accepted`, `low_confidence`, `scope_expansion`,
    /// `requires_confirmation`, `schedule_needs_time`, `unclear`, `abstained`,
    /// or the backend's failure kind.
    pub cause: String,
}

impl Decided {
    fn unclear(cause: impl Into<String>) -> Self {
        Self { decision: Decision::Unclear, cause: cause.into() }
    }

    /// Whether this decision writes `completed` rather than `unclear`.
    pub fn is_applied(&self) -> bool {
        self.decision != Decision::Unclear
    }
}

/// The kernel's matrix over one call's outcome.
///
/// Conservative in one direction only: every doubt collapses to `unclear`,
/// which is the state with no side effect. `scope_expansion` is checked before
/// the relation so that a relation the kernel would otherwise apply cannot be
/// applied on the strength of a widened scope — the design says a model does
/// not approve its own expansion.
pub fn decide(outcome: &AdjudicationOutcome) -> Decided {
    let suggestion = match outcome {
        AdjudicationOutcome::Failed { kind } => return Decided::unclear(kind.clone()),
        AdjudicationOutcome::Abstained => return Decided::unclear("abstained"),
        AdjudicationOutcome::Suggested(suggestion) => suggestion,
    };
    if suggestion.scope_expansion {
        return Decided::unclear("scope_expansion");
    }
    if suggestion.requires_confirmation {
        return Decided::unclear("requires_confirmation");
    }
    if suggestion.confidence < MIN_CONFIDENCE {
        return Decided::unclear("low_confidence");
    }
    let decision = match suggestion.relation {
        AdjudicationRelation::Continue => Decision::Continue,
        AdjudicationRelation::ReviseTask | AdjudicationRelation::NewTask => Decision::Supersede,
        AdjudicationRelation::Stop => Decision::Cancel,
        // A scheduled job needs a due time this contract does not carry; the
        // person picks it rather than the kernel inventing one.
        AdjudicationRelation::Schedule => return Decided::unclear("schedule_needs_time"),
        AdjudicationRelation::Unclear => return Decided::unclear("unclear"),
    };
    Decided { decision, cause: "accepted".to_string() }
}

/// What `status` reports about the latest adjudication.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdjudicationStatus {
    /// `"completed"` or `"unclear"`.
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// The payload of an adjudication event, read loosely.
///
/// Loose on purpose: this is a display fold, and a field a later build adds
/// must not make an older reader fail the whole status call. The fields that
/// matter are typed; the rest are strings.
#[derive(Debug, Default, Deserialize)]
struct StatusPayload {
    #[serde(default)]
    intent_id: Option<String>,
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    cause: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    route_id: Option<String>,
    #[serde(default)]
    latency_ms: Option<u64>,
    #[serde(default)]
    suggestion: Option<SuggestionView>,
}

#[derive(Debug, Deserialize)]
struct SuggestionView {
    #[serde(default)]
    relation: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
}

/// The latest adjudication recorded in a session's log, if any.
pub fn latest(events: &[StoredEvent]) -> Result<Option<AdjudicationStatus>, serde_json::Error> {
    let mut latest: Option<AdjudicationStatus> = None;
    for event in events {
        let event_word = match event.event.event_type.as_str() {
            EventType::INTENT_ADJUDICATION_COMPLETED => "completed",
            EventType::INTENT_ADJUDICATION_UNCLEAR => "unclear",
            _ => continue,
        };
        let payload: StatusPayload = serde_json::from_value(event.event.payload.clone())?;
        latest = Some(AdjudicationStatus {
            event: event_word.to_string(),
            intent_id: payload.intent_id,
            decision: payload.decision,
            relation: payload.suggestion.as_ref().and_then(|s| s.relation.clone()),
            confidence: payload.suggestion.as_ref().and_then(|s| s.confidence),
            cause: payload.cause,
            question: payload.question,
            route_id: payload.route_id,
            latency_ms: payload.latency_ms,
        });
    }
    Ok(latest)
}

impl Scheduler {
    /// Give this scheduler the adjudicator it may ask about `unclear` messages.
    ///
    /// Set by the app layer, which is the only place that knows whether a
    /// decision backend was configured. A scheduler without one is not broken:
    /// it records every unclear message as unadjudicated and the message keeps
    /// its turn, which is the same thing a failed call produces.
    pub fn with_intent_adjudicator(
        mut self,
        adjudicator: Arc<dyn agent_ports::intent_adjudication::IntentAdjudicator>,
    ) -> Self {
        self.adjudicator = Some(adjudicator);
        self
    }

    /// The latest adjudication this session recorded, for `status`.
    pub async fn intent_adjudication(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<AdjudicationStatus>, SchedulerError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(latest(&events)?)
    }

    /// Resolve the newest queued `unclear` message against the work around it.
    ///
    /// Runs at the turn boundary, after controls and before the next turn is
    /// claimed — the one place in this build where a decision can be applied
    /// without cutting a step in half. It is idempotent: an item that already
    /// has an adjudication event is not asked about again, so a turn that fails
    /// to start does not produce a second call.
    pub(crate) async fn adjudicate_pending(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SchedulerError> {
        let Some(adjudicator) = self.adjudicator.clone() else {
            return Ok(());
        };

        let pending: Vec<InboxItem> = self
            .inbox
            .pending(session_id)
            .await?
            .into_iter()
            .filter(|item| item.kind == InboxKind::NextTurn)
            .collect();
        let Some(message) = pending.last() else {
            return Ok(());
        };
        if message.payload.get(ROUTE).and_then(Value::as_str) != Some("unclear") {
            return Ok(());
        }
        // Nothing around it to be about: with no other instruction queued the
        // relation is "new task" by definition, and asking a model would spend
        // a call to learn nothing.
        let others: Vec<InboxItem> = pending[..pending.len() - 1].to_vec();
        if others.is_empty() {
            return Ok(());
        }
        let Some(message_text) = message.payload.get("text").and_then(Value::as_str) else {
            return Ok(());
        };

        let history = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        if adjudicated_items(&history).contains(&message.item_id) {
            return Ok(());
        }

        let message_intent = item_intent_id(message);
        let input = AdjudicationInput {
            session_id: session_id.clone(),
            // No turn is open at this boundary; naming one would be a lie about
            // what the decision was made during.
            turn_id: None,
            context_revision: history.last().map(|event| event.seq.as_u64()).unwrap_or(0),
            task_summary: task_summary(&history)?,
            pending: others
                .iter()
                .filter_map(|item| item.payload.get("text").and_then(Value::as_str))
                .map(str::to_string)
                .collect(),
            recent: recent_turns(&history),
            // No step is running at this boundary, so there is no tool result
            // to summarize. The key is left absent rather than filled with
            // something a backend might read as evidence.
            untrusted_tool_summary: None,
            message: message_text.to_string(),
        };

        // Recorded before the call: a process that dies during it leaves "we
        // asked" in the log, which is a fact; nothing at all is not.
        let requested = json!({
            "intent_id": message_intent,
            "item_id": message.item_id,
            "trigger": TRIGGER_UNCLEAR_ROUTE,
            "batch_size": 1 + others.len(),
            "pack_version": agent_protocol::intent_adjudication::PACK_VERSION,
            "prompt_hash": agent_protocol::intent_adjudication::prompt_hash(),
            "input_hash": agent_protocol::intent_adjudication::input_hash(&input),
        });
        self.append_events(
            session_id,
            vec![draft(
                session_id,
                EventType::INTENT_ADJUDICATION_REQUESTED,
                requested,
                Actor::Scheduler,
                None,
                None,
                None,
            )],
        )
        .await?;

        let report = adjudicator.adjudicate(&input).await;
        let decided = decide(&report.outcome);

        let mut drafts = Vec::new();
        match decided.decision {
            Decision::Continue | Decision::Unclear => {}
            Decision::Supersede => {
                for item in &others {
                    push_discard(&mut drafts, self, session_id, item, DiscardCause::Superseded)
                        .await?;
                    if let Some(intent) = item_intent_id(item) {
                        drafts.push(draft(
                            session_id,
                            EventType::INTENT_SUPERSEDED,
                            json!({
                                "intent_id": intent,
                                "superseded_by": message_intent,
                                "adjudicated": true,
                            }),
                            Actor::Scheduler,
                            None,
                            None,
                            None,
                        ));
                    }
                }
            }
            Decision::Cancel => {
                for item in &others {
                    push_discard(&mut drafts, self, session_id, item, DiscardCause::Cancelled)
                        .await?;
                    if let Some(intent) = item_intent_id(item) {
                        drafts.push(draft(
                            session_id,
                            EventType::INTENT_CANCELLED,
                            json!({
                                "intent_id": intent,
                                "reason": "withdrawn by an adjudicated stop",
                                "adjudicated": true,
                            }),
                            Actor::Scheduler,
                            None,
                            None,
                            None,
                        ));
                    }
                }
            }
        }

        let (event_type, question) = if decided.is_applied() {
            (EventType::INTENT_ADJUDICATION_COMPLETED, None)
        } else {
            (EventType::INTENT_ADJUDICATION_UNCLEAR, Some(clarification_question()))
        };
        drafts.insert(
            0,
            draft(
                session_id,
                event_type,
                event_payload(
                    &message.item_id,
                    message_intent.as_ref(),
                    &report,
                    &decided,
                    question,
                ),
                Actor::Scheduler,
                None,
                None,
                None,
            ),
        );
        self.append_events(session_id, drafts).await
    }

    /// Read the log's end and append at it under one lease.
    ///
    /// Every writer here does this: a read followed by an append at the read
    /// position, with nobody able to move the sequence in between (YA-36).
    async fn append_events(
        &self,
        session_id: &SessionId,
        drafts: Vec<EventDraft>,
    ) -> Result<(), SchedulerError> {
        let _lease = self.store.lease(session_id).await?;
        let next = self.next_seq(session_id).await?;
        self.store.append(session_id, next, drafts).await?;
        Ok(())
    }
}

/// The discard draft for one item, with a reason that says who did it.
async fn push_discard(
    drafts: &mut Vec<EventDraft>,
    scheduler: &Scheduler,
    session_id: &SessionId,
    item: &InboxItem,
    cause: DiscardCause,
) -> Result<(), SchedulerError> {
    let reason = match cause {
        DiscardCause::Cancelled => "withdrawn by an adjudicated stop",
        _ => "an adjudicated instruction replaces it",
    };
    drafts
        .push(scheduler.inbox.discard_draft(session_id, &item.item_id, cause, Some(reason)).await?);
    Ok(())
}

/// One adjudication event's payload: the call's audit fields, the suggestion,
/// and — when there is one — the kernel's decision.
fn event_payload(
    item_id: &InboxItemId,
    intent_id: Option<&IntentId>,
    report: &AdjudicationReport,
    decided: &Decided,
    question: Option<String>,
) -> Value {
    let suggestion = match &report.outcome {
        AdjudicationOutcome::Suggested(suggestion) => Some(suggestion),
        _ => None,
    };
    let mut payload = json!({
        "intent_id": intent_id,
        "item_id": item_id,
        "route_id": report.route_id,
        "revision": report.revision,
        "pack_version": report.pack_version,
        "prompt_hash": report.prompt_hash,
        "input_hash": report.input_hash,
        "latency_ms": report.latency_ms,
        "suggestion": suggestion,
        "cause": decided.cause,
    });
    object_mut(&mut payload)
        .insert("decision".to_string(), Value::String(decided.decision.as_str().to_string()));
    if let Some(question) = question {
        object_mut(&mut payload).insert("question".to_string(), Value::String(question));
    }
    payload
}

fn object_mut(value: &mut Value) -> &mut serde_json::Map<String, Value> {
    value.as_object_mut().expect("payload is built as an object")
}

/// The work state's own account of the current task, if it recorded one.
fn task_summary(events: &[StoredEvent]) -> Result<Option<String>, SchedulerError> {
    let work = crate::state::current(events)?.unwrap_or_default();
    let mut parts = Vec::new();
    if !work.goal.is_empty() {
        parts.push(format!("goal: {}", work.goal));
    }
    if !work.next.is_empty() {
        parts.push(format!("next: {}", work.next));
    }
    Ok(if parts.is_empty() { None } else { Some(parts.join("; ")) })
}

/// The last few exchanges, as the adjudicator is allowed to see them.
///
/// User lines come from `intent/recorded` — the person's own words — and
/// assistant lines from the text blocks of the last settled model response.
/// Tool traffic is deliberately absent: it is the input the design says must be
/// trimmed and marked untrusted, and at this boundary there is none of it to
/// summarize.
fn recent_turns(events: &[StoredEvent]) -> Vec<RecentLine> {
    let mut lines: Vec<RecentLine> = Vec::new();
    for event in events {
        match event.event.event_type.as_str() {
            EventType::INTENT_RECORDED => {
                if let Some(text) = event.event.payload.get("text").and_then(Value::as_str) {
                    lines.push(RecentLine { role: "user".to_string(), text: text.to_string() });
                }
            }
            EventType::MODEL_RESPONSE_COMPLETED => {
                if let Some(text) = assistant_text(&event.event.payload) {
                    lines.push(RecentLine { role: "assistant".to_string(), text });
                }
            }
            _ => {}
        }
    }
    let keep = 4.min(lines.len());
    lines.split_off(lines.len() - keep)
}

/// The text blocks of one recorded model response, joined.
fn assistant_text(payload: &Value) -> Option<String> {
    let blocks = payload.get("response")?.get("content_blocks")?.as_array()?;
    let text: Vec<&str> = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect();
    if text.is_empty() { None } else { Some(text.join(" ")) }
}

/// Items that already have an adjudication event.
fn adjudicated_items(events: &[StoredEvent]) -> HashSet<InboxItemId> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.event.event_type.as_str(),
                EventType::INTENT_ADJUDICATION_REQUESTED
                    | EventType::INTENT_ADJUDICATION_COMPLETED
                    | EventType::INTENT_ADJUDICATION_UNCLEAR
            )
        })
        .filter_map(|event| event.event.payload.get("item_id").cloned())
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_loop::DefaultAgentLoop;
    use agent_ports::event_store::EventStore;
    use agent_ports::intent_adjudication::IntentAdjudicator;
    use agent_protocol::inbox::InboxItemStatus;
    use agent_protocol::intent_adjudication::AdjudicationSuggestion;
    use agent_testkit::{
        FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine,
        one_model_router,
    };
    use chrono::Utc;
    use serde_json::json;
    use std::sync::Mutex;

    fn suggestion(relation: AdjudicationRelation, confidence: f64) -> AdjudicationSuggestion {
        AdjudicationSuggestion {
            relation,
            target_task_id: None,
            confidence,
            preserve: Vec::new(),
            invalidate: Vec::new(),
            scope_expansion: false,
            requires_confirmation: false,
            reason: "test".to_string(),
        }
    }

    /// A scheduler that can be driven without a model: nothing in these tests
    /// opens a turn.
    fn harness(
        outcome: AdjudicationOutcome,
    ) -> (Scheduler, Arc<FakeEventStore>, SessionId, Arc<Scripted>) {
        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(Vec::new()));
        let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
        let loop_ = Arc::new(DefaultAgentLoop::new(
            model,
            gateway,
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let scripted = Arc::new(Scripted { outcome, calls: Mutex::new(Vec::new()) });
        let scheduler = Scheduler::new(store.clone(), loop_, 8)
            .with_intent_adjudicator(scripted.clone() as Arc<dyn IntentAdjudicator>);
        (scheduler, store, SessionId::from_string("adj-session"), scripted)
    }

    /// An adjudicator that answers with one scripted outcome and records what
    /// it was asked.
    struct Scripted {
        outcome: AdjudicationOutcome,
        calls: Mutex<Vec<AdjudicationInput>>,
    }

    impl Scripted {
        fn calls(&self) -> Vec<AdjudicationInput> {
            self.calls.lock().expect("not poisoned").clone()
        }
    }

    #[async_trait::async_trait]
    impl IntentAdjudicator for Scripted {
        async fn adjudicate(&self, input: &AdjudicationInput) -> AdjudicationReport {
            self.calls.lock().expect("not poisoned").push(input.clone());
            AdjudicationReport {
                route_id: "scripted".to_string(),
                revision: Some("rev-1".to_string()),
                pack_version: agent_protocol::intent_adjudication::PACK_VERSION.to_string(),
                prompt_hash: agent_protocol::intent_adjudication::prompt_hash(),
                input_hash: agent_protocol::intent_adjudication::input_hash(input),
                latency_ms: 3,
                outcome: self.outcome.clone(),
            }
        }
    }

    async fn event_types(store: &FakeEventStore, sid: &SessionId) -> Vec<String> {
        store
            .read_stream(sid, EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event.event_type.to_string())
            .collect()
    }

    async fn pending(store: &Arc<FakeEventStore>, sid: &SessionId) -> Vec<InboxItem> {
        crate::inbox::Inbox::new(store.clone()).pending(sid).await.unwrap()
    }

    /// Queue two ordinary instructions, then one the rules must leave unclear.
    ///
    /// The two are there so the message has work around it to be about — the
    /// trigger for asking a model at all.
    async fn queue_with_unclear_tail(scheduler: &Scheduler, sid: &SessionId) {
        let now = Utc::now();
        scheduler.inbox.submit_user_message(sid, "修复 junction 逃逸", now).await.unwrap();
        scheduler.inbox.submit_user_message(sid, "继续", now).await.unwrap();
        scheduler.inbox.submit_user_message(sid, "我觉得应该停止了", now).await.unwrap();
    }

    #[tokio::test]
    async fn a_rule_routed_message_never_reaches_the_adjudicator() {
        // No `unclear` marker anywhere: the rules answered every message, so a
        // model is not asked and nothing is spent.
        for text in ["修复 junction 逃逸", "继续", "停止"] {
            let (scheduler, _store, sid, scripted) = harness(AdjudicationOutcome::Abstained);
            scheduler.inbox.submit_user_message(&sid, "先做这个", Utc::now()).await.unwrap();
            scheduler.inbox.submit_user_message(&sid, text, Utc::now()).await.unwrap();

            scheduler.adjudicate_pending(&sid).await.unwrap();
            assert_eq!(scripted.calls().len(), 0, "{text:?} was routed by the rules");
        }
    }

    #[tokio::test]
    async fn one_unclear_message_beside_queued_work_is_asked_about_exactly_once() {
        let (scheduler, store, sid, scripted) = harness(AdjudicationOutcome::Suggested(
            suggestion(AdjudicationRelation::Continue, 0.9),
        ));
        queue_with_unclear_tail(&scheduler, &sid).await;

        scheduler.adjudicate_pending(&sid).await.unwrap();

        let calls = scripted.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message, "我觉得应该停止了");
        assert_eq!(calls[0].pending.len(), 2, "the queued instructions are named");
        assert!(calls[0].turn_id.is_none(), "no turn is open at this boundary");

        let types = event_types(&store, &sid).await;
        assert_eq!(
            types.iter().filter(|t| t.as_str() == EventType::INTENT_ADJUDICATION_REQUESTED).count(),
            1
        );
        assert_eq!(
            types.iter().filter(|t| t.as_str() == EventType::INTENT_ADJUDICATION_COMPLETED).count(),
            1
        );

        // Idempotent: the item already has an adjudication, so a second
        // boundary does not pay for a second call.
        scheduler.adjudicate_pending(&sid).await.unwrap();
        assert_eq!(scripted.calls().len(), 1);
    }

    #[tokio::test]
    async fn an_accepted_new_task_supersedes_the_queued_work_and_keeps_the_new_item() {
        let (scheduler, store, sid, _scripted) = harness(AdjudicationOutcome::Suggested(
            suggestion(AdjudicationRelation::NewTask, 0.92),
        ));
        queue_with_unclear_tail(&scheduler, &sid).await;

        scheduler.adjudicate_pending(&sid).await.unwrap();

        let left = pending(&store, &sid).await;
        assert_eq!(left.len(), 1, "only the adjudicated message is still queued");
        assert_eq!(left[0].payload["text"], "我觉得应该停止了");

        let types = event_types(&store, &sid).await;
        let superseded =
            types.iter().filter(|t| t.as_str() == EventType::INTENT_SUPERSEDED).count();
        assert_eq!(superseded, 2, "both queued instructions were replaced");
        let discarded = store
            .read_stream(&sid, EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .map(|event| event.event.payload)
            .collect::<Vec<_>>();
        assert_eq!(discarded.len(), 2);
        assert!(discarded.iter().all(|payload| payload["cause"] == "superseded"), "{discarded:?}");
    }

    #[tokio::test]
    async fn an_accepted_stop_withdraws_the_queue_and_leaves_the_message_its_turn() {
        let (scheduler, store, sid, _scripted) =
            harness(AdjudicationOutcome::Suggested(suggestion(AdjudicationRelation::Stop, 0.95)));
        queue_with_unclear_tail(&scheduler, &sid).await;

        scheduler.adjudicate_pending(&sid).await.unwrap();

        let left = pending(&store, &sid).await;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].payload["text"], "我觉得应该停止了");
        let types = event_types(&store, &sid).await;
        assert_eq!(
            types.iter().filter(|t| t.as_str() == EventType::INTENT_CANCELLED).count(),
            2,
            "a stop withdraws what was queued; it does not supersede it"
        );
        assert_eq!(types.iter().filter(|t| t.as_str() == EventType::INTENT_SUPERSEDED).count(), 0);
    }

    #[tokio::test]
    async fn doubt_has_no_side_effect_and_asks_one_question() {
        let mut confirm = suggestion(AdjudicationRelation::Stop, 0.99);
        confirm.requires_confirmation = true;
        let doubtful = [
            AdjudicationOutcome::Suggested(suggestion(AdjudicationRelation::NewTask, 0.2)),
            AdjudicationOutcome::Suggested(suggestion(AdjudicationRelation::Schedule, 0.99)),
            AdjudicationOutcome::Suggested(confirm),
            AdjudicationOutcome::Failed { kind: "timeout".to_string() },
            AdjudicationOutcome::Abstained,
        ];
        for outcome in doubtful {
            let (scheduler, store, sid, _scripted) = harness(outcome.clone());
            queue_with_unclear_tail(&scheduler, &sid).await;

            scheduler.adjudicate_pending(&sid).await.unwrap();

            assert_eq!(pending(&store, &sid).await.len(), 3, "{outcome:?} left the queue alone");
            let types = event_types(&store, &sid).await;
            assert_eq!(
                types
                    .iter()
                    .filter(|t| t.as_str() == EventType::INTENT_ADJUDICATION_UNCLEAR)
                    .count(),
                1,
                "{outcome:?}"
            );
            assert_eq!(
                types
                    .iter()
                    .filter(|t| t.as_str() == EventType::INTENT_ADJUDICATION_COMPLETED)
                    .count(),
                0
            );
            let latest = scheduler.intent_adjudication(&sid).await.unwrap().expect("a decision");
            assert_eq!(latest.event, "unclear");
            assert!(latest.question.is_some());
        }
    }

    #[tokio::test]
    async fn a_scope_expansion_is_not_applied_even_at_certainty() {
        let mut expansion = suggestion(AdjudicationRelation::NewTask, 1.0);
        expansion.scope_expansion = true;
        let (scheduler, store, sid, _scripted) = harness(AdjudicationOutcome::Suggested(expansion));
        queue_with_unclear_tail(&scheduler, &sid).await;

        scheduler.adjudicate_pending(&sid).await.unwrap();

        assert_eq!(pending(&store, &sid).await.len(), 3);
        let latest = scheduler.intent_adjudication(&sid).await.unwrap().expect("a decision");
        assert_eq!(latest.cause.as_deref(), Some("scope_expansion"));
    }

    #[tokio::test]
    async fn consecutive_unclear_messages_are_asked_about_as_one_batch() {
        let (scheduler, _store, sid, scripted) = harness(AdjudicationOutcome::Suggested(
            suggestion(AdjudicationRelation::Continue, 0.9),
        ));
        scheduler.inbox.submit_user_message(&sid, "先做这个", Utc::now()).await.unwrap();
        for text in ["我觉得应该停止了", "这个测试继续跑下去会不会太久", "先别取消，我看看"]
        {
            scheduler.inbox.submit_user_message(&sid, text, Utc::now()).await.unwrap();
        }

        scheduler.adjudicate_pending(&sid).await.unwrap();

        let calls = scripted.calls();
        assert_eq!(calls.len(), 1, "one call per boundary, not one per message");
        assert_eq!(calls[0].message, "先别取消，我看看", "the newest message is the subject");
        assert_eq!(calls[0].pending.len(), 3, "the earlier ones are described as queued work");
    }

    #[tokio::test]
    async fn an_unclear_message_with_nothing_queued_is_not_asked_about() {
        // No other instruction, so the relation is "new task" by definition and
        // a model would be asked to confirm arithmetic.
        let (scheduler, _store, sid, scripted) =
            harness(AdjudicationOutcome::Suggested(suggestion(AdjudicationRelation::NewTask, 0.9)));
        scheduler.inbox.submit_user_message(&sid, "我觉得应该停止了", Utc::now()).await.unwrap();

        scheduler.adjudicate_pending(&sid).await.unwrap();
        assert_eq!(scripted.calls().len(), 0);
    }

    #[tokio::test]
    async fn a_log_without_route_markers_is_not_adjudicated() {
        // Items written before this slice carry no `route`; guessing one from
        // the current queue would answer a question the write path never asked.
        let (scheduler, store, sid, scripted) =
            harness(AdjudicationOutcome::Suggested(suggestion(AdjudicationRelation::NewTask, 0.9)));
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, json!({ "text": "old" }), 0, Actor::User)
            .await
            .unwrap();
        scheduler
            .inbox
            .append(&sid, InboxKind::NextTurn, json!({ "text": "older" }), 0, Actor::User)
            .await
            .unwrap();

        scheduler.adjudicate_pending(&sid).await.unwrap();
        assert_eq!(scripted.calls().len(), 0);
        assert_eq!(pending(&store, &sid).await.len(), 2);
        assert!(
            pending(&store, &sid).await.iter().all(|item| item.status == InboxItemStatus::Pending)
        );
    }

    #[test]
    fn an_accepted_relation_is_applied() {
        for (relation, decision) in [
            (AdjudicationRelation::Continue, Decision::Continue),
            (AdjudicationRelation::NewTask, Decision::Supersede),
            (AdjudicationRelation::ReviseTask, Decision::Supersede),
            (AdjudicationRelation::Stop, Decision::Cancel),
        ] {
            let decided = decide(&AdjudicationOutcome::Suggested(suggestion(relation, 0.9)));
            assert_eq!(decided.decision, decision, "{relation:?}");
            assert_eq!(decided.cause, "accepted");
        }
    }

    #[test]
    fn doubt_collapses_to_unclear_and_nothing_else() {
        let mut low = suggestion(AdjudicationRelation::NewTask, MIN_CONFIDENCE - 0.01);
        assert_eq!(
            decide(&AdjudicationOutcome::Suggested(low.clone())),
            Decided::unclear("low_confidence")
        );

        // Confidence is not the only gate: a scope expansion is refused at any
        // confidence, and so is a suggestion that asks for confirmation.
        low.confidence = 1.0;
        low.scope_expansion = true;
        assert_eq!(
            decide(&AdjudicationOutcome::Suggested(low.clone())),
            Decided::unclear("scope_expansion")
        );

        low.scope_expansion = false;
        low.requires_confirmation = true;
        assert_eq!(
            decide(&AdjudicationOutcome::Suggested(low.clone())),
            Decided::unclear("requires_confirmation")
        );

        low.requires_confirmation = false;
        low.relation = AdjudicationRelation::Schedule;
        assert_eq!(
            decide(&AdjudicationOutcome::Suggested(low)),
            Decided::unclear("schedule_needs_time")
        );

        assert_eq!(
            decide(&AdjudicationOutcome::Suggested(suggestion(
                AdjudicationRelation::Unclear,
                0.99
            ))),
            Decided::unclear("unclear")
        );
        assert_eq!(decide(&AdjudicationOutcome::Abstained), Decided::unclear("abstained"));
        assert_eq!(
            decide(&AdjudicationOutcome::Failed { kind: "timeout".to_string() }),
            Decided::unclear("timeout")
        );
    }

    #[test]
    fn the_threshold_itself_is_high_enough_to_confirm() {
        let decided = decide(&AdjudicationOutcome::Suggested(suggestion(
            AdjudicationRelation::Stop,
            MIN_CONFIDENCE,
        )));
        assert_eq!(decided.decision, Decision::Cancel);
    }

    #[test]
    fn the_question_is_one_deterministic_sentence() {
        assert_eq!(clarification_question(), clarification_question());
        assert_eq!(clarification_question().matches('？').count(), 1);
    }

    #[test]
    fn a_response_yields_its_text_blocks_and_ignores_tool_blocks() {
        let payload = json!({
            "response": {
                "content_blocks": [
                    { "type": "text", "text": "the tests pass" },
                    { "type": "tool_use", "tool_call_id": "c1", "tool_name": "shell.run", "arguments": {} },
                    { "type": "text", "text": "and the build is clean" }
                ]
            }
        });
        assert_eq!(
            assistant_text(&payload).as_deref(),
            Some("the tests pass and the build is clean")
        );
        assert!(assistant_text(&json!({ "response": { "content_blocks": [] } })).is_none());
        assert!(assistant_text(&json!({})).is_none());
    }
}
