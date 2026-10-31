//! What a turn does when the provider says the request was too large.
//!
//! A context-length rejection is the one model failure the loop can repair on
//! its own: the request was too big, and no repeat of the same bytes can help.
//! The recovery is local and deterministic — it makes no model call of its own —
//! and it is allowed exactly once. These tests drive it through the public loop
//! port, because the claim is about what a turn sends next, which is visible
//! from outside.

use std::collections::HashSet;
use std::sync::Arc;

use agent_loop::DefaultAgentLoop;
use agent_ports::agent_loop::{Journal, StopReason, TurnContext, TurnInputKind, TurnStateFacts};
use agent_ports::model_provider::{ContentBlock, ModelProviderError, ModelRequest};
use agent_protocol::events::{EventDraft, EventType};
use agent_protocol::ids::{MessageUid, TurnId};
use agent_protocol::model::{MessageRole, ModelMessage, estimate_message};
use agent_protocol::profile::{ResolvedContextBudget, RunBudget};
use agent_testkit::{
    DrivenTurn, FakeContextEngine, FakeModelProvider, FakeModelStep, FakeToolGateway,
    FakeToolProvider, drive_turn, one_model_router,
};

fn ctx() -> TurnContext {
    TurnContext {
        session_id: agent_protocol::ids::SessionId::from_string("s"),
        turn_id: TurnId::new(),
        run_id: agent_protocol::ids::RunId::new(),
        user_input: serde_json::json!({"text": "continue"}),
        journal: Journal::new(Arc::new(agent_testkit::RecordingJournal::new())),
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

fn budget(input_working_set_tokens: u32) -> ResolvedContextBudget {
    ResolvedContextBudget {
        context_window_tokens: input_working_set_tokens.saturating_mul(4),
        input_working_set_tokens,
        completion_cap_tokens: 1_024,
        completion_cap_ceiling_tokens: 2_048,
        compaction_headroom_tokens: 0,
        compaction_trigger_tokens: input_working_set_tokens,
        retain_recent_tokens: 0,
    }
}

fn facts(target: u32) -> TurnStateFacts {
    TurnStateFacts {
        started_at: chrono::DateTime::parse_from_rfc3339("2026-10-31T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        queued_inputs: 0,
        input_kind: TurnInputKind::User,
        previous_turn: None,
        resumed: None,
        max_steps: 8,
        budget: RunBudget { context: Some(budget(target)), ..RunBudget::default() },
        completion: None,
    }
}

fn text(role: MessageRole, uid: &str, body: &str) -> ModelMessage {
    ModelMessage {
        message_uid: MessageUid::from_string(uid),
        role,
        content: vec![ContentBlock::text(body)],
    }
}

fn call(uid: &str, call_id: &str) -> ModelMessage {
    ModelMessage {
        message_uid: MessageUid::from_string(uid),
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ToolUse {
            tool_call_id: call_id.to_string(),
            tool_name: "fs.read".to_string(),
            arguments: serde_json::json!({"path": "a"}),
        }],
    }
}

fn result(uid: &str, call_id: &str) -> ModelMessage {
    ModelMessage {
        message_uid: MessageUid::from_string(uid),
        role: MessageRole::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: call_id.to_string(),
            tool_name: "fs.read".to_string(),
            content: "contents".to_string(),
            is_error: false,
        }],
    }
}

/// `count` assistant messages of settled history, each large enough to matter.
fn long_history(count: usize) -> Vec<ModelMessage> {
    (0..count)
        .map(|index| {
            text(
                MessageRole::Assistant,
                &format!("msg/history/{index}"),
                &format!("history {index}: {}", "the earlier work went on. ".repeat(12)),
            )
        })
        .collect()
}

fn overflow() -> ModelProviderError {
    ModelProviderError::ContextOverflow {
        status: 413,
        body: "request entity too large".to_string(),
    }
}

/// Drive one turn over `history` and `steps`, with a context budget that forces
/// trimming, and return what the loop sent.
async fn run(
    history: Vec<ModelMessage>,
    steps: Vec<FakeModelStep>,
    target: u32,
) -> (DrivenTurn, Vec<ModelRequest>) {
    let model = Arc::new(FakeModelProvider::script(steps));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
        Arc::new(FakeContextEngine::new(history, agent_protocol::ids::EventSeq::new(3))),
        one_model_router(),
    );
    let mut ctx = ctx();
    ctx.state = Some(facts(target));
    let outcome = drive_turn(&loop_, &ctx, 8).await.expect("the turn ran");
    let requests = model.requests();
    (outcome, requests)
}

