//! The `schedule` and `tick` commands: time as an input to the queue.
//!
//! `schedule` only appends to the Inbox — it needs a store and a queue, not a
//! model, a tool surface, or a started plugin. That is deliberate: arranging or
//! cancelling a future turn must keep working when the model configuration is
//! broken, for the same reason `plugin` does not assemble a runtime.
//!
//! `tick` is the other side: it assembles nothing itself but takes an assembled
//! [`Scheduler`] and drives at most one turn. The caller owns the clock and
//! passes `now`, which is what keeps the decision reproducible from the log.

use agent_kernel::approval::ApprovalService;
use agent_kernel::inbox::{Inbox, scheduled_due_at};
use agent_kernel::scheduler::{RunReport, Scheduler, SchedulerError};
use agent_protocol::events::Actor;
use agent_protocol::ids::{InboxItemId, SessionId, TurnId};
use chrono::{DateTime, Duration, Utc};

use crate::{CliOutcome, CliOutput, json_output};

/// How long `serve` waits when nothing is due before looking again.
pub const SERVE_POLL: Duration = Duration::seconds(30);

/// `schedule at <when> <text>`: queue one turn for `due_at`.
pub async fn run_schedule_at(
    inbox: &Inbox,
    session_id: &SessionId,
    when: &str,
    text: &str,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    let due_at = agent_kernel::schedule::parse_when(when, now).map_err(|e| e.to_string())?;
    let item = inbox
        .schedule(session_id, due_at, text, 0, Actor::User)
        .await
        .map_err(|e| e.to_string())?;

    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({
            "item_id": item.item_id.as_str(),
            "due_at": due_at.to_rfc3339(),
            "text": item.payload["text"],
        }))?)
    } else {
        CliOutput::Text(format!("scheduled {} at {}", item.item_id, due_at.to_rfc3339()))
    };
    Ok(CliOutcome::ok(output))
}

/// `schedule list`: the pending scheduled items, with whether they are overdue.
pub async fn run_schedule_list(
    inbox: &Inbox,
    session_id: &SessionId,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    let items = inbox.scheduled(session_id).await.map_err(|e| e.to_string())?;

    let rows: Vec<serde_json::Value> = items
        .iter()
        .map(|item| {
            let due_at = scheduled_due_at(item);
            let text = item.payload.get("text").and_then(|v| v.as_str());
            match due_at {
                Ok(due_at) => serde_json::json!({
                    "item_id": item.item_id.as_str(),
                    "due_at": due_at.to_rfc3339(),
                    "overdue": due_at <= now,
                    "text": text,
                }),
                Err(reason) => serde_json::json!({
                    "item_id": item.item_id.as_str(),
                    "due_at": null,
                    "overdue": false,
                    "text": text,
                    "error": reason,
                }),
            }
        })
        .collect();

    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({ "items": rows }))?)
    } else if rows.is_empty() {
        CliOutput::Text("no scheduled items".to_string())
    } else {
        let lines = items
            .iter()
            .map(|item| {
                let due = match scheduled_due_at(item) {
                    Ok(due_at) => {
                        let state = if due_at <= now { " (overdue)" } else { "" };
                        format!("{}{state}", due_at.to_rfc3339())
                    }
                    Err(reason) => format!("[unreadable: {reason}]"),
                };
                let text = item.payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("{}  {due}  {text}", item.item_id)
            })
            .collect::<Vec<_>>();
        CliOutput::Text(lines.join("\n"))
    };
    Ok(CliOutcome::ok(output))
}

/// `schedule cancel <id>`: withdraw a pending scheduled item.
///
/// A refusal is `DidNotRun`, not a failure: nothing was cancelled, which is
/// exactly the "no answer" the exit status `2` names. The reason is still the
/// answer to the question that was asked, so it goes to stdout.
pub async fn run_schedule_cancel(
    inbox: &Inbox,
    session_id: &SessionId,
    item_id: &str,
    json: bool,
) -> Result<CliOutcome, String> {
    let id = InboxItemId::from_string(item_id);
    match inbox.cancel_scheduled(session_id, &id).await {
        Ok(()) => Ok(CliOutcome::ok(if json {
            CliOutput::Json(json_output(&serde_json::json!({
                "cancelled": id.as_str(),
            }))?)
        } else {
            CliOutput::Text(format!("cancelled {id}"))
        })),
        Err(e) => Ok(CliOutcome::did_not_run(if json {
            CliOutput::Json(json_output(&serde_json::json!({
                "cancelled": null,
                "reason": e.to_string(),
            }))?)
        } else {
            CliOutput::Text(format!("nothing cancelled: {e}"))
        })),
    }
}

