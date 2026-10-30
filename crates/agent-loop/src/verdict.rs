//! What the turn's last response says, and which rule the verdict names.
//!
//! The choice is a claim about evidence, so it is made in one place instead of
//! being spread through the loop's control flow: a response the provider ended
//! before the model finished is not an answer at all, and it must not be
//! described by any of the rules that say what an answer rests on. Two endings
//! are that shape and they are read here together — the output limit cut the
//! response off, or the content filter withheld it. On 2026-10-30 a reasoning
//! model spent the whole 2,000-token default on reasoning tokens, returned no
//! content with `finish_reason: length`, and this branch called it a final
//! answer: exit 0, `final_answer` in `metrics`, and `(no text)` in `why`.

use agent_ports::agent_loop::StopReason;
use agent_protocol::model::{FinishReason, ModelResponse};

use crate::context_layers::truncate;

/// Why the provider ended the response before the model finished.
///
/// One enum for the endings that are not the model's choice, because they share
/// the whole of what the turn does about them: none of the calls in it is
/// dispatched, the turn ends on the provider's ending, and the verdict names
/// that ending instead of an answer rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Halt {
    /// The response ran into the run's `max_output_tokens`.
    OutputLimit,
    /// The provider refused to continue generating (its content filter).
    ContentFilter,
}

impl Halt {
    /// Read the halt off a response; `None` is the ordinary case — the model
    /// stopped, or proposed calls that arrived whole.
    pub(crate) fn of(response: &ModelResponse) -> Option<Self> {
        match response.finish_reason.as_str() {
            FinishReason::LENGTH => Some(Self::OutputLimit),
            FinishReason::CONTENT_FILTER => Some(Self::ContentFilter),
            // Every other ending — `stop`, `tool_calls`, a provider's own word,
            // a missing one — is read as the model's own stop. That is the
            // reading this build has always had, and widening it would turn
            // providers that omit the field into failures.
            _ => None,
        }
    }
}

/// How the response that ended the turn should be read.
pub(crate) struct Facts {
    /// The provider ended the response before the model finished, if it did.
    pub halt: Option<Halt>,
    /// The limit that was in force, for the verdict to name.
    pub output_limit: Option<u32>,
    /// Calls the response proposed that the halt kept from being dispatched.
    pub undispatched_calls: u32,
    /// The reading asked for an action and no call was dispatched.
    pub unmet_action: bool,
    /// A check the request named completed after the work.
    pub criterion_observed: bool,
    /// The turn switched posture to repair after a real failure.
    pub repair_after_failure: bool,
}

impl Facts {
    /// A halt with nothing else to weigh: the provider ended the response, so
    /// no posture, criterion or repair history can make it an answer.
    pub(crate) fn halted(halt: Halt, output_limit: Option<u32>, undispatched_calls: u32) -> Self {
        Self {
            halt: Some(halt),
            output_limit,
            undispatched_calls,
            unmet_action: false,
            criterion_observed: false,
            repair_after_failure: false,
        }
    }
}

/// What the verdict says and how the turn stops because of it.
pub(crate) struct Verdict {
    pub claim: String,
    pub rule: &'static str,
    pub stop: StopReason,
}