fn event_types<'a>(outcome: &'a DrivenTurn, kind: &str) -> Vec<&'a EventDraft> {
    outcome.events.iter().filter(|event| event.event_type.as_str() == kind).collect()
}

fn uids(request: &ModelRequest) -> Vec<&str> {
    request.messages.iter().map(|message| message.message_uid.as_str()).collect()
}

/// The message half of the request estimate. The loop's target also covers the
/// tool definitions, so a message-only count under the target is the weaker
/// claim and holds whenever the full one does.
fn message_tokens(request: &ModelRequest) -> usize {
    request.messages.iter().map(estimate_message).sum()
}

/// What the request's tool definitions cost, on the same estimator the loop
/// uses: the wire shape, without the manifest bookkeeping.
fn tool_tokens(request: &ModelRequest) -> usize {
    request
        .tool_manifest
        .entries
        .iter()
        .map(|entry| {
            let wire = serde_json::json!({
                "type": "function",
                "function": {
                    "name": entry.tool_id,
                    "description": entry.description.trim(),
                    "parameters": entry.input_schema,
                },
            });
            agent_protocol::model::estimate_text(&wire.to_string())
        })
        .sum()
}

#[tokio::test]
async fn context_overflow_compresses_and_retries_once() {
    let history = long_history(60);
    let history_tokens: usize = history.iter().map(estimate_message).sum();
    let target = u32::try_from(history_tokens / 2).expect("a target fits a u32");
    let (outcome, requests) = run(
        history.clone(),
        vec![
            FakeModelStep::failed(overflow()),
            FakeModelStep::settled(FakeModelProvider::answer("done")),
        ],
        target,
    )
    .await;

    assert_eq!(requests.len(), 2, "one recovery attempt, then the answer");
    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    assert_eq!(outcome.final_content.as_deref(), Some("done"));

    let history_uids: HashSet<&str> =
        history.iter().map(|message| message.message_uid.as_str()).collect();
    let first = &requests[0];
    let second = &requests[1];
    let second_uids: HashSet<&str> = uids(second).into_iter().collect();

    // Everything that was not old history — the run's frame, this turn's layers,
    // and the current user input — is still there.
    for uid in uids(first) {
        if !history_uids.contains(uid) {
            assert!(
                second_uids.contains(uid),
                "the frame and this turn's input must survive: {uid} was dropped"
            );
        }
    }
    // And some history did leave, or the retry would send the same bytes.
    assert!(
        second_uids.len() < uids(first).len(),
        "the retry must send less than the rejected request"
    );

    // The retry fits the conservative target.
    assert!(
        message_tokens(second) <= target as usize,
        "the retry exceeds the target: {} > {target}",
        message_tokens(second)
    );

    // One record of what was removed, and exactly one.
    let compacted = event_types(&outcome, EventType::CONTEXT_COMPACTED);
    assert_eq!(compacted.len(), 1, "one compaction record");
    assert_eq!(compacted[0].payload["strategy"], "overflow-trim");
    assert_eq!(compacted[0].payload["fallback"], "provider_context_overflow");
    assert_eq!(compacted[0].payload["scope"], "overflow");

    // The retry is a new attempt that says what it is, and the failed one is on
    // the record too.
    let prepared = event_types(&outcome, EventType::MODEL_REQUEST_PREPARED);
    assert_eq!(prepared.len(), 2);
    assert!(prepared[0].payload.get("context_recovery").is_none());
    assert_eq!(prepared[1].payload["context_recovery"], "max_safe_compression");
    assert_eq!(prepared[1].payload["original_error_kind"], "context_overflow");
    assert_ne!(
        prepared[0].payload["attempt_id"], prepared[1].payload["attempt_id"],
        "the retry is a new attempt"
    );

    let failed = event_types(&outcome, EventType::MODEL_ATTEMPT_FAILED);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].payload["error_kind"], "context_overflow");
}

#[tokio::test]
async fn a_second_context_overflow_stops_without_a_third_attempt() {
    let history = long_history(60);
    let history_tokens: usize = history.iter().map(estimate_message).sum();
    let target = u32::try_from(history_tokens / 2).expect("a target fits a u32");
    let (outcome, requests) = run(
        history,
        vec![
            FakeModelStep::failed(overflow()),
            FakeModelStep::failed(overflow()),
            FakeModelStep::settled(FakeModelProvider::answer("never reached")),
        ],
        target,
    )
    .await;

    assert_eq!(requests.len(), 2, "recovery is one attempt, never a loop");
    assert_eq!(outcome.stop_reason, StopReason::FatalError);
    assert_eq!(
        event_types(&outcome, EventType::MODEL_ATTEMPT_FAILED).len(),
        2,
        "both rejected attempts are recorded"
    );
    assert_eq!(event_types(&outcome, EventType::STEP_FAILED).len(), 1, "the step ends once");
}

