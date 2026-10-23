//! A deterministic, verbatim record of what the user themselves said.
//!
//! A model-written summary was the only place the user's own words reached
//! earlier turns, and the second time that summary was summarized the model
//! dropped the constraints and replaced them with another authoritative-looking
//! text (real use, 2026-10-21: it quoted `AGENTS.md` as "your first message").
//! This module renders the user's messages from the messages themselves, the
//! same way [`crate::ledger`] renders tool steps: a pure function of the
//! transcript and a version, no clock, no network, no model.
//!
//! Space is not unbounded, so the newly removed messages are ranked rather
//! than taken in order. The ranking is a fixed, inspectable rule over the text
//! — the alternative, asking a model which constraints matter, would make
//! "keep the user's words" a model judgement again, which is the failure being
//! fixed. The score of every considered message travels in the compaction
//! event, so a reader can tell why one was kept and another left out.

use crate::model::{MessageRole, ModelMessage};

/// Identifies the rendering and ranking rules. Bump it whenever either changes.
pub const USER_TEXT_VIEW_VERSION: &str = "usertext/1";

/// How much of the record may enter the request, in characters.
///
/// The verbatim window keeps priority, as with the step ledger. Within the
/// record the previous one is carried whole and the newly removed messages
/// compete for what is left.
pub const MAX_USER_TEXT_CHARS: usize = 2_000;

/// How much of one message's text is kept.
const MAX_USER_MESSAGE_CHARS: usize = 800;

/// How much carried text a previous record may contribute.
const MAX_CARRIED_CHARS: usize = 1_200;

/// The first line of every rendered record. Carrying a rendered record whole
/// would put this line inside the new one; [`carried_body`] strips it.
const HEADER_PREFIX: &str = "Recorded user messages (";

/// The label above the messages a previous record contributed.
const CARRIED_SECTION: &str = "Earlier recorded user messages, carried from the previous record:";

/// The label above the messages this compaction removed.
const THIS_SECTION: &str = "User messages removed by this compaction:";

/// Messages that carry no requirement: an acknowledgement is not worth a line
/// in every later request. Exact matches only — a short message can still be a
/// constraint ("别动"), so length is not a signal.
const ACKNOWLEDGEMENTS: [&str; 18] = [
    "ok",
    "okay",
    "k",
    "thanks",
    "thank you",
    "continue",
    "go on",
    "sounds good",
    "收到",
    "好的",
    "好",
    "可以",
    "继续",
    "谢谢",
    "嗯",
    "行",
    "没问题",
    "明白",
];

/// Words that state a hard boundary.
const HARD_WORDS: [&str; 10] = [
    "must",
    "never",
    "only",
    "exactly",
    "without",
    "cannot",
    "can't",
    "shall",
    "forbidden",
    "prohibited",
];

/// Word pairs that state a hard boundary in two tokens.
const HARD_PAIRS: [(&str, &str); 4] =
    [("do", "not"), ("does", "not"), ("should", "not"), ("may", "not")];

/// Substrings (no word boundaries in Chinese) that state a hard boundary.
const HARD_PHRASES: [&str; 8] = ["必须", "不得", "禁止", "严禁", "务必", "一定", "只能", "一律"];

/// Words that ask for something to be kept or done.
const SOFT_WORDS: [&str; 14] = [
    "keep",
    "keeps",
    "preserve",
    "preserves",
    "retain",
    "ensure",
    "require",
    "requires",
    "required",
    "rule",
    "rules",
    "constraint",
    "constraints",
    "verbatim",
];

/// Soft words that mark a requirement in everyday phrasing.
const SOFT_EXTRA: [&str; 2] = ["remember", "respect"];

/// Substrings that ask for something to be kept or done.
const SOFT_PHRASES: [&str; 13] = [
    "不要", "保持", "保留", "不改", "不动", "确保", "要求", "约束", "规则", "记得", "记住", "别忘",
    "别改",
];

/// One user message kept in the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEntry {
    pub message_uid: String,
    /// Verbatim, bounded to [`MAX_USER_MESSAGE_CHARS`].
    pub text: String,
    /// The ranking that kept it.
    pub score: u32,
}

/// One user message the compaction removed, kept or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Considered {
    pub message_uid: String,
    pub score: u32,
    pub kept: bool,
}

