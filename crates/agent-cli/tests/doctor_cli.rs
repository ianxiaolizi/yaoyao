//! CLI doctor command tests.

use agent_cli::{Verdict, run_doctor};
use agent_kernel::doctor::KernelDoctor;
use agent_protocol::ids::SchemaVersion;
use agent_testkit::FakeDoctorCheck;

fn sv() -> SchemaVersion {
    SchemaVersion::from_string("0.1.0")
}

#[tokio::test]
async fn doctor_human_readable_contains_check_summary() {
    let doctor = KernelDoctor::new(vec![Box::new(FakeDoctorCheck::passing("build"))]).unwrap();

    let outcome = run_doctor(&doctor, sv(), false).await.unwrap();
    match outcome.output {
        agent_cli::CliOutput::Text(t) => {
            assert!(t.contains("✓"));
            assert!(t.contains("healthy"));
        }
        agent_cli::CliOutput::Json(_) => panic!("expected text output"),
    }
    assert_eq!(outcome.verdict, Verdict::Answered, "a healthy build is an answer");
}

#[tokio::test]
async fn doctor_json_contains_schema_version_and_status() {
    let doctor = KernelDoctor::new(vec![Box::new(FakeDoctorCheck::passing("build"))]).unwrap();

    let outcome = run_doctor(&doctor, sv(), true).await.unwrap();
    match outcome.output {
        agent_cli::CliOutput::Json(j) => {
            assert!(j.as_str().starts_with("{\n"), "one pretty object: {j}");
            let parsed: serde_json::Value = serde_json::from_str(j.as_str()).expect("valid JSON");
            assert_eq!(parsed["schema_version"], "0.1.0");
            assert_eq!(parsed["aggregate_status"], "passed");
            assert!(parsed["checks"].is_array());
        }
        agent_cli::CliOutput::Text(_) => panic!("expected JSON output"),
    }
}

/// An unhealthy build is an answer, and the verdict says which answer it is.
///
/// §3 C3.4 and 裁决 #5: the doctor ran to the end and what it found was bad, so
/// the status is the one meaning "there is an answer, and it is no" — `1` in
/// `apps/agent`. `2` was rejected for the opposite reason: the doctor *did* run.
/// What can be pinned here is the half that lives in this crate, the verdict;
/// `the_verdict_alphabet_is_three_distinct_statuses` in the app pins the number.
///
/// The binary now reaches this verdict through real checks (YA-45, 阶段 5.1),
/// and the end-to-end class tests live in `apps/agent`. This one stays at the
/// crate's own seam because the seam is what it asserts: a failing message in
/// the report becomes `AnsweredNo`, in both output shapes.
#[tokio::test]
async fn doctor_reports_a_failing_check_as_a_refusal() {
    let doctor = KernelDoctor::new(vec![
        Box::new(FakeDoctorCheck::passing("build")),
        Box::new(FakeDoctorCheck::failing("store", "the log is unreadable")),
    ])
    .unwrap();

    for json in [false, true] {
        let outcome = run_doctor(&doctor, sv(), json).await.unwrap();
        assert_eq!(
            outcome.verdict,
            Verdict::AnsweredNo,
            "an unhealthy build is a verdict, not a success (json={json})"
        );
        // The answer is still printed in whichever shape was asked for; the
        // status says the same thing, it does not replace it.
        let printed = match outcome.output {
            agent_cli::CliOutput::Text(t) => t,
            agent_cli::CliOutput::Json(j) => j.as_str().to_string(),
        };
        assert!(printed.contains("store"), "the failing check is named: {printed}");
    }
}
