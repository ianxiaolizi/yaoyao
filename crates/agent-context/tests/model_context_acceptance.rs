//! M4 Task D-2 acceptance: the model's history is derived from the durable log.
//!
//! These drive real turns through the Scheduler over a real `JsonlEventStore`,
//! because the property under test is about a *recording*: what the model was
//! sent must be exactly what the log rebuilds. A fake store would let the two
//! drift in the same way the old in-memory history did.
//!
//! The loop keeps no transcript of its own, so the model's own view is read
//! back from the recording provider — that is the "sent" side of every
//! comparison here, and the derived context is the "rebuilt" side.

use std::collections::HashSet;
use std::sync::Arc;

use agent_context::ProjectionContextEngine;
use agent_context::trim::ContextPolicy;
use agent_kernel::recovery::Recovery;
use agent_kernel::scheduler::{RunReport, Scheduler};
use agent_loop::DefaultAgentLoop;
use agent_ports::agent_loop::StopReason;
use agent_ports::context_engine::ContextEngine;
use agent_ports::event_store::EventStore;
use agent_ports::tool_gateway::ToolGateway;
use agent_ports::tool_provider::{ToolProvider, ToolSideEffect};
use agent_projection::{ApplyError, ModelContextProjection, Projection, ProjectionDefinition};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId, StepId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::model::{ContentBlock, MessageRole, ModelMessage, ModelResponse};
use agent_store_jsonl::JsonlEventStore;
use agent_testkit::{FakeModelProvider, FakeToolGateway, FakeToolProvider, one_model_router};
use chrono::Utc;
use tempfile::TempDir;

/// A session wired the way `apps/agent` wires it: JSONL store, projection-backed
/// context engine, loop, Scheduler.
struct Harness {
    store: Arc<JsonlEventStore>,
    model: Arc<FakeModelProvider>,
    scheduler: Scheduler,
    session_id: SessionId,
    /// The window the live engine trims with, if any. `derived` uses the same
    /// policy, so "what a fresh process would derive" means the same thing
    /// here as it does for the run.
    policy: Option<ContextPolicy>,
    /// The store, and any checkpoint, live under here. Kept alive for the test.
    data_dir: TempDir,
}

fn harness(model: Arc<FakeModelProvider>, gateway: Arc<dyn ToolGateway>) -> Harness {
    harness_with_window(model, gateway, None)
}

fn harness_with_window(
    model: Arc<FakeModelProvider>,
    gateway: Arc<dyn ToolGateway>,
    policy: Option<ContextPolicy>,
) -> Harness {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let store = Arc::new(JsonlEventStore::new(data_dir.path()));
    let engine = ProjectionContextEngine::new(store.clone(), data_dir.path());
    let context = Arc::new(match policy {
        Some(policy) => engine.with_context_policy(policy),
        None => engine,
    });
    let loop_ =
        Arc::new(DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router()));
    let scheduler = Scheduler::new(store.clone(), loop_, 8);
    Harness { store, model, scheduler, session_id: SessionId::from_string("d2"), policy, data_dir }
}

/// A gateway over tools that always succeed.
fn plain_gateway(tools: FakeToolProvider) -> Arc<dyn ToolGateway> {
    let tools: Arc<dyn ToolProvider> = Arc::new(tools);
    Arc::new(FakeToolGateway::new(tools))
}

impl Harness {
    /// Submit `text` as the next turn's input and drive it.
    async fn ask(&self, text: &str) -> RunReport {
        self.scheduler
            .inbox()
            .append(
                &self.session_id,
                InboxKind::NextTurn,
                serde_json::json!({ "text": text }),
                0,
                Actor::User,
            )
            .await
            .expect("append input");
        self.scheduler
            .run_pending(&self.session_id, Utc::now())
            .await
            .expect("run pending")
            .expect("a turn ran")
    }

    /// The durable log, in order.
    async fn log(&self) -> Vec<StoredEvent> {
        self.store
            .read_stream(&self.session_id, EventSeq::ZERO, usize::MAX)
            .await
            .expect("read log")
    }

    /// The durable event types, in order.
    async fn types(&self) -> Vec<String> {
        self.log().await.into_iter().map(|e| e.event.event_type.to_string()).collect()
    }

    /// The history as a fresh process with the same window would derive it.
    async fn derived(&self) -> Vec<ModelMessage> {
        let engine = ProjectionContextEngine::new(self.store.clone(), self.data_dir.path());
        let engine = match self.policy {
            Some(policy) => engine.with_context_policy(policy),
            None => engine,
        };
        engine.derive(&self.session_id).await.expect("derive").messages
    }

    /// Every settled model response in the log, in order.
    async fn responses(&self) -> Vec<StoredEvent> {
        self.log()
            .await
            .into_iter()
            .filter(|e| e.event.event_type.as_str() == EventType::MODEL_RESPONSE_COMPLETED)
            .collect()
    }

