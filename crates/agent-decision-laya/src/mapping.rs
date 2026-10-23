//! The vocabulary mapping: this project's decisions in, Laya's answers out.
//!
//! The two vocabularies are independent. `DecisionKind` is this project's
//! (`crates/agent-protocol/src/decision.rs`); Laya's is `choice` / `score` /
//! `noul`. Three of the four map cleanly and the fourth does not exist:
//!
//! ```text
//! Categorical  ->  choice   criteria is an object keyed by option id
//! Ordinal      ->  score    criteria is an array of level labels
//! Binary       ->  noul     P(true)
//! Ranking      ->  (none)
//! ```
//!
//! `Ranking` has no primitive on the other side — Laya's `shortlist` recalls
//! candidates, it does not order a decision — so a pack containing one is
//! **refused as a payload**, not a question quietly skipped. Whose shape is
//! wrong decides the label: every refusal in this module that happens before a
//! request is sent says the pack could not be handed over, which is
//! [`DecisionError::Payload`]; `Protocol` is kept for an answer the backend did
//! give and this side could not read. A backend that silently answered only the
//! questions it liked would make "the model agreed" and "the model was never
//! asked" look the same.
//!
//! An `Ordinal` question in this project carries no levels (the protocol leaves
//! `options` empty for that kind), so the levels come from the question's own
//! options when a caller supplied them and from
//! [`DEFAULT_ORDINAL_LEVELS`] otherwise. Either way the count used to build the
//! question is the count used to normalize the answer, so a `score` of "level
//! 1 of 5" always becomes the same number in `0..=1`.

use std::collections::BTreeMap;

use agent_ports::decision::{
    DecisionAnswer, DecisionError, DecisionKind, DecisionQuestion, DecisionResult, DecisionValue,
    QuestionPack,
};

use crate::wire::{WireAnswer, WireQuestion};

/// The levels an `Ordinal` question is given when it names none of its own.
///
/// A neutral five-point scale. It is legible in a question's `instructions`
/// ("lowest to highest") and its length is what makes a `score` convertible; a
/// caller that cares what the middle means should put its own levels in the
/// question's options.
pub const DEFAULT_ORDINAL_LEVELS: [&str; 5] = ["lowest", "low", "medium", "high", "highest"];

/// How far outside the level-index range a `score` may land and still be read
/// as the nearest endpoint.
///
/// `laya-serve` reports an expected index, and a computed expectation can
/// overshoot `0..=width` by a rounding step. That much is clamped — it is the
/// same answer with float noise on it. Anything further out is the server
/// answering outside the scale it was given, which is a protocol failure and
/// not a silent vote for "lowest" or "highest".
const SCORE_EPSILON: f64 = 1e-6;

/// The wire type name for one of this project's question kinds.
fn wire_kind(kind: DecisionKind) -> Option<&'static str> {
    match kind {
        DecisionKind::Categorical => Some("choice"),
        DecisionKind::Ordinal => Some("score"),
        DecisionKind::Binary => Some("noul"),
        DecisionKind::Ranking => None,
    }
}

/// The levels a `score` question is asked with.
fn levels_for(
    question: &DecisionQuestion,
    default_levels: &[&str],
) -> Result<Vec<String>, DecisionError> {
    let levels: Vec<String> = if question.options.is_empty() {
        default_levels.iter().map(|label| (*label).to_string()).collect()
    } else {
        question.options.iter().map(|option| option.label.clone()).collect()
    };

    if levels.len() < 2 {
        return Err(DecisionError::Payload(format!(
            "question '{}' is ordinal and has {} level(s); a score needs at least two to have a \
             direction",
            question.question_id,
            levels.len()
        )));
    }
    Ok(levels)
}

