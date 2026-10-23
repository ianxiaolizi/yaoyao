//! `steer`, `interrupt` and `export`: the commands that talk *about* a session
//! rather than driving one.
//!
//! All three are tested against a hand-built log instead of by running a turn.
//! What is under test here is where the command points — which turn a note is
//! addressed to, whether an interrupt is queued at all, which events and
//! artifact references end up in an export — and a log written directly is the
//! honest fixture for that. Whether a steered note reaches a running loop, and
//! whether a queued interrupt stops one, is the kernel's contract, pinned by
//! the scheduler's own tests (`an_interrupt_ends_the_turn_at_the_next_boundary`).

use std::sync::Arc;

use agent_cli::{CliOutput, Verdict, run_export, run_interrupt, run_steer};
use agent_kernel::scheduler::Scheduler;
use agent_ports::agent_loop::{
    AgentLoop, AgentLoopError, SteeringInput, StepOutcome, TurnContext, TurnStart, TurnState,
};
use agent_ports::event_store::EventStore;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, RunId, SchemaVersion, SessionId, TurnId};
use agent_protocol::inbox::InboxKind;
use agent_testkit::FakeEventStore;
use chrono::Utc;

/// A loop that refuses to run.
///
/// The tests below never drive a turn. A loop that could produce a step would
/// let a test pass by having run something, which is exactly what these tests
/// are not asking about.
struct NoTurnDriven;

#[async_trait::async_trait]
impl AgentLoop for NoTurnDriven {
    async fn begin_turn(&self, _ctx: &TurnContext) -> Result<TurnStart, AgentLoopError> {
        Err(AgentLoopError::Model("this test never drives a turn".to_string()))
    }

    async fn step(
        &self,
        _turn: &mut TurnState,
        _steering: &[SteeringInput],
    ) -> Result<StepOutcome, AgentLoopError> {
        Err(AgentLoopError::Model("this test never drives a turn".to_string()))
    }
}

fn draft(
    session: &SessionId,
    event_type: &str,
    turn: Option<&TurnId>,
    payload: serde_json::Value,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: session.clone(),
        run_id: Some(RunId::from_string("run-1")),
        turn_id: turn.cloned(),
        step_id: None,
        actor: Actor::Scheduler,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

async fn open_turn(store: &FakeEventStore, session: &SessionId, turn: &TurnId) {
    store
        .append(
            session,
            EventSeq::ZERO,
            vec![draft(session, EventType::TURN_STARTED, Some(turn), serde_json::json!({}))],
        )
        .await
        .unwrap();
}

fn scheduler(store: Arc<FakeEventStore>) -> Scheduler {
    Scheduler::new(store, Arc::new(NoTurnDriven), 4)
}

#[tokio::test]
async fn steer_addresses_the_turn_that_is_open() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("steer-open");
    let turn = TurnId::from_string("turn-1");
    open_turn(&store, &sid, &turn).await;
    let scheduler = scheduler(store);

    let output = run_steer(&scheduler, &sid, "also check the tests", true).await.unwrap();

    match output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert_eq!(value["expected_turn_id"], "turn-1");
        }
        _ => panic!("expected json output"),
    }
    // The target travels with the text, not only in the report: the scheduler
    // reads it from the item, and the report is for the person who typed it.
    let pending = scheduler.inbox().pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, InboxKind::NextStep);
    assert_eq!(pending[0].payload["text"], "also check the tests");
    assert_eq!(pending[0].payload["expected_turn_id"], "turn-1");
}

#[tokio::test]
async fn steer_with_no_open_turn_addresses_the_next_one() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("steer-idle");
    let scheduler = scheduler(store);

    let output = run_steer(&scheduler, &sid, "start with the tests", true).await.unwrap();

    match output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert!(value["expected_turn_id"].is_null(), "{value}");
        }
        _ => panic!("expected json output"),
    }
    let pending = scheduler.inbox().pending(&sid).await.unwrap();
    assert!(pending[0].payload["expected_turn_id"].is_null());
}

#[tokio::test]
async fn steer_refuses_text_that_is_not_there() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("steer-empty");
    let scheduler = scheduler(store);

    let err = run_steer(&scheduler, &sid, "   ", false).await.unwrap_err();
    assert!(err.contains("needs text"), "unexpected error: {err}");
}

async fn append(store: &FakeEventStore, session: &SessionId, drafts: Vec<EventDraft>) {
    store.append(session, EventSeq::ZERO, drafts).await.unwrap();
}

