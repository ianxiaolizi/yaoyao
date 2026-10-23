//! CLI tool integration: run a single tool through the guarded gateway seam.

use std::sync::Arc;

use agent_cli::{CliOutput, Verdict, run_tool};
use agent_ports::tool_provider::{ToolResult, ToolSideEffect};
use agent_protocol::ids::SessionId;
use agent_testkit::{FakeToolGateway, FakeToolProvider};

#[tokio::test]
async fn tool_runs_and_returns_content() {
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    let gateway = FakeToolGateway::new(tools);
    let sid = SessionId::from_string("tool-cli-session");

    let outcome = run_tool(&gateway, &sid, "echo", serde_json::json!({}), false).await.unwrap();
    match outcome.output {
        CliOutput::Text(t) => assert_eq!(t, "hello"),
        _ => panic!("expected text output"),
    }
    assert_eq!(
        outcome.verdict,
        Verdict::Answered,
        "a tool that completed is not a command that failed"
    );
}

#[tokio::test]
async fn tool_json_serializes_the_result() {
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    let gateway = FakeToolGateway::new(tools);
    let sid = SessionId::from_string("tool-cli-session");

    let outcome = run_tool(&gateway, &sid, "echo", serde_json::json!({}), true).await.unwrap();
    match outcome.output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert_eq!(value["status"], "completed");
            assert_eq!(value["content"], "hello");
        }
        _ => panic!("expected json output"),
    }
}

#[tokio::test]
async fn a_tool_that_failed_is_a_command_that_failed() {
    let tools = Arc::new(FakeToolProvider::new().with_result(
        "boom",
        ToolResult::failed("it did not work"),
        ToolSideEffect::Pure,
    ));
    let gateway = FakeToolGateway::new(tools);
    let sid = SessionId::from_string("tool-cli-session");

    let outcome = run_tool(&gateway, &sid, "boom", serde_json::json!({}), false).await.unwrap();

    // The result is still the answer and still printed: a tool that ran and
    // came back bad was carried out, which is what `Err` would deny. What the
    // status adds is that nobody has to parse the answer to learn which kind
    // of answer it was.
    match &outcome.output {
        CliOutput::Text(t) => assert_eq!(t, "it did not work"),
        other => panic!("expected text output, got {other:?}"),
    }
    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
}

#[tokio::test]
async fn a_failed_tool_still_prints_its_result_as_json() {
    let tools = Arc::new(FakeToolProvider::new().with_result(
        "boom",
        ToolResult::failed("it did not work"),
        ToolSideEffect::Pure,
    ));
    let gateway = FakeToolGateway::new(tools);
    let sid = SessionId::from_string("tool-cli-session");

    let outcome = run_tool(&gateway, &sid, "boom", serde_json::json!({}), true).await.unwrap();

    // The machine-readable path keeps its shape. A caller that pipes stdout
    // gets the same object it always got, with the status it needs inside it,
    // rather than an empty stream and a message on stderr.
    match &outcome.output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert_eq!(value["status"], "failed");
            assert_eq!(value["content"], "it did not work");
        }
        other => panic!("expected json output, got {other:?}"),
    }
    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
}
