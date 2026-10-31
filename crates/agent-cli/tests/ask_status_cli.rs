//! CLI ask/status integration: drive a run and read session state through the
//! command runner, backed by fakes.

use std::sync::Arc;

use agent_cli::{CliOutput, Verdict, run_ask, run_status};
use agent_kernel::scheduler::Scheduler;
use agent_loop::DefaultAgentLoop;
use agent_ports::event_store::EventStore;
use agent_ports::model_provider::ModelResponse;
use agent_ports::tool_provider::ToolSideEffect;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, RunId, SchemaVersion, SessionId};
use agent_protocol::model::{ContentBlock, FinishReason, ModelRoute, ModelTier, Usage};
use agent_protocol::permissions::CapabilitySet;
use agent_protocol::profile::{ResolvedProfileSnapshot, RunBudget};
use agent_testkit::{
    FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine,
    one_model_router,
};
use chrono::Utc;

fn scheduler() -> (Scheduler, SessionId) {
    let (scheduler, _, sid) = scheduler_over_a_store();
    (scheduler, sid)
}

/// The same session, with the store handed back: seeding an event needs it.
fn scheduler_over_a_store() -> (Scheduler, Arc<FakeEventStore>, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("hello")]));
    let tools = Arc::new(FakeToolProvider::new());
    let gateway = Arc::new(FakeToolGateway::new(tools));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model,
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    ));
    let scheduler = Scheduler::new(store.clone(), loop_, 8);
    let sid = SessionId::from_string("cli-session");
    (scheduler, store, sid)
}

/// Open a run the way the scheduler opens one — a `run/started` with no terminal
/// event after it — so that the next `run` finds the session already busy.
///
/// Written out field by field rather than through a helper on purpose: an open
/// run *is* a `run/started` nothing closed, and a test that seeded it any other
/// way would be testing the seeding helper.
async fn seed_open_run(store: &FakeEventStore, sid: &SessionId) {
    let draft = EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(EventType::RUN_STARTED),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: sid.clone(),
        run_id: Some(RunId::new()),
        turn_id: None,
        step_id: None,
        actor: Actor::Scheduler,
        created_at: chrono::Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    };
    store.append(sid, EventSeq::ZERO, vec![draft]).await.unwrap();
}

#[tokio::test]
async fn ask_returns_the_model_answer() {
    let (scheduler, sid) = scheduler();
    let outcome = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();
    match outcome.output {
        CliOutput::Text(t) => assert_eq!(t, "hello"),
        _ => panic!("expected text output"),
    }
    assert_eq!(outcome.verdict, Verdict::Answered, "the turn ran and that is the answer");
}

#[tokio::test]
async fn ask_json_reports_final_content() {
    let (scheduler, sid) = scheduler();
    let outcome = run_ask(&scheduler, &sid, "hi", Utc::now(), true).await.unwrap();
    match outcome.output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert_eq!(value["final_content"], "hello");
            assert_eq!(value["stop_reason"], "final_answer");
        }
        _ => panic!("expected json output"),
    }
}

/// A `run` asked for while a run is already active did not run.
///
/// This is §3 C3.5's second case and 裁决 #6. The scheduler declines to start a
/// second run, so the input this command appended is still in the inbox
/// untouched — the command reached the scheduler and the run did not happen.
/// That is `DidNotRun` (exit `2`), not an answer: reporting it as one would tell
/// a script that only reads `$?` that its text had been run.
#[tokio::test]
async fn a_run_while_a_run_is_active_did_not_run() {
    let (scheduler, store, sid) = scheduler_over_a_store();
    seed_open_run(store.as_ref(), &sid).await;

    let outcome = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();

    assert_eq!(outcome.verdict, Verdict::DidNotRun);
    match outcome.output {
        CliOutput::Text(t) => assert!(t.contains("no run"), "unexpected answer: {t}"),
        _ => panic!("expected text output"),
    }
    // The untouched item is the evidence that nothing ran: the run that is
    // active is the seeded one, and the text is still waiting its turn.
    assert_eq!(scheduler.inbox().pending(&sid).await.unwrap().len(), 1);

    // The same answer in the other shape, checked at the site that used to be
    // the odd one out: `--json` is one pretty object whether the command came
    // back with a report or with a reason not to run.
    let json = run_ask(&scheduler, &sid, "hi", Utc::now(), true).await.unwrap();
    assert_eq!(json.verdict, Verdict::DidNotRun);
    let CliOutput::Json(doc) = json.output else { panic!("expected json output") };
    assert!(doc.as_str().starts_with("{\n"), "one pretty object: {doc}");
    let value: serde_json::Value = serde_json::from_str(doc.as_str()).unwrap();
    assert_eq!(value["reason"], "busy_or_no_input");
}

