//! The three status axes, folded from the log.
//!
//! The words live in [`agent_protocol::status`]; this module answers which one
//! is true for a session. It is a read-only fold with no cache and no writes,
//! like [`crate::lifecycle`] and [`crate::intent`]: a reader asks the log, and
//! two readers of one log answer the same thing.
//!
//! One rule shapes the whole fold: **completion is derived, never recorded.**
//! No event says "this task is complete". The answer is a function of the run's
//! ending and the verification facts around it, so a turn that ends after a
//! passing completion check is complete and a turn that ends because the model
//! stopped talking is not — even though both write `turn/completed`.
//!
//! The verification axis is deliberately conservative before the evidence
//! ledger (slice A5) exists: any tool event after a passing check marks that
//! check [`Stale`](agent_protocol::status::VerificationStatus::Stale), whether
//! or not the tool wrote anything. A reader that reports a stale check as
//! current is the failure this axis exists to prevent (CONTRACT §7 #167).

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::status::{CompletionStatus, DeliveryStatus, RunStatus, VerificationStatus};
use serde_json::Value;

use crate::scheduler::{Scheduler, SchedulerError};

/// The three axes and the completion claim they derive, for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusAxes {
    /// The latest run's state. `None` is "this session never opened a run" —
    /// not "the run is fine".
    pub run_status: Option<RunStatus>,
    pub delivery_status: DeliveryStatus,
    pub verification_status: VerificationStatus,
    /// The derived claim. `None` when there is nothing to claim: no run, a run
    /// still working, or an information answer that needs no machine criterion.
    pub completion_status: Option<CompletionStatus>,
}

impl Scheduler {
    /// Fold this session's log into its status axes.
    ///
    /// A read like [`Scheduler::lifecycle`]: nothing is written and no lease
    /// is taken, so `status` can be asked while another process drives the
    /// session.
    pub async fn status_axes(&self, session_id: &SessionId) -> Result<StatusAxes, SchedulerError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(derive(&events))
    }
}

/// The latest run, as the fold needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunView {
    status: RunStatus,
    /// Whether the run yielded and is waiting for a decision or an input.
    yielded: bool,
    /// Where the latest `run/started` sits, for "did this run do any work".
    start_seq: EventSeq,
}

/// Derive the status axes from a session's events.
pub fn derive(events: &[StoredEvent]) -> StatusAxes {
    let run = latest_run(events);
    let delivery_status = delivery(events);
    let verification_status = verification(events, run.as_ref());
    let completion_status = completion(run.as_ref(), verification_status);
    StatusAxes {
        run_status: run.map(|run| run.status),
        delivery_status,
        verification_status,
        completion_status,
    }
}

/// Fold the latest run: the last `run/started`, then its own terminal event.
///
/// A log whose latest start has no terminal is a run that never closed — a
/// process that died mid-run, or one still going — and it stays in progress
/// until recovery closes it. The endpoint's closing by id is the same
/// compare-and-set [`crate::lifecycle`] uses, so a late event for an older run
/// cannot close a newer one.
fn latest_run(events: &[StoredEvent]) -> Option<RunView> {
    let start = events
        .iter()
        .rposition(|stored| stored.event.event_type.as_str() == EventType::RUN_STARTED)?;
    let run_id = events[start].event.run_id.clone();
    let mut view =
        RunView { status: RunStatus::InProgress, yielded: false, start_seq: events[start].seq };
    for stored in &events[start + 1..] {
        if stored.event.run_id != run_id {
            continue;
        }
        view.status = match stored.event.event_type.as_str() {
            EventType::RUN_COMPLETED => RunStatus::Completed,
            EventType::RUN_FAILED => RunStatus::Failed,
            EventType::RUN_CANCELLED | EventType::RUN_INTERRUPTED => RunStatus::Interrupted,
            EventType::RUN_YIELDED => {
                view.yielded = true;
                RunStatus::InProgress
            }
            _ => continue,
        };
    }
    Some(view)
}