    /// The messages the model was actually sent, in order, each message kept
    /// once — the first time it appears. Sending the same message twice (same
    /// uid) is what a resumed turn does, not a new message.
    ///
    /// The per-turn state and task layers are removed: they are placed by the
    /// loop for each request and are not folded into history (CONTRACT #42),
    /// so the rebuilt history must not contain them. Everything else the log
    /// rebuilds must equal what was sent.
    fn sent(&self) -> Vec<ModelMessage> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut messages = Vec::new();
        for request in self.model.requests() {
            for message in settled(request.messages) {
                if seen.insert(message.message_uid.as_str().to_string()) {
                    messages.push(message);
                }
            }
        }
        messages
    }
}

/// A request's messages minus the per-turn layers and tail blocks, which are
/// not history.
///
/// The state, task and identity layers open a request for its turn; the
/// work-state block closes it. All of them are placed by the loop for each
/// request and are not folded into history (CONTRACT #42), so the rebuilt
/// transcript must not contain them.
fn settled(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    messages
        .into_iter()
        .filter(|message| {
            let uid = message.message_uid.as_str();
            !uid.starts_with("msg/state/")
                && !uid.starts_with("msg/task/")
                && !uid.starts_with("msg/identity/")
                && !uid.starts_with("msg/work-state/")
        })
        .collect()
}

/// The first request the loop sent in `turn_id`.
fn first_request(
    h: &Harness,
    turn_id: &agent_protocol::ids::TurnId,
) -> agent_protocol::model::ModelRequest {
    h.model.requests().into_iter().find(|r| &r.turn_id == turn_id).expect("a request for that turn")
}

/// The assistant message the log recorded for a `model/response.completed`.
fn response_message(event: &StoredEvent) -> ModelMessage {
    let attempt_id = event.event.payload["attempt_id"].as_str().expect("attempt id");
    let response: ModelResponse =
        serde_json::from_value(event.event.payload["response"].clone()).expect("response");
    ModelMessage::assistant_for_attempt(
        &agent_protocol::ids::AttemptId::from_string(attempt_id),
        response.content_blocks.clone(),
    )
}

/// Assert the derived history is exactly what the model was sent, plus the last
/// turn's own output.
///
/// The tail is genuinely un-sent rather than a discrepancy: a turn ends when the
/// model answers without calling a tool, so that answer is recorded but never
/// appears in a request — the next turn is what sees it. Demanding `trailing`
/// elements keeps the assertion honest about that, instead of letting a missing
/// message hide behind a `starts_with`.
async fn assert_derived_extends_sent(h: &Harness, trailing: usize) {
    let sent = h.sent();
    let derived = h.derived().await;
    assert_eq!(derived.len(), sent.len() + trailing, "rebuilt length: {derived:?}");
    assert_eq!(
        &derived[..sent.len()],
        sent.as_slice(),
        "rebuilt history differs from what the model was sent"
    );
    let answers = h.responses().await;
    let expected_tail: Vec<ModelMessage> =
        answers.iter().map(response_message).rev().take(trailing).rev().collect();
    assert_eq!(
        &derived[sent.len()..],
        expected_tail.as_slice(),
        "the record the model has not seen yet must still be the log's own response"
    );
}

#[tokio::test]
async fn the_second_turn_sees_the_first_turns_durable_history() {
    // Two plain answers, one per turn.
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::answer("first answer"),
        FakeModelProvider::answer("second answer"),
    ]));
    let h = harness(model, plain_gateway(FakeToolProvider::new()));

    let first = h.ask("first").await;
    let second = h.ask("second").await;

    // The first turn saw nothing settled before it.
    let opening = first_request(&h, &first.turn_id);
    assert_eq!(
        settled(opening.messages),
        vec![ModelMessage::user_for_turn(&first.turn_id, "first")]
    );
    // The revision is a log position, not a count of messages: the session's
    // start at sequence 0 and the queued input at sequence 1 are everything
    // that had been written when the first request was built.
    assert_eq!(opening.context_revision, 1, "nothing but the start and the input was durable");

    // The second turn saw the first turn's transcript, verbatim from the log.
    let later_request = first_request(&h, &second.turn_id);
    let later = settled(later_request.messages.clone());
    assert_eq!(later.len(), 3, "history, then this turn's input: {later:?}");
    assert_eq!(later[0], ModelMessage::user_for_turn(&first.turn_id, "first"));

    let completed = h
        .log()
        .await
        .into_iter()
        .find(|e| e.event.event_type.as_str() == EventType::MODEL_RESPONSE_COMPLETED)
        .expect("the first turn settled a response");
    let attempt_id =
        completed.event.payload["attempt_id"].as_str().expect("attempt id").to_string();
    let response: ModelResponse =
        serde_json::from_value(completed.event.payload["response"].clone()).expect("response");
    assert_eq!(
        later[1],
        ModelMessage::assistant_for_attempt(
            &agent_protocol::ids::AttemptId::from_string(attempt_id),
            response.content_blocks.clone(),
        ),
        "the assistant message must be the one the log recorded, not a re-rendering of it"
    );
    assert_eq!(later[2], ModelMessage::user_for_turn(&second.turn_id, "second"));

    // A zero here would mean the request names no log position, which is the
    // old behavior wearing a revision-shaped field.
    assert!(
        later_request.context_revision > opening.context_revision,
        "revision must advance with the log"
    );
}

