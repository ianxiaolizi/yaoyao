//! Reading a request the way a test wants to read it: what was said, in
//! order, past the frame the loop places around the conversation.
//!
//! These live in the test-support crate because more than one crate's tests
//! ask the same question, and the crate that asked first is at its size
//! ratchet.

use agent_protocol::model::{ContentBlock, MessageRole, ModelMessage};

/// Whether a message is the loop's own per-request tail — the work state, the
/// budget reminder — rather than anything the user or the model said.
pub fn is_loop_tail(uid: &str) -> bool {
    uid.starts_with("msg/work-state/")
        || uid.starts_with("msg/budget/")
        || uid.starts_with("msg/stall/")
}

/// The last thing said, past the loop's own tails.
pub fn last_said(messages: &[ModelMessage]) -> Option<&ModelMessage> {
    messages.iter().rev().find(|m| !is_loop_tail(m.message_uid.as_str()))
}

/// The words the model was shown, message by message, in order.
///
/// System messages are skipped: they are the frame around the conversation
/// (instructions, the per-turn state and task layers, the context notice),
/// while tests that use this are about what the user and the tools said. The
/// loop's own tails are skipped for the same reason — they are not words
/// anyone said.
///
/// Text and tool outcomes are what carry words; a tool proposal carries none
/// and so contributes nothing. The result reads as "what was said to the
/// model, and when".
pub fn shown_text(messages: &[ModelMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role != MessageRole::System && !is_loop_tail(m.message_uid.as_str()))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                    ContentBlock::ToolUse { .. } => None,
                })
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .collect()
}
