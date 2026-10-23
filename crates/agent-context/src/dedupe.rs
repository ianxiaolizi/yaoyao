//! Rewriting an earlier repeated tool result into a pointer to the later one.
//!
//! A model that reads the same file or runs the same command again gets the
//! same text it already has. The second copy is useful at the moment it
//! arrives (it is evidence the file still says that, or that the command still
//! says that), but it is not worth carrying twice through every later request:
//! the window pays for both copies on every step until one is summarized.
//!
//! The rule here is a rewrite of settled history, applied at the turn boundary
//! before the window is fitted: the newest copy stays verbatim, earlier copies
//! whose call identity and result text are both identical become one line that
//! names the copy that was kept. The log is untouched — the full text is still
//! in the events, the artifact store, and the ledger — so nothing is
//! unrecoverable, and a rebuild applies the same rule to the same log.
//!
//! Only `fs.read` and `shell.run` are collapsed. Both calls stay in the
//! history as calls, so "the command ran twice" is still visible; what is not
//! repeated is the output that both runs agreed on. A command that failed once
//! and succeeded once has two different outcomes and is never merged. Writes
//! are not collapsed at all: their result text describes an effect, and the
//! same effect twice is not the same fact as once.

use std::collections::HashMap;

use agent_protocol::model::{ContentBlock, ModelMessage};

/// The tools whose identical repeated results are collapsed here.
const COLLAPSED_TOOLS: [&str; 2] = ["fs.read", "shell.run"];

/// Collapse repeated identical results, newest copy kept verbatim.
///
/// Two results are "the same" only when their calls were made with identical
/// arguments and the result text and failure flag are identical: two empty
/// files have the same text but are not the same file, and a read that failed
/// did not return the file. The arguments come from the call the result
/// belongs to, so nothing is inferred from prose.
pub fn repeated_results(mut messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    // Call id → its arguments, rendered. The value prints its object keys in a
    // stable order, so two equal argument objects render equal.
    let arguments: HashMap<String, String> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { tool_call_id, arguments, .. } => {
                Some((tool_call_id.clone(), arguments.to_string()))
            }
            _ => None,
        })
        .collect();

    /// One collapsible result, located so it can be rewritten afterwards.
    struct Read<'a> {
        message: usize,
        block: usize,
        call: &'a str,
        arguments: &'a str,
        content: &'a str,
        is_error: bool,
    }

    let mut reads: Vec<Read<'_>> = Vec::new();
    for (message, model_message) in messages.iter().enumerate() {
        for (block, content) in model_message.content.iter().enumerate() {
            let ContentBlock::ToolResult { tool_call_id, tool_name, content, is_error } = content
            else {
                continue;
            };
            if !COLLAPSED_TOOLS.contains(&tool_name.as_str()) {
                continue;
            }
            let Some(arguments) = arguments.get(tool_call_id) else {
                continue;
            };
            reads.push(Read {
                message,
                block,
                call: tool_call_id,
                arguments,
                content,
                is_error: *is_error,
            });
        }
    }

    // Every result that has an identical twin later in the history is rewritten
    // to name the *last* twin: the newest copy is the one the window is most
    // likely to keep, so a pointer to it survives trimming and compaction.
    let mut rewrites: Vec<(usize, usize, String)> = Vec::new();
    for read in &reads {
        let later = reads
            .iter()
            .filter(|other| other.arguments == read.arguments)
            .filter(|other| other.is_error == read.is_error)
            .rfind(|other| other.content == read.content);
        if let Some(later) = later
            && (later.message, later.block) > (read.message, read.block)
        {
            rewrites.push((read.message, read.block, later.call.to_string()));
        }
    }

    // The borrows above end here; the rewrite itself only needs the call id of
    // the copy that was kept.
    for (message, block, later) in rewrites {
        if let ContentBlock::ToolResult { tool_name, content, .. } =
            &mut messages[message].content[block]
        {
            *content = pointer(tool_name, &later);
        }
    }
    messages
}

/// What replaces the earlier copy's text. It names the kept copy by its
/// provider call id, which is the id the model saw on the call it made.
fn pointer(tool_name: &str, later_call: &str) -> String {
    format!(
        "[This {tool_name} returned exactly what the call \"{later_call}\" later in this \
         conversation returned; the text is not repeated here.]"
    )
}

#[cfg(test)]
mod tests {
    use agent_protocol::model::MessageRole;

    use super::*;