#[tokio::test]
async fn interrupt_refuses_when_there_is_no_run_to_interrupt() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("interrupt-idle");
    let scheduler = scheduler(store);

    let outcome = run_interrupt(&scheduler, &sid, false).await.unwrap();

    assert_eq!(outcome.verdict, Verdict::DidNotRun);
    let CliOutput::Text(text) = outcome.output else { panic!("expected text output") };
    assert!(text.contains("no run to interrupt"), "{text}");
    // The refusal is the point, not politeness. An interrupt appended to an idle
    // session is read at the *top* of the next run's boundary loop — before its
    // first step — so it would not wait harmlessly, it would kill a turn nobody
    // asked it to touch.
    assert!(scheduler.inbox().pending(&sid).await.unwrap().is_empty());
}

#[tokio::test]
async fn interrupt_refusal_is_a_machine_readable_no() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("interrupt-idle-json");
    let scheduler = scheduler(store);

    let outcome = run_interrupt(&scheduler, &sid, true).await.unwrap();

    assert_eq!(outcome.verdict, Verdict::DidNotRun);
    let CliOutput::Json(text) = outcome.output else { panic!("expected json output") };
    assert!(text.as_str().starts_with("{\n"), "one pretty object: {text}");
    let value: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
    assert!(value["queued"].is_null(), "{value}");
    assert_eq!(value["reason"], "no_run");
}

#[tokio::test]
async fn interrupt_queues_while_a_run_is_open() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("interrupt-open");
    let turn = TurnId::from_string("turn-1");
    append(
        &store,
        &sid,
        vec![
            draft(&sid, EventType::RUN_STARTED, None, serde_json::json!({})),
            draft(&sid, EventType::TURN_STARTED, Some(&turn), serde_json::json!({})),
        ],
    )
    .await;
    let scheduler = scheduler(store);

    let outcome = run_interrupt(&scheduler, &sid, false).await.unwrap();

    assert_eq!(outcome.verdict, Verdict::Answered);
    let pending = scheduler.inbox().pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, InboxKind::Interrupt);
    // The item carries a reason because the inbox type has the field. Nothing
    // reads it back — `Ending::Interrupted` settles on the item id and writes
    // its own `cause` — which is why there is no `--reason` flag to set it.
    assert_eq!(pending[0].payload["reason"], "user_interrupt");
}

#[tokio::test]
async fn interrupt_queues_for_a_turn_that_is_only_waiting() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("interrupt-yielded");
    let turn = TurnId::from_string("turn-1");
    append(
        &store,
        &sid,
        vec![
            draft(&sid, EventType::RUN_STARTED, None, serde_json::json!({})),
            draft(&sid, EventType::TURN_STARTED, Some(&turn), serde_json::json!({})),
            draft(&sid, EventType::TURN_YIELDED, Some(&turn), serde_json::json!({})),
            draft(&sid, EventType::RUN_YIELDED, None, serde_json::json!({})),
        ],
    )
    .await;
    let scheduler = scheduler(store);

    // The state this test exists for, asserted rather than assumed: a session
    // paused on a decision has no open run at all, because `run/yielded` cleared
    // it. A check that read only `open_run` would refuse to interrupt the very
    // state a person most often wants out of.
    let lifecycle = scheduler.lifecycle(&sid).await.unwrap();
    assert!(lifecycle.open_run.is_none(), "{lifecycle:?}");
    assert_eq!(lifecycle.yielded_turn.as_ref().map(|t| t.as_str()), Some("turn-1"));

    let outcome = run_interrupt(&scheduler, &sid, false).await.unwrap();

    assert_eq!(outcome.verdict, Verdict::Answered);
    let pending = scheduler.inbox().pending(&sid).await.unwrap();
    assert_eq!(pending[0].kind, InboxKind::Interrupt);
}

