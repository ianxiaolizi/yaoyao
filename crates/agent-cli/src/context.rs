//! `context inspect`: what the model's next request would carry.
//!
//! This is a reader, not a run: it opens no providers, registers no plugins,
//! and writes no event. It is handed a report already built by the layer that
//! may see both the projection and the trimmer, plus the session's events for
//! the last request's recorded facts. Keeping the presentation here means the
//! command has one place to decide its text and its JSON, and one set of tests
//! that do not need a live model.
//!
//! The report itself is built elsewhere on purpose: the estimate has one
//! implementation (`agent-context::trim`), and this crate may not depend on
//! that crate. Duplicating the counting here would be a second answer to "how
//! big is this", which is the defect the single estimator exists to prevent.

use agent_ports::context_engine::ContextUsageReport;
use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId, TurnId};

use crate::{CliOutcome, CliOutput, json_output};

/// How many largest messages `context inspect` shows when `--top` is absent.
pub const DEFAULT_TOP_MESSAGES: usize = 5;

/// How much of a per-turn layer's text the human-readable output shows.
///
/// The layers are bounded when placed (the task layer quotes at most 2000
/// characters), but a report line should stay a report line. The JSON answer
/// carries the full text as recorded.
const LAYER_PREVIEW_CHARS: usize = 160;

/// Facts recorded on the last `model/request.prepared`.
///
/// Read back rather than re-derived: the log says what the request actually
/// carried, including whether a notice was placed. A report that recomputed
/// these would describe what the next request *would* say, not what the last
/// one did.
struct LastRequest {
    seq: EventSeq,
    attempt_id: Option<String>,
    context_revision: Option<u64>,
    message_count: Option<u64>,
    context_message_count: Option<u64>,
    dropped_message_count: Option<u64>,
    layout_version: Option<u64>,
    /// The settled derivation's estimate: history only.
    estimated_context_tokens: Option<u64>,
    /// The whole request's estimate: every message actually sent, including
    /// the stable head, the per-turn layers, the input and in-flight traffic.
    estimated_messages_tokens: Option<u64>,
    /// The tool definitions sent beside the messages.
    estimated_tool_tokens: Option<u64>,
    /// Messages plus tools: the number to compare with provider input.
    estimated_request_tokens: Option<u64>,
    max_input_tokens: Option<u64>,
    reserved_output_tokens: Option<u64>,
    tool_manifest_id: Option<String>,
    tool_count: Option<u64>,
    /// The definitions behind `tool_manifest_id`, in full, from the
    /// `model/tools.frozen` event that preceded this request. The manifest id
    /// alone cannot say what was in it once the process is gone.
    tool_definitions: Option<serde_json::Value>,
    /// The ordered messages this request sent, as `{message_uid, role, hash}`.
    /// The content of each uid is durable in the events that produced it; this
    /// is the list that proves which ones, in which order, were sent.
    messages: Option<serde_json::Value>,
    messages_hash: Option<String>,
    required_capabilities: Vec<String>,
    notice: Option<serde_json::Value>,
    /// The run-scoped capability layer the request carried, as recorded by the
    /// loop. Like the per-turn layers it is not part of the derived context.
    capability: Option<serde_json::Value>,
    /// The per-turn state and task layers the request carried, as recorded by
    /// the loop. They are not part of the derived context, so they appear here
    /// rather than in the category table.
    layers: Option<serde_json::Value>,
    /// The deterministic ledger this turn's compaction wrote, if any: the
    /// message that carries it, its view version, and a hash of the text the
    /// request sent.
    ledger: Option<serde_json::Value>,
    /// The verbatim record of the user's words this turn's compaction wrote,
    /// if any: which message carries it and a hash of the text sent.
    usertext: Option<serde_json::Value>,
    /// The failures record this turn's compaction wrote, if any: which message
    /// carries it and a hash of the text sent.
    failures: Option<serde_json::Value>,
    provider_usage: Option<ProviderUsage>,
}

