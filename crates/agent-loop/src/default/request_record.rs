//! The checkable record a prepared request carries.
//!
//! The request's messages are durable elsewhere already — run instructions, the
//! turn's layers, the input item, the folded history, every tool result — and
//! repeating their content on each step would make the log grow with the whole
//! context on every step. What the prepared event records instead is a reference
//! per message: its uid, its role, and a hash of its content, plus one aggregate
//! hash over the order.
//!
//! Kept out of `default.rs` rather than in it, for the same reason
//! `event_drafts.rs` is: the loop is at its line ratchet, and turning a request
//! into references is a fact about the request, not about the step sending it.

use super::*;

/// The ordered messages a request carries, as checkable references.
///
/// The per-message hash is what makes a reference checkable, and the aggregate
/// hash covers the order.
pub(super) fn message_references(
    messages: &[ModelMessage],
) -> Result<(Vec<serde_json::Value>, String), AgentLoopError> {
    let mut refs = Vec::with_capacity(messages.len());
    for message in messages {
        refs.push(serde_json::json!({
            "message_uid": message.message_uid.as_str(),
            "role": message.role.as_str(),
            "hash": digest::json(&serde_json::to_value(message)?),
        }));
    }
    let hash = digest::json(&serde_json::Value::Array(refs.clone()));
    Ok((refs, hash))
}
