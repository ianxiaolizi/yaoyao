//! What a compaction summary is allowed to send.
//!
//! A summary may be routed to a faster model with a much smaller window than
//! the one that wrote the history. These tests pin that the summary request
//! fits that model's own budget — its input working set, completion cap, and a
//! reasoning depth it declared — rather than borrowing the main route's.

use super::*;
use agent_ports::model_router::RouteRequest;
use agent_protocol::model::ReasoningEffort;
use agent_protocol::profile::SummaryRouteBudget;

/// A router that sends summaries to the fast tier and everything else to the
/// main one.
struct SummarizeOnFast {
    main: ModelRoute,
    fast: ModelRoute,
}

impl ModelRouter for SummarizeOnFast {
    fn route(&self, request: &RouteRequest) -> ModelRoute {
        if request.task.as_str() == TaskClass::SUMMARIZE {
            self.fast.clone()
        } else {
            self.main.clone()
        }
    }
}

fn context_budget(input_working_set_tokens: u32) -> ResolvedContextBudget {
    ResolvedContextBudget {
        context_window_tokens: input_working_set_tokens * 8,
        input_working_set_tokens,
        completion_cap_tokens: 64,
        completion_cap_ceiling_tokens: 128,
        compaction_headroom_tokens: 0,
        compaction_trigger_tokens: input_working_set_tokens,
        retain_recent_tokens: 0,
    }
}

fn candidate_message(index: usize) -> ModelMessage {
    ModelMessage::user_for_turn(
        &agent_protocol::ids::TurnId::from_string(format!("t-summary-{index}")),
        format!("message {index}: {}", "lorem ipsum dolor sit amet ".repeat(20)),
    )
}

/// A candidate larger than the fast model's input working set is clipped on
/// whole-message boundaries to what that model can accept; the summary request
/// carries the fast route's cap and declared depth, never the main route's.
#[tokio::test]
async fn a_large_summary_candidate_is_clipped_to_the_fast_route_budget() {
    let candidates: Vec<ModelMessage> = (0..8).map(candidate_message).collect();
    let candidate = CompactionCandidate {
        messages: candidates.clone(),
        estimated_tokens: candidates.iter().map(estimate_message).sum(),
        retained_tokens: 0,
        budget_tokens: 10_000,
        trigger_tokens: 5_000,
        observed_input_tokens: None,
        scope: CompactionScope::Settled,
    };
    let context = Arc::new(
        FakeContextEngine::new(candidates.clone(), EventSeq::new(3)).with_compaction(candidate),
    );
    let model = Arc::new(FakeModelProvider::new(vec![
        FakeModelProvider::answer("summarized"),
        FakeModelProvider::answer("done"),
    ]));
    let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
    let main_route = ModelRoute::new(ModelTier::Main, "test-provider", "test-model");
    let fast_route = ModelRoute::new(ModelTier::Fast, "test-provider", "fast-model");
    let router = Arc::new(SummarizeOnFast { main: main_route.clone(), fast: fast_route.clone() });
    let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, router);

    // The fast window fits the fixed prompt and instruction plus exactly two
    // candidate messages; the third would exceed it.
    let fixed: usize = estimate_message(&ModelMessage::new(
        MessageRole::System,
        vec![ContentBlock::text(COMPACTION_PROMPT)],
    )) + estimate_message(&ModelMessage::new(
        MessageRole::User,
        vec![ContentBlock::text(COMPACTION_INSTRUCTION)],
    ));
    let candidate_cost = estimate_message(&candidates[0]);
    let fast_input = (fixed + 2 * candidate_cost + candidate_cost / 2) as u32;
    let mut turn = ctx();
    turn.state = Some(TurnStateFacts {
        budget: RunBudget {
            context: Some(context_budget(10_000)),
            reasoning_effort: Some(ReasoningEffort::High),
            supported_reasoning_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
            summary: Some(SummaryRouteBudget {
                context: context_budget(fast_input),
                reasoning_effort: Some(ReasoningEffort::Low),
                supported_reasoning_efforts: vec![ReasoningEffort::Low],
            }),
            ..RunBudget::default()
        },
        ..facts()
    });

    let outcome = run_turn(&loop_, &turn).await;
    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one summary request, then the step's own");
    let summary_request = &requests[0];

    assert_eq!(summary_request.model_route.as_ref().map(|r| r.tier), Some(ModelTier::Fast));
    let estimated: usize = summary_request.messages.iter().map(estimate_message).sum();
    assert!(
        estimated <= fast_input as usize,
        "the summary request must fit the fast input working set: {estimated} > {fast_input}"
    );
    assert_eq!(summary_request.budget.reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(summary_request.budget.max_tokens, Some(64));

    // The candidate messages that were sent are an oldest leading prefix: the
    // request is prompt, then a prefix of the candidate, then the instruction.
    let body = &summary_request.messages;
    assert_eq!(body.first().map(|m| m.role), Some(MessageRole::System));
    assert_eq!(body.last().map(|m| m.role), Some(MessageRole::User));
    let sent: Vec<&str> =
        body[1..body.len() - 1].iter().map(|message| message.message_uid.as_str()).collect();
    assert!(!sent.is_empty(), "the fast route must still select something");
    assert!(sent.len() < candidates.len(), "the oversized tail must be dropped");
    let expected: Vec<&str> =
        candidates[..sent.len()].iter().map(|message| message.message_uid.as_str()).collect();
    assert_eq!(sent, expected, "only the oldest leading prefix may be summarized");

    // The kept messages are a contiguous prefix of the candidate, and the
    // transcript drops exactly them in favor of the summary.
    let compacted = outcome
        .events
        .iter()
        .find(|event| event.event_type.as_str() == EventType::CONTEXT_COMPACTED)
        .expect("a compaction event");
    assert_eq!(compacted.payload["strategy"], "summary");
}
