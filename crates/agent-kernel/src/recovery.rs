//! Recovery — repair dangling open entities and unsettled tool calls after a
//! crash.
//!
//! Recovery appends explicit terminal facts; it never truncates or rewrites
//! the log. Rules (revised for M5):
//!   * an open run/turn/step is closed with its `interrupted` event;
//!   * a dispatched tool call (`tool/started`) with no terminal outcome is
//!     marked `tool/outcome_unknown`, whatever its side-effect class: the log
//!     cannot say whether it ran, and a code that claims otherwise would be
//!     inventing a fact. The class shapes the *advice* the model gets, not the
//!     code — a read-only call is one you may simply issue again, and the
//!     payload says so in words;
//!   * a session waiting on a decision (`turn/yielded`, no answer yet) keeps the
//!     turn it stopped: its step/turn/run are left open for `resume` to carry
//!     forward, and closing them here would file "we lost this" about a turn
//!     that is merely paused. The one thing still repaired is a call that was
//!     dispatched and never came back — a crash in the middle of resuming leaves
//!     exactly that, and it must be marked before anyone retries. See
//!     [`Recovery::recover`].

use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::tool_provider::{ToolProvider, ToolSideEffect};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, RunId, SchemaVersion, SessionId, StepId, TurnId};
use chrono::Utc;

use crate::lifecycle;

/// Repairs dangling open entities for a session.
pub struct Recovery {
    store: Arc<dyn EventStore>,
    tools: Arc<dyn ToolProvider>,
}

/// What one recovery pass settled.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    pub closed_step: bool,
    pub closed_turn: bool,
    pub closed_run: bool,
    /// Calls that were dispatched but never came back, marked
    /// `tool/outcome_unknown`.
    pub tools_outcome_unknown: usize,
    /// Set when the session is waiting on a decision. Its step/turn/run were
    /// left open on purpose: `resume` owns everything after a yield.
    pub deferred_to_resume: Option<TurnId>,
}

/// Errors from Recovery operations.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
}

/// A `tool/started` that was never followed by a terminal tool event.
#[derive(Debug)]
struct DanglingTool {
    tool_name: String,
    /// The provider's id for this call, when the log recorded one. It is the
    /// only identity that tells two calls to the same tool apart.
    provider_tool_call_id: Option<String>,
    run_id: Option<RunId>,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
}

impl Recovery {
    pub fn new(store: Arc<dyn EventStore>, tools: Arc<dyn ToolProvider>) -> Self {
        Self { store, tools }
    }

    /// Append the terminal facts that repair any dangling open state.
    ///
    /// Idempotent: once the closers are appended, a later pass finds nothing
    /// open and appends nothing.
    ///
    /// One session state is deliberately left half-open. A session sitting at
    /// `turn/yielded` is waiting for the user to answer an approval, not
    /// recovering from a crash: its half-finished step is the place the turn
    /// stopped, and closing it here as `interrupted` would file "we lost this"
    /// about a turn that is about to be carried forward. `resume` reads that
    /// state and settles it; repair would only make its evidence disappear.
    ///
    /// Dangling *calls* are still settled in that state, and that is not a
    /// detail. `resume` dispatches an approved call in two phases — the durable
    /// `tool/started` first, the outcome after — precisely so a crash in between
    /// leaves a call that Recovery will find. Marking it `tool/outcome_unknown`
    /// is what stops the next resume from reading the older `approval_pending`
    /// as "still waiting" and running the call a second time.
    pub async fn recover(&self, session_id: &SessionId) -> Result<RecoveryReport, RecoveryError> {
        // Reading the log and appending at its end is one step, not two: between
        // them the session's next sequence can move — every `agent` command is
        // its own process over the same session — and an append computed from a
        // read that has since moved is refused by the store. The lease is what
        // makes the pair atomic. It is held across the fold as well, so the
        // drafts are built from the same log the sequence is taken from. (YA-36)
        let _lease = self.store.lease(session_id).await?;
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        let lc = lifecycle::fold(&events);

        let dangling = dangling_tools(&events);
        let mut drafts = Vec::new();
        let mut report = RecoveryReport::default();

        for tool in &dangling {
            report.tools_outcome_unknown += 1;
            let side_effect = self.tools.side_effect(&tool.tool_name);
            drafts.push(draft(
                session_id,
                EventType::TOOL_OUTCOME_UNKNOWN,
                unsettled_payload(tool, side_effect),
                Actor::Scheduler,
                tool.run_id.as_ref(),
                tool.turn_id.as_ref(),
                tool.step_id.as_ref(),
            ));
        }

        // A paused turn is carried forward, not closed. Everything after this
        // point is about entities, and these three belong to the wait.
        if let Some(turn) = lc.yielded_turn.clone() {
            report.deferred_to_resume = Some(turn);
        } else {
            if let Some(step) = lc.open_step {
                report.closed_step = true;
                drafts.push(draft(
                    session_id,
                    EventType::STEP_INTERRUPTED,
                    serde_json::json!({ "reason": "recovery" }),
                    Actor::Scheduler,
                    lc.open_run.as_ref(),
                    lc.open_turn.as_ref(),
                    Some(&step),
                ));
            }
            if let Some(turn) = lc.open_turn {
                report.closed_turn = true;
                drafts.push(draft(
                    session_id,
                    EventType::TURN_INTERRUPTED,
                    serde_json::json!({ "reason": "recovery" }),
                    Actor::Scheduler,
                    lc.open_run.as_ref(),
                    Some(&turn),
                    None,
                ));
            }
            if let Some(run) = lc.open_run {
                report.closed_run = true;
                drafts.push(draft(
                    session_id,
                    EventType::RUN_INTERRUPTED,
                    serde_json::json!({ "reason": "recovery" }),
                    Actor::Scheduler,
                    Some(&run),
                    None,
                    None,
                ));
            }
        }

        if drafts.is_empty() {
            return Ok(report);
        }

        let next = self.next_seq(session_id).await?;
        self.store.append(session_id, next, drafts).await?;
        Ok(report)
    }

