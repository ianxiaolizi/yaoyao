//! `agent metrics`: count what the event logs say happened.
//!
//! A reader, like `session list`: it assembles no runtime, opens no model and
//! writes no event. The numbers are folded from the durable logs by
//! [`agent_projection::metrics`]; this module only decides which logs to read
//! and how the answer is written down.
//!
//! This lives here rather than in `agent-cli` for the reason the session verbs
//! do: how sessions are laid out under the data directory is the store's to
//! know, and the command layer is deliberately not allowed to depend on a store
//! adapter.
//!
//! `--out` writes the derived document. It is derived data and says so on its
//! own (`derived_from: "event-log"`): deleting it and folding the same logs
//! again produces the same bytes, so nothing may treat it as a second source of
//! truth.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use agent_cli::{CliOutcome, CliOutput, json_output};
use agent_ports::event_store::EventStore;
use agent_projection::metrics::{self, MetricsReport};
use agent_protocol::ids::{EventSeq, SessionId};

/// Fold the logs under `data_dir` and answer.
///
/// `only` narrows the report to one session. Without it the answer is the whole
/// data directory, because the question this command exists to answer — "is
/// this week better than last?" — is not a per-session one.
pub async fn run_metrics(
    store: &Arc<dyn EventStore>,
    data_dir: &Path,
    only: Option<&SessionId>,
    out: Option<&str>,
    json: bool,
) -> Result<CliOutcome, String> {
    let ids = match only {
        Some(id) => {
            // Asked about a session that is not there: an answer, not an empty
            // report, and never a directory created by looking.
            if !agent_store_jsonl::has_session(data_dir, id.as_str()) {
                return Ok(CliOutcome::did_not_run(CliOutput::Text(format!(
                    "no session {}: nothing to summarise",
                    id.as_str()
                ))));
            }
            vec![id.clone()]
        }
        None => {
            let mut ids = agent_store_jsonl::session_ids(data_dir)
                .map_err(|error| format!("cannot read the data directory: {error}"))?;
            // The directory's enumeration order is the file system's, not the
            // answer's; the report sorts rows anyway, and this keeps the read
            // order stable for anyone watching the command.
            ids.sort();
            ids
        }
    };

    let mut sessions = Vec::with_capacity(ids.len());
    for id in ids {
        let events = store
            .read_stream(&id, EventSeq::ZERO, usize::MAX)
            .await
            .map_err(|error| format!("cannot read session {}: {error}", id.as_str()))?;
        sessions.push((id, events));
    }
    let report = metrics::report(sessions);

    match out {
        Some(path) => write_report(&report, Path::new(path), json),
        None if json => Ok(CliOutcome::ok(CliOutput::Json(json_output(&report)?))),
        None => Ok(CliOutcome::ok(CliOutput::Text(render_text(&report)))),
    }
}

/// Write the derived document, and answer with a receipt.
///
/// The document is always the same pretty JSON the `--json` answer would have
/// been; `--json` here only changes the *receipt*, so the file is one shape
/// whether or not a parser asked for it.
fn write_report(report: &MetricsReport, path: &Path, json: bool) -> Result<CliOutcome, String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let document = json_output(report)?;
    std::fs::write(path, document.as_str())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;

    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({
            "derived_from": report.derived_from,
            "sessions": report.sessions.len(),
            "events": report.totals.events,
            "path": path.display().to_string(),
        }))?)
    } else {
        CliOutput::Text(format!(
            "wrote metrics for {} session(s) to {} (derived from the event log)",
            report.sessions.len(),
            path.display()
        ))
    };
    Ok(CliOutcome::ok(output))
}