/// The last compaction attempt this log recorded, compacted or failed.
///
/// Both outcomes are facts worth reading back: a session that keeps failing to
/// summarize is one whose history is only protected by the trimmer, and that
/// is not visible from the request alone. The payload is the recorded event's,
/// so the JSON answer carries every field the loop wrote without a second copy
/// of the schema here.
struct LastCompaction {
    seq: EventSeq,
    /// `compacted` or `failed`.
    outcome: &'static str,
    payload: serde_json::Value,
}

impl LastCompaction {
    fn to_json(&self) -> serde_json::Value {
        let mut value = self.payload.clone();
        value["seq"] = serde_json::json!(self.seq.as_u64());
        value["outcome"] = serde_json::json!(self.outcome);
        value
    }
}

/// The provider's own token counts for the attempt the last request opened.
///
/// `cache_hit_tokens`/`cache_miss_tokens` are the only numbers that say whether
/// the request layout is actually cheap: a hit costs a fraction of a miss.
struct ProviderUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_hit_tokens: Option<u64>,
    cache_miss_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

impl LastRequest {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "seq": self.seq.as_u64(),
            "attempt_id": self.attempt_id,
            "context_revision": self.context_revision,
            "message_count": self.message_count,
            "context_message_count": self.context_message_count,
            "dropped_message_count": self.dropped_message_count,
            "layout_version": self.layout_version,
            "estimated_context_tokens": self.estimated_context_tokens,
            "estimated_messages_tokens": self.estimated_messages_tokens,
            "estimated_tool_tokens": self.estimated_tool_tokens,
            "estimated_request_tokens": self.estimated_request_tokens,
            "max_input_tokens": self.max_input_tokens,
            "reserved_output_tokens": self.reserved_output_tokens,
            "tool_manifest_id": self.tool_manifest_id,
            "tool_count": self.tool_count,
            "tool_definitions": self.tool_definitions,
            "messages": self.messages,
            "messages_hash": self.messages_hash,
            "required_capabilities": self.required_capabilities,
            "context_notice": self.notice,
            "context_capability": self.capability,
            "context_layers": self.layers,
            "context_ledger": self.ledger,
            "context_usertext": self.usertext,
            "context_failures": self.failures,
            "provider_usage": self.provider_usage.as_ref().map(ProviderUsage::to_json),
        })
    }
}

impl ProviderUsage {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": self.total_tokens,
            "cache_hit_tokens": self.cache_hit_tokens,
            "cache_miss_tokens": self.cache_miss_tokens,
            "reasoning_tokens": self.reasoning_tokens,
        })
    }
}

/// Render one context report.
///
/// The verdict is always `Answered` once the report exists: an empty session is
/// an answer about an empty context, not a command that failed to run. A log
/// that could not be read never reaches this function.
pub fn run_context_inspect(
    report: &ContextUsageReport,
    session_id: &SessionId,
    revision: EventSeq,
    events: &[StoredEvent],
    json: bool,
) -> Result<CliOutcome, String> {
    let last = last_request(events);
    let compaction = last_compaction(events);

    let output = if json {
        let value = serde_json::json!({
            "session": session_id.as_str(),
            "context_revision": revision.as_u64(),
            "context": report,
            "last_request": last.as_ref().map(LastRequest::to_json),
            "last_compaction": compaction.as_ref().map(LastCompaction::to_json),
        });
        CliOutput::Json(json_output(&value)?)
    } else {
        CliOutput::Text(render_text(
            report,
            session_id,
            revision,
            last.as_ref(),
            compaction.as_ref(),
        ))
    };
    Ok(CliOutcome::ok(output))
}

