//! The `verify` command: run one structured check and report what it found.
//!
//! The check is written down either way — a check that could not start is
//! reported the same way a failing one is, because the interesting question a
//! reader has is "what does the record say now?", and a command that only
//! records its successes leaves the previous pass standing.
//!
//! The exit status is the check's own answer: `0` passed, `1` ran and did not
//! pass, `2` could not run. That is the same alphabet every other command uses
//! (§3 C3.5), applied to the one subject this command has.

use agent_kernel::verifier::verify_and_record;
use agent_ports::event_store::EventStore;
use agent_ports::verifier::CommandVerifier;
use agent_protocol::ids::SessionId;
use agent_protocol::verification::CheckSpec;
use chrono::{DateTime, Utc};

use crate::{CliOutcome, CliOutput, Verdict, json_output};

/// Run `spec`, record the outcome, and render it.
pub async fn run_verify(
    store: &dyn EventStore,
    verifier: &dyn CommandVerifier,
    session_id: &SessionId,
    spec: &CheckSpec,
    authorization_source: &str,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    let recorded = verify_and_record(store, verifier, session_id, spec, authorization_source, now)
        .await
        .map_err(|e| format!("could not record the check: {e}"))?;
    let evidence = &recorded.evidence;
    let status = recorded.status.as_str();

    let verdict = match recorded.status {
        agent_protocol::evidence::EvidenceStatus::Passed => Verdict::Answered,
        agent_protocol::evidence::EvidenceStatus::Failed => Verdict::AnsweredNo,
        // Error, and any status a newer build wrote that this one cannot read:
        // nothing about the task follows, so there is no answer to give.
        _ => Verdict::DidNotRun,
    };

    let output = if json {
        let value = serde_json::json!({
            "session": session_id.as_str(),
            "check_id": evidence.check_id,
            "target": evidence.target,
            "program": evidence.program,
            "args": evidence.args,
            "status": status,
            "exit_code": evidence.exit_code,
            "stdout_sha256": evidence.stdout_sha256,
            "stderr_sha256": evidence.stderr_sha256,
            "tests_passed": evidence.tests_passed,
            "task_revision": evidence.task_revision,
            "environment_hash": evidence.environment_hash,
            "apparatus_hash": evidence.input_manifest_hash,
            "apparatus_files": recorded
                .apparatus
                .as_ref()
                .map(|manifest| manifest.entries.len()),
            "reason": recorded.reason,
        });
        CliOutput::Json(json_output(&value)?)
    } else {
        CliOutput::Text(render(&recorded, spec))
    };
    Ok(CliOutcome { output, verdict })
}

fn render(recorded: &agent_kernel::verifier::RecordedCheck, spec: &CheckSpec) -> String {
    let evidence = &recorded.evidence;
    let digest = evidence.stdout_sha256.as_deref().map(short).unwrap_or_else(|| "none".to_string());
    let mut lines = vec![
        format!("check:    {}", spec.command_line()),
        format!("status:   {}", recorded.status.as_str()),
        format!(
            "exit:     {}",
            evidence.exit_code.map(|code| code.to_string()).unwrap_or_else(|| "none".to_string())
        ),
        format!("stdout:   sha256 {digest}"),
    ];
    if let Some(tests) = evidence.tests_passed {
        lines.push(format!("tests:    {tests} passed"));
    }
    lines.push(format!("revision: {}", evidence.task_revision));
    if let Some(apparatus) = &recorded.apparatus {
        lines.push(format!(
            "apparatus: sha256 {} ({} files)",
            short(&apparatus.hash),
            apparatus.entries.len()
        ));
    }
    if !recorded.reason.is_empty() {
        lines.push(format!("reason:   {}", recorded.reason));
    }
    lines.join(
        "
",
    )
}

/// The first twelve hex characters of a digest, which is enough to compare two
/// by eye and short enough to read in a terminal.
fn short(digest: &str) -> String {
    digest.chars().take(12).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::apparatus::{ApparatusManifest, ApparatusSpec};
    use agent_protocol::evidence::EvidenceStatus;
    use agent_protocol::ids::EventSeq;
    use agent_protocol::verification::{CheckRun, Expectation};
    use agent_testkit::FakeEventStore;
    use std::sync::Arc;

    /// A verifier the command can be tested with: no process is started, and
    /// the run it reports is the caller's.
    struct FixedVerifier(CheckRun);

    impl CommandVerifier for FixedVerifier {
        fn verify(&self, _spec: &CheckSpec, _started_at: DateTime<Utc>) -> CheckRun {
            self.0.clone()
        }

        fn manifest(&self, _spec: &ApparatusSpec) -> Result<ApparatusManifest, String> {
            Err("this test verifier reads no apparatus".to_string())
        }
    }

    fn spec() -> CheckSpec {
        CheckSpec {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
            cwd: None,
            timeout_ms: 1_000,
            apparatus: ApparatusSpec::default(),
            expect: Expectation::default(),
        }
    }

    fn running(exit: i32) -> CheckRun {
        CheckRun {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            exit_code: Some(exit),
            timed_out: false,
            stdout_sha256: "a".repeat(64),
            stdout_bytes: 12,
            stderr_sha256: "b".repeat(64),
            stderr_bytes: 0,
            contains_expected: None,
            tests_passed: Some(7),
            environment_hash: "c".repeat(16),
            failure: None,
        }
    }

    async fn store_and_session() -> (Arc<FakeEventStore>, SessionId) {
        (Arc::new(FakeEventStore::new()), SessionId::from_string("verified"))
    }

    #[tokio::test]
    async fn a_passing_check_answers_yes_and_is_written_down() {
        let (store, session) = store_and_session().await;
        let outcome = run_verify(
            store.as_ref(),
            &FixedVerifier(running(0)),
            &session,
            &spec(),
            "operator",
            Utc::now(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome.verdict, Verdict::Answered);
        let CliOutput::Text(text) = &outcome.output else { panic!("text was asked for") };
        assert!(text.contains("status:   passed"), "{text}");
        assert!(text.contains("tests:    7 passed"), "{text}");

        let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event.event_type.as_str(),
            agent_protocol::events::EventType::VERIFICATION_RECORDED
        );
    }

    #[tokio::test]
    async fn a_failing_check_still_exits_as_an_answer() {
        let (store, session) = store_and_session().await;
        let outcome = run_verify(
            store.as_ref(),
            &FixedVerifier(running(3)),
            &session,
            &spec(),
            "operator",
            Utc::now(),
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome.verdict, Verdict::AnsweredNo, "it ran; the answer was no");
        let CliOutput::Json(json) = &outcome.output else { panic!("json was asked for") };
        let value: serde_json::Value = serde_json::from_str(json.as_str()).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["exit_code"], 3);
        assert_eq!(value["program"], "cargo");
    }

    #[tokio::test]
    async fn a_check_that_could_not_run_is_not_an_answer() {
        let (store, session) = store_and_session().await;
        let mut run = running(0);
        run.exit_code = None;
        run.failure = Some("no such program".to_string());
        let outcome = run_verify(
            store.as_ref(),
            &FixedVerifier(run),
            &session,
            &spec(),
            "operator",
            Utc::now(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome.verdict, Verdict::DidNotRun);
        let CliOutput::Text(text) = &outcome.output else { panic!("text was asked for") };
        assert!(text.contains("no such program"), "{text}");
        assert_eq!(running(0).judge(&Expectation::default()).status, EvidenceStatus::Passed);
    }
}
