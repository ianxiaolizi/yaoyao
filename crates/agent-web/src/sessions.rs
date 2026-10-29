//! The read model: sessions and their transcripts, folded from the log.
//!
//! Listing sessions is a filesystem question (which directories hold a log) and
//! reading one is a projection question (what the events mean). Neither writes:
//! this module opens no lease and appends nothing, so a reader can never change
//! the state it is describing.

use std::collections::HashMap;
use std::path::Path;

use agent_kernel::approval::PENDING_TTL;
use agent_kernel::model_selection::ModelChoice;
use agent_ports::event_store::EventStore;
use agent_projection::{
    ActionStatus, ApprovalState, ItemBody, ProjectionDefinition, SessionProjection, TranscriptItem,
    TranscriptProjection, TranscriptState, TurnEnding,
};
use agent_protocol::events::StoredEvent;
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
#[derive(Clone)]
pub(crate) struct Folded {
    pub transcript: TranscriptState,
    pub steps: u64,
    pub open_turn: Option<String>,
    pub yielded: bool,
    pub forked_from: Option<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
    /// The most recent `model/request.prepared` payload. Context is a read of
    /// what the last request actually carried, not a fresh estimate.
    pub last_request: Option<Value>,
    /// The latest plan the loop recorded. `plan/updated` is the only writer.
    pub last_plan: Option<Value>,
    /// Children this session delegated to, in spawn order.
    pub subagents: Vec<Value>,
    /// The model the session is set to use, if the person ever chose one. Absent
    /// means the server's startup decision still applies.
    pub model: Option<ModelChoice>,
    /// Whether the session is put away. Archived sessions keep their whole log
    /// and stay openable; they are only moved out of the working list.
    pub archived: bool,
}

/// A transcript that can be advanced one event at a time.
///
/// The SSE endpoint needs this rather than [`fold`]: replaying the whole log per
/// appended event would make a long session quadratic, and re-sending every
/// item would make the page repaint rows that did not change. The fold keeps a
/// wire copy of each row it has handed out, so `apply` can say exactly which
/// rows are news.
pub(crate) struct LiveTranscript {
    projection: TranscriptProjection,
    state: TranscriptState,
    sent: HashMap<String, Value>,
    /// The instant the row shapes are answered at, handed in by the handler.
    /// The page's clock is not the server's, and a row that expired while the
    /// stream was open must be re-shaped against the same idea of "now" the
    /// request was served at.
    now: DateTime<Utc>,
}

impl LiveTranscript {
    /// Fold the log once and remember every row as already sent.
    pub(crate) fn fold(events: &[StoredEvent], now: DateTime<Utc>) -> Result<Self, ReadError> {
        let projection = TranscriptProjection::new();
        let mut state = projection.init();
        for event in events {
            state = projection.apply(&state, event).map_err(|error| ReadError::Session {
                session: event.event.session_id.as_str().to_string(),
                message: error.to_string(),
            })?;
        }
        let sent =
            state.items.iter().map(|item| (item.id.clone(), wire_item_at(item, now))).collect();
        Ok(Self { projection, state, sent, now })
    }

    /// The rows a client at the inclusive SSE cursor `from` has not seen yet.
    ///
    /// SSE's cursor is inclusive (`from=0` means "the whole log") because the
    /// first event has seq 0 and there is no predecessor to express "before
    /// it". `transcript?since=` keeps the exclusive rule: there, "since 0"
    /// means "changes after the opening prompt".
    pub(crate) fn snapshot(&self, from: u64) -> Vec<Value> {
        self.state
            .items
            .iter()
            .filter(|item| item.updated_seq.as_u64() >= from)
            .map(|item| wire_item_at(item, self.now))
            .collect()
    }

    pub(crate) fn head(&self) -> u64 {
        self.state.head.as_u64()
    }

