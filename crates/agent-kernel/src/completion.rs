//! The completion gate: does a check cover the work on the table?
//!
//! [`agent_protocol::evidence`] names what a check observed and what refusal
//! looks like. This module answers, from one session's log, which verdict the
//! latest check gets.
//!
//! Two rules carry the whole thing:
//!
//! * **Freshness is a revision, not a clock.** [`task_revision`] counts the
//!   events that can have changed what is on the table. A check is fresh when
//!   the revision has not moved since it ran; a timestamp comparison would let
//!   a check from before a change look current whenever the clock was coarse.
//! * **A claim is refused, not amended.** The gate never upgrades a check: a
//!   criterion that ran through a pipeline ending in `| tail` reports the
//!   pipeline's status, so it is refused even though the gateway called it a
//!   success (CONTRACT §7 #170).
//!
//! The fold is read-only, like [`crate::status`] and [`crate::intent`]: two
//! readers of one log answer the same thing, and `status` may be asked while
//! another process drives the session.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::evidence::{
    ClaimVerdict, Evidence, EvidenceKind, EvidenceStatus, masking_operator,
};
use agent_protocol::ids::EventSeq;
use serde_json::Value;

/// How many events in this session can have changed the work on the table.
///
/// Every `tool/*` event counts except `tool/not_started`: that one is the log's
/// own word for a call the kernel refused before dispatch, so it is the one
/// event in the family that provably changed nothing (CONTRACT §7 #132). The
/// revision is a count rather than a digest because it only has to answer "did
/// anything happen since?", and a count is something a person can check by
/// reading the log.
pub fn task_revision(events: &[StoredEvent]) -> u64 {
    events.iter().filter(|stored| is_change(stored)).count() as u64
}

/// Whether a check that ran at `seq` is still current.
///
/// The comparison is against the revision *before* the check, not after: the
/// check's own tool events (a criterion that runs a command is dispatched like
/// any other call) must not make it stale the moment it is written.
pub fn is_current(events: &[StoredEvent], evidence: &Evidence) -> bool {
    evidence.task_revision == revision_before(events, latest_evidence_seq(events))
}

/// The apparatus this session's checks are graded against, if one was frozen.
///
/// Only the digest is kept: the gate answers "is this the same apparatus?", and
/// a reader that wants *which file* changed reads the freeze event itself, where
/// the entries are. A freeze this build cannot parse is kept as `hash: None` and
/// therefore matches nothing — a manifest this build cannot read is not a
/// licence to accept, which is the same direction as every other unknown here.
pub fn frozen_apparatus(events: &[StoredEvent]) -> Option<FrozenApparatus> {
    let stored = events.iter().rfind(|stored| {
        stored.event.event_type.as_str() == EventType::VERIFICATION_APPARATUS_FROZEN
    })?;
    let hash = serde_json::from_value::<agent_protocol::apparatus::ApparatusManifest>(
        stored.event.payload.clone(),
    )
    .ok()
    .map(|manifest| manifest.hash);
    Some(FrozenApparatus { hash, seq: stored.seq })
}

/// What the gate needs to know about a frozen apparatus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenApparatus {
    /// The digest, or `None` when the freeze event could not be read.
    pub hash: Option<String>,
    /// Where it was frozen, for a reader following a refusal back.
    pub seq: EventSeq,
}

/// The latest evidence-bearing event in the session, if any.
pub fn latest_evidence(events: &[StoredEvent]) -> Option<Evidence> {
    let (index, stored) = events.iter().enumerate().rfind(|(_, stored)| is_evidence(stored))?;
    let revision = revision_up_to(events, index);
    Some(match stored.event.event_type.as_str() {
        EventType::COMPLETION_CHECKED => evidence_from_check(stored, revision),
        _ => evidence_from_recorded(stored, revision),
    })
}

