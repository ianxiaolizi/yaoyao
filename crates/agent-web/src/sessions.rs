//! The read model: sessions and their transcripts, folded from the log.
//!
//! Listing sessions is a filesystem question (which directories hold a log) and
//! reading one is a projection question (what the events mean). Neither writes:
//! this module opens no lease and appends nothing, so a reader can never change
//! the state it is describing.

use std::path::Path;

use agent_ports::event_store::EventStore;
use agent_projection::{
    ActionStatus, ApprovalState, ItemBody, ProjectionDefinition, SessionProjection, TranscriptItem,
    TranscriptProjection, TranscriptState, TurnEnding,
};
use agent_protocol::ids::{EventSeq, SessionId};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

/// Why a read could not be answered.
///
/// Every variant is a refusal to hand back a **shorter** view than the log
/// supports: a session whose log cannot be folded fails the request rather than
/// being skipped, because a list that silently drops the broken session looks
/// exactly like a list of healthy ones.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("cannot read the data directory: {0}")]
    DataDir(#[source] std::io::Error),
    #[error("session {session}: {message}")]
    Session { session: String, message: String },
}

/// What the fold knows about one session.
pub(crate) struct Folded {
    pub transcript: TranscriptState,
    pub steps: u64,
    pub open_turn: Option<String>,
    pub yielded: bool,
    pub forked_from: Option<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
}

/// Fold one session's log through both projections.
pub(crate) async fn fold(store: &dyn EventStore, session: &SessionId) -> Result<Folded, ReadError> {
    let events = store.read_stream(session, EventSeq::ZERO, usize::MAX).await.map_err(|error| {
        ReadError::Session { session: session.as_str().to_string(), message: error.to_string() }
    })?;

    let session_projection = SessionProjection::new();
    let mut session_state = session_projection.init();
    let transcript_projection = TranscriptProjection::new();
    let mut transcript = transcript_projection.init();
    for event in &events {
        session_state = session_projection.apply(&session_state, event).map_err(|error| {
            ReadError::Session { session: session.as_str().to_string(), message: error.to_string() }
        })?;
        transcript = transcript_projection.apply(&transcript, event).map_err(|error| {
            ReadError::Session { session: session.as_str().to_string(), message: error.to_string() }
        })?;
    }

    Ok(Folded {
        transcript,
        steps: session_state.steps_started,
        open_turn: session_state.open_turn.as_ref().map(|id| id.as_str().to_string()),
        yielded: session_state.yielded_turn.is_some(),
        forked_from: session_state.parent_session_id.as_ref().map(|id| id.as_str().to_string()),
        last_activity_at: events.last().map(|event| event.recorded_at),
    })
}

/// Every session under the data directory, newest activity first.
pub(crate) async fn list(store: &dyn EventStore, data_dir: &Path) -> Result<Vec<Value>, ReadError> {
    let mut sessions: Vec<(Option<DateTime<Utc>>, String, Value)> = Vec::new();
    for id in session_ids(data_dir)? {
        let folded = fold(store, &id).await?;
        let summary = summary(&id, &folded);
        sessions.push((folded.last_activity_at, id.as_str().to_string(), summary));
    }
    // Newest first; ties (two sessions with the same second) break by id so the
    // order is stable between requests.
    sessions.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Ok(sessions.into_iter().map(|(_, _, summary)| summary).collect())
}

/// One session's transcript, with items changed after `since`.
pub(crate) async fn transcript(
    store: &dyn EventStore,
    session: &SessionId,
    since: Option<u64>,
) -> Result<Value, ReadError> {
    let folded = fold(store, session).await?;
    let items: Vec<Value> = folded
        .transcript
        .items
        .iter()
        // No cursor means "everything"; a cursor means "what changed after it".
        // The first event is seq 0, so `> 0` would otherwise drop the opening
        // prompt from an unfiltered read.
        .filter(|item| since.is_none_or(|cursor| item.updated_seq.as_u64() > cursor))
        .map(wire_item)
        .collect();
    Ok(json!({
        "v": 1,
        "session": session.as_str(),
        "head": folded.transcript.head.as_u64(),
        "items": items,
    }))
}

/// The summary line for the sidebar.
fn summary(session: &SessionId, folded: &Folded) -> Value {
    let title = folded
        .transcript
        .items
        .iter()
        .find_map(|item| match &item.body {
            ItemBody::User { text, .. } => Some(headline(text)),
            _ => None,
        })
        .unwrap_or_default();
    let snippet = folded
        .transcript
        .items
        .iter()
        .rev()
        .find_map(|item| match &item.body {
            ItemBody::Answer { text } if !text.trim().is_empty() => Some(headline(text)),
            _ => None,
        })
        .unwrap_or_default();
    let pending = folded
        .transcript
        .items
        .iter()
        .filter(|item| {
            matches!(&item.body, ItemBody::Approval { state: ApprovalState::Pending, .. })
        })
        .count();

    json!({
        "id": session.as_str(),
        "title": title,
        "snippet": snippet,
        "steps": folded.steps,
        "last_activity_at": folded.last_activity_at.map(|at| at.to_rfc3339()),
        "usage": folded.transcript.last_usage.and_then(|usage| serde_json::to_value(usage).ok()),
        "open_turn": folded.open_turn,
        "pending_approvals": pending,
        "yielded": folded.yielded,
        "forked_from": folded.forked_from,
    })
}

