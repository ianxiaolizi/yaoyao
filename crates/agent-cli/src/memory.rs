//! The `memory` commands: a search and a rebuild over the derived index.
//!
//! Neither command writes an event — the index is not a fact, and a search that
//! changed the log would make "what the model saw" depend on who searched. The
//! port is injected rather than built here, so this crate does not depend on a
//! particular storage engine.

use agent_ports::memory_index::{MemoryIndex, MemoryQuery};
use agent_protocol::ids::SessionId;

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
