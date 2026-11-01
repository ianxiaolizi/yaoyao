//! The security-review family, over the existing broker.
//!
//! Same shape as the permission classifier and for the same reasons: the pack
//! and the bounded input come from `agent_protocol::security_review`, the call
//! goes through [`DecisionBroker`] (timeout, abstention, failure and validation
//! behave as they do for every other family), and the answer is read back as the
//! structured contract that module defines.
//!
//! Three things this module decides:
//!
//! - **What a report's evidence status is.** A pass that clears the contract's
//!   own bar is `passed`; a refusal is `failed`; a review that could not run, or
//!   whose answer is not a verdict, is `error`. The gate reads the status, never
//!   the prose, so this mapping is the whole interface between a model's opinion
//!   and a blocked delivery.
//! - **The provenance the backend cannot know.** Route, prompt hash, revision
//!   and input digest are written by this adapter, not taken from the answer: a
//!   backend that claimed a revision never saw it.
//! - **No retry.** Like the classifier: a second review of the same work is a
//!   second chance for a flaky backend to clear it.

use std::sync::Arc;
use std::time::Duration;

use agent_ports::decision::{DecisionBackendOutcome, DecisionValue, FastDecisionProvider};
use agent_ports::security_reviewer::{
    SecurityReviewInput, SecurityReviewReport, SecurityReviewer, SecurityVerdict,
};
use agent_protocol::evidence::{Evidence, EvidenceKind, EvidenceStatus};
use agent_protocol::ids::EventId;
use agent_protocol::security_review::{self as pack, QUESTION_ID, SecurityVerdictKind};

use crate::broker::DecisionBroker;
use crate::rule::RuleDecisionProvider;

/// How long one review gets.
///
/// Longer than a classification (600 ms) because a review reads a whole task's
/// actions rather than one call, and shorter than a person would tolerate
/// staring at a finished turn. Past this the review is an `error` and the
/// delivery is blocked — the same outcome an unreadable answer gets.
pub const REVIEW_TIMEOUT: Duration = Duration::from_millis(1500);

/// Build the reviewer over `backend`, or `None` when there is none.
///
/// A build with no sidecar has no independent reviewer, and that is recorded as
/// "nobody asked": a high-risk task then has no security evidence and the
/// completion gate refuses it, rather than treating silence as a pass.
pub fn reviewer_for(
    backend: Option<Arc<dyn FastDecisionProvider>>,
) -> Option<Arc<dyn SecurityReviewer>> {
    backend.map(|backend| {
        Arc::new(BrokerReviewer {
            broker: DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty()))
                .with_backend(backend)
                .with_timeout(REVIEW_TIMEOUT),
        }) as Arc<dyn SecurityReviewer>
    })
}

/// The reviewer: one broker, one pack, one deadline.
struct BrokerReviewer {
    broker: DecisionBroker,
}

#[async_trait::async_trait]
impl SecurityReviewer for BrokerReviewer {
    async fn review(&self, input: SecurityReviewInput) -> SecurityReviewReport {
        let input = input.bounded();
        let pack = pack::pack();
        let state = match pack::state(&input) {
            Ok(state) => state,
            Err(error) => {
                return report_of(
                    &input,
                    "",
                    EvidenceStatus::Error,
                    Some(error.kind().to_string()),
                );
            }
        };

        let decision = self.broker.decide(&pack, &state).await;
        if let Some(aborted) = &decision.aborted {
            return report_of(&input, "", EvidenceStatus::Error, Some(aborted.clone()));
        }

        let Some(chosen) = &decision.chosen else {
            let kind = decision
                .backends
                .iter()
                .find(|report| report.backend_id != "rule")
                .map(|report| match &report.outcome {
                    DecisionBackendOutcome::Failed { error_kind } => error_kind.clone(),
                    DecisionBackendOutcome::Abstained => "abstained".to_string(),
                    DecisionBackendOutcome::Answered(_) => "protocol".to_string(),
                })
                .unwrap_or_else(|| "unavailable".to_string());
            return report_of(&input, "", EvidenceStatus::Error, Some(kind));
        };

        let route = decision.chosen_backend.clone().unwrap_or_default();
        let value = match chosen.answer_for(QUESTION_ID).map(|answer| &answer.value) {
            Some(DecisionValue::Structured { value }) => value,
            // The pack validated one structured answer, so a different shape is
            // the layers drifting apart, not an opinion.
            _ => return report_of(&input, &route, EvidenceStatus::Error, Some("protocol".into())),
        };
        let answer = match pack::parse(value) {
            Ok(answer) => answer,
            Err(reason) => {
                return report_of(&input, &route, EvidenceStatus::Error, Some(reason));
            }
        };

        let verdict =
            SecurityVerdict::from_answer(answer, route.clone(), pack::prompt_hash(), &input);
        let status = match verdict.verdict {
            _ if verdict.accepts() => EvidenceStatus::Passed,
            SecurityVerdictKind::Pass => EvidenceStatus::Failed,
            SecurityVerdictKind::Fail => EvidenceStatus::Failed,
            SecurityVerdictKind::Unknown => EvidenceStatus::Error,
        };
        let failure = verdict.refusal();
        SecurityReviewReport {
            evidence: evidence(&input, &route, status),
            verdict: Some(verdict),
            failure_kind: failure,
        }
    }
}