/// One item in the shape the page renders.
///
/// The projection's body is an enum with an internal tag; the wire flattens it
/// so a row is one object with a `kind` and its own fields, which is what the
/// component table consumes.
fn wire_item(item: &TranscriptItem) -> Value {
    let mut row = serde_json::Map::new();
    row.insert("id".to_string(), item.id.clone().into());
    row.insert("seq".to_string(), item.seq.as_u64().into());
    if let Some(turn) = &item.turn_id {
        row.insert("turn".to_string(), turn.as_str().into());
    }
    if let Some(step) = &item.step_id {
        row.insert("step".to_string(), step.as_str().into());
    }

    match &item.body {
        ItemBody::User { text, .. } => {
            row.insert("kind".to_string(), "user".into());
            row.insert("text".to_string(), text.clone().into());
        }
        ItemBody::Note { text } => {
            row.insert("kind".to_string(), "note".into());
            row.insert("text".to_string(), text.clone().into());
        }
        ItemBody::Action { tool, arguments, status, content, duration_ms, .. } => {
            row.insert("kind".to_string(), "action".into());
            row.insert("tool".to_string(), tool.clone().into());
            row.insert("arguments".to_string(), arguments.clone());
            row.insert("status".to_string(), action_status(*status).into());
            if let Some(content) = content {
                row.insert("content".to_string(), content.clone().into());
            }
            if let Some(duration_ms) = duration_ms {
                row.insert("duration_ms".to_string(), (*duration_ms).into());
            }
        }
        ItemBody::Approval { approval_id, tool, summary, state } => {
            row.insert("kind".to_string(), "approval".into());
            row.insert("approval_id".to_string(), approval_id.as_str().into());
            row.insert("tool".to_string(), tool.clone().into());
            row.insert("summary".to_string(), summary.clone().into());
            row.insert("state".to_string(), approval_state(*state).into());
        }
        ItemBody::Answer { text } => {
            row.insert("kind".to_string(), "answer".into());
            row.insert("text".to_string(), text.clone().into());
        }
        ItemBody::TurnEnd { ending, stop_reason, usage } => {
            row.insert("kind".to_string(), "turn_end".into());
            row.insert("ending".to_string(), turn_ending(*ending).into());
            if let Some(reason) = stop_reason {
                row.insert("stop_reason".to_string(), reason.clone().into());
            }
            if let Some(usage) = usage
                && let Ok(value) = serde_json::to_value(usage)
            {
                row.insert("usage".to_string(), value);
            }
        }
    }
    Value::Object(row)
}

fn action_status(status: ActionStatus) -> &'static str {
    match status {
        ActionStatus::Running => "running",
        ActionStatus::Completed => "completed",
        ActionStatus::Failed => "failed",
        ActionStatus::Refused => "refused",
        ActionStatus::Waiting => "waiting",
        ActionStatus::Cancelled => "cancelled",
    }
}

fn approval_state(state: ApprovalState) -> &'static str {
    match state {
        ApprovalState::Pending => "pending",
        ApprovalState::Granted => "granted",
        ApprovalState::Denied => "denied",
        ApprovalState::Expired => "expired",
        ApprovalState::Cancelled => "cancelled",
    }
}

fn turn_ending(ending: TurnEnding) -> &'static str {
    match ending {
        TurnEnding::Answered => "answered",
        TurnEnding::MaxSteps => "max_steps",
        TurnEnding::Yielded => "yielded",
        TurnEnding::Failed => "failed",
        TurnEnding::Cancelled => "cancelled",
    }
}

/// The first line of a message, cut at a character boundary.
fn headline(text: &str) -> String {
    const LIMIT: usize = 80;
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= LIMIT {
        return first.to_string();
    }
    let mut cut: String = first.chars().take(LIMIT).collect();
    cut.push('…');
    cut
}

/// The directories under `data_dir` that hold a session log.
///
/// `file_type()` is read without following a link, so a symlinked directory is
/// not a session: the list is what this data directory actually contains.
fn session_ids(data_dir: &Path) -> Result<Vec<SessionId>, ReadError> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        // A machine that has never run a turn has no data directory yet; that
        // is an empty list, not a failure to read one.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ReadError::DataDir(error)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(ReadError::DataDir)?;
        if !entry.file_type().map_err(ReadError::DataDir)?.is_dir() {
            continue;
        }
        if !entry.path().join("events.jsonl").is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !safe_session_id(&name) {
            continue;
        }
        ids.push(SessionId::from_string(name));
    }
    Ok(ids)
}

/// Whether a directory name can be handed to the store at all.
///
/// The store applies the full C5.3 rules; this only refuses what would escape
/// the data directory before the store ever sees it.
pub(crate) fn safe_session_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_headline_is_one_line_and_bounded() {
        assert_eq!(headline("first\nsecond"), "first");
        assert_eq!(headline("  padded  "), "padded");
        let long = "字".repeat(100);
        let cut = headline(&long);
        assert!(cut.chars().count() <= 81);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_session_id_may_not_escape_its_directory() {
        for allowed in ["default", "2026-10-11.run", "a b"] {
            assert!(safe_session_id(allowed), "{allowed} should pass the escape check");
        }
        for refused in ["", ".", "..", "a/b", "a\\b"] {
            assert!(!safe_session_id(refused), "{refused} must not pass");
        }
    }
}