/// The gate's verdict on this session.
pub fn gate(events: &[StoredEvent]) -> ClaimVerdict {
    let Some(evidence) = latest_evidence(events) else {
        return ClaimVerdict::Missing;
    };
    let revision = task_revision(events);
    if evidence.status == EvidenceStatus::Error || evidence.status == EvidenceStatus::Unknown {
        return ClaimVerdict::VerifierError { evidence };
    }
    if evidence.kind == EvidenceKind::Unknown {
        // A check whose kind this build cannot read may have been anything. It
        // is not a reason to claim completion, and calling it a run failure
        // would be a different lie.
        return ClaimVerdict::Failed { evidence };
    }
    // Integrity is asked before freshness: a session that froze its apparatus
    // and is now being graded against a different one has a problem a revision
    // comparison cannot see — the edit may have landed before the check ran.
    if let Some(frozen) = frozen_apparatus(events) {
        match frozen.hash.as_deref() {
            Some(hash) if evidence.input_manifest_hash.as_deref() == Some(hash) => {}
            _ => {
                let found = evidence.input_manifest_hash.clone();
                return ClaimVerdict::IntegrityChanged {
                    evidence,
                    expected: frozen.hash.unwrap_or_default(),
                    found,
                };
            }
        }
    }
    if evidence.task_revision != revision {
        return ClaimVerdict::Stale { evidence, revision };
    }
    if evidence.kind == EvidenceKind::CompletionCriterion
        && let Some(operator) = evidence.target.as_deref().and_then(masking_operator)
    {
        return ClaimVerdict::ExitMasked { evidence, operator };
    }
    if evidence.status != EvidenceStatus::Passed {
        return ClaimVerdict::Failed { evidence };
    }
    ClaimVerdict::Accepted(evidence)
}

/// Adapt a `completion/checked` event to an evidence record.
///
/// The criterion path predates the ledger and records a status, a target and a
/// step count, not an exit code: the gateway reports "the call completed", not
/// "the program exited zero". Keeping that absence visible is why `exit_code`
/// stays `None` here rather than being filled with a plausible zero.
fn evidence_from_check(stored: &StoredEvent, revision: u64) -> Evidence {
    let payload = &stored.event.payload;
    let status = match payload.get("status").and_then(Value::as_str) {
        Some("completed") => EvidenceStatus::Passed,
        Some("incomplete") => EvidenceStatus::Failed,
        Some("error") => EvidenceStatus::Error,
        _ => EvidenceStatus::Unknown,
    };
    Evidence {
        check_id: format!("completion/{}", stored.seq),
        task_revision: revision,
        kind: EvidenceKind::CompletionCriterion,
        status,
        target: payload.get("target").and_then(Value::as_str).map(str::to_string),
        program: None,
        args: Vec::new(),
        cwd: None,
        exit_code: None,
        stdout_sha256: None,
        stderr_sha256: None,
        tests_passed: None,
        artifact_refs: Vec::new(),
        started_at: None,
        finished_at: Some(stored.recorded_at),
        environment_hash: None,
        verifier_identity: "completion_criterion".to_string(),
        verifier_process: None,
        verifier_model: None,
        verifier_context_hash: None,
        input_manifest_hash: None,
        authorization_source: "profile.completion".to_string(),
    }
}

/// Read a `verification/recorded` payload.
///
/// The payload *is* the evidence object. A payload this build cannot parse is
/// recorded as an error rather than skipped: a check whose record is unreadable
/// is not a check anyone may lean on, and skipping it would let the next-oldest
/// pass stand in its place.
fn evidence_from_recorded(stored: &StoredEvent, revision: u64) -> Evidence {
    match serde_json::from_value::<Evidence>(stored.event.payload.clone()) {
        Ok(mut evidence) => {
            if evidence.check_id.is_empty() {
                evidence.check_id = format!("verification/{}", stored.seq);
            }
            evidence
        }
        Err(error) => {
            let mut evidence = Evidence::error(
                format!("verification/{}", stored.seq),
                revision,
                "unreadable".into(),
            );
            evidence.target = Some(error.to_string());
            evidence
        }
    }
}

fn is_evidence(stored: &StoredEvent) -> bool {
    matches!(
        stored.event.event_type.as_str(),
        EventType::COMPLETION_CHECKED | EventType::VERIFICATION_RECORDED
    )
}