fn render_text(
    report: &ContextUsageReport,
    session_id: &SessionId,
    revision: EventSeq,
    last: Option<&LastRequest>,
    compaction: Option<&LastCompaction>,
) -> String {
    let mut lines = vec![
        format!("session: {session_id}"),
        format!("context revision: {} (the last folded log position)", revision.as_u64()),
        format!(
            "messages: {} in context; {} dropped by the window",
            report.message_count, report.dropped_messages
        ),
        budget_line(report),
        String::from("by kind:"),
    ];
    for category in &report.categories {
        lines.push(format!(
            "  {:<20} {:>3} msg {:>3} block {:>7} tok",
            category.kind.as_str(),
            category.messages,
            category.blocks,
            category.estimated_tokens
        ));
    }

    lines.push(String::from("largest messages:"));
    if report.largest_messages.is_empty() {
        lines.push(String::from("  (none)"));
    }
    for (index, message) in report.largest_messages.iter().enumerate() {
        let kinds = message.kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join("+");
        lines.push(format!(
            "  {}. {:>7} tok  {:<9} [{}]",
            index + 1,
            message.estimated_tokens,
            message.role.as_str(),
            kinds
        ));
        lines.push(format!("     {}  \"{}\"", message.message_uid, message.preview));
    }

    match last {
        None => lines.push(String::from("last request: none (no model request in this log)")),
        Some(last) => {
            lines.push(format!("last request: seq {}", last.seq.as_u64()));
            if let Some(revision) = last.context_revision {
                lines.push(format!("  context revision: {revision}"));
            }
            let sent = last.message_count.map(|count| count.to_string());
            let settled = last.context_message_count.map(|count| count.to_string());
            let dropped = last.dropped_message_count.map(|count| count.to_string());
            let estimate = last.estimated_context_tokens;
            if sent.is_some() || settled.is_some() || dropped.is_some() || estimate.is_some() {
                lines.push(format!(
                    "  request: {} sent / {} settled; {} dropped; estimate {} tok",
                    sent.as_deref().unwrap_or("?"),
                    settled.as_deref().unwrap_or("?"),
                    dropped.as_deref().unwrap_or("?"),
                    estimate.map(|value| value.to_string()).as_deref().unwrap_or("?"),
                ));
            }
            if let Some(count) = last.tool_count {
                let manifest = last.tool_manifest_id.as_deref().unwrap_or("?");
                let capabilities = if last.required_capabilities.is_empty() {
                    String::from("none")
                } else {
                    last.required_capabilities.join(", ")
                };
                lines.push(format!(
                    "  tools: {count} tool(s), manifest {manifest}; required capabilities: {capabilities}"
                ));
            }
            if let Some(frozen) = &last.tool_definitions {
                let count = frozen.get("tool_count").and_then(serde_json::Value::as_u64);
                let seq = frozen.get("seq").and_then(serde_json::Value::as_u64);
                let hash =
                    frozen.get("entries_hash").and_then(serde_json::Value::as_str).unwrap_or("?");
                lines.push(format!(
                    "  tool definitions: {} frozen at seq {} (entries hash {hash})",
                    count.map(|value| value.to_string()).as_deref().unwrap_or("?"),
                    seq.map(|value| value.to_string()).as_deref().unwrap_or("?"),
                ));
            }
            if let Some(messages) = last.messages.as_ref().and_then(serde_json::Value::as_array) {
                lines.push(format!(
                    "  message refs: {} sent, hash {}",
                    messages.len(),
                    last.messages_hash.as_deref().unwrap_or("?")
                ));
            }
            if let Some(notice) = &last.notice {
                let kind = notice.get("kind").and_then(|value| value.as_str()).unwrap_or("?");
                lines.push(format!("  notice: {kind}"));
            }
            if let Some(preview) = last.capability.as_ref().and_then(text_preview) {
                lines.push(format!("  capability layer: {preview}"));
            }
            if let Some(layers) = &last.layers {
                for (key, label) in [("state", "state layer"), ("task", "task layer")] {
                    if let Some(preview) = layer_preview(layers, key) {
                        lines.push(format!("  {label}: {preview}"));
                    }
                }
            }
            if let Some(compaction) = compaction {
                lines.push(compaction_line(compaction));
            }
            if let Some(usage) = &last.provider_usage {
                let mut line = format!(
                    "  provider usage for that attempt: input {}, output {}, total {}",
                    usage.input_tokens.map(|value| value.to_string()).as_deref().unwrap_or("?"),
                    usage.output_tokens.map(|value| value.to_string()).as_deref().unwrap_or("?"),
                    usage.total_tokens.map(|value| value.to_string()).as_deref().unwrap_or("?"),
                );
                match (usage.cache_hit_tokens, usage.cache_miss_tokens) {
                    (Some(hit), Some(miss)) if hit + miss > 0 => {
                        line.push_str(&format!(
                            "; cache hit {hit} / miss {miss} ({}% hit)",
                            hit.saturating_mul(100) / (hit + miss)
                        ));
                    }
                    (Some(hit), _) => line.push_str(&format!("; cache hit {hit}")),
                    _ => {}
                }
                lines.push(line);
                if let Some(reasoning) = usage.reasoning_tokens {
                    lines.push(format!("  reasoning tokens: {reasoning}"));
                }
                if let Some(estimated) = last.estimated_request_tokens {
                    let comparison = match usage.input_tokens {
                        Some(actual) if actual > 0 => format!(
                            "; provider input {actual}; difference {}%",
                            estimated.abs_diff(actual) * 100 / actual
                        ),
                        _ => String::new(),
                    };
                    lines.push(format!(
                        "  estimated request: {estimated} tok (messages {}, tools {}){comparison}",
                        last.estimated_messages_tokens
                            .map(|value| value.to_string())
                            .as_deref()
                            .unwrap_or("?"),
                        last.estimated_tool_tokens
                            .map(|value| value.to_string())
                            .as_deref()
                            .unwrap_or("?"),
                    ));
                } else if estimate.is_some() {
                    lines.push(String::from(
                        "  this request predates whole-request estimates; only the settled \
                         context is estimated",
                    ));
                }
            }
        }
    }

    lines.join("\n")
}

