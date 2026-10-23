//! The rule backend: the deterministic baseline that always exists.
//!
//! Every other backend is optional — a local model may be down, a sidecar may
//! time out, an LLM may be over budget. Rules are not: they are pure functions
//! of the state, so there is no configuration in which "we had no baseline" is
//! a true statement. The broker relies on that by always running this one.

use agent_ports::decision::{
    DecisionAnswer, DecisionError, DecisionFamily, DecisionResult, DecisionState, DecisionValue,
    FastDecisionProvider, QuestionPack,
};

/// What a rule answers with.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleAnswer {
    /// The same value every time.
    Fixed(DecisionValue),
    /// The state's fact named by `key`, projected into a decision value.
    ///
    /// This is how a rule says "whatever the deterministic path already
    /// decided" without the rule table having to be rebuilt per call.
    Fact { key: String },
}

/// One rule: when a pack in `family` asks `question_id`, answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleDecision {
    pub family: DecisionFamily,
    pub question_id: String,
    pub answer: RuleAnswer,
}

impl RuleDecision {
    /// A rule answering with a constant.
    pub fn fixed(
        family: impl Into<String>,
        question_id: impl Into<String>,
        value: DecisionValue,
    ) -> Self {
        Self {
            family: DecisionFamily::new(family),
            question_id: question_id.into(),
            answer: RuleAnswer::Fixed(value),
        }
    }

    /// A rule answering with the state's fact `key`.
    pub fn from_fact(
        family: impl Into<String>,
        question_id: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            family: DecisionFamily::new(family),
            question_id: question_id.into(),
            answer: RuleAnswer::Fact { key: key.into() },
        }
    }
}

/// A backend that answers from a fixed rule table.
#[derive(Default)]
pub struct RuleDecisionProvider {
    rules: Vec<RuleDecision>,
}

impl RuleDecisionProvider {
    /// A provider over the given rules.
    pub fn new(rules: Vec<RuleDecision>) -> Self {
        Self { rules }
    }

    /// A provider with no rules: it abstains on everything.
    ///
    /// Still a valid baseline — "no opinion" is an opinion — and the one a
    /// caller gets when a family has no deterministic answer to give yet.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The rules, for inspection in tests.
    pub fn rules(&self) -> &[RuleDecision] {
        &self.rules
    }
}

/// Project a state fact into a decision value.
///
/// The mapping is mechanical and kind-preserving: a bool is binary, a string is
/// a category, a number is a score, an array of strings is a ranking. Anything
/// else has no reading as an answer and yields `None`, so a rule pointing at an
/// object fact abstains instead of inventing a value.
fn value_from_fact(fact: &serde_json::Value) -> Option<DecisionValue> {
    match fact {
        serde_json::Value::Bool(value) => Some(DecisionValue::Binary { value: *value }),
        serde_json::Value::String(choice) => {
            Some(DecisionValue::Categorical { choice: choice.clone() })
        }
        serde_json::Value::Number(number) => {
            number.as_f64().map(|score| DecisionValue::Ordinal { score })
        }
        serde_json::Value::Array(items) => {
            let order = items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()?;
            Some(DecisionValue::Ranking { order })
        }
        serde_json::Value::Null | serde_json::Value::Object(_) => None,
    }
}

#[async_trait::async_trait]
impl FastDecisionProvider for RuleDecisionProvider {
    async fn decide(
        &self,
        pack: &QuestionPack,
        state: &DecisionState,
    ) -> Result<DecisionResult, DecisionError> {
        let mut answers = Vec::new();
        for rule in &self.rules {
            if rule.family != pack.family {
                continue;
            }
            // A rule for a question this pack does not ask is not an answer to
            // anything; skipping it keeps one table usable across pack versions.
            if !pack.questions.iter().any(|q| q.question_id == rule.question_id) {
                continue;
            }
            let value = match &rule.answer {
                RuleAnswer::Fixed(value) => Some(value.clone()),
                RuleAnswer::Fact { key } => state.fact(key).and_then(value_from_fact),
            };
            if let Some(value) = value {
                answers.push(DecisionAnswer::new(rule.question_id.clone(), value));
            }
        }

        let result = if answers.is_empty() {
            DecisionResult::abstained(pack)
        } else {
            DecisionResult::answered(pack, answers)
        };

        // The baseline's own output goes through the same check every backend's
        // does. A rule pointing at a fact of the wrong shape is a configuration
        // bug, and it should surface as a failed baseline in the trace rather
        // than as a plausible-looking answer.
        pack.validate(&result).map_err(DecisionError::Protocol)?;
        Ok(result)
    }

