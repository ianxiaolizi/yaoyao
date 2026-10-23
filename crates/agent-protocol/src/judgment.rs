//! The session's judgment ledger: where a task stands and how it is decided.
//!
//! A judgment is a thinking record, not a transcript: five stages — situate,
//! frame, branch, act, reconcile — over the four dimensions of a situation
//! (time, space, people, state) plus the decisions that follow from them
//! (criterion, options, fallback, expectation). It is written by the loop's own
//! `judgment.update` tool (`TOOL_NAME`), which records a `judgment/updated`
//! event and nothing else — the transport never sees it.
//!
//! Unlike the plan, one call carries only the stage it is answering: the loop
//! merges it into the running record, and the event stores the merged record.
//! That is what makes the record a chain rather than five unrelated notes —
//! a reconcile that arrives after a situate still carries the situation.
//!
//! The bounds below are part of the contract rather than a style preference:
//! the record is repeated in every step's task layer, so an unbounded record
//! would grow the context on every request.

use crate::tool::{ToolDefinition, ToolSideEffect, ToolSource};
use serde::{Deserialize, Serialize};

/// The name of the loop-owned tool that writes a judgment.
///
/// It is offered in the turn's manifest like any other tool, but no provider
/// implements it: the loop validates the arguments, merges them into the
/// running record and records the ledger itself.
pub const TOOL_NAME: &str = "judgment.update";

/// Where in the deliberation a record stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgmentStage {
    /// Where am I: time, space, people, state.
    Situate,
    /// What is the problem: the checkable criterion for success.
    Frame,
    /// Which ways are there: options, the choice, and the fallback.
    Branch,
    /// What is the next smallest step, and what should it change.
    Act,
    /// What happened, why, and what next — after a real failure.
    Reconcile,
}

impl JudgmentStage {
    /// The wire word, matching the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Situate => "situate",
            Self::Frame => "frame",
            Self::Branch => "branch",
            Self::Act => "act",
            Self::Reconcile => "reconcile",
        }
    }
}

/// The session's current judgment.
///
/// Every field has a default so a record written by an older build still reads
/// back. What the *model* sends goes through [`Judgment::from_arguments`],
/// which validates the fields its stage is required to fill; the loop then
/// merges it with [`Judgment::merged_with`] and records the result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judgment {
    #[serde(default)]
    pub stage: Option<JudgmentStage>,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub space: String,
    #[serde(default)]
    pub people: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub criterion: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub fallback: String,
    #[serde(default)]
    pub expectation: String,
    #[serde(default)]
    pub reviewed: String,
    #[serde(default)]
    pub observed: String,
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub next: String,
    /// Reconcile only: whether `next` is a stop rather than a retry.
    ///
    /// This is the one bit the gate needs from the model, and the reason it
    /// exists: a weak model writes "I will list the directory" in `next` and
    /// then answers without listing anything. The kernel can check "was there
    /// a tool call after a retry was recorded" — it cannot check prose.
    #[serde(default)]
    pub stop: bool,
}