/// One report line for the last compaction attempt.
///
/// The summary itself is not repeated: it is in the event, it can be long, and
/// the line's job is to say whether the rewrite happened and how much it moved.
fn compaction_line(compaction: &LastCompaction) -> String {
    let payload = &compaction.payload;
    if compaction.outcome == "failed" {
        let kind = payload.get("error_kind").and_then(|v| v.as_str()).unwrap_or("?");
        let message = payload
            .get("message")
            .and_then(|v| v.as_str())
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        return format!(
            "last compaction: seq {}, failed ({kind}): {message}",
            compaction.seq.as_u64()
        );
    }
    let summarized = payload.get("message_count").and_then(|v| v.as_u64());
    let removed = payload.get("removed_messages").and_then(|v| v.as_u64());
    let truncated = payload.get("truncated").and_then(|v| v.as_bool());
    let uid = payload.get("summary_message_uid").and_then(|v| v.as_str()).unwrap_or("?");
    let ledger = payload.get("ledger").and_then(|ledger| {
        let lines = ledger.get("lines")?.as_u64()?;
        let elided = ledger.get("elided").and_then(|v| v.as_u64()).unwrap_or(0);
        Some(format!("; ledger {lines} line(s), {elided} elided"))
    });
    let usertext = payload.get("usertext").and_then(|record| {
        let messages = record.get("messages")?.as_u64()?;
        let elided = record.get("elided").and_then(|v| v.as_u64()).unwrap_or(0);
        Some(format!("; user record {messages} message(s), {elided} elided"))
    });
    let failures = payload.get("failures").and_then(|record| {
        let entries = record.get("entries")?.as_u64()?;
        let elided = record.get("elided").and_then(|v| v.as_u64()).unwrap_or(0);
        Some(format!("; failures {entries} recorded, {elided} elided"))
    });
    format!(
        "last compaction: seq {}, summarized {} message(s) into {uid}; removed {}; truncated {}{}{}{}",
        compaction.seq.as_u64(),
        summarized.map(|v| v.to_string()).as_deref().unwrap_or("?"),
        removed.map(|v| v.to_string()).as_deref().unwrap_or("?"),
        match truncated {
            Some(true) => "yes",
            Some(false) => "no",
            None => "?",
        },
        ledger.unwrap_or_default(),
        usertext.unwrap_or_default(),
        failures.unwrap_or_default()
    )
}

