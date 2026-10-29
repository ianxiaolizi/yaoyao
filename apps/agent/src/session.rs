//! The `session` verbs: create, list, hide and delete.
//!
//! Every one of them is a question about the log directory — which sessions
//! exist, what each one's last word says, and whether the log is still wanted —
//! so none of them assembles a runtime. A model that is misconfigured cannot
//! stop a person from listing or cleaning up their sessions, exactly as it
//! cannot stop `schedule list` or `plugin list`.
//!
//! Delete is the one verb that destroys something, so it is the one that asks
//! the kernel first: [`agent_kernel::session_lifecycle::delete`] refuses while
//! another process is driving the session, which the directory scan cannot see.
//!
//! They live here rather than in `agent-cli`, as the `plugin` verbs do: how a
//! session is laid out on disk is the store's to know
//! ([`agent_store_jsonl::session_ids`]), and the command layer is deliberately
//! not allowed to depend on a store adapter.

use std::path::Path;
use std::sync::Arc;

use agent_kernel::session_archive;
use agent_kernel::session_lifecycle::{self, DeleteOutcome, LifecycleError};
use agent_ports::event_store::{EventStore, is_safe_dir_name};
use agent_protocol::events::Actor;
use agent_protocol::ids::{EventSeq, SessionId};
use chrono::{DateTime, Utc};

use agent_cli::{CliOutcome, CliOutput, json_output};

/// Why a session id cannot be used at all.
///
/// A usage error rather than an answer: the id names a directory, and one that
/// would escape the data directory or collide with a device name never reaches
/// the store. Refusing here is what makes the message about the id the person
/// typed rather than about a store that cannot open a file.
fn bad_id(id: &str) -> String {
    format!("'{id}' cannot be a session id: it must be a plain directory name")
}

/// `session new [--id <id>]`: record that a session has started.
///
/// Idempotent on purpose. Running it twice against the same id leaves one
/// `session/started` in the log and reports `created: false` the second time, so
/// a script that ensures a session exists before using it does not need to know
/// whether this is the first time. A log that predates `session/started` gains
/// one here; the session still counts as existing, because it does.
pub async fn run_session_new(
    store: &Arc<dyn EventStore>,
    id: &SessionId,
    json: bool,
) -> Result<CliOutcome, String> {
    if !is_safe_dir_name(id.as_str()) {
        return Err(bad_id(id.as_str()));
    }
    let before = store
        .read_stream(id, EventSeq::ZERO, usize::MAX)
        .await
        .map_err(|error| format!("cannot read the session's history: {error}"))?;
    let created = before.is_empty();
    session_lifecycle::ensure_started(store, id, Actor::User)
        .await
        .map_err(|error| format!("the session start could not be recorded: {error}"))?;

    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({
            "session_id": id.as_str(),
            "created": created,
        }))?)
    } else if created {
        CliOutput::Text(format!("session {} created", id.as_str()))
    } else {
        CliOutput::Text(format!("session {} already exists", id.as_str()))
    };
    Ok(CliOutcome::ok(output))
}

/// One session as `session list` shows it.
struct Row {
    id: String,
    archived: bool,
    events: usize,
    last_activity_at: Option<DateTime<Utc>>,
}

