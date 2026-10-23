//! Fidelity tests for the doubles that stand in for the other ports.
//!
//! Each is held to what the port promises a caller, and — where the port is
//! silent — to what the double's own documentation claims. `fakes.rs` covers
//! the event store's bookkeeping and the doctor check's results;
//! `fakes_artifacts.rs` covers the artifact store's address scheme.

use std::sync::Arc;

use agent_ports::context_engine::ContextEngine;
use agent_ports::decision::{
    DecisionAdvisor, DecisionError, DecisionMode, DecisionTrace, DecisionValue,
    FastDecisionProvider, QuestionPack,
};
use agent_ports::model_provider::{ContentBlock, ModelProvider, ModelProviderError, ModelResponse};
use agent_ports::model_router::{
    ModelCapability, ModelRoute, ModelRouter, ModelTier, RouteRequest, TaskClass,
};
use agent_ports::secret_provider::{SecretProvider, SecretProviderError};
use agent_ports::tool_gateway::{ToolGateway, ToolGatewayError};
use agent_ports::tool_provider::{ToolInvocation, ToolProvider, ToolProviderError, ToolSideEffect};
use agent_protocol::decision::{DecisionFamily, DecisionQuestion, DecisionState};
use agent_protocol::ids::{EventSeq, SessionId, StepId, TurnId};
use agent_protocol::model::{MessageRole, ModelBudget, ModelMessage, ModelRequest, ToolManifest};
use agent_protocol::permissions::SecretRef;
use agent_testkit::{
    FakeContextEngine, FakeDecisionAdvisor, FakeDecisionBehaviour, FakeDecisionProvider,
    FakeModelProvider, FakeToolGateway, FakeToolProvider, FixedModelRouter, FixedSecret,
    MissingSecret, NullContextEngine, RecordingSink, one_model_router,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// The inputs these doubles are handed. Built here rather than inline so each
// test says what it is about rather than how a request is spelled.
// ---------------------------------------------------------------------------

fn model_request(revision: u64) -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from_string("s1"),
        turn_id: TurnId::from_string("t1"),
        step_id: StepId::from_string("st1"),
        context_revision: revision,
        messages: vec![ModelMessage::text(MessageRole::User, "hello")],
        tool_manifest: ToolManifest::empty(),
        capabilities: Vec::new(),
        response_format: None,
        model_route: None,
        budget: ModelBudget::default(),
    }
}

fn route_request(task: &str) -> RouteRequest {
    RouteRequest {
        session_id: SessionId::from_string("s1"),
        turn_id: TurnId::from_string("t1"),
        task: TaskClass::new(task),
        context_revision: EventSeq::ZERO,
        message_count: 1,
        required_capabilities: Vec::<ModelCapability>::new(),
        budget: ModelBudget::default(),
    }
}

fn pack(question_ids: &[&str]) -> QuestionPack {
    let questions = question_ids
        .iter()
        .map(|id| DecisionQuestion::categorical(*id, "pick one", Vec::new()))
        .collect();
    QuestionPack::new(DecisionFamily::new(DecisionFamily::MODEL_ROUTE), "v1", questions)
}

fn decision_state() -> DecisionState {
    DecisionState {
        session_id: SessionId::from_string("s1"),
        turn_id: None,
        context_revision: 0,
        facts: serde_json::Map::new(),
    }
}

fn answer_text(response: &ModelResponse) -> String {
    match response.content_blocks.first() {
        Some(ContentBlock::Text { text }) => text.clone(),
        other => panic!("expected one text block, got {other:?}"),
    }
}

fn tool_call_ids(response: &ModelResponse) -> Vec<String> {
    response
        .content_blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ModelProvider
// ---------------------------------------------------------------------------

/// The script is spent in order, the last attempt repeats rather than the
/// provider running dry, and the requests it was handed are kept — that record
/// is the model's own view of the conversation, which is what a history test
/// has to compare against.
#[tokio::test]
async fn the_model_fake_replays_its_script_in_order_and_keeps_the_requests() {
    let provider = FakeModelProvider::new(vec![
        FakeModelProvider::answer("first"),
        FakeModelProvider::answer("second"),
    ]);
    let sink = RecordingSink::new();

    let first = provider.stream(&model_request(1), &sink).await.unwrap();
    let second = provider.stream(&model_request(2), &sink).await.unwrap();
    let third = provider.stream(&model_request(3), &sink).await.unwrap();

    assert_eq!(answer_text(&first), "first");
    assert_eq!(answer_text(&second), "second");
    assert_eq!(answer_text(&third), "second", "the last attempt repeats");

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].turn_id, TurnId::from_string("t1"));
    assert_eq!(requests[2].context_revision, 3);
}

