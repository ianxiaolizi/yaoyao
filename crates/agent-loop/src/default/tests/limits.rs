//! The limits a turn runs under, and what it is told about them.
//!
//! Three kinds sit next to each other here because they are read together: the
//! token budget a run is given, the step budget its caller pays for, and the
//! window reminder that warns before the next read fills the context. Each test
//! drives a real `DefaultAgentLoop` over fakes; none of them reaches inside a
//! step.

use super::*;

/// A tool-call response that reports its own token usage, the way a real
/// provider does. It proposes a call so the turn has a next step to reach —
/// an answer would end the turn before any budget boundary is checked. The
/// fake's ordinary responses report no usage, which is the other case the
/// budget has to handle.
fn priced_call(input: u64, output: u64, cache_hit: u64) -> ModelResponse {
    ModelResponse {
        content_blocks: vec![ContentBlock::tool_use(
            String::new(),
            "echo",
            serde_json::json!({"s": "hi"}),
        )],
        usage: Some(agent_protocol::model::Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            total_tokens: Some(input + output),
            cache_hit_tokens: Some(cache_hit),
            ..agent_protocol::model::Usage::default()
        }),
        finish_reason: agent_protocol::model::FinishReason::new(
            agent_protocol::model::FinishReason::TOOL_CALLS,
        ),
        provider_metadata: serde_json::Value::Null,
    }
}

/// The loop under test, with one pure `echo` tool on its surface.
fn echo_loop(model: Arc<FakeModelProvider>) -> DefaultAgentLoop {
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    DefaultAgentLoop::new(
        model,
        Arc::new(FakeToolGateway::new(tools)),
        no_context(),
        one_model_router(),
    )
}

/// The `budget/updated` records a turn wrote, in order.
fn budget_updates(outcome: &DrivenTurn) -> Vec<&EventDraft> {
    outcome.events.iter().filter(|e| e.event_type.as_str() == EventType::BUDGET_UPDATED).collect()
}

/// Usage the provider reports is added up across steps, and the step
/// boundary after the response that crossed `max_total_tokens` is where the
/// run stops — the response itself is not aborted, so the overshoot is one
/// request.
#[tokio::test]
async fn reported_usage_accumulates_and_stops_the_run_at_its_token_budget() {
    let model = Arc::new(FakeModelProvider::new(vec![
        priced_call(100, 20, 50),
        priced_call(100, 20, 50),
        priced_call(100, 20, 50),
    ]));
    let loop_ = echo_loop(model.clone());

    let mut turn = ctx();
    turn.state = Some(TurnStateFacts {
        budget: RunBudget { max_total_tokens: Some(150), ..RunBudget::default() },
        ..facts()
    });
    let outcome = run_turn(&loop_, &turn).await;

    // 120 per response: the first is under the limit, the second crosses
    // it, and the third step never opens.
    assert_eq!(outcome.stop_reason, StopReason::TokenBudgetExceeded);
    assert_eq!(model.requests().len(), 2);

    let updates = budget_updates(&outcome);
    assert_eq!(updates.len(), 2, "one record per completed response");
    let last = &updates[1].payload["spent"];
    assert_eq!(last["input_tokens"], 200);
    assert_eq!(last["output_tokens"], 40);
    assert_eq!(last["cache_hit_tokens"], 100);
    assert_eq!(last["estimated"], false);

    // The second request was assembled knowing what the first had cost.
    let prepared: Vec<_> = outcome
        .events
        .iter()
        .filter(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
        .collect();
    assert_eq!(prepared.len(), 2);
    assert_eq!(prepared[0].payload["spent"]["input_tokens"], 0);
    assert_eq!(prepared[1].payload["spent"]["input_tokens"], 100);
}

/// A provider that reports nothing is estimated rather than counted as
/// zero: a silent free run would outlive the budget it was configured with.
#[tokio::test]
async fn unreported_usage_is_estimated_and_flagged() {
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::tool_call(
        "echo",
        serde_json::json!({"s": "hi"}),
    )]));
    let loop_ = echo_loop(model);

    let mut turn = ctx();
    turn.state = Some(TurnStateFacts {
        budget: RunBudget { max_total_tokens: Some(1), ..RunBudget::default() },
        ..facts()
    });
    let outcome = run_turn(&loop_, &turn).await;

    assert_eq!(outcome.stop_reason, StopReason::TokenBudgetExceeded);
    let updates = budget_updates(&outcome);
    assert_eq!(updates.len(), 1);
    let spent = &updates[0].payload["spent"];
    assert_eq!(spent["estimated"], true);
    assert!(spent["input_tokens"].as_u64().unwrap() > 0, "{spent}");
}

