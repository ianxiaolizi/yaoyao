//! Measuring a derived context without changing it.
//!
//! The report exists to answer four questions about the messages a model is
//! about to see: what is in them, how much is each kind, how much room is left,
//! and which single messages are the largest. It is a read model: it never
//! trims, rewrites, or decides anything.
//!
//! Every number comes from [`crate::trim::estimate_block`] and
//! [`crate::trim::estimate_message`] — the same estimate the trimmer uses. A
//! second counter here would be a second answer to "how big is this context",
//! and the two would drift the first time either changed. The categories plus
//! the framing category therefore add up to the derivation's estimate exactly;
//! a test pins that.
//!
//! The frame's state and task layers are placed per turn by the loop and are
//! not folded into history, so they never reach this module: the report counts
//! the settled context only. `context inspect` reads those two layers back from
//! `model/request.prepared.context_layers` instead, the same way it reads the
//! per-turn notice back. Counting them here as always-zero categories would
//! claim the report measures something it does not see.

use agent_ports::context_engine::{
    ContextCategoryKind, ContextCategoryUsage, ContextUsage, ContextUsageReport, LargestMessage,
};
use agent_protocol::model::{ContentBlock, MessageRole, ModelMessage};

use crate::trim::{FRAME_TOKENS, estimate_block, estimate_message};

/// How many characters of a message a preview carries.
///
/// Characters, not bytes: a preview cut inside a multi-byte character would
/// either panic or stop being text. The ellipsis is the only added character.
const PREVIEW_CHARS: usize = 80;

/// Measure `messages` and the usage the derivation reported for them.
pub fn report(messages: &[ModelMessage], usage: &ContextUsage, top: usize) -> ContextUsageReport {
    let mut categories: Vec<ContextCategoryUsage> = ContextCategoryKind::ALL
        .iter()
        .map(|kind| ContextCategoryUsage {
            kind: *kind,
            messages: 0,
            blocks: 0,
            estimated_tokens: 0,
        })
        .collect();

    let mut sized: Vec<(usize, usize)> = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let mut seen = [false; ContextCategoryKind::ALL.len()];
        for block in &message.content {
            let kind = kind_for(message, block);
            let slot = &mut categories[kind.index()];
            slot.blocks += 1;
            slot.estimated_tokens += estimate_block(block);
            if !seen[kind.index()] {
                seen[kind.index()] = true;
                slot.messages += 1;
            }
        }
        // The frame is charged once per message, exactly as `estimate_message`
        // charges it. Counting it here rather than subtracting later keeps the
        // categories additive.
        let framing = &mut categories[ContextCategoryKind::Framing.index()];
        framing.messages += 1;
        framing.blocks += 1;
        framing.estimated_tokens += FRAME_TOKENS;
        sized.push((index, estimate_message(message)));
    }

    // Stable: two equally large messages keep their log order, so a report of
    // an unchanged context is byte-identical.
    sized.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let largest_messages = sized
        .into_iter()
        .take(top.min(messages.len()))
        .map(|(index, estimated_tokens)| largest_message(&messages[index], estimated_tokens))
        .collect();

    // The usable budget is what the answer does not reserve. A reservation
    // without a window is not a policy, so it contributes nothing here either.
    let budget_tokens = usage.max_input_tokens.map(|max| {
        (u64::from(max).saturating_sub(u64::from(usage.reserved_output_tokens.unwrap_or(0))))
            as usize
    });
    let remaining_tokens =
        budget_tokens.map(|budget| budget.saturating_sub(usage.estimated_tokens));
    let percent_used = budget_tokens.and_then(|budget| {
        if budget == 0 {
            // A zero budget has no percentage: anything divided by it would be
            // a made-up number. The config layer refuses this window anyway.
            None
        } else {
            let percent = (usage.estimated_tokens as u64).saturating_mul(100) / budget as u64;
            Some(u32::try_from(percent.min(u64::from(u32::MAX))).unwrap_or(u32::MAX))
        }
    });

    ContextUsageReport {
        message_count: messages.len(),
        dropped_messages: usage.dropped_messages,
        estimated_tokens: usage.estimated_tokens,
        max_input_tokens: usage.max_input_tokens,
        reserved_output_tokens: usage.reserved_output_tokens,
        budget_tokens,
        remaining_tokens,
        percent_used,
        categories,
        largest_messages,
    }
}

