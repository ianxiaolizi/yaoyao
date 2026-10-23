//! Which part of an event is worth searching.
//!
//! The extraction is a pure, total function of one event. It never fails a
//! rebuild: a payload this build cannot read contributes no text and is
//! reported as malformed, because a disposable index that refuses to be built
//! over one odd record is less useful than one that simply leaves it out.
//!
//! What is indexed is what a person would search for later: a turn's input, a
//! steering note, an assistant's words and tool proposals, and a tool's
//! outcome. Structural payload fields (ids, statuses, revisions) are not text
//! anyone asks a memory for.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::model::ContentBlock;
use serde_json::Value;

/// The result of reading one event for the index.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Extracted {
    /// The text to index, when the event carries any.
    pub text: Option<String>,
    /// The event carries a field the extraction knows, in a shape it cannot
    /// read. Skipped rather than fatal, and reported so a test can tell
    /// "nothing to index" from "could not read what was there".
    pub malformed: bool,
}

impl Extracted {
    fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self { text: (!text.is_empty()).then_some(text), malformed: false }
    }

    fn nothing() -> Self {
        Self::default()
    }

    fn malformed() -> Self {
        Self { text: None, malformed: true }
    }
}

/// Read one event for the index.
pub fn extract(event: &StoredEvent) -> Extracted {
    let payload = &event.event.payload;
    match event.event.event_type.as_str() {
        // An inbox item's text lives one level down, inside the item the
        // `inbox/appended` payload carries; the outer object holds the item's
        // envelope (id, kind, status), not its contents.
        EventType::INBOX_APPENDED => match payload.get("payload") {
            None | Some(Value::Null) => Extracted::nothing(),
            Some(item) => match string_field(item, "text") {
                Ok(Some(text)) => Extracted::text(text),
                Ok(None) => Extracted::nothing(),
                Err(()) => Extracted::malformed(),
            },
        },
        EventType::TURN_STEERED => match string_field(payload, "text") {
            Ok(Some(text)) => Extracted::text(text),
            Ok(None) => Extracted::nothing(),
            Err(()) => Extracted::malformed(),
        },
        EventType::MODEL_RESPONSE_COMPLETED => match payload.get("response") {
            None => Extracted::nothing(),
            Some(response) => match response.get("content_blocks") {
                None => Extracted::nothing(),
                Some(blocks) => match serde_json::from_value::<Vec<ContentBlock>>(blocks.clone()) {
                    Ok(blocks) => Extracted::text(
                        blocks
                            .iter()
                            .map(block_text)
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Err(_) => Extracted::malformed(),
                },
            },
        },
        EventType::TOOL_COMPLETED
        | EventType::TOOL_FAILED
        | EventType::TOOL_CANCELLED
        | EventType::TOOL_NOT_STARTED => {
            let mut parts = Vec::new();
            for key in ["tool_name", "content", "error", "reason"] {
                match string_field(payload, key) {
                    Ok(Some(text)) if !text.is_empty() => parts.push(text.to_string()),
                    Ok(_) => {}
                    // One unreadable known field means the event's text cannot
                    // be trusted to be complete; skip it rather than index a
                    // fragment that reads like the whole outcome.
                    Err(()) => return Extracted::malformed(),
                }
            }
            Extracted::text(parts.join("\n"))
        }
        _ => Extracted::nothing(),
    }
}

fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::ToolUse { tool_name, arguments, .. } => {
            format!("{tool_name}\n{arguments}")
        }
        ContentBlock::ToolResult { tool_name, content, .. } => format!("{tool_name}\n{content}"),
    }
}

/// A payload field that is either absent, a string, or a shape this build
/// cannot read.
fn string_field<'a>(payload: &'a Value, key: &str) -> Result<Option<&'a str>, ()> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(()),
    }
}
