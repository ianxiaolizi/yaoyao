//! What a run leaves behind when its budget, not its work, ended it.
//!
//! A turn that stops on a limit has not answered anything: it ends
//! `turn/yielded`, and the delivery axis already refuses to call that delivered.
//! What it did *not* have is a record of where it stopped — which goals were
//! still open, what it meant to do next, and which checks its evidence came
//! from. Without that record, "continue" means re-reading the whole transcript,
//! which is exactly the cost the budget just refused to pay.
//!
//! `budget/parked` carries this. The Scheduler writes it in the same batch as
//! the turn's terminal events, so a parked turn and the state it parks with
//! cannot be separated by a crash. The state is assembled from facts already in
//! the log — the model's last `state/updated` and `plan/updated`, and the
//! checks in the evidence ledger — rather than asked of the model at the stop:
//! the model that just ran out of budget is the last thing to ask for a
//! summary, and an account written under a spent budget is the one least worth
//! trusting.

use serde::{Deserialize, Serialize};

/// Where a run stopped when its budget ran out.
///
/// This is a *record*, not an instruction: nothing in the kernel acts on it
/// except [`bounded`](Self::bounded)'s size limits. It exists so that the next
/// turn — or the person reading the log — can see what was left without
/// replaying the transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkedState {
    /// The limit's own word: `max_steps`, `max_tool_calls`, `wall_clock_exceeded`,
    /// `max_output_tokens`, `token_budget_exceeded`, `stalled`.
    pub reason: String,
    /// How much of the limit that stopped it was used, as a percentage. 100 is
    /// the ordinary case; a limit enforced inside a step can overshoot.
    #[serde(default)]
    pub percent: u32,
    /// What the plan and the work state still had open when the budget ran out.
    #[serde(default)]
    pub remaining_goals: Vec<String>,
    /// What the model said it would do next, when its last `state.update` said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    /// The checks this run recorded, `check_id:status`, most recent last.
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

impl ParkedState {
    /// Most remaining goals one parked record carries. The plan and the work
    /// state are both bounded already; this is the sum of the two, cut to what
    /// a reader can act on.
    pub const MAX_REMAINING_GOALS: usize = 8;
    /// Most evidence references one parked record carries — the tail of the
    /// ledger, not its history.
    pub const MAX_EVIDENCE_REFS: usize = 5;

    /// A parked record for one limit, before the log's facts are added.
    pub fn new(reason: impl Into<String>, percent: u32) -> Self {
        Self { reason: reason.into(), percent, ..Self::default() }
    }

    /// Trim the lists to what a reader can use.
    ///
    /// Blank entries are dropped — a plan step that is an empty string says
    /// nothing, and repeating it in the parked record would make the record
    /// look fuller than it is — and repeated text is kept once, because the
    /// plan and the work state often say the same thing in the same words.
    /// The most recent evidence survives: the tail is the part a continuation
    /// checks first.
    pub fn bounded(mut self) -> Self {
        self.remaining_goals = dedupe(self.remaining_goals, Self::MAX_REMAINING_GOALS);
        self.evidence_refs = dedupe(self.evidence_refs, Self::MAX_EVIDENCE_REFS);
        self.next_step = self.next_step.map(|next| next.trim().to_string());
        self.next_step = self.next_step.filter(|next| !next.is_empty());
        self
    }
}

/// Keep the first `max` non-blank entries, each once.
fn dedupe(entries: Vec<String>, max: usize) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.trim().to_string();
        if entry.is_empty() || kept.iter().any(|seen| seen == &entry) {
            continue;
        }
        kept.push(entry);
        if kept.len() == max {
            break;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parked_record_round_trips_through_the_event_payload() {
        let state = ParkedState {
            reason: "max_steps".to_string(),
            percent: 100,
            remaining_goals: vec!["run the suite".to_string()],
            next_step: Some("fix the failing test".to_string()),
            evidence_refs: vec!["v1:passed".to_string()],
        }
        .bounded();

        let payload = serde_json::json!({ "parked": state });
        let read: ParkedState =
            serde_json::from_value(payload["parked"].clone()).expect("reads back");
        assert_eq!(read, state);
    }

    #[test]
    fn a_parked_record_without_a_next_step_does_not_carry_an_empty_one() {
        let state = ParkedState::new("max_steps", 100).bounded();
        let payload = serde_json::to_value(&state).unwrap();
        assert!(payload.get("next_step").is_none(), "{payload}");
        assert_eq!(payload["remaining_goals"], serde_json::json!([]));
    }

    #[test]
    fn blank_and_repeated_goals_are_kept_once_and_not_at_all_when_blank() {
        let state = ParkedState {
            reason: "max_steps".to_string(),
            percent: 100,
            remaining_goals: vec![
                " run the suite ".to_string(),
                String::new(),
                "run the suite".to_string(),
                "  ".to_string(),
                "fix the parser".to_string(),
            ],
            next_step: Some("   ".to_string()),
            evidence_refs: vec!["v1:passed".to_string(), "v1:passed".to_string()],
        }
        .bounded();

        assert_eq!(state.remaining_goals, ["run the suite", "fix the parser"]);
        assert_eq!(state.next_step, None, "an all-blank next step is no next step");
        assert_eq!(state.evidence_refs, ["v1:passed"]);
    }

    #[test]
    fn the_lists_stop_at_the_bound_a_reader_can_act_on() {
        let goals: Vec<String> = (0..20).map(|n| format!("goal {n}")).collect();
        let evidence: Vec<String> = (0..20).map(|n| format!("v{n}:passed")).collect();
        let state = ParkedState {
            reason: "max_steps".to_string(),
            percent: 100,
            remaining_goals: goals,
            next_step: None,
            evidence_refs: evidence,
        }
        .bounded();

        assert_eq!(state.remaining_goals.len(), ParkedState::MAX_REMAINING_GOALS);
        assert_eq!(state.evidence_refs.len(), ParkedState::MAX_EVIDENCE_REFS);
        assert_eq!(state.remaining_goals[0], "goal 0", "the first written goal survives");
    }
}
