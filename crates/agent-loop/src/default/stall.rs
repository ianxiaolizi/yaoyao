//! Whether a turn is learning anything, and what it is told when it is not.
//!
//! A turn can spend its whole budget re-reading the same file: every step is a
//! call, every call has a result, and the result is the one the turn already
//! had. The step counter cannot see that — it counts work, and a loop is work —
//! so the loop keeps a second count of its own: consecutive steps since the
//! last one that produced evidence the turn had not seen.
//!
//! Evidence is a tool outcome, keyed by the call and the answer together: the
//! same call returning the same text is not new information, while the same
//! tool with different arguments (another file, another offset) is. That also
//! makes the repeat gate's case count here without a second mechanism — a
//! repeated call contributes nothing this set has not already got.
//!
//! At `stall_steps` steps without new evidence the next request carries one
//! reminder; at twice that the turn ends as `Stalled`. Both numbers come from
//! the run's budget, with a default so a run that configures nothing still has
//! the protection.

use std::collections::{BTreeMap, BTreeSet};

use agent_protocol::digest;
use agent_protocol::ids::TurnId;
use agent_protocol::model::{ContentBlock, ModelMessage};

use super::*;

/// Steps without new evidence before the reminder when a hand-built harness
/// supplies no budget. Configuration-managed runs get the shipped default of
/// 8 from `agent-config`; this lower fallback protects callers that bypass
/// configuration entirely.
pub(super) const DEFAULT_STALL_STEPS: u32 = 6;

/// The evidence a turn has collected, and the streak that has brought none.
#[derive(Debug, Default)]
pub(super) struct Progress {
    /// Digests of every distinct (call, answer) pair this turn has produced.
    seen: BTreeSet<String>,
    /// Consecutive steps that added nothing to `seen`.
    unchanged_steps: u32,
    /// Where this step's own messages begin, set when its assistant turn is
    /// pushed. `None` outside a step that has a response.
    step_from: Option<usize>,
}

impl LoopTurn {
    /// How many steps without new evidence this run tolerates.
    pub(super) fn stall_steps(&self) -> u32 {
        self.budget.stall_steps.filter(|steps| *steps > 0).unwrap_or(DEFAULT_STALL_STEPS)
    }

    /// How many consecutive steps have produced nothing new — what the
    /// prepared request records when it carries the reminder.
    pub(super) fn progress_unchanged_steps(&self) -> u32 {
        self.progress.unchanged_steps
    }

    /// Mark where this step's own messages begin.
    ///
    /// Called just before the assistant turn is pushed: everything from here on
    /// is what the step learned. Framework layers placed after it (the tail
    /// blocks) are the loop talking, not the world answering, and are not in
    /// the range this reads.
    pub(super) fn mark_evidence_start(&mut self) {
        self.progress.step_from = Some(self.messages.len());
    }

    /// Fold the messages this step produced into the turn's evidence, and move
    /// the streak by whether anything in them was new.
    ///
    /// Idempotent within a step: the marker is taken, so an early return that
    /// records and the end-of-step call cannot both count one step.
    pub(super) fn record_evidence(&mut self) {
        let Some(from) = self.progress.step_from.take() else {
            return;
        };
        let from = from.min(self.messages.len());
        let fresh = new_evidence(&self.messages[from..], &mut self.progress.seen);
        if fresh {
            self.progress.unchanged_steps = 0;
        } else {
            self.progress.unchanged_steps = self.progress.unchanged_steps.saturating_add(1);
        }
    }

    /// A step the repeat gate held rather than dispatched.
    ///
    /// It is the clearest case of "nothing new" there is — the loop itself said
    /// the call would do the same thing — so it counts toward the streak
    /// instead of being measured. The held results are the kernel's sentences,
    /// and their wording carries the streak count, so measuring them would read
    /// a new answer on every repeat.
    pub(super) fn record_held_step(&mut self) {
        self.progress.step_from = None;
        self.progress.unchanged_steps = self.progress.unchanged_steps.saturating_add(1);
    }

    /// Whether the turn has made no progress for long enough to stop it.
    ///
    /// Twice the reminder threshold: a reminder is a request to change course,
    /// and a model that answers it needs more than one step to show it did.
    pub(super) fn stall_reached(&self) -> bool {
        self.progress.unchanged_steps >= self.stall_steps().saturating_mul(2)
    }

    /// Whether the next request should carry the reminder.
    pub(super) fn stall_warning(&self) -> bool {
        self.progress.unchanged_steps >= self.stall_steps()
    }
}

