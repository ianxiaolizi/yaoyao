//! The permission-classifier family, over the existing broker.
//!
//! Nothing new is invented here: the pack and the bounded input come from
//! `agent_protocol::permission_classifier`, the call goes through
//! [`DecisionBroker`] (timeout, failure, abstention and validation behave
//! exactly as they do for every other family), and the answer is read back as
//! the structured contract that module defines.
//!
//! Three things this module *does* decide:
//!
//! - **Advisory is the mode**, on the same grounds as the adjudicator: the
//!   consumer is the kernel's permission matrix, which re-checks the answer
//!   against the policy baseline and the risk level before anything runs, and
//!   fails closed to "ask a person" when the answer is absent or unclear.
//! - **No rule baseline answer exists.** A low-risk call never reaches this
//!   family at all; a call that does is one the deterministic layers could not
//!   settle by themselves. The baseline here abstains on everything, so an
//!   abstention is reported as an abstention rather than as a rule's answer.
//! - **The outcome is the decision word, and the confidence is not read here.**
//!   A backend that says "allow" at 0.2 has said allow at 0.2; whether that is
//!   enough is a policy question, and the policy lives beside the matrix
//!   ([`agent_kernel::permission::MIN_CLASSIFIER_CONFIDENCE`]) so a reader can
//!   find every rule that can tighten a call in one place.
//!
//! The classifier has no tools and no writes: the only thing it returns is a
//! [`ClassifierTrace`], and the kernel decides what, if anything, to do.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_ports::decision::{DecisionBackendOutcome, DecisionValue, FastDecisionProvider};
use agent_ports::permission_classifier::{
    ClassifierOutcome, ClassifierTrace, PermissionClassifier, PermissionClassifierInput,
};
use agent_protocol::permission_classifier::{self as pack, QUESTION_ID, SuggestionDecision};

use crate::broker::DecisionBroker;
use crate::rule::RuleDecisionProvider;

/// How long one classification gets.
///
/// Shorter than an adjudication's 800 ms and much longer than a shadow
/// decision's 250 ms: this call sits on the path of an action that is already
/// about to run, and a person is waiting for either the action or the question.
/// Past this the kernel stops asking the classifier and asks *them* instead —
/// which is the same outcome an abstention gets, at the cost of one deadline.
pub const CLASSIFIER_TIMEOUT: Duration = Duration::from_millis(600);

/// Build the classifier over `backend`, or `None` when there is none.
///
/// A build with no sidecar has no independent judge, and the kernel says so
/// rather than assuming silence is consent: a boundary call then records
/// `classifier: unavailable` and stops for a person (CONTRACT §7 #177).
pub fn classifier_for(
    backend: Option<Arc<dyn FastDecisionProvider>>,
) -> Option<Arc<dyn PermissionClassifier>> {
    backend.map(|backend| {
        Arc::new(BrokerClassifier {
            broker: DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty()))
                .with_backend(backend)
                .with_timeout(CLASSIFIER_TIMEOUT),
        }) as Arc<dyn PermissionClassifier>
    })
}

/// The classifier: one broker, one pack, one deadline.
struct BrokerClassifier {
    broker: DecisionBroker,
}