/// `session list`: every session the data directory holds, newest activity
/// first.
///
/// The list is the directory's contents, not an index: a session exists because
/// its log does ([`agent_store_jsonl::session_ids`]), so a log copied in by hand
/// appears and one deleted by any other process disappears, without a second
/// record that could disagree with the first. Reading each log is the price of
/// saying whether it is archived, which only the log knows.
pub async fn run_session_list(
    store: &Arc<dyn EventStore>,
    data_dir: &Path,
    json: bool,
) -> Result<CliOutcome, String> {
    let ids = agent_store_jsonl::session_ids(data_dir)
        .map_err(|error| format!("cannot read the data directory: {error}"))?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let events = store
            .read_stream(&id, EventSeq::ZERO, usize::MAX)
            .await
            .map_err(|error| format!("cannot read session {}: {error}", id.as_str()))?;
        rows.push(Row {
            archived: session_archive::current(&events),
            events: events.len(),
            last_activity_at: events.last().map(|event| event.recorded_at),
            id: id.as_str().to_string(),
        });
    }
    // Newest first; ties (two sessions with the same second) break by id so the
    // order is stable between runs, and the same rule the page's list uses.
    rows.sort_by(|left, right| {
        right.last_activity_at.cmp(&left.last_activity_at).then_with(|| left.id.cmp(&right.id))
    });

    let output = if json {
        let rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "session_id": row.id,
                    "archived": row.archived,
                    "events": row.events,
                    "last_activity_at": row.last_activity_at.map(|at| at.to_rfc3339()),
                })
            })
            .collect();
        CliOutput::Json(json_output(&serde_json::json!({ "sessions": rows }))?)
    } else if rows.is_empty() {
        CliOutput::Text("no sessions".to_string())
    } else {
        let lines = rows
            .iter()
            .map(|row| {
                let state = if row.archived { "archived" } else { "active" };
                let last = row
                    .last_activity_at
                    .map(|at| at.to_rfc3339())
                    .unwrap_or_else(|| "-".to_string());
                format!("{}  {state}  {} events  {last}", row.id, row.events)
            })
            .collect::<Vec<_>>();
        CliOutput::Text(lines.join("\n"))
    };
    Ok(CliOutcome::ok(output))
}

/// `session archive <id>` / `session unarchive <id>`: put a session away or
/// take it back out.
///
/// Hidden, not gone: the log keeps every event and the session stays readable,
/// which is why this is not `delete`. A session already in the asked-for state
/// is left alone and reported as such — an archive that appends every time it
/// runs would fill the log with events saying what it already said.
///
/// The existence check reads the data directory rather than the store, because
/// opening a session is what creates its directory: a typo'd id would otherwise
/// leave an empty session behind, and a log whose only event says "hidden" is
/// not a session anybody asked for.
pub async fn run_session_archive(
    store: &Arc<dyn EventStore>,
    data_dir: &Path,
    id: &SessionId,
    archived: bool,
    json: bool,
) -> Result<CliOutcome, String> {
    if !is_safe_dir_name(id.as_str()) {
        return Err(bad_id(id.as_str()));
    }
    let verb = if archived { "archived" } else { "unarchived" };
    if !agent_store_jsonl::has_session(data_dir, id.as_str()) {
        let infinitive = if archived { "archive" } else { "unarchive" };
        return Ok(CliOutcome::did_not_run(CliOutput::Text(format!(
            "no session {}: nothing to {infinitive}",
            id.as_str()
        ))));
    }
    let events = store
        .read_stream(id, EventSeq::ZERO, usize::MAX)
        .await
        .map_err(|error| format!("cannot read the session's history: {error}"))?;
    let changed = session_archive::current(&events) != archived;
    if changed {
        session_archive::set(store.clone(), id, archived, Actor::User)
            .await
            .map_err(|error| format!("the session could not be {verb}: {error}"))?;
    }

    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({
            "session_id": id.as_str(),
            "archived": archived,
            "changed": changed,
        }))?)
    } else if changed {
        CliOutput::Text(format!("session {} {verb}", id.as_str()))
    } else {
        CliOutput::Text(format!("session {} was already {verb}", id.as_str()))
    };
    Ok(CliOutcome::ok(output))
}

