//! Metrics: what a session's log says happened, counted.
//!
//! This is a fold over durable events like the other projections in this crate,
//! and deliberately not a projection that keeps a checkpoint: the numbers
//! describe a whole log rather than the current open entities, they are cheap
//! to rebuild, and nothing is allowed to store them as truth. The log remains
//! the only fact; a document written from these numbers is derived data a
//! rebuild can replace.
//!
//! Every count comes from an event and nothing else. No clock is read, no
//! elapsed time is measured, and two folds of the same events produce the same
//! document byte for byte — which is what makes the numbers comparable between
//! days and between machines, and the only reason to keep them at all.
//!
//! A forked session inherits its parent's prefix into its own log, with the
//! original `event_id`s. [`report`] counts those events inside the child's row
//! — that is what the child's log says — and counts each of them **once** in
//! the totals, because they happened once.

use std::collections::{BTreeMap, BTreeSet};

use agent_protocol::events::{EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, RunId, SessionId, TurnId};
use agent_protocol::tool::{
    APPROVAL_DENIED_OUTCOME, GATE_REFUSED_OUTCOME, GATEWAY_REFUSED_OUTCOME,
};
use chrono::{DateTime, Utc};
use serde::Serialize;

/// Where a report's numbers came from, stated so a file carrying them says so
/// on its own.
pub const DERIVED_FROM: &str = "event-log";

/// The stop reason a `turn/completed` with no recorded reason ended with.
///
/// The Scheduler writes `stop_reason` on the terminal event only when the turn
/// did not end with an ordinary answer (a budget stop, a tool-call cap, a wall
/// clock stop); an ordinary final answer has no field, because there was
/// nothing to explain. A reader that took the absence as "unknown" would count
/// every normal turn as unexplained.
const STOP_FINAL_ANSWER: &str = "final_answer";

/// The key a response gets when the run that made it recorded no model route.
const UNATTRIBUTED_MODEL: &str = "unattributed";

/// Everything one log says happened, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionMetrics {
    /// Events in the log. For a forked session this includes the inherited
    /// prefix, because that is what the log contains.
    pub events: u64,
    /// The first and last `recorded_at` in sequence order. Omitted when the
    /// log is empty, so a `{}` is distinguishable from a zero-length window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_at: Option<DateTime<Utc>>,
    pub turns: TurnMetrics,
    pub runs: RunMetrics,
    pub tools: ToolMetrics,
    pub approvals: ApprovalMetrics,
    pub compactions: CompactionMetrics,
    pub tokens: TokenMetrics,
    /// Token counts per model, keyed `provider/model` as the run's
    /// `profile/resolved` recorded it. A response whose run recorded no route
    /// lands under `unattributed`.
    pub models: BTreeMap<String, ModelMetrics>,
    /// How many times the loop refused to end a turn on an answer it had not
    /// reconciled. This is the one failure-handling signal the event
    /// vocabulary currently records; there is no `failure_action` field yet,
    /// so nothing here claims to count one.
    pub judgment_required: u64,
}

/// Turn counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TurnMetrics {
    pub started: u64,
    /// Distinct started turns that reached a terminal event.
    pub ended: u64,
    /// Started turns with no terminal event yet — the log stops mid-turn.
    pub open: u64,
    /// Terminal events by ending, not distinct turns: a turn that yields and
    /// is later resumed ends twice, and both endings are things that happened.
    pub by_ending: BTreeMap<&'static str, u64>,
    /// Terminal events by stop reason. `turn/completed` copies the reason the
    /// event recorded, or `final_answer` when it recorded none; the other
    /// endings map to the reason their type implies (see [`apply`]).
    pub by_stop_reason: BTreeMap<String, u64>,
}

/// Run counters. Wall time is the sum of closed runs' recorded durations, not
/// the span between the first and last event, so a session that sat idle
/// between two turns does not report the idle time as work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunMetrics {
    pub started: u64,
    pub ended: u64,
    pub open: u64,
    pub wall_time_ms: u64,
}