/// Build the questions to ask, or refuse a pack this sidecar cannot answer.
pub(crate) fn questions_of(
    pack: &QuestionPack,
    default_levels: &[&str],
) -> Result<BTreeMap<String, WireQuestion>, DecisionError> {
    let mut questions = BTreeMap::new();

    for question in &pack.questions {
        let Some(kind) = wire_kind(question.kind) else {
            return Err(DecisionError::Payload(format!(
                "question '{}' is a ranking, which this backend has no primitive for; Laya \
                 answers choice, score, and noul only",
                question.question_id
            )));
        };

        let criteria = match question.kind {
            DecisionKind::Categorical => {
                if question.options.is_empty() {
                    return Err(DecisionError::Payload(format!(
                        "question '{}' is categorical and offers no options",
                        question.question_id
                    )));
                }
                let options: serde_json::Map<String, serde_json::Value> = question
                    .options
                    .iter()
                    .map(|option| {
                        (option.id.clone(), serde_json::Value::String(option.label.clone()))
                    })
                    .collect();
                Some(serde_json::Value::Object(options))
            }
            DecisionKind::Ordinal => {
                let levels = levels_for(question, default_levels)?;
                Some(serde_json::Value::Array(
                    levels.into_iter().map(serde_json::Value::String).collect(),
                ))
            }
            // A noul question takes no criteria; the server defaults the two
            // sides to false/true. Sending an empty object is not the same as
            // sending nothing, so nothing is sent.
            DecisionKind::Binary => None,
            DecisionKind::Ranking => unreachable!("refused above"),
        };

        let wire = WireQuestion { kind, instructions: question.prompt.clone(), criteria };
        if questions.insert(question.question_id.clone(), wire).is_some() {
            return Err(DecisionError::Payload(format!(
                "the pack asks question '{}' twice",
                question.question_id
            )));
        }
    }

    Ok(questions)
}

