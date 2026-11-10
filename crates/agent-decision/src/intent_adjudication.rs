//! The intent-adjudication family, over the existing broker.
//!
//! Nothing new is invented here: the pack and the bounded input come from
//! `agent_protocol::intent_adjudication`, the call goes through
//! [`DecisionBroker`] (so timeout, failure, abstention, and validation behave
//! exactly as they do for every other family), and the result is read back as
//! the structured contract those layers already define.
//!
//! Two things this module *does* decide:
//!
//! - **Advisory is the mode.** This family is the one exception to "only shadow
//!   is available", granted by CONTRACT §7 #161: its consumer is the kernel's
//!   policy matrix, which re-checks every suggestion before acting and fails
//!   closed to a clarification question. The route advisor keeps shadow.
//! - **No rule baseline answer exists.** The kernel's rule layer
//!   (`adjudicate::route`) already ran and produced `unclear` — that is *why*
//!   this call happens. So the baseline here abstains on everything, and an
//!   abstention is reported as such rather than being mistaken for a rule.
//!
//! The adjudicator has no tools and no writes: the only thing it returns is a
//! [`AdjudicationReport`], and the kernel decides what, if anything, to do.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_ports::decision::{DecisionBackendOutcome, DecisionValue, FastDecisionProvider};
use agent_ports::intent_adjudication::{
    AdjudicationInput, AdjudicationOutcome, AdjudicationReport, IntentAdjudicator,
};
use agent_protocol::intent_adjudication::{self, QUESTION_ID};

use crate::broker::DecisionBroker;
use crate::rule::RuleDecisionProvider;

/// How long one adjudication call gets.
///
/// Longer than a shadow decision's 250 ms because this call sits on the path of
/// a message that the rules could not route, and a wrong guess costs a task
/// version rather than a trace line. Still short enough that the person waiting
/// does not feel it: past this the kernel stops asking and asks *them* instead.
pub const ADJUDICATION_TIMEOUT: Duration = Duration::from_millis(800);

/// Build the adjudicator over `backend`, or `None` when there is none.
///
/// A build with no sidecar has no one to ask. That is not a broken stack: the
/// kernel then records every unclear message as unadjudicated and asks the
/// person, which is the same answer a failed call gets.
pub fn adjudicator_for(
    backend: Option<Arc<dyn FastDecisionProvider>>,
) -> Option<Arc<dyn IntentAdjudicator>> {
    backend.map(|backend| {
        Arc::new(BrokerAdjudicator {
            broker: DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty()))
                .with_backend(backend)
                .with_timeout(ADJUDICATION_TIMEOUT),
            timeout: ADJUDICATION_TIMEOUT,
        }) as Arc<dyn IntentAdjudicator>
    })
}

/// The adjudicator: one broker, one pack, one deadline.
struct BrokerAdjudicator {
    broker: DecisionBroker,
    /// Kept beside the broker's per-backend deadline so the report's latency is
    /// the whole call, not the backend's share of it.
    timeout: Duration,
}