/// Read the response the turn ended on.
pub(crate) fn answer(text: &str, facts: &Facts) -> Verdict {
    let summary =
        if text.trim().is_empty() { String::from("(no text)") } else { truncate(text, 160) };
    // A halt outranks every other claim. The other rules say what an answer
    // rests on; none of them is true of a response the provider cut off or
    // withheld, and a reader who saw `criterion-observed` beside half an answer
    // would be told the check justified it.
    if let Some(halt) = facts.halt {
        let kept = match facts.undispatched_calls {
            0 => String::new(),
            calls => format!(
                "; {calls} proposed call{} not dispatched",
                if calls == 1 { "" } else { "s" }
            ),
        };
        return match halt {
            Halt::OutputLimit => {
                let limit = match facts.output_limit {
                    Some(limit) => format!(" at {limit} completion tokens"),
                    None => String::new(),
                };
                Verdict {
                    claim: format!(
                        "truncated: the provider stopped the response{limit} before the turn \
                         finished{kept}: {summary}"
                    ),
                    rule: "verdict/output-truncated",
                    stop: StopReason::MaxOutputTokens,
                }
            }
            Halt::ContentFilter => Verdict {
                claim: format!(
                    "refused: the provider withheld the response under its content filter{kept}: \
                     {summary}"
                ),
                rule: "verdict/content-filtered",
                stop: StopReason::ContentFiltered,
            },
        };
    }
    // The order is the one the loop has always used, and the three flags do not
    // overlap: a check names a dispatched call, a repair switch is not the act
    // posture, and an unmet action is the act posture with nothing dispatched.
    let claim = if facts.unmet_action {
        format!("answer (the reading asked for an action; no call was dispatched): {summary}")
    } else {
        format!("answer: {summary}")
    };
    let rule = if facts.criterion_observed {
        "verdict/criterion-observed"
    } else if facts.repair_after_failure {
        "verdict/after-failure"
    } else if facts.unmet_action {
        "verdict/action-unmet"
    } else {
        "verdict/answer"
    };
    Verdict { claim, rule, stop: StopReason::FinalAnswer }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            halt: None,
            output_limit: None,
            undispatched_calls: 0,
            unmet_action: false,
            criterion_observed: false,
            repair_after_failure: false,
        }
    }

    #[test]
    fn a_plain_answer_rests_on_itself() {
        let verdict = answer("done", &facts());
        assert_eq!(verdict.rule, "verdict/answer");
        assert_eq!(verdict.claim, "answer: done");
        assert_eq!(verdict.stop, StopReason::FinalAnswer);
    }

    #[test]
    fn a_truncated_response_is_not_an_answer() {
        let facts = Facts {
            halt: Some(Halt::OutputLimit),
            output_limit: Some(2_000),
            criterion_observed: true,
            ..facts()
        };
        let verdict = answer("half a sent", &facts);
        assert_eq!(verdict.rule, "verdict/output-truncated");
        assert_eq!(verdict.stop, StopReason::MaxOutputTokens);
        assert!(verdict.claim.contains("2000 completion tokens"), "{}", verdict.claim);
        assert!(verdict.claim.contains("half a sent"), "{}", verdict.claim);
    }

    #[test]
    fn an_empty_truncated_response_says_so_without_claiming_an_answer() {
        let facts = Facts { halt: Some(Halt::OutputLimit), ..facts() };
        let verdict = answer("   ", &facts);
        assert!(verdict.claim.contains("(no text)"), "{}", verdict.claim);
        assert!(!verdict.claim.starts_with("answer"), "{}", verdict.claim);
    }

    /// A truncated batch of calls is the same halt as truncated prose: the plan
    /// was cut, and the verdict says how much of it was kept back.
    #[test]
    fn a_truncated_batch_of_calls_names_the_calls_it_did_not_dispatch() {
        let facts = Facts {
            halt: Some(Halt::OutputLimit),
            output_limit: Some(8_000),
            undispatched_calls: 2,
            ..facts()
        };
        let verdict = answer("", &facts);
        assert_eq!(verdict.rule, "verdict/output-truncated");
        assert_eq!(verdict.stop, StopReason::MaxOutputTokens);
        assert!(verdict.claim.contains("2 proposed calls not dispatched"), "{}", verdict.claim);
        assert!(!verdict.claim.contains("calls not dispatched;"), "{}", verdict.claim);
    }

    #[test]
    fn a_content_filtered_response_is_refused_and_not_answered() {
        let facts = Facts { halt: Some(Halt::ContentFilter), ..facts() };
        let verdict = answer("", &facts);
        assert_eq!(verdict.rule, "verdict/content-filtered");
        assert_eq!(verdict.stop, StopReason::ContentFiltered);
        assert!(verdict.claim.starts_with("refused"), "{}", verdict.claim);
    }

    /// The halt is read off the response, and only the two provider endings
    /// count: a provider that omits the field, or invents its own ending, is
    /// read as the model's own stop — as it always was.
    #[test]
    fn only_the_provider_s_two_endings_are_read_as_a_halt() {
        let response = |reason: &str| {
            let mut response = ModelResponse::text("");
            response.finish_reason = FinishReason::new(reason);
            response
        };
        assert_eq!(Halt::of(&response(FinishReason::LENGTH)), Some(Halt::OutputLimit));
        assert_eq!(Halt::of(&response(FinishReason::CONTENT_FILTER)), Some(Halt::ContentFilter));
        assert_eq!(Halt::of(&response(FinishReason::STOP)), None);
        assert_eq!(Halt::of(&response(FinishReason::TOOL_CALLS)), None);
        assert_eq!(Halt::of(&response("provider_specific_pause")), None);
    }

    #[test]
    fn the_evidence_rules_keep_their_order() {
        let observed = Facts { criterion_observed: true, ..facts() };
        assert_eq!(answer("done", &observed).rule, "verdict/criterion-observed");
        let repaired = Facts { repair_after_failure: true, ..facts() };
        assert_eq!(answer("done", &repaired).rule, "verdict/after-failure");
        // A contradiction between the two cannot happen in the loop (a repair
        // turn that observed a check keeps the check's rule), but if it did,
        // the check is the stronger evidence.
        let both = Facts { criterion_observed: true, repair_after_failure: true, ..facts() };
        assert_eq!(answer("done", &both).rule, "verdict/criterion-observed");
    }

    #[test]
    fn an_action_that_was_only_answered_is_named_on_the_verdict() {
        let unmet = Facts { unmet_action: true, ..facts() };
        let verdict = answer("done", &unmet);
        assert_eq!(verdict.rule, "verdict/action-unmet");
        assert!(verdict.claim.contains("no call was dispatched"), "{}", verdict.claim);
    }
}