/// Every request carries one budget reminder at its tail: the numbers always,
/// the wrap-up advice from the finalize threshold on. The run still ends only
/// when the budget is actually spent.
#[tokio::test]
async fn the_budget_reminder_is_one_tail_message_that_turns_final() {
    let model = Arc::new(FakeModelProvider::new(vec![
        priced_call(450, 0, 0),
        priced_call(450, 0, 0),
        priced_call(450, 0, 0),
    ]));
    let loop_ = echo_loop(model.clone());

    let mut turn = ctx();
    turn.state = Some(TurnStateFacts {
        budget: RunBudget {
            max_total_tokens: Some(1_000),
            finalize_at_percent: Some(85),
            ..RunBudget::default()
        },
        ..facts()
    });
    let outcome = run_turn(&loop_, &turn).await;

    // 0% -> 45% -> 90%: the third request is the first in finalize, and
    // the response it produces spends the rest.
    assert_eq!(outcome.stop_reason, StopReason::TokenBudgetExceeded);
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    let uid = format!("msg/budget/{}", turn.turn_id);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            request.messages.iter().filter(|m| m.message_uid.as_str() == uid).count(),
            1,
            "request {index} carries one reminder, not a pile"
        );
        assert_eq!(
            request.messages.last().unwrap().message_uid.as_str(),
            uid,
            "the reminder is the last thing the model reads"
        );
    }
    let normal = requests[1]
        .messages
        .last()
        .map(|m| m.text_content())
        .expect("the second request carries a reminder");
    assert!(normal.contains("spent 450 of its 1000-token budget"), "{normal}");
    assert!(normal.contains("Phase: normal"), "45% is not the finalize phase: {normal}");
    let finalize_request = &requests[2];
    let final_text = finalize_request.messages.last().unwrap().text_content();
    assert!(final_text.contains("Do not open a new line of work"), "{final_text}");

    let prepared: Vec<_> = outcome
        .events
        .iter()
        .filter(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
        .collect();
    assert_eq!(prepared[2].payload["budget_phase"], "finalize");
    assert_eq!(prepared[2].payload["budget_notice"]["percent"], 90);
    // The request before it was not finalizing, and the log says so beside the
    // reminder it did carry: what the model saw is readable per request.
    assert_eq!(prepared[1].payload["budget_phase"], "normal");
    assert_eq!(prepared[1].payload["budget_notice"]["percent"], 45);
    assert!(
        prepared[1].payload["budget_notice"]["text"].as_str().unwrap().contains("Phase: normal")
    );
}

/// A context whose derivation reports `settled` tokens against a `window` of
/// input budget, with `history` as its one settled message when it is stated.
fn notice_context(
    history: Option<&'static str>,
    settled: usize,
    window: u32,
) -> Arc<dyn ContextEngine> {
    let history: Vec<ModelMessage> = history
        .map(|text| ModelMessage::user_for_turn(&agent_protocol::ids::TurnId::new(), text))
        .into_iter()
        .collect();
    Arc::new(FakeContextEngine::new(history, EventSeq::new(3)).with_usage(ContextUsage {
        dropped_messages: 0,
        estimated_tokens: settled,
        max_input_tokens: Some(window),
        reserved_output_tokens: Some(0),
    }))
}