#[async_trait::async_trait]
impl IntentAdjudicator for BrokerAdjudicator {
    async fn adjudicate(&self, input: &AdjudicationInput) -> AdjudicationReport {
        let started = Instant::now();
        let pack = intent_adjudication::pack();
        let state = match intent_adjudication::state(input) {
            Ok(state) => state,
            Err(error) => {
                // No backend is asked when the input cannot be bounded; the
                // report says that rather than looking like an abstention.
                return AdjudicationReport::unasked(error.kind());
            }
        };

        let trace = self.broker.decide(&pack, &state).await;
        let latency_ms = started.elapsed().as_millis() as u64;

        let audit = |route_id: String, revision: Option<String>| AdjudicationReport {
            route_id,
            revision,
            pack_version: intent_adjudication::PACK_VERSION.to_string(),
            prompt_hash: intent_adjudication::prompt_hash(),
            input_hash: intent_adjudication::input_hash(input),
            latency_ms,
            outcome: AdjudicationOutcome::Abstained,
        };

        if let Some(aborted) = &trace.aborted {
            return AdjudicationReport::unasked(aborted.clone());
        }

        let Some(chosen) = &trace.chosen else {
            // Nobody answered. The one backend's own report says why.
            let Some(report) = trace.backends.iter().find(|r| r.backend_id != "rule") else {
                return AdjudicationReport::unasked("unavailable");
            };
            let mut audit = audit(report.backend_id.clone(), report.revision.clone());
            audit.outcome = match &report.outcome {
                DecisionBackendOutcome::Failed { error_kind } => {
                    AdjudicationOutcome::Failed { kind: error_kind.clone() }
                }
                DecisionBackendOutcome::Abstained => AdjudicationOutcome::Abstained,
                // An answered report would have settled as the chosen one.
                DecisionBackendOutcome::Answered(_) => {
                    AdjudicationOutcome::Failed { kind: "protocol".to_string() }
                }
            };
            return audit;
        };

        let chosen_id = trace.chosen_backend.clone().unwrap_or_default();
        let backend = trace.report_for(&chosen_id);
        let mut audit = audit(chosen_id, backend.and_then(|report| report.revision.clone()));

        let value = match chosen.answer_for(QUESTION_ID).map(|answer| &answer.value) {
            Some(DecisionValue::Structured { value }) => value,
            // The pack validated one structured answer, so this is the shape
            // drifting between the pack layer and this one — a protocol failure
            // by definition, not an opinion.
            _ => {
                audit.outcome = AdjudicationOutcome::Failed { kind: "protocol".to_string() };
                return audit;
            }
        };
        audit.outcome = match intent_adjudication::parse(value) {
            Ok(suggestion) => AdjudicationOutcome::Suggested(suggestion),
            Err(_) => AdjudicationOutcome::Failed { kind: "protocol".to_string() },
        };
        audit
    }
}

