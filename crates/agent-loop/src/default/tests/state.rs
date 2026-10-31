//! The work state a long turn keeps: what `state.update` records, what the
//! tail block shows, what a new turn opens with, and what settlement does with
//! the traffic the state stands in for.
//!
//! All of them drive a real `DefaultAgentLoop` over fakes and read only what a
//! reader of the log could read: the events the turn wrote and the requests the
//! provider received.

use super::*;
use agent_protocol::state::WorkState;

/// A loop whose surface is one pure `echo` tool returning `result`.
fn echo_loop(model: Arc<FakeModelProvider>, result: &str) -> DefaultAgentLoop {
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", result, ToolSideEffect::Pure));
    DefaultAgentLoop::new(
        model,
        Arc::new(FakeToolGateway::new(tools)),
        no_context(),
        one_model_router(),
    )
}

/// A context engine that reports a window, so the loop has a settle share to
/// measure the request against.
fn windowed(window: u32) -> Arc<dyn ContextEngine> {
    Arc::new(FakeContextEngine::new(Vec::new(), EventSeq::new(1)).with_usage(ContextUsage {
        dropped_messages: 0,
        estimated_tokens: 0,
        input_working_set_tokens: Some(window),
    }))
}

/// What a model-written state looks like in a tool call.
fn state_call(goal: &str) -> ModelResponse {
    FakeModelProvider::tool_call(
        "state.update",
        serde_json::json!({
            "goal": goal,
            "next": "run the tests",
            "done": [{ "what": "wrote the lexer", "evidence": "call-0" }],
        }),
    )
}

fn state_events(outcome: &DrivenTurn) -> Vec<&EventDraft> {
    outcome.events.iter().filter(|e| e.event_type.as_str() == EventType::STATE_UPDATED).collect()
}

/// The uid the tail block of one turn carries.
fn block_uid(turn: &TurnContext) -> String {
    format!("msg/work-state/{}", turn.turn_id)
}

/// The loop's answer is recorded, and the next request reads the same words
/// back at its tail: one block, replaced rather than repeated.
#[tokio::test]
async fn a_state_update_is_recorded_and_read_back_at_the_tail() {
    let model = Arc::new(FakeModelProvider::new(vec![
        state_call("ship the parser"),
        FakeModelProvider::answer("done"),
    ]));
    let loop_ = echo_loop(model.clone(), "ok");
    let mut turn = ctx();
    turn.work = Some(WorkState::default());
    let outcome = run_turn(&loop_, &turn).await;

    let updated = state_events(&outcome);
    assert_eq!(updated.len(), 1, "one accepted state, one event");
    assert_eq!(updated[0].payload["schema"], "state/1");
    assert_eq!(updated[0].payload["state"]["goal"], "ship the parser");
    // The answer to the call names what was recorded, so the model learns the
    // write landed without reading the tail to find out.
    let recorded = outcome
        .events
        .iter()
        .find(|e| e.event_type.as_str() == EventType::TOOL_COMPLETED)
        .expect("the result of the state call");
    assert_eq!(recorded.payload["content"], "state recorded: 1 item(s) done");

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    // The first request had the placeholder (nothing recorded yet); the second
    // carries the account.
    let first = requests[0].messages.iter().find(|m| m.message_uid.as_str() == block_uid(&turn));
    assert!(
        first
            .expect("a block in the first request")
            .text_content()
            .contains("has not recorded one yet")
    );
    let blocks: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|m| m.message_uid.as_str() == block_uid(&turn))
        .collect();
    assert_eq!(blocks.len(), 1, "one block, not a pile");
    let text = blocks[0].text_content();
    assert!(text.contains("Account:"), "{text}");
    assert!(text.contains("ship the parser"), "{text}");
    assert!(text.contains("run the tests"), "{text}");
    assert_eq!(
        requests[1].messages.last().expect("a tail").message_uid.as_str(),
        block_uid(&turn),
        "the block is the last thing the model reads when nothing else is a reminder"
    );
}

/// A state that does not read is refused with the reason, and nothing is
/// recorded: the next step keeps the state the log already holds.
#[tokio::test]
async fn a_refused_state_records_nothing() {
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("state.update", serde_json::json!({ "next": "x" })),
        FakeModelProvider::answer("done"),
    ]));
    let loop_ = echo_loop(model.clone(), "ok");
    let mut turn = ctx();
    turn.work = Some(WorkState::default());
    let outcome = run_turn(&loop_, &turn).await;

    assert!(state_events(&outcome).is_empty(), "a refused state is not an event");
    let failed = outcome
        .events
        .iter()
        .find(|e| e.event_type.as_str() == EventType::TOOL_FAILED)
        .expect("the refusal");
    let content = failed.payload["content"].as_str().expect("a message for the model");
    assert!(content.starts_with("state not recorded:"), "{content}");
    let requests = model.requests();
    let blocks: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|m| m.message_uid.as_str() == block_uid(&turn))
        .collect();
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].text_content().contains("has not recorded one yet"));
}

