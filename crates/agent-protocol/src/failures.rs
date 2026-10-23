//! A deterministic record of the calls that failed, kept across compactions.
//!
//! The step ledger keeps one line per call and, since `ledger/2`, a bounded
//! detail for a failure. That is enough for the next turn, but not for the
//! turns after it: a later compaction carries only the *tail* of the previous
//! ledger, so the oldest failure — usually the baseline failure a later
//! question is about — is the first thing cut. Real use on 2026-10-21 asked,
//! three compactions later, what the first test run's assertion expected and
//! got; the values were in the ledger after the first compaction and gone by
//! the second.
//!
//! So failures get their own record, built from the tool results themselves
//! like the ledger and rendered by a pure function of the transcript and a
//! version. The previous record is carried whole (its head kept, because the
//! earliest failures are the ones later compactions cannot see directly); new
//! failures are appended, and the record is bounded as a whole.

use std::collections::HashMap;

use crate::ledger::failure_detail;
use crate::model::{ContentBlock, ModelMessage};

/// Identifies the rendering rules. Bump it whenever the text shape changes.
pub const FAILURE_VIEW_VERSION: &str = "failures/1";

/// How much of the record may enter the request, in characters.
pub const MAX_FAILURE_CHARS: usize = 1_000;

/// How much carried text a previous record may contribute.
const MAX_CARRIED_CHARS: usize = 700;

/// How much of a call's arguments is kept.
const MAX_ARGUMENT_CHARS: usize = 160;

/// How much of the result's first line is kept.
const MAX_RESULT_CHARS: usize = 120;

/// The first line of every rendered record. Carrying a rendered record whole
/// would put this line inside the new one; [`carried_body`] strips it.
const HEADER_PREFIX: &str = "Recorded failures (";

/// The label above the failures a previous record contributed.
const CARRIED_SECTION: &str = "Earlier failures, carried from the previous record:";

/// The label above the failures this compaction removed.
const THIS_SECTION: &str = "Failures in this compaction:";

/// One failed call and what its result said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureEntry {
    pub tool: String,
    /// Compact JSON, already bounded.
    pub arguments: String,
    /// The first non-empty line of the result, already bounded.
    pub result: String,
    /// The lines that name the failure, already bounded, when there are any.
    pub detail: Option<String>,
}

/// The failures recorded by one compaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Failures {
    /// The record this compaction replaces, carried forward from where it
    /// started (bounded). A later compaction cannot see the earliest failures
    /// any other way.
    pub carried: Option<String>,
    /// Whether a carried record had to be dropped entirely to meet the bound.
    pub carried_elided: bool,
    /// The failed calls this compaction removed.
    pub entries: Vec<FailureEntry>,
    /// Entries dropped from the front because the text passed its bound.
    pub elided: usize,
}

impl Failures {
    /// Collect the failed calls among `messages` and carry the previous record.
    ///
    /// Calls and results are paired by the provider's call id, the same key the
    /// ledger uses. A result with no call is skipped, and a call with no result
    /// is not a failure — nothing answered it, so nothing said it failed.
    pub fn build(messages: &[ModelMessage]) -> Self {
        let mut slots: Vec<Option<FailureEntry>> = Vec::new();
        let mut answered: Vec<bool> = Vec::new();
        let mut pending: HashMap<String, usize> = HashMap::new();
        let mut carried_parts: Vec<String> = Vec::new();

        for message in messages {
            if message.is_failures() {
                let body = carried_body(&message.text_content());
                if !body.is_empty() {
                    carried_parts.push(body);
                }
                continue;
            }
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { tool_call_id, tool_name, arguments } => {
                        pending.insert(tool_call_id.clone(), slots.len());
                        answered.push(false);
                        slots.push(Some(FailureEntry {
                            tool: tool_name.clone(),
                            arguments: bounded(
                                &serde_json::to_string(arguments).unwrap_or_default(),
                                MAX_ARGUMENT_CHARS,
                            ),
                            result: String::new(),
                            detail: None,
                        }));
                    }
                    ContentBlock::ToolResult { tool_call_id, tool_name, content, is_error } => {
                        let Some(index) = pending.remove(tool_call_id) else {
                            continue;
                        };
                        if !*is_error {
                            slots[index] = None;
                            continue;
                        }
                        answered[index] = true;
                        let first = content
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .unwrap_or("no output");
                        if let Some(entry) = slots[index].as_mut() {
                            entry.tool = tool_name.clone();
                            entry.result = bounded(first, MAX_RESULT_CHARS);
                            entry.detail = failure_detail(content, first);
                        }
                    }
                    ContentBlock::Text { .. } => {}
                }
            }
        }

        let entries = slots
            .into_iter()
            .zip(answered)
            .filter_map(|(slot, answered)| if answered { slot } else { None })
            .collect();
        let carried = if carried_parts.is_empty() {
            None
        } else {
            Some(head(&carried_parts.join("\n"), MAX_CARRIED_CHARS))
        };
        let mut failures = Self { carried, carried_elided: false, entries, elided: 0 };
        failures.bound();
        failures
    }

    /// Whether the record has nothing to say.
    pub fn is_empty(&self) -> bool {
        self.carried.is_none() && self.entries.is_empty()
    }

    /// The failed calls this compaction added.
    pub fn entries(&self) -> &[FailureEntry] {
        &self.entries
    }

    /// Whether the record carries an earlier one.
    pub fn carried(&self) -> bool {
        self.carried.is_some()
    }

    /// The text the model sees. Deterministic for a given record value.
    pub fn render(&self) -> String {
        let mut out = format!(
            "Recorded failures ({FAILURE_VIEW_VERSION}; copied from the tool results, not \
             model-written):"
        );
        if let Some(carried) = &self.carried {
            out.push('\n');
            out.push_str(CARRIED_SECTION);
            out.push('\n');
            out.push_str(carried);
        }
        if self.carried_elided {
            out.push_str("\nAn earlier failure record was omitted to stay within the bound.");
        }
        if !self.entries.is_empty() {
            out.push('\n');
            out.push_str(THIS_SECTION);
            for entry in &self.entries {
                out.push('\n');
                out.push_str(&format!(
                    "- {} {} -> failed: {}",
                    entry.tool, entry.arguments, entry.result
                ));
                if let Some(detail) = &entry.detail {
                    out.push_str("\n  detail: ");
                    out.push_str(detail);
                }
            }
        }
        if self.elided > 0 {
            out.push('\n');
            out.push_str(&format!(
                "({} earlier failure(s) left out to stay within the bound.)",
                self.elided
            ));
        }
        out
    }

    /// Drop oldest entries, then the carried record, until the text fits.
    fn bound(&mut self) {
        while self.render().chars().count() > MAX_FAILURE_CHARS {
            if !self.entries.is_empty() {
                self.entries.remove(0);
                self.elided += 1;
            } else if self.carried.is_some() {
                self.carried = None;
                self.carried_elided = true;
            } else {
                break;
            }
        }
    }
}

