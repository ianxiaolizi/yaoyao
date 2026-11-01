//! Running a structured check and recording what it found.
//!
//! The gate in [`crate::completion`] answers "may this evidence support a
//! completion?"; this module is the producer that gives it evidence worth
//! answering about. A check is a program and its arguments
//! ([`CheckSpec`](agent_protocol::verification::CheckSpec)); the running is a
//! port ([`CommandVerifier`]); what comes back is judged by
//! [`CheckRun::judge`] and written down as `verification/recorded`.
//!
//! Three rules shape the record:
//!
//! * **The revision is read after the check, not before.** The evidence claims
//!   "this check covered the work as it stood at revision N", so N has to be the
//!   revision at the moment the record is made. Reading it first would let a
//!   change that landed while the check ran be counted as covered — the exact
//!   staleness the gate exists to catch.
//! * **The event carries the evidence object itself.** A reader that cannot
//!   parse it records an error rather than skipping it
//!   ([`crate::completion`]), so there is no second place the facts live.
//! * **A check that could not run still gets a record.** Silence would let an
//!   older pass stand in its place, which is indistinguishable, to the gate,
//!   from a fresh verification.

use agent_ports::event_store::EventStore;
use agent_ports::verifier::CommandVerifier;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::evidence::{Evidence, EvidenceKind, EvidenceStatus};
use agent_protocol::ids::{EventId, SchemaVersion, SessionId};
use agent_protocol::verification::{CheckRun, CheckSpec};
use chrono::{DateTime, Utc};

use crate::completion;

/// What one recorded check says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCheck {
    pub evidence: Evidence,
    /// The judge's status. Repeated here so a caller does not have to read the
    /// evidence back to find out what happened.
    pub status: EvidenceStatus,
    /// Why the check did not pass; empty when it did.
    pub reason: String,
}

impl RecordedCheck {
    pub fn passed(&self) -> bool {
        self.status == EvidenceStatus::Passed
    }
}

/// Run `spec`, judge it, and append `verification/recorded`.
///
/// `now` is the caller's clock — the kernel does not read one to decide anything
/// (STYLE §1) — and is used only for the envelope's `created_at`.
///
/// The append goes through [`EventStore::append_at_tail`] rather than a sequence
/// the caller read, because a check is often run while another process is
/// driving the session: there is no decision built from the number, so a stale
/// number is not a conflict to re-decide, only a number nobody should have
/// computed.
pub async fn verify_and_record(
    store: &dyn EventStore,
    verifier: &dyn CommandVerifier,
    session_id: &SessionId,
    spec: &CheckSpec,
    authorization_source: &str,
    now: DateTime<Utc>,
) -> Result<RecordedCheck, agent_ports::event_store::EventStoreError> {
    // The check starts when the caller asked for it: `now` is this call's start
    // instant as well as the record's `created_at`. The adapter measures only
    // how long the run took (STYLE §1 — no adapter reads a wall clock).
    let run = verifier.verify(spec, now);
    let judgement = run.judge(&spec.expect);

    // Read after the check: the revision this evidence covers is the revision
    // the log has when the record is written.
    let events =
        store.read_stream(session_id, agent_protocol::ids::EventSeq::ZERO, usize::MAX).await?;
    let revision = completion::task_revision(&events);

    let event_id = EventId::new();
    let evidence =
        evidence_from(&run, judgement.status, spec, revision, authorization_source, &event_id);
    let mut payload = serde_json::to_value(&evidence)?;
    // The reason is for the person reading the log. It is not part of
    // `Evidence` because it is a sentence about this run, not a fact that can be
    // compared between runs.
    if !judgement.reason.is_empty() {
        payload["reason"] = serde_json::json!(judgement.reason);
    }
    let draft = EventDraft {
        event_id: event_id.clone(),
        event_type: EventType::new(EventType::VERIFICATION_RECORDED),
        schema_version: SchemaVersion::current(),
        session_id: session_id.clone(),
        // A check is a session-level fact: it is about the work, not about the
        // turn that happened to be running when someone asked for it.
        run_id: None,
        turn_id: None,
        step_id: None,
        actor: Actor::System,
        created_at: now,
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: serde_json::json!({}),
        ignorable: false,
    };
    store.append_at_tail(session_id, vec![draft]).await?;

    Ok(RecordedCheck { evidence, status: judgement.status, reason: judgement.reason })
}