/// Which category one block belongs to.
fn kind_for(message: &ModelMessage, block: &ContentBlock) -> ContextCategoryKind {
    match block {
        ContentBlock::Text { .. } => match message.role {
            MessageRole::System => system_kind(message.message_uid.as_str()),
            MessageRole::User => ContextCategoryKind::UserText,
            MessageRole::Assistant => ContextCategoryKind::AssistantText,
            MessageRole::Tool => ContextCategoryKind::ToolText,
        },
        ContentBlock::ToolUse { .. } => ContextCategoryKind::ToolCalls,
        ContentBlock::ToolResult { is_error, .. } => {
            if *is_error {
                ContextCategoryKind::ToolResultsFailed
            } else {
                ContextCategoryKind::ToolResultsOk
            }
        }
    }
}

/// A system message is identified by the uid its producer chose, not by
/// parsing its text: the uid is the durable identity, and the report should
/// classify the message the log names rather than the sentence it happens to
/// carry.
fn system_kind(uid: &str) -> ContextCategoryKind {
    if uid.starts_with("msg/notice/") {
        ContextCategoryKind::Notice
    } else if uid.starts_with("msg/system/") {
        ContextCategoryKind::Instructions
    } else {
        ContextCategoryKind::SystemOther
    }
}

fn largest_message(message: &ModelMessage, estimated_tokens: usize) -> LargestMessage {
    let mut kinds: Vec<ContextCategoryKind> = Vec::new();
    for block in &message.content {
        let kind = kind_for(message, block);
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds.sort_by_key(|kind| kind.index());

    LargestMessage {
        message_uid: message.message_uid.as_str().to_string(),
        role: message.role,
        kinds,
        estimated_tokens,
        preview: preview(message),
    }
}

/// The first block, collapsed to one line and cut to [`PREVIEW_CHARS`].
///
/// The preview is for a person reading the report, never for replay: the
/// durable message is the whole one.
fn preview(message: &ModelMessage) -> String {
    let raw = match message.content.first() {
        Some(ContentBlock::Text { text }) => text.clone(),
        Some(ContentBlock::ToolUse { tool_name, arguments, .. }) => {
            format!("{tool_name}({arguments})")
        }
        Some(ContentBlock::ToolResult { content, .. }) => content.clone(),
        None => String::new(),
    };
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let mut preview: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use agent_protocol::ids::{RunId, StepId, TurnId};
    use agent_protocol::model::ContentBlock;

    use super::*;

    fn usage(messages: &[ModelMessage]) -> ContextUsage {
        ContextUsage {
            dropped_messages: 0,
            estimated_tokens: messages.iter().map(estimate_message).sum(),
            ..ContextUsage::default()
        }
    }

    fn category(report: &ContextUsageReport, kind: ContextCategoryKind) -> &ContextCategoryUsage {
        report.categories.iter().find(|category| category.kind == kind).expect("every category")
    }

    fn mixed() -> Vec<ModelMessage> {
        let run = RunId::from_string("run-1");
        let turn = TurnId::from_string("turn-1");
        let step = StepId::from_string("step-1");
        vec![
            ModelMessage::system_for_run(&run, "You are an agent."),
            ModelMessage::context_notice_for_turn(&turn, "Context notice: 2 messages omitted."),
            ModelMessage::new(MessageRole::User, vec![ContentBlock::text("read the file")]),
            ModelMessage::new(
                MessageRole::Assistant,
                vec![
                    ContentBlock::text("Working on it."),
                    ContentBlock::tool_use("c1", "fs.read", serde_json::json!({ "path": "a.txt" })),
                ],
            ),
            ModelMessage::tool_for_call(&step, "c1", "fs.read", "file contents", false),
            ModelMessage::tool_for_call(&step, "c2", "fs.write", "permission denied", true),
        ]
    }

    #[test]
    fn the_categories_add_up_to_the_derivations_estimate() {
        let messages = mixed();
        let usage = usage(&messages);
        let report = report(&messages, &usage, 3);

        let total: usize = report.categories.iter().map(|category| category.estimated_tokens).sum();
        assert_eq!(total, usage.estimated_tokens);
    }

    #[test]
    fn instructions_and_the_notice_are_told_apart_by_uid() {
        let messages = mixed();
        let report = report(&messages, &usage(&messages), 3);

        assert_eq!(category(&report, ContextCategoryKind::Instructions).messages, 1);
        assert_eq!(category(&report, ContextCategoryKind::Notice).messages, 1);
        assert_eq!(category(&report, ContextCategoryKind::SystemOther).messages, 0);
    }

    #[test]
    fn failed_and_successful_tool_results_are_different_categories() {
        let messages = mixed();
        let report = report(&messages, &usage(&messages), 3);

        let ok = category(&report, ContextCategoryKind::ToolResultsOk);
        let failed = category(&report, ContextCategoryKind::ToolResultsFailed);
        assert_eq!(ok.messages, 1);
        assert_eq!(failed.messages, 1);
        assert!(ok.estimated_tokens > 0);
        assert!(failed.estimated_tokens > 0);
    }

    #[test]
    fn one_message_may_count_in_two_categories_but_its_blocks_are_exact() {
        let messages = mixed();
        let report = report(&messages, &usage(&messages), 3);

        // The assistant message carries text and a tool call.
        assert_eq!(category(&report, ContextCategoryKind::AssistantText).messages, 1);
        assert_eq!(category(&report, ContextCategoryKind::ToolCalls).messages, 1);
        assert_eq!(category(&report, ContextCategoryKind::ToolCalls).blocks, 1);
        assert_eq!(report.message_count, messages.len());
        assert_eq!(category(&report, ContextCategoryKind::Framing).messages, messages.len());
    }

    #[test]
    fn largest_messages_are_ranked_and_each_preview_is_bounded() {
        let messages = vec![
            ModelMessage::new(MessageRole::User, vec![ContentBlock::text("short")]),
            ModelMessage::new(MessageRole::Assistant, vec![ContentBlock::text("x".repeat(400))]),
        ];
        let report = report(&messages, &usage(&messages), 1);

        assert_eq!(report.largest_messages.len(), 1);
        assert_eq!(report.largest_messages[0].message_uid, messages[1].message_uid.as_str());
        assert!(report.largest_messages[0].preview.chars().count() <= PREVIEW_CHARS + 1);
        assert!(report.largest_messages[0].preview.ends_with('…'));
    }

    #[test]
    fn a_preview_cuts_characters_not_bytes() {
        let messages =
            vec![ModelMessage::new(MessageRole::User, vec![ContentBlock::text("爻".repeat(100))])];
        let report = report(&messages, &usage(&messages), 1);

        let preview = &report.largest_messages[0].preview;
        assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1);
        assert!(preview.starts_with("爻"));
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn the_budget_takes_the_reservation_out_before_measuring() {
        let messages = mixed();
        let mut usage = usage(&messages);
        usage.max_input_tokens = Some(1000);
        usage.reserved_output_tokens = Some(200);
        usage.estimated_tokens = 400;

        let report = report(&messages, &usage, 3);
        assert_eq!(report.budget_tokens, Some(800));
        assert_eq!(report.remaining_tokens, Some(400));
        assert_eq!(report.percent_used, Some(50));
    }

    #[test]
    fn no_window_means_no_percentage_and_no_remaining_room() {
        let messages = mixed();
        let report = report(&messages, &usage(&messages), 3);

        assert_eq!(report.budget_tokens, None);
        assert_eq!(report.remaining_tokens, None);
        assert_eq!(report.percent_used, None);
    }

    #[test]
    fn a_context_over_its_budget_has_no_remaining_room_but_still_reports_usage() {
        let messages = mixed();
        let mut usage = usage(&messages);
        usage.max_input_tokens = Some(100);
        usage.reserved_output_tokens = Some(0);
        usage.estimated_tokens = 250;

        let report = report(&messages, &usage, 3);
        assert_eq!(report.budget_tokens, Some(100));
        assert_eq!(report.remaining_tokens, Some(0));
        assert_eq!(report.percent_used, Some(250));
    }

    #[test]
    fn an_empty_context_reports_every_category_at_zero() {
        let report = report(&[], &ContextUsage::default(), 3);

        assert_eq!(report.message_count, 0);
        assert!(report.largest_messages.is_empty());
        assert_eq!(report.categories.len(), ContextCategoryKind::ALL.len());
        assert!(report.categories.iter().all(|category| category.estimated_tokens == 0));
    }
}