#[tokio::test]
async fn the_log_rebuilds_exactly_the_messages_the_model_was_sent() {
    // Two turns, each with a tool round trip.
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({ "s": "one" })),
        FakeModelProvider::answer("turn one done"),
        FakeModelProvider::tool_call("echo", serde_json::json!({ "s": "two" })),
        FakeModelProvider::answer("turn two done"),
    ]));
    let tools = FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure);
    let h = harness(model, plain_gateway(tools));

    h.ask("first").await;
    h.ask("second").await;

    // Element-wise: same uids, same roles, same content. A uid that drifts —
    // a random one, or one built from something the loop cannot know at send
    // time — makes this fail even when the text looks right.
    assert_derived_extends_sent(&h, 1).await;
    let sent = h.sent();
    assert_eq!(sent.len(), 7, "user/assistant/tool per turn, plus turn one's answer: {sent:?}");

    let derived = h.derived().await;
    let roles: Vec<MessageRole> = derived.iter().map(|m| m.role).collect();
    let expected_roles = vec![
        MessageRole::User,
        MessageRole::Assistant,
        MessageRole::Tool,
        MessageRole::Assistant,
        MessageRole::User,
        MessageRole::Assistant,
        MessageRole::Tool,
        MessageRole::Assistant,
    ];
    assert_eq!(roles, expected_roles, "one round trip per turn, in order");
}

/// A window changes what is sent, not what is true: the request carries a
/// suffix of the derived history plus this turn's input, and the record says
/// exactly how much was left behind.
#[tokio::test]
async fn a_windowed_history_sends_the_kept_suffix_plus_this_turns_input() {
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::answer("done"),
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::answer("done"),
    ]));
    let tools = FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure);
    // Costs: the opening user message 5, the call/result pair 12, the answer 5.
    // The budget fits the newest answer and the pair together (17) but not the
    // opening message as well (22).
    let h = harness_with_window(
        model,
        plain_gateway(tools),
        Some(ContextPolicy {
            context_window_tokens: 18,
            input_working_set_tokens: 18,
            completion_cap_tokens: 0,
            compaction_trigger_tokens: 0,
            retain_recent_tokens: 9,
        }),
    );
    h.ask("first").await;

    let kept = h.derived().await;
    assert_eq!(kept.len(), 3, "the opening message should have been dropped: {kept:?}");
    assert!(
        !kept.iter().any(|m| m.content.iter().any(|b| b.as_text() == Some("first"))),
        "the dropped message must not be sent under another uid: {kept:?}"
    );
    // Deriving the same log twice keeps the same messages, uid for uid.
    assert_eq!(h.derived().await, kept, "the window must be deterministic");

    // Without a window the same log is sent whole: the policy is what changed,
    // not the log.
    let whole = ProjectionContextEngine::new(h.store.clone(), h.data_dir.path())
        .derive(&h.session_id)
        .await
        .expect("derive")
        .messages;
    assert_eq!(whole.len(), 4, "an unset window must not trim: {whole:?}");
    assert_eq!(&whole[1..], kept.as_slice(), "the window keeps a suffix of the same history");

    h.scheduler
        .inbox()
        .append(
            &h.session_id,
            InboxKind::NextTurn,
            serde_json::json!({ "text": "second" }),
            0,
            Actor::User,
        )
        .await
        .expect("append input");
    let report = h
        .scheduler
        .run_pending(&h.session_id, Utc::now())
        .await
        .expect("run pending")
        .expect("a turn ran");

    let request = first_request(&h, &report.turn_id);
    // Settled history leads, then the per-turn layers, then the trim notice:
    // volatile messages sit after the cached prefix and before this turn's
    // input.
    assert_eq!(
        &request.messages[..kept.len()],
        kept.as_slice(),
        "the request must open with the kept suffix unchanged"
    );
    let state = &request.messages[kept.len()];
    assert_eq!(state.message_uid.as_str(), format!("msg/state/{}", report.turn_id));
    let task = &request.messages[kept.len() + 1];
    assert_eq!(task.message_uid.as_str(), format!("msg/task/{}", report.turn_id));
    let identity = &request.messages[kept.len() + 2];
    assert_eq!(identity.message_uid.as_str(), format!("msg/identity/{}", report.turn_id));
    let notice = &request.messages[kept.len() + 3];
    assert_eq!(notice.message_uid.as_str(), format!("msg/notice/{}", report.turn_id));
    assert_eq!(notice.role, MessageRole::System);
    assert!(notice.text_content().contains("1 older message(s) were omitted"), "{notice:?}");
    // The input follows the notice, and the loop's work-state block closes
    // the request: where the work stands is the last thing a step reads.
    assert_eq!(
        &request.messages[kept.len() + 4..kept.len() + 5],
        [ModelMessage::user_for_turn(&report.turn_id, "second")].as_slice(),
        "this turn's input stays verbatim after the layers"
    );
    let tail = request.messages.last().expect("a tail");
    assert_eq!(tail.message_uid.as_str(), format!("msg/work-state/{}", report.turn_id));

    // The record of the derivation carries the same numbers the engine used,
    // so a reader does not have to re-derive the log to know what was dropped.
    let prepared = h
        .log()
        .await
        .into_iter()
        .find(|e| {
            e.event.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED
                && e.event.turn_id.as_ref() == Some(&report.turn_id)
        })
        .expect("model/request.prepared");
    assert_eq!(
        prepared.event.payload["message_count"], 9,
        "the sent count includes the three per-turn layers, the notice, the input and the \
         work-state tail"
    );
    assert_eq!(prepared.event.payload["context_message_count"], 3);
    assert_eq!(prepared.event.payload["dropped_message_count"], 1);
    assert_eq!(prepared.event.payload["estimated_context_tokens"], 17);
    assert_eq!(prepared.event.payload["input_working_set_tokens"], 18);
    assert_eq!(prepared.event.payload["context_notice"]["kind"], "trimmed+near-limit");
    // The percentage is measured over the whole request — tool definitions
    // included — since 2026-10-31, and this fixture's 18-token window could
    // never hold them. That it accompanies the trim is what this test pins;
    // the exact share is placed under test where the window is real.
    let percent = prepared.event.payload["context_notice"]["percent"].as_u64().unwrap_or(0);
    assert!(percent >= 85, "{percent}");
}

