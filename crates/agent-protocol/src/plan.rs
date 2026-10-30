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
//!
//! ## Steps may wait on steps
//!
//! A step may name itself (`id`) and say which other steps must be `done`
//! before it can start (`depends_on`). The ledger is then read as a dependency
//! graph rather than as a list: [`Plan::ready`] answers which open steps may
//! start now, and the task layer shows that line. Nothing here *executes* the
//! graph — a plan is still what the model says it is doing, and the model is
//! still the one that acts. What the graph buys today is that the model has to
//! write its ordering down where the later steps read it, and that an ordering
//! which cannot be finished (a cycle) is refused at the moment it is written
//! instead of being discovered halfway through.
//!
//! Ids are optional, so every plan written before this field existed is still
//! a valid plan and still reads back unchanged: no id, no dependencies, and
//! therefore no ordering stated beyond the written order of the steps.

use std::collections::BTreeMap;

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
    /// A name other steps can wait on, if this step has one.
    ///
    /// Optional, and absent rather than empty when a step has no name: a plan
    /// written before ids existed keeps its exact bytes, and a model that does
    /// not need ordering does not have to invent names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The ids of steps that must be `done` before this one can start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
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
    /// Longest step id, in characters.
    pub const MAX_ID_CHARS: usize = 40;
    /// Most dependencies one step may declare.
    pub const MAX_DEPENDS: usize = 20;

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty() && self.items.is_empty() && self.facts.is_empty()
    }

    /// Whether any step waits on another.
    ///
    /// A plan without dependencies is rendered exactly as it was before steps
    /// could have them: the written order is the only ordering it states.
    pub fn has_dependencies(&self) -> bool {
        self.items.iter().any(|item| !item.depends_on.is_empty())
    }

    /// The open steps that may start now: `todo`, and every dependency `done`.
    ///
    /// `doing` is not ready — it has already started — and `done` is not
    /// ready because there is nothing left to do. A dependency the plan does
    /// not contain is treated as not done: a plan read back from a log is not
    /// re-validated, and the safe reading of a missing step is "not finished".
    pub fn ready(&self) -> Vec<&PlanItem> {
        self.ready_indices().into_iter().map(|index| &self.items[index]).collect()
    }

    /// The indices of [`Self::ready`]'s steps, in the order the plan lists
    /// them.
    fn ready_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.status == PlanItemStatus::Todo)
            .filter(|(_, item)| item.depends_on.iter().all(|dep| self.is_done(dep)))
            .map(|(index, _)| index)
            .collect()
    }

    /// Whether the step named `id` exists and is `done`.
    fn is_done(&self, id: &str) -> bool {
        self.items
            .iter()
            .any(|item| item.id.as_deref() == Some(id) && item.status == PlanItemStatus::Done)
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
        self.validate_graph()
    }

    /// Check the dependency graph: ids are unique names, every dependency
    /// names a step that exists, and no step waits — directly or through
    /// others — on itself.
    ///
    /// A plan that cannot be finished is refused here rather than discovered
    /// halfway through, and every message says what to change. The shape is
    /// checked strictly while the *statuses* are not: a `doing` step with a
    /// dependency that is not done is a contradiction the model may need to
    /// state (a dependency stalled under it), so it renders as waiting rather
    /// than being refused.
    fn validate_graph(&self) -> Result<(), PlanError> {
        let mut by_id: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, item) in self.items.iter().enumerate() {
            let Some(id) = item.id.as_deref() else { continue };
            if id.is_empty() {
                return Err(PlanError::Invalid(format!(
                    "items[{index}].id is empty; give it a name or drop the field"
                )));
            }
            bounded(&format!("items[{index}].id"), id, Self::MAX_ID_CHARS)?;
            if id.chars().any(char::is_whitespace) {
                return Err(PlanError::Invalid(format!(
                    "items[{index}].id is {id:?}; an id is one word — use '-' or '_' instead of spaces"
                )));
            }
            if by_id.insert(id, index).is_some() {
                return Err(PlanError::Invalid(format!(
                    "items[{index}].id is {id:?}, which another step already has; ids must be unique"
                )));
            }
        }

        for (index, item) in self.items.iter().enumerate() {
            if item.depends_on.len() > Self::MAX_DEPENDS {
                return Err(PlanError::Invalid(format!(
                    "items[{index}].depends_on has {} entries; the limit is {}. A step that waits \
                     on everything is not an ordering.",
                    item.depends_on.len(),
                    Self::MAX_DEPENDS,
                )));
            }
            for (position, dep) in item.depends_on.iter().enumerate() {
                let field = format!("items[{index}].depends_on[{position}]");
                if dep.is_empty() {
                    return Err(PlanError::Invalid(format!(
                        "{field} is empty; name the step this one waits on or remove it"
                    )));
                }
                bounded(&field, dep, Self::MAX_ID_CHARS)?;
                if dep.chars().any(char::is_whitespace) {
                    return Err(PlanError::Invalid(format!(
                        "{field} is {dep:?}; an id is one word — use '-' or '_' instead of spaces"
                    )));
                }
                if item.id.as_deref() == Some(dep.as_str()) {
                    return Err(PlanError::Invalid(format!(
                        "{field} names this step's own id {dep:?}; a step cannot wait on itself"
                    )));
                }
                if !by_id.contains_key(dep.as_str()) {
                    let known: Vec<&str> = by_id.keys().copied().collect();
                    let known = if known.is_empty() {
                        String::from("no step has an id yet; give the step it waits on an `id`")
                    } else {
                        format!("the ids in this plan are: {}", known.join(", "))
                    };
                    return Err(PlanError::Invalid(format!(
                        "{field} names {dep:?}, which is not a step in this plan; {known}"
                    )));
                }
                if item.depends_on[..position].contains(dep) {
                    return Err(PlanError::Invalid(format!(
                        "{field} repeats {dep:?}; list each step it waits on once"
                    )));
                }
            }
        }

        match self.find_cycle() {
            Some(cycle) => Err(PlanError::Invalid(format!(
                "the dependencies form a cycle: {}; a plan must be finishable, so remove the \
                 step that closes the loop",
                cycle.join(" -> ")
            ))),
            None => Ok(()),
        }
    }

    /// One dependency cycle, as the labels of the steps that form it, or
    /// `None` when the graph is acyclic.
    ///
    /// Only a step with an `id` can be waited on, so only such a step can
    /// close a loop. The search still starts from every step: the step that
    /// declared the edge is part of what the message should show.
    fn find_cycle(&self) -> Option<Vec<String>> {
        let by_id: BTreeMap<&str, usize> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.id.as_deref().map(|id| (id, index)))
            .collect();
        let mut marks = vec![Mark::Unseen; self.items.len()];
        let mut stack = Vec::new();
        for index in 0..self.items.len() {
            if marks[index] == Mark::Unseen
                && let Some(cycle) = self.walk(index, &by_id, &mut marks, &mut stack)
            {
                return Some(cycle);
            }
        }
        None
    }

    fn walk(
        &self,
        index: usize,
        by_id: &BTreeMap<&str, usize>,
        marks: &mut [Mark],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<String>> {
        marks[index] = Mark::Open;
        stack.push(index);
        for dep in &self.items[index].depends_on {
            let Some(&target) = by_id.get(dep.as_str()) else { continue };
            match marks[target] {
                Mark::Open => {
                    let start = stack.iter().position(|&node| node == target).unwrap_or(0);
                    let mut cycle: Vec<String> =
                        stack[start..].iter().map(|&node| self.label(node)).collect();
                    cycle.push(self.label(target));
                    return Some(cycle);
                }
                Mark::Unseen => {
                    if let Some(cycle) = self.walk(target, by_id, marks, stack) {
                        return Some(cycle);
                    }
                }
                Mark::Closed => {}
            }
        }
        stack.pop();
        marks[index] = Mark::Closed;
        None
    }

    /// What a step is called in a message: its id, or where it sits.
    fn label(&self, index: usize) -> String {
        self.items[index].id.clone().unwrap_or_else(|| format!("step {}", index + 1))
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
                // The name and the waits are shown only where they exist, so a
                // plan that uses neither reads exactly as it always did.
                let mut notes = Vec::new();
                if let Some(id) = &item.id {
                    notes.push(format!("id: {id}"));
                }
                if !item.depends_on.is_empty() {
                    notes.push(format!("after: {}", item.depends_on.join(", ")));
                }
                if !notes.is_empty() {
                    text.push_str(&format!(" ({})", notes.join(", ")));
                }
            }
            // Only a plan that states an ordering gets the readiness line: on
            // a plain list it would say every `todo` step is ready, which the
            // list already says.
            if self.has_dependencies() {
                let ready = self.ready_indices();
                if ready.is_empty() {
                    text.push_str("\nReady now: none; every open step is waiting on another.");
                } else {
                    let names: Vec<String> = ready
                        .into_iter()
                        .map(|index| match &self.items[index].id {
                            Some(id) => format!("{} ({id})", index + 1),
                            None => format!("{}", index + 1),
                        })
                        .collect();
                    text.push_str(&format!("\nReady now: {}", names.join(", ")));
                }
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
            item.id = item.id.as_mut().map(|id| id.trim().to_string());
            for dep in &mut item.depends_on {
                *dep = dep.trim().to_string();
            }
        }
        for fact in &mut self.facts {
            *fact = fact.trim().to_string();
        }
        self
    }
}

