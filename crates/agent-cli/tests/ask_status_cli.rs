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
use agent_protocol::inbox::InboxKind;
use agent_protocol::intent::IntentStatus;
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
    let (scheduler, store, _, sid) = scheduler_over_a_store_and_model();
    (scheduler, store, sid)
}

/// The same session again, with the provider handed back too: one test counts
/// what reached the model, which is how "a control word costs nothing" is shown
/// rather than asserted.
fn scheduler_over_a_store_and_model()
-> (Scheduler, Arc<FakeEventStore>, Arc<FakeModelProvider>, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("hello")]));
    let tools = Arc::new(FakeToolProvider::new());
    let gateway = Arc::new(FakeToolGateway::new(tools));
    let loop_ = Arc::new(DefaultAgentLoop::new(
        model.clone(),
        gateway,
        Arc::new(NullContextEngine::new()),
        one_model_router(),
    ));
    let scheduler = Scheduler::new(store.clone(), loop_, 8);
    let sid = SessionId::from_string("cli-session");
    (scheduler, store, model, sid)
}

/// Open a run the way the scheduler opens one — a `run/started` with no terminal
/// event after it — so that the next `run` finds the session already busy.
///
/// Written out field by field rather than through a helper on purpose: an open
/// run *is* a `run/started` nothing closed, and a test that seeded it any other
/// way would be testing the seeding helper. The run id comes back so a test can
/// close the same run later.
async fn seed_open_run(store: &FakeEventStore, sid: &SessionId) -> RunId {
    let run_id = RunId::new();
    append(store, sid, draft(EventType::RUN_STARTED, sid, &run_id)).await;
    run_id
}

/// End the seeded run, so the queue can drain the way it would after a real run.
async fn close_run(store: &FakeEventStore, sid: &SessionId, run_id: &RunId) {
    append(store, sid, draft(EventType::RUN_COMPLETED, sid, run_id)).await;
}