    /// Apply one durable event and return the rows whose wire shape changed.
    pub(crate) fn apply(&mut self, event: &StoredEvent) -> Result<Vec<Value>, ReadError> {
        self.state =
            self.projection.apply(&self.state, event).map_err(|error| ReadError::Session {
                session: event.event.session_id.as_str().to_string(),
                message: error.to_string(),
            })?;

        let mut changed = Vec::new();
        // Only rows this event touched can have a new wire shape. The filter
        // keeps a long session linear in the number of *changes* rather than in
        // the number of rows times the number of events.
        for item in self.state.items.iter().filter(|item| item.updated_seq >= event.seq) {
            let wire = wire_item_at(item, self.now);
            if self.sent.get(&item.id) == Some(&wire) {
                continue;
            }
            self.sent.insert(item.id.clone(), wire.clone());
            changed.push(wire);
        }
        Ok(changed)
    }
}

/// A small cache of folded logs, keyed by the file signature they were read at.
///
/// Listing ten sessions on every sidebar refresh means folding ten logs; most
/// of those logs did not change between refreshes. The signature is the pair
/// the file system actually knows — length and last write time — so a writer in
/// this process or another one invalidates the entry by touching the file. A
/// fold whose read spans a write is not cached, because the file's signature
/// changed underneath it and the entry would be stale before it was stored.
#[derive(Default)]
pub(crate) struct ReadModel {
    cache: std::sync::Mutex<HashMap<String, Cached>>,
}

#[derive(Clone)]
struct Cached {
    signature: (u64, Option<std::time::SystemTime>),
    folded: Folded,
}

impl ReadModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn signature(
        data_dir: &Path,
        session: &SessionId,
    ) -> Option<(u64, Option<std::time::SystemTime>)> {
        let metadata =
            std::fs::metadata(data_dir.join(session.as_str()).join("events.jsonl")).ok()?;
        Some((metadata.len(), metadata.modified().ok()))
    }

    /// The folded session, from the cache when the log has not moved.
    pub(crate) async fn fold(
        &self,
        store: &dyn EventStore,
        data_dir: &Path,
        session: &SessionId,
    ) -> Result<Folded, ReadError> {
        let before = Self::signature(data_dir, session);
        if let Some(before) = before
            && let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(session.as_str())
            && cached.signature == before
        {
            return Ok(cached.folded.clone());
        }

        let folded = fold(store, session).await?;
        let after = Self::signature(data_dir, session);
        if before.is_some() && before == after {
            let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.insert(
                session.as_str().to_string(),
                Cached {
                    signature: after.expect("the before signature was present"),
                    folded: folded.clone(),
                },
            );
        }
        Ok(folded)
    }
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
    let mut last_request = None;
    let mut last_plan = None;
    let mut subagents: Vec<Value> = Vec::new();
    for event in &events {
        let event_type = event.event.event_type.as_str();
        if event_type == agent_protocol::events::EventType::MODEL_REQUEST_PREPARED {
            last_request = Some(event.event.payload.clone());
        }
        if event_type == agent_protocol::events::EventType::PLAN_UPDATED {
            last_plan = event.event.payload.get("plan").cloned();
        }
        if let Some(child) = event.event.payload.get("child_session_id").and_then(Value::as_str) {
            match event_type {
                agent_protocol::events::EventType::SUBAGENT_SPAWNED => {
                    subagents.push(json!({
                        "child_session_id": child,
                        "provider": event.event.payload.get("provider").cloned(),
                        "objective": event
                            .event
                            .payload
                            .pointer("/request/objective")
                            .cloned(),
                        "parent_turn_id": event.event.payload.get("parent_turn_id").cloned(),
                        "status": "running",
                    }));
                }
                agent_protocol::events::EventType::SUBAGENT_COMPLETED
                | agent_protocol::events::EventType::SUBAGENT_FAILED
                | agent_protocol::events::EventType::SUBAGENT_CANCELLED => {
                    let status = match event_type {
                        agent_protocol::events::EventType::SUBAGENT_COMPLETED => "completed",
                        agent_protocol::events::EventType::SUBAGENT_FAILED => "failed",
                        _ => "cancelled",
                    };
                    if let Some(row) =
                        subagents.iter_mut().find(|row| row["child_session_id"] == child)
                    {
                        row["status"] = status.into();
                        if let Some(outcome) = event.event.payload.get("outcome") {
                            row["outcome"] = outcome.clone();
                        }
                    }
                }
                _ => {}
            }
        }
        session_state = session_projection.apply(&session_state, event).map_err(|error| {
            ReadError::Session { session: session.as_str().to_string(), message: error.to_string() }
        })?;
        transcript = transcript_projection.apply(&transcript, event).map_err(|error| {
            ReadError::Session { session: session.as_str().to_string(), message: error.to_string() }
        })?;
    }

    // A payload that will not read back is a log anomaly, not "no choice": the
    // page would label the session with the default and hide the damage.
    let model =
        agent_kernel::model_selection::current(&events).map_err(|error| ReadError::Session {
            session: session.as_str().to_string(),
            message: format!("model selection: {error}"),
        })?;

    Ok(Folded {
        transcript,
        steps: session_state.steps_started,
        open_turn: session_state.open_turn.as_ref().map(|id| id.as_str().to_string()),
        yielded: session_state.yielded_turn.is_some(),
        forked_from: session_state.parent_session_id.as_ref().map(|id| id.as_str().to_string()),
        last_activity_at: events.last().map(|event| event.recorded_at),
        last_request,
        last_plan,
        subagents,
        model,
        archived: agent_kernel::session_archive::current(&events),
    })
}

