//! Minimal judgment fold for the session's ledger.
//!
//! The kernel cannot depend on agent-projection, so the Scheduler reads the
//! judgment straight from the event stream here, the way it reads the plan and
//! the lifecycle. Each `judgment/updated` already carries the merged record, so
//! the latest one is the current record and the earlier ones are history.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::judgment::{self as judgment_record, Judgment, JudgmentStage};
use agent_protocol::plan;
use agent_protocol::tool::{
    APPROVAL_DENIED_OUTCOME, APPROVAL_PENDING_OUTCOME, GATE_REFUSED_OUTCOME,
    GATEWAY_REFUSED_OUTCOME,
};

/// The judgment the session currently carries, if any.
///
/// A malformed payload is an error rather than "no judgment": the event was
/// written by this build, so a payload that will not read back is a log
/// anomaly, and silently opening the next turn with no record would hide it.
pub fn current(events: &[StoredEvent]) -> Result<Option<Judgment>, serde_json::Error> {
    for stored in events.iter().rev() {
        if stored.event.event_type.as_str() != EventType::JUDGMENT_UPDATED {
            continue;
        }
        let judgment = serde_json::from_value(
            stored.event.payload.get("judgment").cloned().unwrap_or_default(),
        )?;
        return Ok(Some(judgment));
    }
    Ok(None)
}

/// A tool failure the session has not reconciled yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFailure {
    /// The provider's id for the call, which the gate blocks on once.
    pub call_id: String,
    pub tool: String,
    /// The first line of what the tool said, for the note the loop writes.
    pub detail: String,
}