/// What one tick did.
///
/// Structured rather than rendered so a long-running driver (`serve`) can tell
/// the three outcomes apart without reading back the text a single tick would
/// have printed — in particular so "entered the waiting state" can be announced
/// once instead of once per poll.
#[derive(Debug)]
pub enum Tick {
    /// A turn ran, and this is what it produced.
    Ran(RunReport),
    /// The session is held at a decision. Nothing ran; the item stays pending.
    AwaitingDecision { turn_id: TurnId },
    /// Nothing was due and there was no pending input.
    Idle,
}

/// `tick`: sweep expired approvals, then run at most one turn.
///
/// The order is the point. An approval whose time ran out is settled before any
/// turn starts, so a resumed turn sees "expired" rather than "still waiting".
/// A pending `next_turn` is explicit user intent and wins over a scheduled item;
/// a scheduled item only runs when there is no `next_turn` at all.
///
/// Exit status follows the repository's alphabet: `0` when a turn ran, `2` when
/// none did (nothing pending, nothing due, or a held session). There is no
/// "ran and the answer was no" here — the turn's own outcome is its answer.
pub async fn run_tick(
    scheduler: &Scheduler,
    approvals: &ApprovalService,
    session_id: &SessionId,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    match tick_once(scheduler, approvals, session_id, now).await? {
        Tick::Ran(report) => render_ran(report, json),
        Tick::AwaitingDecision { turn_id } => {
            render_did_not_run(&format!("turn {turn_id} is waiting on a decision"), json)
        }
        Tick::Idle => {
            render_did_not_run("no turn ran: nothing was due and there is no pending input", json)
        }
    }
}

/// Drive at most one turn, and say what happened without rendering it.
///
/// The single tick [`run_tick`] renders, split out for `serve`: a daemon needs
/// to distinguish "ran a turn", "waiting on a decision" and "nothing to do"
/// before deciding how long to sleep, and reparsing its own output to find out
/// would make the text part of the contract.
pub async fn tick_once(
    scheduler: &Scheduler,
    approvals: &ApprovalService,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Result<Tick, String> {
    approvals.sweep_expired(session_id).await.map_err(|e| e.to_string())?;

    let report = match scheduler.run_pending(session_id, now).await {
        Ok(report) => report,
        Err(SchedulerError::AwaitingDecision { turn_id }) => {
            return Ok(Tick::AwaitingDecision { turn_id });
        }
        Err(e) => return Err(e.to_string()),
    };
    let report = match report {
        Some(report) => Some(report),
        None => match scheduler.run_due(session_id, now).await {
            Ok(report) => report,
            Err(SchedulerError::AwaitingDecision { turn_id }) => {
                return Ok(Tick::AwaitingDecision { turn_id });
            }
            Err(e) => return Err(e.to_string()),
        },
    };

    Ok(match report {
        Some(report) => Tick::Ran(report),
        None => Tick::Idle,
    })
}

/// The earliest due time among the pending scheduled items, if any is readable.
///
/// Unreadable items are ignored rather than reported: the tick that encounters
/// one discards it, and the wait should not be shortened by an item that is
/// about to be thrown away. An item whose `due_at` is in the past still counts —
/// it is due now, not absent.
pub async fn earliest_due(
    inbox: &Inbox,
    session_id: &SessionId,
) -> Result<Option<DateTime<Utc>>, String> {
    let items = inbox.scheduled(session_id).await.map_err(|e| e.to_string())?;
    Ok(items.iter().filter_map(|item| scheduled_due_at(item).ok()).min())
}

/// What a tick that ran a turn says.
pub fn render_ran(report: RunReport, json: bool) -> Result<CliOutcome, String> {
    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({
            "ran": true,
            "run_id": report.run_id.as_str(),
            "turn_id": report.turn_id.as_str(),
            "stop_reason": report.stop_reason,
            "final_content": report.final_content,
        }))?)
    } else {
        CliOutput::Text(match report.final_content {
            Some(content) => content,
            None => format!("[turn ended: {:?}]", report.stop_reason),
        })
    };
    Ok(CliOutcome::ok(output))
}

/// What a tick that ran nothing says.
pub fn render_did_not_run(reason: &str, json: bool) -> Result<CliOutcome, String> {
    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({ "ran": false, "reason": reason }))?)
    } else {
        CliOutput::Text(reason.to_string())
    };
    Ok(CliOutcome::did_not_run(output))
}