/// A report for a review that produced no answer worth keeping.
fn report_of(
    input: &SecurityReviewInput,
    route: &str,
    status: EvidenceStatus,
    failure_kind: Option<String>,
) -> SecurityReviewReport {
    SecurityReviewReport { verdict: None, evidence: evidence(input, route, status), failure_kind }
}

/// The evidence one review leaves behind.
///
/// `verifier_identity` names the reviewer family and its route, and
/// `verifier_context_hash` is the digest of exactly what was handed over, so a
/// recorded verdict can be matched to an input without storing the input.
fn evidence(input: &SecurityReviewInput, route: &str, status: EvidenceStatus) -> Evidence {
    Evidence {
        check_id: format!("security/{}", EventId::new().as_str()),
        task_revision: input.task_revision,
        kind: EvidenceKind::SecurityReview,
        status,
        target: Some(format!("security_review/{route}")),
        program: None,
        args: Vec::new(),
        cwd: None,
        exit_code: None,
        stdout_sha256: None,
        stderr_sha256: None,
        tests_passed: None,
        artifact_refs: Vec::new(),
        started_at: None,
        finished_at: None,
        environment_hash: None,
        verifier_identity: format!("security_reviewer:{route}"),
        verifier_process: None,
        verifier_model: Some(route.to_string()),
        verifier_context_hash: Some(input.input_hash()),
        input_manifest_hash: None,
        authorization_source: "profile.security".to_string(),
    }
}

