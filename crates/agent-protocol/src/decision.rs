//! Fast-decision protocol.
//!
//! A fast-decision backend is a model, but it is not the conversation model: it
//! answers one narrow, versioned [`QuestionPack`] over a bounded [`DecisionState`]
//! and returns general semantics — binary, categorical, ordinal, ranking — with
//! an optional confidence. Nothing vendor-specific crosses this boundary, which
//! is what lets a rule table, a local sidecar, or an LLM sit behind the same
//! port.
//!
//! Three properties are structural rather than conventional:
//!
//! - **A pack is versioned.** The result and the trace both carry the pack id and
//!   version it answered, so a decision made against an old question set is
//!   recognizable as such instead of being silently reinterpreted.
//! - **An answer is validated against its pack.** [`QuestionPack::validate`]
//!   rejects an answer for an unknown question, a value of the wrong kind, a
//!   category that is not one of the options, or a confidence outside `0..=1`.
//!   A backend that returns malformed output is a failed backend, not a source
//!   of truth.
//! - **An abstention is not an empty answer.** `abstained` is explicit, and an
//!   abstention carrying answers is inconsistent by construction — the caller
//!   never has to guess whether "no answers" meant "no opinion" or "broken".

use crate::ids::{SessionId, TurnId, fresh_id};
use crate::versioning::Version;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which decision family a question pack belongs to.
///
/// An open set: a family this build has never heard of round-trips unchanged,
/// so an adapter can introduce one without a protocol change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecisionFamily(pub String);

impl DecisionFamily {
    /// Which model tier a turn should use. The router keeps the final say.
    pub const MODEL_ROUTE: &'static str = "model_route";
    /// Whether an input carries risk. Only ever a signal; policy decides.
    pub const INPUT_GUARD: &'static str = "input_guard";
    /// Ordering advice for inbox items. Never widens permissions.
    pub const INBOX_PRIORITY: &'static str = "inbox_priority";
    /// Which memories are relevant. Never writes a fact.
    pub const MEMORY_RELEVANCE: &'static str = "memory_relevance";
    /// Reranking of already-authorized tool candidates. Never invents arguments.
    pub const TOOL_RANKING: &'static str = "tool_ranking";

    /// Create a family from its wire string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DecisionFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The shape of an answer, so a result can be checked without knowing the
/// question's prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Binary,
    Categorical,
    Ordinal,
    Ranking,
}

/// One choice a categorical or ranking question offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    /// Stable identifier. This is what an answer names, never the label.
    pub id: String,
    /// Human-facing text shown to a model; never parsed back.
    pub label: String,
}

impl DecisionOption {
    /// An option with an id and its display label.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into() }
    }
}

/// One question in a pack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionQuestion {
    pub question_id: String,
    pub kind: DecisionKind,
    /// The question as asked. Recorded in the pack, deliberately not copied
    /// into the trace: traces keep no unnecessary raw text.
    pub prompt: String,
    /// Options for `Categorical` and `Ranking`; empty otherwise.
    pub options: Vec<DecisionOption>,
}

impl DecisionQuestion {
    /// A categorical question over the given options.
    pub fn categorical(
        question_id: impl Into<String>,
        prompt: impl Into<String>,
        options: Vec<DecisionOption>,
    ) -> Self {
        Self {
            question_id: question_id.into(),
            kind: DecisionKind::Categorical,
            prompt: prompt.into(),
            options,
        }
    }

    /// The id of an option, if this question offers it.
    pub fn has_option(&self, id: &str) -> bool {
        self.options.iter().any(|option| option.id == id)
    }
}

/// A versioned set of questions asked together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionPack {
    pub pack_id: String,
    pub family: DecisionFamily,
    /// The question set's revision. Bump it whenever a question, its kind, or
    /// its options change: the trace records this, and a comparison across two
    /// different versions is not a comparison.
    pub version: String,
    pub questions: Vec<DecisionQuestion>,
}