/// How long a `serve` loop should sleep before looking again.
///
/// Zero when something is already due, so a queue of due items drains without
/// an artificial wait. Otherwise the time until the next due item, capped at
/// [`SERVE_POLL`] so a schedule written by another process is noticed without
/// a long sleep. Pure, so the timing rule is testable without sleeping.
pub fn next_wait(earliest: Option<DateTime<Utc>>, now: DateTime<Utc>, poll: Duration) -> Duration {
    match earliest {
        Some(due) if due <= now => Duration::zero(),
        Some(due) => (due - now).min(poll),
        None => poll,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(when: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(when).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn an_overdue_or_due_item_does_not_wait() {
        let now = at("2026-10-18T09:00:00Z");
        assert_eq!(next_wait(Some(at("2026-10-18T08:59:00Z")), now, SERVE_POLL), Duration::zero());
        assert_eq!(next_wait(Some(now), now, SERVE_POLL), Duration::zero());
    }

    /// A due scheduled item and a pending `next_turn` in the same queue: the
    /// explicit request is the one that runs, and the scheduled item survives.
    #[tokio::test]
    async fn a_pending_turn_runs_before_a_due_schedule() {
        use std::sync::Arc;

        use agent_kernel::scheduler::Scheduler;
        use agent_loop::DefaultAgentLoop;
        use agent_protocol::ids::SessionId;
        use agent_protocol::inbox::InboxKind;
        use agent_testkit::{
            FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider,
            NullContextEngine, one_model_router,
        };

        let store = Arc::new(FakeEventStore::new());
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::answer("explicit"),
            FakeModelProvider::answer("scheduled"),
        ]));
        let loop_ = Arc::new(DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        let scheduler = Scheduler::new(store.clone(), loop_, 8);
        let approvals = ApprovalService::new(store);
        let sid = SessionId::from_string("tick-session");

        scheduler
            .inbox()
            .schedule(&sid, Utc::now() - Duration::seconds(1), "scheduled", 0, Actor::User)
            .await
            .unwrap();
        scheduler
            .inbox()
            .append(
                &sid,
                InboxKind::NextTurn,
                serde_json::json!({ "text": "explicit" }),
                0,
                Actor::User,
            )
            .await
            .unwrap();

        let outcome = run_tick(&scheduler, &approvals, &sid, Utc::now(), false).await.unwrap();
        assert_eq!(outcome.verdict, crate::Verdict::Answered);
        match &outcome.output {
            CliOutput::Text(text) => assert_eq!(text, "explicit"),
            CliOutput::Json(_) => panic!("expected text output"),
        }
        assert_eq!(
            scheduler.inbox().scheduled(&sid).await.unwrap().len(),
            1,
            "the scheduled item waits for a tick with no explicit work"
        );
    }

    /// An item the loop cannot read never sets the wait: it is discarded by the
    /// tick that finds it, and sleeping until its (nonexistent) due time would
    /// make a bad item delay the whole queue.
    #[tokio::test]
    async fn an_unreadable_item_does_not_shorten_the_wait() {
        use std::sync::Arc;

        use agent_kernel::inbox::Inbox;
        use agent_protocol::ids::SessionId;
        use agent_protocol::inbox::InboxKind;
        use agent_testkit::FakeEventStore;

        let store = Arc::new(FakeEventStore::new());
        let inbox = Inbox::new(store);
        let sid = SessionId::from_string("wait-session");
        let due = at("2026-10-18T10:00:00Z");

        inbox.schedule(&sid, due, "the readable one", 0, Actor::User).await.unwrap();
        inbox
            .append(
                &sid,
                InboxKind::Scheduled,
                serde_json::json!({ "text": "no due_at at all" }),
                0,
                Actor::User,
            )
            .await
            .unwrap();

        assert_eq!(earliest_due(&inbox, &sid).await.unwrap(), Some(due));
    }

    #[test]
    fn a_future_item_waits_at_most_the_poll_interval() {
        let now = at("2026-10-18T09:00:00Z");
        assert_eq!(
            next_wait(Some(at("2026-10-18T09:00:05Z")), now, SERVE_POLL),
            Duration::seconds(5)
        );
        assert_eq!(
            next_wait(Some(at("2026-10-18T10:00:00Z")), now, SERVE_POLL),
            SERVE_POLL,
            "a distant item is checked at the poll interval, not slept through"
        );
        assert_eq!(next_wait(None, now, SERVE_POLL), SERVE_POLL);
    }
}