#[async_trait::async_trait]
impl PermissionClassifier for BrokerClassifier {
    async fn classify(&self, input: &PermissionClassifierInput) -> ClassifierTrace {
        let started = Instant::now();
        let pack = pack::pack();
        let state = match pack::state(input) {
            Ok(state) => state,
            // No backend is asked when the input cannot be bounded; the trace
            // says that rather than borrowing an outcome it did not earn.
            Err(error) => return trace_of("", error.kind(), 0, None),
        };

        let decision = self.broker.decide(&pack, &state).await;
        let latency_ms = started.elapsed().as_millis() as u64;

        let audit = |route_id: String, revision: Option<String>, outcome: ClassifierOutcome| {
            ClassifierTrace {
                route_id,
                revision: revision.unwrap_or_default(),
                pack_version: pack::PACK_VERSION.to_string(),
                prompt_hash: pack::prompt_hash(),
                input_hash: pack::input_hash(input),
                latency_ms,
                outcome,
                suggestion: None,
            }
        };

        if let Some(aborted) = &decision.aborted {
            return trace_of("", aborted, latency_ms, None);
        }

        let Some(chosen) = &decision.chosen else {
            let Some(report) = decision.backends.iter().find(|r| r.backend_id != "rule") else {
                return trace_of("", "unavailable", latency_ms, None);
            };
            let kind = match &report.outcome {
                DecisionBackendOutcome::Failed { error_kind } => error_kind.clone(),
                DecisionBackendOutcome::Abstained => "abstained".to_string(),
                DecisionBackendOutcome::Answered(_) => "protocol".to_string(),
            };
            let mut trace = audit(
                report.backend_id.clone(),
                report.revision.clone(),
                ClassifierOutcome::Unclear,
            );
            trace.outcome = outcome_for(&kind);
            return trace;
        };

        let chosen_id = decision.chosen_backend.clone().unwrap_or_default();
        let revision = decision.report_for(&chosen_id).and_then(|r| r.revision.clone());
        let mut trace = audit(chosen_id, revision, ClassifierOutcome::Unclear);

        let value = match chosen.answer_for(QUESTION_ID).map(|answer| &answer.value) {
            Some(DecisionValue::Structured { value }) => value,
            // The pack validated one structured answer, so this is the shape
            // drifting between the pack layer and this one — a protocol failure
            // by definition, not an opinion.
            _ => return trace,
        };
        let Ok(suggestion) = pack::parse(value) else {
            return trace;
        };
        trace.outcome = match suggestion.decision {
            SuggestionDecision::Allow => ClassifierOutcome::Allow,
            SuggestionDecision::Ask => ClassifierOutcome::Ask,
            SuggestionDecision::Deny => ClassifierOutcome::Deny,
        };
        trace.suggestion = Some(suggestion);
        trace
    }
}

/// A trace for a call no backend answered, naming what went wrong.
fn trace_of(
    route_id: &str,
    kind: &str,
    latency_ms: u64,
    input_hash: Option<String>,
) -> ClassifierTrace {
    let mut trace = ClassifierTrace::unavailable(route_id);
    trace.outcome = outcome_for(kind);
    trace.latency_ms = latency_ms;
    trace.input_hash = input_hash.unwrap_or_default();
    trace
}

/// Which of the two "nobody answered" outcomes a backend's failure is.
///
/// `unavailable` and `timeout` mean no classifier was reachable, which is a
/// different fact from one that answered wrongly — the first says the build has
/// no independent judge, the second says the judge drifted. Both stop the call;
/// only the record tells them apart.
fn outcome_for(kind: &str) -> ClassifierOutcome {
    match kind {
        "unavailable" | "timeout" | "state_too_large" | "state_unmeasurable" => {
            ClassifierOutcome::Unavailable
        }
        _ => ClassifierOutcome::Unclear,
    }
}