impl Judgment {
    /// Longest line for one situational dimension.
    pub const MAX_DIMENSION_CHARS: usize = 160;
    /// Longest statement of the checkable criterion.
    pub const MAX_CRITERION_CHARS: usize = 200;
    /// Most options one branch may carry.
    pub const MAX_OPTIONS: usize = 5;
    /// Longest option line.
    pub const MAX_OPTION_CHARS: usize = 120;
    /// Longest fallback.
    pub const MAX_FALLBACK_CHARS: usize = 160;
    /// Longest expected observation of the next step.
    pub const MAX_EXPECTATION_CHARS: usize = 160;
    /// Longest line in a reconcile record.
    pub const MAX_RECONCILE_CHARS: usize = 160;

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.stage.is_none()
            && self.time.is_empty()
            && self.space.is_empty()
            && self.people.is_empty()
            && self.state.is_empty()
            && self.criterion.is_empty()
            && self.options.is_empty()
            && self.fallback.is_empty()
            && self.expectation.is_empty()
            && self.reviewed.is_empty()
            && self.observed.is_empty()
            && self.verdict.is_empty()
            && self.next.is_empty()
            && !self.stop
    }

    /// Parse and validate one stage's update.
    ///
    /// Only the fields that stage is required to answer are checked here; a
    /// stage may carry lines for other stages too, and they are merged the
    /// same way. The error text is written for the model: it says what is
    /// missing and what to do about it.
    pub fn from_arguments(arguments: &serde_json::Value) -> Result<Self, JudgmentError> {
        let update: Self = serde_json::from_value(arguments.clone()).map_err(|error| {
            JudgmentError::Invalid(format!("arguments do not match the judgment schema: {error}"))
        })?;
        let update = update.trimmed();
        update.validate()?;
        Ok(update)
    }

    /// Check the bounds and the fields this stage must fill.
    pub fn validate(&self) -> Result<(), JudgmentError> {
        let stage = self.stage.ok_or_else(|| {
            JudgmentError::Invalid(String::from(
                "stage is empty; say which stage this is: situate, frame, branch, act or reconcile",
            ))
        })?;
        match stage {
            JudgmentStage::Situate => {
                self.require(&[
                    ("time", &self.time, "when this task stands, how fresh the evidence is"),
                    ("space", &self.space, "the boundary and the reach of this step"),
                    ("people", &self.people, "who asked, what they know, what is theirs to decide"),
                    ("state", &self.state, "the plan's position and the world as it is now"),
                ])?;
            }
            JudgmentStage::Frame => {
                self.require(&[(
                    "criterion",
                    &self.criterion,
                    "the checkable condition that means the task is done",
                )])?;
            }
            JudgmentStage::Branch => {
                if self.options.len() < 2 {
                    return Err(JudgmentError::Invalid(format!(
                        "options has {} entr{}; a branch needs at least two ways, or it is an \
                         impulse, not a decision",
                        self.options.len(),
                        if self.options.len() == 1 { "y" } else { "ies" }
                    )));
                }
                self.require(&[(
                    "fallback",
                    &self.fallback,
                    "where this retreats to when it fails",
                )])?;
            }
            JudgmentStage::Act => {
                self.require(&[(
                    "expectation",
                    &self.expectation,
                    "what this step should change, so a missing change is visible",
                )])?;
            }
            JudgmentStage::Reconcile => {
                // One required field, on purpose. A weak model handed four
                // required lines to a reconcile filled them one refusal at a
                // time and never got back to the task; the gate exists to make
                // it look, not to make it write a report. `reviewed` is a fact
                // the loop already holds and fills in when it is omitted;
                // `observed` and `verdict` are kept when offered, because they
                // still reach the next step's task layer.
                if self.next.is_empty() {
                    return Err(JudgmentError::Invalid(String::from(
                        "next is empty; for stage reconcile it must say a changed retry, or why \
                         stopping is right — for example {\"stage\":\"reconcile\",\"next\":\"\
                         list the directory first, then read the file that is there\"}",
                    )));
                }
            }
        }

        for (name, value, _) in self.lines() {
            let limit = match name {
                "criterion" => Self::MAX_CRITERION_CHARS,
                "fallback" => Self::MAX_FALLBACK_CHARS,
                "expectation" => Self::MAX_EXPECTATION_CHARS,
                "reviewed" | "observed" | "verdict" | "next" => Self::MAX_RECONCILE_CHARS,
                _ => Self::MAX_DIMENSION_CHARS,
            };
            bounded(name, value, limit)?;
        }
        if self.options.len() > Self::MAX_OPTIONS {
            return Err(JudgmentError::Invalid(format!(
                "options has {} entries; the limit is {}. Keep only the ways that are still open.",
                self.options.len(),
                Self::MAX_OPTIONS,
            )));
        }
        for (index, option) in self.options.iter().enumerate() {
            if option.is_empty() {
                return Err(JudgmentError::Invalid(format!(
                    "options[{index}] is empty; remove it or say what the way is"
                )));
            }
            bounded(&format!("options[{index}]"), option, Self::MAX_OPTION_CHARS)?;
        }
        Ok(())
    }

    /// Merge one stage's update into this record.
    ///
    /// Non-empty lines replace what was there; empty ones keep the previous
    /// value. That is what lets a reconcile record still show the situation it
    /// was recorded against without the model resending it.
    pub fn merged_with(&self, update: &Self) -> Self {
        let mut merged = self.clone();
        merged.stage = update.stage.or(merged.stage);
        for (slot, value) in [
            (&mut merged.time, &update.time),
            (&mut merged.space, &update.space),
            (&mut merged.people, &update.people),
            (&mut merged.state, &update.state),
            (&mut merged.criterion, &update.criterion),
            (&mut merged.fallback, &update.fallback),
            (&mut merged.expectation, &update.expectation),
            (&mut merged.reviewed, &update.reviewed),
            (&mut merged.observed, &update.observed),
            (&mut merged.verdict, &update.verdict),
            (&mut merged.next, &update.next),
        ] {
            if !value.is_empty() {
                *slot = value.clone();
            }
        }
        if !update.options.is_empty() {
            merged.options = update.options.clone();
        }
        if update.stage == Some(JudgmentStage::Reconcile) {
            merged.stop = update.stop;
        }
        merged
    }

    /// Whether this record's reconcile section names the given tool.
    ///
    /// The check is deliberately loose about wording and exact about the tool:
    /// a weak model may describe the call in its own words, but a reconcile
    /// that never names the tool that failed is a reconcile of something else.
    pub fn reconciles(&self, tool: &str) -> bool {
        self.reviewed.to_lowercase().contains(&tool.to_lowercase())
    }

    /// The judgment as the task layer shows it.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::from("Judgment: none recorded yet.");
        }
        let mut text = String::from("Judgment so far:");
        let stage = self.stage.map(|stage| stage.as_str()).unwrap_or("unrecorded");
        text.push_str(&format!("\nStage: {stage}"));
        if !self.time.is_empty() {
            text.push_str(&format!("\nTime: {}", self.time));
        }
        if !self.space.is_empty() {
            text.push_str(&format!("\nSpace: {}", self.space));
        }
        if !self.people.is_empty() {
            text.push_str(&format!("\nPeople: {}", self.people));
        }
        if !self.state.is_empty() {
            text.push_str(&format!("\nState: {}", self.state));
        }
        if !self.criterion.is_empty() {
            text.push_str(&format!("\nCriterion: {}", self.criterion));
        }
        if !self.options.is_empty() {
            text.push_str(&format!("\nOptions: {}", self.options.join(" | ")));
        }
        if !self.fallback.is_empty() {
            text.push_str(&format!("\nFallback: {}", self.fallback));
        }
        if !self.expectation.is_empty() {
            text.push_str(&format!("\nExpectation: {}", self.expectation));
        }
        if !self.reviewed.is_empty() {
            text.push_str(&format!("\nReviewed: {}", self.reviewed));
        }
        if !self.observed.is_empty() {
            text.push_str(&format!("\nObserved: {}", self.observed));
        }
        if !self.verdict.is_empty() {
            text.push_str(&format!("\nVerdict: {}", self.verdict));
        }
        if !self.next.is_empty() {
            text.push_str(&format!("\nNext: {}", self.next));
        }
        if self.stop {
            text.push_str("\nDecision: stop");
        }
        text
    }

    /// The non-empty single-line sections, by name.
    fn lines(&self) -> Vec<(&'static str, &String, &'static str)> {
        vec![
            ("time", &self.time, ""),
            ("space", &self.space, ""),
            ("people", &self.people, ""),
            ("state", &self.state, ""),
            ("criterion", &self.criterion, ""),
            ("fallback", &self.fallback, ""),
            ("expectation", &self.expectation, ""),
            ("reviewed", &self.reviewed, ""),
            ("observed", &self.observed, ""),
            ("verdict", &self.verdict, ""),
            ("next", &self.next, ""),
        ]
    }

    fn require(&self, fields: &[(&str, &String, &str)]) -> Result<(), JudgmentError> {
        for (name, value, what) in fields {
            if value.is_empty() {
                return Err(JudgmentError::Invalid(format!(
                    "{name} is empty; for stage {} it must say {what}",
                    self.stage.map(|stage| stage.as_str()).unwrap_or("unrecorded"),
                )));
            }
        }
        Ok(())
    }

    fn trimmed(mut self) -> Self {
        for slot in [
            &mut self.time,
            &mut self.space,
            &mut self.people,
            &mut self.state,
            &mut self.criterion,
            &mut self.fallback,
            &mut self.expectation,
            &mut self.reviewed,
            &mut self.observed,
            &mut self.verdict,
            &mut self.next,
        ] {
            *slot = slot.trim().to_string();
        }
        for option in &mut self.options {
            *option = option.trim().to_string();
        }
        self
    }
}