/// Two scripted calls arrive with no provider call id, so the fake mints one —
/// and it has to mint a different one each time, or a replay would collapse two
/// tool calls into one.
#[tokio::test]
async fn the_model_fake_mints_a_distinct_call_id_for_each_scripted_tool_call() {
    let provider = FakeModelProvider::new(vec![
        FakeModelProvider::tool_call("fs.read", json!({ "path": "a" })),
        FakeModelProvider::tool_call("fs.read", json!({ "path": "b" })),
    ]);
    let sink = RecordingSink::new();

    let first = provider.stream(&model_request(0), &sink).await.unwrap();
    let second = provider.stream(&model_request(0), &sink).await.unwrap();

    let ids = [&tool_call_ids(&first)[..], &tool_call_ids(&second)[..]].concat();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_ne!(ids[0], ids[1]);
}

/// An interrupted stream never settles, and the count it reports is a claim
/// about the wire — so it is asserted against the deltas the sink really saw,
/// not against the number the fixture was built with.
#[tokio::test]
async fn an_interrupted_attempt_reports_the_deltas_it_actually_sent() {
    let provider = FakeModelProvider::interrupted(3);
    let sink = RecordingSink::new();

    let error = provider.stream(&model_request(0), &sink).await.unwrap_err();

    assert_eq!(sink.events().len(), 3);
    match error {
        ModelProviderError::StreamInterrupted { received_deltas, .. } => assert_eq!(
            received_deltas, 3,
            "the count has to be the deltas that went out, not the ones that were scripted"
        ),
        other => panic!("expected a stream interruption, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ModelRouter
// ---------------------------------------------------------------------------

#[test]
fn the_fixed_router_answers_one_route_and_records_every_request() {
    let route = ModelRoute::new(ModelTier::Main, "test-provider", "test-model");
    let router = FixedModelRouter::new(route);

    assert_eq!(router.route(&route_request("interactive")).model, "test-model");
    assert_eq!(router.route(&route_request("summarize")).model, "test-model");

    let requests = router.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].task.as_str(), "summarize");
}

/// The one-model router exists for tests that are not about routing, so the
/// only thing to hold it to is that its answer does not move with the question.
#[test]
fn every_request_gets_the_same_route_from_the_one_model_router() {
    let router = one_model_router();
    let first = router.route(&route_request("interactive"));
    let second = router.route(&route_request("summarize"));

    assert_eq!((first.provider, first.model), (second.provider, second.model));
}

// ---------------------------------------------------------------------------
// FastDecisionProvider and DecisionAdvisor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_decision_fake_answers_only_what_the_pack_asks_and_records_the_call() {
    let provider = FakeDecisionProvider::new(
        "backend-a",
        FakeDecisionBehaviour::Answer(vec![
            ("q1".to_string(), DecisionValue::Categorical { choice: "a".to_string() }),
            ("q9".to_string(), DecisionValue::Categorical { choice: "z".to_string() }),
        ]),
    );
    let requested = pack(&["q1"]);

    let result = provider.decide(&requested, &decision_state()).await.unwrap();

    assert_eq!(result.pack_id, requested.pack_id);
    assert!(result.answer_for("q1").is_some());
    assert!(result.answer_for("q9").is_none(), "a question the pack never asked is not answered");

    assert_eq!(provider.call_count(), 1);
    assert_eq!(provider.calls()[0].0.pack_id, requested.pack_id);
}

/// Abstaining and failing are different answers, and a caller that had to
/// read both as "no answer" could not tell a backend with no opinion from one
/// that fell over.
#[tokio::test]
async fn abstaining_and_failing_are_not_the_same_answer() {
    let abstaining = FakeDecisionProvider::new("a", FakeDecisionBehaviour::Abstain);
    let failing = FakeDecisionProvider::new(
        "b",
        FakeDecisionBehaviour::Fail(DecisionError::Unavailable("backend is down".to_string())),
    );
    let requested = pack(&["q1"]);

    let result = abstaining.decide(&requested, &decision_state()).await.unwrap();
    assert!(result.abstained);
    assert!(result.answers.is_empty());

    assert!(matches!(
        failing.decide(&requested, &decision_state()).await,
        Err(DecisionError::Unavailable(_))
    ));
}