/// Turn one response into a result for the pack that was asked, or refuse it.
///
/// An abstention is synthesized here: a pack is answered only if **every**
/// question's stated confidence clears `threshold`. One unsure answer abstains
/// the whole pack rather than being dropped from it — a partial set recorded as
/// `Answered` reads exactly like a complete one, and a trace that cannot tell
/// "the model had no opinion on one question" apart from "the model answered
/// everything" is not a comparable decision record. Laya has no way to say "I
/// don't know", so this is where the sidecar's certainty becomes the project's
/// vocabulary — and it is the only place a low-confidence answer stops being an
/// answer.
pub(crate) fn answers_of(
    pack: &QuestionPack,
    default_levels: &[&str],
    answers: &BTreeMap<String, WireAnswer>,
    threshold: f64,
) -> Result<DecisionResult, DecisionError> {
    for id in answers.keys() {
        if !pack.questions.iter().any(|question| &question.question_id == id) {
            return Err(DecisionError::Protocol(format!(
                "the backend answered question '{id}', which was not asked"
            )));
        }
    }

    let mut mapped = Vec::new();
    let mut unsure = false;
    for question in &pack.questions {
        let Some(answer) = answers.get(&question.question_id) else {
            return Err(DecisionError::Protocol(format!(
                "the backend did not answer question '{}'",
                question.question_id
            )));
        };

        let Some(kind) = wire_kind(question.kind) else {
            return Err(DecisionError::Payload(format!(
                "question '{}' is a ranking, which this backend has no primitive for",
                question.question_id
            )));
        };
        if answer.kind() != kind {
            return Err(DecisionError::Protocol(format!(
                "question '{}' was asked as {kind} and answered as {}",
                question.question_id,
                answer.kind()
            )));
        }

        let confidence = answer.answer_confidence();
        // An unstated confidence is not a low one: it is the absence of the
        // one number the gate reads, and inventing a zero for it would turn
        // "the server said nothing" into "the server was unsure".
        if let Some(stated) = confidence
            && stated < threshold
        {
            // The answer is still mapped below: a malformed value is a
            // protocol failure whether or not the model was unsure of it. Only
            // the pack's outcome changes — one unsure answer abstains the lot.
            unsure = true;
        }

        let value = match answer {
            WireAnswer::Choice { choice, .. } => {
                DecisionValue::Categorical { choice: choice.clone() }
            }
            WireAnswer::Score { score, .. } => {
                let levels = levels_for(question, default_levels)?;
                let width = (levels.len() - 1) as f64;
                if !score.is_finite() {
                    return Err(DecisionError::Protocol(format!(
                        "question '{}' answered with a non-finite score",
                        question.question_id
                    )));
                }
                // A finite score still has a range: it is an index into the
                // levels the question was asked with. Reading 9.0 on a
                // five-level scale as "highest" would turn a backend that
                // ignored the scale into a confident vote.
                if !(-SCORE_EPSILON..=width + SCORE_EPSILON).contains(score) {
                    return Err(DecisionError::Protocol(format!(
                        "question '{}' answered with score {score}, outside the 0..={width} \
                         level index it was asked with",
                        question.question_id
                    )));
                }
                // `score` is the expected level *index*; the protocol's ordinal
                // is a normalized 0..=1 with the direction the question named.
                DecisionValue::Ordinal { score: (score / width).clamp(0.0, 1.0) }
            }
            WireAnswer::Noul { noul, .. } => {
                if !noul.is_finite() {
                    return Err(DecisionError::Protocol(format!(
                        "question '{}' answered with a non-finite probability",
                        question.question_id
                    )));
                }
                // `noul` is P(true); the midpoint is the only threshold that
                // does not smuggle in a preference the caller never stated.
                DecisionValue::Binary { value: *noul > 0.5 }
            }
        };

        let mut decision = DecisionAnswer::new(question.question_id.clone(), value);
        decision.confidence = confidence;
        mapped.push(decision);
    }

    if unsure || mapped.is_empty() {
        return Ok(DecisionResult::abstained(pack));
    }
    Ok(DecisionResult::answered(pack, mapped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::decision::{DecisionFamily, DecisionOption, DecisionQuestion};

    fn choice_pack() -> QuestionPack {
        QuestionPack::new(
            DecisionFamily::new("model_route"),
            "v1",
            vec![DecisionQuestion::categorical(
                "tier",
                "Which tier?",
                vec![
                    DecisionOption::new("main", "the main tier"),
                    DecisionOption::new("fast", "the fast tier"),
                ],
            )],
        )
    }

    fn score_pack(levels: Vec<DecisionOption>) -> QuestionPack {
        QuestionPack::new(
            DecisionFamily::new("input_guard"),
            "v1",
            vec![DecisionQuestion {
                question_id: "risk".to_string(),
                kind: DecisionKind::Ordinal,
                prompt: "How risky?".to_string(),
                options: levels,
            }],
        )
    }

    #[test]
    fn a_choice_question_sends_its_options_keyed_by_id() {
        let questions = questions_of(&choice_pack(), &DEFAULT_ORDINAL_LEVELS).unwrap();
        let tier = &questions["tier"];
        assert_eq!(tier.kind, "choice");
        assert_eq!(tier.instructions, "Which tier?");
        assert_eq!(
            tier.criteria,
            Some(serde_json::json!({"main": "the main tier", "fast": "the fast tier"})),
            "the label is what the model reads; the id is what it answers with"
        );
    }

    #[test]
    fn a_score_question_sends_the_default_levels_when_it_names_none() {
        let questions = questions_of(&score_pack(vec![]), &DEFAULT_ORDINAL_LEVELS).unwrap();
        assert_eq!(
            questions["risk"].criteria,
            Some(serde_json::json!(["lowest", "low", "medium", "high", "highest"]))
        );
    }

    #[test]
    fn a_score_question_that_names_levels_uses_them() {
        let pack = score_pack(vec![
            DecisionOption::new("n", "none"),
            DecisionOption::new("l", "low"),
            DecisionOption::new("h", "high"),
        ]);
        let questions = questions_of(&pack, &DEFAULT_ORDINAL_LEVELS).unwrap();
        assert_eq!(questions["risk"].criteria, Some(serde_json::json!(["none", "low", "high"])));
    }

    #[test]
    fn a_binary_question_sends_no_criteria() {
        let pack = QuestionPack::new(
            DecisionFamily::new("input_guard"),
            "v1",
            vec![DecisionQuestion {
                question_id: "needs_tools".to_string(),
                kind: DecisionKind::Binary,
                prompt: "Does this need a tool?".to_string(),
                options: vec![],
            }],
        );
        let questions = questions_of(&pack, &DEFAULT_ORDINAL_LEVELS).unwrap();
        assert_eq!(questions["needs_tools"].criteria, None);
        assert_eq!(questions["needs_tools"].kind, "noul");
    }

    #[test]
    fn a_ranking_is_refused_as_a_payload_rather_than_skipped() {
        let pack = QuestionPack::new(
            DecisionFamily::new("tool_ranking"),
            "v1",
            vec![DecisionQuestion {
                question_id: "order".to_string(),
                kind: DecisionKind::Ranking,
                prompt: "Rank.".to_string(),
                options: vec![DecisionOption::new("a", "a")],
            }],
        );
        let error = questions_of(&pack, &DEFAULT_ORDINAL_LEVELS).unwrap_err();
        assert_eq!(error.kind(), "payload");
        assert!(error.to_string().contains("ranking"), "{error}");
    }

    #[test]
    fn a_categorical_question_with_no_options_is_refused() {
        let pack = QuestionPack::new(
            DecisionFamily::new("model_route"),
            "v1",
            vec![DecisionQuestion {
                question_id: "tier".to_string(),
                kind: DecisionKind::Categorical,
                prompt: "Which tier?".to_string(),
                options: vec![],
            }],
        );
        assert_eq!(questions_of(&pack, &DEFAULT_ORDINAL_LEVELS).unwrap_err().kind(), "payload");
    }

    #[test]
    fn a_repeated_question_id_is_refused() {
        let pack = QuestionPack::new(
            DecisionFamily::new("model_route"),
            "v1",
            vec![
                DecisionQuestion::categorical("tier", "a", vec![DecisionOption::new("m", "m")]),
                DecisionQuestion::categorical("tier", "b", vec![DecisionOption::new("f", "f")]),
            ],
        );
        assert_eq!(questions_of(&pack, &DEFAULT_ORDINAL_LEVELS).unwrap_err().kind(), "payload");
    }

    #[test]
    fn an_ordinal_question_with_one_level_is_refused_as_a_payload() {
        // One level has no direction, so there is no `score` to ask for and the
        // question never leaves this side. The shape is the caller's, so the
        // label is `payload` — not `protocol`, which would blame the backend for
        // a request it was never handed.
        let pack = score_pack(vec![DecisionOption::new("only", "only")]);
        let error = questions_of(&pack, &DEFAULT_ORDINAL_LEVELS).unwrap_err();
        assert_eq!(error.kind(), "payload");
        assert!(error.to_string().contains("ordinal"), "{error}");
    }

    #[test]
    fn a_ranking_that_reaches_the_answering_side_is_still_the_caller_s_shape() {
        // `answers_of` re-checks the kind defensively; a ranking it is handed
        // directly is the same pack-shape fault as in `questions_of`, so the
        // same label follows it. Read off the same error, not the same function.
        let pack = QuestionPack::new(
            DecisionFamily::new("tool_ranking"),
            "v1",
            vec![DecisionQuestion {
                question_id: "order".to_string(),
                kind: DecisionKind::Ranking,
                prompt: "Rank.".to_string(),
                options: vec![DecisionOption::new("a", "a")],
            }],
        );
        let answers = BTreeMap::from([(
            "order".to_string(),
            WireAnswer::Choice { choice: "a".to_string(), answer_confidence: Some(0.9) },
        )]);
        let error = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap_err();
        assert_eq!(error.kind(), "payload");
    }

    #[test]
    fn a_choice_answer_becomes_a_categorical_value_with_its_confidence() {
        let pack = choice_pack();
        let answers = BTreeMap::from([(
            "tier".to_string(),
            WireAnswer::Choice { choice: "main".to_string(), answer_confidence: Some(0.9) },
        )]);
        let result = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap();

        assert!(!result.abstained);
        assert_eq!(result.pack_id, pack.pack_id, "the pack is stamped by the adapter");
        assert_eq!(result.pack_version, pack.version);
        assert_eq!(
            result.answer_for("tier").unwrap().value,
            DecisionValue::Categorical { choice: "main".to_string() }
        );
        assert_eq!(result.answer_for("tier").unwrap().confidence, Some(0.9));
        assert!(pack.validate(&result).is_ok());
    }

    #[test]
    fn a_score_answer_is_normalized_from_its_level_index() {
        let pack = score_pack(vec![
            DecisionOption::new("0", "none"),
            DecisionOption::new("1", "low"),
            DecisionOption::new("2", "medium"),
            DecisionOption::new("3", "high"),
            DecisionOption::new("4", "severe"),
        ]);
        // The live server answered 0.9647 for a state it judged "very low";
        // that is the expected index near level 1 of five.
        let answers = BTreeMap::from([(
            "risk".to_string(),
            WireAnswer::Score { score: 0.9647, answer_confidence: Some(0.59) },
        )]);
        let result = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap();

        let DecisionValue::Ordinal { score } = result.answer_for("risk").unwrap().value else {
            panic!("a score question answers ordinally");
        };
        assert!((score - 0.9647 / 4.0).abs() < 1e-9, "{score}");
        assert!((0.0..=1.0).contains(&score));
        assert!(pack.validate(&result).is_ok());
    }

    #[test]
    fn a_score_is_clamped_when_the_levels_it_was_asked_with_are_fewer() {
        // The same index normalized against three levels: 2.0/2.0 = 1.0.
        let pack = score_pack(vec![
            DecisionOption::new("a", "a"),
            DecisionOption::new("b", "b"),
            DecisionOption::new("c", "c"),
        ]);
        let answers = BTreeMap::from([(
            "risk".to_string(),
            WireAnswer::Score { score: 2.0, answer_confidence: Some(0.9) },
        )]);
        let result = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap();
        assert_eq!(result.answer_for("risk").unwrap().value, DecisionValue::Ordinal { score: 1.0 });
    }

    #[test]
    fn a_score_outside_the_level_index_range_is_a_protocol_failure() {
        // The five default levels span the index range 0..=4.
        let pack = score_pack(vec![]);
        let answer = |score: f64| {
            BTreeMap::from([(
                "risk".to_string(),
                WireAnswer::Score { score, answer_confidence: Some(0.9) },
            )])
        };

        for score in [-3.0, -0.1, 4.1, 9.0] {
            let error =
                answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answer(score), 0.5).unwrap_err();
            assert_eq!(error.kind(), "protocol", "score {score} is not an index into 0..=4");
            assert!(error.to_string().contains("outside"), "{error}");
        }

        // A rounding step past the end is still the endpoint, not a fault.
        let noisy = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answer(4.0 + 1e-9), 0.5).unwrap();
        assert_eq!(noisy.answer_for("risk").unwrap().value, DecisionValue::Ordinal { score: 1.0 });
    }

    #[test]
    fn a_noul_answer_crosses_the_midpoint_into_a_boolean() {
        let pack = QuestionPack::new(
            DecisionFamily::new("input_guard"),
            "v1",
            vec![DecisionQuestion {
                question_id: "needs_tools".to_string(),
                kind: DecisionKind::Binary,
                prompt: "?".to_string(),
                options: vec![],
            }],
        );
        let answer = |noul: f64| {
            BTreeMap::from([(
                "needs_tools".to_string(),
                WireAnswer::Noul { noul, answer_confidence: Some(0.9) },
            )])
        };

        for (noul, expected) in [(0.0236, false), (0.5, false), (0.5001, true), (0.97, true)] {
            let result = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answer(noul), 0.5).unwrap();
            assert_eq!(
                result.answer_for("needs_tools").unwrap().value,
                DecisionValue::Binary { value: expected },
                "P(true) = {noul}"
            );
        }
    }

    #[test]
    fn an_unsure_answer_is_dropped_and_nothing_left_is_an_abstention() {
        let pack = choice_pack();
        let answers = BTreeMap::from([(
            "tier".to_string(),
            WireAnswer::Choice { choice: "fast".to_string(), answer_confidence: Some(0.31) },
        )]);

        let result = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap();
        assert!(result.abstained, "0.31 is below the 0.5 threshold");
        assert!(result.answers.is_empty());
        assert!(pack.validate(&result).is_ok(), "an abstention is a valid result");

        // At a threshold it clears, the same answer is served.
        let served = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.3).unwrap();
        assert!(!served.abstained);
    }

    #[test]
    fn one_unsure_answer_abstains_the_whole_pack() {
        // A two-question pack where only one answer clears the threshold. A
        // partial set recorded as `Answered` would be indistinguishable from a
        // complete one, so the pack abstains instead of carrying half an answer.
        let pack = QuestionPack::new(
            DecisionFamily::new("input_guard"),
            "v1",
            vec![
                DecisionQuestion::categorical(
                    "tier",
                    "Which tier?",
                    vec![
                        DecisionOption::new("main", "the main tier"),
                        DecisionOption::new("fast", "the fast tier"),
                    ],
                ),
                DecisionQuestion {
                    question_id: "needs_tools".to_string(),
                    kind: DecisionKind::Binary,
                    prompt: "Does this need a tool?".to_string(),
                    options: vec![],
                },
            ],
        );
        let answers = BTreeMap::from([
            (
                "tier".to_string(),
                WireAnswer::Choice { choice: "main".to_string(), answer_confidence: Some(0.9) },
            ),
            (
                "needs_tools".to_string(),
                WireAnswer::Noul { noul: 0.9, answer_confidence: Some(0.1) },
            ),
        ]);

        let result = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap();
        assert!(result.abstained, "one answer below the threshold abstains the pack");
        assert!(result.answers.is_empty(), "no partial answer is served");
        assert!(pack.validate(&result).is_ok(), "an abstention is a valid result");
    }

    #[test]
    fn an_unstated_confidence_is_not_treated_as_an_unsure_one() {
        let pack = choice_pack();
        let answers = BTreeMap::from([(
            "tier".to_string(),
            WireAnswer::Choice { choice: "main".to_string(), answer_confidence: None },
        )]);
        let result = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.99).unwrap();
        assert!(!result.abstained, "a missing number is not a low one");
    }

    #[test]
    fn a_wrongly_shaped_answer_is_a_protocol_failure() {
        let pack = choice_pack();
        let answers = BTreeMap::from([(
            "tier".to_string(),
            WireAnswer::Noul { noul: 0.9, answer_confidence: Some(0.9) },
        )]);
        let error = answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap_err();
        assert_eq!(error.kind(), "protocol");
    }

    #[test]
    fn a_missing_or_extra_answer_is_a_protocol_failure() {
        let pack = choice_pack();
        let empty = BTreeMap::new();
        assert_eq!(
            answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &empty, 0.5).unwrap_err().kind(),
            "protocol",
            "a question that was asked and not answered is not an abstention"
        );

        let extra = BTreeMap::from([(
            "not_asked".to_string(),
            WireAnswer::Choice { choice: "main".to_string(), answer_confidence: Some(0.9) },
        )]);
        assert_eq!(
            answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &extra, 0.5).unwrap_err().kind(),
            "protocol"
        );
    }

    #[test]
    fn a_non_finite_number_is_a_protocol_failure() {
        let pack = score_pack(vec![]);
        let answers = BTreeMap::from([(
            "risk".to_string(),
            WireAnswer::Score { score: f64::NAN, answer_confidence: Some(0.9) },
        )]);
        assert_eq!(
            answers_of(&pack, &DEFAULT_ORDINAL_LEVELS, &answers, 0.5).unwrap_err().kind(),
            "protocol"
        );
    }
}