    fn provider_id(&self) -> String {
        "rule".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::ids::SessionId;

    fn pack() -> QuestionPack {
        QuestionPack::new(
            DecisionFamily::new(DecisionFamily::MODEL_ROUTE),
            "v1",
            vec![agent_ports::decision::DecisionQuestion::categorical(
                "tier",
                "Which tier?",
                vec![
                    agent_ports::decision::DecisionOption::new("main", "main"),
                    agent_ports::decision::DecisionOption::new("fast", "fast"),
                ],
            )],
        )
    }

    fn state(facts: serde_json::Map<String, serde_json::Value>) -> DecisionState {
        DecisionState::new(SessionId::from_string("s"), None, 0, facts).expect("small enough")
    }

    fn facts(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[tokio::test]
    async fn a_fixed_rule_answers_the_question() {
        let provider = RuleDecisionProvider::new(vec![RuleDecision::fixed(
            DecisionFamily::MODEL_ROUTE,
            "tier",
            DecisionValue::Categorical { choice: "main".to_string() },
        )]);

        let result = provider.decide(&pack(), &state(facts(&[]))).await.unwrap();

        assert!(!result.abstained);
        assert_eq!(
            result.answer_for("tier").map(|a| a.value.clone()),
            Some(DecisionValue::Categorical { choice: "main".to_string() })
        );
    }

    #[tokio::test]
    async fn a_fact_rule_copies_the_state_verbatim() {
        let provider = RuleDecisionProvider::new(vec![RuleDecision::from_fact(
            DecisionFamily::MODEL_ROUTE,
            "tier",
            "chosen_tier",
        )]);

        let result = provider
            .decide(&pack(), &state(facts(&[("chosen_tier", serde_json::json!("fast"))])))
            .await
            .unwrap();

        assert_eq!(
            result.answer_for("tier").map(|a| a.value.clone()),
            Some(DecisionValue::Categorical { choice: "fast".to_string() })
        );
    }

    #[tokio::test]
    async fn a_missing_fact_abstains_rather_than_guessing() {
        let provider = RuleDecisionProvider::new(vec![RuleDecision::from_fact(
            DecisionFamily::MODEL_ROUTE,
            "tier",
            "chosen_tier",
        )]);

        let result = provider.decide(&pack(), &state(facts(&[]))).await.unwrap();

        assert!(result.abstained);
        assert!(result.answers.is_empty());
    }

    #[tokio::test]
    async fn a_fact_with_no_answer_reading_abstains() {
        let provider = RuleDecisionProvider::new(vec![RuleDecision::from_fact(
            DecisionFamily::MODEL_ROUTE,
            "tier",
            "chosen_tier",
        )]);

        let result = provider
            .decide(&pack(), &state(facts(&[("chosen_tier", serde_json::json!({"a": 1}))])))
            .await
            .unwrap();

        assert!(result.abstained);
    }

    #[tokio::test]
    async fn rules_for_other_families_and_questions_are_ignored() {
        let provider = RuleDecisionProvider::new(vec![
            RuleDecision::fixed(
                DecisionFamily::INPUT_GUARD,
                "tier",
                DecisionValue::Categorical { choice: "main".to_string() },
            ),
            RuleDecision::fixed(
                DecisionFamily::MODEL_ROUTE,
                "not_asked",
                DecisionValue::Categorical { choice: "main".to_string() },
            ),
        ]);

        let result = provider.decide(&pack(), &state(facts(&[]))).await.unwrap();

        assert!(result.abstained, "an inapplicable rule must not become an answer");
    }

    #[tokio::test]
    async fn a_rule_of_the_wrong_shape_fails_instead_of_answering() {
        // A binary value cannot answer a categorical question. The baseline is
        // not exempt from the pack's validation.
        let provider = RuleDecisionProvider::new(vec![RuleDecision::fixed(
            DecisionFamily::MODEL_ROUTE,
            "tier",
            DecisionValue::Binary { value: true },
        )]);

        let error = provider.decide(&pack(), &state(facts(&[]))).await.unwrap_err();

        assert_eq!(error.kind(), "protocol");
    }

    #[test]
    fn the_baseline_backend_id_is_stable() {
        assert_eq!(RuleDecisionProvider::empty().provider_id(), "rule");
        assert!(RuleDecisionProvider::empty().revision().is_none());
    }
}