impl QuestionPack {
    /// A pack over `questions`, with a generated id.
    pub fn new(
        family: DecisionFamily,
        version: impl Into<String>,
        questions: Vec<DecisionQuestion>,
    ) -> Self {
        Self { pack_id: fresh_id(), family, version: version.into(), questions }
    }

    /// The question ids, in order. This — not the prompts — is what a trace
    /// keeps.
    pub fn question_ids(&self) -> Vec<String> {
        self.questions.iter().map(|q| q.question_id.clone()).collect()
    }

    /// Check one result against this pack.
    ///
    /// Anything a backend returns must survive this before it can influence
    /// anything, so a malformed answer degrades to the rule baseline instead of
    /// being read as an opinion.
    pub fn validate(&self, result: &DecisionResult) -> Result<(), String> {
        if result.pack_id != self.pack_id {
            return Err(format!(
                "answer is for pack {} but this pack is {}",
                result.pack_id, self.pack_id
            ));
        }
        if result.pack_version != self.version {
            return Err(format!(
                "answer is for version {} but this pack is {}",
                result.pack_version, self.version
            ));
        }
        if result.abstained {
            return if result.answers.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "abstention carries {} answer(s); an abstention has no answers",
                    result.answers.len()
                ))
            };
        }
        if result.answers.is_empty() {
            return Err("a non-abstaining result carries no answers".to_string());
        }

        let mut seen: Vec<&str> = Vec::new();
        for answer in &result.answers {
            if seen.contains(&answer.question_id.as_str()) {
                return Err(format!("question {} answered twice", answer.question_id));
            }
            seen.push(&answer.question_id);

            let question = self
                .questions
                .iter()
                .find(|q| q.question_id == answer.question_id)
                .ok_or_else(|| format!("answer for unknown question {}", answer.question_id))?;

            if answer.value.kind() != question.kind {
                return Err(format!(
                    "question {} is {:?} but was answered as {:?}",
                    question.question_id,
                    question.kind,
                    answer.value.kind()
                ));
            }
            match &answer.value {
                DecisionValue::Categorical { choice } if !question.has_option(choice) => {
                    return Err(format!(
                        "question {} chose unknown option {choice}",
                        question.question_id
                    ));
                }
                DecisionValue::Ranking { order } => {
                    let mut ranked = order.clone();
                    ranked.sort();
                    ranked.dedup();
                    let mut options: Vec<String> =
                        question.options.iter().map(|o| o.id.clone()).collect();
                    options.sort();
                    if ranked.len() != order.len() || ranked != options {
                        return Err(format!(
                            "question {} ranked {order:?}, which is not a permutation of its options",
                            question.question_id
                        ));
                    }
                }
                DecisionValue::Ordinal { score } if !(0.0..=1.0).contains(score) => {
                    return Err(format!(
                        "question {} scored {score}, outside 0..=1",
                        question.question_id
                    ));
                }
                _ => {}
            }
            if let Some(confidence) = answer.confidence
                && !(0.0..=1.0).contains(&confidence)
            {
                return Err(format!(
                    "question {} reported confidence {confidence}, outside 0..=1",
                    question.question_id
                ));
            }
        }
        Ok(())
    }
}

/// A general decision value: no model-specific primitive reaches this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionValue {
    Binary {
        value: bool,
    },
    Categorical {
        choice: String,
    },
    /// A normalized score in `0..=1`, where the direction is the question's.
    Ordinal {
        score: f64,
    },
    /// Option ids, best first.
    Ranking {
        order: Vec<String>,
    },
}

impl DecisionValue {
    /// Which question shape this value answers.
    pub fn kind(&self) -> DecisionKind {
        match self {
            Self::Binary { .. } => DecisionKind::Binary,
            Self::Categorical { .. } => DecisionKind::Categorical,
            Self::Ordinal { .. } => DecisionKind::Ordinal,
            Self::Ranking { .. } => DecisionKind::Ranking,
        }
    }
}