/// Tool-call counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolMetrics {
    /// Calls that reached `tool/started`. A call refused by the gate or
    /// approved-and-denied never starts, so it is counted in `refused` or
    /// `approval_denied` and not here.
    pub calls: u64,
    pub completed: u64,
    /// Every `tool/failed`, whatever its cause. The two subsets below were not
    /// dispatched at all and are counted separately as well.
    pub failed: u64,
    pub cancelled: u64,
    pub interrupted: u64,
    /// A dispatch whose outcome the log could not settle (crash recovery).
    pub outcome_unknown: u64,
    /// A proposed call the policy or budget refused before dispatch.
    pub not_started: u64,
    /// Subset of `failed`: refused by the gate or the gateway, so nothing ran.
    pub refused: u64,
    /// Subset of `failed`: the person denied the approval, so nothing ran.
    pub approval_denied: u64,
    /// `tool/failed` events that were not refusals, by tool name — calls that
    /// ran (or tried to) and came back unsuccessful.
    pub failures_by_tool: BTreeMap<String, u64>,
}

/// Approval counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ApprovalMetrics {
    pub requested: u64,
    pub granted: u64,
    pub denied: u64,
    pub expired: u64,
    pub cancelled: u64,
}

/// Compaction counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CompactionMetrics {
    pub started: u64,
    pub compacted: u64,
    pub failed: u64,
    /// Compactions that started and recorded no outcome — a process that died
    /// mid-summary. Derived, not an event of its own.
    pub unfinished: u64,
}

/// Token counters, summed over every response that reported usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TokenMetrics {
    /// Model attempts opened.
    pub attempts: u64,
    pub responses: u64,
    /// Responses whose `usage` was reported (`Some`, even if partial). A
    /// provider without the `token_usage` capability reports nothing, and
    /// counts here rather than as zeros.
    pub responses_with_usage: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
}

/// One model's share of the responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModelMetrics {
    pub responses: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// One session's row in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMetricsRow {
    pub session_id: SessionId,
    #[serde(flatten)]
    pub metrics: SessionMetrics,
}

/// Every session's numbers, plus the ones that are not per-session.
///
/// `totals` is folded from the deduplicated union of every log's events, not
/// by adding the rows: a fork inherits its parent's events, and adding rows
/// would count that history twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetricsReport {
    pub derived_from: &'static str,
    /// Sorted by session id so the document does not depend on the order the
    /// data directory was enumerated in.
    pub sessions: Vec<SessionMetricsRow>,
    pub totals: SessionMetrics,
}

/// Fold one session's log.
pub fn fold(events: &[StoredEvent]) -> SessionMetrics {
    let mut metrics = SessionMetrics::default();
    let mut state = FoldState::default();
    for stored in events {
        apply(&mut metrics, &mut state, stored);
    }
    finish(&mut metrics, &state);
    metrics
}

/// Fold every session's log into one report.
///
/// Rows keep whatever their own log contains, inherited prefix included. The
/// totals are folded from the same events with the inherited copies removed by
/// `event_id`, which fork preserves precisely so lineage is traceable.
pub fn report(sessions: Vec<(SessionId, Vec<StoredEvent>)>) -> MetricsReport {
    let mut rows = Vec::with_capacity(sessions.len());
    let mut totals = SessionMetrics::default();
    let mut total_state = FoldState::default();
    let mut counted: BTreeSet<EventId> = BTreeSet::new();

    for (session_id, events) in sessions {
        rows.push(SessionMetricsRow { session_id, metrics: fold(&events) });
        for stored in &events {
            if counted.insert(stored.event.event_id.clone()) {
                apply(&mut totals, &mut total_state, stored);
            }
        }
    }
    finish(&mut totals, &total_state);
    rows.sort_by(|left, right| left.session_id.cmp(&right.session_id));

    MetricsReport { derived_from: DERIVED_FROM, sessions: rows, totals }
}

/// What a fold remembers between events and does not report.
#[derive(Debug, Default)]
struct FoldState {
    started_turns: BTreeSet<TurnId>,
    ended_turns: BTreeSet<TurnId>,
    /// Open runs, by the time their `run/started` was recorded.
    open_runs: BTreeMap<RunId, DateTime<Utc>>,
    /// The model each run was routed to, from its `profile/resolved`.
    run_models: BTreeMap<RunId, String>,
}

