//! What a parked turn leaves behind, assembled from the log at the moment it
//! stops.
//!
//! The vocabulary is in [`agent_protocol::parking`]; this is the fold behind
//! it. It is deliberately a fold and not a summary request: the model that just
//! spent its budget is the last thing to ask for an account of what is left,
//! and every fact here already exists in the log — the plan's open steps, the
//! model's last written state, and the checks in the evidence ledger.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::parking::ParkedState;
use agent_protocol::plan::PlanItemStatus;

/// The parked record for a turn that stopped on a limit.
///
/// `reason` is the stop reason's own word and `percent` how much of the limit
/// was used. A log whose plan or state will not read back contributes nothing
/// rather than failing: the turn is already ending, and refusing to write the
/// record would lose the facts that *did* read back.
pub fn state_of(events: &[StoredEvent], reason: &str, percent: u32) -> ParkedState {
    let mut state = ParkedState::new(reason, percent);
    // The plan is the intent: its unfinished steps are what the run said it
    // still had to do, in the order it meant to do them.
    if let Ok(Some(plan)) = crate::plan::current(events) {
        state.remaining_goals.extend(
            plan.items
                .iter()
                .filter(|item| item.status != PlanItemStatus::Done)
                .map(|item| item.text.clone()),
        );
    }
    // The work state is what the model last said about itself. `next` is the
    // continuation's starting point; the open questions and the block, if any,
    // are goals the plan does not name.
    if let Ok(Some(work)) = crate::state::current(events) {
        state.next_step = Some(work.next).filter(|next| !next.trim().is_empty());
        state.remaining_goals.extend(work.doing.into_iter().map(|doing| doing.what));
        state.remaining_goals.extend(work.open);
        if let Some(blocked) = work.blocked.filter(|blocked| !blocked.trim().is_empty()) {
            state.remaining_goals.push(format!("blocked: {blocked}"));
        }
    }
    // The ledger's tail: what the run could point at when it stopped. Older
    // checks are history; the most recent ones are what a continuation reads
    // before it decides what to re-run.
    state.evidence_refs = events
        .iter()
        .filter(|stored| stored.event.event_type.as_str() == EventType::VERIFICATION_RECORDED)
        .filter_map(|stored| {
            let payload = &stored.event.payload;
            let check_id = payload.get("check_id")?.as_str()?;
            let status = payload.get("status").and_then(|status| status.as_str()).unwrap_or("?");
            Some(format!("{check_id}:{status}"))
        })
        .collect();
    // The ledger's *tail*, not its head: the newest checks are what a
    // continuation reads before it decides what to re-run.
    let keep_from = state
        .evidence_refs
        .len()
        .saturating_sub(agent_protocol::parking::ParkedState::MAX_EVIDENCE_REFS);
    state.evidence_refs = state.evidence_refs.split_off(keep_from);
    state.bounded()
}