/// One answer, with the backend's stated probability for it when it gives one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionAnswer {
    pub question_id: String,
    pub value: DecisionValue,
    /// `None` means the backend stated no probability — not zero confidence.
    pub confidence: Option<f64>,
}

impl DecisionAnswer {
    /// An answer with no stated confidence.
    pub fn new(question_id: impl Into<String>, value: DecisionValue) -> Self {
        Self { question_id: question_id.into(), value, confidence: None }
    }

    /// The same answer, with the backend's probability that it is right.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = Some(confidence);
        self
    }
}

/// What a backend returned for one pack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResult {
    pub pack_id: String,
    pub pack_version: String,
    pub answers: Vec<DecisionAnswer>,
    /// The backend declined to answer. Explicit, so "no opinion" is never
    /// inferred from an empty answer list.
    pub abstained: bool,
}

impl DecisionResult {
    /// A result answering a pack.
    pub fn answered(pack: &QuestionPack, answers: Vec<DecisionAnswer>) -> Self {
        Self {
            pack_id: pack.pack_id.clone(),
            pack_version: pack.version.clone(),
            answers,
            abstained: false,
        }
    }

    /// A result declining to answer a pack.
    pub fn abstained(pack: &QuestionPack) -> Self {
        Self {
            pack_id: pack.pack_id.clone(),
            pack_version: pack.version.clone(),
            answers: Vec::new(),
            abstained: true,
        }
    }

    /// The answer to one question, if this result has one.
    pub fn answer_for(&self, question_id: &str) -> Option<&DecisionAnswer> {
        self.answers.iter().find(|a| a.question_id == question_id)
    }
}

/// How much a decision is allowed to influence execution.
///
/// Promotion from `Shadow` is per family and gated on revision pinning,
/// held-out evaluation, calibration, latency caps, and rollback drills. Only
/// `Shadow` is reachable in this build; the other two exist so the value is
/// recorded honestly wherever a mode is written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    /// Observed and traced; never acted on.
    Shadow,
    /// Returned to the caller, which decides whether to use it.
    Advisory,
    /// Acted on within an allowlist of actions.
    Bounded,
}

impl DecisionMode {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Advisory => "advisory",
            Self::Bounded => "bounded",
        }
    }
}

impl std::fmt::Display for DecisionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one backend did within one broker call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DecisionBackendOutcome {
    /// The backend returned a well-formed, non-abstaining result.
    Answered(DecisionResult),
    /// The backend declined to answer.
    Abstained,
    /// The backend failed. `error_kind` is a stable machine-readable label
    /// (`timeout`, `unavailable`, `protocol`, ...), not the backend's message.
    Failed { error_kind: String },
}

impl DecisionBackendOutcome {
    /// Whether this outcome carries an answer.
    pub fn is_answered(&self) -> bool {
        matches!(self, Self::Answered(_))
    }
}

/// One backend's entry in a trace: what it said and how long it took.
///
/// Every backend that ran is recorded, including the ones whose answer was not
/// used. That is what makes shadow comparison possible at all — a trace that
/// kept only the winner would have nothing to compare with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionBackendReport {
    pub backend_id: String,
    /// Model revision or artifact hash, when the backend pins one. A decision
    /// that cannot be attributed to a revision cannot be replayed.
    pub revision: Option<String>,
    pub outcome: DecisionBackendOutcome,
    pub latency_ms: u64,
}

