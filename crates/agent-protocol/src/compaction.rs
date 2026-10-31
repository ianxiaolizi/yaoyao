//! Atomic groups of a transcript, and the split compaction keeps.
//!
//! A tool call and the messages answering it must share one fate: summarizing
//! one without the other leaves the model an answer it cannot attribute, or a
//! call nothing will answer. The trimmer and the loop both need that rule — the
//! loop compacts its own in-flight traffic mid-turn — and the loop may not
//! depend on `agent-context`, so the shape lives here, beside the estimator it
//! is measured with.

use crate::model::{ContentBlock, MessageRole, ModelMessage, estimate_message};

/// The share of the usable window compaction keeps as recent history.
///
/// Fixed for now. A future adaptive policy can change it without changing the
/// log format: `context/compacted` records what was actually summarized, so a
/// past compaction replays the same however the split is chosen today.
pub const COMPACTION_KEEP_PERCENT: u32 = 50;

/// One atomic group of messages: an inclusive range kept or dropped whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    pub start: usize,
    pub end: usize,
    /// An orphaned tool result. Never kept, whatever the budget.
    pub keep: bool,
}

impl Group {
    /// Estimated tokens of the group, framing included.
    pub fn cost(&self, messages: &[ModelMessage]) -> usize {
        messages[self.start..=self.end].iter().map(estimate_message).sum()
    }
}

/// The number of system messages opening the history that are not summaries.
///
/// Only the contiguous leading run counts. A summary (and the ledger beside
/// it) is a `system` message but it stands in for history, not for the run's
/// instructions: if it were counted as part of the preamble it could never be
/// proposed, and every compaction would add another summary beside the last
/// one instead of replacing it.
pub fn leading_system_len(messages: &[ModelMessage]) -> usize {
    messages
        .iter()
        .take_while(|message| {
            message.role == MessageRole::System && !message.is_compaction_output()
        })
        .count()
}

/// Split the history into atomic groups, in order.
///
/// A message proposing tool calls absorbs the messages that carry results for
/// those calls, so the call and its answer share one fate. Grouping ahead of the
/// budget walk is what makes "never leave a result without its call" a property
/// of the shape rather than a check after the fact.
pub fn groups(messages: &[ModelMessage]) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let start = index;
        let mut end = index;
        let mut wanted: Vec<String> = tool_use_ids(&messages[index]);

        while !wanted.is_empty() && end + 1 < messages.len() {
            let next = &messages[end + 1];
            let answers = tool_result_ids(next);
            if !answers.iter().any(|id| wanted.iter().any(|wanted_id| wanted_id == id)) {
                break;
            }
            end += 1;
            // A grouped message may itself propose calls (a provider that
            // answers and calls in one message); those are part of the group
            // and must keep their own results.
            wanted.extend(tool_use_ids(next));
        }

        // A message with results but no calls of its own is only legitimate as
        // part of the group above; reaching it here means it has no call.
        let orphan = wanted.is_empty() && !tool_result_ids(&messages[index]).is_empty();
        groups.push(Group { start, end, keep: !orphan });
        index = end + 1;
    }
    groups
}

/// Where the split falls: the first group that stays, and what staying costs.
///
/// Walks groups newest to oldest, always keeping the newest one, and keeps
/// whole groups while they still fit `keep_target` — measured on top of
/// `spent`, the tokens that always remain (a preamble, a fixed head). `None`
/// means nothing would be summarized: keeping the newest group already leaves
/// no older group to propose.
pub fn keep_from(
    messages: &[ModelMessage],
    groups: &[Group],
    spent: usize,
    keep_target: usize,
) -> Option<(usize, usize)> {
    let kept: Vec<usize> =
        groups.iter().enumerate().filter(|(_, group)| group.keep).map(|(index, _)| index).collect();
    let newest = *kept.last()?;
    let mut keep_from = newest;
    let mut retained = spent + groups[newest].cost(messages);
    for &index in kept.iter().rev().skip(1) {
        let cost = groups[index].cost(messages);
        if retained + cost > keep_target {
            break;
        }
        retained += cost;
        keep_from = index;
    }
    if keep_from == kept[0] {
        return None;
    }
    Some((keep_from, retained))
}

/// The longest run of whole oldest groups that fits `available_tokens`.
///
/// Used when the loop has to clip a summary candidate into the fast route's
/// input working set: the prefix is taken group by group from the oldest end,
/// so a tool call and its result are always kept or dropped together. Orphan
/// groups carry no call and are skipped rather than paid for. An empty result
/// means even the first eligible group does not fit — the caller then decides
/// whether a larger route can answer.
pub fn fit_leading_groups(messages: &[ModelMessage], available_tokens: usize) -> Vec<ModelMessage> {
    let mut fitted = Vec::new();
    let mut spent = 0usize;
    for group in groups(messages) {
        if !group.keep {
            continue;
        }
        let cost = group.cost(messages);
        if spent.saturating_add(cost) > available_tokens {
            break;
        }
        spent += cost;
        fitted.extend_from_slice(&messages[group.start..=group.end]);
    }
    fitted
}