/// A truncated answer is not an answer: it exits `1`, keeps the text that
/// arrived, and says what it is.
///
/// 2026-10-30: a response cut off at the output limit was logged and printed as
/// an ordinary answer. The terminal now names the stop, and the command judges
/// it `AnsweredNo` so only `$?` is needed to tell half an answer from a whole
/// one (CONTRACT §7 #131).
#[tokio::test]
async fn a_truncated_answer_is_reported_as_not_answered() {
    let store = Arc::new(FakeEventStore::new());
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::truncated("half a sen")]));
    let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model,
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    ));
    let scheduler = Scheduler::new(store.clone(), loop_, 8);
    let sid = SessionId::from_string("truncated");

    let outcome = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();
    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
    let CliOutput::Text(text) = outcome.output else { panic!("expected text output") };
    assert!(text.starts_with("half a sen"), "the text that arrived is kept: {text}");
    assert!(text.contains("[truncated:"), "the notice says it is not whole: {text}");

    // The log says the same thing, in the form `metrics` and `why` read.
    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let end = events
        .iter()
        .find(|event| event.event.event_type.as_str() == EventType::TURN_COMPLETED)
        .expect("the turn ended");
    assert_eq!(end.event.payload["stop_reason"], "max_output_tokens");
    assert_eq!(end.event.payload["final_content"], "half a sen");
}

/// The provider's other ending that is not the model's choice gets the same
/// reading: exit `1`, and a notice that says rerunning will not help
/// (CONTRACT §7 #132).
#[tokio::test]
async fn a_content_filtered_answer_is_reported_as_not_answered() {
    let store = Arc::new(FakeEventStore::new());
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::content_filtered("")]));
    let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model,
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    ));
    let scheduler = Scheduler::new(store.clone(), loop_, 8);
    let sid = SessionId::from_string("filtered");

    let outcome = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();
    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
    let CliOutput::Text(text) = outcome.output else { panic!("expected text output") };
    assert!(text.contains("[refused:"), "the notice says the provider withheld it: {text}");
    assert!(text.contains("content filter"), "the notice names the ending that stopped it: {text}");

    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let end = events
        .iter()
        .find(|event| event.event.event_type.as_str() == EventType::TURN_COMPLETED)
        .expect("the turn ended");
    assert_eq!(end.event.payload["stop_reason"], "content_filtered");
}

#[tokio::test]
async fn status_reports_closed_state_after_an_ask() {
    let (scheduler, sid) = scheduler();
    let _ = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();

    let output = run_status(&scheduler, &sid, false).await.unwrap();
    match output {
        CliOutput::Text(t) => {
            assert!(t.contains("open run:  none"), "unexpected status: {t}");
            assert!(t.contains("pending:   0"), "unexpected status: {t}");
        }
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn status_json_reports_pending_count() {
    let (scheduler, sid) = scheduler();
    // Submit without running: the item stays pending.
    scheduler
        .inbox()
        .append(
            &sid,
            agent_protocol::inbox::InboxKind::NextTurn,
            serde_json::json!({"text": "queued"}),
            0,
            agent_protocol::events::Actor::User,
        )
        .await
        .unwrap();

    let output = run_status(&scheduler, &sid, true).await.unwrap();
    match output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert_eq!(value["pending"], 1);
        }
        _ => panic!("expected json output"),
    }
}