/// The durable record of one decision call.
///
/// It carries the pack id and version, the question ids, and every backend's
/// outcome — but not the question prose, the state, or any raw input text. A
/// trace is meant to be replayable and auditable without becoming a second copy
/// of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionTrace {
    /// The trace shape's schema version (YA-21).
    ///
    /// Absent means current: traces were persisted before this field existed.
    /// The reader refuses a newer trace rather than replaying fields whose
    /// meaning it does not know, because a trace that replays "successfully"
    /// under the wrong rules is worse than one that is refused.
    #[serde(default = "crate::versioning::current_decision_trace")]
    pub schema_version: Version,
    pub trace_id: String,
    pub created_at: DateTime<Utc>,
    pub family: DecisionFamily,
    pub mode: DecisionMode,
    pub pack_id: String,
    pub pack_version: String,
    /// The questions asked, by id.
    pub question_ids: Vec<String>,
    /// Every backend that ran, in the order it ran. The rule baseline is one of
    /// them.
    pub backends: Vec<DecisionBackendReport>,
    /// Which backend's answer was settled on; `None` when none answered.
    pub chosen_backend: Option<String>,
    pub chosen: Option<DecisionResult>,
    /// Set when no backend report was obtained: the call never reached the
    /// backends (a state that could not be built, for instance), or it was cut
    /// off before one came back. `None` means every backend was asked, so a
    /// missing `chosen` with no `aborted` reads as "nobody had an opinion"
    /// rather than "nothing ran".
    pub aborted: Option<String>,
    /// Wall clock for the whole call, including the backends that lost.
    pub latency_ms: u64,
}

impl DecisionTrace {
    /// An empty trace for one pack.
    pub fn new(pack: &QuestionPack, mode: DecisionMode) -> Self {
        Self {
            schema_version: crate::versioning::formats::DECISION_TRACE,
            trace_id: fresh_id(),
            created_at: Utc::now(),
            family: pack.family.clone(),
            mode,
            pack_id: pack.pack_id.clone(),
            pack_version: pack.version.clone(),
            question_ids: pack.question_ids(),
            backends: Vec::new(),
            chosen_backend: None,
            chosen: None,
            aborted: None,
            latency_ms: 0,
        }
    }

    /// The report for one backend, if it ran.
    pub fn report_for(&self, backend_id: &str) -> Option<&DecisionBackendReport> {
        self.backends.iter().find(|r| r.backend_id == backend_id)
    }
}

/// The largest `facts` object a backend may be handed.
///
/// A fast decision answers one narrow question; state larger than this is a
/// context dump, and dumping context into a side model is the main model's job,
/// not a side channel's. The cap is a fail-closed guard, not a tuning knob.
pub const MAX_FACTS_BYTES: usize = 8192;

/// The bounded state a decision is made over.
///
/// Every fact here must be derivable from durable state or from configuration —
/// never content that exists only in this process's memory. A decision backend
/// is still a model, and a model that sees something no event accounts for has
/// seen something unreplayable. Only the size is enforced today; an
/// allowlist of fact keys becomes meaningful once a family has a consumer for
/// one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionState {
    pub session_id: SessionId,
    /// `None` for a decision taken outside any turn.
    pub turn_id: Option<TurnId>,
    /// The durable log position the facts were read from.
    pub context_revision: u64,
    pub facts: serde_json::Map<String, serde_json::Value>,
}

/// Why a `DecisionState` could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecisionStateError {
    #[error("decision state is {bytes} bytes, over the {limit}-byte limit")]
    TooLarge { bytes: usize, limit: usize },
    #[error("decision state could not be measured: {0}")]
    Unmeasurable(String),
}

impl DecisionStateError {
    /// A stable machine-readable reason, for the trace.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "state_too_large",
            Self::Unmeasurable(_) => "state_unmeasurable",
        }
    }
}

