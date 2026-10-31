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
use agent_ports::agent_loop::{Journal, StopReason, TurnContext, TurnInputKind, TurnStateFacts};
use agent_protocol::events::EventDraft;
use agent_protocol::events::EventType;
use agent_protocol::model::{FinishReason, ModelResponse, ReasoningEffort, Usage};
use agent_protocol::profile::{ResolvedContextBudget, RunBudget};
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

// ---------------------------------------------------------------------------
// The one recovery from a reasoning-only truncation
// ---------------------------------------------------------------------------

/// A response that spent `tokens` entirely on reasoning: no content at all,
/// `finish_reason: length`, and usage proving the cap went to thinking.
fn reasoning_only(tokens: u64) -> ModelResponse {
    ModelResponse {
        content_blocks: Vec::new(),
        usage: Some(Usage {
            input_tokens: Some(5_010),
            output_tokens: Some(tokens),
            total_tokens: Some(5_010 + tokens),
            reasoning_tokens: Some(tokens),
            ..Usage::default()
        }),
        finish_reason: FinishReason::new(FinishReason::LENGTH),
        provider_metadata: serde_json::Value::Null,
    }
}

fn budget(cap: u32, ceiling: u32) -> ResolvedContextBudget {
    ResolvedContextBudget {
        completion_cap_tokens: cap,
        completion_cap_ceiling_tokens: ceiling,
        ..ResolvedContextBudget::default()
    }
}

/// A first turn whose facts carry `budget`, driven over `model`; the provider's
/// own record of the requests comes back with the outcome.
async fn run_with_budget(
    model: FakeModelProvider,
    budget: RunBudget,
) -> (agent_testkit::DrivenTurn, Vec<agent_protocol::model::ModelRequest>) {
    let model = Arc::new(model);
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    );
    let mut ctx = ctx();
    ctx.state = Some(TurnStateFacts {
        started_at: chrono::DateTime::parse_from_rfc3339("2026-10-31T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        queued_inputs: 0,
        input_kind: TurnInputKind::User,
        previous_turn: None,
        resumed: None,
        max_steps: 8,
        budget,
        completion: None,
    });
    let outcome = drive_turn(&loop_, &ctx, 8).await.expect("the turn ran");
    let requests = model.requests();
    (outcome, requests)
}

fn prepared_requests(outcome: &agent_testkit::DrivenTurn) -> Vec<&EventDraft> {
    outcome
        .events
        .iter()
        .filter(|event| event.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
        .collect()
}

#[tokio::test]
async fn the_loop_retries_once_before_treating_output_as_final() {
    let run_budget = RunBudget {
        max_output_tokens: Some(262_144),
        context: Some(budget(262_144, 393_216)),
        ..RunBudget::default()
    };
    let (outcome, requests) = run_with_budget(
        FakeModelProvider::new(vec![reasoning_only(8_000), FakeModelProvider::answer("done")]),
        run_budget,
    )
    .await;

    assert_eq!(requests.len(), 2, "one recovery, then the answer");
    assert_eq!(requests[1].budget.max_tokens, Some(393_216), "the retry asks for more room");
    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert_eq!(outcome.final_content.as_deref(), Some("done"));

    let prepared = prepared_requests(&outcome);
    assert_eq!(prepared.len(), 2);
    assert_eq!(prepared[1].payload["output_recovery"], "reasoning_only");
    assert!(prepared[0].payload.get("output_recovery").is_none(), "the first ask is ordinary");

    // Only the settled response is durable: the truncated one is discarded
    // rather than recorded as an answer the turn kept.
    let responses: Vec<_> = outcome
        .events
        .iter()
        .filter(|event| event.event_type.as_str() == EventType::MODEL_RESPONSE_COMPLETED)
        .collect();
    assert_eq!(responses.len(), 1);
}

#[tokio::test]
async fn a_second_reasoning_only_truncation_ends_as_max_output_tokens() {
    let run_budget = RunBudget {
        max_output_tokens: Some(262_144),
        context: Some(budget(262_144, 393_216)),
        ..RunBudget::default()
    };
    let (outcome, requests) = run_with_budget(
        FakeModelProvider::new(vec![reasoning_only(8_000), reasoning_only(16_000)]),
        run_budget,
    )
    .await;

    assert_eq!(requests.len(), 2, "recovery is one attempt, never a loop");
    assert_eq!(outcome.stop_reason, StopReason::MaxOutputTokens);
    let verdict = verdict_of(&outcome.events);
    assert_eq!(verdict.rule, "verdict/output-truncated");
    assert!(
        verdict.claim.starts_with("reasoning budget exhausted"),
        "the verdict says where the budget went: {}",
        verdict.claim
    );
}

#[tokio::test]
async fn at_the_ceiling_the_retry_asks_for_less_depth_instead() {
    let run_budget = RunBudget {
        max_output_tokens: Some(393_216),
        reasoning_effort: Some(ReasoningEffort::Max),
        supported_reasoning_efforts: vec![
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ],
        context: Some(budget(393_216, 393_216)),
        ..RunBudget::default()
    };
    let (outcome, requests) = run_with_budget(
        FakeModelProvider::new(vec![reasoning_only(393_216), FakeModelProvider::answer("done")]),
        run_budget,
    )
    .await;

    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].budget.max_tokens, Some(393_216), "there is no more room to give");
    assert_eq!(requests[1].budget.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
}