/// The last real tool failure the judgment record has not reconciled.
///
/// This exists for the resume boundary: an approval holds a call, the turn
/// yields, and the resumed call fails in a turn of its own. The loop that
/// opens that turn has no memory of the previous one, so the fact is read back
/// from the log and handed to it — otherwise a failure that happened across a
/// yield would never be gated.
///
/// Only failures the loop would have tracked are eligible: a call that did not
/// run (`approval_pending`, `approval_denied`), that the gateway refused before
/// running (`gateway_refused`), or that a kernel gate held with its batch
/// (`gate_refused`) carries its own actionable text and is not an observation
/// about the world. The loop's own ledgers (`plan.update`,
/// `judgment.update`) are refusals of the model's own writing, on the same
/// terms. Anything earlier than the latest reconcile is settled.
pub fn pending_failure(events: &[StoredEvent]) -> Option<PendingFailure> {
    let reconciled_at = events
        .iter()
        .rev()
        .find(|stored| {
            stored.event.event_type.as_str() == EventType::JUDGMENT_UPDATED
                && stored
                    .event
                    .payload
                    .get("judgment")
                    .and_then(|record| record.get("stage"))
                    .and_then(|stage| stage.as_str())
                    == Some(JudgmentStage::Reconcile.as_str())
        })
        .map(|stored| stored.seq.as_u64());

    for stored in events.iter().rev() {
        // Sequence numbers start at zero, so "before the reconcile" cannot be
        // spelled as a comparison against a zero default.
        if let Some(settled) = reconciled_at
            && stored.seq.as_u64() <= settled
        {
            return None;
        }
        if stored.event.event_type.as_str() != EventType::TOOL_FAILED {
            continue;
        }
        let payload = &stored.event.payload;
        if matches!(
            payload.get("outcome").and_then(|value| value.as_str()),
            Some(APPROVAL_PENDING_OUTCOME)
                | Some(APPROVAL_DENIED_OUTCOME)
                | Some(GATEWAY_REFUSED_OUTCOME)
                | Some(GATE_REFUSED_OUTCOME)
        ) {
            continue;
        }
        let Some(tool) = payload.get("tool_name").and_then(|value| value.as_str()) else {
            continue;
        };
        if tool == plan::TOOL_NAME || tool == judgment_record::TOOL_NAME {
            continue;
        }
        let content = payload.get("content").and_then(|value| value.as_str()).unwrap_or("");
        return Some(PendingFailure {
            call_id: payload
                .get("provider_tool_call_id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            tool: tool.to_string(),
            detail: content.lines().next().unwrap_or_default().to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft, EventType};
    use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId};
    use chrono::Utc;

    fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: agent_protocol::ids::EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("s"),
                run_id: None,
                turn_id: None,
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

    fn judgment_event(seq: u64, stage: &str) -> StoredEvent {
        stored(
            seq,
            EventType::JUDGMENT_UPDATED,
            serde_json::json!({
                "provider_tool_call_id": "call-1",
                "judgment": { "stage": stage, "reviewed": "shell.run cat x" },
            }),
        )
    }

    #[test]
    fn no_judgment_event_means_no_record() {
        let events = vec![stored(1, EventType::TURN_STARTED, serde_json::json!({}))];
        assert_eq!(current(&events).expect("readable"), None);
    }

    #[test]
    fn the_latest_judgment_replaces_the_previous_one() {
        let events = vec![judgment_event(1, "situate"), judgment_event(2, "reconcile")];
        let judgment = current(&events).expect("readable").expect("a record");
        assert_eq!(judgment.stage.map(|stage| stage.as_str()), Some("reconcile"));
        assert_eq!(judgment.reviewed, "shell.run cat x");
    }

    fn tool_failed(seq: u64, tool: &str, call: &str, outcome: Option<&str>) -> StoredEvent {
        let mut payload = serde_json::json!({
            "tool_name": tool,
            "provider_tool_call_id": call,
            "status": "failed",
            "content": "no such file: missing.txt\nsecond line",
        });
        if let Some(outcome) = outcome {
            payload["outcome"] = serde_json::json!(outcome);
        }
        stored(seq, EventType::TOOL_FAILED, payload)
    }

    #[test]
    fn a_real_tool_failure_is_pending_until_reconciled() {
        let events = vec![tool_failed(3, "shell.run", "call-1", None)];
        let pending = pending_failure(&events).expect("pending");
        assert_eq!(pending.tool, "shell.run");
        assert_eq!(pending.call_id, "call-1");
        assert_eq!(pending.detail, "no such file: missing.txt");

        let mut events = events;
        events.push(judgment_event(4, "reconcile"));
        assert_eq!(pending_failure(&events), None);
    }

    #[test]
    fn calls_that_did_not_run_or_were_refused_are_not_failures() {
        for outcome in [
            APPROVAL_PENDING_OUTCOME,
            APPROVAL_DENIED_OUTCOME,
            GATEWAY_REFUSED_OUTCOME,
            GATE_REFUSED_OUTCOME,
        ] {
            let events = vec![tool_failed(3, "shell.run", "call-1", Some(outcome))];
            assert_eq!(pending_failure(&events), None, "{outcome}");
        }
    }

    #[test]
    fn the_loop_ledgers_are_not_world_observations() {
        let events = vec![
            tool_failed(3, plan::TOOL_NAME, "call-1", None),
            tool_failed(4, judgment_record::TOOL_NAME, "call-2", None),
        ];
        assert_eq!(pending_failure(&events), None);
    }

    #[test]
    fn only_failures_after_the_last_reconcile_are_pending() {
        let events = vec![
            tool_failed(3, "shell.run", "call-1", None),
            judgment_event(4, "reconcile"),
            tool_failed(5, "fs.read", "call-2", None),
        ];
        let pending = pending_failure(&events).expect("pending");
        assert_eq!(pending.call_id, "call-2");
    }

    #[test]
    fn a_judgment_event_without_a_readable_record_is_an_error() {
        // An empty object is a valid (empty) record, so the unreadable case is
        // a payload of the wrong shape.
        let broken = stored(
            1,
            EventType::JUDGMENT_UPDATED,
            serde_json::json!({ "judgment": { "stage": "think" } }),
        );
        assert!(current(&[broken]).is_err());
    }
}
