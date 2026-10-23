//! A deterministic directory of the steps a compaction summarizes.
//!
//! A model-written summary is lossy, and the part it drops first is the
//! directory of what actually ran: which tool, with which arguments, to what
//! result. This module renders that directory from the messages themselves.
//! It is a pure function of the transcript and a version — no clock, no
//! network, no model — so the same messages under the same version always
//! yield the same text, and the ledger cannot invent a step that did not
//! happen.
//!
//! The version travels with every recorded ledger: a reader comparing two logs
//! can tell whether a difference is the transcript or the rendering rules.
//!
//! A failed call keeps a bounded detail drawn from its result — the lines that
//! name the failure — because the first line alone ("exit code 101") is not
//! what the next turn needs. The detail is extracted by fixed markers, not by a
//! model: a model-written summary can drop an assertion's expected value, and
//! did (2026-10-21 real use, three runs).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::{ContentBlock, ModelMessage};

/// Identifies the rendering rules. Bump it whenever the text shape changes.
pub const LEDGER_VIEW_VERSION: &str = "ledger/2";

/// How much of one ledger may enter the request, in characters.
///
/// The verbatim window keeps priority. A ledger past this bound drops its
/// oldest lines first and records how many it dropped, so the model knows the
/// directory is not the whole story.
pub const MAX_LEDGER_CHARS: usize = 2_000;

/// How much of a line's arguments is kept.
const MAX_ARGUMENT_CHARS: usize = 160;

/// How much of a result's first line is kept.
const MAX_RESULT_CHARS: usize = 120;

/// How much of a failed result's informative lines is kept, in characters.
const MAX_DETAIL_CHARS: usize = 400;

/// The words that mark a line of a failed result as worth keeping, by rank.
///
/// Fixed and lowercase: the same result always yields the same detail, and a
/// line that names the failure is kept without a model deciding it matters.
/// The ranks matter when the detail's bound is reached: a test run lists the
/// failed test names before the assertion that failed, and keeping the names
/// first spent the whole bound before the expected value appeared
/// (2026-10-21 real use). A line in a lower rank never displaces one above it.
const FAILURE_MARKER_TIERS: &[&[&str]] = &[
    &["panicked", "assert", "left:", "right:", "expected"],
    &["error", "fatal", "cannot", "not found", "os error", "denied", "refused"],
    &["failed", "failure"],
];

/// How many carried-forward characters a previous ledger may contribute.
const MAX_CARRIED_CHARS: usize = 800;

/// How many distinct written files the ledger lists.
const MAX_FILES: usize = 12;

/// The first line of every rendered ledger. Carrying a rendered ledger whole
/// would put this line inside the new one; [`carried_body`] strips it.
const HEADER_PREFIX: &str = "Recorded step ledger (";

/// The label above this compaction's steps.
const THIS_SECTION: &str = "Steps in this compaction:";

/// The label above the steps a previous ledger contributed.
const CARRIED_SECTION: &str = "Earlier steps, carried from the previous ledger:";

/// One call and the result that answered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerLine {
    pub tool: String,
    /// Compact JSON, already bounded.
    pub arguments: String,
    /// The first non-empty line of the result, already bounded.
    pub result: String,
    /// For a failed result, the lines that name the failure, already bounded.
    ///
    /// Present only when the result's first line alone does not say what went
    /// wrong — a test run's first line is its exit code, while the assertion
    /// that failed is several lines below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub failed: bool,
}

/// The directory of steps summarized by one compaction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// The ledger this compaction replaces, carried forward verbatim (bounded).
    /// Carrying it keeps an earlier compaction's steps auditable after a later
    /// one would otherwise have summarized only the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried: Option<String>,
    /// Whether a carried ledger had to be dropped entirely to meet the bound.
    #[serde(default)]
    pub carried_elided: bool,
    #[serde(default)]
    pub lines: Vec<LedgerLine>,
    /// Files a summarized write call changed, newest last.
    #[serde(default)]
    pub files: Vec<String>,
    /// Lines dropped from the front because the text passed its bound.
    #[serde(default)]
    pub elided: usize,
}

