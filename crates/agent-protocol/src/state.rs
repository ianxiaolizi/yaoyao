//! The work state a run keeps about itself: what it is doing, what it has
//! finished, and what it could not do.
//!
//! A long run loses its purpose to compaction. The transcript is chronological,
//! so the earliest statement of the goal is the first thing a window gives up,
//! and what is left is the most recent tool results — a model that has been
//! reading files for forty steps can no longer say which question it was
//! answering. The plan ledger (`crate::plan`) holds the *intended* steps; this
//! holds what the model *claims to know now*, in the same words a later step
//! will read.
//!
//! It is written by the loop's own `state.update` tool ([`TOOL_NAME`]), which
//! records a `state/updated` event and nothing else. Each call is a full
//! replacement, for the reason `plan.update` also is (CONTRACT.md §7 #45):
//! there is one writer, so what it last sent is exactly what every later turn
//! reads, and no patch has to be reasoned about.
//!
//! The state is *the model's* account. The objective half — budget spent,
//! steps taken, the last failure — is computed by the loop and placed beside
//! it; nothing here is treated as a fact about the world. `done[].evidence`
//! is required and its shape is checked, but whether the evidence proves the
//! claim is the model's judgment, and the loop does not pretend otherwise.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolDefinition, ToolSideEffect, ToolSource};

/// The name of the loop-owned tool that records the work state.
///
/// Offered in the turn's manifest like `plan.update`, answered by the loop
/// itself, and reserved by the app assembly so a plugin cannot claim it.
pub const TOOL_NAME: &str = "state.update";

/// The schema word carried by a [`WorkState`]'s arguments.
pub const SCHEMA: &str = "state/1";

/// How sure the model says it is of a hypothesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /// The wire word, matching the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// One thing the model says is finished, and how it knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoneItem {
    pub what: String,
    /// A pointer the claim can be checked against: a tool call id, a path, a
    /// URL, an artifact reference. Required, because an unevidenced "done" is
    /// exactly what a long run accumulates.
    pub evidence: String,
}

/// One thing the model is working on now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoingItem {
    pub what: String,
    pub why: String,
}

/// A belief the model holds but has not settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hypothesis {
    pub claim: String,
    pub confidence: Confidence,
    pub evidence: String,
}

/// Something that went wrong, kept so the next step does not repeat it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureItem {
    pub what: String,
    pub why: String,
}

/// The model's account of where the work stands.
///
/// Deserialization is liberal (every field defaults) because the same shape is
/// read back from the durable log, where a version written by an older build
/// may not carry every field. What the *model* sends goes through
/// [`WorkState::from_arguments`], which enforces the bounds below.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkState {
    /// The schema word the model sent, when it sent one.
    ///
    /// Accepted and checked rather than refused, because the tool definition
    /// tells the model to send it; omitted from events when absent so a
    /// state recorded without it reads exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub next: String,
    #[serde(default)]
    pub done: Vec<DoneItem>,
    #[serde(default)]
    pub doing: Vec<DoingItem>,
    #[serde(default)]
    pub hypotheses: Vec<Hypothesis>,
    #[serde(default)]
    pub failures: Vec<FailureItem>,
    #[serde(default)]
    pub open: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

impl WorkState {
    /// Longest goal, in characters.
    pub const MAX_GOAL_CHARS: usize = 500;
    /// Longest statement of the next action, in characters.
    pub const MAX_NEXT_CHARS: usize = 300;
    /// Most finished items one state may claim.
    pub const MAX_DONE: usize = 10;
    /// Most in-progress items one state may carry.
    pub const MAX_DOING: usize = 10;
    /// Most hypotheses one state may carry.
    pub const MAX_HYPOTHESES: usize = 10;
    /// Most failures one state may carry.
    pub const MAX_FAILURES: usize = 10;
    /// Most open questions one state may carry.
    pub const MAX_OPEN: usize = 10;
    /// Longest `what`, in characters.
    pub const MAX_WHAT_CHARS: usize = 200;
    /// Longest `why`, in characters.
    pub const MAX_WHY_CHARS: usize = 300;
    /// Longest claim, in characters.
    pub const MAX_CLAIM_CHARS: usize = 300;
    /// Longest evidence pointer, in characters.
    pub const MAX_EVIDENCE_CHARS: usize = 300;
    /// Longest open question, in characters.
    pub const MAX_OPEN_CHARS: usize = 200;
    /// Longest blocked statement, in characters.
    pub const MAX_BLOCKED_CHARS: usize = 300;
    /// Longest rendered state, in characters.
    ///
    /// The rendered block replaces plain traffic in the request, so it is
    /// bounded like a layer rather than like a transcript: roughly 1,500
    /// tokens at the estimator's four characters per token.
    pub const MAX_RENDER_CHARS: usize = 6_000;

