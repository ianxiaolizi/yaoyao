//! What the turn's last response says, and which rule the verdict names.
//!
//! The choice is a claim about evidence, so it is made in one place instead of
//! being spread through the loop's control flow: a truncated response is not an
//! answer at all (the provider cut it off at the run's output limit), and it
//! must not be described by any of the rules that say what an answer rests on.
//! On 2026-10-30 a reasoning model spent the whole 2,000-token default on
//! reasoning tokens, returned no content with `finish_reason: length`, and this
//! branch called it a final answer: exit 0, `final_answer` in `metrics`, and
//! `(no text)` in `why`.

use agent_ports::agent_loop::StopReason;

use crate::context_layers::truncate;

/// How the response that ended the turn should be read.
pub(crate) struct Facts {
    /// The provider stopped the response at the output limit.
    pub truncated: bool,
    /// The limit that was in force, for the verdict to name.
    pub output_limit: Option<u32>,
    /// The reading asked for an action and no call was dispatched.
    pub unmet_action: bool,
    /// A check the request named completed after the work.
    pub criterion_observed: bool,
    /// The turn switched posture to repair after a real failure.
    pub repair_after_failure: bool,
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
    // Truncation outranks every other claim. The other rules say what an answer
    // rests on; none of them is true of text the provider cut off, and a reader
    // who saw `criterion-observed` beside half an answer would be told the
    // check justified it.
    if facts.truncated {
        let limit = match facts.output_limit {
            Some(limit) => format!(" at {limit} completion tokens"),
            None => String::new(),
        };
        return Verdict {
            claim: format!(
                "truncated: the provider stopped the response{limit} before the answer \
                 finished: {summary}"
            ),
            rule: "verdict/output-truncated",
            stop: StopReason::MaxOutputTokens,
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
            truncated: false,
            output_limit: None,
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
        let mut facts = facts();
        facts.truncated = true;
        facts.output_limit = Some(2_000);
        facts.criterion_observed = true;
        let verdict = answer("half a sent", &facts);
        assert_eq!(verdict.rule, "verdict/output-truncated");
        assert_eq!(verdict.stop, StopReason::MaxOutputTokens);
        assert!(verdict.claim.contains("2000 completion tokens"), "{}", verdict.claim);
        assert!(verdict.claim.contains("half a sent"), "{}", verdict.claim);
    }

    #[test]
    fn an_empty_truncated_response_says_so_without_claiming_an_answer() {
        let mut facts = facts();
        facts.truncated = true;
        let verdict = answer("   ", &facts);
        assert!(verdict.claim.contains("(no text)"), "{}", verdict.claim);
        assert!(!verdict.claim.starts_with("answer"), "{}", verdict.claim);
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