/// Every session under the data directory, newest activity first.
pub(crate) async fn list(
    store: &dyn EventStore,
    data_dir: &Path,
    reads: &ReadModel,
    now: DateTime<Utc>,
) -> Result<Vec<Value>, ReadError> {
    let mut sessions: Vec<(Option<DateTime<Utc>>, String, Value)> = Vec::new();
    for id in session_ids(data_dir)? {
        let folded = reads.fold(store, data_dir, &id).await?;
        let summary = summary(&id, &folded, now);
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
    data_dir: &Path,
    session: &SessionId,
    since: Option<u64>,
    reads: &ReadModel,
    now: DateTime<Utc>,
) -> Result<Value, ReadError> {
    let folded = reads.fold(store, data_dir, session).await?;
    let items: Vec<Value> = folded
        .transcript
        .items
        .iter()
        // No cursor means "everything"; a cursor means "what changed after it".
        // The first event is seq 0, so `> 0` would otherwise drop the opening
        // prompt from an unfiltered read.
        .filter(|item| since.is_none_or(|cursor| item.updated_seq.as_u64() > cursor))
        .map(|item| wire_item_at(item, now))
        .collect();
    Ok(json!({
        "v": 1,
        "session": session.as_str(),
        "head": folded.transcript.head.as_u64(),
        "items": items,
    }))
}

/// The newest `git.diff` observation in a session, as a reader-shaped report.
///
/// This is a projection of an observation the kernel already recorded, not a
/// fresh `git diff`: a page refresh must not change what the agent saw, and a
/// repository may have moved since. A session that never called `git.diff` has
/// no report — that is a `404`, not an empty diff (which would claim the tree
/// was clean).
pub(crate) async fn diff(
    store: &dyn EventStore,
    data_dir: &Path,
    session: &SessionId,
    reads: &ReadModel,
) -> Result<Option<Value>, ReadError> {
    let folded = reads.fold(store, data_dir, session).await?;
    for item in folded.transcript.items.iter().rev() {
        let ItemBody::Action {
            tool,
            arguments,
            status: ActionStatus::Completed,
            content: Some(content),
            ..
        } = &item.body
        else {
            continue;
        };
        if tool != "git.diff" {
            continue;
        }
        let parsed =
            serde_json::from_str::<Value>(content).unwrap_or_else(|_| json!({ "raw": content }));
        return Ok(Some(json!({
            "v": 1,
            "session": session.as_str(),
            "turn": item.turn_id.as_ref().map(|turn| turn.as_str()),
            "seq": item.updated_seq.as_u64(),
            "source": "git.diff",
            "arguments": arguments,
            "diff": parsed,
        })));
    }
    Ok(None)
}

/// What the most recent model request carried.
///
/// The numbers are read from `model/request.prepared`, not re-estimated. The
/// categories are a decomposition of that record: history, the frozen tool
/// definitions, and the rest (the stable head, per-turn layers and this turn's
/// input). A category is omitted when the request did not record its number,
/// rather than shown as zero.
pub(crate) async fn context(
    store: &dyn EventStore,
    data_dir: &Path,
    session: &SessionId,
    reads: &ReadModel,
) -> Result<Option<Value>, ReadError> {
    let folded = reads.fold(store, data_dir, session).await?;
    let Some(request) = folded.last_request.as_ref() else {
        return Ok(None);
    };
    let number = |key: &str| request.get(key).and_then(Value::as_u64);
    let history = number("estimated_context_tokens");
    let messages = number("estimated_messages_tokens");
    let tools = number("estimated_tool_tokens");
    let whole = number("estimated_request_tokens");
    let turn = messages.zip(history).map(|(messages, history)| messages.saturating_sub(history));

    let mut categories = Vec::new();
    for (name, tokens) in [("历史", history), ("工具定义", tools), ("本轮层与输入", turn)]
    {
        if let Some(tokens) = tokens {
            categories.push(json!({ "name": name, "tokens": tokens }));
        }
    }

    let preview = |value: Option<&Value>| {
        value.and_then(Value::as_str).map(|text| {
            json!({
                "chars": text.chars().count(),
                "preview": text.chars().take(240).collect::<String>(),
            })
        })
    };
    let layers = request.get("context_layers").map(|layers| {
        json!({
            "state": preview(layers.pointer("/state/text")),
            "task": preview(layers.pointer("/task/text")),
        })
    });

    Ok(Some(json!({
        "v": 1,
        "session": session.as_str(),
        "context_revision": number("context_revision"),
        "used": whole.unwrap_or_else(|| categories.iter().filter_map(|row| row["tokens"].as_u64()).sum()),
        "budget": {
            "max_input_tokens": number("max_input_tokens"),
            "reserved_output_tokens": number("reserved_output_tokens"),
        },
        "categories": categories,
        "messages": {
            "count": number("message_count"),
            "context_count": number("context_message_count"),
            "dropped": number("dropped_message_count"),
        },
        "tools": {
            "count": number("tool_count"),
            "manifest_id": request.get("tool_manifest_id").cloned(),
        },
        "layers": layers,
        "notice": request.get("context_notice").cloned(),
        "usage": request.get("provider_usage").cloned(),
    })))
}

/// The session's latest plan ledger, if the loop recorded one.
pub(crate) async fn plan(
    store: &dyn EventStore,
    data_dir: &Path,
    session: &SessionId,
    reads: &ReadModel,
) -> Result<Option<Value>, ReadError> {
    let folded = reads.fold(store, data_dir, session).await?;
    Ok(folded.last_plan.map(|plan| {
        json!({
            "v": 1,
            "session": session.as_str(),
            "source": "plan/updated",
            "plan": plan,
        })
    }))
}

/// The children this session delegated to, with their last known status.
pub(crate) async fn subagents(
    store: &dyn EventStore,
    data_dir: &Path,
    session: &SessionId,
    reads: &ReadModel,
) -> Result<Option<Value>, ReadError> {
    let folded = reads.fold(store, data_dir, session).await?;
    if folded.subagents.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "v": 1,
        "session": session.as_str(),
        "source": "subagent/spawned",
        "children": folded.subagents,
    })))
}