    /// Whether the model has recorded nothing yet.
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty()
            && self.next.is_empty()
            && self.done.is_empty()
            && self.doing.is_empty()
            && self.hypotheses.is_empty()
            && self.failures.is_empty()
            && self.open.is_empty()
            && self.blocked.is_none()
    }

    /// Parse and validate the arguments of a `state.update` call.
    ///
    /// Strings are trimmed before they are measured and kept. The error text
    /// is written for the model: it names the field and the limit.
    pub fn from_arguments(arguments: &serde_json::Value) -> Result<Self, StateError> {
        let state: Self = serde_json::from_value(arguments.clone()).map_err(|error| {
            StateError::Invalid(format!("arguments do not match the state schema: {error}"))
        })?;
        let state = state.trimmed();
        state.validate()?;
        Ok(state)
    }

    /// Check every bound, assuming the strings are already trimmed.
    pub fn validate(&self) -> Result<(), StateError> {
        if let Some(schema) = &self.schema
            && schema != SCHEMA
        {
            return Err(StateError::Invalid(format!(
                "schema is {schema:?}; this tool records {SCHEMA}. Leave the field out if you \
                 are not sure."
            )));
        }
        if self.goal.is_empty() {
            return Err(StateError::Invalid(String::from(
                "goal is empty; state in one sentence what this run is trying to achieve",
            )));
        }
        bounded("goal", &self.goal, Self::MAX_GOAL_CHARS)?;
        if self.next.is_empty() && self.blocked.is_none() {
            return Err(StateError::Invalid(String::from(
                "next is empty; name the next concrete action, or set blocked",
            )));
        }
        bounded("next", &self.next, Self::MAX_NEXT_CHARS)?;
        capped("done", self.done.len(), Self::MAX_DONE)?;
        for (index, item) in self.done.iter().enumerate() {
            bounded(&format!("done[{index}].what"), &item.what, Self::MAX_WHAT_CHARS)?;
            if item.what.is_empty() {
                return Err(StateError::Invalid(format!(
                    "done[{index}].what is empty; say what is finished"
                )));
            }
            if item.evidence.is_empty() {
                return Err(StateError::Invalid(format!(
                    "done[{index}].evidence is empty; name the tool call, path or URL that \
                     shows it — an unevidenced claim of progress is what this field exists to \
                     prevent"
                )));
            }
            bounded(&format!("done[{index}].evidence"), &item.evidence, Self::MAX_EVIDENCE_CHARS)?;
        }
        capped("doing", self.doing.len(), Self::MAX_DOING)?;
        for (index, item) in self.doing.iter().enumerate() {
            bounded(&format!("doing[{index}].what"), &item.what, Self::MAX_WHAT_CHARS)?;
            bounded(&format!("doing[{index}].why"), &item.why, Self::MAX_WHY_CHARS)?;
        }
        capped("hypotheses", self.hypotheses.len(), Self::MAX_HYPOTHESES)?;
        for (index, item) in self.hypotheses.iter().enumerate() {
            bounded(&format!("hypotheses[{index}].claim"), &item.claim, Self::MAX_CLAIM_CHARS)?;
            bounded(
                &format!("hypotheses[{index}].evidence"),
                &item.evidence,
                Self::MAX_EVIDENCE_CHARS,
            )?;
        }
        capped("failures", self.failures.len(), Self::MAX_FAILURES)?;
        for (index, item) in self.failures.iter().enumerate() {
            bounded(&format!("failures[{index}].what"), &item.what, Self::MAX_WHAT_CHARS)?;
            bounded(&format!("failures[{index}].why"), &item.why, Self::MAX_WHY_CHARS)?;
        }
        capped("open", self.open.len(), Self::MAX_OPEN)?;
        for (index, item) in self.open.iter().enumerate() {
            bounded(&format!("open[{index}]"), item, Self::MAX_OPEN_CHARS)?;
        }
        if let Some(blocked) = &self.blocked {
            if blocked.is_empty() {
                return Err(StateError::Invalid(String::from(
                    "blocked is empty; say what cannot be done, or drop the field",
                )));
            }
            bounded("blocked", blocked, Self::MAX_BLOCKED_CHARS)?;
        }
        let rendered = self.render();
        if rendered.chars().count() > Self::MAX_RENDER_CHARS {
            return Err(StateError::Invalid(format!(
                "the rendered state is {} characters; the limit is {}. Keep what the next step \
                 needs and drop what it can re-derive.",
                rendered.chars().count(),
                Self::MAX_RENDER_CHARS,
            )));
        }
        Ok(())
    }

    /// The state as a prompt block: what a step that has lost its history reads
    /// first.
    ///
    /// Empty sections are omitted rather than shown empty, so a state that
    /// names only a goal and a next action is two lines. The whole block is
    /// labelled as the model's own account, because that is what it is.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::from("Work state: the model has not recorded one yet.");
        }
        let mut text = String::from("Work state (the model's own account):");
        if !self.goal.is_empty() {
            text.push_str(&format!("\nGoal: {}", self.goal));
        }
        if !self.next.is_empty() {
            text.push_str(&format!("\nNext: {}", self.next));
        }
        if let Some(blocked) = &self.blocked {
            text.push_str(&format!("\nBlocked: {blocked}"));
        }
        if !self.done.is_empty() {
            text.push_str("\nDone:");
            for item in &self.done {
                text.push_str(&format!("\n- {} (evidence: {})", item.what, item.evidence));
            }
        }
        if !self.doing.is_empty() {
            text.push_str("\nDoing:");
            for item in &self.doing {
                text.push_str(&format!("\n- {} ({})", item.what, item.why));
            }
        }
        if !self.hypotheses.is_empty() {
            text.push_str("\nHypotheses:");
            for item in &self.hypotheses {
                text.push_str(&format!(
                    "\n- {} ({} confidence; {})",
                    item.claim,
                    item.confidence.as_str(),
                    item.evidence
                ));
            }
        }
        if !self.failures.is_empty() {
            text.push_str("\nFailures:");
            for item in &self.failures {
                text.push_str(&format!("\n- {} ({})", item.what, item.why));
            }
        }
        if !self.open.is_empty() {
            text.push_str("\nOpen:");
            for item in &self.open {
                text.push_str(&format!("\n- {item}"));
            }
        }
        text
    }

    /// The compact line the task layer carries at the start of a turn: enough
    /// to know what the previous turn was for, without repeating the block.
    ///
    /// `None` when nothing was recorded, so a session that never called the
    /// tool reads exactly as it did before the tool existed.
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut text = String::from("Work state carried over:");
        if !self.goal.is_empty() {
            text.push_str(&format!(" goal: {}", self.goal));
        }
        if !self.next.is_empty() {
            text.push_str(&format!("; next: {}", self.next));
        }
        if let Some(blocked) = &self.blocked {
            text.push_str(&format!("; blocked: {blocked}"));
        }
        if !self.done.is_empty() {
            text.push_str(&format!("; {} item(s) reported done", self.done.len()));
        }
        Some(text)
    }

    /// Trim every string that the model sent.
    fn trimmed(&self) -> Self {
        Self {
            schema: self.schema.as_ref().map(|schema| schema.trim().to_string()),
            goal: self.goal.trim().to_string(),
            next: self.next.trim().to_string(),
            done: self
                .done
                .iter()
                .map(|item| DoneItem {
                    what: item.what.trim().to_string(),
                    evidence: item.evidence.trim().to_string(),
                })
                .collect(),
            doing: self
                .doing
                .iter()
                .map(|item| DoingItem {
                    what: item.what.trim().to_string(),
                    why: item.why.trim().to_string(),
                })
                .collect(),
            hypotheses: self
                .hypotheses
                .iter()
                .map(|item| Hypothesis {
                    claim: item.claim.trim().to_string(),
                    confidence: item.confidence,
                    evidence: item.evidence.trim().to_string(),
                })
                .collect(),
            failures: self
                .failures
                .iter()
                .map(|item| FailureItem {
                    what: item.what.trim().to_string(),
                    why: item.why.trim().to_string(),
                })
                .collect(),
            open: self.open.iter().map(|item| item.trim().to_string()).collect(),
            blocked: self.blocked.as_ref().map(|item| item.trim().to_string()),
        }
    }
}