/// A cached answer for a different pack is the stale-result case, and what
/// makes it detectable is that the result names the pack it was built for.
#[tokio::test]
async fn an_answer_for_another_pack_comes_back_naming_that_pack() {
    let provider = FakeDecisionProvider::new(
        "stale",
        FakeDecisionBehaviour::AnswerForAnotherPack(vec![(
            "q1".to_string(),
            DecisionValue::Categorical { choice: "a".to_string() },
        )]),
    );
    let requested = pack(&["q1"]);

    let result = provider.decide(&requested, &decision_state()).await.unwrap();

    assert_ne!(result.pack_id, requested.pack_id, "the caller has to be able to tell");
}

#[test]
fn the_decision_fake_reports_the_identity_it_was_given() {
    let provider =
        FakeDecisionProvider::answering("backend-a", "q1", DecisionValue::Binary { value: true })
            .with_revision("rev-7");

    assert_eq!(provider.provider_id(), "backend-a");
    assert_eq!(provider.revision().as_deref(), Some("rev-7"));
}

#[tokio::test]
async fn the_advisor_fake_hands_back_its_canned_trace_and_records_what_it_was_shown() {
    let requested = pack(&["q1"]);
    let canned = DecisionTrace::new(&requested, DecisionMode::Advisory);
    let advisor = FakeDecisionAdvisor::returning(canned.clone());
    let route = ModelRoute::new(ModelTier::Main, "chosen-provider", "chosen-model");

    let returned = advisor.advise_model_route(&route_request("interactive"), &route).await;

    assert_eq!(returned.trace_id, canned.trace_id);
    assert_eq!(returned.pack_id, requested.pack_id);
    assert_eq!(returned.mode, DecisionMode::Advisory);

    assert_eq!(advisor.call_count(), 1);
    let (recorded, chosen) = &advisor.calls()[0];
    assert_eq!(recorded.task.as_str(), "interactive");
    assert_eq!(chosen.model, "chosen-model");
}

#[tokio::test]
async fn an_advisor_with_no_canned_trace_answers_with_an_empty_shadow_one() {
    let advisor = FakeDecisionAdvisor::default();
    let route = ModelRoute::new(ModelTier::Main, "p", "m");

    let returned = advisor.advise_model_route(&route_request("interactive"), &route).await;

    assert_eq!(returned.mode, DecisionMode::Shadow);
    assert!(returned.question_ids.is_empty());
    assert!(returned.chosen.is_none(), "no decision, rather than a decision nobody made");
}

// ---------------------------------------------------------------------------
// SecretProvider
// ---------------------------------------------------------------------------

#[test]
fn the_fixed_secret_resolves_every_reference_to_its_value() {
    let secret = FixedSecret::new("s3cr3t");

    for name in ["API_KEY", "ANY_OTHER"] {
        let reference = SecretRef { source: "env".to_string(), name: name.to_string() };
        assert_eq!(secret.resolve(&reference).unwrap(), "s3cr3t");
    }
}