impl std::fmt::Debug for BrokerClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerClassifier").field("mode", &self.broker.mode()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::decision::{DecisionError, DecisionMode};
    use agent_protocol::ids::SessionId;
    use agent_protocol::permission::{ActionFacts, ActionSpec, Reversibility, RiskLevel};
    use agent_protocol::permissions::{Capability, PolicyDecision};
    use agent_testkit::{FakeDecisionBehaviour, FakeDecisionProvider};
    use serde_json::json;

    fn input() -> PermissionClassifierInput {
        PermissionClassifierInput {
            session_id: SessionId::from_string("s"),
            task_id: "intent-1".to_string(),
            task_revision: 3,
            task_summary: Some("clean the build directory".to_string()),
            action: ActionFacts::from_spec(ActionSpec {
                task_id: "intent-1".to_string(),
                task_revision: 3,
                tool_name: "shell.run".to_string(),
                capabilities: vec![Capability::new(Capability::PROCESS_SPAWN)],
                risk: RiskLevel::L3,
                reversibility: Reversibility::Irreversible,
                targets: Vec::new(),
                arguments: json!({ "command": "rm -rf build" }),
                summary: "shell.run rm -rf build".to_string(),
            }),
            policy_baseline: PolicyDecision::Ask,
            allowed_capabilities: vec![Capability::new(Capability::PROCESS_SPAWN)],
            recent_actions: Vec::new(),
            untrusted_context_summary: None,
        }
    }

    fn answer(decision: &str, confidence: f64) -> DecisionValue {
        DecisionValue::Structured {
            value: json!({
                "decision": decision,
                "confidence": confidence,
                "risk": "l3",
                "reversible": false,
                "requires_confirmation": true,
                "reason": "test",
            }),
        }
    }

    fn answering(value: DecisionValue) -> FakeDecisionBehaviour {
        FakeDecisionBehaviour::Answer(vec![(QUESTION_ID.to_string(), value)])
    }

    fn classifier(behaviour: FakeDecisionBehaviour) -> Arc<dyn PermissionClassifier> {
        classifier_with("stub", behaviour)
    }

    fn classifier_with(
        id: &str,
        behaviour: FakeDecisionBehaviour,
    ) -> Arc<dyn PermissionClassifier> {
        let backend = Arc::new(FakeDecisionProvider::new(id, behaviour).with_revision("rev-1"));
        classifier_for(Some(backend as Arc<dyn FastDecisionProvider>)).expect("a backend was given")
    }

    #[tokio::test]
    async fn a_structured_backend_answers_the_permission_pack() {
        let trace = classifier(answering(answer("deny", 0.9))).classify(&input()).await;

        assert_eq!(trace.route_id, "stub");
        assert_eq!(trace.revision, "rev-1");
        assert_eq!(trace.pack_version, pack::PACK_VERSION);
        assert_eq!(trace.prompt_hash, pack::prompt_hash());
        assert_eq!(trace.input_hash, pack::input_hash(&input()));
        assert_eq!(trace.outcome, ClassifierOutcome::Deny);
        let suggestion = trace.suggestion.expect("the answer is kept");
        assert_eq!(suggestion.confidence, 0.9);
        assert_eq!(suggestion.risk, Some(RiskLevel::L3));
    }

    #[tokio::test]
    async fn a_low_confidence_allow_is_kept_as_it_was_answered() {
        // The number is not re-decided here: the trace says what the backend
        // said, and the kernel's matrix is what refuses to act on 0.2.
        let trace = classifier(answering(answer("allow", 0.2))).classify(&input()).await;
        assert_eq!(trace.outcome, ClassifierOutcome::Allow);
        assert_eq!(trace.suggestion.as_ref().map(|s| s.confidence), Some(0.2));
    }

    #[tokio::test]
    async fn an_answer_that_is_not_the_contract_is_unclear_and_keeps_nothing() {
        let trace = classifier(answering(DecisionValue::Structured {
            value: json!({ "decision": "maybe", "confidence": 0.5 }),
        }))
        .classify(&input())
        .await;
        assert_eq!(trace.outcome, ClassifierOutcome::Unclear);
        assert!(trace.suggestion.is_none());
        assert!(trace.outcome.asks());
    }

    #[tokio::test]
    async fn an_answer_of_the_wrong_kind_is_unclear() {
        let trace =
            classifier(answering(DecisionValue::Categorical { choice: "allow".to_string() }))
                .classify(&input())
                .await;
        assert_eq!(trace.outcome, ClassifierOutcome::Unclear);
    }

    #[tokio::test]
    async fn an_abstention_is_unclear_rather_than_an_opinion() {
        let trace = classifier(FakeDecisionBehaviour::Abstain).classify(&input()).await;
        assert_eq!(trace.outcome, ClassifierOutcome::Unclear);
        assert!(trace.suggestion.is_none());
    }

    #[tokio::test]
    async fn a_backend_that_cannot_be_reached_is_unavailable() {
        let trace = classifier(FakeDecisionBehaviour::Fail(DecisionError::Unavailable(
            "sidecar down".to_string(),
        )))
        .classify(&input())
        .await;
        assert_eq!(trace.outcome, ClassifierOutcome::Unavailable);
        assert_eq!(trace.route_id, "stub");
    }

    #[tokio::test]
    async fn a_hanging_backend_times_out_without_reaching_the_kernel() {
        let backend = Arc::new(FakeDecisionProvider::new(
            "slow",
            FakeDecisionBehaviour::Hang(Duration::from_secs(30)),
        ));
        let fast = BrokerClassifier {
            broker: DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty()))
                .with_backend(backend)
                .with_timeout(Duration::from_millis(20)),
        };
        let trace = fast.classify(&input()).await;
        assert_eq!(trace.outcome, ClassifierOutcome::Unavailable);
    }

    #[test]
    fn no_backend_means_no_classifier() {
        assert!(classifier_for(None).is_none());
    }

    #[tokio::test]
    async fn the_broker_runs_this_family_in_advisory_mode() {
        let backend = Arc::new(FakeDecisionProvider::new("stub", FakeDecisionBehaviour::Abstain));
        let broker =
            DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty())).with_backend(backend);
        assert_eq!(broker.mode(), DecisionMode::Advisory);
        let trace = broker.decide(&pack::pack(), &pack::state(&input()).unwrap()).await;
        assert_eq!(trace.mode, DecisionMode::Advisory);
        assert_eq!(
            trace.family.as_str(),
            agent_protocol::decision::DecisionFamily::PERMISSION_CLASSIFIER
        );
        assert_eq!(trace.question_ids, vec![QUESTION_ID.to_string()]);
    }
}