#[tokio::test]
async fn a_failed_attempt_never_reaches_the_history() {
    let model = Arc::new(FakeModelProvider::failing(
        agent_ports::model_provider::ModelProviderError::Http {
            status: 500,
            body: "upstream is down".to_string(),
        },
    ));
    let h = harness(model, plain_gateway(FakeToolProvider::new()));

    let turned = h.ask("first").await;
    assert_eq!(turned.stop_reason, StopReason::FatalError);

    let derived = h.derived().await;
    assert_eq!(
        derived,
        vec![ModelMessage::user_for_turn(&turned.turn_id, "first")],
        "a turn that produced no response leaves no assistant message"
    );

    let types: Vec<String> =
        h.log().await.into_iter().map(|e| e.event.event_type.to_string()).collect();
    assert!(types.iter().any(|t| t == EventType::MODEL_ATTEMPT_FAILED), "{types:?}");
    assert!(!types.iter().any(|t| t == EventType::MODEL_RESPONSE_COMPLETED), "{types:?}");
}

#[tokio::test]
async fn an_approval_yield_leaves_the_call_open_for_the_decision() {
    // Every call is held for approval, so the tool never runs.
    let tools: Arc<dyn ToolProvider> =
        Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    let gateway: Arc<dyn ToolGateway> =
        Arc::new(FakeToolGateway::requiring_approval(tools, "ap-1"));
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({ "s": "hi" })),
        FakeModelProvider::answer("carried on"),
    ]));
    let h = harness(model, gateway);

    let held = h.ask("run it").await;
    assert_eq!(held.stop_reason, StopReason::NeedsUserInput);

    // The wait is a durable fact, and it names what has to answer it.
    let failed = h
        .log()
        .await
        .into_iter()
        .find(|e| e.event.event_type.as_str() == EventType::TOOL_FAILED)
        .expect("the held call is recorded");
    assert_eq!(failed.event.payload["outcome"], "approval_pending");
    assert_eq!(failed.event.payload["approval_id"], "ap-1");
    assert_eq!(failed.event.payload["status"], "failed");
    assert_eq!(
        failed.event.payload["content"], "approval required: ap-1; the tool was not executed",
        "the log says what happened even though the transcript does not"
    );

    // But a wait is not an answer. The transcript stops at the proposal,
    // because nothing ran and nothing is decided — and because the first
    // outcome for a call is the one that sticks: if the wait were folded into
    // the transcript now, the settlement `resume` writes after the decision
    // could never replace it, and the model would be told forever that a call
    // it never got to make had failed.
    let sent = h.sent();
    assert!(
        sent.iter().all(|m| m.role != agent_protocol::model::MessageRole::Tool),
        "a held call has not been answered: {sent:?}"
    );
    // The log rebuilds everything the model was sent, plus the assistant
    // message the turn ended on — and still no tool result.
    assert_derived_extends_sent(&h, 1).await;
    let derived = h.derived().await;
    assert!(
        derived.iter().all(|m| m.role != agent_protocol::model::MessageRole::Tool),
        "the wait is not an answer: {derived:?}"
    );

    // Which is a claim about the future, so check the future: settling that
    // same call id later is what answers it.
    let step = failed.event.step_id.clone().expect("the invocation has a step");
    let call_id =
        derived[1].content[0].as_tool_use().expect("the proposal is a tool_use").0.to_string();
    let settlement = AgentStep::new(&h.session_id, &step);
    let next = h
        .store
        .read_stream(&h.session_id, EventSeq::ZERO, usize::MAX)
        .await
        .unwrap()
        .last()
        .map(|e| e.seq.next())
        .unwrap_or(EventSeq::ZERO);
    h.store
        .append(
            &h.session_id,
            next,
            vec![settlement.draft(
                EventType::TOOL_COMPLETED,
                serde_json::json!({
                    "tool_name": "echo",
                    "provider_tool_call_id": call_id.clone(),
                    "status": "completed",
                    "content": "the approved call ran",
                }),
            )],
        )
        .await
        .expect("settle the held call");

    let after = h.derived().await;
    let answered = after
        .iter()
        .find(|m| m.role == agent_protocol::model::MessageRole::Tool)
        .expect("the settlement answers the call the wait left open");
    assert_eq!(
        answered.content,
        ModelMessage::tool_for_call(&step, call_id, "echo", "the approved call ran", false,)
            .content
    );
}