/// `session delete <id>`: remove the session's log.
///
/// Two outcomes are answers rather than failures. A session that is not there
/// was already asked for and is gone, so deleting twice succeeds twice; a
/// session another process is driving is *not* deleted, and that is exit status
/// `2` with the holder's pid in the message — the same refusal `agent ask` gets,
/// and the one thing a directory scan cannot say on its own.
pub async fn run_session_delete(
    store: &Arc<dyn EventStore>,
    id: &SessionId,
    json: bool,
) -> Result<CliOutcome, String> {
    if !is_safe_dir_name(id.as_str()) {
        return Err(bad_id(id.as_str()));
    }
    let removed = match session_lifecycle::delete(store, id).await {
        Ok(DeleteOutcome::Removed) => true,
        Ok(DeleteOutcome::Absent) => false,
        Err(busy @ LifecycleError::Busy { .. }) => {
            return Ok(CliOutcome::did_not_run(CliOutput::Text(format!(
                "session {} was not deleted: {busy}",
                id.as_str()
            ))));
        }
        Err(error) => return Err(format!("the session log could not be removed: {error}")),
    };

    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({
            "session_id": id.as_str(),
            "deleted": removed,
        }))?)
    } else if removed {
        CliOutput::Text(format!("session {} deleted", id.as_str()))
    } else {
        CliOutput::Text(format!("session {} did not exist", id.as_str()))
    };
    Ok(CliOutcome::ok(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::EventType;
    use agent_store_jsonl::JsonlEventStore;

    async fn log(
        store: &Arc<dyn EventStore>,
        id: &str,
    ) -> Vec<agent_protocol::events::StoredEvent> {
        store
            .read_stream(&SessionId::from_string(id), EventSeq::ZERO, usize::MAX)
            .await
            .expect("reads")
    }

    fn text(outcome: &CliOutcome) -> String {
        match &outcome.output {
            CliOutput::Text(text) => text.clone(),
            CliOutput::Json(json) => json.to_string(),
        }
    }

    fn store_at(dir: &std::path::Path) -> Arc<dyn EventStore> {
        Arc::new(JsonlEventStore::new(dir))
    }

    #[tokio::test]
    async fn creating_a_session_writes_started_on_an_empty_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let id = SessionId::from_string("new-one");

        let outcome = run_session_new(&store, &id, false).await.expect("creates");
        assert_eq!(outcome.verdict, agent_cli::Verdict::Answered);
        let events = log(&store, "new-one").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.event_type.as_str(), EventType::SESSION_STARTED);
    }

    #[tokio::test]
    async fn creating_twice_leaves_one_start_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let id = SessionId::from_string("twice");

        let _ = run_session_new(&store, &id, false).await.expect("creates");
        let second = run_session_new(&store, &id, true).await.expect("is idempotent");
        assert!(text(&second).contains("\"created\": false"), "{}", text(&second));
        let starts = log(&store, "twice")
            .await
            .iter()
            .filter(|event| event.event.event_type.as_str() == EventType::SESSION_STARTED)
            .count();
        assert_eq!(starts, 1);
    }

    #[tokio::test]
    async fn a_log_without_a_start_gains_one_without_becoming_new() {
        // A session written by an older build has no `session/started`. Asking
        // for it again records the start once and still reports that the session
        // was already there.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let id = SessionId::from_string("legacy");
        // `Inbox::append` writes the start itself now, so the legacy shape is
        // built by hand: one event, and it is not a start.
        let mut legacy_draft = session_lifecycle::started_draft(&id, Actor::User);
        legacy_draft.event_type = agent_protocol::events::EventType::new(EventType::INBOX_APPENDED);
        legacy_draft.payload = serde_json::json!({ "item_id": "i-1" });
        store.append(&id, EventSeq::ZERO, vec![legacy_draft]).await.expect("writes the legacy log");

        let outcome = run_session_new(&store, &id, true).await.expect("records the start");
        assert!(text(&outcome).contains("\"created\": false"), "{}", text(&outcome));
        let events = log(&store, "legacy").await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event.event_type.as_str(), EventType::SESSION_STARTED);
    }

    #[tokio::test]
    async fn listing_reads_the_directory_and_marks_archived_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).expect("data dir");
        let store = store_at(&data);
        for id in ["one", "two"] {
            let _ =
                run_session_new(&store, &SessionId::from_string(id), false).await.expect("creates");
        }
        let _ = run_session_archive(&store, &data, &SessionId::from_string("two"), true, false)
            .await
            .expect("archives");

        let listed = run_session_list(&store, &data, false).await.expect("lists");
        let rendered = text(&listed);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().any(|line| line.starts_with("one  active")));
        assert!(lines.iter().any(|line| line.starts_with("two  archived")));
    }

    #[tokio::test]
    async fn a_directory_without_sessions_lists_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let listed =
            run_session_list(&store, &dir.path().join("absent"), false).await.expect("lists");
        assert_eq!(text(&listed), "no sessions");
    }

    #[tokio::test]
    async fn archiving_a_missing_session_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let outcome =
            run_session_archive(&store, dir.path(), &SessionId::from_string("ghost"), true, false)
                .await
                .expect("runs");
        assert_eq!(outcome.verdict, agent_cli::Verdict::DidNotRun);
        assert!(text(&outcome).contains("no session ghost"));
        assert!(!dir.path().join("ghost").exists(), "a missing session must stay missing");
    }

    #[tokio::test]
    async fn archiving_twice_writes_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let id = SessionId::from_string("hide");
        let _ = run_session_new(&store, &id, false).await.expect("creates");
        let first =
            run_session_archive(&store, dir.path(), &id, true, true).await.expect("archives");
        assert!(text(&first).contains("\"changed\": true"), "{}", text(&first));
        let _ = run_session_archive(&store, dir.path(), &id, true, false)
            .await
            .expect("archives again");
        let archives = log(&store, "hide")
            .await
            .iter()
            .filter(|event| event.event.event_type.as_str() == EventType::SESSION_ARCHIVED)
            .count();
        assert_eq!(archives, 1);
    }

    #[tokio::test]
    async fn deleting_a_missing_session_succeeds_and_repeats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let id = SessionId::from_string("gone");
        let outcome = run_session_delete(&store, &id, true).await.expect("runs");
        assert_eq!(outcome.verdict, agent_cli::Verdict::Answered);
        assert!(text(&outcome).contains("\"deleted\": false"), "{}", text(&outcome));
    }

    #[tokio::test]
    async fn deleting_a_session_removes_its_log_for_good() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(dir.path());
        let id = SessionId::from_string("remove-me");
        let _ = run_session_new(&store, &id, false).await.expect("creates");
        assert!(dir.path().join("remove-me").exists());

        let outcome = run_session_delete(&store, &id, false).await.expect("deletes");
        assert!(text(&outcome).contains("deleted"), "{}", text(&outcome));
        assert!(!dir.path().join("remove-me").exists());
    }

    #[tokio::test]
    async fn a_session_another_driver_holds_is_not_deleted() {
        // Two stores over one directory share nothing but the files, which is
        // the cross-process case: a claim held by one refuses the other.
        let dir = tempfile::tempdir().expect("tempdir");
        let id = SessionId::from_string("busy");
        let first = store_at(dir.path());
        let _ = run_session_new(&first, &id, false).await.expect("creates");
        let second = store_at(dir.path());
        let held = second.driver_lease(&id).await.expect("takes the claim");

        let outcome = run_session_delete(&first, &id, false).await.expect("runs");
        assert_eq!(outcome.verdict, agent_cli::Verdict::DidNotRun);
        let message = text(&outcome);
        assert!(message.contains("already being driven"), "{message}");
        assert!(message.contains("pid"), "the refusal names the holder: {message}");
        assert!(dir.path().join("busy").exists(), "a refused delete removes nothing");

        drop(held);
        let retried = run_session_delete(&first, &id, false).await.expect("runs");
        assert_eq!(retried.verdict, agent_cli::Verdict::Answered);
        assert!(!dir.path().join("busy").exists());
    }
}
