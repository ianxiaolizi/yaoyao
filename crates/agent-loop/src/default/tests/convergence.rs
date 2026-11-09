//! One file rewritten over and over with no passing check, and what the turn is
//! told about it.
//!
//! The stall notice asks whether the turn is learning anything. This asks
//! whether what it keeps changing has ever been looked at. Both are reminders
//! placed on the request, both are cleared when their condition stops holding,
//! and neither ends the turn — these tests drive those three properties through
//! the real step loop with a fake model and a fake gateway.

use super::*;

const PATH: &str = "web/app.js";

/// One `fs.write` to [`PATH`].
fn write_call(content: &str) -> agent_protocol::model::ModelResponse {
    FakeModelProvider::tool_call(
        "fs.write",
        serde_json::json!({ "path": PATH, "content": content }),
    )
}

/// One `shell.run` carrying `command`.
fn check_call(command: &str) -> agent_protocol::model::ModelResponse {
    FakeModelProvider::tool_call("shell.run", serde_json::json!({ "command": command }))
}

/// A loop over `responses` and a gateway with an editor and a command runner.
///
/// `shell.run` is registered as a read, not a destructive call: the question
/// here is what the loop says after a check, not whether a check needs consent.
fn writing_loop(
    responses: Vec<agent_protocol::model::ModelResponse>,
    tools: FakeToolProvider,
) -> (DefaultAgentLoop, Arc<FakeModelProvider>, Arc<FakeToolProvider>) {
    let model = Arc::new(FakeModelProvider::new(responses));
    let tools = Arc::new(tools);
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(tools.clone())),
        no_context(),
        one_model_router(),
    );
    (loop_, model, tools)
}

/// An editor and a command runner that completes.
fn editing_tools() -> FakeToolProvider {
    FakeToolProvider::new()
        .with_tool("fs.write", "written", ToolSideEffect::Write)
        .with_tool("fs.edit", "edited", ToolSideEffect::Write)
        .with_tool("shell.run", "ok", ToolSideEffect::Read)
}

/// The requests that carried the convergence notice, by index.
fn noticed_requests(model: &FakeModelProvider, turn: &TurnContext) -> Vec<usize> {
    let uid = format!("msg/convergence/{}", turn.turn_id);
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

/// The prepared-record payloads, in request order.
fn prepared(outcome: &DrivenTurn) -> Vec<&EventDraft> {
    outcome
        .events
        .iter()
        .filter(|event| event.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
        .collect()
}

/// The third edit with no check in between arms one notice on the next request;
/// two edits do not. The turn still ends on the model's answer: the notice is a
/// reminder, not a stop.
#[tokio::test]
async fn three_edits_without_a_check_arm_one_notice() {
    let (loop_, model, _) = writing_loop(
        vec![
            write_call("v1"),
            write_call("v2"),
            write_call("v3"),
            FakeModelProvider::answer("done"),
        ],
        editing_tools(),
    );
    let turn = ctx();

    let outcome = drive_turn(&loop_, &turn, 8).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer, "a reminder is not a stop");
    assert_eq!(
        noticed_requests(&model, &turn),
        vec![3],
        "armed on the request after the third edit"
    );

    // The notice is the last thing the model reads, and it names the file and
    // how far the drift has gone.
    let request = &model.requests()[3];
    let last = request.messages.last().expect("a notice is placed");
    assert_eq!(last.message_uid.as_str(), format!("msg/convergence/{}", turn.turn_id));
    let text = last.text_content();
    assert!(text.contains(PATH), "{text}");
    assert!(text.contains("3 times"), "{text}");

    // The prepared record carries the same facts, so the sentence can be read
    // back without re-deriving the turn.
    let prepared = prepared(&outcome);
    let notice = &prepared[3].payload["convergence_notice"];
    assert_eq!(notice["path"], PATH);
    assert_eq!(notice["edits"], 3);
    assert!(prepared[2].payload.get("convergence_notice").is_none(), "two edits are ordinary work");
}

/// A passing check clears the notice and every count: the next edit starts from
/// one again, so it does not immediately re-arm.
#[tokio::test]
async fn a_passing_check_clears_the_notice() {
    let (loop_, model, tools) = writing_loop(
        vec![
            write_call("v1"),
            write_call("v2"),
            write_call("v3"),
            check_call("cargo test"),
            write_call("v4"),
            FakeModelProvider::answer("done"),
        ],
        editing_tools(),
    );
    let turn = ctx();

    let outcome = drive_turn(&loop_, &turn, 8).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert_eq!(noticed_requests(&model, &turn), vec![3], "the check answered it");
    assert_eq!(tools.call_count(), 5, "every call reached the gateway");
}

/// A check that fails is not cover: the notice stays, and the count keeps
/// moving, because the file has still not been shown to be right.
#[tokio::test]
async fn a_failing_check_leaves_the_notice_in_place() {
    let tools = FakeToolProvider::new()
        .with_tool("fs.write", "written", ToolSideEffect::Write)
        .with_scripted_results(
            "shell.run",
            vec![ToolResult::failed("exit code 1\nnot done yet")],
            ToolSideEffect::Read,
        );
    let (loop_, model, _) = writing_loop(
        vec![
            write_call("v1"),
            write_call("v2"),
            write_call("v3"),
            check_call("cargo test"),
            write_call("v4"),
            FakeModelProvider::answer("done"),
        ],
        tools,
    );
    let turn = ctx();

    let outcome = drive_turn(&loop_, &turn, 8).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert_eq!(noticed_requests(&model, &turn), vec![3, 4, 5]);

    // The fourth edit replaces the notice rather than adding a second.
    let uid = format!("msg/convergence/{}", turn.turn_id);
    let carried = model.requests()[5]
        .messages
        .iter()
        .filter(|message| message.message_uid.as_str() == uid)
        .count();
    assert_eq!(carried, 1, "one notice, replaced in place");

    // The failure did not reset anything: the next edit is the fourth.
    let prepared = prepared(&outcome);
    assert_eq!(prepared[3].payload["convergence_notice"]["edits"], 3);
    assert_eq!(prepared[4].payload["convergence_notice"]["edits"], 3);
    assert_eq!(prepared[5].payload["convergence_notice"]["edits"], 4);
}

/// Three edits spread over three files are not a file drifting: no notice.
#[tokio::test]
async fn edits_to_different_files_never_arm_the_notice() {
    let (loop_, model, _) = writing_loop(
        vec![
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({ "path": "a.js", "content": "1" }),
            ),
            FakeModelProvider::tool_call(
                "fs.edit",
                serde_json::json!({ "path": "b.js", "content": "2" }),
            ),
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({ "path": "c.js", "content": "3" }),
            ),
            FakeModelProvider::answer("done"),
        ],
        editing_tools(),
    );
    let turn = ctx();

    let outcome = drive_turn(&loop_, &turn, 8).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert!(noticed_requests(&model, &turn).is_empty());
}
