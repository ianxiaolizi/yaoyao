//! Fitting settled history into an estimated token window.
//!
//! The estimate is deliberate rather than exact: no tokenizer dependency, no
//! network, no model-specific table. ASCII characters are counted at the
//! prose rate measured on the project's endpoint (0.40 tokens each), CJK
//! characters at 0.59, other non-ASCII at 1, with a fixed per-message frame.
//! The weights come from calibration, not a tokenizer: a real provider will
//! still disagree at the margins, which is why every request records the
//! provider's own usage beside the estimate and why the policy is off unless a
//! configuration turns it on.
//!
//! Trimming keeps a suffix of the history, newest first, and never splits a
//! tool call from its result. A group is an atomic unit: one user message, one
//! text-only assistant message, or an assistant message proposing tool calls
//! together with the messages carrying their results. The newest group is kept
//! even when it alone exceeds the budget — an empty history would be a different
//! conversation, and a turn with no context is worse than one that overflows
//! once and is trimmed next turn.
//!
//! Leading system messages are not history: they say what the run is, are
//! placed in front of every request by the loop, and are never dropped. They
//! do count against the budget, because the window they compete for is the
//! same one — a prompt that is never trimmed would otherwise be invisible in
//! the estimate.

use agent_ports::context_engine::{CompactionCandidate, CompactionScope, ContextUsage};
use agent_protocol::compaction::{Group, groups, keep_from, leading_system_len};
use agent_protocol::model::ModelMessage;
use agent_protocol::profile::ResolvedContextBudget;

// The estimator lives in `agent-protocol` because the loop, which records a
// per-request estimate, may not depend on this crate. Re-exported here so the
// trimmer and the inspection report keep reading one implementation.
pub use agent_protocol::model::{FRAME_TOKENS, estimate_block, estimate_message, estimate_text};

// The atomic-group rule lives beside the estimator for the same reason: the
// loop applies it to its own in-flight traffic mid-turn. Re-exported so the
// trimmer's callers keep one name for it.
pub use agent_protocol::compaction::COMPACTION_KEEP_PERCENT;

/// The resolved budget history is fitted into.
///
/// Every field is a frozen number from one run's [`ResolvedContextBudget`]; the
/// policy does no arithmetic of its own. The old form asked the trimmer to
/// derive a usable window from `max_input_tokens - reserved_output_tokens` and
/// a percentage, which is how a 1M-token model came to be trimmed against 8,000
/// tokens and compacted at a share of that (CONTRACT §7 #138).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    /// The model's physical window, for display only: history is never fitted
    /// against this number.
    pub context_window_tokens: u32,
    /// The most estimated input tokens one request may carry.
    pub input_working_set_tokens: u32,
    /// The completion cap that leaves the window beside the working set.
    pub completion_cap_tokens: u32,
    /// The whole-request size at which older history is proposed for
    /// summarization.
    pub compaction_trigger_tokens: u32,
    /// How many of the newest tokens compaction keeps whole.
    pub retain_recent_tokens: u32,
}

impl ContextPolicy {
    /// The policy one resolved budget describes.
    pub fn resolved(budget: ResolvedContextBudget) -> Self {
        Self {
            context_window_tokens: budget.context_window_tokens,
            input_working_set_tokens: budget.input_working_set_tokens,
            completion_cap_tokens: budget.completion_cap_tokens,
            compaction_trigger_tokens: budget.compaction_trigger_tokens,
            retain_recent_tokens: budget.retain_recent_tokens,
        }
    }
}

/// The kept messages and what keeping them cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Trimmed {
    pub messages: Vec<ModelMessage>,
    pub usage: ContextUsage,
}