/// Whether the latest settled turn handed the user an answer.
///
/// The turn's terminal payload is the fact: `final_content` is what a front
/// end showed. Text that ends a turn without being the answer — a token budget,
/// a stall, a provider cut, a content filter — is excluded by the `incomplete`
/// marker the loop writes and by the stop reason beside it (CONTRACT §7
/// #131–#132). Only the *latest* settled turn counts: an older answer is
/// reported through the intent axis's staleness, not as the current delivery.
fn delivery(events: &[StoredEvent]) -> DeliveryStatus {
    let Some(end) = events.iter().rev().find(|stored| {
        matches!(
            stored.event.event_type.as_str(),
            EventType::TURN_COMPLETED
                | EventType::TURN_FAILED
                | EventType::TURN_CANCELLED
                | EventType::TURN_INTERRUPTED
                | EventType::TURN_YIELDED
        )
    }) else {
        return DeliveryStatus::Unanswered;
    };
    if end.event.event_type.as_str() != EventType::TURN_COMPLETED {
        return DeliveryStatus::Unanswered;
    }
    let payload = &end.event.payload;
    let content = payload.get("final_content").and_then(Value::as_str).unwrap_or_default().trim();
    let incomplete = payload.get("incomplete").and_then(Value::as_bool).unwrap_or(false);
    let partial = matches!(
        payload.get("stop_reason").and_then(Value::as_str),
        Some("max_output_tokens" | "content_filtered" | "token_budget_exceeded" | "stalled")
    );
    if content.is_empty() || incomplete || partial {
        DeliveryStatus::Unanswered
    } else {
        DeliveryStatus::Delivered
    }
}

/// Whether the work on the table is verified, and whether the verification is
/// still fresh.
///
/// The latest check is the operator's criterion (`completion/checked`). Its
/// payload's `status` is `completed`, `incomplete`, or `error`:
///
/// * `completed` — the criterion passed. Fresh unless a tool ran after it.
/// * `error` — the check itself could not run. A failed verifier is not a
///   failed task, but nothing may be claimed either, so this is `Blocked`.
/// * `incomplete` — the criterion exists and does not yet pass.
///
/// "Tool ran after" is the pre-A5 stand-in for "something persisted changed";
/// it is conservative in the only safe direction.
fn verification(events: &[StoredEvent], run: Option<&RunView>) -> VerificationStatus {
    let Some(run) = run else {
        return VerificationStatus::NotApplicable;
    };
    match run.status {
        // A run still working has nothing fresh to check yet. This includes a
        // run parked on an approval: the result it would be checked against
        // does not exist.
        RunStatus::InProgress => VerificationStatus::Unverified,
        // A failed or interrupted run did not produce a result to verify. That
        // is a refusal to claim, not a failed check.
        RunStatus::Failed | RunStatus::Interrupted => VerificationStatus::Blocked,
        RunStatus::Completed => {
            let last_check = events.iter().rev().find(|stored| is_check(stored));
            let last_pass = events
                .iter()
                .rev()
                .find(|stored| is_check(stored) && check_status(stored) == Some("completed"));
            let Some(check) = last_check else {
                // No criterion was ever asked. A run that touched no tools is
                // an information answer; one that did tool work is unverified.
                return if tool_since(events, run.start_seq) {
                    VerificationStatus::Unverified
                } else {
                    VerificationStatus::NotApplicable
                };
            };
            match check_status(check) {
                Some("completed") => {
                    if tool_since(events, check.seq) {
                        VerificationStatus::Stale
                    } else {
                        VerificationStatus::Verified
                    }
                }
                Some("error") => VerificationStatus::Blocked,
                _ => {
                    // Incomplete, or a status this build has no name for. An
                    // older pass that later work has run past is stale; a pass
                    // that nothing has followed is not a claim about this run
                    // either, but it is not false — the check still stands.
                    match last_pass {
                        Some(pass) if tool_since(events, pass.seq) => VerificationStatus::Stale,
                        Some(_) => VerificationStatus::Verified,
                        None => VerificationStatus::Unverified,
                    }
                }
            }
        }
    }
}

/// The derived completion claim.
///
/// Only a completed run with fresh verification is `VerifiedComplete`.
/// Everything that looked finished without meeting that bar is
/// `BlockedOrParked`: there is no `unverified_complete` in this vocabulary.
/// An information answer (`NotApplicable`) makes no completion claim at all —
/// it is delivered, and saying more would count it as work that was checked.
fn completion(run: Option<&RunView>, verification: VerificationStatus) -> Option<CompletionStatus> {
    let run = run?;
    match run.status {
        RunStatus::InProgress => {
            if run.yielded {
                Some(CompletionStatus::BlockedOrParked)
            } else {
                None
            }
        }
        RunStatus::Failed | RunStatus::Interrupted => Some(CompletionStatus::BlockedOrParked),
        RunStatus::Completed => match verification {
            VerificationStatus::Verified => Some(CompletionStatus::VerifiedComplete),
            VerificationStatus::Stale
            | VerificationStatus::Blocked
            | VerificationStatus::Unverified => Some(CompletionStatus::BlockedOrParked),
            VerificationStatus::NotApplicable => None,
        },
    }
}