/// A run that spent its step budget did not finish, and the command says so:
/// `AnsweredNo` (exit `1`), with a line a person can act on.
///
/// Before this, an exhausted step budget printed `[turn ended: MaxSteps]` and
/// exited `0` — a silent stop a script could not tell from an answer.
#[tokio::test]
async fn a_run_that_spent_its_steps_is_reported_as_not_answered() {
    let store = Arc::new(FakeEventStore::new());
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::tool_call(
        "echo",
        serde_json::json!({"s": "hi"}),
    )]));
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    let gateway = Arc::new(FakeToolGateway::new(tools));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model,
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    ));
    // One step: the turn ends at the next boundary with the budget exhausted.
    let scheduler = Scheduler::new(store.clone(), loop_, 1);
    let sid = SessionId::from_string("spent-steps");

    let outcome = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();
    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
    let CliOutput::Text(text) = outcome.output else { panic!("expected text output") };
    assert!(text.contains("incomplete: max_steps"), "the notice names the limit: {text}");
    assert!(text.contains("step(s)"), "the notice says how far it got: {text}");

    // The terminal says the same in the shape a fold reads, including the spend
    // beside the step count.
    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let end = events
        .iter()
        .find(|event| event.event.event_type.as_str() == EventType::TURN_COMPLETED)
        .expect("the turn ended");
    assert_eq!(end.event.payload["stop_reason"], "max_steps");
    assert_eq!(end.event.payload["spent"]["steps"], 1);
    assert!(end.event.payload["spent"].get("input_tokens").is_some());
}

/// A run that spent its whole token budget is a limit stop like any other: the
/// command exits `1` and the line names the limit, the spend and the one change
/// that would let the work continue.
///
/// The budget here is one token, so the first response — which reports what it
/// cost — crosses it and the step boundary after it ends the run. What matters
/// is that the counting is the provider's own number, carried all the way to
/// the terminal payload and the summary line, rather than a count the CLI has
/// to guess.
#[tokio::test]
async fn a_run_that_spent_its_token_budget_is_reported_as_not_answered() {
    let store = Arc::new(FakeEventStore::new());
    let model = Arc::new(FakeModelProvider::new(vec![priced_tool_call()]));
    let tools = Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
    let gateway = Arc::new(FakeToolGateway::new(tools));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model,
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    ));
    let scheduler = Scheduler::new(store.clone(), loop_, 8).with_profile_snapshot(
        snapshot_with_budget(RunBudget { max_total_tokens: Some(1), ..RunBudget::default() }),
    );
    let sid = SessionId::from_string("spent-tokens");

    let outcome = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();
    assert_eq!(outcome.verdict, Verdict::AnsweredNo);
    let CliOutput::Text(text) = outcome.output else { panic!("expected text output") };
    assert!(
        text.contains("incomplete: token_budget_exceeded"),
        "the notice names the limit: {text}"
    );
    assert!(
        text.contains("spent 100 input + 20 output tokens"),
        "the spend is the provider's: {text}"
    );
    assert!(text.contains("raise `budget.max_total_tokens`"), "the notice is actionable: {text}");

    // The same facts in the log, where a fold reads them.
    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let end = events
        .iter()
        .find(|event| event.event.event_type.as_str() == EventType::TURN_COMPLETED)
        .expect("the turn ended");
    assert_eq!(end.event.payload["stop_reason"], "token_budget_exceeded");
    assert_eq!(end.event.payload["spent"]["input_tokens"], 100);
    assert_eq!(end.event.payload["spent"]["output_tokens"], 20);
    assert_eq!(end.event.payload["spent"]["estimated"], false);
}

/// A tool-call response that reports its own usage, so the run has a next step
/// to reach and a number to count.
fn priced_tool_call() -> ModelResponse {
    ModelResponse {
        content_blocks: vec![ContentBlock::tool_use(
            String::new(),
            "echo",
            serde_json::json!({"s": "hi"}),
        )],
        usage: Some(Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            total_tokens: Some(120),
            cache_hit_tokens: Some(0),
            ..Usage::default()
        }),
        finish_reason: FinishReason::new(FinishReason::TOOL_CALLS),
        provider_metadata: serde_json::Value::Null,
    }
}

/// A profile snapshot that carries only the budget this test is about.
fn snapshot_with_budget(budget: RunBudget) -> ResolvedProfileSnapshot {
    ResolvedProfileSnapshot {
        profile_id: "long".to_string(),
        config_hash: String::new(),
        plugin_versions: std::collections::BTreeMap::new(),
        policy_version: "deadbeef".to_string(),
        decision_backends: Vec::new(),
        model_route: ModelRoute::new(ModelTier::Main, "fake", "echo"),
        model_routes: std::collections::BTreeMap::new(),
        memory_enabled: false,
        capability_set: CapabilitySet::new(),
        budget,
    }
    .sealed()
}