/// Apply `policy` to a settled history.
///
/// `None` returns the messages untouched and reports only the estimate: "no
/// window configured" must mean the behaviour that existed before windows did,
/// not a different one with a different default.
pub fn apply(messages: Vec<ModelMessage>, policy: Option<ContextPolicy>) -> Trimmed {
    let total: usize = messages.iter().map(estimate_message).sum();
    let Some(policy) = policy else {
        return Trimmed {
            messages,
            usage: ContextUsage { estimated_tokens: total, ..ContextUsage::default() },
        };
    };

    let budget = policy.input_working_set_tokens as usize;

    // Leading system messages that are not summaries stay unconditionally and
    // are charged first: they are the run's instructions, not conversation, and
    // every request carries them. A summary is history and may shrink like the
    // rest. Only what follows the preamble is a suffix that may shrink.
    let split = leading_system_len(&messages);
    let (preamble, history) = messages.split_at(split);
    let preamble_cost: usize = preamble.iter().map(estimate_message).sum();
    let groups = groups(history);

    // Walk the atomic groups newest to oldest, keeping whole groups. Orphan
    // groups (a tool result with no call anywhere before it) are never kept:
    // keeping one would hand the model a result it cannot attribute, which no
    // provider accepts.
    let mut chosen: Vec<&Group> = Vec::new();
    let mut used = preamble_cost;
    let mut first = true;
    for group in groups.iter().rev().filter(|group| group.keep) {
        let cost = group.cost(history);
        if first {
            // The newest group stays even over budget.
            first = false;
            used += cost;
            chosen.push(group);
            continue;
        }
        if used + cost > budget {
            break;
        }
        used += cost;
        chosen.push(group);
    }
    chosen.reverse();

    let mut kept: Vec<ModelMessage> = preamble.to_vec();
    kept.extend(chosen.iter().flat_map(|group| history[group.start..=group.end].iter().cloned()));
    let dropped_messages = messages.len() - kept.len();

    Trimmed {
        messages: kept,
        usage: ContextUsage {
            dropped_messages,
            estimated_tokens: used,
            input_working_set_tokens: Some(policy.input_working_set_tokens),
        },
    }
}

/// The oldest history the policy proposes summarizing, if any.
///
/// The newest group is never proposed: a summary of the only thing the model
/// just did is worse than carrying it whole.
///
/// The proposal is made over the whole derived view, not over what trimming
/// kept. When the window has already started dropping, the dropped messages
/// are still in the log and still in the projection's state; summarizing them
/// too is what makes the next fold smaller instead of merely as small.
///
/// `observed_input_tokens` is the provider's own input count for the most
/// recent request in this session, when it reported one. Real usage is the
/// trigger when it exists: the estimate covers only the messages the engine
/// derived, while a real request also carries the stable head, the per-turn
/// layers, and the tool definitions — on 2026-10-19 the estimate was about
/// half of the provider's count, so a threshold read off it alone fires late.
/// The estimate remains the fallback for a provider that reports no usage.
pub fn compaction_candidate(
    messages: &[ModelMessage],
    policy: &ContextPolicy,
    observed_input_tokens: Option<u32>,
) -> Option<CompactionCandidate> {
    let budget = policy.input_working_set_tokens as usize;
    let trigger = policy.compaction_trigger_tokens as usize;
    if budget == 0 || trigger == 0 {
        return None;
    }
    let estimated_total: usize = messages.iter().map(estimate_message).sum();
    let observed = observed_input_tokens.filter(|tokens| *tokens > 0);
    let measured = observed.map(|tokens| tokens as usize).unwrap_or(estimated_total);
    if measured < trigger {
        return None;
    }

    let split = leading_system_len(messages);
    let (preamble, history) = messages.split_at(split);
    let preamble_cost: usize = preamble.iter().map(estimate_message).sum();
    let history_groups = groups(history);

    // The newest group is kept whatever it costs; then keep whole groups going
    // backwards while they still fit the resolved retention target.
    let keep_target = policy.retain_recent_tokens as usize;
    let (keep_from, retained) = keep_from(history, &history_groups, preamble_cost, keep_target)?;

    let candidate: Vec<ModelMessage> = history_groups[..keep_from]
        .iter()
        .filter(|group| group.keep)
        .flat_map(|group| history[group.start..=group.end].iter().cloned())
        .collect();
    if candidate.is_empty() {
        return None;
    }
    Some(CompactionCandidate {
        estimated_tokens: candidate.iter().map(estimate_message).sum(),
        messages: candidate,
        retained_tokens: retained,
        budget_tokens: budget,
        trigger_tokens: policy.compaction_trigger_tokens,
        observed_input_tokens: observed,
        scope: CompactionScope::Settled,
    })
}

