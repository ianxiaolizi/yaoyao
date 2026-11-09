//! The capability stop: what counts as a missing capability, when the turn
//! ends on it, and what the calls it never dispatched are told.

use super::*;

/// A gateway that refuses the named tools as a capability gap and runs
/// everything else. It stands in for the isolation floor: what matters is
/// the layer the refusal states, not a policy engine.
struct CapabilityGateway {
    refusal: agent_protocol::tool::ToolPolicyRecord,
    refused: &'static [&'static str],
}

#[async_trait::async_trait]
impl ToolGateway for CapabilityGateway {
    async fn execute(
        &self,
        _session_id: &SessionId,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError> {
        if self.refused.contains(&invocation.tool_name.as_str()) {
            return Err(ToolGatewayError::PolicyDenied {
                tool: invocation.tool_name.clone(),
                reason: "capability process.spawn is not provided by this build's execution \
                         sandbox"
                    .to_string(),
                policy: Box::new(self.refusal.clone()),
            });
        }
        Ok(ToolResult::completed("ok"))
    }
}

/// Two calls in one response: the refused one, then one that must be
/// answered as never dispatched when the turn stops.
fn capability_calls() -> ModelResponse {
    ModelResponse {
        content_blocks: vec![
            ContentBlock::tool_use(
                "call_a",
                "shell.run",
                serde_json::json!({ "command": "ffmpeg -version" }),
            ),
            ContentBlock::tool_use(
                "call_b",
                "fs.write",
                serde_json::json!({ "path": "notes.txt", "content": "x" }),
            ),
        ],
        usage: None,
        finish_reason: agent_protocol::model::FinishReason::new(
            agent_protocol::model::FinishReason::TOOL_CALLS,
        ),
        provider_metadata: serde_json::Value::Null,
    }
}

fn sandbox_refusal() -> agent_protocol::tool::ToolPolicyRecord {
    use agent_protocol::permissions::{Capability, RefusalLayer};
    agent_protocol::tool::ToolPolicyRecord::denied(
        [Capability::new(Capability::PROCESS_SPAWN)],
        [(Capability::new(Capability::PROCESS_SPAWN), RefusalLayer::SandboxUnavailable)],
    )
}

/// One `sandbox_unavailable` refusal is a call the model can replace, not
/// a reason to end the turn: the turn continues and the model answers.
#[tokio::test]
async fn one_capability_refusal_does_not_stop_the_turn() {
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("shell.run", serde_json::json!({ "command": "ls" })),
        FakeModelProvider::tool_call("fs.read", serde_json::json!({ "path": "notes.txt" })),
        FakeModelProvider::answer("done"),
    ]));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(CapabilityGateway { refusal: sandbox_refusal(), refused: &["shell.run"] }),
        no_context(),
        one_model_router(),
    );

    let outcome = run_turn(&loop_, &ctx()).await;

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer, "{outcome:?}");
    assert_eq!(of_type(&outcome.events, EventType::TOOL_FAILED).len(), 1);
    assert!(of_type(&outcome.events, EventType::TOOL_NOT_STARTED).is_empty());
    assert_eq!(model.requests().len(), 3, "the turn kept going");
}

/// The second `sandbox_unavailable` refusal ends the turn: the backend
/// cannot provide the capability, so the loop stops instead of detouring
/// and hands the person the three choices.
#[tokio::test]
async fn two_capability_refusals_stop_the_turn_with_the_three_choices() {
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("shell.run", serde_json::json!({ "command": "ls" })),
        capability_calls(),
    ]));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(CapabilityGateway { refusal: sandbox_refusal(), refused: &["shell.run"] }),
        no_context(),
        one_model_router(),
    );

    let outcome = run_turn(&loop_, &ctx()).await;

    assert_eq!(outcome.stop_reason, StopReason::CapabilityBlocked);
    let text = outcome.final_content.expect("the stop carries the ask");
    assert!(text.contains("shell.run") && text.contains("process.spawn"), "{text}");
    assert!(text.contains("app-container"), "{text}");
    assert!(text.contains("1.") && text.contains("2.") && text.contains("3."), "{text}");
    // The call after the one that crossed the rule never ran, and it still
    // has an answer: a dropped call id would make the next request invalid.
    let not_started = of_type(&outcome.events, EventType::TOOL_NOT_STARTED);
    assert_eq!(not_started.len(), 1, "{:?}", outcome.events);
    assert_eq!(not_started[0].payload["provider_tool_call_id"], "call_b");
    assert!(
        not_started[0].payload["content"].as_str().unwrap().contains("missing capability twice"),
        "{:?}",
        not_started[0].payload
    );
    assert_eq!(of_type(&outcome.events, EventType::TOOL_FAILED).len(), 2);
    // The step the stop happened in is closed, like any other ending.
    assert_eq!(of_type(&outcome.events, EventType::STEP_COMPLETED).len(), 2);
    assert_eq!(model.requests().len(), 2, "the stop ends the turn where it is hit");
}

/// A policy-rule denial is a decision about a call, not a capability gap:
/// two of them do not end the turn.
#[tokio::test]
async fn repeated_rule_denials_are_not_a_capability_gap() {
    use agent_protocol::permissions::{Capability, RefusalLayer};
    use agent_protocol::tool::ToolPolicyRecord;

    let refusal = ToolPolicyRecord::denied(
        [Capability::new(Capability::PROCESS_SPAWN)],
        [(Capability::new(Capability::PROCESS_SPAWN), RefusalLayer::Rule)],
    );
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("shell.run", serde_json::json!({ "command": "ls" })),
        FakeModelProvider::tool_call("shell.run", serde_json::json!({ "command": "pwd" })),
        FakeModelProvider::answer("done"),
    ]));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(CapabilityGateway { refusal, refused: &["shell.run"] }),
        no_context(),
        one_model_router(),
    );

    let outcome = run_turn(&loop_, &ctx()).await;

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer, "{outcome:?}");
    assert_eq!(of_type(&outcome.events, EventType::TOOL_FAILED).len(), 2);
    assert!(of_type(&outcome.events, EventType::TOOL_NOT_STARTED).is_empty());
}
