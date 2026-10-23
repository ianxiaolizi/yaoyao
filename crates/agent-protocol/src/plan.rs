//! The session's plan ledger.
//!
//! A plan is what the model says it is doing, kept as a durable fact rather
//! than left in the transcript: the goal, the ordered steps and their status,
//! and the facts later steps must not re-derive. It is written by the loop's
//! own `plan.update` tool (`TOOL_NAME`), which records a `plan/updated` event
//! and nothing else — the transport never sees it.
//!
//! Each plan is a full replacement of the previous one. That is the whole
//! semantics: there are no patches, so what the model last sent is exactly
//! what every later turn reads. The bounds below are part of the contract
//! rather than a style preference: the plan is repeated in every turn's task
//! layer, so an unbounded ledger would grow the context on every step.

use crate::tool::{ToolDefinition, ToolSideEffect, ToolSource};
use serde::{Deserialize, Serialize};

/// The name of the loop-owned tool that writes a plan.
///
/// It is offered in the turn's manifest like any other tool, but no provider
/// implements it: the loop validates the arguments and records the ledger
/// itself. The name is reserved by the app assembly so a plugin cannot claim
/// it.
pub const TOOL_NAME: &str = "plan.update";

/// Where one step of a plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Todo,
    Doing,
    Done,
}

impl PlanItemStatus {
    /// The wire word, matching the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Doing => "doing",
            Self::Done => "done",
        }
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanItemStatus,
}

/// The session's current plan.
///
/// Deserialization is deliberately liberal (every field has a default) because
/// this same shape is read back from the durable log, where a version written
/// by an older build may not have every field. What the *model* sends goes
/// through [`Plan::from_arguments`], which validates the bounds below.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub items: Vec<PlanItem>,
    #[serde(default)]
    pub facts: Vec<String>,
}

impl Plan {
    /// Longest goal, in characters.
    pub const MAX_GOAL_CHARS: usize = 500;
    /// Most steps one plan may carry.
    pub const MAX_ITEMS: usize = 20;
    /// Longest step text, in characters.
    pub const MAX_ITEM_CHARS: usize = 200;
    /// Most established facts one plan may carry.
    pub const MAX_FACTS: usize = 20;
    /// Longest fact, in characters.
    pub const MAX_FACT_CHARS: usize = 300;

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty() && self.items.is_empty() && self.facts.is_empty()
    }

    /// Parse and validate the arguments of a `plan.update` call.
    ///
    /// Strings are trimmed before they are measured and kept, so a model that
    /// pads a line does not spend its budget on whitespace. The error text is
    /// written for the model: it says what is wrong and what to do about it.
    pub fn from_arguments(arguments: &serde_json::Value) -> Result<Self, PlanError> {
        let plan: Self = serde_json::from_value(arguments.clone()).map_err(|error| {
            PlanError::Invalid(format!("arguments do not match the plan schema: {error}"))
        })?;
        let plan = plan.trimmed();
        plan.validate()?;
        Ok(plan)
    }

    /// Check every bound, assuming the strings are already trimmed.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.goal.is_empty() {
            return Err(PlanError::Invalid(String::from(
                "goal is empty; state what this session is trying to achieve in one sentence",
            )));
        }
        bounded("goal", &self.goal, Self::MAX_GOAL_CHARS)?;
        if self.items.len() > Self::MAX_ITEMS {
            return Err(PlanError::Invalid(format!(
                "items has {} entries; the limit is {}. Keep only the steps that still matter.",
                self.items.len(),
                Self::MAX_ITEMS,
            )));
        }
        for (index, item) in self.items.iter().enumerate() {
            if item.text.is_empty() {
                return Err(PlanError::Invalid(format!(
                    "items[{index}].text is empty; every step needs text"
                )));
            }
            bounded(&format!("items[{index}].text"), &item.text, Self::MAX_ITEM_CHARS)?;
        }
        if self.facts.len() > Self::MAX_FACTS {
            return Err(PlanError::Invalid(format!(
                "facts has {} entries; the limit is {}. Keep only what a later step needs.",
                self.facts.len(),
                Self::MAX_FACTS,
            )));
        }
        for (index, fact) in self.facts.iter().enumerate() {
            if fact.is_empty() {
                return Err(PlanError::Invalid(format!(
                    "facts[{index}] is empty; remove it or say what was established"
                )));
            }
            bounded(&format!("facts[{index}]"), fact, Self::MAX_FACT_CHARS)?;
        }
        Ok(())
    }

    /// The plan as the task layer shows it.
    ///
    /// An empty plan is an explicit sentence rather than an empty line: it
    /// tells the model the ledger exists and nothing has been recorded, which
    /// is what a plan tool is for.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::from("Plan: none recorded yet.");
        }
        let mut text = String::from("Plan for this session:");
        if !self.goal.is_empty() {
            text.push_str(&format!("\nGoal: {}", self.goal));
        }
        if !self.items.is_empty() {
            text.push_str("\nSteps:");
            for (index, item) in self.items.iter().enumerate() {
                text.push_str(&format!(
                    "\n{}. [{}] {}",
                    index + 1,
                    item.status.as_str(),
                    item.text
                ));
            }
        }
        if !self.facts.is_empty() {
            text.push_str("\nFacts:");
            for fact in &self.facts {
                text.push_str(&format!("\n- {fact}"));
            }
        }
        text
    }

    fn trimmed(mut self) -> Self {
        self.goal = self.goal.trim().to_string();
        for item in &mut self.items {
            item.text = item.text.trim().to_string();
        }
        for fact in &mut self.facts {
            *fact = fact.trim().to_string();
        }
        self
    }
}