fn budget_line(report: &ContextUsageReport) -> String {
    let Some(max) = report.max_input_tokens else {
        return String::from("budget: not configured; history is sent untrimmed");
    };
    let reserved = report.reserved_output_tokens.unwrap_or(0);
    let budget = report.budget_tokens.unwrap_or(0);
    let percent = report.percent_used.map(|value| value.to_string()).unwrap_or_else(|| "?".into());
    let remaining = report.remaining_tokens.unwrap_or(0);
    let over = report.estimated_tokens.saturating_sub(budget);
    if over > 0 {
        format!(
            "budget: {max} input tokens, {reserved} reserved, {budget} usable; about {} used \
             ({percent}%), 0 left (over the usable budget by {over})",
            report.estimated_tokens
        )
    } else {
        format!(
            "budget: {max} input tokens, {reserved} reserved, {budget} usable; about {} used \
             ({percent}%), {remaining} left",
            report.estimated_tokens
        )
    }
}

/// The last prepared request, with the response that settled the same attempt.
///
/// Pairing is by `attempt_id` rather than by position: an attempt that failed
/// has no response, and an older response must not be attached to a newer
/// request just because it came last in the file.
fn last_request(events: &[StoredEvent]) -> Option<LastRequest> {
    let mut last: Option<LastRequest> = None;
    // The latest freeze. It is turn-scoped and precedes that turn's first
    // request, so the one attached below is the one this request was sent
    // under — matched by turn id, because an older log may have no freeze and
    // must not inherit a newer turn's.
    let mut frozen: Option<(Option<TurnId>, EventSeq, serde_json::Value)> = None;
    for event in events {
        let payload = &event.event.payload;
        match event.event.event_type.as_str() {
            EventType::MODEL_TOOLS_FROZEN => {
                frozen = Some((event.event.turn_id.clone(), event.seq, payload.clone()));
            }
            EventType::MODEL_REQUEST_PREPARED => {
                let definitions =
                    frozen.as_ref().filter(|(turn, _, _)| *turn == event.event.turn_id);
                last = Some(LastRequest {
                    seq: event.seq,
                    attempt_id: string_field(payload, "attempt_id"),
                    context_revision: number_field(payload, "context_revision"),
                    message_count: number_field(payload, "message_count"),
                    context_message_count: number_field(payload, "context_message_count"),
                    dropped_message_count: number_field(payload, "dropped_message_count"),
                    layout_version: number_field(payload, "layout_version"),
                    estimated_context_tokens: number_field(payload, "estimated_context_tokens"),
                    estimated_messages_tokens: number_field(payload, "estimated_messages_tokens"),
                    estimated_tool_tokens: number_field(payload, "estimated_tool_tokens"),
                    estimated_request_tokens: number_field(payload, "estimated_request_tokens"),
                    max_input_tokens: number_field(payload, "max_input_tokens"),
                    reserved_output_tokens: number_field(payload, "reserved_output_tokens"),
                    tool_manifest_id: string_field(payload, "tool_manifest_id"),
                    tool_count: number_field(payload, "tool_count"),
                    tool_definitions: definitions.map(|(_, seq, frozen)| {
                        let mut recorded = frozen.clone();
                        recorded["seq"] = serde_json::json!(seq.as_u64());
                        recorded
                    }),
                    messages: payload.get("messages").cloned(),
                    messages_hash: string_field(payload, "messages_hash"),
                    required_capabilities: strings_field(payload, "required_capabilities"),
                    notice: payload.get("context_notice").cloned(),
                    capability: payload.get("context_capability").cloned(),
                    layers: payload.get("context_layers").cloned(),
                    ledger: payload.get("context_ledger").cloned(),
                    usertext: payload.get("context_usertext").cloned(),
                    failures: payload.get("context_failures").cloned(),
                    provider_usage: None,
                });
            }
            EventType::MODEL_RESPONSE_COMPLETED => {
                let Some(request) = last.as_mut() else {
                    continue;
                };
                let attempt = string_field(payload, "attempt_id");
                if attempt.is_none() || attempt != request.attempt_id {
                    continue;
                }
                request.provider_usage = provider_usage(payload);
            }
            _ => {}
        }
    }
    last
}