/// Append one event after whatever the session already holds — the seed has to
/// join the log the commands wrote, not fight it for sequence number zero.
async fn append(store: &FakeEventStore, sid: &SessionId, draft: EventDraft) {
    let events = store.read_stream(sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let next = events.last().map_or(EventSeq::ZERO, |event| event.seq.next());
    store.append(sid, next, vec![draft]).await.unwrap();
}

fn draft(event_type: &'static str, sid: &SessionId, run_id: &RunId) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: sid.clone(),
        run_id: Some(run_id.clone()),
        turn_id: None,
        step_id: None,
        actor: Actor::Scheduler,
        created_at: chrono::Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    }
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

#[tokio::test]
async fn status_json_reports_the_latest_intent() {
    let (scheduler, sid) = scheduler();
    let _ = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();

    let outcome = run_status(&scheduler, &sid, true).await.unwrap();
    match outcome {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert!(
                value["latest_intent_id"].is_string(),
                "the instruction that was answered is identifiable: {value}"
            );
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

/// A stop is the rule layer's answer, not a model's: the queued instruction is
/// withdrawn, an interrupt is queued for the run in flight, and the evidence
/// from the turn that already ran stays exactly where it is.
///
/// This is the cheapest of the three stage-A thresholds and the easiest one to
/// regress: the moment a control word is "understood" by a provider, a stop
/// costs a request and can be talked out of.
#[tokio::test]
async fn a_stop_withdraws_the_queue_without_asking_a_model() {
    let (scheduler, store, model, sid) = scheduler_over_a_store_and_model();

    // A turn that really ran, so there is evidence a stop must leave alone.
    let first = run_ask(&scheduler, &sid, "跑一下测试", Utc::now(), false).await.unwrap();
    assert_eq!(first.verdict, Verdict::Answered);
    assert_eq!(model.requests().len(), 1);

    // Then a run in flight with an instruction waiting behind it.
    let run_id = seed_open_run(&store, &sid).await;
    let queued = run_ask(&scheduler, &sid, "重构日志模块", Utc::now(), false).await.unwrap();
    assert_eq!(queued.verdict, Verdict::DidNotRun, "the session was busy");
    assert_eq!(scheduler.inbox().pending(&sid).await.unwrap().len(), 1);

    let outcome = run_ask(&scheduler, &sid, "停止", Utc::now(), false).await.unwrap();

    assert_eq!(model.requests().len(), 1, "a control word must not reach a provider");
    let pending = scheduler.inbox().pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1, "the queued instruction is gone; only the interrupt is left");
    assert_eq!(pending[0].kind, InboxKind::Interrupt);
    assert_eq!(pending[0].payload["reason"], "user_stop");

    assert_eq!(outcome.verdict, Verdict::Answered, "a stop that withdrew something did something");
    let CliOutput::Text(text) = outcome.output else { panic!("expected text output") };
    assert!(text.contains("withdrew 1 queued instruction"), "the stop says what it did: {text}");
    assert!(text.contains("next step boundary"), "and what happens to the run: {text}");

    close_run(&store, &sid, &run_id).await;
    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let completed = events
        .iter()
        .filter(|event| event.event.event_type.as_str() == EventType::TURN_COMPLETED)
        .count();
    assert_eq!(completed, 1, "the answer that was already given is still on the record");
}

/// A stop with nothing in flight and nothing queued did nothing, and says so
/// through the exit code as well as the line.
#[tokio::test]
async fn a_stop_with_nothing_to_stop_did_not_run() {
    let (scheduler, sid) = scheduler();

    let outcome = run_ask(&scheduler, &sid, "停止", Utc::now(), false).await.unwrap();

    assert_eq!(outcome.verdict, Verdict::DidNotRun);
    let CliOutput::Text(text) = outcome.output else { panic!("expected text output") };
    assert!(text.contains("nothing to stop"), "{text}");
}

/// Two instructions queued behind one run: the newer one displaces the older,
/// and the older is on the record as `superseded` rather than being run.
///
/// The run is closed only after both were queued, so the queue really holds two
/// instructions at the moment the second arrives — the situation the 2026-11-01
/// incident was made of.
#[tokio::test]
async fn a_new_instruction_supersedes_the_one_still_queued() {
    let (scheduler, store, model, sid) = scheduler_over_a_store_and_model();
    let run_id = seed_open_run(&store, &sid).await;

    let _ = run_ask(&scheduler, &sid, "第一个任务", Utc::now(), false).await.unwrap();
    let first = scheduler.intents().latest(&sid).await.unwrap().expect("the first intent");
    let _ = run_ask(&scheduler, &sid, "第二个任务", Utc::now(), false).await.unwrap();
    let second = scheduler.intents().latest(&sid).await.unwrap().expect("the second intent");
    assert_ne!(first.intent_id, second.intent_id);

    // The queue holds the newer instruction only.
    let pending = scheduler.inbox().pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].payload["text"], "第二个任务");

    // The older one is not silently gone: it is recorded as displaced, naming
    // the instruction that displaced it.
    let intents = scheduler.intents().all(&sid).await.unwrap();
    let older = intents.iter().find(|intent| intent.intent_id == first.intent_id).unwrap();
    assert_eq!(older.status, IntentStatus::Superseded);
    assert_eq!(older.superseded_by.as_ref(), Some(&second.intent_id));

    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let superseded: Vec<_> = events
        .iter()
        .filter(|event| event.event.event_type.as_str() == EventType::INTENT_SUPERSEDED)
        .collect();
    assert_eq!(superseded.len(), 1, "one instruction was displaced");
    assert_eq!(superseded[0].event.payload["superseded_by"], serde_json::json!(second.intent_id));

    // When the run ends, the queue drains and only the newer instruction runs.
    close_run(&store, &sid, &run_id).await;
    let report = scheduler
        .run_pending(&sid, Utc::now())
        .await
        .unwrap()
        .expect("the queued instruction runs");
    assert_eq!(
        report.intent_id.as_ref(),
        Some(&second.intent_id),
        "the answer belongs to the instruction that displaced the other"
    );
    assert_eq!(model.requests().len(), 1, "the displaced instruction never reached a model");

    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let started: Vec<_> = events
        .iter()
        .filter(|event| event.event.event_type.as_str() == EventType::TURN_STARTED)
        .collect();
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0].event.payload["intent_id"],
        serde_json::json!(second.intent_id),
        "the turn names the instruction it answers"
    );
}

