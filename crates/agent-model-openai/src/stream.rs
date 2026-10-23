//! Folding a stream of chunks into one settled response.

use std::collections::BTreeMap;

use agent_protocol::model::{ContentBlock, FinishReason, ModelResponse, ModelStreamEvent, Usage};

use crate::wire::{WireChunk, chunk_deltas, normalize_finish_reason};

/// A chunk that could not be interpreted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("stream chunk is not valid JSON: {0}")]
    MalformedChunk(String),
    #[error("stream chunk carries {0} choices; expected at most one")]
    UnexpectedChoices(usize),
}

/// A tool call still being assembled from its fragments.
#[derive(Debug, Clone, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Accumulates chunks until the attempt settles.
///
/// Reasoning text is collected but never becomes a content block: it is live
/// output, and the settled response is what the model-visible history is
/// rebuilt from.
#[derive(Default)]
pub struct StreamAccumulator {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<u32, PartialToolCall>,
    /// Tool call indices in first-seen order, so the settled response does not
    /// depend on map ordering.
    order: Vec<u32>,
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    model: Option<String>,
    deltas: u64,
}

impl StreamAccumulator {
    /// An empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many deltas have been produced so far.
    pub fn deltas(&self) -> u64 {
        self.deltas
    }

    /// Whether nothing at all has been accumulated.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.reasoning.is_empty() && self.tool_calls.is_empty()
    }

    /// The reasoning text collected so far. Live output only — it never settles
    /// into the response.
    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    /// The model name the endpoint reported, when it reported one.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Whether the stream reported a `finish_reason`.
    ///
    /// The protocol's own end-of-answer signal. A connection that closes cleanly
    /// without it — and without the `[DONE]` sentinel — was truncated mid-answer,
    /// not finished.
    pub fn has_finish_reason(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// Absorb one `data:` payload and return the deltas it carries.
    pub fn absorb(&mut self, data: &str) -> Result<Vec<ModelStreamEvent>, DecodeError> {
        let chunk: WireChunk =
            serde_json::from_str(data).map_err(|e| DecodeError::MalformedChunk(e.to_string()))?;
        if chunk.choices.len() > 1 {
            return Err(DecodeError::UnexpectedChoices(chunk.choices.len()));
        }
        if let Some(model) = chunk.model.clone() {
            self.model = Some(model);
        }
        // The usage block arrives in its own final chunk, with no choices.
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.into());
        }
        let Some(choice) = chunk.choices.first() else {
            return Ok(Vec::new());
        };
        if let Some(reason) = choice.finish_reason.as_deref() {
            self.finish_reason = Some(normalize_finish_reason(reason));
        }
        if let Some(text) = choice.delta.content.as_deref() {
            self.text.push_str(text);
        }
        if let Some(text) = choice.delta.reasoning_content.as_deref() {
            self.reasoning.push_str(text);
        }
        for call in choice.delta.tool_calls.iter().flatten() {
            let index = call.index.unwrap_or(0);
            if !self.tool_calls.contains_key(&index) {
                self.order.push(index);
            }
            let entry = self.tool_calls.entry(index).or_default();
            if let Some(id) = call.id.as_ref() {
                entry.id = Some(id.clone());
            }
            if let Some(function) = call.function.as_ref() {
                if let Some(name) = function.name.as_ref() {
                    entry.name = Some(name.clone());
                }
                if let Some(fragment) = function.arguments.as_ref() {
                    entry.arguments.push_str(fragment);
                }
            }
        }
        let events = chunk_deltas(&chunk);
        self.deltas += events.len() as u64;
        Ok(events)
    }

    /// The settled response. A stream that never reported a finish reason
    /// settles as `unknown` rather than as a stop.
    pub fn finish(self) -> ModelResponse {
        let mut content_blocks = Vec::new();
        if !self.text.is_empty() {
            content_blocks.push(ContentBlock::text(self.text));
        }
        for index in &self.order {
            let Some(call) = self.tool_calls.get(index) else { continue };
            content_blocks.push(ContentBlock::tool_use(
                call.id.clone().unwrap_or_default(),
                call.name.clone().unwrap_or_default(),
                parse_arguments(&call.arguments),
            ));
        }
        ModelResponse {
            content_blocks,
            usage: self.usage,
            finish_reason: self
                .finish_reason
                .unwrap_or_else(|| FinishReason::new(FinishReason::UNKNOWN)),
            provider_metadata: serde_json::Value::Null,
        }
    }
}

/// Parse accumulated tool arguments.
///
/// A call that took no arguments accumulates to an empty string, which means an
/// empty object. Anything else unparseable becomes `null` rather than a
/// string — a malformed call must not look like a call with one odd argument.
fn parse_arguments(raw: &str) -> serde_json::Value {
    if raw.trim().is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or(serde_json::Value::Null)
}