#[tokio::test]
async fn rebuilding_the_context_twice_yields_identical_message_uids() {
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({ "s": "one" })),
        FakeModelProvider::answer("done"),
    ]));
    let tools = FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure);
    let h = harness(model, plain_gateway(tools));
    h.ask("first").await;

    let projection =
        Projection::new(ModelContextProjection::new(), h.store.clone(), h.data_dir.path());

    let first = projection.build_full(&h.session_id).await.expect("full replay");
    let second = projection.build_full(&h.session_id).await.expect("full replay again");
    let uids = |state: &agent_projection::ModelContextState| -> Vec<String> {
        projection.view(state).into_iter().map(|m| m.message_uid.as_str().to_string()).collect()
    };
    assert_eq!(uids(&first.state), uids(&second.state), "folding the same log twice differs");

    // A checkpoint is an accelerator, never a source of truth: reading through
    // one must land on the same transcript as reading the whole log.
    projection.checkpoint(&h.session_id, &first).await.expect("write checkpoint");
    let from_checkpoint = projection.build(&h.session_id).await.expect("checkpoint + tail");
    assert_eq!(from_checkpoint.state, first.state);
    assert_eq!(from_checkpoint.as_of_seq, first.as_of_seq);
}

#[test]
fn every_core_event_type_is_folded_or_explicitly_ignored() {
    // The fold fails closed on an unknown *required* event, so a core type it
    // silently forgot would not corrupt a transcript — it would make every
    // session containing that event unreadable. This is the tripwire for that.
    let definition = ModelContextProjection::new();
    for event_type in EventType::ALL {
        let stored = stored_event(event_type);
        let folded = definition.apply(&definition.init(), &stored);
        // Two admissible answers, and both are positive: the fold read it, or it
        // knows the type and will not read *this* payload — the probe carries no
        // payload at all, only the type. `model/response.completed` and
        // `session/forked` answer the second way (YA-04); every other core type
        // reads it. The answer that must never come back is `UnknownEventType`,
        // which is what this test has always been about.
        assert!(
            !matches!(folded, Err(ApplyError::UnknownEventType(_))),
            "core event '{event_type}' is neither folded nor ignored: {folded:?}"
        );
    }

    // The other half of the contract: a type nobody declared is not silently
    // absorbed either.
    let unknown = stored_event("made/up");
    assert_eq!(
        definition.apply(&definition.init(), &unknown),
        Err(ApplyError::UnknownEventType(EventType::new("made/up")))
    );
}