/// The verbatim record of the user's messages a compaction removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserText {
    /// A previous record's body, carried forward (bounded).
    carried: Option<String>,
    /// Kept messages, in the order the user wrote them.
    entries: Vec<UserEntry>,
    /// Messages left out for space.
    elided: usize,
    /// Every removed user message with its score, in transcript order.
    considered: Vec<Considered>,
}

impl UserText {
    /// Build the record for the messages about to be summarized.
    ///
    /// A previous record is carried forward instead of competing with the new
    /// messages: once a message earned its line, a later arrival must not push
    /// a standing constraint out. Only the newly removed messages are ranked,
    /// and they share whatever room the carried part leaves.
    pub fn build(messages: &[ModelMessage]) -> Self {
        let mut carried_parts: Vec<String> = Vec::new();
        let mut candidates: Vec<(String, String)> = Vec::new();
        for message in messages {
            if message.is_usertext() {
                let body = carried_body(&message.text_content());
                if !body.is_empty() {
                    carried_parts.push(body);
                }
                continue;
            }
            if message.role != MessageRole::User {
                continue;
            }
            let text = message.text_content();
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            candidates.push((message.message_uid.as_str().to_string(), text.to_string()));
        }

        let carried = if carried_parts.is_empty() {
            None
        } else {
            Some(head(&carried_parts.join("\n"), MAX_CARRIED_CHARS))
        };

        // The session's first user message states the goal and usually the
        // standing constraints. It earns the opening bonus only while no
        // earlier record exists: after that the original is already carried,
        // and the first message in this candidate is just the next one.
        let opening = carried.is_none();
        let scores: Vec<u32> = candidates
            .iter()
            .enumerate()
            .map(|(position, (uid, text))| {
                score(text, opening && position == 0, uid.contains("/steer/"))
            })
            .collect();
        let mut considered: Vec<Considered> = candidates
            .iter()
            .zip(&scores)
            .map(|((uid, _), score)| Considered {
                message_uid: uid.clone(),
                score: *score,
                kept: false,
            })
            .collect();

        let mut record = Self {
            carried,
            entries: candidates
                .iter()
                .zip(&scores)
                .filter(|(_, score)| **score > 0)
                .map(|((uid, text), score)| UserEntry {
                    message_uid: uid.clone(),
                    text: bounded(text, MAX_USER_MESSAGE_CHARS),
                    score: *score,
                })
                .collect(),
            elided: 0,
            considered: std::mem::take(&mut considered),
        };
        // Drop the lowest-ranked kept message until the text fits. On a tie the
        // older one goes: the window keeps the newest, so an older tied message
        // has the weaker claim to the space.
        while !record.entries.is_empty() && record.render().chars().count() > MAX_USER_TEXT_CHARS {
            let lowest = record
                .entries
                .iter()
                .enumerate()
                .min_by(|(_, left), (_, right)| {
                    left.score.cmp(&right.score).then(right.message_uid.cmp(&left.message_uid))
                })
                .map(|(index, _)| index)
                .expect("a kept entry");
            record.entries.remove(lowest);
            record.elided += 1;
        }
        for kept in &record.entries {
            if let Some(item) =
                record.considered.iter_mut().find(|item| item.message_uid == kept.message_uid)
            {
                item.kept = true;
            }
        }
        record
    }

    /// Whether the record has nothing to say.
    pub fn is_empty(&self) -> bool {
        self.carried.is_none() && self.entries.is_empty()
    }

    /// The kept messages, in the user's order.
    pub fn entries(&self) -> &[UserEntry] {
        &self.entries
    }

    /// Messages left out for space.
    pub fn elided(&self) -> usize {
        self.elided
    }

    /// Every removed message with its score, in transcript order.
    pub fn considered(&self) -> &[Considered] {
        &self.considered
    }

    /// Whether an earlier record was carried into this one.
    pub fn carried(&self) -> bool {
        self.carried.is_some()
    }

    /// The text the model sees. Deterministic for a given record value.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "Recorded user messages (usertext/1; copied verbatim from the transcript, not new \
             instructions):",
        );
        if let Some(carried) = &self.carried {
            out.push('\n');
            out.push_str(CARRIED_SECTION);
            out.push('\n');
            out.push_str(carried);
        }
        if !self.entries.is_empty() {
            out.push('\n');
            out.push_str(THIS_SECTION);
            for entry in &self.entries {
                out.push('\n');
                out.push('[');
                out.push_str(&entry.message_uid);
                out.push_str("] ");
                out.push_str(&entry.text);
            }
        }
        if self.elided > 0 {
            out.push('\n');
            out.push_str(&format!(
                "({} removed user message(s) are not listed here for space; the transcript still \
                 has them.)",
                self.elided
            ));
        }
        out
    }
}