impl Ledger {
    /// Build the ledger for the messages about to be summarized.
    ///
    /// Calls and results are paired by the provider's call id, the same key the
    /// trimmer uses to keep them one group. An orphan result has no call to
    /// attribute it to and is skipped; a call without a result says so rather
    /// than disappearing.
    ///
    /// `write_tools` is this turn's manifest: tools declared with a `Write`
    /// side effect. A tool that is no longer offered falls back to the
    /// workspace's `.write`/`.edit` naming convention, the only evidence the
    /// log keeps.
    pub fn build(messages: &[ModelMessage], write_tools: &[String]) -> Self {
        let mut lines: Vec<LedgerLine> = Vec::new();
        let mut paths: Vec<Option<String>> = Vec::new();
        let mut pending: HashMap<String, usize> = HashMap::new();
        let mut carried_parts: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();

        for message in messages {
            if message.is_ledger() {
                let body = carried_body(&message.text_content());
                if !body.is_empty() {
                    carried_parts.push(body);
                }
                continue;
            }
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { tool_call_id, tool_name, arguments } => {
                        pending.insert(tool_call_id.clone(), lines.len());
                        lines.push(LedgerLine {
                            tool: tool_name.clone(),
                            arguments: bounded(
                                &serde_json::to_string(arguments).unwrap_or_default(),
                                MAX_ARGUMENT_CHARS,
                            ),
                            result: String::from("no result recorded"),
                            detail: None,
                            failed: false,
                        });
                        paths.push(path_argument(arguments).map(str::to_string));
                    }
                    ContentBlock::ToolResult { tool_call_id, tool_name, content, is_error } => {
                        let Some(index) = pending.remove(tool_call_id) else {
                            continue;
                        };
                        let first = content
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .unwrap_or("no output");
                        lines[index].tool = tool_name.clone();
                        lines[index].result = bounded(first, MAX_RESULT_CHARS);
                        lines[index].detail =
                            if *is_error { failure_detail(content, first) } else { None };
                        lines[index].failed = *is_error;
                        if !*is_error
                            && is_write_tool(&lines[index].tool, write_tools)
                            && let Some(path) = paths[index].take()
                            && !files.iter().any(|known| known == &path)
                        {
                            files.push(path);
                        }
                    }
                    ContentBlock::Text { .. } => {}
                }
            }
        }

        if files.len() > MAX_FILES {
            files.drain(..files.len() - MAX_FILES);
        }
        let carried = if carried_parts.is_empty() {
            None
        } else {
            Some(tail(&carried_parts.join("\n"), MAX_CARRIED_CHARS))
        };
        let mut ledger = Self { carried, carried_elided: false, lines, files, elided: 0 };
        ledger.bound();
        ledger
    }

    /// Whether the ledger has nothing to say.
    pub fn is_empty(&self) -> bool {
        self.carried.is_none() && self.lines.is_empty() && self.files.is_empty()
    }

    /// The text the model sees. Deterministic for a given ledger value.
    pub fn render(&self) -> String {
        let mut out = format!(
            "Recorded step ledger ({LEDGER_VIEW_VERSION}; generated from the messages, not \
             model-written):"
        );
        if let Some(carried) = &self.carried {
            out.push('\n');
            out.push_str(CARRIED_SECTION);
            out.push('\n');
            out.push_str(carried);
        }
        if self.carried_elided {
            out.push_str("\nAn earlier ledger was omitted to stay within the ledger bound.");
        }
        if !self.lines.is_empty() {
            out.push('\n');
            out.push_str(THIS_SECTION);
            for (index, line) in self.lines.iter().enumerate() {
                out.push('\n');
                out.push_str(&format!(
                    "{}. {} {} -> {}: {}",
                    index + 1,
                    line.tool,
                    line.arguments,
                    if line.failed { "failed" } else { "ok" },
                    line.result
                ));
                if let Some(detail) = &line.detail {
                    out.push_str("\n   detail: ");
                    out.push_str(detail);
                }
            }
        }
        if self.elided > 0 {
            out.push('\n');
            out.push_str(&format!(
                "({} older step(s) left out to stay within the ledger bound.)",
                self.elided
            ));
        }
        if !self.files.is_empty() {
            out.push_str("\nFiles written by the summarized steps: ");
            out.push_str(&self.files.join(", "));
        }
        out
    }

    /// Drop oldest lines, then the carried ledger, until the text fits.
    ///
    /// Each iteration removes something, so this always terminates; the final
    /// fallback text (header plus notice) is far below the bound.
    fn bound(&mut self) {
        while self.render().chars().count() > MAX_LEDGER_CHARS {
            if !self.lines.is_empty() {
                self.lines.remove(0);
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

/// Whether a call's tool is one that changes a file.
///
/// The turn's manifest knows the declared side effect. A call from an earlier
/// turn may name a tool this turn no longer offers, and the log keeps the name
/// but not the definition — the naming convention is the only evidence left.
pub fn is_write_tool(tool_name: &str, write_tools: &[String]) -> bool {
    if write_tools.iter().any(|known| known == tool_name) {
        return true;
    }
    let name = tool_name.to_ascii_lowercase();
    name.ends_with(".write") || name.ends_with(".edit")
}

/// The file a call names, in either of the two argument spellings the
/// workspace's tools use.
pub fn path_argument(arguments: &serde_json::Value) -> Option<&str> {
    arguments
        .get("path")
        .or_else(|| arguments.get("file_path"))
        .and_then(|value| value.as_str())
        .filter(|path| !path.is_empty())
}

/// The steps of a previous ledger, with that ledger's own structure removed.
///
/// A ledger message carries the text a reader saw, header and section labels
/// included. Copying it whole into the next ledger would nest one ledger in
/// another — two headers, two "steps in this compaction" labels, one of them
/// false. Only the structural lines are dropped; every step, the elision
/// notice and the file line survive, because those are the facts.
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

/// The lines of a failed result that name the failure, joined and bounded.
///
/// The first line is the one already kept as the result — for a test run it is
/// the exit code — so it is skipped here. A line is kept when it carries one of
/// [`FAILURE_MARKERS`], and the result's last non-empty line is always kept:
/// a failure that names nothing marker-like still ends with its verdict. Lines
/// are ranked by [`FAILURE_MARKER_TIERS`] before the bound is applied, so the
/// assertion's values come first even when the result lists test names first.
/// Exact duplicates are dropped, and the joined text is bounded like any other
/// line, so the same result always yields the same detail.
pub fn failure_detail(content: &str, first: &str) -> Option<String> {
    let lines: Vec<&str> =
        content.lines().map(str::trim).filter(|line| !line.is_empty() && *line != first).collect();
    let mut ranked: Vec<(usize, &str)> = Vec::new();
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if let Some(tier) = FAILURE_MARKER_TIERS
            .iter()
            .position(|markers| markers.iter().any(|m| lower.contains(m)))
            && !ranked.iter().any(|(_, kept)| *kept == *line)
        {
            ranked.push((tier, line));
        }
    }
    // Stable: within one rank the result's own order is kept.
    ranked.sort_by_key(|(tier, _)| *tier);
    let mut kept: Vec<&str> = ranked.into_iter().map(|(_, line)| line).collect();
    if let Some(last) = lines.last()
        && !kept.contains(last)
    {
        kept.push(last);
    }
    if kept.is_empty() {
        return None;
    }
    Some(bounded(&kept.join(" || "), MAX_DETAIL_CHARS))
}

/// The first `max_chars` characters of `text`, without splitting a character.
fn bounded(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_string(),
    }
}

/// The last `max_chars` characters of `text`, without splitting a character.
fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    let start = text.char_indices().nth(skip).map_or(text.len(), |(index, _)| index);
    let rest = &text[start..];
    // Start at a line boundary when there is one: a fragment of a line reads
    // as a continuation of the label above it, not as a line that was cut.
    match rest.find('\n') {
        Some(newline) if newline + 1 < rest.len() => format!("...{}", &rest[newline + 1..]),
        _ => format!("...{rest}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageRole, ModelMessage};

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
    fn a_call_and_its_result_become_one_line() {
        let messages = vec![
            call("1", "fs.read", serde_json::json!({ "path": "src/lib.rs" })),
            result("1", "fs.read", "line one\nline two", false),
        ];
        let ledger = Ledger::build(&messages, &[]);
        assert_eq!(ledger.lines.len(), 1);
        let line = &ledger.lines[0];
        assert_eq!(line.tool, "fs.read");
        assert_eq!(line.arguments, r#"{"path":"src/lib.rs"}"#);
        assert_eq!(line.result, "line one");
        assert!(!line.failed);
        assert!(ledger.render().contains("1. fs.read"));
    }

    #[test]
    fn a_failure_keeps_its_first_line_and_the_flag() {
        let messages = vec![
            call("1", "fs.read", serde_json::json!({ "path": "missing" })),
            result("1", "fs.read", "no such file: missing", true),
        ];
        let ledger = Ledger::build(&messages, &[]);
        assert!(ledger.lines[0].failed);
        assert!(ledger.render().contains("-> failed: no such file: missing"));
    }

    #[test]
    fn a_failure_keeps_the_lines_that_name_what_failed() {
        let content = "exit code 101\n\
                       running 8 tests\n\
                       test allocate_rounds_to_nearest_bucket ... FAILED\n\
                       \n\
                       failures:\n\
                       \n\
                       ---- allocate_rounds_to_nearest_bucket stdout ----\n\
                       thread 'tests::allocate' panicked at src/lib.rs:88:5:\n\
                       assertion `left == right` failed\n  \
                       left: Ok(74)\n \
                       right: Ok(111)\n\
                       test result: FAILED. 7 passed; 1 failed";
        let messages = vec![
            call("1", "shell.run", serde_json::json!({ "command": "scripts\\test.cmd" })),
            result("1", "shell.run", content, true),
        ];
        let ledger = Ledger::build(&messages, &[]);
        let line = &ledger.lines[0];
        assert_eq!(line.result, "exit code 101");
        let detail = line.detail.as_deref().expect("the failure's own lines");
        assert!(detail.contains("left: Ok(74)"), "{detail}");
        assert!(detail.contains("right: Ok(111)"), "{detail}");
        // The first line is already the result; repeating it would spend the
        // detail's bound on what the reader has.
        assert!(!detail.contains("exit code 101"), "{detail}");
        assert!(ledger.render().contains("\n   detail: "), "{}", ledger.render());
    }

    #[test]
    fn a_successful_result_has_no_detail() {
        let messages = vec![
            call("1", "shell.run", serde_json::json!({ "command": "scripts\\test.cmd" })),
            result("1", "shell.run", "exit code 0\nall green", false),
        ];
        let ledger = Ledger::build(&messages, &[]);
        assert!(ledger.lines[0].detail.is_none());
    }

    #[test]
    fn a_failure_with_nothing_beyond_its_first_line_has_no_detail() {
        let messages = vec![
            call("1", "fs.read", serde_json::json!({ "path": "missing" })),
            result("1", "fs.read", "no such file: missing", true),
        ];
        let ledger = Ledger::build(&messages, &[]);
        assert!(ledger.lines[0].detail.is_none());
    }

    #[test]
    fn a_failures_detail_is_bounded_and_deterministic() {
        let content = (0..80)
            .map(|index| format!("assertion {index} failed: expected Ok(1), got Ok(0)"))
            .collect::<Vec<_>>()
            .join("\n");
        let messages = vec![
            call("1", "shell.run", serde_json::json!({ "command": "x" })),
            result("1", "shell.run", &format!("exit code 1\n{content}"), true),
        ];
        let first = Ledger::build(&messages, &[]);
        let again = Ledger::build(&messages, &[]);
        assert_eq!(first, again);
        let detail = first.lines[0].detail.as_deref().unwrap();
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS + 3, "{}", detail.len());
    }

    #[test]
    fn an_orphan_result_is_not_a_step() {
        let messages = vec![result("x", "fs.read", "stray", false)];
        let ledger = Ledger::build(&messages, &[]);
        assert!(ledger.is_empty());
    }

    #[test]
    fn a_call_without_a_result_says_so() {
        let messages = vec![call("1", "fs.read", serde_json::json!({ "path": "a" }))];
        let ledger = Ledger::build(&messages, &[]);
        assert_eq!(ledger.lines[0].result, "no result recorded");
    }

    #[test]
    fn write_calls_name_the_files_they_changed() {
        let messages = vec![
            call("1", "fs.write", serde_json::json!({ "path": "notes.md" })),
            result("1", "fs.write", "wrote 12 bytes", false),
            call("2", "fs.write", serde_json::json!({ "path": "failed.md" })),
            result("2", "fs.write", "denied", true),
        ];
        let ledger = Ledger::build(&messages, &["fs.write".to_string()]);
        assert_eq!(ledger.files, vec!["notes.md".to_string()]);
        assert!(ledger.render().contains("Files written by the summarized steps: notes.md"));
    }

    #[test]
    fn the_manifest_names_the_write_tools_a_call_no_longer_can_ask_about() {
        let messages = vec![
            call("1", "custom.store", serde_json::json!({ "file_path": "a.txt" })),
            result("1", "custom.store", "ok", false),
        ];
        let ledger = Ledger::build(&messages, &["custom.store".to_string()]);
        assert_eq!(ledger.files, vec!["a.txt".to_string()]);
    }

    #[test]
    fn a_previous_ledger_is_carried_forward_verbatim() {
        let earlier = ModelMessage::ledger_for(
            &crate::ids::AttemptId::from_string("a1"),
            "Earlier lines here.",
        );
        let messages = vec![
            earlier,
            call("2", "fs.read", serde_json::json!({ "path": "b" })),
            result("2", "fs.read", "ok", false),
        ];
        let ledger = Ledger::build(&messages, &[]);
        assert!(ledger.render().contains("Earlier steps, carried from the previous ledger:"));
        assert!(ledger.render().contains("Earlier lines here."));
        assert!(ledger.render().contains("Steps in this compaction:"));
    }

    #[test]
    fn carrying_a_rendered_ledger_does_not_nest_it() {
        let earlier = ModelMessage::ledger_for(
            &crate::ids::AttemptId::from_string("a1"),
            Ledger {
                carried: None,
                carried_elided: false,
                lines: vec![LedgerLine {
                    tool: "fs.list".to_string(),
                    arguments: r#"{"path":"data"}"#.to_string(),
                    result: "data/file1.txt".to_string(),
                    detail: None,
                    failed: false,
                }],
                files: Vec::new(),
                elided: 0,
            }
            .render(),
        );
        let messages = vec![
            earlier,
            call("2", "fs.read", serde_json::json!({ "path": "b" })),
            result("2", "fs.read", "ok", false),
        ];
        let rendered = Ledger::build(&messages, &[]).render();
        assert_eq!(rendered.matches(HEADER_PREFIX).count(), 1, "{rendered}");
        assert_eq!(rendered.matches(THIS_SECTION).count(), 1, "{rendered}");
        assert_eq!(rendered.matches(CARRIED_SECTION).count(), 1, "{rendered}");
        assert!(rendered.contains("1. fs.list"), "{rendered}");
        assert!(rendered.contains("1. fs.read"), "{rendered}");
    }

    #[test]
    fn a_carried_ledger_starts_at_a_step_boundary() {
        let long = (1..=60)
            .map(|index| format!("{index}. fs.read {{\"path\":\"f{index}\"}} -> ok: x"))
            .collect::<Vec<_>>()
            .join("\n");
        let earlier =
            ModelMessage::ledger_for(&crate::ids::AttemptId::from_string("a1"), long.clone());
        let rendered = Ledger::build(&[earlier], &[]).render();
        let carried = rendered
            .lines()
            .skip_while(|line| *line != CARRIED_SECTION)
            .nth(1)
            .expect("a carried section");
        let rest = carried.strip_prefix("...").expect("a cut marker");
        assert!(rest.chars().next().is_some_and(|c| c.is_ascii_digit()), "{carried}");
        assert!(long.contains(rest), "the carried tail is the text's own tail: {carried}");
    }

    #[test]
    fn the_rendering_is_bounded_and_says_what_it_dropped() {
        let mut messages = Vec::new();
        for index in 0..200 {
            let id = index.to_string();
            messages.push(call(
                &id,
                "fs.read",
                serde_json::json!({ "path": format!("file-{index}.rs") }),
            ));
            messages.push(result(&id, "fs.read", "some output text", false));
        }
        let ledger = Ledger::build(&messages, &[]);
        assert!(ledger.render().chars().count() <= MAX_LEDGER_CHARS);
        assert!(ledger.elided > 0);
        assert!(ledger.render().contains("older step(s) left out"));
    }

    #[test]
    fn the_same_messages_render_the_same_bytes() {
        let messages = vec![
            call("1", "fs.read", serde_json::json!({ "path": "a" })),
            result("1", "fs.read", "ok", false),
        ];
        assert_eq!(Ledger::build(&messages, &[]).render(), Ledger::build(&messages, &[]).render());
    }
}