fn apply(metrics: &mut SessionMetrics, state: &mut FoldState, stored: &StoredEvent) {
    let event = &stored.event;
    metrics.events += 1;
    if metrics.first_at.is_none() {
        metrics.first_at = Some(stored.recorded_at);
    }
    metrics.last_at = Some(stored.recorded_at);

    match event.event_type.as_str() {
        EventType::PROFILE_RESOLVED => record_route(state, event),
        EventType::RUN_STARTED => {
            metrics.runs.started += 1;
            if let Some(run) = &event.run_id {
                state.open_runs.insert(run.clone(), stored.recorded_at);
            }
        }
        EventType::RUN_COMPLETED
        | EventType::RUN_FAILED
        | EventType::RUN_CANCELLED
        | EventType::RUN_YIELDED
        | EventType::RUN_INTERRUPTED => {
            metrics.runs.ended += 1;
            if let Some(start) = event.run_id.as_ref().and_then(|run| state.open_runs.remove(run)) {
                let elapsed = stored.recorded_at.signed_duration_since(start);
                metrics.runs.wall_time_ms += elapsed.num_milliseconds().max(0) as u64;
            }
        }
        EventType::TURN_STARTED => {
            metrics.turns.started += 1;
            if let Some(turn) = &event.turn_id {
                state.started_turns.insert(turn.clone());
            }
        }
        EventType::TURN_COMPLETED => {
            count_ending(metrics, state, event, "completed", recorded_stop_reason(event));
        }
        EventType::TURN_FAILED => count_ending(metrics, state, event, "failed", "fatal_error"),
        EventType::TURN_CANCELLED => {
            count_ending(metrics, state, event, "cancelled", "user_cancelled")
        }
        EventType::TURN_YIELDED => {
            count_ending(metrics, state, event, "yielded", "needs_user_input")
        }
        EventType::TURN_INTERRUPTED => {
            count_ending(metrics, state, event, "interrupted", "interrupted")
        }
        EventType::TOOL_STARTED => metrics.tools.calls += 1,
        EventType::TOOL_COMPLETED => metrics.tools.completed += 1,
        EventType::TOOL_FAILED => count_tool_failure(metrics, event),
        EventType::TOOL_CANCELLED => metrics.tools.cancelled += 1,
        EventType::TOOL_INTERRUPTED => metrics.tools.interrupted += 1,
        EventType::TOOL_OUTCOME_UNKNOWN => metrics.tools.outcome_unknown += 1,
        EventType::TOOL_NOT_STARTED => metrics.tools.not_started += 1,
        EventType::APPROVAL_REQUESTED => metrics.approvals.requested += 1,
        EventType::APPROVAL_GRANTED => metrics.approvals.granted += 1,
        EventType::APPROVAL_DENIED => metrics.approvals.denied += 1,
        EventType::APPROVAL_EXPIRED => metrics.approvals.expired += 1,
        EventType::APPROVAL_CANCELLED => metrics.approvals.cancelled += 1,
        EventType::CONTEXT_COMPACTION_STARTED => metrics.compactions.started += 1,
        EventType::CONTEXT_COMPACTED => metrics.compactions.compacted += 1,
        EventType::CONTEXT_COMPACTION_FAILED => metrics.compactions.failed += 1,
        EventType::MODEL_ATTEMPT_STARTED => metrics.tokens.attempts += 1,
        EventType::MODEL_RESPONSE_COMPLETED => count_response(metrics, state, event),
        EventType::JUDGMENT_REQUIRED => metrics.judgment_required += 1,
        _ => {}
    }
}

/// What the terminal event itself says the turn stopped for, if it says
/// anything.
fn recorded_stop_reason(event: &EventDraft) -> &str {
    event.payload.get("stop_reason").and_then(|value| value.as_str()).unwrap_or(STOP_FINAL_ANSWER)
}

