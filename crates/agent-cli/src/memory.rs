//! The `memory` commands: search and rebuild over the derived index, and the
//! long-term facts kept in the store.
//!
//! None of them writes an event — the index is not a fact, and a search that
//! changed the log would make "what the model saw" depend on who searched. The
//! fact verbs are the exception in the other direction: `remember` and `forget`
//! write to the memory store, which is its own durable thing, and delete from
//! it for real. The port is injected rather than built here, so this crate does
//! not depend on a particular storage engine.

use agent_ports::memory_index::{MemoryIndex, MemoryQuery};
use agent_ports::memory_store::{
    ListFacts, MemoryFact, MemorySensitivity, MemoryStatus, MemoryStore, Remember,
};
use agent_protocol::ids::SessionId;
use chrono::{DateTime, Utc};

use crate::{CliOutcome, CliOutput, json_output};

/// How many hits `memory search` returns when the caller names no limit.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;

/// `memory search <text>`: find events whose indexed text contains `query`.
///
/// Zero hits is an answer, not a failure: the exit code stays `0` and the
/// output says nothing matched. A search that could not run at all — the index
/// is a version this build does not know, the log cannot be read — is the
/// caller's `2`.
pub async fn run_memory_search(
    index: &dyn MemoryIndex,
    session_id: &SessionId,
    query: &str,
    limit: usize,
    json: bool,
) -> Result<CliOutcome, String> {
    let text = query.trim();
    if text.is_empty() {
        return Err("memory search needs a word to look for".to_string());
    }
    let hits = index
        .search(&MemoryQuery { session_id: session_id.clone(), text: text.to_string(), limit })
        .await
        .map_err(|e| e.to_string())?;

    let output = if json {
        let rows: Vec<serde_json::Value> = hits
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "seq": hit.seq.as_u64(),
                    "event_type": hit.event_type,
                    "turn_id": hit.turn_id.as_ref().map(|id| id.as_str()),
                    "step_id": hit.step_id.as_ref().map(|id| id.as_str()),
                    "snippet": hit.snippet,
                    // Present so a caller can see why one hit came before
                    // another; comparable only within this one array.
                    "score": hit.score,
                })
            })
            .collect();
        CliOutput::Json(json_output(&serde_json::json!({
            "session": session_id.as_str(),
            "query": text,
            "hits": rows,
        }))?)
    } else if hits.is_empty() {
        CliOutput::Text(format!("no matches for '{text}'"))
    } else {
        CliOutput::Text(
            hits.iter()
                .map(|hit| {
                    format!(
                        "{}  {}  {}",
                        hit.seq.as_u64(),
                        hit.event_type,
                        hit.snippet.replace('\n', " ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    Ok(CliOutcome::ok(output))
}

/// `memory reindex`: discard the derived index and build it from the log.
pub async fn run_memory_reindex(
    index: &dyn MemoryIndex,
    session_id: &SessionId,
    json: bool,
) -> Result<CliOutcome, String> {
    let entries = index.rebuild(session_id).await.map_err(|e| e.to_string())?;
    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({
            "session": session_id.as_str(),
            "entries": entries,
        }))?)
    } else {
        CliOutput::Text(format!("indexed {entries} entries for session '{session_id}'"))
    };
    Ok(CliOutcome::ok(output))
}

/// One fact as JSON, the shape `--json` and `memory export` share.
///
/// Times are RFC 3339 in UTC. `key`, `source`, `supersedes` and `expires_at`
/// are `null` when absent rather than omitted: a reader that has to tell "no
/// key" from "field this build does not write" should be able to, and a null
/// says the first.
pub fn fact_json(fact: &MemoryFact) -> serde_json::Value {
    serde_json::json!({
        "id": fact.id,
        "namespace": fact.namespace,
        "key": fact.key,
        "text": fact.text,
        "confidence": fact.confidence,
        "origin": fact.origin.as_str(),
        "source": fact.source.as_ref().map(|source| serde_json::json!({
            "session": source.session.as_str(),
            "call": source.call,
        })),
        "sensitivity": fact.sensitivity.as_str(),
        "created_at": fact.created_at.to_rfc3339(),
        "updated_at": fact.updated_at.to_rfc3339(),
        "supersedes": fact.supersedes,
        "expires_at": fact.expires_at.map(|at| at.to_rfc3339()),
        "status": fact.status.as_str(),
    })
}

/// One fact as a line a person reads: the handle first, then what it is.
fn fact_line(fact: &MemoryFact) -> String {
    let identity = match &fact.key {
        Some(key) => format!("{}/{key}", fact.namespace),
        None => fact.namespace.clone(),
    };
    let mut line = format!("{}  {identity}  {}", fact.id, fact.text.replace('\n', " "));
    if fact.status != MemoryStatus::Active {
        line.push_str("  (replaced)");
    }
    if fact.sensitivity != MemorySensitivity::Normal {
        line.push_str("  (private)");
    }
    line
}

/// `memory remember <text>`: write one fact down.
///
/// Conflicts are failures (`2`), because a second fact for a key without a
/// `supersedes` is a request that could not be carried out — and the error
/// names the id that has to be superseded.
pub async fn run_memory_remember(
    store: &dyn MemoryStore,
    request: Remember,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    let fact = store.remember(request, now).await.map_err(|e| e.to_string())?;
    let output = if json {
        CliOutput::Json(json_output(&fact_json(&fact))?)
    } else {
        CliOutput::Text(format!("remembered {}", fact.id))
    };
    Ok(CliOutcome::ok(output))
}

/// `memory list`: every active fact, retired and expired included when asked.
pub async fn run_memory_list(
    store: &dyn MemoryStore,
    filter: &ListFacts,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    let facts = store.list(filter, now).await.map_err(|e| e.to_string())?;
    let output = if json {
        let rows: Vec<serde_json::Value> = facts.iter().map(fact_json).collect();
        CliOutput::Json(json_output(&serde_json::Value::Array(rows))?)
    } else if facts.is_empty() {
        CliOutput::Text("nothing is remembered".to_string())
    } else {
        CliOutput::Text(facts.iter().map(fact_line).collect::<Vec<_>>().join("\n"))
    };
    Ok(CliOutcome::ok(output))
}

/// `memory forget <id>`: delete a fact for good.
///
/// A fact that is already gone is an answer (`0`, "no fact with id …"), the
/// same way a search with no matches is: the caller asked for a state that
/// already holds.
pub async fn run_memory_forget(
    store: &dyn MemoryStore,
    id: &str,
    json: bool,
) -> Result<CliOutcome, String> {
    let forgotten = store.forget(id).await.map_err(|e| e.to_string())?;
    let output = if json {
        CliOutput::Json(json_output(&serde_json::json!({ "id": id, "forgotten": forgotten }))?)
    } else if forgotten {
        CliOutput::Text(format!("forgot {id}"))
    } else {
        CliOutput::Text(format!("no fact with id '{id}'"))
    };
    Ok(CliOutcome::ok(output))
}

/// `memory export`: every fact this store holds, as one JSON array.
///
/// This is the way out of a version this build cannot read, so it exports
/// everything — retired and expired included — and it is JSON in both modes:
/// there is no human rendering of "all of it" that could be pasted back into a
/// migration. With `out` the array is written there and the answer is a
/// sentence about the file; without it the array is the answer.
pub async fn run_memory_export(
    store: &dyn MemoryStore,
    now: DateTime<Utc>,
    out: Option<&str>,
    json: bool,
) -> Result<CliOutcome, String> {
    let facts = store
        .list(
            &ListFacts { include_superseded: true, include_expired: true, ..Default::default() },
            now,
        )
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<serde_json::Value> = facts.iter().map(fact_json).collect();
    let document = json_output(&serde_json::Value::Array(rows))?;

    if let Some(path) = out {
        std::fs::write(path, format!("{document}\n"))
            .map_err(|e| format!("cannot write {path}: {e}"))?;
        let output = if json {
            CliOutput::Json(json_output(&serde_json::json!({
                "facts": facts.len(),
                "out": path,
            }))?)
        } else {
            CliOutput::Text(format!("wrote {} fact(s) to {path}", facts.len()))
        };
        return Ok(CliOutcome::ok(output));
    }
    Ok(CliOutcome::ok(CliOutput::Json(document)))
}