/// What the loop reports for `turn`'s first request when no window is near.
///
/// The notice's percentage is measured over the whole request now — tool
/// definitions included — so a test places its window as a share of this
/// number instead of re-deriving the manifest's cost. A window of a million
/// tokens cannot reach the threshold, and what the prepared record says is
/// exactly the estimate the next run's `begin_turn` will make.
async fn first_request_tokens(
    turn: &TurnContext,
    history: Option<&'static str>,
    settled: usize,
) -> usize {
    let loop_ = DefaultAgentLoop::new(
        Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")])),
        Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
        notice_context(history, settled, 1_000_000),
        one_model_router(),
    );
    let outcome = run_turn(&loop_, turn).await;
    let payload = prepared_payload(&outcome);
    assert!(payload.get("context_notice").is_none(), "a million-token window is not near");
    usize::try_from(payload["estimated_request_tokens"].as_u64().unwrap_or(0)).unwrap_or(0)
}

/// A window that is configured but not yet full is still worth one line
/// when the next large read could exhaust it. The reminder goes after
/// settled history, immediately before this turn's input, so it reads as a
/// condition of this turn and cannot break the cached prefix.
#[tokio::test]
async fn a_context_near_the_budget_is_reminded_once_after_history() {
    let mut ctx = ctx();
    ctx.instructions = Some(RunInstructions {
        text: "the run's rules".to_string(),
        project_file: None,
        builtin: "test".to_string(),
    });
    let tokens = first_request_tokens(&ctx, Some("history"), 480).await;
    let window = u32::try_from(tokens * 100 / 96).expect("a window fits a `u32`");
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
        notice_context(Some("history"), 480, window),
        one_model_router(),
    );

    let outcome = run_turn(&loop_, &ctx).await;
    let payload = prepared_payload(&outcome);
    assert_eq!(payload["context_notice"]["kind"], "near-limit");
    assert_eq!(payload["context_notice"]["percent"], 96);
    assert_eq!(payload["context_notice"]["max_input_tokens"], window);

    let request = &model.requests()[0];
    assert_eq!(request.messages[0].role, MessageRole::System);
    assert_eq!(request.messages[0].text_content(), "the run's rules");
    assert_eq!(request.messages[1].text_content(), "history");
    // The identity layer sits with the frame, before the notice: "who answers"
    // is a standing fact about the turn and "how full this is" is measured over
    // it.
    assert_eq!(request.messages[2].message_uid.as_str(), format!("msg/identity/{}", ctx.turn_id));
    assert_eq!(request.messages[3].role, MessageRole::System);
    assert_eq!(request.messages[3].message_uid.as_str(), format!("msg/notice/{}", ctx.turn_id));
    let notice = request.messages[3].text_content();
    let left = u64::from(window) - u64::try_from(tokens).expect("tokens fit a `u64`");
    assert!(notice.contains("this request is using about 96%"), "{notice}");
    assert!(notice.contains(&format!("{left} tokens left")), "{notice}");
    assert_eq!(request.messages[4], ModelMessage::user_for_turn(&ctx.turn_id, "hi"));
}

#[tokio::test]
async fn a_context_below_the_threshold_carries_no_notice() {
    let ctx = ctx();
    let tokens = first_request_tokens(&ctx, None, 424).await;
    let window = u32::try_from(tokens * 100 / 84).expect("a window fits a `u32`");
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
    let loop_ = DefaultAgentLoop::new(
        model,
        Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
        notice_context(None, 424, window),
        one_model_router(),
    );

    let outcome = run_turn(&loop_, &ctx).await;
    let payload = prepared_payload(&outcome);
    assert!(payload.get("context_notice").is_none(), "84% is below the threshold: {payload}");
    // The identity layer and this turn's input, and nothing else.
    assert_eq!(payload["message_count"], 2, "{payload}");
}

#[tokio::test]
async fn a_context_without_a_configured_window_carries_no_notice() {
    let context: Arc<dyn ContextEngine> =
        Arc::new(FakeContextEngine::new(vec![], EventSeq::new(1)).with_usage(ContextUsage {
            dropped_messages: 0,
            estimated_tokens: 1_000_000,
            max_input_tokens: None,
            reserved_output_tokens: None,
        }));
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
    let loop_ = DefaultAgentLoop::new(
        model,
        Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
        context,
        one_model_router(),
    );

    let outcome = run_turn(&loop_, &ctx()).await;
    let payload = prepared_payload(&outcome);
    assert!(payload.get("context_notice").is_none(), "{payload}");
    assert_eq!(payload["message_count"], 2, "the identity layer and the input");
}