/// How much of a run's budget a limit ending consumed, as a percentage.
///
/// The larger of the step budget and the token budget: a turn can stop on
/// either, and the number beside the parked record should describe the limit
/// that was actually reached. The wall clock is not counted here — the loop
/// measured it against its own monotonic deadline, and the elapsed time is
/// already in the terminal payload's `spent`.
pub fn percent_of(steps: u32, max_steps: Option<u32>, tokens: u64, max_tokens: Option<u64>) -> u32 {
    let steps_percent = max_steps
        .and_then(|max_steps| steps.saturating_mul(100).checked_div(max_steps))
        .unwrap_or(0);
    let tokens_percent = max_tokens
        .filter(|max| *max > 0)
        .map_or(0, |max| u32::try_from(tokens.saturating_mul(100) / max).unwrap_or(100));
    steps_percent.max(tokens_percent).min(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_step_ceiling_contributes_no_percentage() {
        assert_eq!(percent_of(12, None, 0, None), 0);
        assert_eq!(percent_of(12, None, 50, Some(100)), 50);
        assert_eq!(percent_of(2, Some(4), 0, None), 50);
    }
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId};
    use chrono::Utc;

    fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: agent_protocol::ids::EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("s"),
                run_id: None,
                turn_id: None,
                step_id: None,
                actor: Actor::Agent,
                created_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    fn plan(seq: u64) -> StoredEvent {
        stored(
            seq,
            EventType::PLAN_UPDATED,
            serde_json::json!({
                "plan": {
                    "items": [
                        { "text": "read the failing test", "status": "done" },
                        { "text": "fix the tokenizer", "status": "doing" },
                        { "text": "run the suite", "status": "todo" },
                    ],
                },
            }),
        )
    }

    fn work(seq: u64) -> StoredEvent {
        stored(
            seq,
            EventType::STATE_UPDATED,
            serde_json::json!({
                "state": {
                    "goal": "make the suite pass",
                    "next": "run cargo test -p tokenizer",
                    "doing": [{ "what": "fix the tokenizer", "why": "tests fail" }],
                    "open": ["is the fixture stale?"],
                    "blocked": "the fixture needs a network fetch",
                },
            }),
        )
    }

    fn recorded(seq: u64, check_id: &str, status: &str) -> StoredEvent {
        stored(
            seq,
            EventType::VERIFICATION_RECORDED,
            serde_json::json!({
                "check_id": check_id,
                "task_revision": 1,
                "kind": "verifier",
                "status": status,
                "program": "cargo",
                "args": ["test"],
                "exit_code": 0,
                "verifier_identity": "verifier",
                "authorization_source": "operator",
            }),
        )
    }

    #[test]
    fn a_parked_record_names_what_the_run_had_left() {
        let events = [plan(0), work(1), recorded(2, "v1", "failed")];
        let state = state_of(&events, "max_steps", 100);

        assert_eq!(state.reason, "max_steps");
        assert_eq!(state.percent, 100);
        assert_eq!(state.next_step.as_deref(), Some("run cargo test -p tokenizer"));
        assert_eq!(
            state.remaining_goals,
            [
                "fix the tokenizer",
                "run the suite",
                "is the fixture stale?",
                "blocked: the fixture needs a network fetch"
            ],
            "the done step is not a remaining goal; the plan comes before the state"
        );
        assert_eq!(state.evidence_refs, ["v1:failed"]);
    }

    #[test]
    fn a_turn_that_wrote_no_plan_or_state_still_parks() {
        let state = state_of(&[], "max_steps", 100);
        assert_eq!(state.reason, "max_steps");
        assert!(state.remaining_goals.is_empty());
        assert_eq!(state.next_step, None);
        assert!(state.evidence_refs.is_empty());
    }

    #[test]
    fn a_log_that_will_not_read_back_contributes_nothing_and_does_not_fail() {
        // A plan payload written by a build this one cannot read must not cost
        // the turn the parts that do read: the ledger's tail is still reported.
        let events = [
            stored(0, EventType::PLAN_UPDATED, serde_json::json!("junk")),
            recorded(1, "v1", "passed"),
        ];
        let state = state_of(&events, "token_budget_exceeded", 87);
        assert!(state.remaining_goals.is_empty());
        assert_eq!(state.evidence_refs, ["v1:passed"]);
        assert_eq!(state.percent, 87);
    }

    #[test]
    fn only_the_tail_of_the_ledger_is_kept() {
        let events: Vec<StoredEvent> =
            (0..12).map(|n| recorded(n, &format!("v{n}"), "passed")).collect();
        let state = state_of(&events, "max_steps", 100);
        assert_eq!(state.evidence_refs.len(), ParkedState::MAX_EVIDENCE_REFS);
        assert_eq!(state.evidence_refs.last().unwrap(), "v11:passed", "the most recent survives");
    }

    #[test]
    fn the_percent_describes_the_limit_that_was_reached() {
        // Steps at 2 of 2 is 100% even with no token budget configured.
        assert_eq!(percent_of(2, Some(2), 0, None), 100);
        // Halfway through the steps is 50%.
        assert_eq!(percent_of(4, Some(8), 0, None), 50);
        // A token-heavy turn reports the larger of the two.
        assert_eq!(percent_of(1, Some(100), 900, Some(1_000)), 90);
        // Overshoot is still 100%: a percent above it is noise.
        assert_eq!(percent_of(9, Some(8), 0, None), 100);
    }
}