/// A queue entry whose instruction no longer stands is reported as stale rather
/// than counted as work the session still owes.
///
/// The write path cannot produce this shape any more — it discards the item in
/// the same append that displaces the instruction — so the test writes it the
/// way an older log holds it: a pending item bound to an intent that is already
/// `superseded`.
#[tokio::test]
async fn a_queued_item_from_a_displaced_instruction_is_reported_stale() {
    let (scheduler, store, sid) = scheduler_over_a_store();
    let _ = seed_open_run(&store, &sid).await;
    let _ = run_ask(&scheduler, &sid, "第一个任务", Utc::now(), false).await.unwrap();
    let first = scheduler.intents().latest(&sid).await.unwrap().expect("the first intent");
    let _ = run_ask(&scheduler, &sid, "第二个任务", Utc::now(), false).await.unwrap();

    let stale = scheduler
        .inbox()
        .append(
            &sid,
            InboxKind::NextTurn,
            serde_json::json!({"text": "第一个任务", "intent_id": first.intent_id}),
            0,
            Actor::User,
        )
        .await
        .unwrap();

    let CliOutput::Json(body) = run_status(&scheduler, &sid, true).await.unwrap() else {
        panic!("expected json output")
    };
    let status: serde_json::Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(status["stale_items"], serde_json::json!([stale.item_id.as_str()]));
    assert_eq!(status["pending"], 2, "the entry is still counted, and still named");

    let CliOutput::Text(text) = run_status(&scheduler, &sid, false).await.unwrap() else {
        panic!("expected text output")
    };
    assert!(
        text.contains("1 queued item(s) belong to instructions that no longer stand"),
        "{text}"
    );
}

/// The answer `ask` gives says which instruction it belongs to, and `status`
/// says when the newest instruction has not been answered yet.
///
/// The second half is the rule from the 2026-11-01 report: an answer from an
/// older instruction must not read as an answer to the newest one. `ask`
/// reports its own intent; `status` compares the answered intent with the
/// latest one and says when they differ.
#[tokio::test]
async fn an_answer_to_an_older_instruction_is_marked_stale() {
    let (scheduler, store, sid) = scheduler_over_a_store();

    let outcome = run_ask(&scheduler, &sid, "查一下日志", Utc::now(), true).await.unwrap();
    let CliOutput::Json(body) = outcome.output else { panic!("expected json output") };
    let answer: serde_json::Value = serde_json::from_str(body.as_str()).unwrap();
    let answered = answer["intent_id"].as_str().expect("the answer names its intent").to_string();
    assert_eq!(answer["stale"], false, "nothing has overtaken it yet");

    // A newer instruction arrives while a run is open; it waits its turn.
    seed_open_run(&store, &sid).await;
    let _ = run_ask(&scheduler, &sid, "先别管日志，去修 junction 逃逸", Utc::now(), false)
        .await
        .unwrap();

    let CliOutput::Json(body) = run_status(&scheduler, &sid, true).await.unwrap() else {
        panic!("expected json output")
    };
    let status: serde_json::Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(status["answered_intent_id"], serde_json::json!(answered));
    assert_ne!(status["latest_intent_id"], serde_json::json!(answered));
    assert_eq!(status["stale_answer"], true, "the newest instruction has not run: {status}");

    let CliOutput::Text(text) = run_status(&scheduler, &sid, false).await.unwrap() else {
        panic!("expected text output")
    };
    assert!(text.contains(&format!("answered:  {answered}")), "{text}");
    assert!(text.contains("note:"), "the text output carries the warning too: {text}");
    assert!(text.contains("newest one has not run yet"), "{text}");
}

/// A `completion/checked` event, as the loop writes it at a step boundary.
fn check_draft(sid: &SessionId, run_id: &RunId, status: &str) -> EventDraft {
    let mut event = draft(EventType::COMPLETION_CHECKED, sid, run_id);
    event.payload = serde_json::json!({ "status": status, "kind": "command", "steps": 1 });
    event
}

/// A tool result, as the loop writes it after a call the gateway allowed.
fn tool_draft(sid: &SessionId, run_id: &RunId) -> EventDraft {
    let mut event = draft(EventType::TOOL_COMPLETED, sid, run_id);
    event.payload = serde_json::json!({ "tool_name": "fs.write", "call_id": "call-1" });
    event
}