/// The failures of a previous record, with that record's own structure removed.
fn carried_body(text: &str) -> String {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let structural = line.is_empty()
                || line.starts_with(HEADER_PREFIX)
                || line == THIS_SECTION
                || line == CARRIED_SECTION;
            (!structural).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first `max_chars` characters of `text`, without splitting a character.
///
/// The head is kept rather than the tail: this record exists for the failures
/// a later compaction would otherwise never see, and those are the oldest.
fn head(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}... (this earlier part was truncated)", &text[..end]),
        None => text.to_string(),
    }
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
    use crate::model::MessageRole;

    fn call(id: &str, tool: &str, arguments: serde_json::Value) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::ToolUse {
                tool_call_id: id.to_string(),
                tool_name: tool.to_string(),
                arguments,
            }],
        )
    }

    fn result(id: &str, tool: &str, content: &str, is_error: bool) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Tool,
            vec![ContentBlock::ToolResult {
                tool_call_id: id.to_string(),
                tool_name: tool.to_string(),
                content: content.to_string(),
                is_error,
            }],
        )
    }

    #[test]
    fn a_failed_call_is_recorded_with_the_lines_that_name_the_failure() {
        let content = "exit code 101\n\
                       test a ... FAILED\n\
                       thread 'a' panicked at src/spec.rs:4:5:\n\
                       assertion `left == right` failed: expected Ok(111), got Ok(74)";
        let messages = vec![
            call("1", "shell.run", serde_json::json!({ "command": "scripts\\test.cmd" })),
            result("1", "shell.run", content, true),
        ];
        let failures = Failures::build(&messages);
        assert_eq!(failures.entries.len(), 1);
        let entry = &failures.entries[0];
        assert_eq!(entry.tool, "shell.run");
        assert_eq!(entry.result, "exit code 101");
        let detail = entry.detail.as_deref().expect("the assertion's own lines");
        assert!(detail.contains("expected Ok(111), got Ok(74)"), "{detail}");
        let text = failures.render();
        assert!(text.contains("Recorded failures (failures/1"), "{text}");
        assert!(text.contains("expected Ok(111), got Ok(74)"), "{text}");
    }

    #[test]
    fn a_successful_call_is_not_recorded() {
        let messages = vec![
            call("1", "shell.run", serde_json::json!({ "command": "true" })),
            result("1", "shell.run", "exit code 0", false),
        ];
        assert!(Failures::build(&messages).is_empty());
    }

    #[test]
    fn a_call_without_a_result_is_not_a_failure() {
        let messages = vec![call("1", "fs.read", serde_json::json!({ "path": "a" }))];
        assert!(Failures::build(&messages).is_empty());
    }

    #[test]
    fn the_previous_record_is_carried_whole() {
        let earlier = ModelMessage::failures_for(
            &crate::ids::AttemptId::from_string("a1"),
            "Recorded failures (failures/1; copied):\n\
             Failures in this compaction:\n\
             - shell.run {\"command\":\"x\"} -> failed: exit code 1",
        );
        let messages = vec![
            earlier,
            call("1", "fs.read", serde_json::json!({ "path": "missing" })),
            result("1", "fs.read", "no such file: missing", true),
        ];
        let failures = Failures::build(&messages);
        assert!(failures.carried());
        let text = failures.render();
        assert!(text.contains("Earlier failures, carried from the previous record:"), "{text}");
        assert!(text.contains("shell.run {\"command\":\"x\"}"), "{text}");
        assert!(text.contains("no such file: missing"), "{text}");
    }

    #[test]
    fn an_older_failure_survives_new_ones_up_to_the_bound() {
        let mut messages = Vec::new();
        for index in 0..40 {
            messages.push(call(
                &format!("c{index}"),
                "fs.read",
                serde_json::json!({ "path": "p" }),
            ));
            messages.push(result(
                &format!("c{index}"),
                "fs.read",
                &format!("error {index}: cannot read p"),
                true,
            ));
        }
        let failures = Failures::build(&messages);
        let text = failures.render();
        assert!(text.chars().count() <= MAX_FAILURE_CHARS, "{}", text.chars().count());
        assert!(text.contains("left out"), "{text}");
    }

    #[test]
    fn the_rendering_is_deterministic() {
        let messages = vec![
            call("1", "shell.run", serde_json::json!({ "command": "x" })),
            result("1", "shell.run", "exit code 1\nerror: boom", true),
        ];
        assert_eq!(Failures::build(&messages).render(), Failures::build(&messages).render());
    }
}
