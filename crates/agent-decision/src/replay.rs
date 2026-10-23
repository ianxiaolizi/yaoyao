//! Reading a recorded decision back.
//!
//! A trace is the durable record of one decision call, and the reason it exists
//! is that a decision which cannot be audited after the fact is a decision
//! nobody can be held to. "Audited" has to mean something checkable, so this
//! module supplies the check: take a trace, ignore its summary fields, and
//! re-derive the conclusion from the reports underneath using the same rule the
//! broker used when it first ran — [`crate::broker::settle_into`], not a copy of
//! it.
//!
//! What this proves, and what it does not:
//!
//! - **Proved.** `chosen_backend` and `chosen` are the ones the record implies.
//!   A trace whose summary disagrees with its own reports is a trace that cannot
//!   be read, and a reader learns that here rather than by believing it.
//! - **Proved.** Nothing was recorded as answered that contradicts the shape of
//!   an answer: no abstention wearing an `Answered` label, no answer about a
//!   different pack, no answer to a question the trace does not list.
//! - **Not proved.** That the answers are *correct*. Re-checking them against
//!   the questions needs the pack, and a trace deliberately carries the pack id
//!   and version but not the questions themselves — so a caller holding the pack
//!   compares [`Replayed`] against it, and a caller holding only the trace can
//!   still catch a trace that contradicts itself.
//! - **Not proved.** That the backends were honest. A report is what a backend
//!   said, not what it computed; the revision pin on each report is what makes
//!   that attribution reproducible elsewhere, and it is not this function's job
//!   to go and ask again.

use agent_ports::decision::{DecisionBackendOutcome, DecisionResult, DecisionTrace};
use agent_protocol::versioning::{self, VersionError, formats};

use crate::broker::settle_into;

/// The choice a trace's own reports yield.
///
/// Returned by [`replay`], and equal to the trace's `chosen_backend`/`chosen`
/// in every trace [`verify`] accepts.
#[derive(Debug, Clone, PartialEq)]
pub struct Replayed {
    pub chosen_backend: Option<String>,
    pub chosen: Option<DecisionResult>,
}

/// Why a trace did not replay.
///
/// Every variant is a contradiction inside one record — the trace and its own
/// reports saying different things — never a judgement about whether a backend
/// was right.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayMismatch {
    /// The trace's schema version cannot be read by this build (YA-21).
    ///
    /// Checked before replay: replaying a trace under rules its author did not
    /// write is a confident answer built on another version's meanings, which
    /// is worse than a refusal.
    #[error(transparent)]
    UnsupportedVersion(#[from] VersionError),
    /// The recorded winner is not the backend the reports hand the win to.
    #[error(
        "the trace records {recorded:?} as the chosen backend, but its own reports select {replayed:?}"
    )]
    ChosenBackendNotReproduced { recorded: Option<String>, replayed: Option<String> },
    /// The recorded answer is not the one its backend's report carries.
    #[error(
        "the answer recorded as chosen is not the one its report carries (backend {backend:?})"
    )]
    ChosenAnswerNotTheReport { backend: Option<String> },
    /// `aborted` means no backend was reached; reports say otherwise.
    #[error("the trace is aborted yet carries {reports} backend report(s)")]
    AbortedWithReports { reports: usize },
    /// An `Answered` outcome whose payload says it abstained: the broker records
    /// an abstention as `Abstained`, so one wearing this label was not produced
    /// by the broker.
    #[error("backend {backend} is recorded as answered but its result abstains")]
    AbstentionRecordedAsAnAnswer { backend: String },
    /// An answer about a pack or version other than the one the trace names.
    #[error(
        "backend {backend} answered pack {pack_id}@{pack_version}, but the trace is about another"
    )]
    AnswerIsNotAboutThisPack { backend: String, pack_id: String, pack_version: String },
    /// An answer to a question the trace does not list as asked.
    #[error("backend {backend} answered {question_id}, which the trace does not list as asked")]
    AnswerToAnUnaskedQuestion { backend: String, question_id: String },
}