fn is_change(stored: &StoredEvent) -> bool {
    matches!(
        stored.event.event_type.as_str(),
        EventType::TOOL_STARTED
            | EventType::TOOL_COMPLETED
            | EventType::TOOL_FAILED
            | EventType::TOOL_CANCELLED
            | EventType::TOOL_INTERRUPTED
            | EventType::TOOL_OUTCOME_UNKNOWN
    )
}

/// The number of changes that happened strictly after the event at `index`.
fn revision_up_to(events: &[StoredEvent], index: usize) -> u64 {
    events[..index].iter().filter(|stored| is_change(stored)).count() as u64
}

/// The sequence of the latest evidence event, used by [`is_current`].
fn latest_evidence_seq(events: &[StoredEvent]) -> EventSeq {
    events
        .iter()
        .filter(|stored| is_evidence(stored))
        .map(|stored| stored.seq)
        .next_back()
        .unwrap_or(EventSeq::ZERO)
}

fn revision_before(events: &[StoredEvent], seq: EventSeq) -> u64 {
    events.iter().filter(|stored| stored.seq < seq && is_change(stored)).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventId, RunId, SchemaVersion, SessionId, TurnId};
    use chrono::Utc;

    fn stored(seq: u64, event_type: &str, payload: Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::current(),
                session_id: SessionId::from_string("s"),
                run_id: Some(RunId::from_string("r")),
                turn_id: Some(TurnId::from_string("t")),
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

    fn check(seq: u64, status: &str) -> StoredEvent {
        stored(seq, EventType::COMPLETION_CHECKED, serde_json::json!({ "status": status }))
    }

    fn tool(seq: u64) -> StoredEvent {
        stored(seq, EventType::TOOL_COMPLETED, serde_json::json!({ "tool_name": "fs.write" }))
    }

    fn recorded(seq: u64, status: &str) -> StoredEvent {
        recorded_against(seq, status, None)
    }

    fn recorded_against(seq: u64, status: &str, apparatus: Option<&str>) -> StoredEvent {
        let mut payload = serde_json::json!({
            "check_id": "v1",
            "task_revision": 0,
            "kind": "verifier",
            "status": status,
            "program": "cargo",
            "args": ["test"],
            "exit_code": 0,
            "verifier_identity": "verifier",
            "authorization_source": "operator",
        });
        if let Some(hash) = apparatus {
            payload["input_manifest_hash"] = serde_json::json!(hash);
        }
        stored(seq, EventType::VERIFICATION_RECORDED, payload)
    }

    fn frozen(seq: u64, hash: &str) -> StoredEvent {
        stored(
            seq,
            EventType::VERIFICATION_APPARATUS_FROZEN,
            serde_json::json!({
                "spec": { "include": ["tests/**"] },
                "entries": [],
                "hash": hash,
            }),
        )
    }

    #[test]
    fn a_session_with_no_check_has_no_evidence() {
        assert_eq!(gate(&[]), ClaimVerdict::Missing);
        assert_eq!(gate(&[tool(0)]), ClaimVerdict::Missing);
    }

    #[test]
    fn a_passing_check_with_nothing_after_it_is_accepted() {
        let events = vec![tool(0), check(1, "completed")];
        let verdict = gate(&events);
        assert!(verdict.is_accepted(), "{verdict:?}");
        assert_eq!(verdict.evidence().unwrap().check_id, "completion/1");
        assert_eq!(verdict.evidence().unwrap().task_revision, 1, "it covered the work before it");
    }

    #[test]
    fn a_change_after_the_check_makes_it_stale() {
        let events = vec![check(0, "completed"), tool(1)];
        match gate(&events) {
            ClaimVerdict::Stale { evidence, revision } => {
                assert_eq!(evidence.task_revision, 0);
                assert_eq!(revision, 1);
            }
            other => panic!("expected stale, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_call_is_not_a_change() {
        let events = vec![
            check(0, "completed"),
            stored(1, EventType::TOOL_NOT_STARTED, serde_json::json!({ "tool_name": "fs.write" })),
        ];
        assert!(gate(&events).is_accepted());
        assert_eq!(task_revision(&events), 0);
    }

    #[test]
    fn a_check_that_could_not_run_is_never_accepted() {
        let events = vec![check(0, "error")];
        assert!(matches!(gate(&events), ClaimVerdict::VerifierError { .. }));
    }

    #[test]
    fn a_check_that_said_no_is_not_a_pass() {
        assert!(matches!(gate(&[check(0, "incomplete")]), ClaimVerdict::Failed { .. }));
        assert!(matches!(gate(&[check(0, "something_new")]), ClaimVerdict::VerifierError { .. }));
    }

    #[test]
    fn a_criterion_whose_pipeline_ends_the_status_is_refused() {
        let mut event = check(0, "completed");
        event.event.payload["target"] = serde_json::json!("node --test | tail -5");
        match gate(&[event]) {
            ClaimVerdict::ExitMasked { operator, .. } => assert_eq!(operator, "| tail"),
            other => panic!("expected exit masked, got {other:?}"),
        }
    }

    #[test]
    fn a_pass_against_the_frozen_apparatus_is_accepted() {
        let events = [frozen(0, "aaa"), recorded_against(1, "passed", Some("aaa"))];
        assert_eq!(gate(&events).as_str(), "accepted");
    }

    #[test]
    fn a_pass_against_an_apparatus_that_moved_is_refused() {
        // The check is fresh by revision and says it passed — but it was graded
        // against different files than the session froze. That is the edit that
        // makes a suite pass without the work being right.
        let events = [frozen(0, "aaa"), recorded_against(1, "passed", Some("bbb"))];
        let verdict = gate(&events);
        assert_eq!(verdict.as_str(), "integrity_changed");
        match verdict {
            ClaimVerdict::IntegrityChanged { expected, found, .. } => {
                assert_eq!(expected, "aaa");
                assert_eq!(found.as_deref(), Some("bbb"));
            }
            other => panic!("expected integrity_changed, got {other:?}"),
        }
    }

    #[test]
    fn a_criterion_that_states_no_apparatus_cannot_stand_once_one_is_frozen() {
        // A criterion still says "a command exited zero"; it cannot say what it
        // was graded against. Once a session has frozen an apparatus, a pass
        // with no manifest is a pass nobody can check.
        let events = [frozen(0, "aaa"), recorded_against(1, "passed", None)];
        let verdict = gate(&events);
        assert_eq!(verdict.as_str(), "integrity_changed");
        assert!(matches!(verdict, ClaimVerdict::IntegrityChanged { found: None, .. }));
    }

    #[test]
    fn with_nothing_frozen_the_apparatus_is_not_asked_about() {
        // Sessions that never declared an apparatus keep the A5 answer.
        let events = [recorded_against(0, "passed", None)];
        assert_eq!(gate(&events).as_str(), "accepted");
    }

    #[test]
    fn a_freeze_this_build_cannot_read_matches_nothing() {
        let broken = stored(0, EventType::VERIFICATION_APPARATUS_FROZEN, serde_json::json!("junk"));
        let events = [broken, recorded_against(1, "passed", None)];
        assert_eq!(gate(&events).as_str(), "integrity_changed");
    }

    #[test]
    fn a_structured_record_is_read_and_outranks_an_older_criterion() {
        let events = vec![check(0, "incomplete"), recorded(1, "passed")];
        let verdict = gate(&events);
        assert!(verdict.is_accepted(), "{verdict:?}");
        let evidence = verdict.evidence().unwrap();
        assert_eq!(evidence.kind, EvidenceKind::Verifier);
        assert_eq!(evidence.exit_code, Some(0));
    }

    #[test]
    fn a_record_this_build_cannot_read_is_an_errored_check() {
        let events =
            vec![stored(0, EventType::VERIFICATION_RECORDED, serde_json::json!({ "nope": 1 }))];
        assert!(matches!(gate(&events), ClaimVerdict::VerifierError { .. }));
    }

    #[test]
    fn the_revision_is_the_count_of_events_that_can_change_the_world() {
        let events = vec![tool(0), tool(1), check(2, "completed")];
        assert_eq!(task_revision(&events), 2);
        assert!(is_current(&events, &latest_evidence(&events).unwrap()));
    }
}
