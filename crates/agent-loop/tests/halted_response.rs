//! Responses the provider ended before the model finished.
//!
//! Two endings are that shape and they are read the same way: the output limit
//! cut the response off, or the content filter withheld it. Neither is an
//! answer, and a cut-off response's calls are not a plan — nothing in either is
//! dispatched (CONTRACT §7 #131, #132).
//!
//! Driven through the public loop port rather than the inline test module: the
//! claim under test is about what a turn does with a finished response, which is
//! visible from outside, and the inline module is already at its size ratchet.

use std::sync::Arc;

use agent_loop::DefaultAgentLoop;
use agent_ports::agent_loop::{Journal, StopReason, TurnContext};
use agent_protocol::events::EventDraft;
use agent_protocol::events::EventType;
use agent_protocol::reason::ReasonNode;
use agent_testkit::{
    FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine, RecordingJournal,
    drive_turn, one_model_router,
};

/// The same first turn the inline module's tests use: no derived history, no
/// clock, no capability facts.
fn ctx() -> TurnContext {
    TurnContext {
        session_id: agent_protocol::ids::SessionId::from_string("s"),
        turn_id: agent_protocol::ids::TurnId::new(),
        run_id: agent_protocol::ids::RunId::new(),
        user_input: serde_json::json!({"text": "hi"}),
        journal: Journal::new(Arc::new(RecordingJournal::new())),
        instructions: None,
        state: None,
        capability: None,
        plan: None,
        workers: Vec::new(),
        work: None,
        judgment: None,
        pending_failure: None,
    }
}

fn verdict_of(events: &[EventDraft]) -> ReasonNode {
    events
        .iter()
        .filter(|event| event.event_type.as_str() == EventType::REASON_RECORDED)
        .map(|event| {
            serde_json::from_value(event.payload["node"].clone())
                .expect("a recorded node reads back")
        })
        .find(|node: &ReasonNode| node.kind == agent_protocol::reason::ReasonKind::Verdict)
        .expect("a verdict names the stop")
}

async fn run(
    model: FakeModelProvider,
    tools: Arc<FakeToolProvider>,
) -> (agent_testkit::DrivenTurn, Arc<FakeToolProvider>) {
    let gateway = Arc::new(FakeToolGateway::new(tools.clone()));
    let loop_ = DefaultAgentLoop::new(
        Arc::new(model),
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    );
    let outcome = drive_turn(&loop_, &ctx(), 8).await.expect("the turn ran");
    (outcome, tools)
}

#[tokio::test]
async fn a_response_cut_off_at_the_output_limit_is_not_a_final_answer() {
    let (outcome, _) = run(
        FakeModelProvider::new(vec![FakeModelProvider::truncated("half a sent")]),
        Arc::new(FakeToolProvider::new()),
    )
    .await;

    assert_eq!(outcome.stop_reason, StopReason::MaxOutputTokens);
    assert_eq!(outcome.final_content.as_deref(), Some("half a sent"));
    assert_eq!(verdict_of(&outcome.events).rule, "verdict/output-truncated");
}

#[tokio::test]
async fn a_truncated_response_dispatches_none_of_its_calls() {
    let (outcome, tools) = run(
        FakeModelProvider::new(vec![FakeModelProvider::truncated_with_call(
            "fs.read",
            serde_json::json!({"path": "a"}),
        )]),
        Arc::new(FakeToolProvider::new().with_tool(
            "fs.read",
            "contents",
            agent_protocol::tool::ToolSideEffect::Pure,
        )),
    )
    .await;

    assert_eq!(outcome.stop_reason, StopReason::MaxOutputTokens);
    assert_eq!(tools.call_count(), 0, "a cut-off batch is not a plan: nothing ran");
    let not_started: Vec<_> = outcome
        .events
        .iter()
        .filter(|event| event.event_type.as_str() == EventType::TOOL_NOT_STARTED)
        .collect();
    assert_eq!(not_started.len(), 1, "the call still gets an answer the model can read");
    assert!(
        not_started[0].payload["content"]
            .as_str()
            .unwrap_or_default()
            .contains("output-token limit"),
        "the answer names the limit: {}",
        not_started[0].payload
    );
    let verdict = verdict_of(&outcome.events);
    assert_eq!(verdict.rule, "verdict/output-truncated");
    assert!(verdict.claim.contains("1 proposed call not dispatched"), "{}", verdict.claim);
}

#[tokio::test]
async fn a_content_filtered_response_is_not_an_answer() {
    let (outcome, tools) = run(
        FakeModelProvider::new(vec![FakeModelProvider::content_filtered("")]),
        Arc::new(FakeToolProvider::new()),
    )
    .await;

    assert_eq!(outcome.stop_reason, StopReason::ContentFiltered);
    assert_eq!(tools.call_count(), 0);
    assert_eq!(verdict_of(&outcome.events).rule, "verdict/content-filtered");
}
