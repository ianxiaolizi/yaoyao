//! The operator's criterion, asked at a step boundary.
//!
//! A run whose task is already done should not keep spending steps on it. These
//! drive a criterion through the fake gateway: one that passes (the turn ends,
//! and no further request is made), one that does not (the turn continues and
//! the log says the check ran), and one the gateway refuses (recorded as a
//! check that could not run — not as completion).

use super::*;
use agent_protocol::profile::{CompletionKind, CompletionSpec};

/// A loop that always proposes `echo`, with a criterion the fake gateway
/// answers with `result`.
fn checked_loop(
    result: ToolResult,
) -> (DefaultAgentLoop, Arc<FakeToolProvider>, Arc<FakeModelProvider>) {
    let model = Arc::new(FakeModelProvider::new(
        (0..40)
            .map(|_| FakeModelProvider::tool_call("echo", serde_json::json!({"s": "hi"})))
            .collect(),
    ));
    let tools = Arc::new(
        FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure).with_result(
            CHECK_TOOL,
            result,
            ToolSideEffect::Read,
        ),
    );
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(tools.clone())),
        no_context(),
        one_model_router(),
    );
    (loop_, tools, model)
}

/// The tool name the command criterion dispatches, as `completion` spells it.
const CHECK_TOOL: &str = "shell.run";

fn criterion(interval_steps: u32) -> CompletionSpec {
    CompletionSpec {
        kind: CompletionKind::Command,
        command: Some("python check.py".to_string()),
        url: None,
        interval_steps,
        timeout_ms: 1_000,
    }
}

fn commanded(mut turn: TurnContext, spec: CompletionSpec) -> TurnContext {
    turn.state = Some(TurnStateFacts { completion: Some(spec), ..facts() });
    turn
}

fn checks(outcome: &DrivenTurn) -> Vec<&EventDraft> {
    outcome
        .events
        .iter()
        .filter(|event| event.event_type.as_str() == EventType::COMPLETION_CHECKED)
        .collect()
}

/// The check passes on its interval boundary: the turn ends as `Completed`
/// before the request that boundary would have carried. One request, not two.
#[tokio::test]
async fn a_passing_check_ends_the_turn_before_its_next_request() {
    let (loop_, tools, model) = checked_loop(ToolResult::completed("ok"));
    let turn = commanded(ctx(), criterion(2));

    let outcome = drive_turn(&loop_, &turn, 8).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(model.requests().len(), 1, "the second step's request is never spent");
    assert_eq!(checks(&outcome).len(), 1);
    let payload = &checks(&outcome)[0].payload;
    assert_eq!(payload["kind"], "command");
    assert_eq!(payload["target"], "python check.py");
    assert_eq!(payload["status"], "completed");
    assert_eq!(payload["steps"], 2);

    // The criterion is the operator's command, run through the gateway the way
    // a model call would be — not a name the model can reach.
    let calls = tools.invocations();
    assert!(
        calls
            .iter()
            .any(|call| call.tool_name == CHECK_TOOL
                && call.arguments["command"] == "python check.py"),
        "the check ran as a configured call: {calls:?}"
    );
}

/// A check that does not pass is recorded and the turn carries on: the
/// criterion is a signal, not a stop.
#[tokio::test]
async fn a_failing_check_leaves_the_turn_running() {
    let (loop_, _, model) = checked_loop(ToolResult::failed("exit code 1\nnot done yet"));
    let turn = commanded(ctx(), criterion(2));

    let outcome = drive_turn(&loop_, &turn, 3).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::MaxSteps, "the budget ends it, not the check");
    assert_eq!(checks(&outcome).len(), 1);
    let payload = &checks(&outcome)[0].payload;
    assert_eq!(payload["status"], "incomplete");
    assert!(
        payload["detail"].as_str().unwrap_or_default().contains("not done yet"),
        "the check's output is recorded: {payload}"
    );
    assert_eq!(model.requests().len(), 3, "every step still asks the model");
}

/// The interval decides when the criterion is asked, and it is asked on the
/// step ordinal — 3 and 6, not 1 and 4.
#[tokio::test]
async fn the_criterion_is_asked_on_its_own_interval() {
    let (loop_, _, _) = checked_loop(ToolResult::failed("no"));
    let turn = commanded(ctx(), criterion(3));

    let outcome = drive_turn(&loop_, &turn, 7).await.expect("the turn ran");

    let steps: Vec<_> =
        checks(&outcome).iter().map(|event| event.payload["steps"].clone()).collect();
    assert_eq!(steps, vec![serde_json::json!(3), serde_json::json!(6)]);
}

/// A criterion the gateway cannot run is not completion: the failure is
/// recorded as an error and the turn continues.
#[tokio::test]
async fn a_check_the_gateway_cannot_run_is_an_error_not_completion() {
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("echo", serde_json::json!({"s": "hi"})),
        FakeModelProvider::answer("done"),
    ]));
    // No `shell.run` on this gateway: the criterion names a tool this run does
    // not have.
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    let loop_ = DefaultAgentLoop::new(
        model,
        Arc::new(FakeToolGateway::new(tools)),
        no_context(),
        one_model_router(),
    );
    let turn = commanded(ctx(), criterion(1));

    let outcome = drive_turn(&loop_, &turn, 4).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    let checks = checks(&outcome);
    assert_eq!(checks.len(), 2, "interval 1 asks at every boundary");
    for check in &checks {
        assert_eq!(check.payload["status"], "error");
        assert!(
            check.payload["detail"].as_str().unwrap_or_default().contains(CHECK_TOOL),
            "the failure names the tool: {}",
            check.payload
        );
    }
}

/// A run without a criterion asks nothing: the ordinary path is unchanged.
#[tokio::test]
async fn a_run_without_a_criterion_never_checks() {
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
    let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
    let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router());

    let outcome = drive_turn(&loop_, &ctx(), 4).await.expect("the turn ran");

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert!(checks(&outcome).is_empty());
}