/// A log with a turn, a tool that offloaded its output, and the offload event
/// that names the blob — the three things an export has to render differently.
async fn exportable() -> (Arc<FakeEventStore>, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("export-session");
    let turn = TurnId::from_string("turn-1");
    let reference = serde_json::json!({
        "locator": "artifact://sha256/abc",
        "bytes": 60_000,
        "mime": "text/plain",
    });

    let drafts = vec![
        draft(&sid, EventType::TURN_STARTED, Some(&turn), serde_json::json!({})),
        draft(
            &sid,
            EventType::TOOL_STARTED,
            Some(&turn),
            serde_json::json!({ "tool_name": "fs.read", "arguments": { "path": "notes.txt" } }),
        ),
        draft(
            &sid,
            EventType::TOOL_RESULT_ARTIFACT_OFFLOADED,
            Some(&turn),
            serde_json::json!({ "tool_name": "fs.read", "artifact_refs": [reference] }),
        ),
        draft(
            &sid,
            EventType::TOOL_COMPLETED,
            Some(&turn),
            serde_json::json!({
                "tool_name": "fs.read",
                "status": "completed",
                "content": "the whole file was long",
                "artifact_refs": [reference],
            }),
        ),
    ];
    store.append(&sid, EventSeq::ZERO, drafts).await.unwrap();
    (store, sid)
}

#[tokio::test]
async fn export_markdown_groups_by_turn_and_names_the_artifacts() {
    let (store, sid) = exportable().await;

    let output = run_export(store.as_ref(), &sid, None, false, false).await.unwrap();
    let CliOutput::Text(markdown) = output else { panic!("expected text output") };

    assert!(markdown.contains("# Session export-session"), "{markdown}");
    assert!(markdown.contains("## Turn turn-1"), "{markdown}");
    assert!(markdown.contains("**tool/started** fs.read"), "{markdown}");
    assert!(markdown.contains("**tool/completed** fs.read (completed)"), "{markdown}");
    assert!(markdown.contains("## Artifacts"), "{markdown}");
    assert!(markdown.contains("artifact://sha256/abc (60000 bytes, text/plain)"), "{markdown}");
}