#[cfg(test)]
mod tests {
    use agent_protocol::model::{ContentBlock, MessageRole};

    use super::*;

    fn text(role: MessageRole, text: &str) -> ModelMessage {
        ModelMessage::new(role, vec![ContentBlock::text(text)])
    }

    fn user(text_: &str) -> ModelMessage {
        text(MessageRole::User, text_)
    }

    fn system(text_: &str) -> ModelMessage {
        text(MessageRole::System, text_)
    }

    fn assistant(text_: &str) -> ModelMessage {
        text(MessageRole::Assistant, text_)
    }

    fn call(id: &str) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::tool_use(id, "echo", serde_json::json!({ "x": "y" }))],
        )
    }

    fn result(id: &str) -> ModelMessage {
        ModelMessage::new(
            MessageRole::User,
            vec![ContentBlock::tool_result(id, "echo", "done", false)],
        )
    }

    fn uids(messages: &[ModelMessage]) -> Vec<&str> {
        messages.iter().map(|m| m.message_uid.as_str()).collect()
    }

    fn big() -> ModelMessage {
        user(&"a".repeat(400))
    }

    /// A resolved budget shaped like the derived DeepSeek one: a 1M window, a
    /// quarter of it as the working set, compaction at 80% of that, and the
    /// newest eighth of the window kept whole.
    fn deepseek_budget() -> ResolvedContextBudget {
        ResolvedContextBudget {
            context_window_tokens: 1_048_576,
            input_working_set_tokens: 262_144,
            completion_cap_tokens: 262_144,
            completion_cap_ceiling_tokens: 393_216,
            compaction_headroom_tokens: 65_536,
            compaction_trigger_tokens: 209_715,
            retain_recent_tokens: 125_829,
        }
    }

    /// A policy that fits history into `working_set` tokens, triggers at that
    /// same number, and keeps the newest half as recent history.
    fn fitted(working_set: u32) -> ContextPolicy {
        ContextPolicy {
            context_window_tokens: working_set.saturating_mul(4),
            input_working_set_tokens: working_set,
            completion_cap_tokens: working_set / 4,
            compaction_trigger_tokens: working_set,
            retain_recent_tokens: working_set / 2,
        }
    }

    /// A policy whose trigger and retention are stated directly, so a test can
    /// place the two boundaries where it wants them.
    fn compacting(trigger: u32, working_set: u32, retain: u32) -> ContextPolicy {
        ContextPolicy {
            context_window_tokens: working_set.saturating_mul(4),
            input_working_set_tokens: working_set,
            completion_cap_tokens: working_set / 4,
            compaction_trigger_tokens: trigger,
            retain_recent_tokens: retain,
        }
    }

    #[test]
    fn with_no_policy_the_history_is_untouched_and_only_measured() {
        let messages = vec![user("one"), assistant("two"), user("three")];
        let trimmed = apply(messages.clone(), None);
        assert_eq!(trimmed.messages, messages);
        assert_eq!(trimmed.usage.dropped_messages, 0);
        assert_eq!(trimmed.usage.input_working_set_tokens, None);
        assert!(trimmed.usage.estimated_tokens > 0);
        assert_eq!(
            trimmed.usage.estimated_tokens,
            messages.iter().map(estimate_message).sum::<usize>()
        );
    }

    /// The working set is the number history is fitted into. There is no
    /// reservation subtracted here any more: the resolver already took the
    /// completion cap out of the window when it chose the working set.
    #[test]
    fn history_is_fitted_into_the_resolved_working_set() {
        let messages = vec![user("aaaaaaaaaa"), user("bbbbbbbbbb")];
        let cost = estimate_message(&messages[1]);
        let trimmed = apply(messages.clone(), Some(fitted((cost + 1) as u32)));
        assert_eq!(uids(&trimmed.messages), vec![messages[1].message_uid.as_str()]);
        assert_eq!(trimmed.usage.input_working_set_tokens, Some((cost + 1) as u32));
    }

    #[test]
    fn the_oldest_messages_go_first_and_the_newest_stay() {
        let messages = vec![user("old"), assistant("older answer"), user("new")];
        let newest_cost = estimate_message(messages.last().unwrap());
        // Room for the newest message only.
        let trimmed = apply(messages.clone(), Some(fitted(10)));
        assert_eq!(uids(&trimmed.messages), vec![messages[2].message_uid.as_str()]);
        assert_eq!(trimmed.usage.dropped_messages, 2);
        assert_eq!(trimmed.usage.estimated_tokens, newest_cost);
    }

    #[test]
    fn a_tool_call_and_its_result_are_kept_or_dropped_together() {
        let messages = vec![user("ask"), call("c1"), result("c1"), assistant("answer")];
        let call_cost = estimate_message(&messages[1]) + estimate_message(&messages[2]);
        let answer_cost = estimate_message(&messages[3]);
        // Enough for the newest answer and the pair, but not for the opening
        // user message as well.
        let policy = fitted((call_cost + answer_cost + 1) as u32);
        let trimmed = apply(messages.clone(), Some(policy));
        assert_eq!(
            uids(&trimmed.messages),
            vec![
                messages[1].message_uid.as_str(),
                messages[2].message_uid.as_str(),
                messages[3].message_uid.as_str()
            ]
        );
        assert!(
            trimmed.messages.iter().any(|m| m.content.iter().any(|b| b.as_tool_use().is_some())),
            "a result was kept without its call"
        );
    }

    #[test]
    fn a_result_is_never_left_without_its_call() {
        // The budget fits the result + answer but not the call. Grouping must
        // drop the whole pair, and never emit the orphaned result.
        let messages = vec![user("ask"), call("c1"), result("c1"), assistant("answer")];
        let answer_cost = estimate_message(&messages[3]);
        let trimmed = apply(messages.clone(), Some(fitted((answer_cost + 1) as u32)));
        assert_eq!(uids(&trimmed.messages), vec![messages[3].message_uid.as_str()]);
    }

    #[test]
    fn an_orphan_result_anywhere_is_never_kept() {
        let messages = vec![user("ask"), result("ghost"), assistant("answer")];
        // Plenty of budget: the point is that the orphan is excluded by shape.
        let trimmed = apply(messages.clone(), Some(fitted(10_000)));
        assert_eq!(
            uids(&trimmed.messages),
            vec![messages[0].message_uid.as_str(), messages[2].message_uid.as_str()]
        );
        assert_eq!(trimmed.usage.dropped_messages, 1);
    }

    #[test]
    fn the_newest_group_stays_even_when_it_alone_overflows() {
        let messages = vec![user("old"), big()];
        let trimmed = apply(messages.clone(), Some(fitted(5)));
        assert_eq!(uids(&trimmed.messages), vec![messages[1].message_uid.as_str()]);
        assert!(trimmed.usage.estimated_tokens > 5);
    }

    #[test]
    fn the_estimate_is_deterministic_and_grows_with_the_text() {
        // Letters cost 0.18 and digits 0.92 per character, ideographs 0.60, so
        // five ASCII letters stay under one token while two ideographs cross
        // it. The weights are calibrated, not exact per character.
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_text("abcd"), 1);
        assert_eq!(estimate_text("abcde"), 1);
        assert_eq!(estimate_text("你好"), 2);
        assert_eq!(estimate_text("a你"), 1);
        assert!(estimate_text("hello") >= estimate_text("hi"));
        assert_eq!(estimate_text("same"), estimate_text("same"));
    }

    #[test]
    fn the_estimate_lands_within_the_measured_range_for_mixed_traffic() {
        // Live measurements against the project's endpoint: 277 provider
        // tokens for this 1,063-character mix of prose, code, tool-definition
        // JSON, and Chinese on 2026-10-19 (re-measured at 278 on 2026-10-20),
        // and 171 tokens for the 304-character Chinese sample (re-measured at
        // 172). The per-class weights were calibrated on a wider set the same
        // day; these two samples check that the older traffic still lands
        // within 15%, which is the tolerance that keeps the configured window
        // honest about what fits.
        let prose = "The context window is a working set, not a limit. Every request carries a stable head \
followed by the settled history, and only at a turn boundary does the layout change. \
When a tool returns a large result, the loop keeps its beginning and its end, writes the \
whole result to an artifact, and gives the model a pointer instead of the body. ";
        let code = "fn estimate_message(message: &ModelMessage) -> usize {\n    \
message.content.iter().map(estimate_block).sum::<usize>() + FRAME_TOKENS\n}\n\
pub fn estimate_text(text: &str) -> usize {\n    \
let units: u64 = text.chars().map(|ch| if ch.is_ascii() { UNITS_PER_ASCII_LETTER } else { UNITS_PER_IDEOGRAPH }).sum();\n    \
usize::try_from(units.div_ceil(UNITS_PER_TOKEN)).unwrap_or(usize::MAX)\n}\n";
        let json = r#"{"type":"function","function":{"name":"fs_edit","description":"Replace one exact occurrence of oldString with newString in a workspace file.","parameters":{"type":"object","properties":{"path":{"type":"string"},"oldString":{"type":"string"},"newString":{"type":"string"}},"required":["path","oldString","newString"]}}}"#;
        let chinese =
            "每一轮请求都以稳定的前缀开始，指令和能力说明在最前面，之后才是已经结束的历史。";
        let mixed = [prose, code, json, chinese].concat();
        let mixed_estimate = estimate_text(&mixed) as f64;
        let chinese_only = "每一轮请求都以稳定的前缀开始，指令和能力说明在最前面，之后才是已经结束的历史。\
只有在轮次边界才会重新组织上下文，轮内只追加、不改写，否则前缀缓存会失效。";
        let chinese_estimate = estimate_text(&chinese_only.repeat(4)) as f64;
        assert!((mixed_estimate - 277.0).abs() / 277.0 <= 0.15, "{mixed_estimate}");
        assert!((chinese_estimate - 171.0).abs() / 171.0 <= 0.15, "{chinese_estimate}");
    }

    #[test]
    fn a_message_costs_its_blocks_plus_one_frame() {
        let message = ModelMessage::new(
            MessageRole::User,
            vec![
                ContentBlock::text("hello"),
                ContentBlock::tool_use("call_1", "fs.read", serde_json::json!({ "path": "a" })),
            ],
        );
        let blocks: usize = message.content.iter().map(estimate_block).sum();
        assert_eq!(estimate_message(&message), blocks + FRAME_TOKENS);
    }

    #[test]
    fn the_leading_system_message_survives_a_budget_too_small_for_the_history() {
        let messages = vec![system("you are yaoyao"), user("ask"), assistant("answer")];
        let trimmed = apply(messages.clone(), Some(fitted(1)));
        // Whatever the budget, the run's instructions lead the request.
        assert_eq!(uids(&trimmed.messages)[0], messages[0].message_uid.as_str());
        assert_eq!(
            uids(&trimmed.messages)[1],
            messages[2].message_uid.as_str(),
            "the newest history group should follow the instructions"
        );
        assert_eq!(trimmed.usage.dropped_messages, 1);
        assert_eq!(
            trimmed.usage.estimated_tokens,
            estimate_message(&messages[0]) + estimate_message(&messages[2])
        );
    }

    #[test]
    fn the_system_message_is_charged_against_the_budget() {
        // Two equally sized history messages. The budget fits the instructions
        // plus exactly one of them; counting the instructions is what makes the
        // older one drop.
        let messages = vec![system(&"s".repeat(400)), big(), big()];
        let system_cost = estimate_message(&messages[0]);
        let message_cost = estimate_message(&messages[1]);
        let trimmed = apply(messages.clone(), Some(fitted((system_cost + message_cost) as u32)));
        assert_eq!(
            uids(&trimmed.messages),
            vec![messages[0].message_uid.as_str(), messages[2].message_uid.as_str()]
        );
        assert_eq!(trimmed.usage.estimated_tokens, system_cost + message_cost);
    }

    #[test]
    fn compaction_uses_the_resolved_trigger_not_a_percentage_of_the_old_reservation() {
        // The old rule compacted at a share of `max_input_tokens - reservation`
        // (75% of 8,000 on the shipped configuration). The resolved trigger is a
        // number of its own: a request of about 216,000 estimated tokens fires
        // it, and the working set the proposal reports is the resolved 262,144 —
        // neither is a percentage of anything.
        let policy = ContextPolicy::resolved(deepseek_budget());
        let messages = vec![user(&"a".repeat(1_200_000)), user("recent")];
        let candidate = compaction_candidate(&messages, &policy, None).expect("the trigger fired");
        assert_eq!(candidate.budget_tokens, 262_144);
        assert_eq!(candidate.trigger_tokens, 209_715);
        assert_eq!(candidate.scope, CompactionScope::Settled);
    }

    /// Retention keeps the newest whole groups, and never splits a call from
    /// its result, until the resolved retained budget is reached.
    #[test]
    fn retention_keeps_the_newest_balanced_groups() {
        let policy = ContextPolicy::resolved(deepseek_budget());
        let mut messages = Vec::new();
        for index in 0..40 {
            messages.push(user(&format!("ask {index}")));
            messages.push(call(&format!("c{index}")));
            messages.push(result(&format!("c{index}")));
            messages.push(assistant(&"a".repeat(80_000)));
        }
        let trimmed = apply(messages.clone(), Some(policy));
        assert!(trimmed.usage.estimated_tokens <= 262_144, "{}", trimmed.usage.estimated_tokens);
        assert!(trimmed.usage.dropped_messages > 0, "the history had to shrink");
        // Every kept result has its call before it and every kept call has its
        // result after it: the boundary never falls inside a pair.
        let kept = &trimmed.messages;
        for (index, message) in kept.iter().enumerate() {
            if let Some((id, _, _)) = message.content.iter().find_map(|b| b.as_tool_use()) {
                assert!(
                    kept.get(index + 1).is_some_and(|next| next
                        .content
                        .iter()
                        .any(|b| b.as_tool_result().is_some_and(|r| r.0 == id))),
                    "kept a call without its result"
                );
            }
            if let Some((id, _, _, _)) = message.content.iter().find_map(|b| b.as_tool_result()) {
                assert!(
                    kept[..index].iter().any(|before| before
                        .content
                        .iter()
                        .any(|b| b.as_tool_use().is_some_and(|c| c.0 == id))),
                    "kept a result without its call"
                );
            }
        }
    }

    #[test]
    fn a_zero_trigger_keeps_the_trimmer_as_the_only_protection() {
        let messages = vec![big(), big(), big()];
        let policy = compacting(0, 10, 5);
        assert!(compaction_candidate(&messages, &policy, None).is_none());
    }

    #[test]
    fn below_the_trigger_nothing_is_proposed() {
        // Two 76-token messages are 152 estimated tokens in all; the trigger is
        // 360, so the policy has not been reached.
        let messages = vec![big(), big()];
        assert!(compaction_candidate(&messages, &compacting(360, 400, 200), None).is_none());
    }

    #[test]
    fn real_usage_triggers_before_the_estimate_reaches_the_threshold() {
        // The estimate covers only the derived messages; a real request also
        // carries the stable head, the per-turn layers, and the tool
        // definitions. Here the messages estimate about 640 tokens, below the
        // 800-token trigger, but the provider says the last request was 900
        // input tokens. The user message alone exceeds the 500-token retention
        // target, so it is the part proposed for summarizing.
        let messages = vec![user(&"a".repeat(3500)), assistant("recent")];
        assert!(compaction_candidate(&messages, &compacting(800, 1000, 500), None).is_none());

        let candidate = compaction_candidate(&messages, &compacting(800, 1000, 500), Some(900))
            .expect("real usage reaches the trigger");
        assert_eq!(candidate.observed_input_tokens, Some(900));
        assert_eq!(candidate.scope, CompactionScope::Settled);
        // The split is still chosen on the estimate: only the trigger moves.
        assert_eq!(uids(&candidate.messages), vec![messages[0].message_uid.as_str()]);
    }

    #[test]
    fn a_measured_request_below_the_trigger_is_not_compacted_on_the_estimate() {
        // The reverse case: the estimate alone would fire, but the provider's
        // count says the request is small, so there is nothing to do — a
        // threshold read off the estimate is the one that can be wrong.
        let messages = vec![user(&"a".repeat(400)), assistant(&"b".repeat(400))];
        let policy = compacting(50, 100, 50);
        assert!(compaction_candidate(&messages, &policy, None).is_some());
        assert!(compaction_candidate(&messages, &policy, Some(10)).is_none());
    }

    #[test]
    fn the_oldest_groups_are_proposed_and_the_newest_stays() {
        // Three 80-token groups against a 300-token working set at a 150-token
        // trigger: the trigger is reached, and the 150-token retention fits the
        // newest group only, so the two oldest are summarized.
        let messages = vec![user(&"a".repeat(400)), user(&"b".repeat(400)), user(&"c".repeat(400))];
        let candidate =
            compaction_candidate(&messages, &compacting(150, 300, 150), None).expect("a proposal");
        assert_eq!(
            uids(&candidate.messages),
            vec![messages[0].message_uid.as_str(), messages[1].message_uid.as_str()]
        );
        assert_eq!(
            candidate.estimated_tokens,
            estimate_message(&messages[0]) + estimate_message(&messages[1])
        );
        assert_eq!(candidate.retained_tokens, estimate_message(&messages[2]));
        assert_eq!(candidate.budget_tokens, 300);
        assert_eq!(candidate.trigger_tokens, 150);
    }

    #[test]
    fn a_tool_call_and_its_result_are_proposed_together() {
        // The retention fits the newest answer only, so the call/result pair
        // and the user message before them are summarized as whole groups —
        // never a call without its result.
        let messages = vec![user("old"), call("c1"), result("c1"), assistant(&"a".repeat(600))];
        let candidate =
            compaction_candidate(&messages, &compacting(110, 220, 110), None).expect("a proposal");
        assert_eq!(
            uids(&candidate.messages),
            vec![
                messages[0].message_uid.as_str(),
                messages[1].message_uid.as_str(),
                messages[2].message_uid.as_str()
            ]
        );
        assert_eq!(candidate.retained_tokens, estimate_message(&messages[3]));
    }

    #[test]
    fn a_call_and_its_result_are_kept_together_on_the_other_side() {
        // Here the pair fits the retention and stays; only the user message
        // before it is proposed. The boundary still never falls between the
        // call and its result.
        let messages = vec![user(&"a".repeat(500)), call("c1"), result("c1"), assistant("done")];
        let candidate =
            compaction_candidate(&messages, &compacting(100, 200, 100), None).expect("a proposal");
        assert_eq!(uids(&candidate.messages), vec![messages[0].message_uid.as_str()]);
        assert!(
            candidate.messages.iter().all(|message| message.role != MessageRole::Assistant),
            "the call must not be summarized without its result"
        );
    }

    #[test]
    fn the_newest_group_is_never_proposed() {
        // One message over the trigger: there is nothing older to summarize.
        let messages = vec![big()];
        assert!(compaction_candidate(&messages, &compacting(50, 100, 50), None).is_none());
    }

    #[test]
    fn the_leading_system_message_is_never_proposed() {
        let messages = vec![system("you are yaoyao"), big(), big()];
        let candidate =
            compaction_candidate(&messages, &compacting(150, 300, 150), None).expect("a proposal");
        assert_eq!(uids(&candidate.messages), vec![messages[1].message_uid.as_str()]);
    }

    #[test]
    fn a_leading_summary_is_history_and_can_be_proposed() {
        // A summary is a system message, but it stands in for history. If the
        // preamble rule swallowed it, a second compaction could never name it,
        // and every summary would survive beside the next one.
        let instruction = system("you are yaoyao");
        let summary =
            ModelMessage::summary_for(&agent_protocol::ids::AttemptId::new(), "old summary");
        let messages = vec![instruction.clone(), summary.clone(), big(), big()];
        let candidate =
            compaction_candidate(&messages, &compacting(150, 300, 150), None).expect("a proposal");
        let proposed = uids(&candidate.messages);
        assert!(
            proposed.contains(&summary.message_uid.as_str()),
            "the old summary must be in the candidate: {proposed:?}"
        );
        assert!(
            !proposed.contains(&instruction.message_uid.as_str()),
            "the run's instructions stay out of it: {proposed:?}"
        );
    }
}