impl DecisionState {
    /// A state over `facts`, failing closed when the facts exceed
    /// [`MAX_FACTS_BYTES`].
    ///
    /// The check is here, at construction, so an oversized state can never
    /// reach a backend: no backend is asked, and no call is paid for.
    pub fn new(
        session_id: SessionId,
        turn_id: Option<TurnId>,
        context_revision: u64,
        facts: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, DecisionStateError> {
        let bytes = serde_json::to_vec(&facts)
            .map_err(|e| DecisionStateError::Unmeasurable(e.to_string()))?
            .len();
        if bytes > MAX_FACTS_BYTES {
            return Err(DecisionStateError::TooLarge { bytes, limit: MAX_FACTS_BYTES });
        }
        Ok(Self { session_id, turn_id, context_revision, facts })
    }

    /// One fact, if present.
    pub fn fact(&self, key: &str) -> Option<&serde_json::Value> {
        self.facts.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str) -> DecisionOption {
        DecisionOption::new(id, id)
    }

    fn route_pack() -> QuestionPack {
        QuestionPack::new(
            DecisionFamily::new(DecisionFamily::MODEL_ROUTE),
            "v1",
            vec![DecisionQuestion::categorical(
                "tier",
                "Which tier should serve this turn?",
                vec![option("main"), option("fast"), option("thinking")],
            )],
        )
    }

    #[test]
    fn a_pack_and_its_result_round_trip_through_serde() {
        let pack = route_pack();
        let decoded: QuestionPack =
            serde_json::from_str(&serde_json::to_string(&pack).unwrap()).unwrap();
        assert_eq!(decoded, pack);

        let result = DecisionResult::answered(
            &pack,
            vec![
                DecisionAnswer::new(
                    "tier",
                    DecisionValue::Categorical { choice: "fast".to_string() },
                )
                .with_confidence(0.62),
            ],
        );
        let decoded: DecisionResult =
            serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
        assert_eq!(decoded, result);
        assert!(pack.validate(&result).is_ok());
    }

    #[test]
    fn an_unknown_family_round_trips_unchanged() {
        let family = DecisionFamily::new("some_future_family");
        let decoded: DecisionFamily =
            serde_json::from_str(&serde_json::to_string(&family).unwrap()).unwrap();
        assert_eq!(decoded.as_str(), "some_future_family");
        assert_ne!(decoded.as_str(), DecisionFamily::MODEL_ROUTE);
    }

    #[test]
    fn an_abstention_carries_no_answers() {
        let pack = route_pack();
        let abstained = DecisionResult::abstained(&pack);
        assert!(abstained.abstained);
        assert!(abstained.answers.is_empty());
        assert!(pack.validate(&abstained).is_ok());

        let mut inconsistent = abstained.clone();
        inconsistent.answers.push(DecisionAnswer::new(
            "tier",
            DecisionValue::Categorical { choice: "main".to_string() },
        ));
        assert!(pack.validate(&inconsistent).is_err());
    }

    #[test]
    fn a_non_abstaining_result_with_no_answers_is_rejected() {
        let pack = route_pack();
        let empty = DecisionResult::answered(&pack, Vec::new());
        assert!(pack.validate(&empty).is_err());
    }

    #[test]
    fn an_answer_of_the_wrong_kind_is_rejected() {
        let pack = route_pack();
        let wrong = DecisionResult::answered(
            &pack,
            vec![DecisionAnswer::new("tier", DecisionValue::Binary { value: true })],
        );
        let reason = pack.validate(&wrong).unwrap_err();
        assert!(reason.contains("Binary"), "{reason}");
    }

    #[test]
    fn an_answer_for_an_unknown_question_is_rejected() {
        let pack = route_pack();
        let stray = DecisionResult::answered(
            &pack,
            vec![DecisionAnswer::new(
                "not_asked",
                DecisionValue::Categorical { choice: "main".to_string() },
            )],
        );
        assert!(pack.validate(&stray).is_err());
    }

    #[test]
    fn a_choice_outside_the_options_is_rejected() {
        let pack = route_pack();
        let invented = DecisionResult::answered(
            &pack,
            vec![DecisionAnswer::new(
                "tier",
                DecisionValue::Categorical { choice: "gpt-9".to_string() },
            )],
        );
        assert!(pack.validate(&invented).is_err());
    }

    #[test]
    fn a_ranking_is_checked_as_a_permutation() {
        let pack = QuestionPack::new(
            DecisionFamily::new(DecisionFamily::TOOL_RANKING),
            "v1",
            vec![DecisionQuestion {
                question_id: "order".to_string(),
                kind: DecisionKind::Ranking,
                prompt: "Rank the candidates.".to_string(),
                options: vec![option("a"), option("b")],
            }],
        );

        let good = DecisionResult::answered(
            &pack,
            vec![DecisionAnswer::new(
                "order",
                DecisionValue::Ranking { order: vec!["b".to_string(), "a".to_string()] },
            )],
        );
        assert!(pack.validate(&good).is_ok());

        let short = DecisionResult::answered(
            &pack,
            vec![DecisionAnswer::new(
                "order",
                DecisionValue::Ranking { order: vec!["b".to_string(), "b".to_string()] },
            )],
        );
        assert!(pack.validate(&short).is_err());
    }

    #[test]
    fn a_confidence_outside_zero_to_one_is_rejected() {
        let pack = route_pack();
        let overconfident = DecisionResult::answered(
            &pack,
            vec![
                DecisionAnswer::new(
                    "tier",
                    DecisionValue::Categorical { choice: "main".to_string() },
                )
                .with_confidence(1.4),
            ],
        );
        assert!(pack.validate(&overconfident).is_err());
    }

    #[test]
    fn a_result_answering_a_different_pack_is_rejected() {
        let pack = route_pack();
        let mut elsewhere = DecisionResult::answered(&pack, Vec::new());
        elsewhere.pack_id = uuid::Uuid::new_v4().to_string();
        assert!(pack.validate(&elsewhere).is_err());

        let mut other_version = pack.clone();
        other_version.version = "v2".to_string();
        let stale = DecisionResult::answered(
            &pack,
            vec![DecisionAnswer::new(
                "tier",
                DecisionValue::Categorical { choice: "main".to_string() },
            )],
        );
        assert!(other_version.validate(&stale).is_err());
    }

    #[test]
    fn a_state_over_the_cap_fails_closed() {
        let mut facts = serde_json::Map::new();
        facts.insert("padding".to_string(), serde_json::Value::String("x".repeat(MAX_FACTS_BYTES)));

        let error = DecisionState::new(SessionId::from_string("s"), None, 0, facts).unwrap_err();
        match error {
            DecisionStateError::TooLarge { bytes, limit } => {
                assert!(bytes > limit);
                assert_eq!(limit, MAX_FACTS_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }

        let mut facts = serde_json::Map::new();
        facts.insert("padding".to_string(), serde_json::Value::String("x".repeat(MAX_FACTS_BYTES)));
        let error = DecisionState::new(SessionId::from_string("s"), None, 0, facts).unwrap_err();
        assert_eq!(error.kind(), "state_too_large");
    }

    #[test]
    fn a_state_at_the_cap_is_accepted() {
        let mut facts = serde_json::Map::new();
        // `{"k":"..."}`; the JSON envelope is 9 bytes around the value.
        facts.insert("k".to_string(), serde_json::Value::String("x".repeat(MAX_FACTS_BYTES - 9)));
        let state =
            DecisionState::new(SessionId::from_string("s"), None, 7, facts.clone()).unwrap();
        assert_eq!(state.fact("k"), facts.get("k"));
        assert_eq!(state.context_revision, 7);
    }

    #[test]
    fn a_trace_keeps_question_ids_and_not_prompts() {
        let pack = route_pack();
        let trace = DecisionTrace::new(&pack, DecisionMode::Shadow);

        assert_eq!(trace.question_ids, vec!["tier".to_string()]);
        assert_eq!(trace.pack_version, pack.version);
        assert!(trace.chosen.is_none());
        assert!(trace.chosen_backend.is_none());
        assert!(trace.aborted.is_none());

        let encoded = serde_json::to_string(&trace).unwrap();
        assert!(
            !encoded.contains("Which tier"),
            "a trace must not copy the question prose: {encoded}"
        );
    }
}