/// A step's colour during the cycle search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Unseen,
    /// On the search path: reaching it again closes a cycle.
    Open,
    /// Fully explored and not part of a cycle from here.
    Closed,
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
                      of every turn. A step may name itself with `id` and list the `id`s of \
                      steps that must be done before it can start in `depends_on`; the plan \
                      then shows which steps are ready now. Limits: goal 500 characters; at \
                      most 20 steps of 200 characters each; at most 20 facts of 300 characters \
                      each; ids one word up to 40 characters; at most 20 dependencies per \
                      step, each naming a step in this plan; dependencies must not form a \
                      cycle."
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
                            "id": {
                                "type": "string",
                                "description": "A name this step can be referred to by: one \
                                                word, no spaces.",
                                "maxLength": Plan::MAX_ID_CHARS,
                            },
                            "depends_on": {
                                "type": "array",
                                "description": "The ids of steps that must be done before this \
                                                one can start.",
                                "maxItems": Plan::MAX_DEPENDS,
                                "items": {
                                    "type": "string",
                                    "maxLength": Plan::MAX_ID_CHARS,
                                },
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
    fn a_step_may_name_itself_and_what_it_waits_on() {
        let plan = Plan::from_arguments(&serde_json::json!({
            "goal": "ship it",
            "items": [
                { "text": " read ", "status": "done", "id": " read " },
                { "text": "fix", "status": "todo", "depends_on": [" read "] },
            ],
        }))
        .expect("a plan with dependencies");

        assert_eq!(plan.items[0].id.as_deref(), Some("read"));
        assert!(plan.items[0].depends_on.is_empty());
        assert_eq!(plan.items[1].id, None);
        assert_eq!(plan.items[1].depends_on, vec!["read"]);

        // The graph survives the log, and a step without a name stays without
        // one rather than becoming an empty string.
        let json = serde_json::to_value(&plan).expect("serialize");
        assert_eq!(json["items"][1]["depends_on"], serde_json::json!(["read"]));
        assert!(json["items"][1].get("id").is_none(), "{json}");
        assert!(json["items"][0].get("depends_on").is_none(), "{json}");
        let decoded: Plan = serde_json::from_value(json).expect("deserialize");
        assert_eq!(decoded, plan);
    }

    #[test]
    fn a_duplicate_id_is_refused() {
        let error = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "one", "status": "todo", "id": "read" },
                { "text": "two", "status": "todo", "id": "read" },
            ],
        }))
        .expect_err("two steps cannot share a name");
        assert!(error.to_string().contains("items[1].id"), "{error}");
        assert!(error.to_string().contains("unique"), "{error}");
    }

    #[test]
    fn a_dependency_that_names_no_step_is_refused_with_the_ids_it_could_have_meant() {
        let error = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "one", "status": "done", "id": "read" },
                { "text": "two", "status": "todo", "depends_on": ["reed"] },
            ],
        }))
        .expect_err("a step cannot wait on a step that does not exist");
        let message = error.to_string();
        assert!(message.contains("items[1].depends_on[0]"), "{message}");
        assert!(message.contains("\"reed\""), "{message}");
        assert!(message.contains("read"), "it lists the ids the plan does have: {message}");

        // With no ids at all, the message says how to make one rather than
        // listing an empty set.
        let bare = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [{ "text": "one", "status": "todo", "depends_on": ["x"] }],
        }))
        .expect_err("nothing can be waited on yet");
        assert!(bare.to_string().contains("no step has an id"), "{bare}");
    }

    #[test]
    fn a_step_cannot_wait_on_itself() {
        let error = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [{ "text": "one", "status": "todo", "id": "a", "depends_on": ["a"] }],
        }))
        .expect_err("a step cannot wait on itself");
        assert!(error.to_string().contains("wait on itself"), "{error}");
    }

    #[test]
    fn a_cycle_is_refused_and_the_loop_is_named() {
        let error = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "one", "status": "todo", "id": "a", "depends_on": ["b"] },
                { "text": "two", "status": "todo", "id": "b", "depends_on": ["a"] },
            ],
        }))
        .expect_err("a plan that can never be finished is not a plan");
        let message = error.to_string();
        assert!(message.contains("cycle"), "{message}");
        assert!(message.contains("a -> b -> a"), "{message}");

        // A cycle need not be a two-step loop, and a step that merely points
        // into one is not itself part of the reported path.
        let long = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "lead", "status": "todo", "id": "lead", "depends_on": ["a"] },
                { "text": "one", "status": "todo", "id": "a", "depends_on": ["b"] },
                { "text": "two", "status": "todo", "id": "b", "depends_on": ["c"] },
                { "text": "three", "status": "todo", "id": "c", "depends_on": ["a"] },
            ],
        }))
        .expect_err("a three-step cycle is a cycle");
        let message = long.to_string();
        assert!(message.contains("a -> b -> c -> a"), "{message}");
        assert!(!message.contains("lead"), "the entry point is not part of the loop: {message}");
    }

    #[test]
    fn an_id_that_is_not_one_word_is_refused() {
        let blank = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [{ "text": "one", "status": "todo", "id": "   " }],
        }))
        .expect_err("an empty id is not a name");
        assert!(blank.to_string().contains("id is empty"), "{blank}");

        let spaced = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [{ "text": "one", "status": "todo", "id": "read the test" }],
        }))
        .expect_err("an id with a space is two words");
        assert!(spaced.to_string().contains("one word"), "{spaced}");

        let long = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [{ "text": "one", "status": "todo", "id": "x".repeat(Plan::MAX_ID_CHARS + 1) }],
        }))
        .expect_err("an id has a bound too");
        assert!(long.to_string().contains("limit is 40"), "{long}");

        let dep_spaced = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "one", "status": "todo", "id": "a" },
                { "text": "two", "status": "todo", "depends_on": ["a b"] },
            ],
        }))
        .expect_err("a dependency id is a name too");
        assert!(dep_spaced.to_string().contains("one word"), "{dep_spaced}");
    }

    #[test]
    fn a_dependency_listed_twice_is_refused() {
        let error = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "one", "status": "todo", "id": "a" },
                { "text": "two", "status": "todo", "depends_on": ["a", "a"] },
            ],
        }))
        .expect_err("waiting on the same step twice says nothing new");
        assert!(error.to_string().contains("repeats"), "{error}");
    }

    #[test]
    fn only_steps_whose_dependencies_are_done_are_ready() {
        let plan = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "read", "status": "done", "id": "read" },
                { "text": "fix", "status": "todo", "id": "fix", "depends_on": ["read"] },
                { "text": "review", "status": "todo", "id": "review", "depends_on": ["fix"] },
                { "text": "start", "status": "doing" },
            ],
        }))
        .expect("a valid plan");

        // A done step is finished, a doing one has started, and a todo step
        // whose dependency is not done is still waiting.
        let ready: Vec<&str> = plan.ready().iter().map(|item| item.text.as_str()).collect();
        assert_eq!(ready, ["fix"]);
        assert!(plan.has_dependencies());
    }

    #[test]
    fn a_plan_with_dependencies_renders_names_waits_and_what_is_ready() {
        let plan = Plan::from_arguments(&serde_json::json!({
            "goal": "ship it",
            "items": [
                { "text": "read the test", "status": "done", "id": "read" },
                { "text": "fix it", "status": "todo", "id": "fix", "depends_on": ["read"] },
                { "text": "run the suite", "status": "todo", "depends_on": ["fix"] },
            ],
        }))
        .expect("a valid plan");
        assert_eq!(
            plan.render(),
            "Plan for this session:\n\
             Goal: ship it\n\
             Steps:\n\
             1. [done] read the test (id: read)\n\
             2. [todo] fix it (id: fix, after: read)\n\
             3. [todo] run the suite (after: fix)\n\
             Ready now: 2 (fix)"
        );
    }

    #[test]
    fn a_plan_whose_open_steps_all_wait_says_so() {
        let plan = Plan::from_arguments(&serde_json::json!({
            "goal": "g",
            "items": [
                { "text": "started", "status": "doing", "id": "a" },
                { "text": "later", "status": "todo", "id": "b", "depends_on": ["a"] },
            ],
        }))
        .expect("a valid plan");
        assert!(plan.ready().is_empty());
        assert!(
            plan.render().ends_with("Ready now: none; every open step is waiting on another."),
            "{}",
            plan.render()
        );
    }

    #[test]
    fn a_plan_without_dependencies_never_prints_a_ready_line() {
        // The readiness line exists because a graph has an answer the list
        // does not: on a plain list every open step is ready, which the list
        // already says.
        let plan = Plan::from_arguments(&a_plan()).expect("a valid plan");
        assert!(!plan.has_dependencies());
        assert!(!plan.render().contains("Ready now"), "{}", plan.render());
    }

    #[test]
    fn the_tool_declaration_states_the_dependency_bounds() {
        let definition = tool_definition();
        assert!(definition.description.contains("depends_on"), "{}", definition.description);
        assert!(definition.description.contains("cycle"), "{}", definition.description);
        let step = &definition.input_schema["properties"]["items"]["items"]["properties"];
        assert_eq!(step["id"]["maxLength"], Plan::MAX_ID_CHARS);
        assert_eq!(step["depends_on"]["maxItems"], Plan::MAX_DEPENDS);
        assert_eq!(step["depends_on"]["items"]["maxLength"], Plan::MAX_ID_CHARS);
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