/// A state the model sent that cannot be recorded, with the reason to hand
/// back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("{0}")]
    Invalid(String),
}

fn bounded(field: &str, value: &str, max: usize) -> Result<(), StateError> {
    if value.chars().count() > max {
        return Err(StateError::Invalid(format!(
            "{field} is {} characters; the limit is {max}. Shorten it.",
            value.chars().count()
        )));
    }
    Ok(())
}

fn capped(field: &str, len: usize, max: usize) -> Result<(), StateError> {
    if len > max {
        return Err(StateError::Invalid(format!(
            "{field} has {len} entries; the limit is {max}. Keep only what a later step needs."
        )));
    }
    Ok(())
}

/// The declaration of the loop-owned `state.update` tool.
///
/// It reaches the model through the turn's manifest, so the bounds are stated
/// here as well as enforced in [`WorkState::validate`].
pub fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_NAME.to_string(),
        description: format!(
            "Record where this run stands: the goal, the next concrete action, what is \
             finished and the evidence for it, what is in progress, what you believe but have \
             not settled, what failed, what is still open, and what is blocked. Send the \
             complete state every time; it replaces the previous one. The state is shown at \
             the end of every request and carried into later turns, so it is how a long run \
             keeps its purpose after old context is compacted. Call it before changing \
             direction, before the final answer, and when a budget notice asks you to settle. \
             Every `done` entry needs `evidence` (the tool call, path or URL that shows it). \
             Limits: goal {goal}; next {next}; at most {counts} entries per list; each `what` \
             {what}; each `why`, claim and evidence {why}; blocked {blocked}.",
            goal = WorkState::MAX_GOAL_CHARS,
            next = WorkState::MAX_NEXT_CHARS,
            counts = WorkState::MAX_DONE,
            what = WorkState::MAX_WHAT_CHARS,
            why = WorkState::MAX_WHY_CHARS,
            blocked = WorkState::MAX_BLOCKED_CHARS,
        ),
        source: ToolSource::Builtin,
        side_effect: ToolSideEffect::Pure,
        capability_requirements: Vec::new(),
        idempotent: true,
        timeout_ms: None,
        input_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["goal"],
            // A fully blocked task has no honest next action. One of the two
            // fields is required, and `validate` checks that `next` is not
            // merely an empty string when it is the one supplied.
            "anyOf": [
                { "required": ["next"] },
                { "required": ["blocked"] },
            ],
            "properties": {
                "schema": {
                    "type": "string",
                    "description": "Always \"state/1\".",
                    "enum": [SCHEMA],
                },
                "goal": {
                    "type": "string",
                    "description": "One sentence: what this run is trying to achieve.",
                    "maxLength": WorkState::MAX_GOAL_CHARS,
                },
                "next": {
                    "type": "string",
                    "description": "The next concrete action, not a plan.",
                    "maxLength": WorkState::MAX_NEXT_CHARS,
                },
                "done": {
                    "type": "array",
                    "maxItems": WorkState::MAX_DONE,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["what", "evidence"],
                        "properties": {
                            "what": { "type": "string", "maxLength": WorkState::MAX_WHAT_CHARS },
                            "evidence": {
                                "type": "string",
                                "description": "The tool call id, path or URL that shows it.",
                                "maxLength": WorkState::MAX_EVIDENCE_CHARS,
                            },
                        },
                    },
                },
                "doing": {
                    "type": "array",
                    "maxItems": WorkState::MAX_DOING,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["what", "why"],
                        "properties": {
                            "what": { "type": "string", "maxLength": WorkState::MAX_WHAT_CHARS },
                            "why": { "type": "string", "maxLength": WorkState::MAX_WHY_CHARS },
                        },
                    },
                },
                "hypotheses": {
                    "type": "array",
                    "maxItems": WorkState::MAX_HYPOTHESES,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["claim", "confidence", "evidence"],
                        "properties": {
                            "claim": { "type": "string", "maxLength": WorkState::MAX_CLAIM_CHARS },
                            "confidence": {
                                "type": "string",
                                "enum": ["low", "medium", "high"],
                            },
                            "evidence": {
                                "type": "string",
                                "maxLength": WorkState::MAX_EVIDENCE_CHARS,
                            },
                        },
                    },
                },
                "failures": {
                    "type": "array",
                    "maxItems": WorkState::MAX_FAILURES,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["what", "why"],
                        "properties": {
                            "what": { "type": "string", "maxLength": WorkState::MAX_WHAT_CHARS },
                            "why": { "type": "string", "maxLength": WorkState::MAX_WHY_CHARS },
                        },
                    },
                },
                "open": {
                    "type": "array",
                    "maxItems": WorkState::MAX_OPEN,
                    "items": { "type": "string", "maxLength": WorkState::MAX_OPEN_CHARS },
                },
                "blocked": {
                    "type": ["string", "null"],
                    "description": "What cannot be done, and why. Null when nothing is blocked.",
                    "maxLength": WorkState::MAX_BLOCKED_CHARS,
                },
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_state() -> serde_json::Value {
        serde_json::json!({
            "schema": "state/1",
            "goal": "make the airbench submissions pass",
            "next": "read the failing test for problem 3",
            "done": [
                { "what": "fetched all 15 problems", "evidence": "call_7" },
            ],
            "doing": [
                { "what": "debugging problem 3", "why": "its test fails on edge case 2" },
            ],
            "hypotheses": [
                { "claim": "the boundary is off by one", "confidence": "medium",
                  "evidence": "two inputs differ only at the upper bound" },
            ],
            "failures": [
                { "what": "submission 2", "why": "time limit exceeded" },
            ],
            "open": ["does problem 4 need the same fix?"],
            "blocked": "problem 9 needs the editor server, which is not running",
        })
    }

    #[test]
    fn a_state_with_every_section_renders_in_the_order_a_later_step_reads() {
        let state = WorkState::from_arguments(&a_state()).expect("a valid state");

        assert_eq!(state.goal, "make the airbench submissions pass");
        assert_eq!(state.next, "read the failing test for problem 3");
        assert_eq!(state.done[0].evidence, "call_7");
        assert_eq!(state.hypotheses[0].confidence, Confidence::Medium);
        assert_eq!(
            state.blocked.as_deref(),
            Some("problem 9 needs the editor server, which is not running")
        );

        let rendered = state.render();
        let order: Vec<usize> =
            ["Goal:", "Next:", "Blocked:", "Done:", "Doing:", "Hypotheses:", "Failures:", "Open:"]
                .iter()
                .map(|heading| {
                    rendered.find(heading).unwrap_or_else(|| panic!("{heading} in {rendered}"))
                })
                .collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "{rendered}");
        assert!(rendered.contains("call_7"), "{rendered}");
        assert!(rendered.contains("medium confidence"), "{rendered}");
    }

    #[test]
    fn a_state_nothing_was_recorded_in_says_so_and_summarizes_to_nothing() {
        let empty = WorkState::default();

        assert!(empty.is_empty());
        assert_eq!(empty.render(), "Work state: the model has not recorded one yet.");
        assert_eq!(empty.summary(), None);
    }

    #[test]
    fn a_done_item_without_evidence_is_refused_by_name() {
        let error = WorkState::from_arguments(&serde_json::json!({
            "goal": "g",
            "next": "n",
            "done": [{ "what": "finished the parser", "evidence": "  " }],
        }))
        .expect_err("an unevidenced done entry");

        assert!(error.to_string().contains("done[0].evidence is empty"), "{error}");
    }

    #[test]
    fn the_arguments_must_carry_a_goal_and_a_next_action() {
        let no_goal = WorkState::from_arguments(&serde_json::json!({"next": "n"}))
            .expect_err("an empty goal");
        assert!(no_goal.to_string().contains("goal is empty"), "{no_goal}");

        let no_next = WorkState::from_arguments(&serde_json::json!({"goal": "g"}))
            .expect_err("an empty next");
        assert!(no_next.to_string().contains("next is empty"), "{no_next}");
    }

    #[test]
    fn a_blocked_state_needs_no_next_action() {
        let state = WorkState::from_arguments(&serde_json::json!({
            "goal": "submit every answer",
            "blocked": "the editor server is not running, and no local replacement exists",
        }))
        .expect("a fully blocked task has no honest next action to invent");

        assert_eq!(state.next, "");
        let rendered = state.render();
        assert!(rendered.contains("Blocked:"), "{rendered}");
        assert!(!rendered.contains("Next:"), "{rendered}");
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_dropped() {
        let error = WorkState::from_arguments(&serde_json::json!({
            "goal": "g",
            "next": "n",
            "mood": "confident",
        }))
        .expect_err("an unknown field");

        assert!(error.to_string().contains("mood"), "{error}");
    }

    #[test]
    fn a_schema_word_of_another_version_is_refused() {
        let error = WorkState::from_arguments(&serde_json::json!({
            "schema": "state/2",
            "goal": "g",
            "next": "n",
        }))
        .expect_err("a schema from another version");

        assert!(error.to_string().contains("state/2"), "{error}");
        assert!(error.to_string().contains("state/1"), "{error}");
    }

    #[test]
    fn a_rendered_state_over_the_limit_is_refused() {
        // Every entry is within its own bound, and together they render past
        // the block's limit: the bounds are per field, the budget is per
        // request.
        let claim = "x".repeat(WorkState::MAX_CLAIM_CHARS);
        let evidence = "y".repeat(WorkState::MAX_EVIDENCE_CHARS);
        let hypotheses: Vec<serde_json::Value> = (0..WorkState::MAX_HYPOTHESES)
            .map(|_| {
                serde_json::json!({
                    "claim": claim,
                    "confidence": "low",
                    "evidence": evidence,
                })
            })
            .collect();
        let error = WorkState::from_arguments(&serde_json::json!({
            "goal": "g",
            "next": "n",
            "hypotheses": hypotheses,
        }))
        .expect_err("a rendered state over the limit");

        assert!(error.to_string().contains("rendered state"), "{error}");
    }

    #[test]
    fn the_summary_carries_the_goal_next_and_blocked_across_turns() {
        let state = WorkState::from_arguments(&a_state()).expect("a valid state");
        let summary = state.summary().expect("a recorded state summarizes");

        assert!(summary.contains("goal: make the airbench submissions pass"), "{summary}");
        assert!(summary.contains("next: read the failing test"), "{summary}");
        assert!(summary.contains("blocked: problem 9"), "{summary}");
        assert!(summary.contains("1 item(s) reported done"), "{summary}");
    }

    #[test]
    fn strings_are_trimmed_before_they_are_measured_and_kept() {
        let state = WorkState::from_arguments(&serde_json::json!({
            "goal": "  ship it  ",
            "next": " run the tests ",
            "open": ["  why is it slow?  "],
        }))
        .expect("a valid state");

        assert_eq!(state.goal, "ship it");
        assert_eq!(state.next, "run the tests");
        assert_eq!(state.open[0], "why is it slow?");
    }

    #[test]
    fn the_tool_definition_is_offered_and_states_its_limits() {
        let definition = tool_definition();

        assert_eq!(definition.name, "state.update");
        assert_eq!(definition.source, ToolSource::Builtin);
        assert_eq!(definition.side_effect, ToolSideEffect::Pure);
        assert!(definition.description.contains("evidence"), "{}", definition.description);
        assert_eq!(definition.input_schema["required"], serde_json::json!(["goal"]));
        assert_eq!(
            definition.input_schema["anyOf"],
            serde_json::json!([
                { "required": ["next"] },
                { "required": ["blocked"] },
            ])
        );
        assert_eq!(
            definition.input_schema["properties"]["done"]["items"]["required"],
            serde_json::json!(["what", "evidence"])
        );
    }
}