/// The summary line for the sidebar.
fn summary(session: &SessionId, folded: &Folded, now: DateTime<Utc>) -> Value {
    let title = folded
        .transcript
        .items
        .iter()
        .find_map(|item| match &item.body {
            ItemBody::User { resumed: false, text, .. } => Some(headline(text)),
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
            matches!(
                &item.body,
                ItemBody::Approval {
                    state: ApprovalState::Pending,
                    created_at,
                    ..
                } if !approval_expired(*created_at, now)
            )
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
        // Absent until the person picks one: the page then falls back to the
        // server's startup label rather than inventing a selection.
        "model": folded.model,
        "archived": folded.archived,
    })
}

/// Whether a pending request has outlived the kernel's `PENDING_TTL`.
///
/// This is a *derivation*, not a write: the kernel still owns the lifecycle and
/// will record `approval/expired` when a resume reaches the wait. The read
/// model uses the same constant so the card stops offering buttons the moment
/// the question has actually expired, rather than one refresh later.
pub(crate) fn approval_expired(created_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    created_at.is_some_and(|created| created + PENDING_TTL <= now)
}

/// One item in the shape the page renders, with the read instant supplied.
///
/// The projection's body is an enum with an internal tag; the wire flattens it
/// so a row is one object with a `kind` and its own fields, which is what the
/// component table consumes. The instant is a parameter because this layer does
/// not read a clock: the process edge hands one in, and every row in one answer
/// is shaped against the same one (CONTRACT.md §7 裁决 #20).
fn wire_item_at(item: &TranscriptItem, now: DateTime<Utc>) -> Value {
    let mut row = serde_json::Map::new();
    row.insert("id".to_string(), item.id.clone().into());
    row.insert("seq".to_string(), item.seq.as_u64().into());
    // The cursor is about *last* change, not first appearance: a tool row that
    // started before `since` and finished after it is part of the answer.
    row.insert("updated_seq".to_string(), item.updated_seq.as_u64().into());
    if let Some(turn) = &item.turn_id {
        row.insert("turn".to_string(), turn.as_str().into());
    }
    if let Some(step) = &item.step_id {
        row.insert("step".to_string(), step.as_str().into());
    }

    match &item.body {
        ItemBody::User { text, resumed, .. } => {
            row.insert("kind".to_string(), "user".into());
            row.insert("text".to_string(), text.clone().into());
            if *resumed {
                row.insert("resumed".to_string(), true.into());
            }
        }
        ItemBody::Note { text } => {
            row.insert("kind".to_string(), "note".into());
            row.insert("text".to_string(), text.clone().into());
        }
        ItemBody::Thinking { text, steps } => {
            row.insert("kind".to_string(), "thinking".into());
            row.insert("text".to_string(), text.clone().into());
            row.insert("steps".to_string(), (*steps).into());
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
        ItemBody::Approval { approval_id, tool, summary, state, created_at } => {
            let expired = *state == ApprovalState::Pending && approval_expired(*created_at, now);
            row.insert("kind".to_string(), "approval".into());
            row.insert("approval_id".to_string(), approval_id.as_str().into());
            row.insert("tool".to_string(), tool.clone().into());
            row.insert("summary".to_string(), summary.clone().into());
            row.insert(
                "state".to_string(),
                if expired { "expired" } else { approval_state(*state) }.into(),
            );
            if let Some(created_at) = created_at {
                row.insert("created_at".to_string(), created_at.to_rfc3339().into());
                row.insert(
                    "expires_at".to_string(),
                    (*created_at + PENDING_TTL).to_rfc3339().into(),
                );
            }
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
        TurnEnding::LimitReached => "limit_reached",
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
    // The store's rule is the authority, not a second, looser guess: an id that
    // passes here but fails there would make the page accept a turn and then
    // fail to write it. Windows device names and trailing dots are part of that
    // rule and are easy to forget in a hand-written check.
    agent_ports::event_store::is_safe_dir_name(id)
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
    fn a_resumed_prompt_is_tagged_on_the_wire() {
        let item = TranscriptItem {
            id: "u:1".to_string(),
            seq: EventSeq::new(1),
            updated_seq: EventSeq::new(1),
            turn_id: None,
            step_id: None,
            body: ItemBody::User {
                text: "The approval ended the wait".to_string(),
                inbox_item: None,
                resumed: true,
            },
        };
        let wire = wire_item_at(&item, Utc::now());
        assert_eq!(wire["kind"], "user");
        assert_eq!(wire["resumed"], true);

        let normal = TranscriptItem {
            body: ItemBody::User {
                text: "what I typed".to_string(),
                inbox_item: None,
                resumed: false,
            },
            ..item
        };
        let wire = wire_item_at(&normal, Utc::now());
        assert!(wire.get("resumed").is_none(), "a person's message carries no marker");
    }

    #[test]
    fn a_session_id_may_not_escape_its_directory_or_name_a_device() {
        for allowed in ["default", "2026-10-11.run", "web-20261024-180000-abcd", "a_b"] {
            assert!(safe_session_id(allowed), "{allowed} should pass the escape check");
        }
        for refused in ["", ".", "..", "a/b", "a\\b", "a b", "CON", "NUL.txt", "trailing."] {
            assert!(!safe_session_id(refused), "{refused} must not pass");
        }
    }
}
