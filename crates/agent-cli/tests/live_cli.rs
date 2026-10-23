//! What a person watching a live turn sees, without a terminal.
//!
//! Pure rendering: a live event becomes a labelled chunk or nothing, and the
//! chunk assembler decides where lines break. No process, no store, no clock.

use agent_cli::{LiveChunk, LiveLines, render_live};
use agent_protocol::events::{EventType, LiveEvent};
use agent_protocol::ids::SessionId;
use agent_protocol::model::ModelStreamEvent;
use chrono::Utc;

fn live(event_type: &str, payload: serde_json::Value) -> LiveEvent {
    LiveEvent {
        live_seq: 1,
        event_type: EventType::new(event_type),
        session_id: SessionId::from_string("live"),
        created_at: Utc::now(),
        payload,
        metadata: serde_json::json!({}),
    }
}

fn delta(text: &str) -> LiveEvent {
    live(
        EventType::MODEL_STREAM_DELTA,
        serde_json::to_value(ModelStreamEvent::TextDelta { text: text.to_string() }).unwrap(),
    )
}

#[test]
fn assistant_text_is_labelled_and_reasoning_is_labelled_differently() {
    let chunk = render_live(&delta("hello")).expect("text deltas are shown");
    assert_eq!(chunk, LiveChunk { label: "[model] ", text: "hello".to_string() });

    let reasoning = live(
        EventType::MODEL_STREAM_REASONING_DELTA,
        serde_json::to_value(ModelStreamEvent::ReasoningDelta { text: "hmm".to_string() }).unwrap(),
    );
    let chunk = render_live(&reasoning).expect("reasoning is shown, and marked as its own thing");
    assert_eq!(chunk.label, "[think] ");
}

#[test]
fn structural_live_events_render_to_nothing() {
    // A stream starting is a boundary, and a tool-call delta is half an
    // argument: neither is prose, and both are named in the durable chain.
    assert!(render_live(&live(EventType::MODEL_STREAM_STARTED, serde_json::json!({}))).is_none());
    let call = live(
        EventType::MODEL_STREAM_TOOL_CALL_DELTA,
        serde_json::to_value(ModelStreamEvent::ToolCallDelta {
            index: 0,
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("fs.read".to_string()),
            arguments_delta: "{\"pa".to_string(),
        })
        .unwrap(),
    );
    assert!(render_live(&call).is_none());
    // A payload that is not a stream event at all — a newer producer, a
    // different shape — is skipped rather than guessed at.
    assert!(
        render_live(&live("model/stream.delta", serde_json::json!({"text": "sneaky"}))).is_none()
    );
}

#[test]
fn a_run_of_deltas_is_one_line_with_one_label() {
    let mut lines = LiveLines::default();
    assert_eq!(lines.push(render_live(&delta("Hel")).unwrap()), "[model] Hel");
    assert_eq!(lines.push(render_live(&delta("lo")).unwrap()), "lo");
    assert_eq!(lines.finish(), "\n");
    assert_eq!(lines.finish(), "", "closing an already-closed line prints nothing");
}

#[test]
fn switching_kind_closes_the_line_first() {
    let mut lines = LiveLines::default();
    assert_eq!(lines.push(render_live(&delta("answer")).unwrap()), "[model] answer");
    let reasoning = live(
        EventType::MODEL_STREAM_REASONING_DELTA,
        serde_json::to_value(ModelStreamEvent::ReasoningDelta { text: "why".to_string() }).unwrap(),
    );
    assert_eq!(lines.push(render_live(&reasoning).unwrap()), "\n[think] why");
    assert_eq!(lines.finish(), "\n");
}

#[test]
fn finishing_before_anything_was_written_prints_nothing() {
    let mut lines = LiveLines::default();
    assert_eq!(lines.finish(), "");
}
