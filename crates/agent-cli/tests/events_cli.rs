//! The `--events` chain: one event per line, as JSON.

use agent_cli::{json_output, render_events};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, RunId, SchemaVersion, SessionId, TurnId};
use chrono::{TimeZone, Utc};

fn stored(
    seq: u64,
    event_type: &str,
    run: Option<&RunId>,
    turn: Option<&TurnId>,
    payload: serde_json::Value,
) -> StoredEvent {
    let session = SessionId::from_string("chain-session");
    StoredEvent {
        seq: EventSeq::new(seq),
        recorded_at: Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap(),
        event: EventDraft {
            event_id: EventId::from_string(format!("evt-{seq}")),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::from_string("0.1.0"),
            session_id: session,
            run_id: run.cloned(),
            turn_id: turn.cloned(),
            step_id: None,
            actor: Actor::Scheduler,
            created_at: Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap(),
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: serde_json::json!({}),
            ignorable: false,
        },
    }
}

#[test]
fn each_event_is_one_line_of_json() {
    let run = RunId::from_string("run-1");
    let turn = TurnId::from_string("turn-1");
    let chain = render_events(&[
        stored(0, EventType::RUN_STARTED, Some(&run), None, serde_json::json!({})),
        stored(1, EventType::TURN_COMPLETED, Some(&run), Some(&turn), serde_json::json!({"x": 1})),
    ]);

    let lines: Vec<&str> = chain.lines().collect();
    assert_eq!(lines.len(), 2, "one line per event, and no trailing newline: {chain:?}");

    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("a JSON object");
    assert_eq!(first["seq"], 0);
    assert_eq!(first["type"], EventType::RUN_STARTED);
    assert_eq!(first["session_id"], "chain-session");
    assert_eq!(first["actor"], "scheduler");
    assert_eq!(first["event_id"], "evt-0");
    assert_eq!(first["run_id"], "run-1");
    assert_eq!(first["turn_id"], serde_json::Value::Null, "a run level event has no turn");
    assert_eq!(first["recorded_at"], "2026-10-08T12:00:00+00:00");

    let second: serde_json::Value = serde_json::from_str(lines[1]).expect("a JSON object");
    assert_eq!(second["seq"], 1);
    assert_eq!(second["turn_id"], "turn-1");
    assert_eq!(second["payload"], serde_json::json!({"x": 1}), "the payload is carried whole");
}

/// The other shape: a single-object `--json` answer is one pretty object.
///
/// The two shapes are different decisions on purpose — this file pins the
/// line-delimited one above, this test pins the single-object one. It is
/// checked here because [`json_output`] is the only way to build a `JsonDoc` —
/// a command cannot choose another shape — so the body of that function is
/// where the rule actually lives, and this is what would notice if it changed.
#[test]
fn a_single_object_answer_is_pretty() {
    let answer = json_output(&serde_json::json!({ "session": "s", "pending": 1 })).unwrap();
    assert!(answer.as_str().starts_with("{\n"), "one pretty object: {answer}");
    assert!(answer.as_str().contains("\n  \"pending\": 1"), "{answer}");
    // Pretty is whitespace, not content: the same bytes parse to the same value.
    let parsed: serde_json::Value = serde_json::from_str(answer.as_str()).unwrap();
    assert_eq!(parsed["pending"], 1);
}

#[test]
fn a_payload_cannot_break_the_line() {
    // A payload containing newlines is the one thing that could turn one event
    // into two lines; JSON escaping is what stops it, and this is the test that
    // says so rather than assuming it.
    let chain = render_events(&[stored(
        0,
        EventType::TURN_COMPLETED,
        None,
        None,
        serde_json::json!({ "text": "line one\nline two" }),
    )]);
    assert_eq!(chain.lines().count(), 1, "{chain:?}");
    let value: serde_json::Value = serde_json::from_str(&chain).expect("a JSON object");
    assert_eq!(value["payload"]["text"], "line one\nline two");
}

#[test]
fn no_events_render_to_nothing() {
    assert_eq!(render_events(&[]), "", "an empty chain is not an empty line");
}
