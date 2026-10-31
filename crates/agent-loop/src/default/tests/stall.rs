//! A turn that keeps asking the same question, and what it is told.
//!
//! The loop counts steps without new evidence and does two things with the
//! count: one reminder at the configured threshold, and the end of the turn at
//! twice that. Both are driven here over a fake tool that answers the same way
//! every time — the shape of the 2026-10-31 run that re-read the same files
//! until its step budget was gone.

use super::*;

/// A loop whose model proposes `echo` with `arguments` for `steps` responses,
/// followed by an answer it never reaches when the turn stalls.
fn echoing_loop(
    steps: usize,
    arguments: impl Fn(usize) -> serde_json::Value,
) -> (DefaultAgentLoop, Arc<FakeModelProvider>) {
    let mut responses: Vec<agent_protocol::model::ModelResponse> =
        (0..steps).map(|index| FakeModelProvider::tool_call("echo", arguments(index))).collect();
    responses.push(FakeModelProvider::answer("done"));
    let model = Arc::new(FakeModelProvider::new(responses));
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(tools)),
        no_context(),
        one_model_router(),
    );
    (loop_, model)
}

/// The requests that carried the stall reminder, by index.
fn reminded_requests(model: &FakeModelProvider, turn: &TurnContext) -> Vec<usize> {
    let uid = format!("msg/stall/{}", turn.turn_id);
    model
        .requests()
        .iter()
        .enumerate()
        .filter(|(_, request)| {
            request.messages.iter().any(|message| message.message_uid.as_str() == uid)
        })
        .map(|(index, _)| index)
        .collect()
}

/// With the default threshold (six steps), the reminder arrives on the eighth
/// request and the turn ends at the fourteenth boundary — thirteen requests, no
/// more. The first step is the one that learns (the call is new to the turn),
/// so the streak reaches six on the seventh request.
#[tokio::test]
async fn a_turn_that_keeps_repeating_itself_is_reminded_once_then_stopped() {
    let (loop_, model) = echoing_loop(40, |_| serde_json::json!({"s": "hi"}));
    let turn = ctx();

    let outcome = drive_turn(&loop_, &turn, 40).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::Stalled);
    assert_eq!(model.requests().len(), 13, "twice the default threshold ends the turn");
    assert_eq!(reminded_requests(&model, &turn), (7..13).collect::<Vec<_>>());

    // The reminder is the last thing the model reads, and it says what to do
    // instead: change course, or record the blockage.
    let request = &model.requests()[7];
    let last = request.messages.last().expect("a reminder is placed");
    assert_eq!(last.message_uid.as_str(), format!("msg/stall/{}", turn.turn_id));
    let text = last.text_content();
    assert!(text.contains("the last 6 steps produced no new evidence"), "{text}");
    assert!(text.contains("record what is blocked"), "{text}");
    assert!(text.contains("ends as stalled"), "{text}");

    // The prepared record carries the streak and the limit it was measured
    // against, so the sentence can be read back without re-deriving the turn.
    let prepared: Vec<_> = outcome
        .events
        .iter()
        .filter(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
        .collect();
    let notice = &prepared[7].payload["stall_notice"];
    assert_eq!(notice["unchanged_steps"], 6);
    assert_eq!(notice["stall_steps"], 6);
    assert!(prepared[6].payload.get("stall_notice").is_none(), "no warning before the threshold");
}

/// A call with different arguments is a different reading, so the streak never
/// starts — even with a threshold of two.
#[tokio::test]
async fn a_turn_that_reads_something_different_every_step_is_never_warned() {
    let (loop_, model) = echoing_loop(10, |index| serde_json::json!({"s": index}));
    let mut turn = ctx();
    turn.state = Some(TurnStateFacts {
        budget: RunBudget { stall_steps: Some(2), ..RunBudget::default() },
        ..facts()
    });

    let outcome = drive_turn(&loop_, &turn, 20).await.expect("the turn ran");

    // Ten different calls, then the answer: no warning, no stall.
    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert!(reminded_requests(&model, &turn).is_empty(), "different arguments are new evidence");
}

/// The threshold is the run's, and the stop is at twice it.
#[tokio::test]
async fn a_threshold_of_two_warns_after_two_silent_steps_and_stops_at_four() {
    let (loop_, model) = echoing_loop(20, |_| serde_json::json!({"s": "hi"}));
    let mut turn = ctx();
    turn.state = Some(TurnStateFacts {
        budget: RunBudget { stall_steps: Some(2), ..RunBudget::default() },
        ..facts()
    });

    let outcome = drive_turn(&loop_, &turn, 20).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::Stalled);
    assert_eq!(model.requests().len(), 5);
    assert_eq!(reminded_requests(&model, &turn), vec![3, 4]);
}

/// A turn whose first look is repeated held by the repeat gate still counts as
/// a silent step: the kernel already said the call would do the same thing.
#[tokio::test]
async fn a_held_step_counts_as_no_progress() {
    let (loop_, model) = echoing_loop(6, |_| serde_json::json!({"s": "hi"}));
    let mut turn = ctx();
    turn.state = Some(TurnStateFacts {
        budget: RunBudget { stall_steps: Some(2), ..RunBudget::default() },
        ..facts()
    });

    let outcome = drive_turn(&loop_, &turn, 6).await.expect("the turn ran");

    // Step 1 learns, step 2 repeats, step 3 is held by the gate, step 4
    // repeats (with a reminder), step 5 repeats, and the sixth boundary ends
    // the turn.
    assert_eq!(outcome.stop_reason, StopReason::Stalled);
    assert_eq!(model.requests().len(), 5);
    let held: Vec<_> = outcome
        .events
        .iter()
        .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
        .collect();
    assert_eq!(held.len(), 1, "the gate fired once");
}