/// The last compaction outcome in the log, whichever kind it was.
///
/// `compaction.started` is deliberately not an outcome: a start with neither a
/// compaction nor a failure after it means the log was cut short or read while
/// the turn was in flight, and reporting it as "the last compaction" would
/// describe something that has not happened.
fn last_compaction(events: &[StoredEvent]) -> Option<LastCompaction> {
    let mut last: Option<LastCompaction> = None;
    for event in events {
        let outcome = match event.event.event_type.as_str() {
            EventType::CONTEXT_COMPACTED => "compacted",
            EventType::CONTEXT_COMPACTION_FAILED => "failed",
            _ => continue,
        };
        last =
            Some(LastCompaction { seq: event.seq, outcome, payload: event.event.payload.clone() });
    }
    last
}

fn provider_usage(payload: &serde_json::Value) -> Option<ProviderUsage> {
    let usage = payload.get("response")?.get("usage")?;
    let parsed = ProviderUsage {
        input_tokens: usage.get("input_tokens").and_then(serde_json::Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(serde_json::Value::as_u64),
        total_tokens: usage.get("total_tokens").and_then(serde_json::Value::as_u64),
        cache_hit_tokens: usage.get("cache_hit_tokens").and_then(serde_json::Value::as_u64),
        cache_miss_tokens: usage.get("cache_miss_tokens").and_then(serde_json::Value::as_u64),
        reasoning_tokens: usage.get("reasoning_tokens").and_then(serde_json::Value::as_u64),
    };
    // The key exists but reports nothing — a provider that does not count
    // tokens still serializes its (empty) usage. That is "unreported", not
    // "zero", and printing `?` for it would suggest a count went missing.
    if parsed.input_tokens.is_none()
        && parsed.output_tokens.is_none()
        && parsed.total_tokens.is_none()
        && parsed.cache_hit_tokens.is_none()
        && parsed.cache_miss_tokens.is_none()
        && parsed.reasoning_tokens.is_none()
    {
        return None;
    }
    Some(parsed)
}

/// One layer's text, collapsed and cut to [`LAYER_PREVIEW_CHARS`].
///
/// Collapsed because the layer's own line breaks would split one report line
/// into several; cut by characters for the same reason the message preview is.
fn layer_preview(layers: &serde_json::Value, key: &str) -> Option<String> {
    text_preview(layers.get(key)?)
}

/// The `text` of one recorded layer, collapsed and cut the same way.
fn text_preview(recorded: &serde_json::Value) -> Option<String> {
    let text = recorded.get("text")?.as_str()?;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let mut preview: String = chars.by_ref().take(LAYER_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    Some(preview)
}

fn string_field(payload: &serde_json::Value, name: &str) -> Option<String> {
    payload.get(name).and_then(serde_json::Value::as_str).map(str::to_string)
}

fn number_field(payload: &serde_json::Value, name: &str) -> Option<u64> {
    payload.get(name).and_then(serde_json::Value::as_u64)
}

fn strings_field(payload: &serde_json::Value, name: &str) -> Vec<String> {
    payload
        .get(name)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values.iter().filter_map(serde_json::Value::as_str).map(str::to_string).collect()
        })
        .unwrap_or_default()
}