impl std::fmt::Debug for BrokerAdjudicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerAdjudicator")
            .field("mode", &self.broker.mode())
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::decision::{DecisionError, DecisionMode};
    use agent_protocol::ids::SessionId;
    use agent_testkit::{FakeDecisionBehaviour, FakeDecisionProvider};
    use serde_json::json;

    fn input(message: &str) -> AdjudicationInput {
        AdjudicationInput {
            session_id: SessionId::from_string("s"),
            turn_id: None,
            context_revision: 3,
            task_summary: None,
            pending: vec!["run the tests".to_string()],
            recent: Vec::new(),
            untrusted_tool_summary: None,
            message: message.to_string(),
        }
    }

    fn answer(relation: &str, confidence: f64) -> DecisionValue {
        DecisionValue::Structured {
            value: json!({
                "relation": relation,
                "target_task_id": null,
                "confidence": confidence,
                "preserve": ["evidence"],
                "invalidate": [],
                "scope_expansion": false,
                "requires_confirmation": false,
                "reason": "test",
            }),
        }
    }

    /// An answer to this pack's one question.
    fn answering(value: DecisionValue) -> FakeDecisionBehaviour {
        FakeDecisionBehaviour::Answer(vec![(QUESTION_ID.to_string(), value)])
    }

    fn adjudicator(behaviour: FakeDecisionBehaviour) -> Arc<dyn IntentAdjudicator> {
        adjudicator_with("stub", behaviour)
    }

    fn adjudicator_with(id: &str, behaviour: FakeDecisionBehaviour) -> Arc<dyn IntentAdjudicator> {
        let backend = Arc::new(FakeDecisionProvider::new(id, behaviour).with_revision("rev-1"));
        adjudicator_for(Some(backend as Arc<dyn FastDecisionProvider>))
            .expect("a backend was given")
    }

    #[tokio::test]
    async fn a_structured_answer_comes_back_as_a_suggestion_with_its_audit() {
        let report = adjudicator(answering(answer("new_task", 0.9)))
            .adjudicate(&input("do something else"))
            .await;

        assert_eq!(report.route_id, "stub");
        assert_eq!(report.revision.as_deref(), Some("rev-1"));
        assert_eq!(report.pack_version, intent_adjudication::PACK_VERSION);
        assert_eq!(report.prompt_hash, intent_adjudication::prompt_hash());
        let AdjudicationOutcome::Suggested(suggestion) = &report.outcome else {
            panic!("a suggested outcome, not {:?}", report.outcome);
        };
        assert_eq!(suggestion.relation.as_str(), "new_task");
        assert_eq!(suggestion.confidence, 0.9);
    }

    #[tokio::test]
    async fn the_input_digest_is_the_digest_of_the_facts_asked_over() {
        let report = adjudicator(FakeDecisionBehaviour::Abstain).adjudicate(&input("hello")).await;
        let expected = intent_adjudication::input_hash(&input("hello"));
        assert_eq!(report.input_hash, expected);
        assert_eq!(report.outcome, AdjudicationOutcome::Abstained);
    }

    #[tokio::test]
    async fn an_answer_that_is_not_the_contract_is_a_protocol_failure() {
        let report = adjudicator(answering(DecisionValue::Structured {
            value: json!({ "relation": "maybe", "confidence": 0.5 }),
        }))
        .adjudicate(&input("hello"))
        .await;
        assert_eq!(report.outcome, AdjudicationOutcome::Failed { kind: "protocol".into() });
    }

    #[tokio::test]
    async fn an_answer_of_the_wrong_kind_is_a_protocol_failure() {
        let report =
            adjudicator(answering(DecisionValue::Categorical { choice: "new_task".to_string() }))
                .adjudicate(&input("hello"))
                .await;
        assert_eq!(report.outcome, AdjudicationOutcome::Failed { kind: "protocol".into() });
    }

    #[tokio::test]
    async fn a_backend_failure_is_reported_by_its_stable_kind() {
        let report = adjudicator(FakeDecisionBehaviour::Fail(DecisionError::Unavailable(
            "sidecar down".to_string(),
        )))
        .adjudicate(&input("hello"))
        .await;
        assert_eq!(report.outcome, AdjudicationOutcome::Failed { kind: "unavailable".into() });
        assert_eq!(report.route_id, "stub");
    }

    #[tokio::test]
    async fn a_hanging_backend_times_out_without_reaching_the_kernel() {
        let backend = Arc::new(FakeDecisionProvider::new(
            "slow",
            FakeDecisionBehaviour::Hang(Duration::from_secs(30)),
        ));
        let fast = BrokerAdjudicator {
            broker: DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty()))
                .with_backend(backend)
                .with_timeout(Duration::from_millis(20)),
            timeout: Duration::from_millis(20),
        };
        let report = fast.adjudicate(&input("hello")).await;
        assert_eq!(report.outcome, AdjudicationOutcome::Failed { kind: "timeout".into() });
    }

    #[test]
    fn no_backend_means_no_adjudicator() {
        assert!(adjudicator_for(None).is_none());
    }

    #[tokio::test]
    async fn the_broker_runs_this_family_in_advisory_mode() {
        let backend = Arc::new(FakeDecisionProvider::new("stub", FakeDecisionBehaviour::Abstain));
        let broker =
            DecisionBroker::advisory(Arc::new(RuleDecisionProvider::empty())).with_backend(backend);
        assert_eq!(broker.mode(), DecisionMode::Advisory);
        let trace = broker
            .decide(&intent_adjudication::pack(), &intent_adjudication::state(&input("x")).unwrap())
            .await;
        assert_eq!(trace.mode, DecisionMode::Advisory);
        assert_eq!(
            trace.family.as_str(),
            agent_protocol::decision::DecisionFamily::INTENT_ADJUDICATION
        );
        assert_eq!(trace.question_ids, vec![QUESTION_ID.to_string()]);
    }
}