    async fn next_seq(&self, session_id: &SessionId) -> Result<EventSeq, RecoveryError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO))
    }
}

/// Collect `tool/started` events with no matching terminal tool event, in order.
///
/// Matching prefers `provider_tool_call_id` — the provider's own id for the
/// call, which the loop has put on every tool event since D-1 — and falls back
/// to the tool name (LIFO) only when a terminal event carries no id: logs
/// written before D-1 never recorded one. A name is not an identity, so the
/// fallback is a best effort, not a guarantee.
fn dangling_tools(events: &[StoredEvent]) -> Vec<DanglingTool> {
    let mut open: Vec<DanglingTool> = Vec::new();
    for stored in events {
        let e = &stored.event;
        match e.event_type.as_str() {
            EventType::TOOL_STARTED => open.push(DanglingTool {
                tool_name: tool_name(e),
                provider_tool_call_id: provider_tool_call_id(e),
                run_id: e.run_id.clone(),
                turn_id: e.turn_id.clone(),
                step_id: e.step_id.clone(),
            }),
            EventType::TOOL_COMPLETED
            | EventType::TOOL_FAILED
            | EventType::TOOL_CANCELLED
            | EventType::TOOL_INTERRUPTED
            | EventType::TOOL_OUTCOME_UNKNOWN => {
                if let Some(pos) = settled_call(&open, e) {
                    open.remove(pos);
                }
            }
            _ => {}
        }
    }
    open
}

/// Which still-open call a terminal tool event settles, if any.
///
/// When the terminal event names a call id, that is the whole answer: an id no
/// open call holds settles nothing, and falling back to the name there would
/// close some unrelated call of the same tool. Only an id-less terminal event —
/// one from a log the pre-D-1 loop wrote — falls back to the most recent open
/// call of that name.
fn settled_call(open: &[DanglingTool], terminal: &EventDraft) -> Option<usize> {
    match provider_tool_call_id(terminal) {
        Some(id) => open.iter().rposition(|t| t.provider_tool_call_id.as_deref() == Some(&id)),
        None => {
            let name = tool_name(terminal);
            open.iter().rposition(|t| t.tool_name == name)
        }
    }
}

/// The payload of a synthesized closer.
///
/// The call id is carried through when the dangling `tool/started` had one, so
/// that `Recovery`'s closers match their call by the same rule every other
/// terminal tool event does — and a projection rebuilding the transcript does
/// not have to fall back to matching names for them.
///
/// `content` is written out in full rather than left for the projection to
/// compose from `reason`: the sentence the model reads is the sentence the
/// event carries, so a rebuild copies it instead of re-deriving it from a flag
/// whose meaning lives in another crate's match arm.
fn unsettled_payload(
    tool: &DanglingTool,
    side_effect: Option<ToolSideEffect>,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert("tool_name".to_string(), serde_json::Value::String(tool.tool_name.clone()));
    payload.insert(
        "reason".to_string(),
        serde_json::Value::String("interrupted_before_outcome".to_string()),
    );
    if let Some(id) = &tool.provider_tool_call_id {
        payload.insert("provider_tool_call_id".to_string(), serde_json::Value::String(id.clone()));
    }
    payload.insert(
        "side_effect".to_string(),
        serde_json::to_value(side_effect).unwrap_or(serde_json::Value::Null),
    );
    payload
        .insert("content".to_string(), serde_json::Value::String(unsettled_guidance(side_effect)));
    serde_json::Value::Object(payload)
}