fn bounded(field: &str, text: &str, max: usize) -> Result<(), PlanError> {
    let count = text.chars().count();
    if count > max {
        return Err(PlanError::Invalid(format!(
            "{field} is {count} characters; the limit is {max}. Keep it shorter."
        )));
    }
    Ok(())
}

/// A plan the model sent that cannot be recorded, with the reason to hand back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("{0}")]
    Invalid(String),
}

/// The declaration of the loop-owned `plan.update` tool.
///
/// It reaches the model through the turn's manifest like every other tool, so
/// the bounds are stated here as well as enforced in [`Plan::validate`]: the
/// model should know the limit before it hits it.
pub fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_NAME.to_string(),
        description: "Replace this session's plan: the goal being pursued, the ordered steps \
                      with their status, and facts already established that later steps must \
                      not re-derive. Send the complete plan every time; it replaces the \
                      previous one. Use it when the work takes more than a few steps, and \
                      keep it current as steps finish. The plan is shown to you at the start \
                      of every turn. Limits: goal 500 characters; at most 20 steps of 200 \
                      characters each; at most 20 facts of 300 characters each."
            .to_string(),
        source: ToolSource::Builtin,
        side_effect: ToolSideEffect::Pure,
        capability_requirements: Vec::new(),
        idempotent: true,
        timeout_ms: None,
        input_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["goal"],
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "One sentence: what this session is trying to achieve.",
                    "maxLength": Plan::MAX_GOAL_CHARS,
                },
                "items": {
                    "type": "array",
                    "description": "The ordered steps, most important or earliest first.",
                    "maxItems": Plan::MAX_ITEMS,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["text", "status"],
                        "properties": {
                            "text": {
                                "type": "string",
                                "maxLength": Plan::MAX_ITEM_CHARS,
                            },
                            "status": {
                                "type": "string",
                                "enum": ["todo", "doing", "done"],
                            },
                        },
                    },
                },
                "facts": {
                    "type": "array",
                    "description": "Established facts a later step must not re-derive.",
                    "maxItems": Plan::MAX_FACTS,
                    "items": {
                        "type": "string",
                        "maxLength": Plan::MAX_FACT_CHARS,
                    },
                },
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_plan() -> serde_json::Value {
        serde_json::json!({
            "goal": "ship the parser fix",
            "items": [
                { "text": "read the failing test", "status": "done" },
                { "text": "fix the tokenizer", "status": "doing" },
                { "text": "run the suite", "status": "todo" }
            ],
            "facts": ["the failure is in tokenize()"],
        })
    }

    #[test]
    fn a_plan_round_trips_through_serde() {
        let plan = Plan::from_arguments(&a_plan()).expect("a valid plan");
        let json = serde_json::to_string(&plan).expect("serialize");
        let decoded: Plan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, plan);
        assert_eq!(decoded.items[1].status, PlanItemStatus::Doing);
        assert_eq!(decoded.items[1].status.as_str(), "doing");
    }

    #[test]
    fn strings_are_trimmed_before_they_are_measured_or_kept() {
        let plan = Plan::from_arguments(&serde_json::json!({
            "goal": "  ship it  ",
            "items": [{ "text": " step ", "status": "todo" }],
            "facts": [" fact "],
        }))
        .expect("a valid plan");
        assert_eq!(plan.goal, "ship it");
        assert_eq!(plan.items[0].text, "step");
        assert_eq!(plan.facts[0], "fact");
    }

    #[test]
    fn a_goal_is_required_and_cannot_be_blank() {
        for arguments in [
            serde_json::json!({}),
            serde_json::json!({ "goal": "   " }),
            serde_json::json!({ "goal": "" }),
        ] {
            let error = Plan::from_arguments(&arguments).expect_err("the plan is refused");
            assert!(error.to_string().contains("goal is empty"), "{error}");
        }
    }

    #[test]
    fn unknown_fields_are_refused_by_name() {
        let error = Plan::from_arguments(&serde_json::json!({
            "goal": "ship it",
            "steps": [],
        }))
        .expect_err("the plan is refused");
        let message = error.to_string();
        assert!(message.contains("unknown field"), "{message}");
        assert!(message.contains("steps"), "{message}");
    }

    #[test]
    fn an_item_needs_both_text_and_a_known_status() {
        let missing_status = Plan::from_arguments(&serde_json::json!({
            "goal": "ship it",
            "items": [{ "text": "step" }],
        }))
        .expect_err("the plan is refused");
        assert!(missing_status.to_string().contains("status"), "{missing_status}");

        let unknown_status = Plan::from_arguments(&serde_json::json!({
            "goal": "ship it",
            "items": [{ "text": "step", "status": "blocked" }],
        }))
        .expect_err("the plan is refused");
        assert!(unknown_status.to_string().contains("blocked"), "{unknown_status}");
    }

    #[test]
    fn every_bound_is_enforced_with_an_actionable_message() {
        let long_goal = Plan::from_arguments(&serde_json::json!({
            "goal": "x".repeat(Plan::MAX_GOAL_CHARS + 1),
        }))
        .expect_err("the goal is too long");
        assert!(long_goal.to_string().contains("limit is 500"), "{long_goal}");

        let items: Vec<serde_json::Value> = (0..=Plan::MAX_ITEMS)
            .map(|index| serde_json::json!({ "text": format!("step {index}"), "status": "todo" }))
            .collect();
        let too_many = Plan::from_arguments(&serde_json::json!({ "goal": "g", "items": items }))
            .expect_err("too many steps");
        assert!(too_many.to_string().contains("limit is 20"), "{too_many}");

        let long_item = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [{ "text": "x".repeat(Plan::MAX_ITEM_CHARS + 1), "status": "todo" }],
        }))
        .expect_err("the step is too long");
        assert!(long_item.to_string().contains("items[0].text"), "{long_item}");

        let facts: Vec<String> =
            (0..=Plan::MAX_FACTS).map(|index| format!("fact {index}")).collect();
        let too_many_facts =
            Plan::from_arguments(&serde_json::json!({ "goal": "g", "facts": facts }))
                .expect_err("too many facts");
        assert!(too_many_facts.to_string().contains("limit is 20"), "{too_many_facts}");

        let long_fact = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "facts": ["x".repeat(Plan::MAX_FACT_CHARS + 1)],
        }))
        .expect_err("the fact is too long");
        assert!(long_fact.to_string().contains("facts[0]"), "{long_fact}");
    }

    #[test]
    fn an_empty_plan_renders_as_a_stated_absence() {
        assert_eq!(Plan::default().render(), "Plan: none recorded yet.");
        assert!(Plan::default().is_empty());
    }

    #[test]
    fn a_recorded_plan_renders_goal_steps_and_facts() {
        let plan = Plan::from_arguments(&a_plan()).expect("a valid plan");
        let text = plan.render();
        assert_eq!(
            text,
            "Plan for this session:\n\
             Goal: ship the parser fix\n\
             Steps:\n\
             1. [done] read the failing test\n\
             2. [doing] fix the tokenizer\n\
             3. [todo] run the suite\n\
             Facts:\n\
             - the failure is in tokenize()"
        );
    }

    #[test]
    fn the_tool_declaration_says_what_the_loop_enforces() {
        let definition = tool_definition();
        assert_eq!(definition.name, TOOL_NAME);
        assert_eq!(definition.source, ToolSource::Builtin);
        assert_eq!(definition.side_effect, ToolSideEffect::Pure);
        assert!(definition.idempotent);
        assert!(definition.capability_requirements.is_empty());
        let schema = &definition.input_schema;
        assert_eq!(schema["required"], serde_json::json!(["goal"]));
        assert_eq!(schema["properties"]["goal"]["maxLength"], Plan::MAX_GOAL_CHARS);
        assert_eq!(schema["properties"]["items"]["maxItems"], Plan::MAX_ITEMS);
        assert_eq!(
            schema["properties"]["items"]["items"]["properties"]["status"]["enum"],
            serde_json::json!(["todo", "doing", "done"])
        );
    }
}
