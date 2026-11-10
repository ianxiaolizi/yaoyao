//! Whether a turn keeps editing one file without ever checking it.
//!
//! The stall notice asks whether the turn is learning anything. This asks a
//! narrower question with the same shape: is one file being rewritten over and
//! over while no check ever passes? A turn can be making "progress" in the
//! stall ledger — every edit returns something new — and still be drifting,
//! because nothing has confirmed that the file it keeps changing is right.
//!
//! The count is per path and is cleared by a passing check, not by time or by
//! steps. Three edits with no check in between is the threshold: one to get it
//! wrong, one to fix it, one to fix the fix. The request after that third edit
//! is told to go and look.
//!
//! The notice is a reminder, never a stop: the turn's budget and its stall
//! rules decide when it ends. A model that answers it by running a check
//! clears the ledger; a model that cannot check is asked to say so.

use std::collections::BTreeMap;

use agent_ports::tool_provider::ToolStatus;
use agent_protocol::ids::TurnId;
use agent_protocol::model::{ModelMessage, ToolCall};

use super::*;

/// Edits to one path, with no passing check in between, that arm the notice.
pub(super) const EDIT_CHURN_THRESHOLD: u32 = 3;

/// What this turn has edited since its last passing check.
#[derive(Debug, Default)]
pub(super) struct Edits {
    /// Edits per path since the last passing check.
    counts: BTreeMap<String, u32>,
    /// The path that armed the notice, and the count at that moment. Kept so
    /// the notice says the same sentence until a pass clears it, rather than
    /// re-arming on every later edit with a different number.
    armed: Option<(String, u32)>,
}

/// A notice placed at the tail of one request.
pub(super) struct ConvergenceNotice {
    pub path: String,
    pub edits: u32,
    pub text: String,
}

impl Edits {
    /// Record one successful edit of `path`.
    pub(super) fn note_edit(&mut self, path: &str) {
        let count = self.counts.entry(path.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        if *count >= EDIT_CHURN_THRESHOLD {
            // The latest count, so a reader of the prepared record sees how far
            // the drift has gone, not just the step that crossed the line.
            self.armed = Some((path.to_string(), *count));
        }
    }

    /// Record a check that passed.
    ///
    /// Every path starts again: a pass is evidence about the work as a whole,
    /// and keeping counts that were not covered by it would warn about edits
    /// the check may well have been run after.
    pub(super) fn note_pass(&mut self) {
        self.counts.clear();
        self.armed = None;
    }

    /// The path that should be named this request, if any.
    pub(super) fn warning(&self) -> Option<(String, u32)> {
        self.armed.clone()
    }
}

/// Fold one dispatched call into the ledger.
///
/// A check that completed covers every edit behind it; a check that failed is
/// not cover, so the counts stand. An edit that completed is drift until a
/// check says otherwise; an edit that failed did not land and is not drift.
pub(super) fn note_result(
    turn: &mut LoopTurn,
    call: &ToolCall,
    acted_as_check: bool,
    status: ToolStatus,
) {
    if status != ToolStatus::Completed {
        return;
    }
    if acted_as_check {
        turn.convergence.note_pass();
    }
    if let Some(path) = edit_target(call) {
        turn.convergence.note_edit(path);
    }
}

/// The file a write or edit call targets, if it names one.
pub(super) fn edit_target(call: &ToolCall) -> Option<&str> {
    match call.tool_name.as_str() {
        "fs.write" | "fs.edit" => call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .filter(|path| !path.is_empty()),
        _ => None,
    }
}

/// Place this request's convergence reminder, or remove the one the last
/// request carried.
///
/// Same shape as the stall notice: one message, keyed by the turn, replaced
/// rather than repeated, and removed when the condition no longer holds.
pub(super) fn place_notice(turn: &mut LoopTurn, turn_id: &TurnId) -> Option<ConvergenceNotice> {
    let uid = format!("msg/convergence/{turn_id}");
    turn.messages.retain(|message| message.message_uid.as_str() != uid);
    let (path, edits) = turn.convergence.warning()?;
    let text = notice_text(&path, edits);
    turn.messages.push(ModelMessage::convergence_notice_for_turn(turn_id, text.clone()));
    Some(ConvergenceNotice { path, edits, text })
}

/// What the reminder says.
fn notice_text(path: &str, edits: u32) -> String {
    format!(
        "Convergence notice: `{path}` has been edited {edits} times since the last passing check. \
         Rewriting it again without checking it is how a file drifts from what the task needs. \
         Run a check that covers this file — a test, a build, or a render — or say what blocks \
         verification. If you cannot verify it, say so in your answer instead of calling it done."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, path: &str) -> ToolCall {
        ToolCall {
            tool_call_id: "c1".to_string(),
            tool_name: name.to_string(),
            arguments: serde_json::json!({ "path": path, "content": "x" }),
        }
    }

    #[test]
    fn three_edits_to_one_file_arm_one_notice() {
        let mut edits = Edits::default();
        edits.note_edit("src/main.js");
        edits.note_edit("src/main.js");
        assert!(edits.warning().is_none(), "two edits are ordinary work");
        edits.note_edit("src/main.js");
        assert_eq!(edits.warning(), Some(("src/main.js".to_string(), 3)));
        edits.note_edit("src/main.js");
        assert_eq!(edits.warning(), Some(("src/main.js".to_string(), 4)), "the count moves");
    }

    #[test]
    fn edits_to_different_files_do_not_add_up() {
        let mut edits = Edits::default();
        edits.note_edit("a.js");
        edits.note_edit("b.js");
        edits.note_edit("c.js");
        assert!(edits.warning().is_none(), "three files edited once each is not drift");
    }

    #[test]
    fn a_passing_check_clears_every_count() {
        let mut edits = Edits::default();
        for _ in 0..3 {
            edits.note_edit("src/main.js");
        }
        edits.note_pass();
        assert!(edits.warning().is_none());
        edits.note_edit("src/main.js");
        assert!(edits.warning().is_none(), "the count starts again after a pass");
    }

    #[test]
    fn only_write_tools_name_a_target() {
        assert_eq!(edit_target(&call("fs.write", "a.js")), Some("a.js"));
        assert_eq!(edit_target(&call("fs.edit", "a.js")), Some("a.js"));
        assert_eq!(edit_target(&call("fs.read", "a.js")), None);
        assert_eq!(edit_target(&call("shell.run", "a.js")), None);
    }

    #[test]
    fn the_notice_names_the_path_the_count_and_what_to_do() {
        let text = notice_text("src/main.js", 3);
        assert!(text.contains("`src/main.js`"), "{text}");
        assert!(text.contains("3 times"), "{text}");
        assert!(text.contains("say what blocks verification"), "{text}");
        assert!(!text.contains("ends as"), "it is a reminder, not a stop");
    }
}