fn count_ending(
    metrics: &mut SessionMetrics,
    state: &mut FoldState,
    event: &EventDraft,
    ending: &'static str,
    stop_reason: &str,
) {
    *metrics.turns.by_ending.entry(ending).or_default() += 1;
    *metrics.turns.by_stop_reason.entry(stop_reason.to_string()).or_default() += 1;
    // Only a turn this log saw start can end in it. A terminal naming an
    // unknown turn is still counted under its ending — the event is a fact —
    // but it must not make `open` go negative or `ended` exceed `started`.
    if let Some(turn) = &event.turn_id
        && state.started_turns.contains(turn)
    {
        state.ended_turns.insert(turn.clone());
    }
}

fn count_tool_failure(metrics: &mut SessionMetrics, event: &EventDraft) {
    metrics.tools.failed += 1;
    let outcome = event.payload.get("outcome").and_then(|value| value.as_str());
    match outcome {
        Some(GATE_REFUSED_OUTCOME) | Some(GATEWAY_REFUSED_OUTCOME) => {
            metrics.tools.refused += 1;
        }
        Some(APPROVAL_DENIED_OUTCOME) => metrics.tools.approval_denied += 1,
        _ => {
            let tool = event
                .payload
                .get("tool_name")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            *metrics.tools.failures_by_tool.entry(tool.to_string()).or_default() += 1;
        }
    }
}

fn count_response(metrics: &mut SessionMetrics, state: &FoldState, event: &EventDraft) {
    metrics.tokens.responses += 1;
    // The model is counted whether or not usage was reported: "how many
    // responses did this model make" and "what did they cost" are two
    // questions, and a provider that reports no tokens still answered.
    let model = event
        .run_id
        .as_ref()
        .and_then(|run| state.run_models.get(run))
        .cloned()
        .unwrap_or_else(|| UNATTRIBUTED_MODEL.to_string());
    let entry = metrics.models.entry(model).or_default();
    entry.responses += 1;

    let usage = event.payload.get("response").and_then(|response| response.get("usage"));
    let Some(usage) = usage.filter(|usage| !usage.is_null()) else {
        return;
    };
    metrics.tokens.responses_with_usage += 1;
    let input = usage.get("input_tokens").and_then(serde_json::Value::as_u64);
    let output = usage.get("output_tokens").and_then(serde_json::Value::as_u64);
    let total = usage.get("total_tokens").and_then(serde_json::Value::as_u64);
    add(&mut metrics.tokens.input_tokens, input);
    add(&mut metrics.tokens.output_tokens, output);
    add(&mut metrics.tokens.total_tokens, total);
    add(
        &mut metrics.tokens.cache_hit_tokens,
        usage.get("cache_hit_tokens").and_then(serde_json::Value::as_u64),
    );
    add(
        &mut metrics.tokens.cache_miss_tokens,
        usage.get("cache_miss_tokens").and_then(serde_json::Value::as_u64),
    );
    add(
        &mut metrics.tokens.reasoning_tokens,
        usage.get("reasoning_tokens").and_then(serde_json::Value::as_u64),
    );
    add(&mut entry.input_tokens, input);
    add(&mut entry.output_tokens, output);
    add(&mut entry.total_tokens, total);
}

fn record_route(state: &mut FoldState, event: &EventDraft) {
    let Some(run) = &event.run_id else {
        return;
    };
    let Some(route) = event.payload.get("model_route") else {
        return;
    };
    let provider = route.get("provider").and_then(|value| value.as_str());
    let model = route.get("model").and_then(|value| value.as_str());
    if let (Some(provider), Some(model)) = (provider, model) {
        state.run_models.insert(run.clone(), format!("{provider}/{model}"));
    }
}

fn add(target: &mut u64, value: Option<u64>) {
    if let Some(value) = value {
        *target = target.saturating_add(value);
    }
}