    fn path_request(call: &str, path: &str) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::tool_use(call, "fs.read", serde_json::json!({ "path": path }))],
        )
    }

    fn result(call: &str, text: &str) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Tool,
            vec![ContentBlock::tool_result(call, "fs.read", text, false)],
        )
    }

    /// The text a tool-result message carries; tool proposals and results have
    /// no plain text block, so `text_content` would be empty for both.
    fn outcome(message: &ModelMessage) -> String {
        message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn contents(messages: &[ModelMessage]) -> Vec<String> {
        messages.iter().map(outcome).collect()
    }

    #[test]
    fn an_earlier_identical_read_becomes_a_pointer_to_the_later_one() {
        let messages = vec![
            path_request("c1", "a.txt"),
            result("c1", "hello"),
            path_request("c2", "a.txt"),
            result("c2", "hello"),
        ];

        let collapsed = repeated_results(messages);

        assert_eq!(outcome(&collapsed[1]), pointer("fs.read", "c2"));
        assert_eq!(outcome(&collapsed[3]), "hello", "the newest copy stays verbatim");
    }

    #[test]
    fn a_read_whose_text_changed_is_kept_in_full() {
        // The file changed between the two reads: the earlier text is the only
        // record of what it used to say, so it must not be collapsed.
        let messages = vec![
            path_request("c1", "a.txt"),
            result("c1", "before"),
            path_request("c2", "a.txt"),
            result("c2", "after"),
        ];

        let collapsed = repeated_results(messages);

        assert_eq!(contents(&collapsed), vec!["", "before", "", "after"]);
    }

    #[test]
    fn reads_of_different_paths_are_never_collapsed() {
        // Two empty files have the same text; they are still two files.
        let messages = vec![
            path_request("c1", "a.txt"),
            result("c1", ""),
            path_request("c2", "b.txt"),
            result("c2", ""),
        ];

        let collapsed = repeated_results(messages.clone());

        assert_eq!(collapsed, messages);
    }

    #[test]
    fn a_failed_read_is_not_collapsed_with_a_successful_one() {
        // A failure's text is an error message, not the file; it says nothing
        // about the read that succeeded.
        let failed = ModelMessage::new(
            MessageRole::Tool,
            vec![ContentBlock::tool_result("c1", "fs.read", "not found", true)],
        );
        let messages = vec![
            path_request("c1", "a.txt"),
            failed,
            path_request("c2", "a.txt"),
            result("c2", "not found"),
        ];

        let collapsed = repeated_results(messages);

        assert_eq!(outcome(&collapsed[1]), "not found", "the failure keeps its own words");
        assert!(!outcome(&collapsed[1]).starts_with("[This fs.read"));
        assert_eq!(outcome(&collapsed[3]), "not found");
    }

    #[test]
    fn every_earlier_copy_points_at_the_last_one() {
        let messages = vec![
            path_request("c1", "a.txt"),
            result("c1", "hello"),
            path_request("c2", "a.txt"),
            result("c2", "hello"),
            path_request("c3", "a.txt"),
            result("c3", "hello"),
        ];

        let collapsed = repeated_results(messages);

        assert_eq!(outcome(&collapsed[1]), pointer("fs.read", "c3"));
        assert_eq!(outcome(&collapsed[3]), pointer("fs.read", "c3"));
        assert_eq!(outcome(&collapsed[5]), "hello");
    }

    #[test]
    fn collapsing_twice_changes_nothing_more() {
        // Derivation runs on every turn; the rule must reach a fixed point so
        // that two derivations of the same log send the same history.
        let messages = vec![
            path_request("c1", "a.txt"),
            result("c1", "hello"),
            path_request("c2", "a.txt"),
            result("c2", "hello"),
        ];

        let once = repeated_results(messages);
        let twice = repeated_results(once.clone());

        assert_eq!(once, twice);
    }

    #[test]
    fn a_result_whose_call_is_not_in_the_history_is_left_alone() {
        // The arguments cannot be recovered (the call was trimmed away or never
        // recorded), so "same read" cannot be claimed.
        let messages = vec![result("c1", "hello"), result("c2", "hello")];

        let collapsed = repeated_results(messages.clone());

        assert_eq!(collapsed, messages);
    }

    fn command(call: &str, command: &str) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::tool_use(
                call,
                "shell.run",
                serde_json::json!({ "command": command }),
            )],
        )
    }

    fn output(call: &str, text: &str, is_error: bool) -> ModelMessage {
        ModelMessage::new(
            MessageRole::Tool,
            vec![ContentBlock::tool_result(call, "shell.run", text, is_error)],
        )
    }

    #[test]
    fn a_repeated_command_returning_the_same_output_is_collapsed() {
        // Both invocations stay in the history as calls; only the second copy
        // of the output is dropped, because both runs agreed on it.
        let messages = vec![
            command("c1", "git status --short"),
            output("c1", " M a.txt", false),
            command("c2", "git status --short"),
            output("c2", " M a.txt", false),
        ];

        let collapsed = repeated_results(messages);

        assert_eq!(outcome(&collapsed[1]), pointer("shell.run", "c2"));
        assert_eq!(outcome(&collapsed[3]), " M a.txt");
        // The proposal is what keeps "it ran twice" visible.
        assert_eq!(
            collapsed[2].content[0],
            ContentBlock::tool_use(
                "c2",
                "shell.run",
                serde_json::json!({ "command": "git status --short" })
            )
        );
    }

    #[test]
    fn a_repeated_command_with_different_output_keeps_both() {
        // The second run said something new; the first is not redundant.
        let messages = vec![
            command("c1", "ls"),
            output("c1", "a.txt", false),
            command("c2", "ls"),
            output(
                "c2",
                "a.txt
b.txt",
                false,
            ),
        ];

        let collapsed = repeated_results(messages.clone());

        assert_eq!(collapsed, messages);
    }

    #[test]
    fn a_failed_run_is_not_collapsed_with_a_successful_one() {
        let messages = vec![
            command("c1", "false"),
            output("c1", "exit code 1", true),
            command("c2", "false"),
            output("c2", "exit code 1", false),
        ];

        let collapsed = repeated_results(messages.clone());

        assert_eq!(collapsed, messages);
    }

    #[test]
    fn writes_are_never_collapsed() {
        // The same text twice does not make two writes one: the result says an
        // effect happened, and the number of effects is not in the text.
        let write = |call: &str| {
            ModelMessage::new(
                MessageRole::Assistant,
                vec![ContentBlock::tool_use(
                    call,
                    "fs.write",
                    serde_json::json!({ "path": "a.txt" }),
                )],
            )
        };
        let wrote = |call: &str| {
            ModelMessage::new(
                MessageRole::Tool,
                vec![ContentBlock::tool_result(call, "fs.write", "wrote 5 bytes", false)],
            )
        };
        let messages = vec![write("c1"), wrote("c1"), write("c2"), wrote("c2")];

        let collapsed = repeated_results(messages.clone());

        assert_eq!(collapsed, messages);
    }
}