/// What the model is told about a call whose outcome the log never recorded.
///
/// The first sentence is the same in both branches because it is the whole
/// truth: nobody knows whether it ran. What differs is what that means for the
/// next move — and for a tool that cannot change anything, it means very little.
fn unsettled_guidance(side_effect: Option<ToolSideEffect>) -> String {
    let unknown = "This tool call was interrupted before its outcome was recorded, so it may or \
                   may not have run.";
    let harmless = match side_effect {
        Some(ToolSideEffect::Pure) => Some("pure"),
        Some(ToolSideEffect::Read) => Some("read-only"),
        _ => None,
    };
    match harmless {
        Some(label) => format!(
            "{unknown} It is a {label} tool, so running it again changes nothing — re-issue it if \
             you still need the result."
        ),
        None => format!(
            "{unknown} Do not re-issue it blindly: check the external state first (read the file, \
             inspect the workspace, run a status command), and if it may have had a side effect, \
             ask the user before retrying."
        ),
    }
}

fn tool_name(event: &EventDraft) -> String {
    event.payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("unknown").to_string()
}

fn provider_tool_call_id(event: &EventDraft) -> Option<String> {
    event.payload.get("provider_tool_call_id").and_then(|v| v.as_str()).map(str::to_string)
}

fn draft(
    session_id: &SessionId,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
    run_id: Option<&RunId>,
    turn_id: Option<&TurnId>,
    step_id: Option<&StepId>,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session_id.clone(),
        run_id: run_id.cloned(),
        turn_id: turn_id.cloned(),
        step_id: step_id.cloned(),
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_testkit::{FakeEventStore, FakeToolProvider};

    fn recovery() -> (Recovery, Arc<FakeEventStore>, SessionId) {
        let store = Arc::new(FakeEventStore::new());
        let tools = Arc::new(
            FakeToolProvider::new().with_tool("echo", "ok", ToolSideEffect::Pure).with_tool(
                "deploy",
                "ok",
                ToolSideEffect::Destructive,
            ),
        );
        let recovery = Recovery::new(store.clone(), tools);
        let sid = SessionId::from_string("recovery-session");
        (recovery, store, sid)
    }

    async fn seed(store: &FakeEventStore, sid: &SessionId, drafts: Vec<EventDraft>) {
        let next = store
            .read_stream(sid, EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .last()
            .map(|e| e.seq.next())
            .unwrap_or(EventSeq::ZERO);
        store.append(sid, next, drafts).await.unwrap();
    }

    async fn types(store: &FakeEventStore, sid: &SessionId) -> Vec<String> {
        store
            .read_stream(sid, EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.event.event_type.to_string())
            .collect()
    }

    /// The read and the append are one step: `recover` holds the lease across
    /// both. The store refuses an append that arrives without one, so this
    /// passes only while the pairing is covered. (YA-36)
    #[tokio::test]
    async fn recover_holds_the_lease_across_its_read_and_append() {
        let store = Arc::new(FakeEventStore::new().requiring_lease());
        let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "ok", ToolSideEffect::Pure));
        let recovery = Recovery::new(store.clone(), tools);
        let sid = SessionId::from_string("recovery-session");

        // The seed is an append too, so it takes the lease as well — the call
        // under test is `recover`, not the setup around it.
        {
            let _lease = store.lease(&sid).await.unwrap();
            seed(
                &store,
                &sid,
                vec![draft(
                    &sid,
                    EventType::RUN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&RunId::new()),
                    None,
                    None,
                )],
            )
            .await;
        }

        let report = recovery.recover(&sid).await.unwrap();
        assert!(report.closed_run, "the open run is closed by the leased append");
    }

    #[tokio::test]
    async fn recover_closes_open_run_turn_step() {
        let (recovery, store, sid) = recovery();
        let run = RunId::new();
        let turn = TurnId::new();
        let step = StepId::new();
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::RUN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    None,
                    None,
                ),
                draft(
                    &sid,
                    EventType::TURN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    Some(&turn),
                    None,
                ),
                draft(
                    &sid,
                    EventType::STEP_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    Some(&turn),
                    Some(&step),
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert!(report.closed_run && report.closed_turn && report.closed_step);
        assert_eq!(report.tools_outcome_unknown, 0);

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let lc = lifecycle::fold(&events);
        assert!(lc.open_run.is_none() && lc.open_turn.is_none() && lc.open_step.is_none());

        // Idempotent: a second pass finds nothing to do.
        let second = recovery.recover(&sid).await.unwrap();
        assert!(!second.closed_run && !second.closed_turn && !second.closed_step);
    }

    #[tokio::test]
    async fn recover_is_noop_when_nothing_open() {
        let (recovery, store, sid) = recovery();
        let report = recovery.recover(&sid).await.unwrap();
        assert!(!report.closed_run && !report.closed_turn && !report.closed_step);
        assert!(store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_dangling_read_only_tool_is_still_outcome_unknown() {
        // The call was dispatched and never came back. Not knowing whether it ran
        // is the fact, and a read-only tool does not get to be called something
        // else because the consequences happen to be mild — the code says what
        // the log knows, the sentence says what to do about it.
        let (recovery, store, sid) = recovery();
        let step = StepId::new();
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::STEP_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    None,
                    None,
                    Some(&step),
                ),
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "echo" }),
                    Actor::Tool,
                    None,
                    None,
                    Some(&step),
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.tools_outcome_unknown, 1);
        let types = types(&store, &sid).await;
        assert!(types.contains(&"tool/outcome_unknown".to_string()));
        assert!(!types.contains(&"tool/interrupted".to_string()));

        let content = closer_content(&store, &sid).await;
        assert!(content.contains("may or may not have run"), "{content}");
        assert!(content.contains("pure tool"), "the advice is for a harmless call: {content}");
    }

    #[tokio::test]
    async fn a_dangling_side_effecting_or_unknown_tool_gets_the_warning() {
        let (recovery, store, sid) = recovery();
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
                // "mystery" is not registered, so its side-effect class is unknown -> fail closed.
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "mystery" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.tools_outcome_unknown, 2);

        let unknown =
            types(&store, &sid).await.into_iter().filter(|t| t == "tool/outcome_unknown").count();
        assert_eq!(unknown, 2);

        let content = closer_content(&store, &sid).await;
        assert!(content.contains("Do not re-issue it blindly"), "{content}");
    }

    #[tokio::test]
    async fn a_session_waiting_on_a_decision_is_left_alone() {
        // Not a crash: the turn stopped at a step boundary and is waiting for the
        // user. Closing its step here would record "we lost this" about a turn
        // `resume` is going to carry forward, and would take away the evidence
        // resume reads.
        let (recovery, store, sid) = recovery();
        let run = RunId::new();
        let turn = TurnId::new();
        let step = StepId::new();
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::RUN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    None,
                    None,
                ),
                draft(
                    &sid,
                    EventType::TURN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    Some(&turn),
                    None,
                ),
                draft(
                    &sid,
                    EventType::STEP_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    Some(&turn),
                    Some(&step),
                ),
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy" }),
                    Actor::Tool,
                    Some(&run),
                    Some(&turn),
                    Some(&step),
                ),
                draft(
                    &sid,
                    EventType::TOOL_FAILED,
                    serde_json::json!({
                        "tool_name": "deploy",
                        "outcome": "approval_pending",
                        "approval_id": "approval/1",
                    }),
                    Actor::Tool,
                    Some(&run),
                    Some(&turn),
                    Some(&step),
                ),
                draft(
                    &sid,
                    EventType::RUN_YIELDED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    Some(&turn),
                    None,
                ),
                draft(
                    &sid,
                    EventType::TURN_YIELDED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    Some(&run),
                    Some(&turn),
                    None,
                ),
            ],
        )
        .await;
        let before = types(&store, &sid).await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.deferred_to_resume, Some(turn.clone()));
        assert!(!report.closed_step && !report.closed_turn && !report.closed_run);
        assert_eq!(types(&store, &sid).await, before, "recovery must append nothing here");
    }

    #[tokio::test]
    async fn a_call_dispatched_by_a_crashed_resume_is_settled_even_while_yielded() {
        // The shape a crash in the middle of `resume` leaves: the wait is over,
        // the call has been dispatched again, and no outcome was written. If
        // Recovery skipped this while deferring to resume, the next resume would
        // read the older `approval_pending` as "still waiting" and run the call a
        // second time — the one outcome this whole feature exists to prevent.
        let (recovery, store, sid) = recovery();
        let turn = TurnId::new();
        let step = StepId::new();
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::TURN_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    None,
                    Some(&turn),
                    None,
                ),
                draft(
                    &sid,
                    EventType::STEP_STARTED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    None,
                    Some(&turn),
                    Some(&step),
                ),
                draft(
                    &sid,
                    EventType::TOOL_FAILED,
                    serde_json::json!({
                        "tool_name": "deploy",
                        "provider_tool_call_id": "call_1",
                        "outcome": "approval_pending",
                        "approval_id": "approval/1",
                    }),
                    Actor::Tool,
                    None,
                    Some(&turn),
                    Some(&step),
                ),
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call_1" }),
                    Actor::Tool,
                    None,
                    Some(&turn),
                    Some(&step),
                ),
                draft(
                    &sid,
                    EventType::TURN_YIELDED,
                    serde_json::json!({}),
                    Actor::Scheduler,
                    None,
                    Some(&turn),
                    None,
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.deferred_to_resume, Some(turn.clone()));
        assert_eq!(report.tools_outcome_unknown, 1, "the re-dispatched call is settled");
        assert!(!report.closed_step && !report.closed_turn, "the wait itself is not closed");
        assert_eq!(
            closers(&store, &sid).await,
            vec![("deploy".to_string(), Some("call_1".to_string()))]
        );
    }

    /// `(tool_name, provider_tool_call_id)` of every closer Recovery wrote.
    async fn closers(store: &FakeEventStore, sid: &SessionId) -> Vec<(String, Option<String>)> {
        store
            .read_stream(sid, EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event.event_type.as_str() == EventType::TOOL_OUTCOME_UNKNOWN)
            .map(|e| {
                let name = e
                    .event
                    .payload
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let id = e
                    .event
                    .payload
                    .get("provider_tool_call_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                (name, id)
            })
            .collect()
    }

    /// The text every closer carries, joined — the sentence the model will read.
    async fn closer_content(store: &FakeEventStore, sid: &SessionId) -> String {
        store
            .read_stream(sid, EventSeq::ZERO, usize::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.event.event_type.as_str() == EventType::TOOL_OUTCOME_UNKNOWN)
            .filter_map(|e| {
                e.event.payload.get("content").and_then(|v| v.as_str()).map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_terminal_event_settles_the_call_it_names_not_the_newest_one() {
        let (recovery, store, sid) = recovery();
        // Two calls to the same tool, one step. Only the first was answered.
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call_1" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call_2" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
                draft(
                    &sid,
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call_1" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.tools_outcome_unknown, 1, "only call_2 is unsettled");
        assert_eq!(
            closers(&store, &sid).await,
            vec![("deploy".to_string(), Some("call_2".to_string()))],
            "the closer must name the call that is actually open"
        );
    }

    #[tokio::test]
    async fn a_terminal_event_naming_no_open_call_settles_nothing() {
        let (recovery, store, sid) = recovery();
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call_1" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
                // An id nothing is open under: closing call_1 here by name would
                // settle a call this event is not about.
                draft(
                    &sid,
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "deploy", "provider_tool_call_id": "call_9" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.tools_outcome_unknown, 1);
        assert_eq!(
            closers(&store, &sid).await,
            vec![("deploy".to_string(), Some("call_1".to_string()))]
        );
    }

    #[tokio::test]
    async fn a_pre_d1_log_still_pairs_by_name() {
        let (recovery, store, sid) = recovery();
        // No ids anywhere: the pre-D-1 shape, where the name is all there is.
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
                draft(
                    &sid,
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "deploy" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.tools_outcome_unknown, 0);
        assert!(closers(&store, &sid).await.is_empty());
    }

    #[tokio::test]
    async fn a_started_call_with_an_id_is_closed_by_a_closer_that_carries_it() {
        let (recovery, store, sid) = recovery();
        seed(
            &store,
            &sid,
            vec![draft(
                &sid,
                EventType::TOOL_STARTED,
                serde_json::json!({ "tool_name": "echo", "provider_tool_call_id": "call_7" }),
                Actor::Tool,
                None,
                None,
                None,
            )],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.tools_outcome_unknown, 1);
        // The projection matches a closer to its call by this id, so a closer
        // that dropped it would fall back to matching names — the very
        // ambiguity the id exists to remove.
        assert_eq!(
            closers(&store, &sid).await,
            vec![("echo".to_string(), Some("call_7".to_string()))]
        );
    }

    #[tokio::test]
    async fn settled_tool_is_not_flagged_again() {
        let (recovery, store, sid) = recovery();
        seed(
            &store,
            &sid,
            vec![
                draft(
                    &sid,
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "deploy" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
                draft(
                    &sid,
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "deploy" }),
                    Actor::Tool,
                    None,
                    None,
                    None,
                ),
            ],
        )
        .await;

        let report = recovery.recover(&sid).await.unwrap();
        assert_eq!(report.tools_outcome_unknown, 0);
    }
}