#[tokio::test]
async fn emergency_compression_never_splits_a_tool_group_or_drops_user_text() {
    let history = vec![
        text(MessageRole::User, "msg/user/old-1", "the first request the user made, at length"),
        text(
            MessageRole::Assistant,
            "msg/old/1",
            &format!("older prose: {}", "and more of it. ".repeat(120)),
        ),
        call("msg/call/1", "call-1"),
        result("msg/result/1", "call-1"),
        text(
            MessageRole::Assistant,
            "msg/old/2",
            &format!("older prose: {}", "and more of it. ".repeat(120)),
        ),
        text(MessageRole::User, "msg/user/old-2", "a mid-history instruction from the user"),
        call("msg/call/2", "call-2"),
        result("msg/result/2", "call-2"),
        text(
            MessageRole::Assistant,
            "msg/old/3",
            &format!("older prose: {}", "and more of it. ".repeat(120)),
        ),
        text(
            MessageRole::Assistant,
            "msg/old/4",
            &format!("older prose: {}", "and more of it. ".repeat(120)),
        ),
    ];
    let history_tokens: usize = history.iter().map(estimate_message).sum();
    // What the request costs outside history — the frame, this turn's input, and
    // the tool definitions — measured once, so the target below leaves about
    // half the history and only trimming can get there.
    let (_, probe) = run(
        history.clone(),
        vec![FakeModelStep::settled(FakeModelProvider::answer("probe"))],
        u32::MAX,
    )
    .await;
    let fixed = message_tokens(&probe[0]) - history_tokens + tool_tokens(&probe[0]);
    let target = u32::try_from(fixed + history_tokens / 2).expect("a target fits a u32");
    let (outcome, requests) = run(
        history.clone(),
        vec![
            FakeModelStep::failed(overflow()),
            FakeModelStep::settled(FakeModelProvider::answer("done")),
        ],
        target,
    )
    .await;

    assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    let second = &requests[1];
    let second_text: HashSet<String> = second
        .messages
        .iter()
        .map(|message| {
            message.content.iter().filter_map(|block| block.as_text()).collect::<String>()
        })
        .collect();

    // Every user message survives verbatim, including the one that is history.
    for message in history.iter().filter(|message| message.role == MessageRole::User) {
        let body = message.content.iter().filter_map(|block| block.as_text()).collect::<String>();
        assert!(second_text.contains(&body), "user text must never be dropped: {body}");
    }

    // A tool call and its result share one fate.
    for id in ["call-1", "call-2"] {
        let call_present = second.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolUse { tool_call_id, .. } if tool_call_id == id)
            })
        });
        let result_present = second.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == id)
            })
        });
        assert_eq!(
            call_present, result_present,
            "{id}'s call and result must be kept or dropped together"
        );
    }

    assert!(
        uids(second).len() < uids(&requests[0]).len(),
        "the retry still has to be smaller than the rejected request"
    );
}

#[tokio::test]
async fn a_generic_http_400_does_not_enter_context_recovery() {
    let history = long_history(60);
    let history_tokens: usize = history.iter().map(estimate_message).sum();
    let target = u32::try_from(history_tokens / 2).expect("a target fits a u32");
    let (outcome, requests) = run(
        history,
        vec![
            FakeModelStep::failed(ModelProviderError::Http {
                status: 400,
                body: "malformed request".to_string(),
            }),
            FakeModelStep::settled(FakeModelProvider::answer("never reached")),
        ],
        target,
    )
    .await;

    assert_eq!(requests.len(), 1, "only a context overflow is recoverable");
    assert_eq!(outcome.stop_reason, StopReason::FatalError);
    assert!(
        event_types(&outcome, EventType::CONTEXT_COMPACTED).is_empty(),
        "nothing is compacted for an ordinary rejection"
    );
    assert_eq!(event_types(&outcome, EventType::MODEL_ATTEMPT_FAILED).len(), 1);
    assert_eq!(event_types(&outcome, EventType::STEP_FAILED).len(), 1);
}
