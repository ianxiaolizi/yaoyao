//! What a kernel gate tells the model, and what makes two calls the same call.
//!
//! Every text here is written into the turn as a kernel note the model reads on
//! the next step, so the wording is part of the gate: it names the fact the
//! gate saw and the one move that answers it. They live together because they
//! are the same sentence shape — "Kernel check: …, so do this" — and a reader
//! comparing gates should not have to scroll the loop to find them.

use crate::context_layers::StaleRead;

/// What the model is told when it answers after recording a retry without
/// making it.
pub(crate) fn retry_text(retry: &str, hint: String) -> String {
    format!(
        "Kernel check: you recorded a retry — `{retry}` — but answered without making the \
         call. Make it now with a tool call ({hint}). If it cannot be made, record reconcile \
         again with `stop` true and say in `next` why stopping is the right answer. A retry \
         that is only described was not made."
    )
}

/// What the model is told when its answer restates a read from an earlier
/// turn. Names the read and the one call that refreshes it.
pub(crate) fn stale_text(stale: &StaleRead) -> String {
    format!(
        "Kernel check: `{}` was read in an earlier turn ({}), not in this one — the answer \
         restates an old reading. Read it again now with {}, then answer.",
        stale.target, stale.tool, stale.tool
    )
}

/// What the model is told when the same call would run a third time unchanged.
pub(crate) fn repeat_text(tool: &str, count: u32) -> String {
    format!(
        "Kernel check: `{tool}` has already run {count} times in a row with the same \
         arguments; another identical run will do the same thing. Change an argument or use \
         another tool, or record a reconcile saying why this time will be different."
    )
}

/// What makes two calls the same call: the tool and the arguments, byte for
/// byte. This is how `kernel_eval` classifies an identical retry, so the gate
/// and the report agree on what "the same call" means.
pub(crate) fn call_signature(tool_name: &str, arguments: &serde_json::Value) -> String {
    format!("{tool_name}\u{0}{arguments}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_is_the_tool_and_the_arguments_and_nothing_else() {
        let arguments = serde_json::json!({"path": "a.txt"});
        assert_eq!(call_signature("fs.read", &arguments), call_signature("fs.read", &arguments));
        assert_ne!(call_signature("fs.read", &arguments), call_signature("fs.write", &arguments));
        // The separator cannot be forged out of the arguments: a path that
        // contains it would otherwise make two different calls compare equal.
        assert_ne!(
            call_signature("fs.read", &serde_json::json!({ "path": "a\u{0}fs.write" })),
            call_signature("fs.read\u{0}fs.write", &serde_json::json!({ "path": "a" }))
        );
    }
}