/// A record the fold cannot read stops the run rather than being folded away.
///
/// The log is the source of truth and the transcript is derived from it, so a
/// damaged record leaves the model a history quietly shorter than the log — its
/// own settled answer gone, the calls it proposed gone, and the tool results
/// that follow answering nothing. That has to reach whoever asked for the turn,
/// not be smoothed over by the fold.
#[tokio::test]
async fn a_damaged_record_stops_the_run_instead_of_shortening_the_history() {
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
    let h = harness(model, plain_gateway(FakeToolProvider::new()));
    h.ask("first").await;
    let log = h.log().await;
    let next = log.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO);

    // The shape a hand-edited or version-skewed log has: the fields the loop
    // always writes are there, and the response itself cannot be read back.
    let damaged = EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(EventType::MODEL_RESPONSE_COMPLETED),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: h.session_id.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor: Actor::Agent,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({
            "attempt_id": "attempt-1",
            "response": { "not": "a response" },
        }),
        metadata: serde_json::json!({}),
        ignorable: false,
    };
    h.store.append(&h.session_id, next, vec![damaged]).await.expect("append the damaged record");

    let engine = ProjectionContextEngine::new(h.store.clone(), h.data_dir.path());
    let error = engine
        .derive(&h.session_id)
        .await
        .expect_err("a record the fold cannot read must not be folded away");

    let message = error.to_string();
    assert!(
        message.contains("malformed payload") && message.contains(&format!("seq {next}")),
        "the error must name the record: {message}"
    );
}

/// A stored event of `event_type` with no payload. Enough to ask the fold
/// whether it recognizes the type at all.
fn stored_event(event_type: &str) -> StoredEvent {
    StoredEvent {
        seq: EventSeq::new(0),
        recorded_at: Utc::now(),
        event: EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::from_string("0.1.0"),
            session_id: SessionId::from_string("guard"),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::System,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::json!({}),
            metadata: serde_json::json!({}),
            ignorable: false,
        },
    }
}

/// A crash mid-step leaves one assistant message proposing three calls to the
/// same tool, all started, and only the first answered. Recovery has to settle
/// the calls that are *actually* open, and the transcript has to show the model
/// the same thing — this is where the kernel's pairing rule and the projection's
/// matching rule meet, so it is checked through both.
///
/// Three calls, not two: with two, a closer matched by name lands on the one
/// remaining open call whatever the kernel decided, so the transcript would read
/// the same either way and the test would prove nothing.
#[tokio::test]
async fn a_recovery_closer_settles_the_call_it_names_in_the_model_history() {
    let tools: Arc<dyn ToolProvider> =
        Arc::new(FakeToolProvider::new().with_tool("echo", "ok", ToolSideEffect::Pure));
    let gateway: Arc<dyn ToolGateway> = Arc::new(FakeToolGateway::new(tools.clone()));
    // The model is never asked: this log is the one a crash left behind.
    let h = harness(Arc::new(FakeModelProvider::new(Vec::new())), gateway);

    let step = StepId::new();
    let startup = AgentStep::new(&h.session_id, &step);
    h.store
        .append(
            &h.session_id,
            EventSeq::ZERO,
            vec![
                startup.draft(
                    EventType::MODEL_RESPONSE_COMPLETED,
                    serde_json::json!({
                        "attempt_id": "attempt/crash-1",
                        "response": {
                            "content_blocks": [
                                {
                                    "type": "tool_use",
                                    "tool_call_id": "call_1",
                                    "tool_name": "echo",
                                    "arguments": {},
                                },
                                {
                                    "type": "tool_use",
                                    "tool_call_id": "call_2",
                                    "tool_name": "echo",
                                    "arguments": {},
                                },
                                {
                                    "type": "tool_use",
                                    "tool_call_id": "call_3",
                                    "tool_name": "echo",
                                    "arguments": {},
                                },
                            ],
                            "finish_reason": "tool_calls",
                            "provider_metadata": null,
                            "usage": null,
                        },
                    }),
                ),
                startup.draft(
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "echo", "provider_tool_call_id": "call_1" }),
                ),
                startup.draft(
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "echo", "provider_tool_call_id": "call_2" }),
                ),
                startup.draft(
                    EventType::TOOL_STARTED,
                    serde_json::json!({ "tool_name": "echo", "provider_tool_call_id": "call_3" }),
                ),
                // The process died here: the first call got its answer, the
                // other two never did.
                startup.draft(
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({
                        "tool_name": "echo",
                        "provider_tool_call_id": "call_1",
                        "status": "completed",
                        "content": "first answer",
                    }),
                ),
            ],
        )
        .await
        .expect("seed the crashed step");

    let report =
        Recovery::new(h.store.clone(), tools).recover(&h.session_id).await.expect("recover");
    assert_eq!(report.tools_outcome_unknown, 2, "two calls never came back");

    let results: Vec<(String, String)> = h
        .derived()
        .await
        .into_iter()
        .flat_map(|message| message.content)
        .filter_map(|block| {
            block
                .as_tool_result()
                .map(|(id, _name, content, _error)| (id.to_string(), content.to_string()))
        })
        .collect();

    // The sentence the model reads comes out of the event, whole. It says what
    // is actually known (nobody recorded an outcome) and then what that means
    // for `echo`, which is a pure tool: running it again changes nothing.
    let settled = "This tool call was interrupted before its outcome was recorded, so it may or \
                   may not have run. It is a pure tool, so running it again changes nothing — \
                   re-issue it if you still need the result."
        .to_string();
    assert_eq!(
        results,
        vec![
            ("call_1".to_string(), "first answer".to_string()),
            // Call_1 is the one that finished, so a closer naming it would tell
            // the model its answered call never completed. And the two closers
            // land in log order: matching them by name alone attributes them
            // newest-first, which reverses which call is late.
            ("call_2".to_string(), settled.clone()),
            ("call_3".to_string(), settled),
        ]
    );
}