fn is_check(stored: &StoredEvent) -> bool {
    stored.event.event_type.as_str() == EventType::COMPLETION_CHECKED
}

fn check_status(stored: &StoredEvent) -> Option<&str> {
    stored.event.payload.get("status").and_then(Value::as_str)
}

/// Whether a tool call happened after `seq`.
///
/// Every `tool/*` event counts except `tool/not_started`: that one is the
/// log's own word for a call the kernel refused before dispatch, so it is the
/// one event in the family that provably changed nothing (CONTRACT §7 #132).
fn tool_since(events: &[StoredEvent], seq: EventSeq) -> bool {
    events.iter().any(|stored| {
        stored.seq > seq
            && matches!(
                stored.event.event_type.as_str(),
                EventType::TOOL_STARTED
                    | EventType::TOOL_COMPLETED
                    | EventType::TOOL_FAILED
                    | EventType::TOOL_CANCELLED
                    | EventType::TOOL_INTERRUPTED
                    | EventType::TOOL_OUTCOME_UNKNOWN
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventId, RunId, SchemaVersion, TurnId};
    use chrono::Utc;

    fn stored_with_run(
        seq: u64,
        event_type: &str,
        payload: serde_json::Value,
        run: Option<&str>,
    ) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("s"),
                run_id: run.map(RunId::from_string),
                turn_id: Some(TurnId::from_string("t")),
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

    fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
        stored_with_run(seq, event_type, payload, None)
    }

    fn run_event(seq: u64, event_type: &str) -> StoredEvent {
        stored_with_run(seq, event_type, serde_json::json!({}), Some("r1"))
    }

    fn answered(seq: u64, content: &str) -> StoredEvent {
        stored(seq, EventType::TURN_COMPLETED, serde_json::json!({ "final_content": content }))
    }

    fn check(seq: u64, status: &str) -> StoredEvent {
        stored(seq, EventType::COMPLETION_CHECKED, serde_json::json!({ "status": status }))
    }

    fn tool(seq: u64) -> StoredEvent {
        stored(seq, EventType::TOOL_COMPLETED, serde_json::json!({ "tool_name": "fs.write" }))
    }

    #[test]
    fn a_session_without_a_run_has_no_status_claim() {
        let axes = derive(&[]);
        assert_eq!(axes.run_status, None);
        assert_eq!(axes.delivery_status, DeliveryStatus::Unanswered);
        assert_eq!(axes.verification_status, VerificationStatus::NotApplicable);
        assert_eq!(axes.completion_status, None);
    }

    #[test]
    fn a_run_that_has_not_closed_is_in_progress() {
        let axes = derive(&[run_event(0, EventType::RUN_STARTED)]);
        assert_eq!(axes.run_status, Some(RunStatus::InProgress));
        assert_eq!(axes.verification_status, VerificationStatus::Unverified);
        assert_eq!(axes.completion_status, None, "nothing has ended, so nothing is complete");
    }

    #[test]
    fn an_information_answer_is_delivered_without_a_completion_claim() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            answered(1, "hello"),
            run_event(2, EventType::RUN_COMPLETED),
        ];
        let axes = derive(&events);
        assert_eq!(axes.run_status, Some(RunStatus::Completed));
        assert_eq!(axes.delivery_status, DeliveryStatus::Delivered);
        assert_eq!(axes.verification_status, VerificationStatus::NotApplicable);
        assert_eq!(
            axes.completion_status, None,
            "an information answer is delivered, not completed: nothing was verified"
        );
    }

    #[test]
    fn a_partial_answer_is_not_delivered() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            stored(
                1,
                EventType::TURN_COMPLETED,
                serde_json::json!({
                    "final_content": "half a sentence",
                    "stop_reason": "max_output_tokens",
                }),
            ),
            run_event(2, EventType::RUN_COMPLETED),
        ];
        assert_eq!(derive(&events).delivery_status, DeliveryStatus::Unanswered);
    }

    #[test]
    fn an_incomplete_marker_keeps_partial_text_from_counting() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            stored(
                1,
                EventType::TURN_COMPLETED,
                serde_json::json!({
                    "final_content": "half a sentence",
                    "incomplete": true,
                }),
            ),
            run_event(2, EventType::RUN_COMPLETED),
        ];
        assert_eq!(derive(&events).delivery_status, DeliveryStatus::Unanswered);
    }

    #[test]
    fn a_failed_run_is_blocked_and_never_complete() {
        let events =
            vec![run_event(0, EventType::RUN_STARTED), run_event(1, EventType::RUN_FAILED)];
        let axes = derive(&events);
        assert_eq!(axes.run_status, Some(RunStatus::Failed));
        assert_eq!(axes.delivery_status, DeliveryStatus::Unanswered);
        assert_eq!(axes.verification_status, VerificationStatus::Blocked);
        assert_eq!(axes.completion_status, Some(CompletionStatus::BlockedOrParked));
    }

    #[test]
    fn an_interrupted_run_is_interrupted_not_failed() {
        let events =
            vec![run_event(0, EventType::RUN_STARTED), run_event(1, EventType::RUN_CANCELLED)];
        assert_eq!(derive(&events).run_status, Some(RunStatus::Interrupted));
    }

    #[test]
    fn a_yielded_run_is_in_progress_and_parked() {
        let events =
            vec![run_event(0, EventType::RUN_STARTED), run_event(1, EventType::RUN_YIELDED)];
        let axes = derive(&events);
        assert_eq!(axes.run_status, Some(RunStatus::InProgress));
        assert_eq!(axes.completion_status, Some(CompletionStatus::BlockedOrParked));
    }

    #[test]
    fn a_passing_check_is_verified_complete() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            check(1, "completed"),
            stored(2, EventType::TURN_COMPLETED, serde_json::json!({})),
            run_event(3, EventType::RUN_COMPLETED),
        ];
        let axes = derive(&events);
        assert_eq!(axes.run_status, Some(RunStatus::Completed));
        assert_eq!(axes.verification_status, VerificationStatus::Verified);
        assert_eq!(axes.completion_status, Some(CompletionStatus::VerifiedComplete));
    }

    #[test]
    fn a_tool_after_the_check_makes_it_stale() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            check(1, "completed"),
            tool(2),
            stored(3, EventType::TURN_COMPLETED, serde_json::json!({})),
            run_event(4, EventType::RUN_COMPLETED),
        ];
        let axes = derive(&events);
        assert_eq!(axes.verification_status, VerificationStatus::Stale);
        assert_eq!(axes.completion_status, Some(CompletionStatus::BlockedOrParked));
    }

    #[test]
    fn a_check_that_could_not_run_blocks_the_claim() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            check(1, "incomplete"),
            check(2, "error"),
            run_event(3, EventType::RUN_COMPLETED),
        ];
        assert_eq!(derive(&events).verification_status, VerificationStatus::Blocked);
    }

    #[test]
    fn an_incomplete_check_is_unverified_with_no_completion() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            tool(1),
            check(2, "incomplete"),
            stored(3, EventType::TURN_COMPLETED, serde_json::json!({ "final_content": "done" })),
            run_event(4, EventType::RUN_COMPLETED),
        ];
        let axes = derive(&events);
        assert_eq!(axes.verification_status, VerificationStatus::Unverified);
        assert_eq!(axes.completion_status, Some(CompletionStatus::BlockedOrParked));
    }

    #[test]
    fn work_without_a_criterion_is_unverified_not_complete() {
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            tool(1),
            answered(2, "all done"),
            run_event(3, EventType::RUN_COMPLETED),
        ];
        let axes = derive(&events);
        assert_eq!(axes.delivery_status, DeliveryStatus::Delivered);
        assert_eq!(axes.verification_status, VerificationStatus::Unverified);
        assert_eq!(axes.completion_status, Some(CompletionStatus::BlockedOrParked));
    }

    #[test]
    fn a_not_started_tool_is_not_a_change() {
        // `tool/not_started` is the log's word for a call that never ran, so a
        // passing check followed only by one of those is still fresh.
        let events = vec![
            run_event(0, EventType::RUN_STARTED),
            check(1, "completed"),
            stored(2, EventType::TOOL_NOT_STARTED, serde_json::json!({ "tool_name": "fs.write" })),
            stored(3, EventType::TURN_COMPLETED, serde_json::json!({})),
            run_event(4, EventType::RUN_COMPLETED),
        ];
        assert_eq!(derive(&events).verification_status, VerificationStatus::Verified);
    }

    #[test]
    fn an_older_run_cannot_close_a_newer_one() {
        let events = vec![
            stored_with_run(0, EventType::RUN_STARTED, serde_json::json!({}), Some("old")),
            stored_with_run(1, EventType::RUN_STARTED, serde_json::json!({}), Some("new")),
            stored_with_run(2, EventType::RUN_COMPLETED, serde_json::json!({}), Some("old")),
        ];
        assert_eq!(
            derive(&events).run_status,
            Some(RunStatus::InProgress),
            "the late 'old' terminal event must not close the run that started after it"
        );
    }
}