/// An ordinary answer is delivered, but it is not a completion: nothing checked
/// it, so the only honest word is "delivered, no machine criterion".
#[tokio::test]
async fn an_information_answer_has_three_axes_and_no_completion_claim() {
    let (scheduler, sid) = scheduler();
    let _ = run_ask(&scheduler, &sid, "hi", Utc::now(), false).await.unwrap();

    let CliOutput::Json(body) = run_status(&scheduler, &sid, true).await.unwrap() else {
        panic!("expected json output")
    };
    let status: serde_json::Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(status["run_status"], "completed");
    assert_eq!(status["delivery_status"], "delivered");
    assert_eq!(status["verification_status"], "not_applicable");
    assert_eq!(status["completion_status"], serde_json::Value::Null);
    assert!(status["active_intent_id"].is_string(), "{status}");
    assert_eq!(status["pending_intents"], serde_json::json!([]));
    // The fields that predate this slice are still there: `--json` only grew.
    for key in [
        "session",
        "open_run",
        "open_turn",
        "open_step",
        "pending",
        "latest_intent_id",
        "answered_intent_id",
        "stale_answer",
        "stale_items",
        "intent_adjudication",
    ] {
        assert!(status.get(key).is_some(), "{key} disappeared from status --json: {status}");
    }

    let CliOutput::Text(text) = run_status(&scheduler, &sid, false).await.unwrap() else {
        panic!("expected text output")
    };
    assert!(text.contains("execution:  completed"), "{text}");
    assert!(text.contains("delivery:   delivered"), "{text}");
    assert!(text.contains("verify:     not_applicable"), "{text}");
    assert!(text.contains("completion: -"), "{text}");
}

/// A passing criterion is the one way a session may say it finished.
#[tokio::test]
async fn a_passing_check_makes_a_verified_complete() {
    let (scheduler, store, sid) = scheduler_over_a_store();
    let run_id = RunId::new();
    let mut drafts = vec![draft(EventType::RUN_STARTED, &sid, &run_id)];
    drafts.push(check_draft(&sid, &run_id, "completed"));
    let mut turn = draft(EventType::TURN_COMPLETED, &sid, &run_id);
    turn.turn_id = Some(agent_protocol::ids::TurnId::from_string("seeded-turn"));
    drafts.push(turn);
    drafts.push(draft(EventType::RUN_COMPLETED, &sid, &run_id));
    for event in drafts {
        append(store.as_ref(), &sid, event).await;
    }

    let CliOutput::Json(body) = run_status(&scheduler, &sid, true).await.unwrap() else {
        panic!("expected json output")
    };
    let status: serde_json::Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(status["run_status"], "completed");
    assert_eq!(status["verification_status"], "verified");
    assert_eq!(
        status["completion_status"], "verified_complete",
        "a run the operator's criterion passed is the only verified complete: {status}"
    );
    assert_eq!(
        status["delivery_status"], "unanswered",
        "the check ended the turn with no prose answer to hand over"
    );
}

/// Work after a passing check makes that check stale, and staleness is never
/// reported as a completion.
#[tokio::test]
async fn work_after_a_passing_check_is_stale_and_parked() {
    let (scheduler, store, sid) = scheduler_over_a_store();
    let run_id = RunId::new();
    let drafts = vec![
        draft(EventType::RUN_STARTED, &sid, &run_id),
        check_draft(&sid, &run_id, "completed"),
        tool_draft(&sid, &run_id),
        draft(EventType::RUN_COMPLETED, &sid, &run_id),
    ];
    for event in drafts {
        append(store.as_ref(), &sid, event).await;
    }

    let CliOutput::Json(body) = run_status(&scheduler, &sid, true).await.unwrap() else {
        panic!("expected json output")
    };
    let status: serde_json::Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(status["verification_status"], "stale");
    assert_eq!(status["completion_status"], "blocked_or_parked");
}

/// A queued instruction is "pending", and the one still waiting is the one
/// named — the status a person needs before assuming the answer on screen
/// belongs to what they just asked.
#[tokio::test]
async fn a_queued_instruction_is_reported_as_pending() {
    let (scheduler, store, sid) = scheduler_over_a_store();
    seed_open_run(store.as_ref(), &sid).await;
    let _ = run_ask(&scheduler, &sid, "先别动日志", Utc::now(), false).await.unwrap();

    let CliOutput::Json(body) = run_status(&scheduler, &sid, true).await.unwrap() else {
        panic!("expected json output")
    };
    let status: serde_json::Value = serde_json::from_str(body.as_str()).unwrap();
    assert_eq!(status["run_status"], "in_progress");
    assert_eq!(status["completion_status"], serde_json::Value::Null);
    let queued = status["pending_intents"].as_array().expect("a list");
    assert_eq!(queued.len(), 1, "{status}");
    assert_eq!(status["active_intent_id"], queued[0]);
    assert_eq!(status["latest_intent_id"], queued[0]);
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
        model_capabilities: std::collections::BTreeMap::new(),
        budget_sources: std::collections::BTreeMap::new(),
        completion: None,
    }
    .sealed()
}