/// A draft for one step of the crashed turn.
struct AgentStep {
    session_id: SessionId,
    step_id: StepId,
}

impl AgentStep {
    fn new(session_id: &SessionId, step_id: &StepId) -> Self {
        Self { session_id: session_id.clone(), step_id: step_id.clone() }
    }

    fn draft(&self, event_type: &str, payload: serde_json::Value) -> EventDraft {
        EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::from_string("0.1.0"),
            session_id: self.session_id.clone(),
            run_id: None,
            turn_id: None,
            step_id: Some(self.step_id.clone()),
            actor: Actor::Tool,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: serde_json::json!({}),
            ignorable: false,
        }
    }
}

// ---------------------------------------------------------------------------
// M5 criterion 3: the user can steer at a step boundary
// ---------------------------------------------------------------------------

/// A two-step turn: the model calls a tool, then answers.
fn two_step_model() -> Arc<FakeModelProvider> {
    Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::answer("done"),
    ]))
}

/// A gateway whose one tool answers with a fixed word.
fn echo_gateway() -> Arc<dyn ToolGateway> {
    plain_gateway(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure))
}

/// Steering is a durable fact like any other input, and the model that saw it is
/// the model the log rebuilds.
///
/// The two halves matter separately: the note has to reach the loop (otherwise
/// the feature does not exist) and it has to be in the log (otherwise a resumed
/// session would hand the model a conversation the user never had).
#[tokio::test]
async fn steering_reaches_the_loop_and_survives_a_rebuild() {
    let h = harness(two_step_model(), echo_gateway());
    h.scheduler
        .inbox()
        .append(
            &h.session_id,
            InboxKind::NextTurn,
            serde_json::json!({ "text": "hi" }),
            0,
            Actor::User,
        )
        .await
        .expect("append input");
    let note =
        h.scheduler.inbox().steer(&h.session_id, "and check the tests", None).await.expect("steer");

    let report = h
        .scheduler
        .run_pending(&h.session_id, Utc::now())
        .await
        .expect("run pending")
        .expect("a turn ran");

    // The loop was told, as the same message the log will name: the uid is
    // (turn, item), so the two sides agree on its identity rather than merely
    // on its words.
    let expected =
        ModelMessage::user_for_steering(&report.turn_id, &note.item_id, "and check the tests");
    let requests = h.model.requests();
    assert_eq!(requests.len(), 2, "one request per step");
    // The note is the last thing the user said; the loop's own tail block
    // follows it and is not a message anyone steered with.
    let last_user = requests[0]
        .messages
        .iter()
        .rev()
        .find(|m| !m.message_uid.as_str().starts_with("msg/work-state/"));
    assert_eq!(last_user, Some(&expected));

    // And a fresh process, reading only the log, hands the model the same
    // conversation.
    let derived = h.derived().await;
    assert!(derived.contains(&expected), "the note must survive a rebuild: {derived:?}");
    assert_derived_extends_sent(&h, 1).await;

    // The record says where it went in and what it was, so an export or an audit
    // does not have to reconstruct either.
    let steered = h
        .log()
        .await
        .into_iter()
        .find(|e| e.event.event_type.as_str() == EventType::TURN_STEERED)
        .expect("turn/steered");
    assert_eq!(steered.event.payload["item_id"], note.item_id.as_str());
    assert_eq!(steered.event.payload["text"], "and check the tests");
    assert_eq!(steered.event.turn_id.as_ref(), Some(&report.turn_id));
}