/// Re-derive the conclusion from the reports alone.
///
/// A caller holding a trace but not trusting its summary (a reader of an old
/// history, a checker comparing two builds' traces) can ask what the record
/// actually says.
pub fn replay(trace: &DecisionTrace) -> Replayed {
    let mut chosen = None;
    let mut chosen_backend = None;
    for report in &trace.backends {
        settle_into(&mut chosen, &mut chosen_backend, report);
    }
    Replayed { chosen_backend, chosen }
}

/// Check that a trace's conclusion is the one its own record yields.
///
/// The checks are ordered from the record's shape to its contents, so the first
/// error reported is the most structural one.
pub fn verify(trace: &DecisionTrace) -> Result<(), ReplayMismatch> {
    // The version gate comes first: every check below reads the record's
    // fields, and a trace from a future build may use them differently.
    versioning::check(&trace.schema_version, &formats::DECISION_TRACE)?;

    // An abort means the backends were never reached. A trace with both an
    // abort and reports is not a call that happened, and the distinction is the
    // one thing `aborted` exists to carry: "nothing ran" must not read as
    // "everybody declined".
    if trace.aborted.is_some() && !trace.backends.is_empty() {
        return Err(ReplayMismatch::AbortedWithReports { reports: trace.backends.len() });
    }

    let derived = replay(trace);
    if derived.chosen_backend != trace.chosen_backend {
        return Err(ReplayMismatch::ChosenBackendNotReproduced {
            recorded: trace.chosen_backend.clone(),
            replayed: derived.chosen_backend,
        });
    }
    if derived.chosen != trace.chosen {
        return Err(ReplayMismatch::ChosenAnswerNotTheReport {
            backend: trace.chosen_backend.clone(),
        });
    }

    for report in &trace.backends {
        let DecisionBackendOutcome::Answered(result) = &report.outcome else {
            continue;
        };
        if result.abstained {
            return Err(ReplayMismatch::AbstentionRecordedAsAnAnswer {
                backend: report.backend_id.clone(),
            });
        }
        if result.pack_id != trace.pack_id || result.pack_version != trace.pack_version {
            return Err(ReplayMismatch::AnswerIsNotAboutThisPack {
                backend: report.backend_id.clone(),
                pack_id: result.pack_id.clone(),
                pack_version: result.pack_version.clone(),
            });
        }
        for answer in &result.answers {
            if !trace.question_ids.contains(&answer.question_id) {
                return Err(ReplayMismatch::AnswerToAnUnaskedQuestion {
                    backend: report.backend_id.clone(),
                    question_id: answer.question_id.clone(),
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::{RuleDecision, RuleDecisionProvider};
    use agent_ports::decision::{
        DecisionAnswer, DecisionBackendReport, DecisionError, DecisionFamily, DecisionMode,
        DecisionValue, FastDecisionProvider, QuestionPack,
    };
    use agent_testkit::{FakeDecisionBehaviour, FakeDecisionProvider};
    use std::sync::Arc;

    use crate::broker::DecisionBroker;
    use crate::model_route;

    const TIER: &str = model_route::TIER_QUESTION;

    fn pack() -> QuestionPack {
        model_route::pack()
    }

    async fn trace_with(
        backends: Vec<Arc<dyn FastDecisionProvider>>,
    ) -> agent_ports::decision::DecisionTrace {
        let baseline = Arc::new(RuleDecisionProvider::new(vec![RuleDecision::from_fact(
            DecisionFamily::MODEL_ROUTE,
            TIER,
            model_route::CHOSEN_TIER_FACT,
        )]));
        let mut broker = DecisionBroker::new(DecisionMode::Shadow, baseline).expect("shadow");
        for backend in backends {
            broker = broker.with_backend(backend);
        }

        let state = model_route::state(
            &agent_ports::model_router::RouteRequest {
                session_id: agent_protocol::ids::SessionId::from_string("s"),
                turn_id: agent_protocol::ids::TurnId::from_string("t"),
                task: agent_ports::model_router::TaskClass::new(
                    agent_ports::model_router::TaskClass::INTERACTIVE,
                ),
                context_revision: agent_protocol::ids::EventSeq::new(3),
                message_count: 1,
                required_capabilities: Vec::new(),
                budget: agent_ports::model_router::ModelBudget::default(),
            },
            &agent_ports::model_router::ModelRoute::new(
                agent_ports::model_router::ModelTier::Main,
                "openai-compat",
                "m",
            ),
        )
        .expect("state is small");

        broker.decide(&pack(), &state).await
    }

    #[tokio::test]
    async fn a_live_trace_replays_to_its_own_conclusion() {
        let backend = Arc::new(FakeDecisionProvider::answering(
            "local",
            TIER,
            DecisionValue::Categorical { choice: "fast".to_string() },
        ));
        let trace = trace_with(vec![backend]).await;

        assert_eq!(trace.chosen_backend.as_deref(), Some("local"));
        let derived = replay(&trace);
        assert_eq!(derived.chosen_backend.as_deref(), Some("local"));
        assert_eq!(derived.chosen, trace.chosen);
        verify(&trace).expect("a trace the broker wrote replays");
    }

    #[tokio::test]
    async fn a_trace_that_only_the_baseline_answered_replays_too() {
        // The degradation path is the one acceptance cares about, so it must be
        // as checkable as the happy one.
        let backend = Arc::new(FakeDecisionProvider::new(
            "local",
            FakeDecisionBehaviour::Fail(DecisionError::Unavailable("down".to_string())),
        ));
        let trace = trace_with(vec![backend]).await;

        assert_eq!(trace.chosen_backend.as_deref(), Some("rule"));
        verify(&trace).expect("degrading to the baseline is a conclusion like any other");
    }

    #[tokio::test]
    async fn an_aborted_trace_replays_to_nothing() {
        let trace = trace_with(Vec::new()).await;
        let mut aborted = trace.clone();
        aborted.backends.clear();
        aborted.chosen = None;
        aborted.chosen_backend = None;
        aborted.aborted = Some("state_too_large".to_string());

        assert_eq!(replay(&aborted).chosen_backend, None);
        verify(&aborted).expect("nothing ran and the trace says so");

        // The same trace with a report left in it is a contradiction.
        aborted.backends = trace.backends;
        assert!(matches!(
            verify(&aborted),
            Err(ReplayMismatch::AbortedWithReports { reports }) if reports > 0
        ));
    }

    #[tokio::test]
    async fn a_summary_that_disagrees_with_the_reports_is_caught() {
        let trace = trace_with(Vec::new()).await;

        let mut renamed = trace.clone();
        renamed.chosen_backend = Some("somebody-else".to_string());
        assert!(matches!(verify(&renamed), Err(ReplayMismatch::ChosenBackendNotReproduced { .. })));

        // The subtler edit: the right backend, but an answer it did not give.
        let mut substituted = trace.clone();
        let mut answer = substituted.chosen.clone().expect("the baseline answered");
        for a in &mut answer.answers {
            a.value = DecisionValue::Categorical { choice: "thinking".to_string() };
        }
        substituted.chosen = Some(answer);
        assert_eq!(
            verify(&substituted),
            Err(ReplayMismatch::ChosenAnswerNotTheReport { backend: Some("rule".to_string()) })
        );
    }

    /// Write a trace's summary back from its reports, so that a test mutating a
    /// report reaches the content checks instead of tripping the structural one
    /// first. This is the shape of a trace produced by something other than the
    /// broker: self-consistent, and still wrong.
    fn resummarize(trace: &mut agent_ports::decision::DecisionTrace) {
        let derived = replay(trace);
        trace.chosen_backend = derived.chosen_backend;
        trace.chosen = derived.chosen;
    }

    #[tokio::test]
    async fn an_answer_about_another_pack_or_question_is_caught() {
        let trace = trace_with(Vec::new()).await;

        let mut elsewhere = trace.clone();
        let report = elsewhere.backends.first_mut().expect("the baseline ran");
        let DecisionBackendOutcome::Answered(result) = &mut report.outcome else {
            panic!("the baseline answered");
        };
        result.pack_version = "v0-somebody-elses".to_string();
        resummarize(&mut elsewhere);
        assert!(matches!(verify(&elsewhere), Err(ReplayMismatch::AnswerIsNotAboutThisPack { .. })));

        let mut stray = trace_with(Vec::new()).await;
        let report = stray.backends.first_mut().expect("the baseline ran");
        let DecisionBackendOutcome::Answered(result) = &mut report.outcome else {
            panic!("the baseline answered");
        };
        result.answers.push(DecisionAnswer::new(
            "never_asked",
            DecisionValue::Categorical { choice: "main".to_string() },
        ));
        resummarize(&mut stray);
        assert_eq!(
            verify(&stray),
            Err(ReplayMismatch::AnswerToAnUnaskedQuestion {
                backend: "rule".to_string(),
                question_id: "never_asked".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn an_abstention_wearing_an_answer_label_is_caught() {
        let trace = trace_with(Vec::new()).await;

        let mut wrong = trace.clone();
        let report = wrong.backends.first_mut().expect("the baseline ran");
        let DecisionBackendOutcome::Answered(result) = &mut report.outcome else {
            panic!("the baseline answered");
        };
        result.abstained = true;
        result.answers.clear();
        resummarize(&mut wrong);

        assert_eq!(
            verify(&wrong),
            Err(ReplayMismatch::AbstentionRecordedAsAnAnswer { backend: "rule".to_string() })
        );
    }

    #[tokio::test]
    async fn a_persisted_trace_still_replays_after_a_round_trip() {
        // The reader of a trace is a later process holding JSON, not the one
        // that made it, so the check has to survive the encoding.
        let trace = trace_with(Vec::new()).await;
        let json = serde_json::to_string(&trace).unwrap();
        let decoded: agent_ports::decision::DecisionTrace = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, trace);
        verify(&decoded).expect("decoding does not change what the record says");
    }

    #[tokio::test]
    async fn a_trace_from_a_future_schema_is_refused_rather_than_replayed() {
        let trace = trace_with(Vec::new()).await;

        let mut future = trace.clone();
        future.schema_version = agent_protocol::versioning::Version::parse("99").unwrap();
        assert!(
            matches!(
                verify(&future),
                Err(ReplayMismatch::UnsupportedVersion(VersionError::Future { .. }))
            ),
            "a future trace must not reach the replay rules, and must be named as future"
        );

        // A trace written before the field existed reads as current: the JSON
        // has no `schema_version`, and that is the shape v1 wrote.
        let mut json = serde_json::to_value(&trace).unwrap();
        json.as_object_mut().unwrap().remove("schema_version");
        let old: agent_ports::decision::DecisionTrace = serde_json::from_value(json).unwrap();
        assert_eq!(old.schema_version, formats::DECISION_TRACE);
        verify(&old).expect("a pre-versioning trace is still a trace of this shape");
    }

    #[test]
    fn a_report_that_failed_is_not_an_answer() {
        // A report whose outcome is a failure is skipped, not guessed at: the
        // failure kinds are the broker's vocabulary, not this module's.
        let mut trace = agent_ports::decision::DecisionTrace::new(&pack(), DecisionMode::Shadow);
        trace.backends.push(DecisionBackendReport {
            backend_id: "local".to_string(),
            revision: None,
            outcome: DecisionBackendOutcome::Failed { error_kind: "unavailable".to_string() },
            latency_ms: 1,
        });

        assert_eq!(replay(&trace).chosen_backend, None);
        verify(&trace).expect("nobody answering is a conclusion, not a mismatch");
    }
}