/// Place this request's stall reminder at the tail, or remove the one the last
/// request carried.
///
/// Removing on a step that made progress matters: a stale "you are going in
/// circles" over a step that has just found something new is a false statement
/// the model would act on.
pub(super) fn place_stall_notice(turn: &mut LoopTurn, turn_id: &TurnId) -> Option<String> {
    let uid = format!("msg/stall/{turn_id}");
    turn.messages.retain(|message| message.message_uid.as_str() != uid);
    if !turn.stall_warning() {
        return None;
    }
    let text = stall_text(turn.progress.unchanged_steps, turn.stall_steps());
    turn.messages.push(ModelMessage::stall_notice_for_turn(turn_id, text.clone()));
    Some(text)
}

/// What the reminder says.
fn stall_text(unchanged_steps: u32, limit: u32) -> String {
    format!(
        "Stall notice: the last {unchanged_steps} steps produced no new evidence — every call \
         returned something this turn had already seen. Repeating a call will not change the \
         answer. Do something different: another tool, another argument, or another reading of \
         the problem. If nothing available can settle it, record what is blocked in the work \
         state (`state.update`, a `blocked` entry with what you know) and answer with what you \
         have. At {limit} such steps the run ends as stalled rather than spending its budget \
         going in circles."
    )
}

/// Add this step's (call, answer) pairs to `seen`; report whether any was new.
fn new_evidence(messages: &[ModelMessage], seen: &mut BTreeSet<String>) -> bool {
    // A result's digest needs its call's arguments: identical output from two
    // different calls (two files that happen to be equal, two greps that found
    // the same line) is two pieces of evidence, not one. Tool blocks precede
    // their results in every message order the loop builds, but collecting the
    // calls first costs nothing and does not depend on that.
    let mut arguments: BTreeMap<&str, &serde_json::Value> = BTreeMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { tool_call_id, arguments: args, .. } = block {
                arguments.insert(tool_call_id.as_str(), args);
            }
        }
    }
    let mut fresh = false;
    for message in messages {
        for block in &message.content {
            let ContentBlock::ToolResult { tool_call_id, tool_name, content, is_error } = block
            else {
                continue;
            };
            let args = arguments
                .get(tool_call_id.as_str())
                .map(|value| digest::json(value))
                .unwrap_or_default();
            let key = digest::fnv1a_hex(
                format!("{tool_name}\u{0}{args}\u{0}{is_error}\u{0}{content}").as_bytes(),
            );
            if seen.insert(key) {
                fresh = true;
            }
        }
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: &str, id: &str, content: &str) -> ModelMessage {
        ModelMessage::new(
            agent_protocol::model::MessageRole::Tool,
            vec![ContentBlock::tool_result(id, name, content, false)],
        )
    }

    fn call(name: &str, id: &str, arguments: serde_json::Value) -> ModelMessage {
        ModelMessage::new(
            agent_protocol::model::MessageRole::Assistant,
            vec![ContentBlock::tool_use(id, name, arguments)],
        )
    }

    #[test]
    fn a_repeated_call_with_the_same_answer_is_not_new_evidence() {
        let mut seen = BTreeSet::new();
        let step = vec![
            call("fs.read", "c1", serde_json::json!({"path": "a.txt"})),
            result("fs.read", "c1", "41\n"),
        ];

        assert!(new_evidence(&step, &mut seen), "the first look is new");
        assert!(!new_evidence(&step, &mut seen), "the same look again is not");
    }

    #[test]
    fn the_same_answer_from_a_different_call_is_new_evidence() {
        let mut seen = BTreeSet::new();
        new_evidence(
            &[
                call("fs.read", "c1", serde_json::json!({"path": "a.txt"})),
                result("fs.read", "c1", "41\n"),
            ],
            &mut seen,
        );
        let other = [
            call("fs.read", "c2", serde_json::json!({"path": "b.txt"})),
            result("fs.read", "c2", "41\n"),
        ];

        assert!(new_evidence(&other, &mut seen), "another file is another reading");
    }

    #[test]
    fn a_step_with_no_calls_adds_nothing() {
        let mut seen = BTreeSet::new();
        let prose = ModelMessage::new(
            agent_protocol::model::MessageRole::Assistant,
            vec![ContentBlock::text("thinking about it")],
        );

        assert!(!new_evidence(&[prose], &mut seen));
    }
}