/// The human-readable answer: totals first, then one line per session.
fn render_text(report: &MetricsReport) -> String {
    if report.sessions.is_empty() {
        return "no sessions".to_string();
    }
    let totals = &report.totals;
    let mut lines = vec![
        format!(
            "derived from the event log: {} session(s), {} event(s)",
            report.sessions.len(),
            totals.events
        ),
        format!(
            "turns: {} started, {} ended, {} open",
            totals.turns.started, totals.turns.ended, totals.turns.open
        ),
    ];
    if !totals.turns.by_ending.is_empty() {
        lines.push(format!("  endings: {}", pairs(&totals.turns.by_ending)));
    }
    if !totals.turns.by_stop_reason.is_empty() {
        lines.push(format!("  stop reasons: {}", pairs(&totals.turns.by_stop_reason)));
    }
    lines.push(format!(
        "runs: {} started, {} ended, {} open, wall time {}",
        totals.runs.started,
        totals.runs.ended,
        totals.runs.open,
        seconds(totals.runs.wall_time_ms)
    ));

    let tools = &totals.tools;
    let mut line = format!(
        "tools: {} call(s), {} completed, {} failed",
        tools.calls, tools.completed, tools.failed
    );
    for (count, label) in [
        (tools.refused, "refused"),
        (tools.approval_denied, "approval denied"),
        (tools.cancelled, "cancelled"),
        (tools.interrupted, "interrupted"),
        (tools.outcome_unknown, "outcome unknown"),
        (tools.not_started, "not started"),
    ] {
        if count > 0 {
            line.push_str(&format!(", {count} {label}"));
        }
    }
    lines.push(line);
    if !tools.failures_by_tool.is_empty() {
        lines.push(format!("  failed by tool: {}", pairs(&tools.failures_by_tool)));
    }

    let approvals = &totals.approvals;
    let mut line = format!(
        "approvals: {} requested, {} granted, {} denied",
        approvals.requested, approvals.granted, approvals.denied
    );
    if approvals.expired > 0 {
        line.push_str(&format!(", {} expired", approvals.expired));
    }
    if approvals.cancelled > 0 {
        line.push_str(&format!(", {} cancelled", approvals.cancelled));
    }
    lines.push(line);

    let compactions = &totals.compactions;
    let mut line = format!(
        "compactions: {} started, {} compacted, {} failed",
        compactions.started, compactions.compacted, compactions.failed
    );
    if compactions.unfinished > 0 {
        line.push_str(&format!(", {} unfinished", compactions.unfinished));
    }
    lines.push(line);

    let tokens = &totals.tokens;
    let mut line = format!(
        "tokens: {} attempt(s), {}/{} response(s) reported usage: in {}, out {}, total {}",
        tokens.attempts,
        tokens.responses_with_usage,
        tokens.responses,
        tokens.input_tokens,
        tokens.output_tokens,
        tokens.total_tokens
    );
    if tokens.cache_hit_tokens > 0 || tokens.cache_miss_tokens > 0 {
        line.push_str(&format!(
            ", cache hit {}, miss {}",
            tokens.cache_hit_tokens, tokens.cache_miss_tokens
        ));
    }
    if tokens.reasoning_tokens > 0 {
        line.push_str(&format!(", reasoning {}", tokens.reasoning_tokens));
    }
    lines.push(line);

    for (model, metrics) in &totals.models {
        lines.push(format!(
            "model {model}: {} response(s), in {}, out {}, total {}",
            metrics.responses, metrics.input_tokens, metrics.output_tokens, metrics.total_tokens
        ));
    }
    if totals.judgment_required > 0 {
        lines.push(format!("judgment blocks: {}", totals.judgment_required));
    }

    // The work itself, after the bookkeeping: what it cost, what changed, what
    // checked it, and how the log left it.
    lines.push(task_line(
        &totals.task,
        totals.tools.calls,
        &final_label(totals.final_status.as_deref(), totals.turns.open),
    ));

    lines.push("sessions:".to_string());
    for row in &report.sessions {
        let metrics = &row.metrics;
        let mut line = format!(
            "  {}  {} turn(s), {} failed tool(s), {} token(s), {}",
            row.session_id.as_str(),
            metrics.turns.started,
            metrics.tools.failed,
            metrics.tokens.total_tokens,
            seconds(metrics.runs.wall_time_ms)
        );
        // Only where there was work to report: a session that never started a
        // step has no cost to show, and a zero suffix would only add noise.
        if metrics.task.steps > 0
            || metrics.task.edits > 0
            || metrics.task.verifications > 0
            || metrics.task.post_failure_edits > 0
        {
            line.push_str(&format!(
                ", {} step(s), {} tool call(s), {} edit(s), {} verification(s), \
                 {} post-failure edit(s), final {}",
                metrics.task.steps,
                metrics.tools.calls,
                metrics.task.edits,
                metrics.task.verifications,
                metrics.task.post_failure_edits,
                final_label(metrics.final_status.as_deref(), metrics.turns.open),
            ));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// How a task ended, in the report's words.
///
/// A session with no terminal event is `open` when it still has a turn in
/// flight, and `none` when it never started one. The two are different facts:
/// one is work in progress, the other is a log that has no work in it.
fn final_label(status: Option<&str>, open_turns: u64) -> String {
    match status {
        Some(status) => status.to_string(),
        None if open_turns > 0 => "open".to_string(),
        None => "none".to_string(),
    }
}

/// The task totals as one line, with the last change's short digest.
fn task_line(
    task: &agent_projection::metrics::TaskMetrics,
    tool_calls: u64,
    final_status: &str,
) -> String {
    let mut line = format!(
        "task: {} step(s), {tool_calls} tool call(s), {} edit(s), {} verification(s), \
         {} criterion check(s), {} post-failure edit(s), final {final_status}",
        task.steps, task.edits, task.verifications, task.criterion_checks, task.post_failure_edits,
    );
    if let (Some(path), Some(digest)) = (&task.last_modified_path, &task.last_modified_sha256) {
        line.push_str(&format!(", last change {path} {}", short_digest(digest)));
    }
    line
}

/// The first twelve characters of a digest — enough to compare two reports by
/// eye, and always on a character boundary.
fn short_digest(digest: &str) -> String {
    digest.chars().take(12).collect()
}

/// `key=count` pairs in the map's own (sorted) order.
fn pairs<K: std::fmt::Display>(map: &BTreeMap<K, u64>) -> String {
    map.iter().map(|(key, count)| format!("{key}={count}")).collect::<Vec<_>>().join(" ")
}

/// Milliseconds as seconds with three decimals, without a float in the path.
fn seconds(milliseconds: u64) -> String {
    format!("{}.{:03}s", milliseconds / 1000, milliseconds % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft, EventType};
    use agent_protocol::ids::{EventId, SchemaVersion, TurnId};
    use agent_store_jsonl::JsonlEventStore;
    use chrono::{DateTime, Utc};

    fn draft(session: &str, event_type: &str, payload: serde_json::Value) -> EventDraft {
        let now = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().expect("timestamp");
        EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::current(),
            session_id: SessionId::from_string(session),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::User,
            created_at: now,
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: serde_json::json!({}),
            ignorable: false,
        }
    }

    async fn store_with(sessions: &[(&str, Vec<EventDraft>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlEventStore::new(dir.path());
        for (id, drafts) in sessions {
            let session = SessionId::from_string(*id);
            let next = store
                .read_stream(&session, EventSeq::ZERO, usize::MAX)
                .await
                .expect("reads")
                .last()
                .map(|event| event.seq.next())
                .unwrap_or(EventSeq::ZERO);
            store.append(&session, next, drafts.clone()).await.expect("writes");
        }
        dir
    }

    fn text(outcome: &CliOutcome) -> String {
        match &outcome.output {
            CliOutput::Text(text) => text.clone(),
            CliOutput::Json(json) => json.to_string(),
        }
    }

    fn store(dir: &std::path::Path) -> Arc<dyn EventStore> {
        Arc::new(JsonlEventStore::new(dir).with_lease_wait(Duration::ZERO))
    }

    use std::time::Duration;

    #[tokio::test]
    async fn every_session_is_read_and_rows_are_sorted_by_id() {
        let dir = store_with(&[
            ("zeta", vec![draft("zeta", EventType::TURN_STARTED, serde_json::json!({}))]),
            ("alpha", vec![draft("alpha", EventType::TURN_STARTED, serde_json::json!({}))]),
        ])
        .await;
        let outcome =
            run_metrics(&store(dir.path()), dir.path(), None, None, false).await.expect("runs");
        let rendered = text(&outcome);
        assert!(rendered.contains("2 session(s)"), "{rendered}");
        let alpha = rendered.find("alpha").expect("alpha is listed");
        let zeta = rendered.find("zeta").expect("zeta is listed");
        assert!(alpha < zeta, "rows sort by id: {rendered}");
    }

    #[tokio::test]
    async fn the_json_answer_carries_totals_and_one_row_per_session() {
        let mut start = draft("one", EventType::TURN_STARTED, serde_json::json!({}));
        let mut end = draft("one", EventType::TURN_COMPLETED, serde_json::json!({}));
        let turn = TurnId::from_string("t1");
        start.turn_id = Some(turn.clone());
        end.turn_id = Some(turn);
        let dir = store_with(&[("one", vec![start, end])]).await;
        let outcome =
            run_metrics(&store(dir.path()), dir.path(), None, None, true).await.expect("runs");
        let CliOutput::Json(document) = &outcome.output else {
            panic!("--json must answer with one object");
        };
        let value: serde_json::Value = serde_json::from_str(document.as_str()).expect("parses");
        assert_eq!(value["derived_from"], "event-log");
        assert_eq!(value["totals"]["turns"]["started"], 1);
        assert_eq!(value["sessions"][0]["session_id"], "one");
        assert_eq!(value["sessions"][0]["turns"]["ended"], 1);
    }

    #[tokio::test]
    async fn a_named_session_that_does_not_exist_is_reported_and_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_metrics(
            &store(dir.path()),
            dir.path(),
            Some(&SessionId::from_string("ghost")),
            None,
            false,
        )
        .await
        .expect("runs");
        assert_eq!(outcome.verdict, agent_cli::Verdict::DidNotRun);
        assert!(text(&outcome).contains("no session ghost"));
        assert!(!dir.path().join("ghost").exists(), "looking must not create a session");
    }

    #[tokio::test]
    async fn an_empty_data_directory_is_an_empty_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome =
            run_metrics(&store(dir.path()), dir.path(), None, None, false).await.expect("runs");
        assert_eq!(outcome.verdict, agent_cli::Verdict::Answered);
        assert_eq!(text(&outcome), "no sessions");
    }

    #[tokio::test]
    async fn the_text_report_names_the_task_and_the_last_change() {
        let dir = store_with(&[(
            "one",
            vec![
                draft(
                    "one",
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "fs.write" }),
                ),
                draft(
                    "one",
                    EventType::VERIFICATION_RECORDED,
                    serde_json::json!({ "check_id": "c1" }),
                ),
                draft(
                    "one",
                    EventType::WORKSPACE_CHANGES_RECORDED,
                    serde_json::json!({ "changes": [{
                        "relative_path": "web/app.js",
                        "after_sha256": "0123456789abcdef",
                    }] }),
                ),
            ],
        )])
        .await;
        let outcome =
            run_metrics(&store(dir.path()), dir.path(), None, None, false).await.expect("runs");
        let rendered = text(&outcome);
        assert!(
            rendered.contains(
                "task: 0 step(s), 0 tool call(s), 1 edit(s), 1 verification(s), \
                 0 criterion check(s), 0 post-failure edit(s), final none, \
                 last change web/app.js 0123456789ab"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "0 step(s), 0 tool call(s), 1 edit(s), 1 verification(s), \
                 0 post-failure edit(s), final none"
            ),
            "the row has the suffix: {rendered}"
        );
    }

    #[tokio::test]
    async fn the_report_counts_edits_after_a_failed_check_until_a_pass() {
        let dir = store_with(&[(
            "one",
            vec![
                draft("one", EventType::STEP_STARTED, serde_json::json!({})),
                draft(
                    "one",
                    EventType::VERIFICATION_RECORDED,
                    serde_json::json!({ "check_id": "c1", "status": "failed" }),
                ),
                draft(
                    "one",
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "fs.write" }),
                ),
                draft(
                    "one",
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "fs.edit" }),
                ),
                draft(
                    "one",
                    EventType::VERIFICATION_RECORDED,
                    serde_json::json!({ "check_id": "c2", "status": "passed" }),
                ),
                // The pass cleared the state: this edit is ordinary work.
                draft(
                    "one",
                    EventType::TOOL_COMPLETED,
                    serde_json::json!({ "tool_name": "fs.write" }),
                ),
                draft("one", EventType::TURN_COMPLETED, serde_json::json!({})),
            ],
        )])
        .await;
        let outcome =
            run_metrics(&store(dir.path()), dir.path(), None, None, false).await.expect("runs");
        let rendered = text(&outcome);
        assert!(
            rendered.contains(
                "task: 1 step(s), 0 tool call(s), 3 edit(s), 2 verification(s), \
                 0 criterion check(s), 2 post-failure edit(s), final completed:final_answer"
            ),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn the_json_answer_carries_the_task_block_and_omits_a_missing_digest() {
        let dir = store_with(&[(
            "one",
            vec![draft("one", EventType::TURN_STARTED, serde_json::json!({}))],
        )])
        .await;
        let outcome =
            run_metrics(&store(dir.path()), dir.path(), None, None, true).await.expect("runs");
        let CliOutput::Json(document) = &outcome.output else {
            panic!("--json must answer with one object");
        };
        let value: serde_json::Value = serde_json::from_str(document.as_str()).expect("parses");
        assert_eq!(value["totals"]["task"]["steps"], 0);
        assert_eq!(value["totals"]["task"]["edits"], 0);
        assert_eq!(value["totals"]["task"]["verifications"], 0);
        assert_eq!(value["totals"]["task"]["criterion_checks"], 0);
        assert_eq!(value["totals"]["task"]["post_failure_edits"], 0);
        assert!(
            value["totals"]["task"].get("last_modified_path").is_none(),
            "absent digests are omitted"
        );
        assert!(
            value["totals"].get("final_status").is_none(),
            "no terminal event, no invented ending"
        );
    }

    #[tokio::test]
    async fn writing_the_report_twice_produces_the_same_bytes() {
        let dir = store_with(&[(
            "one",
            vec![
                draft("one", EventType::TURN_STARTED, serde_json::json!({})),
                draft("one", EventType::TURN_COMPLETED, serde_json::json!({})),
            ],
        )])
        .await;
        let first = dir.path().join("out/metrics.json");
        let second = dir.path().join("second/metrics.json");
        let store = store(dir.path());
        let one =
            run_metrics(&store, dir.path(), None, Some(first.to_str().expect("utf-8")), false)
                .await
                .expect("writes");
        assert!(text(&one).contains("wrote metrics"), "{}", text(&one));
        let second_run =
            run_metrics(&store, dir.path(), None, Some(second.to_str().expect("utf-8")), false)
                .await
                .expect("writes");
        assert_eq!(second_run.verdict, agent_cli::Verdict::Answered);
        let left = std::fs::read_to_string(&first).expect("reads");
        let right = std::fs::read_to_string(&second).expect("reads");
        assert_eq!(left, right);
        assert!(left.contains("\"derived_from\": \"event-log\""));
    }

    #[tokio::test]
    async fn only_the_named_session_lands_in_the_report() {
        let dir = store_with(&[
            ("one", vec![draft("one", EventType::TURN_STARTED, serde_json::json!({}))]),
            ("two", vec![draft("two", EventType::TURN_STARTED, serde_json::json!({}))]),
        ])
        .await;
        let outcome = run_metrics(
            &store(dir.path()),
            dir.path(),
            Some(&SessionId::from_string("two")),
            None,
            false,
        )
        .await
        .expect("runs");
        let rendered = text(&outcome);
        assert!(rendered.contains("1 session(s)"), "{rendered}");
        assert!(!rendered.contains("  one  "), "{rendered}");
        assert!(rendered.contains("  two  "), "{rendered}");
    }
}