fn bounded(field: &str, text: &str, max: usize) -> Result<(), JudgmentError> {
    let count = text.chars().count();
    if count > max {
        return Err(JudgmentError::Invalid(format!(
            "{field} is {count} characters; the limit is {max}. Keep it shorter."
        )));
    }
    Ok(())
}

/// A judgment the model sent that cannot be recorded, with the reason to hand
/// back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JudgmentError {
    #[error("{0}")]
    Invalid(String),
}

/// The declaration of the loop-owned `judgment.update` tool.
///
/// It reaches the model through the turn's manifest like every other tool, so
/// the stages and bounds are stated here as well as enforced in
/// [`Judgment::validate`]: the model should know the shape before it writes.
pub fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_NAME.to_string(),
        description: "Record one stage of how you are thinking about this task. Call this only \
                      when the kernel asks you to — it does so with a `Kernel check` note, or \
                      with a sentence in the task layer; in this build that means `branch` \
                      before an irreversible call, and \
                      `reconcile` after a real failure. \
                      Stages: \
                      `situate` (time, space, people, state — where you are, how fresh your \
                      evidence is, who decides what, what the world is like now); `frame` \
                      (criterion — the checkable condition that means the task is done); \
                      `branch` (options, at least two, and fallback — where you retreat to); \
                      `act` (expectation — what the next step should change); `reconcile` \
                      (next and stop — after a real failure, either the changed retry you will \
                      do now with stop false, or why stopping is right with stop true; observed \
                      and verdict if you have them; reviewed is filled in for you). \
                      Send only the stage you are recording; earlier sections are kept. After \
                      a real failure, check the world if you have not (list or read), then \
                      record reconcile before giving a final answer. Limits: 160 characters \
                      per line, 200 for criterion, at most 5 options of 120 characters."
            .to_string(),
        source: ToolSource::Builtin,
        side_effect: ToolSideEffect::Pure,
        capability_requirements: Vec::new(),
        idempotent: true,
        timeout_ms: None,
        input_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["stage"],
            "properties": {
                "stage": {
                    "type": "string",
                    "enum": ["situate", "frame", "branch", "act", "reconcile"],
                },
                "time": {
                    "type": "string",
                    "description": "Stage situate: what phase the task is in, how fresh the evidence is, what is left.",
                    "maxLength": Judgment::MAX_DIMENSION_CHARS,
                },
                "space": {
                    "type": "string",
                    "description": "Stage situate: the boundary you work in, how far this step reaches, what is adjacent.",
                    "maxLength": Judgment::MAX_DIMENSION_CHARS,
                },
                "people": {
                    "type": "string",
                    "description": "Stage situate: who asked, what they know, what only they may decide, who else shares this data.",
                    "maxLength": Judgment::MAX_DIMENSION_CHARS,
                },
                "state": {
                    "type": "string",
                    "description": "Stage situate: the plan's position, the world as it is now, anything still pending.",
                    "maxLength": Judgment::MAX_DIMENSION_CHARS,
                },
                "criterion": {
                    "type": "string",
                    "description": "Stage frame: the command or observable condition that means the task is done.",
                    "maxLength": Judgment::MAX_CRITERION_CHARS,
                },
                "options": {
                    "type": "array",
                    "description": "Stage branch: the open ways, at least two, each with its cost and reversibility.",
                    "maxItems": Judgment::MAX_OPTIONS,
                    "items": {
                        "type": "string",
                        "maxLength": Judgment::MAX_OPTION_CHARS,
                    },
                },
                "fallback": {
                    "type": "string",
                    "description": "Stage branch: where you retreat to if the chosen way fails.",
                    "maxLength": Judgment::MAX_FALLBACK_CHARS,
                },
                "expectation": {
                    "type": "string",
                    "description": "Stage act: what the next step should change, so a missing change is visible.",
                    "maxLength": Judgment::MAX_EXPECTATION_CHARS,
                },
                "reviewed": {
                    "type": "string",
                    "description": "Stage reconcile: the call that failed. Optional — the kernel fills in the call that just failed when it is left out.",
                    "maxLength": Judgment::MAX_RECONCILE_CHARS,
                },
                "observed": {
                    "type": "string",
                    "description": "Stage reconcile: what the failure showed.",
                    "maxLength": Judgment::MAX_RECONCILE_CHARS,
                },
                "verdict": {
                    "type": "string",
                    "description": "Stage reconcile: whether the method or the execution was wrong.",
                    "maxLength": Judgment::MAX_RECONCILE_CHARS,
                },
                "next": {
                    "type": "string",
                    "description": "Stage reconcile: the changed retry you will do now, or — with stop set — why stopping is the right answer.",
                    "maxLength": Judgment::MAX_RECONCILE_CHARS,
                },
                "stop": {
                    "type": "boolean",
                    "description": "Stage reconcile: true only when `next` explains why stopping is right. Leave false when `next` is a retry — the kernel then expects you to actually make that call before answering.",
                },
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_situate() -> serde_json::Value {
        serde_json::json!({
            "stage": "situate",
            "time": "second step of three",
            "space": "workspace root, one file",
            "people": "the owner asked; the decision to overwrite is theirs",
            "state": "no plan yet; report.txt is not in the root",
        })
    }

    #[test]
    fn a_situate_records_all_four_dimensions() {
        let judgment = Judgment::from_arguments(&a_situate()).expect("a valid judgment");
        assert_eq!(judgment.stage, Some(JudgmentStage::Situate));
        assert!(judgment.time.contains("second step"));
        assert!(judgment.render().contains("People:"));
    }

    #[test]
    fn every_stage_requires_its_own_fields_with_an_actionable_message() {
        let cases = [
            (
                serde_json::json!({ "stage": "situate", "time": "t", "space": "s", "people": "p" }),
                "state is empty",
            ),
            (serde_json::json!({ "stage": "frame" }), "criterion is empty"),
            (
                serde_json::json!({ "stage": "branch", "options": ["one"], "fallback": "f" }),
                "at least two",
            ),
            (serde_json::json!({ "stage": "act" }), "expectation is empty"),
            (
                serde_json::json!({ "stage": "reconcile", "reviewed": "shell.run", "observed": "o",
                                 "verdict": "v" }),
                "next is empty",
            ),
        ];
        for (arguments, expected) in cases {
            let error = Judgment::from_arguments(&arguments).expect_err("the record is refused");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    /// The gate asks the model to look and say what it will do; it does not
    /// ask for a report. One required line is what a weak model completes.
    #[test]
    fn a_reconcile_needs_only_the_next_line() {
        let update = Judgment::from_arguments(&serde_json::json!({
            "stage": "reconcile",
            "next": "list the directory first, then read the file that is there",
        }))
        .expect("a valid reconcile");
        assert_eq!(update.stage, Some(JudgmentStage::Reconcile));
        assert!(update.reviewed.is_empty(), "the loop fills this in");
        assert!(update.observed.is_empty(), "optional, not missing");
    }

    #[test]
    fn a_stage_is_required_and_must_be_known() {
        let missing = Judgment::from_arguments(&serde_json::json!({})).expect_err("no stage");
        assert!(missing.to_string().contains("stage is empty"), "{missing}");

        let unknown = Judgment::from_arguments(&serde_json::json!({ "stage": "think" }))
            .expect_err("unknown stage");
        assert!(unknown.to_string().contains("unknown variant"), "{unknown}");
    }

    #[test]
    fn strings_are_trimmed_and_bounds_are_enforced() {
        let mut update = a_situate();
        update["time"] = serde_json::json!("  second step  ");
        let judgment = Judgment::from_arguments(&update).expect("valid");
        assert_eq!(judgment.time, "second step");

        let long = serde_json::json!({
            "stage": "situate",
            "time": "x".repeat(Judgment::MAX_DIMENSION_CHARS + 1),
            "space": "s",
            "people": "p",
            "state": "s",
        });
        let error = Judgment::from_arguments(&long).expect_err("too long");
        assert!(error.to_string().contains("limit is 160"), "{error}");
    }

    #[test]
    fn a_merge_keeps_earlier_sections_and_replaces_what_is_resent() {
        let situate = Judgment::from_arguments(&a_situate()).expect("valid");
        let reconcile = Judgment::from_arguments(&serde_json::json!({
            "stage": "reconcile",
            "reviewed": "shell.run cat report.txt",
            "observed": "no such file in the root",
            "verdict": "the path is wrong, not the file",
            "next": "list the directory, then read reports/report.txt",
        }))
        .expect("valid");
        let merged = situate.merged_with(&reconcile);
        assert_eq!(merged.stage, Some(JudgmentStage::Reconcile));
        assert_eq!(merged.time, situate.time);
        assert!(merged.reconciles("shell.run"));
        assert!(merged.render().contains("Observed: no such file"));
    }

    #[test]
    fn an_empty_record_renders_as_a_stated_absence() {
        assert_eq!(Judgment::default().render(), "Judgment: none recorded yet.");
        assert!(Judgment::default().is_empty());
    }

    #[test]
    fn a_recorded_judgment_renders_every_filled_line() {
        let judgment = Judgment::from_arguments(&a_situate()).expect("valid");
        let text = judgment.render();
        assert!(text.starts_with("Judgment so far:\nStage: situate"));
        assert!(text.contains("Time: second step of three"));
        assert!(text.contains("Space: workspace root, one file"));
        assert!(text.contains("People:"));
        assert!(text.contains("State:"));
    }

    #[test]
    fn the_tool_declaration_names_the_loop_enforces() {
        let definition = tool_definition();
        assert_eq!(definition.name, TOOL_NAME);
        assert_eq!(definition.source, ToolSource::Builtin);
        assert_eq!(definition.side_effect, ToolSideEffect::Pure);
        assert!(definition.idempotent);
        let schema = &definition.input_schema;
        assert_eq!(schema["required"], serde_json::json!(["stage"]));
        assert_eq!(
            schema["properties"]["stage"]["enum"],
            serde_json::json!(["situate", "frame", "branch", "act", "reconcile"])
        );
        assert_eq!(schema["properties"]["options"]["maxItems"], Judgment::MAX_OPTIONS);
    }
}