fn tool_use_ids(message: &ModelMessage) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(message: &ModelMessage) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(role: MessageRole, text: &str) -> ModelMessage {
        ModelMessage::new(role, vec![ContentBlock::text(text)])
    }

    fn call(id: &str, tool: &str) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::ToolUse {
                tool_call_id: id.to_string(),
                tool_name: tool.to_string(),
                arguments: serde_json::json!({}),
            }],
        )
    }

    fn result(id: &str, tool: &str) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Tool,
            vec![ContentBlock::ToolResult {
                tool_call_id: id.to_string(),
                tool_name: tool.to_string(),
                content: "ok".to_string(),
                is_error: false,
            }],
        )
    }

    #[test]
    fn a_call_and_its_result_are_one_group() {
        let messages = vec![
            text(MessageRole::User, "a"),
            call("1", "fs.read"),
            result("1", "fs.read"),
            text(MessageRole::Assistant, "b"),
        ];
        let groups = groups(&messages);
        assert_eq!(groups.len(), 3);
        assert_eq!((groups[1].start, groups[1].end), (1, 2));
    }

    #[test]
    fn an_orphan_result_is_never_kept() {
        let messages = vec![text(MessageRole::User, "a"), result("x", "fs.read")];
        let groups = groups(&messages);
        assert!(!groups[1].keep);
    }

    #[test]
    fn the_newest_group_stays_even_over_target_and_nothing_is_proposed() {
        let messages = vec![text(MessageRole::User, "only")];
        let groups = groups(&messages);
        assert_eq!(keep_from(&messages, &groups, 0, 0), None);
    }

    #[test]
    fn older_groups_are_proposed_once_the_target_is_full() {
        let messages = vec![
            text(MessageRole::User, "old message with some prose in it"),
            text(MessageRole::Assistant, "recent message with prose"),
        ];
        let groups = groups(&messages);
        let (keep_from, retained) =
            keep_from(&messages, &groups, 0, 1).expect("the older group is proposed");
        assert_eq!(keep_from, 1);
        assert_eq!(retained, estimate_message(&messages[1]));
    }

    fn text_with_uid(role: MessageRole, uid: &str, text: &str) -> ModelMessage {
        ModelMessage {
            message_uid: crate::ids::MessageUid::from_string(uid),
            role,
            content: vec![ContentBlock::text(text)],
        }
    }

    fn call_with_uid(id: &str) -> ModelMessage {
        let mut message = call(id, "fs.read");
        message.message_uid = crate::ids::MessageUid::from_string(format!("call_{id}"));
        message
    }

    fn result_with_uid(id: &str) -> ModelMessage {
        let mut message = result(id, "fs.read");
        message.message_uid = crate::ids::MessageUid::from_string(format!("result_{id}"));
        message
    }

    fn uids(messages: &[ModelMessage]) -> Vec<&str> {
        messages.iter().map(|message| message.message_uid.as_str()).collect()
    }

    #[test]
    fn fitting_leading_groups_keeps_whole_oldest_groups() {
        let messages = vec![
            text_with_uid(MessageRole::User, "old", "old"),
            call_with_uid("1"),
            result_with_uid("1"),
            text_with_uid(MessageRole::User, "new", "new"),
        ];
        let groups = groups(&messages);
        let available = groups[0].cost(&messages) + groups[1].cost(&messages);
        assert!(available < groups[0].cost(&messages) + groups[1].cost(&messages) + groups[2].cost(&messages));
        let fitted = fit_leading_groups(&messages, available);
        assert_eq!(uids(&fitted), vec!["old", "call_1", "result_1"]);
    }

    #[test]
    fn fitting_leading_groups_never_splits_a_tool_call_from_its_result() {
        let messages = vec![call_with_uid("1"), result_with_uid("1"), text_with_uid(MessageRole::User, "new", "new")];
        assert!(fit_leading_groups(&messages, 1).is_empty());
    }

    #[test]
    fn a_summary_is_history_not_preamble() {
        let messages = vec![
            text(MessageRole::System, "instructions"),
            ModelMessage::summary_for(
                &crate::ids::AttemptId::from_string("a1"),
                "summary of earlier turns",
            ),
            text(MessageRole::User, "next"),
        ];
        assert_eq!(leading_system_len(&messages), 1);
    }

    #[test]
    fn a_ledger_beside_a_summary_is_history_too() {
        let messages = vec![
            text(MessageRole::System, "instructions"),
            ModelMessage::summary_for(&crate::ids::AttemptId::from_string("a1"), "summary"),
            ModelMessage::ledger_for(&crate::ids::AttemptId::from_string("a1"), "ledger"),
            text(MessageRole::User, "next"),
        ];
        assert_eq!(leading_system_len(&messages), 1);
    }
}