fn evidence_from(
    run: &CheckRun,
    status: EvidenceStatus,
    spec: &CheckSpec,
    revision: u64,
    authorization_source: &str,
    event_id: &EventId,
) -> Evidence {
    Evidence {
        // Derived from the event id so two checks in one log never share an id
        // and replaying the record keeps the same one.
        check_id: format!("verifier/{}", event_id.as_str()),
        task_revision: revision,
        kind: EvidenceKind::Verifier,
        status,
        target: Some(spec.command_line()),
        program: Some(spec.program.clone()),
        args: spec.args.clone(),
        cwd: spec.cwd.clone(),
        exit_code: run.exit_code,
        stdout_sha256: Some(run.stdout_sha256.clone()),
        stderr_sha256: Some(run.stderr_sha256.clone()),
        tests_passed: run.tests_passed,
        artifact_refs: Vec::new(),
        started_at: Some(run.started_at),
        finished_at: Some(run.finished_at),
        environment_hash: Some(run.environment_hash.clone()),
        verifier_identity: "verifier".to_string(),
        verifier_process: Some(spec.program.clone()),
        verifier_model: None,
        verifier_context_hash: None,
        // The apparatus manifest arrives with A7; until then no check claims to
        // have one, and `None` is the honest value (CONTRACT §7 #171).
        input_manifest_hash: None,
        authorization_source: authorization_source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::event_store::EventStoreError;
    use agent_protocol::ids::EventSeq;
    use agent_protocol::verification::Expectation;
    use agent_testkit::FakeEventStore;
    use std::sync::Arc;

    fn spec() -> CheckSpec {
        CheckSpec {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
            cwd: None,
            timeout_ms: 1_000,
            expect: Expectation::default(),
        }
    }

    fn run(exit: Option<i32>) -> CheckRun {
        CheckRun {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            exit_code: exit,
            timed_out: false,
            stdout_sha256: "aa".to_string(),
            stdout_bytes: 1,
            stderr_sha256: "bb".to_string(),
            stderr_bytes: 0,
            contains_expected: None,
            tests_passed: Some(3),
            environment_hash: "cc".to_string(),
            failure: None,
        }
    }

    /// A verifier that answers a fixed run, recording whether it was asked.
    struct Fixed(Option<i32>);

    impl CommandVerifier for Fixed {
        fn verify(&self, _spec: &CheckSpec, _started_at: chrono::DateTime<Utc>) -> CheckRun {
            run(self.0)
        }
    }

    async fn session(events: Vec<EventDraft>) -> (Arc<FakeEventStore>, SessionId) {
        let store = Arc::new(FakeEventStore::new());
        let session = SessionId::from_string("checked");
        if !events.is_empty() {
            store.append(&session, EventSeq::ZERO, events).await.unwrap();
        }
        (store, session)
    }

    fn tool_event() -> EventDraft {
        EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(EventType::TOOL_COMPLETED),
            schema_version: SchemaVersion::current(),
            session_id: SessionId::from_string("checked"),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::Agent,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::json!({ "tool_name": "fs.write" }),
            metadata: serde_json::json!({}),
            ignorable: false,
        }
    }

    #[tokio::test]
    async fn a_passing_check_is_recorded_as_evidence() {
        let (store, session) = session(vec![]).await;
        let recorded = verify_and_record(
            store.as_ref(),
            &Fixed(Some(0)),
            &session,
            &spec(),
            "operator",
            Utc::now(),
        )
        .await
        .unwrap();

        assert!(recorded.passed());
        assert_eq!(recorded.evidence.kind, EvidenceKind::Verifier);
        assert_eq!(recorded.evidence.exit_code, Some(0));
        assert_eq!(recorded.evidence.tests_passed, Some(3));

        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        let last = events.last().expect("the record was appended");
        assert_eq!(last.event.event_type.as_str(), EventType::VERIFICATION_RECORDED);
        // The payload is the evidence object, so the gate can read it back.
        let back: Evidence = serde_json::from_value(last.event.payload.clone()).unwrap();
        assert_eq!(back.program.as_deref(), Some("cargo"));
        assert_eq!(back.check_id, recorded.evidence.check_id);
    }

    #[tokio::test]
    async fn the_revision_covers_the_work_up_to_the_record() {
        let (store, session) = session(vec![tool_event(), tool_event()]).await;
        let recorded = verify_and_record(
            store.as_ref(),
            &Fixed(Some(0)),
            &session,
            &spec(),
            "operator",
            Utc::now(),
        )
        .await
        .unwrap();

        assert_eq!(recorded.evidence.task_revision, 2, "both tool events are on the table");

        // And the gate reads the same number back, so the evidence is current.
        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        assert_eq!(
            completion::gate(&events),
            agent_protocol::evidence::ClaimVerdict::Accepted(recorded.evidence)
        );
    }

    #[tokio::test]
    async fn a_check_that_could_not_start_is_recorded_as_an_error() {
        struct Refuses;
        impl CommandVerifier for Refuses {
            fn verify(&self, _spec: &CheckSpec, _started_at: chrono::DateTime<Utc>) -> CheckRun {
                let mut run = run(None);
                run.failure = Some("the program is not on PATH".to_string());
                run
            }
        }

        let (store, session) = session(vec![]).await;
        let recorded =
            verify_and_record(store.as_ref(), &Refuses, &session, &spec(), "operator", Utc::now())
                .await
                .unwrap();

        assert_eq!(recorded.status, EvidenceStatus::Error);
        assert!(!recorded.passed());
        assert!(recorded.reason.contains("not on PATH"), "{}", recorded.reason);
        assert_eq!(recorded.evidence.status, EvidenceStatus::Error);

        // The record is durable, so the gate sees "a check that could not run"
        // rather than "no check" — those are different facts.
        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        assert!(matches!(
            completion::gate(&events),
            agent_protocol::evidence::ClaimVerdict::VerifierError { .. }
        ));
    }

    #[tokio::test]
    async fn a_store_that_refuses_the_append_is_reported() {
        struct Refusing;
        #[async_trait::async_trait]
        impl EventStore for Refusing {
            async fn append(
                &self,
                session_id: &SessionId,
                _expected_next_seq: EventSeq,
                _drafts: Vec<EventDraft>,
            ) -> Result<agent_ports::event_store::AppendReceipt, EventStoreError> {
                Err(EventStoreError::ConcurrentWriter(session_id.clone()))
            }
            async fn lease(
                &self,
                session_id: &SessionId,
            ) -> Result<Box<dyn agent_ports::event_store::SessionLease>, EventStoreError>
            {
                Err(EventStoreError::ConcurrentWriter(session_id.clone()))
            }
            async fn driver_lease(
                &self,
                session_id: &SessionId,
            ) -> Result<Box<dyn agent_ports::event_store::SessionLease>, EventStoreError>
            {
                Err(EventStoreError::ConcurrentWriter(session_id.clone()))
            }
            async fn flush(
                &self,
                session_id: &SessionId,
            ) -> Result<agent_ports::event_store::FlushReceipt, EventStoreError> {
                Err(EventStoreError::ConcurrentWriter(session_id.clone()))
            }
            async fn read_stream(
                &self,
                _session_id: &SessionId,
                _from_seq: EventSeq,
                _limit: usize,
            ) -> Result<Vec<agent_protocol::events::StoredEvent>, EventStoreError> {
                Ok(Vec::new())
            }
            async fn subscribe(
                &self,
                _session_id: &SessionId,
                _from_seq: EventSeq,
            ) -> Result<Box<dyn agent_ports::event_store::EventSubscription>, EventStoreError>
            {
                Err(EventStoreError::ConcurrentWriter(SessionId::from_string("checked")))
            }
        }

        let session = SessionId::from_string("checked");
        let error = verify_and_record(
            &Refusing,
            &Fixed(Some(0)),
            &session,
            &spec(),
            "operator",
            Utc::now(),
        )
        .await
        .expect_err("a refused append is not a silent success");
        assert!(matches!(error, EventStoreError::ConcurrentWriter(_)));
    }
}