/// A turn opens with the state a previous turn left: the task layer says so in
/// the model's own terms, beside the advice to keep it current.
#[tokio::test]
async fn a_carried_state_opens_the_task_layer() {
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
    let loop_ = echo_loop(model.clone(), "ok");
    let mut turn = ctx();
    turn.state = Some(facts());
    turn.work = Some(WorkState {
        goal: "ship the parser".into(),
        next: "run the tests".into(),
        blocked: Some("waiting on the fixture".into()),
        ..WorkState::default()
    });
    run_turn(&loop_, &turn).await;

    let request = &model.requests()[0];
    let task = request
        .messages
        .iter()
        .find(|m| m.message_uid.as_str() == format!("msg/task/{}", turn.turn_id))
        .expect("the task layer");
    let text = task.text_content();
    assert!(text.contains("Work state carried over"), "{text}");
    assert!(text.contains("ship the parser"), "{text}");
    assert!(text.contains("waiting on the fixture"), "{text}");
    assert!(text.contains("Keep the work state current with `state.update`"), "{text}");
}

/// A request that would spend the settle share replaces its older traffic with
/// the state, without asking a provider anything — and the next request still
/// carries the user's words and the goal.
#[tokio::test]
async fn settlement_replaces_traffic_without_asking_the_model() {
    let big = "x".repeat(6_000);
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::answer("done"),
    ]));
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", &big, ToolSideEffect::Pure));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(tools)),
        windowed(3_000),
        one_model_router(),
    );
    let mut turn = ctx();
    turn.work = Some(WorkState {
        goal: "ship the parser".into(),
        next: "run the tests".into(),
        ..WorkState::default()
    });
    let outcome = run_turn(&loop_, &turn).await;

    let compacted: Vec<_> = outcome
        .events
        .iter()
        .filter(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTED)
        .collect();
    assert!(!compacted.is_empty(), "the request crossed the settle share and was settled");
    let settlement = &compacted[0].payload;
    assert_eq!(settlement["strategy"], "state/1");
    assert_eq!(settlement["trigger"], "estimated_request");
    assert_eq!(settlement["fallback"], "none");
    assert_eq!(settlement["subjective"], "present");
    assert_eq!(settlement["scope"], "in-flight");
    assert!(settlement["state_hash"].as_str().is_some_and(|h| !h.is_empty()));
    let before = settlement["estimated_before"].as_u64().expect("a before");
    let after = settlement["estimated_after"].as_u64().expect("an after");
    assert!(after < before, "the settlement must leave a smaller request: {after} < {before}");
    assert!(settlement["dropped_messages"].as_u64().unwrap() >= 2);

    // No summarizer ran: every request is the turn's own step.
    assert!(
        !outcome
            .events
            .iter()
            .any(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTION_STARTED),
        "a settlement does not ask the provider to summarise"
    );
    assert_eq!(model.requests().len(), 4);

    // The user's words and the goal are still there afterwards, and the
    // settlement says where they went.
    let requests = model.requests();
    let after_request = requests.last().expect("the last request");
    assert!(
        after_request.messages.iter().any(|m| m.text_content().contains("hi")),
        "the input stays verbatim"
    );
    let summaries: Vec<_> = after_request
        .messages
        .iter()
        .filter(|m| m.message_uid.as_str().starts_with("msg/summary/"))
        .collect();
    assert_eq!(summaries.len(), 1, "a later settlement replaces the earlier record");
    let summary = summaries[0];
    let text = summary.text_content();
    assert!(text.contains("Settlement (state/1)"), "{text}");
    assert!(text.contains("ship the parser"), "{text}");
    assert!(text.contains("Objective:"), "{text}");
}

/// With no state to write from, the trigger falls back to the summarizer and
/// the event says so.
#[tokio::test]
async fn a_turn_without_a_state_falls_back_to_the_summarizer() {
    let big = "x".repeat(6_000);
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::answer("the handover"),
        FakeModelProvider::tool_call("echo", serde_json::json!({})),
        FakeModelProvider::answer("done"),
    ]));
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", &big, ToolSideEffect::Pure));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(tools)),
        windowed(3_000),
        one_model_router(),
    );
    // `ctx()` carries no `work`: a harness that keeps no state record.
    let outcome = run_turn(&loop_, &ctx()).await;

    let compacted: Vec<_> = outcome
        .events
        .iter()
        .filter(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTED)
        .collect();
    assert_eq!(compacted.len(), 1);
    assert_eq!(compacted[0].payload["strategy"], "summary");
    assert_eq!(compacted[0].payload["fallback"], "settlement");
    assert_eq!(compacted[0].payload["summary"], "the handover");
}