#[test]
fn the_missing_secret_refuses_with_the_reference_it_was_given() {
    let reference = SecretRef { source: "env".to_string(), name: "API_KEY".to_string() };

    match MissingSecret.resolve(&reference) {
        Err(SecretProviderError::NotFound { provider, name }) => {
            assert_eq!(provider, "env");
            assert_eq!(name, "API_KEY");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ContextEngine
// ---------------------------------------------------------------------------

/// An empty history at position zero is what a session looks like at its first
/// turn — which is why loop tests that care about one turn use this rather than
/// a test-only path through the loop.
#[tokio::test]
async fn the_null_context_engine_derives_an_empty_history_at_the_start_of_the_log() {
    let engine = NullContextEngine::new();

    let derived = engine.derive(&SessionId::from_string("s1")).await.unwrap();

    assert!(derived.messages.is_empty());
    assert_eq!(derived.revision, EventSeq::ZERO);
}

#[tokio::test]
async fn the_fake_context_engine_serves_its_history_to_any_session() {
    let engine = FakeContextEngine::new(
        vec![ModelMessage::text(MessageRole::User, "earlier")],
        EventSeq::new(9),
    );

    for session in ["s1", "another"] {
        let derived = engine.derive(&SessionId::from_string(session)).await.unwrap();
        assert_eq!(derived.revision, EventSeq::new(9));
        assert_eq!(derived.messages.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// ToolProvider and ToolGateway
// ---------------------------------------------------------------------------

#[test]
fn the_tool_fake_declares_its_tools_in_name_order_with_their_side_effect() {
    let provider = FakeToolProvider::new()
        .with_tool("fs.write", "wrote", ToolSideEffect::Write)
        .with_tool("fs.read", "read", ToolSideEffect::Read);

    let definitions = provider.definitions();

    let names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["fs.read", "fs.write"], "a manifest built from these must be stable");
    assert_eq!(definitions[0].side_effect, ToolSideEffect::Read);
    assert_eq!(definitions[1].side_effect, ToolSideEffect::Write);
    assert_eq!(provider.side_effect("fs.read"), Some(ToolSideEffect::Read));
    assert_eq!(provider.side_effect("nobody"), None);
}

#[tokio::test]
async fn an_unknown_tool_is_refused_with_what_the_provider_offers() {
    let provider = FakeToolProvider::new().with_tool("fs.read", "read", ToolSideEffect::Read);

    let error =
        provider.invoke(&ToolInvocation::proposed("fs.write", json!({}))).await.unwrap_err();

    match error {
        ToolProviderError::UnknownTool { tool, available } => {
            assert_eq!(tool, "fs.write");
            assert_eq!(available, ["fs.read".to_string()]);
        }
        other => panic!("expected UnknownTool, got {other:?}"),
    }
}

/// Every invocation is in the record, the refused one included. The map answers
/// each call with the same frozen result, so without this a test cannot tell
/// two calls from one — and a record that skipped the refusal would answer
/// "what did the tool accept", which is a different question.
#[tokio::test]
async fn the_tool_fake_records_every_invocation_including_the_one_it_refused() {
    let provider = FakeToolProvider::new().with_tool("fs.read", "read", ToolSideEffect::Read);

    provider.invoke(&ToolInvocation::proposed("fs.read", json!({ "path": "a" }))).await.unwrap();
    provider.invoke(&ToolInvocation::proposed("fs.read", json!({ "path": "b" }))).await.unwrap();
    let _ = provider.invoke(&ToolInvocation::proposed("fs.write", json!({}))).await;

    let calls = provider.invocations();

    assert_eq!(provider.call_count(), 3);
    assert_eq!(calls[0].arguments, json!({ "path": "a" }));
    assert_eq!(calls[1].arguments, json!({ "path": "b" }));
    assert_eq!(calls[2].tool_name, "fs.write", "a call it turned away was still handed over");
}

#[tokio::test]
async fn the_gateway_fake_hands_the_invocation_to_the_provider_it_wraps() {
    let provider =
        Arc::new(FakeToolProvider::new().with_tool("fs.read", "the output", ToolSideEffect::Read));
    let gateway = FakeToolGateway::new(provider.clone());

    let session = SessionId::from_string("s1");
    let result =
        gateway.execute(&session, &ToolInvocation::proposed("fs.read", json!({}))).await.unwrap();

    assert_eq!(result.content, "the output");
    assert_eq!(gateway.definitions().len(), 1, "the surface passes through with the calls");
    assert_eq!(provider.invocations().len(), 1);
}

/// A held call is a call the tool never saw: the gateway is what fails, and it
/// fails before the provider is reached.
#[tokio::test]
async fn a_gateway_that_requires_approval_never_reaches_the_provider() {
    let provider =
        Arc::new(FakeToolProvider::new().with_tool("fs.write", "wrote", ToolSideEffect::Write));
    let gateway = FakeToolGateway::requiring_approval(provider.clone(), "approval-1");

    let session = SessionId::from_string("s1");
    let error = gateway
        .execute(&session, &ToolInvocation::proposed("fs.write", json!({})))
        .await
        .unwrap_err();

    match error {
        ToolGatewayError::ApprovalPending { tool, approval_id } => {
            assert_eq!(tool, "fs.write");
            assert_eq!(approval_id, "approval-1");
        }
        other => panic!("expected ApprovalPending, got {other:?}"),
    }
    assert_eq!(provider.call_count(), 0);
}