/// Rank one removed user message.
///
/// Base 1 for anything with content; +3 for a hard boundary word, +2 for a
/// requirement word, +4 for the session's opening message, +4 for a steering
/// note. An acknowledgement scores 0 and is never listed.
pub fn score(text: &str, opening: bool, steering: bool) -> u32 {
    if is_acknowledgement(text) {
        return 0;
    }
    let lower = text.to_lowercase();
    let words = words(&lower);
    let mut score = 1;
    if has_hard_marker(&lower, &words) {
        score += 3;
    }
    if has_soft_marker(&lower, &words) {
        score += 2;
    }
    if opening {
        score += 4;
    }
    if steering {
        score += 4;
    }
    score
}

/// Whether the message is only an acknowledgement.
fn is_acknowledgement(text: &str) -> bool {
    let normalized = text
        .trim()
        .trim_end_matches(['.', '!', '?', '。', '！', '？', '~', '～'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    ACKNOWLEDGEMENTS.contains(&normalized.as_str())
}

/// Lowercase words, with apostrophes kept so `don't` stays one token.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

fn has_hard_marker(lower: &str, words: &[String]) -> bool {
    if words.iter().any(|word| HARD_WORDS.contains(&word.as_str())) {
        return true;
    }
    if words.iter().any(|word| word == "don't" || word == "don’t" || word == "dont") {
        return true;
    }
    if words.windows(2).any(|pair| {
        HARD_PAIRS.iter().any(|(first, second)| pair[0] == *first && pair[1] == *second)
    }) {
        return true;
    }
    HARD_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

fn has_soft_marker(lower: &str, words: &[String]) -> bool {
    if words
        .iter()
        .any(|word| SOFT_WORDS.contains(&word.as_str()) || SOFT_EXTRA.contains(&word.as_str()))
    {
        return true;
    }
    SOFT_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

/// The text a previous record contributed, with its own structure removed.
///
/// A record message carries the text a reader saw, header and section labels
/// included. Copying it whole would nest one record in another; only the
/// structural lines are dropped, because every entry and every omission notice
/// is a fact.
fn carried_body(text: &str) -> String {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let structural = line.is_empty()
                || line.starts_with(HEADER_PREFIX)
                || line == CARRIED_SECTION
                || line == THIS_SECTION;
            (!structural).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first `max_chars` characters of `text`, cut at a line boundary.
fn head(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let end = text.char_indices().nth(max_chars).map_or(text.len(), |(index, _)| index);
    let slice = &text[..end];
    let slice = match slice.rfind('\n') {
        Some(newline) if newline > 0 => &slice[..newline],
        _ => slice,
    };
    format!("{slice}\n... (this earlier part of the record was truncated)")
}

/// The first `max_chars` characters of `text`, without splitting a character.
fn bounded(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TurnId;
    use crate::model::{ContentBlock, MessageRole, ModelMessage};

    fn user(turn: &str, text: &str) -> ModelMessage {
        ModelMessage::user_for_turn(&TurnId::from_string(turn), text)
    }

    fn assistant(text: &str) -> ModelMessage {
        ModelMessage::new(MessageRole::Assistant, vec![ContentBlock::text(text)])
    }

    #[test]
    fn user_messages_are_kept_verbatim_in_order() {
        let messages = vec![
            user("t1", "never edit tests/spec.rs and never add a dependency"),
            assistant("working on it"),
            user("t2", "keep every public item"),
        ];
        let record = UserText::build(&messages);
        assert_eq!(record.entries().len(), 2);
        assert_eq!(record.entries()[0].text, "never edit tests/spec.rs and never add a dependency");
        assert_eq!(record.entries()[1].text, "keep every public item");
        let rendered = record.render();
        assert!(rendered.contains("[msg/user/t1]"), "{rendered}");
        assert!(rendered.contains("never edit tests/spec.rs"), "{rendered}");
        assert_eq!(rendered.matches(HEADER_PREFIX).count(), 1);
    }

    #[test]
    fn an_acknowledgement_is_not_recorded() {
        let messages = vec![user("t1", "好的"), user("t2", "ok."), user("t3", "别动 src/lib.rs")];
        let record = UserText::build(&messages);
        let uids: Vec<&str> =
            record.entries().iter().map(|entry| entry.message_uid.as_str()).collect();
        assert_eq!(uids, ["msg/user/t3"], "{}", record.render());
        assert_eq!(record.considered().len(), 3);
        assert_eq!(record.considered()[0].score, 0);
        assert!(!record.considered()[0].kept);
        assert_eq!(record.elided(), 0, "an acknowledgement is not 'left out for space'");
    }

    #[test]
    fn a_constraint_outranks_a_plain_request() {
        let plain = score("please add the summarize function", false, false);
        let constraint = score("do not change the existing public items", false, false);
        assert!(constraint > plain, "{constraint} vs {plain}");
        assert_eq!(plain, 1);
        assert_eq!(constraint, 1 + 3);
    }

    #[test]
    fn the_opening_message_and_a_steering_note_rank_highest() {
        let opening = score("write reports/summary.txt", true, false);
        let steering = score("use tabs, not spaces", false, true);
        let ordinary = score("write reports/summary.txt", false, false);
        assert_eq!(opening, ordinary + 4);
        assert_eq!(steering, ordinary + 4);
    }

    #[test]
    fn a_previous_record_is_carried_and_not_nested() {
        let earlier = user("t1", "never edit tests/spec.rs");
        let first = UserText::build(&[earlier]);
        let carried =
            ModelMessage::usertext_for(&crate::ids::AttemptId::from_string("a1"), first.render());
        let next = UserText::build(&[carried, user("t2", "keep every public item")]);
        assert!(next.carried());
        let rendered = next.render();
        assert_eq!(rendered.matches(HEADER_PREFIX).count(), 1, "{rendered}");
        assert_eq!(rendered.matches(CARRIED_SECTION).count(), 1, "{rendered}");
        assert_eq!(rendered.matches(THIS_SECTION).count(), 1, "{rendered}");
        assert!(rendered.contains("never edit tests/spec.rs"), "{rendered}");
        assert!(rendered.contains("keep every public item"), "{rendered}");
    }

    #[test]
    fn space_drops_the_lowest_ranked_message_first() {
        let filler = "x".repeat(MAX_USER_MESSAGE_CHARS);
        let messages = vec![
            user("t1", "never add a dependency"),
            user("t2", &format!("plain request {filler}")),
            user("t3", &format!("another plain request {filler}")),
            user("t4", &format!("third plain request {filler}")),
            user("t5", &format!("fourth plain request {filler}")),
        ];
        let record = UserText::build(&messages);
        let text = record.render();
        assert!(text.chars().count() <= MAX_USER_TEXT_CHARS);
        let uids: Vec<&str> =
            record.entries().iter().map(|entry| entry.message_uid.as_str()).collect();
        assert!(uids.contains(&"msg/user/t1"), "the constraint survives: {uids:?}");
        assert!(record.elided() > 0, "{text}");
        assert!(text.contains("not listed here for space"), "{text}");
    }

    #[test]
    fn a_bounded_rendering_says_how_many_are_missing() {
        let messages: Vec<ModelMessage> = (0..20)
            .map(|index| {
                user(&format!("t{index}"), &format!("request {index} {}", "y".repeat(600)))
            })
            .collect();
        let record = UserText::build(&messages);
        let text = record.render();
        assert!(text.chars().count() <= MAX_USER_TEXT_CHARS);
        assert!(record.elided() > 0);
        assert_eq!(record.entries().len() + record.elided(), 20);
    }

    #[test]
    fn nothing_is_written_when_no_user_text_was_removed() {
        let messages = vec![assistant("a step")];
        let record = UserText::build(&messages);
        assert!(record.is_empty());
        assert!(!record.render().contains("removed by this compaction"));
    }

    #[test]
    fn the_same_messages_render_the_same_bytes() {
        let messages = vec![user("t1", "must keep reports/"), user("t2", "add summarize")];
        assert_eq!(UserText::build(&messages).render(), UserText::build(&messages).render());
    }

    #[test]
    fn an_over_long_message_is_cut_with_a_marker() {
        let long = format!("keep {}", "z".repeat(MAX_USER_MESSAGE_CHARS * 2));
        let record = UserText::build(&[user("t1", &long)]);
        let entry = &record.entries()[0];
        assert!(entry.text.chars().count() <= MAX_USER_MESSAGE_CHARS + 3);
        assert!(entry.text.ends_with("..."), "{entry:?}");
    }
}