impl std::fmt::Debug for BrokerReviewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerReviewer").field("mode", &self.broker.mode()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::decision::{DecisionError, DecisionMode};
    use agent_protocol::security_review::{FindingSeverity, SecurityFinding};
    use agent_testkit::{FakeDecisionBehaviour, FakeDecisionProvider};
    use serde_json::json;

    fn input() -> SecurityReviewInput {
        SecurityReviewInput {
            session_id: "s".to_string(),
            task_id: "intent-1".to_string(),
            task_revision: 3,
            intent_summary: "clean the build directory".to_string(),
            highest_risk: agent_protocol::permission::RiskLevel::L3,
            action_digests: vec!["a".repeat(64)],
            change_manifest_hash: Some("h".to_string()),
            evidence_refs: vec!["verification/2".to_string()],
            untrusted_input_refs: Vec::new(),
            policy_version: "v1".to_string(),
        }
    }

    fn answer(verdict: &str, confidence: f64, findings: serde_json::Value) -> DecisionValue {
        DecisionValue::Structured {
            value: json!({ "verdict": verdict, "confidence": confidence, "findings": findings }),
        }
    }

    fn reviewer_with(id: &str, behaviour: FakeDecisionBehaviour) -> Arc<dyn SecurityReviewer> {
        let backend = Arc::new(FakeDecisionProvider::new(id, behaviour).with_revision("rev-1"));
        reviewer_for(Some(backend as Arc<dyn FastDecisionProvider>)).expect("a backend was given")
    }

    fn answering(value: DecisionValue) -> FakeDecisionBehaviour {
        FakeDecisionBehaviour::Answer(vec![(QUESTION_ID.to_string(), value)])
    }

    #[tokio::test]
    async fn a_passing_review_records_the_model_and_the_input_digest() {
        let report =
            reviewer_with("stub", answering(answer("pass", 0.9, json!([])))).review(input()).await;

        assert_eq!(report.evidence.status, EvidenceStatus::Passed);
        assert_eq!(report.evidence.kind, EvidenceKind::SecurityReview);
        assert_eq!(report.evidence.verifier_identity, "security_reviewer:stub");
        assert_eq!(report.evidence.verifier_model.as_deref(), Some("stub"));
        assert_eq!(
            report.evidence.verifier_context_hash.as_deref(),
            Some(input().input_hash().as_str())
        );
        let verdict = report.verdict.expect("the verdict is kept");
        assert_eq!(verdict.route_id, "stub");
        assert_eq!(verdict.revision, 3);
        assert_eq!(verdict.prompt_hash, pack::prompt_hash());
        assert_eq!(verdict.input_hash, input().input_hash());
    }

    #[tokio::test]
    async fn a_refusal_is_failed_evidence_with_its_reason() {
        let report = reviewer_with(
            "stub",
            answering(answer(
                "fail",
                0.95,
                json!([{ "severity": "high", "category": "scope_escape", "summary": "wrote outside the workspace" }]),
            )),
        )
        .review(input())
        .await;

        assert_eq!(report.evidence.status, EvidenceStatus::Failed);
        assert!(report.failure_kind.unwrap().contains("refused"));
        let verdict = report.verdict.unwrap();
        assert_eq!(verdict.findings[0].severity, FindingSeverity::High);
    }

    #[tokio::test]
    async fn a_low_confidence_pass_is_not_passed_evidence() {
        let report =
            reviewer_with("stub", answering(answer("pass", 0.2, json!([])))).review(input()).await;
        assert_eq!(report.evidence.status, EvidenceStatus::Failed);
        assert!(report.failure_kind.unwrap().contains("0.2"));
    }

    #[tokio::test]
    async fn a_pass_that_contradicts_its_own_finding_is_failed() {
        let finding = SecurityFinding {
            severity: FindingSeverity::Critical,
            category: "secret_exposure".to_string(),
            summary: "a token reached a tracked file".to_string(),
            evidence_ref: None,
        };
        let report = reviewer_with(
            "stub",
            answering(answer("pass", 0.99, serde_json::to_value([finding]).unwrap())),
        )
        .review(input())
        .await;
        assert_eq!(report.evidence.status, EvidenceStatus::Failed);
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_an_error_not_a_verdict() {
        let report = reviewer_with(
            "stub",
            answering(DecisionValue::Structured { value: json!({ "verdict": "maybe" }) }),
        )
        .review(input())
        .await;
        assert_eq!(report.evidence.status, EvidenceStatus::Error);
        assert!(report.verdict.is_none());
        assert!(report.failure_kind.unwrap().contains("not the contract"));
    }

    #[tokio::test]
    async fn an_abstention_is_an_error() {
        let report = reviewer_with("stub", FakeDecisionBehaviour::Abstain).review(input()).await;
        assert_eq!(report.evidence.status, EvidenceStatus::Error);
        assert_eq!(report.failure_kind.as_deref(), Some("abstained"));
    }

    #[tokio::test]
    async fn a_backend_that_cannot_be_reached_is_an_error_that_names_it() {
        let report = reviewer_with(
            "stub",
            FakeDecisionBehaviour::Fail(DecisionError::Unavailable("sidecar down".to_string())),
        )
        .review(input())
        .await;
        assert_eq!(report.evidence.status, EvidenceStatus::Error);
        assert_eq!(report.failure_kind.as_deref(), Some("unavailable"));
    }

    #[tokio::test]
    async fn a_hanging_reviewer_times_out_as_an_error() {
        let backend = Arc::new(FakeDecisionProvider::new(
            "slow",
            FakeDecisionBehaviour::Hang(Duration::from_secs(30)),
        ));
        let reviewer = BrokerReviewer {
            broker: DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty()))
                .with_backend(backend)
                .with_timeout(Duration::from_millis(20)),
        };
        let report = reviewer.review(input()).await;
        assert_eq!(report.evidence.status, EvidenceStatus::Error);
        assert_eq!(report.failure_kind.as_deref(), Some("timeout"));
    }

    #[test]
    fn the_pack_is_the_family_the_protocol_names() {
        assert_eq!(pack::pack().family.as_str(), "security_review");
        assert_eq!(pack::pack().version, "security/1");
    }

    /// The reviewer is a reader, not an actor: what it is asked is one closed
    /// question over references, and there is no surface through which it could
    /// run anything.
    ///
    /// The port itself is the first half of that — `SecurityReviewer` has one
    /// method, which takes an input and returns a report — and this pins the
    /// second: the pack asks one structured question with no options, and the
    /// facts are a closed set of references and digests with one redacted
    /// summary. A field that carried tool arguments or file contents would have
    /// to appear here to reach the reviewer at all.
    #[test]
    fn the_security_reviewer_gets_no_execution_tools() {
        let pack = pack::pack();
        assert_eq!(pack.questions.len(), 1, "one judgement, and it is the only one asked");
        let question = &pack.questions[0];
        assert_eq!(question.kind, agent_protocol::decision::DecisionKind::Structured);
        assert!(question.options.is_empty(), "there is nothing to select, only something to say");

        let mut with_web = input();
        with_web.untrusted_input_refs = vec!["web.request/1".to_string()];
        let facts = pack::facts(&with_web.bounded());
        let mut keys: Vec<&str> = facts.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "action_digests",
                "change_manifest_hash",
                "evidence_refs",
                "highest_risk",
                "intent_summary",
                "policy_version",
                "task_id",
                "task_revision",
                "untrusted_input_refs",
            ],
            "the reviewer is handed references and digests, never a tool call or its output"
        );
    }

    #[test]
    fn the_reviewer_runs_in_advisory_mode() {
        let backend = Arc::new(FakeDecisionProvider::new("stub", FakeDecisionBehaviour::Abstain));
        let reviewer = BrokerReviewer {
            broker: DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty()))
                .with_backend(backend)
                .with_timeout(REVIEW_TIMEOUT),
        };
        assert_eq!(reviewer.broker.mode(), DecisionMode::Advisory);
    }
}