/// Fill in the counters that are relations between other counters rather than
/// events of their own.
fn finish(metrics: &mut SessionMetrics, state: &FoldState) {
    metrics.turns.ended = state.ended_turns.len() as u64;
    metrics.turns.open = state.started_turns.difference(&state.ended_turns).count() as u64;
    metrics.runs.open = state.open_runs.len() as u64;
    let settled = metrics.compactions.compacted.saturating_add(metrics.compactions.failed);
    metrics.compactions.unfinished = metrics.compactions.started.saturating_sub(settled);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::Actor;
    use agent_protocol::ids::{EventSeq, SchemaVersion};

    fn draft(
        session: &str,
        event_type: &str,
        run: Option<&str>,
        turn: Option<&str>,
        payload: serde_json::Value,
        at: &str,
    ) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::ZERO,
            recorded_at: at.parse::<DateTime<Utc>>().expect("timestamp"),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::current(),
                session_id: SessionId::from_string(session),
                run_id: run.map(RunId::from_string),
                turn_id: turn.map(TurnId::from_string),
                step_id: None,
                actor: Actor::System,
                created_at: at.parse::<DateTime<Utc>>().expect("timestamp"),
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    fn seq(mut events: Vec<StoredEvent>) -> Vec<StoredEvent> {
        for (index, stored) in events.iter_mut().enumerate() {
            stored.seq = EventSeq::new(index as u64);
        }
        events
    }

    #[test]
    fn an_empty_log_folds_to_zero_and_no_window() {
        let metrics = fold(&[]);
        assert_eq!(metrics, SessionMetrics::default());
        assert_eq!(metrics.turns.started, 0);
        assert!(metrics.first_at.is_none());
    }

    #[test]
    fn turns_are_counted_by_ending_and_stop_reason() {
        let events = seq(vec![
            draft(
                "s",
                EventType::TURN_STARTED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::TURN_COMPLETED,
                None,
                Some("t1"),
                serde_json::json!({ "final_content": "ok" }),
                "2026-01-01T00:00:01Z",
            ),
            draft(
                "s",
                EventType::TURN_STARTED,
                None,
                Some("t2"),
                serde_json::json!({}),
                "2026-01-01T00:00:02Z",
            ),
            draft(
                "s",
                EventType::TURN_COMPLETED,
                None,
                Some("t2"),
                serde_json::json!({ "stop_reason": "max_steps" }),
                "2026-01-01T00:00:03Z",
            ),
            draft(
                "s",
                EventType::TURN_STARTED,
                None,
                Some("t3"),
                serde_json::json!({}),
                "2026-01-01T00:00:04Z",
            ),
        ]);
        let metrics = fold(&events);
        assert_eq!(metrics.turns.started, 3);
        assert_eq!(metrics.turns.ended, 2);
        assert_eq!(metrics.turns.open, 1);
        assert_eq!(metrics.turns.by_ending.get("completed"), Some(&2));
        assert_eq!(metrics.turns.by_stop_reason.get(STOP_FINAL_ANSWER), Some(&1));
        assert_eq!(metrics.turns.by_stop_reason.get("max_steps"), Some(&1));
    }

    #[test]
    fn a_turn_that_yields_and_is_resumed_ends_once_but_is_counted_twice() {
        // The yield is an ending (it closed the turn) and the completion is
        // another. `ended` counts the turn once because it is one turn; the
        // endings table counts both events, because both happened.
        let events = seq(vec![
            draft(
                "s",
                EventType::TURN_STARTED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::TURN_YIELDED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:01Z",
            ),
            draft(
                "s",
                EventType::TURN_RESUMED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:02Z",
            ),
            draft(
                "s",
                EventType::TURN_COMPLETED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:03Z",
            ),
        ]);
        let metrics = fold(&events);
        assert_eq!(metrics.turns.ended, 1);
        assert_eq!(metrics.turns.open, 0);
        assert_eq!(metrics.turns.by_ending.get("yielded"), Some(&1));
        assert_eq!(metrics.turns.by_ending.get("completed"), Some(&1));
    }

    #[test]
    fn a_terminal_for_a_turn_this_log_never_started_does_not_close_another() {
        let events = seq(vec![
            draft(
                "s",
                EventType::TURN_STARTED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::TURN_COMPLETED,
                None,
                Some("ghost"),
                serde_json::json!({}),
                "2026-01-01T00:00:01Z",
            ),
        ]);
        let metrics = fold(&events);
        assert_eq!(metrics.turns.ended, 0);
        assert_eq!(metrics.turns.open, 1);
        assert_eq!(metrics.turns.by_ending.get("completed"), Some(&1));
    }

    #[test]
    fn wall_time_is_the_sum_of_closed_runs_not_the_log_span() {
        let events = seq(vec![
            draft(
                "s",
                EventType::RUN_STARTED,
                Some("r1"),
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::RUN_COMPLETED,
                Some("r1"),
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:02Z",
            ),
            // A long idle gap before the next run must not be reported as work.
            draft(
                "s",
                EventType::RUN_STARTED,
                Some("r2"),
                None,
                serde_json::json!({}),
                "2026-01-01T01:00:00Z",
            ),
            draft(
                "s",
                EventType::RUN_YIELDED,
                Some("r2"),
                None,
                serde_json::json!({}),
                "2026-01-01T01:00:00.500Z",
            ),
            draft(
                "s",
                EventType::RUN_STARTED,
                Some("r3"),
                None,
                serde_json::json!({}),
                "2026-01-01T02:00:00Z",
            ),
        ]);
        let metrics = fold(&events);
        assert_eq!(metrics.runs.started, 3);
        assert_eq!(metrics.runs.ended, 2);
        assert_eq!(metrics.runs.open, 1);
        assert_eq!(metrics.runs.wall_time_ms, 2_500);
    }

    #[test]
    fn refusals_are_failures_but_not_failures_of_a_tool_that_ran() {
        let events = seq(vec![
            draft(
                "s",
                EventType::TOOL_STARTED,
                None,
                None,
                serde_json::json!({ "tool_name": "shell.run" }),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::TOOL_FAILED,
                None,
                None,
                serde_json::json!({ "tool_name": "shell.run", "content": "exit 1" }),
                "2026-01-01T00:00:01Z",
            ),
            draft(
                "s",
                EventType::TOOL_FAILED,
                None,
                None,
                serde_json::json!({ "tool_name": "fs.write", "outcome": GATEWAY_REFUSED_OUTCOME }),
                "2026-01-01T00:00:02Z",
            ),
            draft(
                "s",
                EventType::TOOL_FAILED,
                None,
                None,
                serde_json::json!({ "tool_name": "fs.write", "outcome": APPROVAL_DENIED_OUTCOME }),
                "2026-01-01T00:00:03Z",
            ),
        ]);
        let metrics = fold(&events);
        assert_eq!(metrics.tools.failed, 3);
        assert_eq!(metrics.tools.refused, 1);
        assert_eq!(metrics.tools.approval_denied, 1);
        assert_eq!(metrics.tools.failures_by_tool.get("shell.run"), Some(&1));
        assert!(!metrics.tools.failures_by_tool.contains_key("fs.write"));
        assert_eq!(metrics.tools.calls, 1, "a refused call never started");
    }

    #[test]
    fn tokens_are_attributed_to_the_run_s_route() {
        let events = seq(vec![
            draft(
                "s",
                EventType::PROFILE_RESOLVED,
                Some("r1"),
                None,
                serde_json::json!({ "model_route": { "provider": "openai", "model": "gpt-5" } }),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::MODEL_ATTEMPT_STARTED,
                Some("r1"),
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:01Z",
            ),
            draft(
                "s",
                EventType::MODEL_RESPONSE_COMPLETED,
                Some("r1"),
                None,
                serde_json::json!({ "response": { "usage": { "input_tokens": 10, "output_tokens": 4, "total_tokens": 14, "cache_hit_tokens": 6 } } }),
                "2026-01-01T00:00:02Z",
            ),
            draft(
                "s",
                EventType::MODEL_ATTEMPT_STARTED,
                Some("r2"),
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:03Z",
            ),
            draft(
                "s",
                EventType::MODEL_RESPONSE_COMPLETED,
                Some("r2"),
                None,
                serde_json::json!({ "response": { "usage": null } }),
                "2026-01-01T00:00:04Z",
            ),
        ]);
        let metrics = fold(&events);
        assert_eq!(metrics.tokens.attempts, 2);
        assert_eq!(metrics.tokens.responses, 2);
        assert_eq!(metrics.tokens.responses_with_usage, 1);
        assert_eq!(metrics.tokens.input_tokens, 10);
        assert_eq!(metrics.tokens.cache_hit_tokens, 6);
        let attributed =
            metrics.models.get("openai/gpt-5").expect("the run's route names the model");
        assert_eq!(attributed.responses, 1);
        assert_eq!(attributed.total_tokens, 14);
        let unattributed = metrics.models.get(UNATTRIBUTED_MODEL).expect("a run with no route");
        assert_eq!(unattributed.responses, 1);
    }

    #[test]
    fn an_unfinished_compaction_is_counted_as_such() {
        let events = seq(vec![
            draft(
                "s",
                EventType::CONTEXT_COMPACTION_STARTED,
                None,
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::CONTEXT_COMPACTION_FAILED,
                None,
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:01Z",
            ),
            draft(
                "s",
                EventType::CONTEXT_COMPACTION_STARTED,
                None,
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:02Z",
            ),
        ]);
        let metrics = fold(&events);
        assert_eq!(metrics.compactions.started, 2);
        assert_eq!(metrics.compactions.failed, 1);
        assert_eq!(metrics.compactions.unfinished, 1);
    }

    #[test]
    fn totals_count_an_inherited_prefix_once() {
        let parent = seq(vec![
            draft(
                "parent",
                EventType::RUN_STARTED,
                Some("r1"),
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "parent",
                EventType::RUN_COMPLETED,
                Some("r1"),
                None,
                serde_json::json!({}),
                "2026-01-01T00:00:01Z",
            ),
            draft(
                "parent",
                EventType::TURN_STARTED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "parent",
                EventType::TURN_COMPLETED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:01Z",
            ),
        ]);
        // The child's log begins with the same events (fork preserves the id)
        // and then has one run of its own.
        let mut inherited = parent.clone();
        for stored in &mut inherited {
            stored.event.session_id = SessionId::from_string("child");
        }
        let mut child = inherited;
        let mut own = seq(vec![
            draft(
                "child",
                EventType::RUN_STARTED,
                Some("r2"),
                None,
                serde_json::json!({}),
                "2026-01-02T00:00:00Z",
            ),
            draft(
                "child",
                EventType::RUN_COMPLETED,
                Some("r2"),
                None,
                serde_json::json!({}),
                "2026-01-02T00:00:02Z",
            ),
        ]);
        let base = child.len() as u64;
        for stored in &mut own {
            stored.seq = EventSeq::new(base + stored.seq.as_u64());
        }
        child.extend(own);

        let report = report(vec![
            (SessionId::from_string("parent"), parent),
            (SessionId::from_string("child"), child),
        ]);
        assert_eq!(report.sessions.len(), 2);
        assert_eq!(report.sessions[0].session_id.as_str(), "child");
        assert_eq!(report.totals.turns.started, 1, "the inherited turn is not a second turn");
        assert_eq!(report.totals.runs.started, 2);
        assert_eq!(report.totals.runs.wall_time_ms, 3_000);
        // The child's own row still counts what its log contains.
        assert_eq!(report.sessions[0].metrics.turns.started, 1);
        assert_eq!(report.sessions[0].metrics.runs.started, 2);
    }

    #[test]
    fn a_report_is_byte_identical_when_folded_twice() {
        let events = seq(vec![
            draft(
                "s",
                EventType::TURN_STARTED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:00Z",
            ),
            draft(
                "s",
                EventType::TURN_COMPLETED,
                None,
                Some("t1"),
                serde_json::json!({}),
                "2026-01-01T00:00:01Z",
            ),
        ]);
        let first =
            serde_json::to_string(&report(vec![(SessionId::from_string("s"), events.clone())]))
                .expect("serializes");
        let second = serde_json::to_string(&report(vec![(SessionId::from_string("s"), events)]))
            .expect("serializes");
        assert_eq!(first, second);
    }
}