#[tokio::test]
async fn export_bundle_is_the_log_and_a_deduplicated_artifact_index() {
    let (store, sid) = exportable().await;

    let output = run_export(store.as_ref(), &sid, None, false, true).await.unwrap();
    let CliOutput::Json(bundle) = output else { panic!("expected json output") };
    let value: serde_json::Value = serde_json::from_str(bundle.as_str()).unwrap();

    assert_eq!(value["session"], "export-session");
    assert_eq!(value["event_count"], 4);
    // The same blob named by two events is one artifact: counting it twice
    // would overstate what the session produced.
    let artifacts = value["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "{artifacts:?}");
    assert_eq!(artifacts[0]["locator"], "artifact://sha256/abc");
    // The events are the log itself, not a rendering of it.
    let events = value["events"].as_array().unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(events[3]["event"]["payload"]["content"], "the whole file was long");
    // Emitted in both directions, so a reader never has to infer "it was
    // scrubbed" from the absence of something.
    assert_eq!(value["redacted"], false);
}

/// A log where a secret got in through the door the kernel leaves open: an
/// inbox item's payload is written verbatim, so text a person typed into a
/// prompt is in the log exactly as typed. The metadata block carries the same
/// token on purpose — a scrub that covers one of two adjacent copies is not a
/// scrub — and a second event with nothing secret in it pins that redaction
/// leaves everything else alone.
const TOKEN: &str = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn leaky() -> (Arc<FakeEventStore>, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("export-leaky");

    let mut item = draft(
        &sid,
        EventType::INBOX_APPENDED,
        None,
        serde_json::json!({
            "kind": "next_turn",
            "payload": { "text": format!("look what I found: {TOKEN}") },
        }),
    );
    item.metadata = serde_json::json!({ "note": format!("copied from {TOKEN}") });

    let plain = draft(
        &sid,
        EventType::TURN_STARTED,
        Some(&TurnId::from_string("turn-1")),
        serde_json::json!({ "nothing": "to hide here" }),
    );

    append(&store, &sid, vec![item, plain]).await;
    (store, sid)
}

#[tokio::test]
async fn export_redacted_scrubs_the_bundle_and_says_that_it_did() {
    let (store, sid) = leaky().await;

    let output = run_export(store.as_ref(), &sid, None, true, true).await.unwrap();
    let CliOutput::Json(bundle) = output else { panic!("expected json output") };
    assert!(!bundle.as_str().contains(TOKEN), "the token survived into the bundle: {bundle}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(bundle.as_str()).unwrap()["redacted"],
        true,
        "a redacted bundle has to say so"
    );
    // Both halves of the draft, not just the payload: `metadata` is written by
    // whatever recorded the event and is free to repeat what the payload says.
    assert!(bundle.as_str().contains("***"), "{bundle}");

    // The unredacted export of the same log still has it. This is the test's own
    // control: without it, a token that never made it into the log at all would
    // pass the assertion above.
    let output = run_export(store.as_ref(), &sid, None, false, true).await.unwrap();
    let CliOutput::Json(bundle) = output else { panic!("expected json output") };
    assert!(bundle.as_str().contains(TOKEN), "the fixture no longer writes the token: {bundle}");
}

async fn bundle_of(store: &FakeEventStore, sid: &SessionId, redacted: bool) -> serde_json::Value {
    let output = run_export(store, sid, None, redacted, true).await.unwrap();
    let CliOutput::Json(bundle) = output else { panic!("expected json output") };
    serde_json::from_str(bundle.as_str()).unwrap()
}

#[tokio::test]
async fn export_redacted_keeps_the_shape_of_what_it_scrubs() {
    let (store, sid) = leaky().await;

    let before = bundle_of(&store, &sid, false).await;
    let after = bundle_of(&store, &sid, true).await;

    // A redacted event is the same event with quieter bytes: same seq, same id,
    // same type. Replacing a value outright instead of recursing through it
    // would make the bundle a different record rather than a sanitized one.
    let shape = |bundle: &serde_json::Value| -> Vec<(u64, String, String)> {
        bundle["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stored| {
                (
                    stored["seq"].as_u64().unwrap(),
                    stored["event"]["event_id"].as_str().unwrap().to_string(),
                    stored["event"]["event_type"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    assert_eq!(shape(&before), shape(&after));

    // Every leaf the token sat in, and nothing else.
    assert_eq!(after["events"][0]["event"]["payload"]["payload"]["text"], "look what I found: ***");
    assert_eq!(after["events"][0]["event"]["metadata"]["note"], "copied from ***");
    assert_eq!(after["events"][1]["event"]["payload"]["nothing"], "to hide here");
    assert!(
        before["events"][0]["event"]["payload"]["payload"]["text"]
            .as_str()
            .unwrap()
            .contains(TOKEN),
        "the fixture no longer puts the token where this test looks for it"
    );
}

#[tokio::test]
async fn export_markdown_only_admits_redaction_when_it_happened() {
    let (store, sid) = leaky().await;

    let markdown = |output: CliOutput| match output {
        CliOutput::Text(text) => text,
        _ => panic!("expected text output"),
    };

    let plain = markdown(run_export(store.as_ref(), &sid, None, false, false).await.unwrap());
    let redacted = markdown(run_export(store.as_ref(), &sid, None, true, false).await.unwrap());

    // The document is the half where this matters most: its one-line summary of
    // an inbox item is the text a person typed, so a token pasted into a prompt
    // shows up in `session.md` in full unless the flag is passed.
    assert!(plain.contains(TOKEN), "the markdown summary stopped showing inbox text: {plain}");
    assert!(!redacted.contains(TOKEN), "{redacted}");
    assert!(
        !plain.contains("Redacted:"),
        "an unredacted export must not carry the notice: {plain}"
    );
    assert!(redacted.contains("Redacted:"), "{redacted}");
    // The notice says what the filter is *not*. A redacted export invites exactly
    // one wrong conclusion — that what remains is safe to paste.
    assert!(redacted.contains("not a guarantee"), "{redacted}");
}

#[tokio::test]
async fn export_writes_both_halves_when_given_a_directory() {
    let (store, sid) = exportable().await;
    // A directory named after a fresh id: unique per run, and owned by this
    // test alone, so removing it afterwards cannot take anything else with it.
    let dir = std::env::temp_dir().join(format!("yaoyao-export-{}", EventId::new().as_str()));
    let dir_text = dir.to_string_lossy().to_string();

    let output = run_export(store.as_ref(), &sid, Some(&dir_text), false, false).await.unwrap();
    match output {
        CliOutput::Text(t) => assert!(t.contains("session.md"), "unexpected output: {t}"),
        _ => panic!("expected text output"),
    }

    let markdown = std::fs::read_to_string(dir.join("session.md")).unwrap();
    let bundle = std::fs::read_to_string(dir.join("bundle.json")).unwrap();
    assert!(markdown.contains("artifact://sha256/abc"), "{markdown}");
    assert!(bundle.contains("\"event_count\": 4"), "{bundle}");

    std::fs::remove_dir_all(&dir).unwrap();
}