/// A note changes a turn by exactly the events the note itself accounts for.
///
/// The same script is driven twice over the same store — once with a note
/// queued, once without — and the two logs are compared. This is the invariant
/// the change has to preserve: a session nobody steers is the session this code
/// drove before there was a steering queue, event for event.
#[tokio::test]
async fn a_note_changes_a_turn_by_exactly_the_events_it_accounts_for() {
    let plain = harness(two_step_model(), echo_gateway());
    plain.ask("hi").await;

    let steered = harness(two_step_model(), echo_gateway());
    steered
        .scheduler
        .inbox()
        .append(
            &steered.session_id,
            InboxKind::NextTurn,
            serde_json::json!({ "text": "hi" }),
            0,
            Actor::User,
        )
        .await
        .expect("append input");
    // The note's item id is not needed here: this test is about the shape of the
    // two runs, and the test above is the one that pins its identity.
    let _note = steered
        .scheduler
        .inbox()
        .steer(&steered.session_id, "and check the tests", None)
        .await
        .expect("steer");
    steered
        .scheduler
        .run_pending(&steered.session_id, Utc::now())
        .await
        .expect("run pending")
        .expect("a turn ran");

    // From the moment the run opens, the steered chain is the plain chain with
    // the note's claim and its injection spliced in at the turn boundary. The
    // note's own `inbox/appended` is the only difference before that, which is
    // why the comparison starts where it does.
    let tail = |types: &[String]| {
        let at = types.iter().position(|t| t == EventType::RUN_STARTED).expect("a run opened");
        types[at..].to_vec()
    };
    let mut expected = tail(&plain.types().await);
    // The claim, the injection, and the posture switch the note caused land at
    // the turn boundary, after whatever the loop records when it opens the turn
    // (the frozen tool set) and before the first step.
    let first_step = expected.iter().position(|t| t == EventType::STEP_STARTED).unwrap();
    expected.splice(
        first_step..first_step,
        [
            EventType::INBOX_CLAIMED.to_string(),
            EventType::TURN_STEERED.to_string(),
            EventType::REASON_RECORDED.to_string(),
        ],
    );
    assert_eq!(tail(&steered.types().await), expected);

    // What the model was told differs by the one message, and nothing else.
    // Message identity is per-run — a turn id is new every turn — so the two are
    // compared on what was said and in what role. That the note's uid is derived
    // from its item rather than minted is pinned by the test above.
    let note_shape = (MessageRole::User, "and check the tests".to_string());
    let mut without_note = shape(&steered.sent());
    without_note.retain(|m| *m != note_shape);
    assert_eq!(
        without_note,
        shape(&plain.sent()),
        "the note is the only thing the two models were told differently"
    );
}

/// What was said to the model, and in what role. Tool proposals carry no words;
/// text and tool outcomes are the two blocks that do.
fn shape(messages: &[ModelMessage]) -> Vec<(MessageRole, String)> {
    messages
        .iter()
        .map(|m| {
            let text = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                    ContentBlock::ToolUse { .. } => None,
                })
                .collect::<String>();
            (m.role, text)
        })
        .collect()
}

/// A repeated identical read is one copy plus a pointer, not two copies: the
/// window pays for the text once. The rewrite happens at derivation, so the
/// model still sees both results while it is working in the turn that made
/// them; only the settled history that later turns are built from is slimmed.
#[tokio::test]
async fn a_repeated_identical_read_is_collapsed_at_the_turn_boundary() {
    let read = "fs.read";
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call(read, serde_json::json!({ "path": "a.txt" })),
        FakeModelProvider::tool_call(read, serde_json::json!({ "path": "a.txt" })),
        FakeModelProvider::answer("done"),
    ]));
    let tools = FakeToolProvider::new().with_tool(read, "hello", ToolSideEffect::Read);
    // A window wide enough to keep everything: this test is about the rewrite,
    // not about trimming.
    let h = harness_with_window(
        model,
        plain_gateway(tools),
        Some(ContextPolicy {
            context_window_tokens: 100_000,
            input_working_set_tokens: 100_000,
            completion_cap_tokens: 0,
            compaction_trigger_tokens: 0,
            retain_recent_tokens: 50_000,
        }),
    );

    h.ask("read a.txt twice").await;

    let derived = h.derived().await;
    let results: Vec<String> = derived
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .map(outcome_text)
        .collect();
    assert_eq!(results.len(), 2, "both calls are still in the history: {derived:?}");
    assert!(
        results[0].starts_with("[This fs.read returned exactly what the call"),
        "{:?}",
        results[0]
    );
    assert_eq!(results[1], "hello", "the newer copy is the one that stays verbatim");
}

/// Without a window the engine sends what the log holds, so nothing is
/// rewritten: the same two reads arrive twice, exactly as before this rule.
#[tokio::test]
async fn without_a_window_a_repeated_read_is_sent_twice() {
    let read = "fs.read";
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call(read, serde_json::json!({ "path": "a.txt" })),
        FakeModelProvider::tool_call(read, serde_json::json!({ "path": "a.txt" })),
        FakeModelProvider::answer("done"),
    ]));
    let tools = FakeToolProvider::new().with_tool(read, "hello", ToolSideEffect::Read);
    let h = harness(model, plain_gateway(tools));

    h.ask("read a.txt twice").await;

    let results: Vec<String> = h
        .derived()
        .await
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .map(outcome_text)
        .collect();
    assert_eq!(results, vec!["hello".to_string(), "hello".to_string()]);
}

/// The text a tool-result message carries. `text_content` reads plain text
/// blocks only, and a tool outcome is a result block.
fn outcome_text(message: &ModelMessage) -> String {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default()
}
