//! Checking a call's arguments against the schema the tool declared.
//!
//! Every tool ships an `input_schema` in the manifest a model reads, so the
//! model is told the shape of every argument before it calls anything. Nothing
//! read that shape back: each provider looked up the fields it needed and
//! answered `missing 'path'` or something else of its own, after the call had
//! been admitted. The same mistake therefore cost a round trip wherever it was
//! caught, and its text differed from tool to tool.
//!
//! This is the one place that enforces the declaration. It runs in the gateway
//! before policy, so a call that could not run is refused before anything asks
//! a person to approve it.
//!
//! It deliberately does **not** replace a provider's own checks. A schema says
//! `fs.edit` takes two strings; only the tool can say the anchor appears
//! exactly once. The schema is the outer bound, and the tool is still the
//! authority on meaning.
//!
//! ## What is refused rather than resolved
//!
//! Compilation is local: the validator is built without the http or file
//! resolvers, so a `$ref` pointing anywhere but inside the document it came in
//! fails to compile and the tool is refused as unenforceable. A declaration
//! that this build cannot decide is not one it may run calls against — "every
//! call is valid" is the worst answer available, and it is what a reader that
//! skips what it does not understand would give.
//!
//! What a meta-schema cannot catch is a keyword misspelled into an annotation:
//! JSON Schema's vocabulary is open, so `propertie` is a legal schema that
//! constrains nothing. That limit is pinned by a test rather than hoped away;
//! the guard against it is a declaration written once by whoever ships the
//! tool, not something a reader of it can recover.
//!
//! Two size limits keep one malicious or merely enormous schema from turning
//! ordinary tool calls into a parsing exercise: the declaration itself, and
//! the argument tree. The argument limit counts nodes rather than bytes,
//! because a legitimate `fs.write` carries a file's worth of text in one
//! string and must not be the thing that gets refused.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// Most bytes one tool's declaration may take. A hand-written schema is a few
/// hundred; a generated one that needs more than this is a manifest problem.
pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;

/// Most nodes the arguments may contain. Deep or wide argument trees are
/// cheap to write and expensive to walk, and no built-in tool takes one.
pub const MAX_ARGUMENT_NODES: usize = 20_000;

/// How many problem sentences one refusal carries. A model that sent a wholly
/// wrong shape does not need the same mistake five hundred times.
pub const MAX_PROBLEMS: usize = 5;

/// Distinct declarations the cache will hold before it is emptied.
const MAX_CACHED_SCHEMAS: usize = 128;

/// Why a call was refused before it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaGap {
    /// The arguments do not match the declaration. Each entry says which field
    /// and what was expected of it.
    Arguments(Vec<String>),
    /// The declaration is not one this build can enforce, and the sentence
    /// says why.
    Declaration(String),
}

/// Compiled declarations, one per distinct schema.
///
/// Compiling is the expensive half and the schema is the same object on every
/// call to a given tool, so the answer is kept. The key is the declaration's
/// own text rather than its name: an MCP server may re-list its tools with a
/// changed schema under an unchanged name, and a name-keyed cache would go on
/// enforcing the old one.
#[derive(Default)]
pub struct SchemaCheck {
    compiled: Mutex<HashMap<String, Result<jsonschema::Validator, String>>>,
}

impl std::fmt::Debug for SchemaCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The cache holds compiled validators, which have no useful rendering
        // and would print as a page of regexes.
        f.debug_struct("SchemaCheck").finish_non_exhaustive()
    }
}

impl SchemaCheck {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuse `arguments` if they do not satisfy `schema`.
    pub fn check(&self, schema: &Value, arguments: &Value) -> Result<(), SchemaGap> {
        let compiled = self.compiled(schema)?;
        if let Some(problems) = node_budget(arguments) {
            return Err(SchemaGap::Arguments(vec![problems]));
        }
        let mut problems = Vec::new();
        for error in compiled.iter_errors(arguments) {
            if problems.len() == MAX_PROBLEMS {
                problems.push("(further problems not listed)".to_string());
                break;
            }
            problems.push(describe(&error));
        }
        if problems.is_empty() { Ok(()) } else { Err(SchemaGap::Arguments(problems)) }
    }

    /// Whether a declaration can be enforced at all, with the reason it cannot.
    ///
    /// Asked at assembly as well as at call time: a server whose tools cannot
    /// be checked is a registration the run should hear about once, not a
    /// surprise on the first call that happens to use one.
    pub fn declaration(&self, schema: &Value) -> Result<(), String> {
        match self.compiled(schema) {
            Ok(_) => Ok(()),
            Err(SchemaGap::Declaration(reason)) => Err(reason),
            // `compiled` has no arguments to look at, so it cannot answer this.
            Err(SchemaGap::Arguments(problems)) => Err(problems.join("; ")),
        }
    }

    fn compiled(&self, schema: &Value) -> Result<jsonschema::Validator, SchemaGap> {
        let text = serde_json::to_string(schema)
            .map_err(|error| SchemaGap::Declaration(format!("it does not serialize: {error}")))?;
        if text.len() > MAX_SCHEMA_BYTES {
            return Err(SchemaGap::Declaration(format!(
                "the declaration is {} bytes, over the {MAX_SCHEMA_BYTES} this build will compile",
                text.len()
            )));
        }
        {
            let cache = self.lock();
            match cache.get(&text) {
                Some(Ok(validator)) => return Ok(validator.clone()),
                Some(Err(reason)) => return Err(SchemaGap::Declaration(reason.clone())),
                None => {}
            }
        }
        let compiled = compile(schema);
        let mut cache = self.lock();
        if cache.len() >= MAX_CACHED_SCHEMAS {
            cache.clear();
        }
        cache.insert(text, compiled.clone());
        compiled.map_err(SchemaGap::Declaration)
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Result<jsonschema::Validator, String>>> {
        // A poisoned lock here means another thread panicked while holding it.
        // The cache holds nothing that a panic could have left half-written —
        // it is replaced wholesale — so the panic does not make the entries
        // unusable, and refusing every tool call afterwards would turn one
        // unrelated panic into a dead session. Take the data back.
        self.compiled.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

fn compile(schema: &Value) -> Result<jsonschema::Validator, String> {
    // The declaration must itself be a schema. Without this, a tool whose
    // `properties` is misspelled compiles into a validator that accepts
    // anything, which looks exactly like a tool with no declaration at all.
    //
    // `validate_for` rather than `validate`: the latter panics when a document
    // names a meta-schema this build does not bundle, and a `$schema` line is
    // something a server writes, not something this build controls.
    jsonschema::meta::validate_for::<jsonschema::json::SerdeJson>(schema).map_err(|error| {
        format!("the declaration is not a valid JSON Schema: {}", describe(&error))
    })?;
    jsonschema::validator_for(schema)
        .map_err(|error| format!("the declaration does not compile: {}", describe(&error)))
}

/// One sentence about one problem: where, and what was expected there.
///
/// The instance path is the half that makes the sentence actionable — "is not
/// of type string" says nothing on its own when a call has six arguments.
fn describe(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error.instance_path().to_string();
    let at = if path.is_empty() { "the arguments themselves".to_string() } else { path };
    format!("{at}: {error}")
}

/// A sentence about the argument tree if it is over budget.
fn node_budget(arguments: &Value) -> Option<String> {
    let mut nodes = 0usize;
    // An explicit stack rather than recursion: the input is a tree a stranger
    // wrote, and the depth it can reach is bounded only by what parsed.
    let mut stack: Vec<&Value> = vec![arguments];
    while let Some(value) = stack.pop() {
        nodes += 1;
        if nodes > MAX_ARGUMENT_NODES {
            return Some(format!(
                "the arguments have more than {MAX_ARGUMENT_NODES} values, which is more than any \
                 tool here takes"
            ));
        }
        match value {
            Value::Object(map) => stack.extend(map.values()),
            Value::Array(items) => stack.extend(items.iter()),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn a_call_that_matches_runs() {
        let check = SchemaCheck::new();
        assert!(check.check(&schema(), &json!({ "path": "a.txt" })).is_ok());
        assert!(check.check(&schema(), &json!({ "path": "a.txt", "limit": 3 })).is_ok());
    }

    #[test]
    fn a_missing_field_is_named_and_the_call_stops() {
        let check = SchemaCheck::new();
        let gap = check.check(&schema(), &json!({})).expect_err("missing path");
        let SchemaGap::Arguments(problems) = gap else { panic!("{gap:?}") };
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("path"), "{problems:?}");
        assert!(problems[0].contains("required"), "{problems:?}");
    }

    #[test]
    fn a_wrong_type_names_the_field_and_what_was_expected() {
        let check = SchemaCheck::new();
        let gap = check
            .check(&schema(), &json!({ "path": "a.txt", "limit": "many" }))
            .expect_err("limit is not an integer");
        let SchemaGap::Arguments(problems) = gap else { panic!("{gap:?}") };
        assert!(problems[0].contains("/limit"), "the field is named: {problems:?}");
        assert!(problems[0].contains("integer"), "the expected type is named: {problems:?}");
    }

    #[test]
    fn an_unknown_field_is_refused_when_the_schema_says_so() {
        let check = SchemaCheck::new();
        let gap = check.check(&schema(), &json!({ "path": "a", "extra": 1 })).expect_err("extra");
        let SchemaGap::Arguments(problems) = gap else { panic!("{gap:?}") };
        assert!(problems[0].contains("extra"), "{problems:?}");
    }

    #[test]
    fn a_declaration_that_is_not_a_schema_is_refused_rather_than_ignored() {
        let check = SchemaCheck::new();
        let broken = json!({ "type": "object", "required": "path" });
        let reason = check.declaration(&broken).expect_err("required must be an array");
        assert!(reason.contains("JSON Schema"), "{reason}");
    }

    #[test]
    fn a_misspelled_keyword_is_not_caught_and_that_is_the_specs_rule() {
        // Pinned as a known limit rather than hoped away. JSON Schema has an
        // open vocabulary: a keyword a draft does not define is an annotation,
        // not an error, so `propertie` is a legal schema that constrains
        // nothing. What guards against it is a definition written by a
        // provider this build compiles, not the meta-schema.
        let check = SchemaCheck::new();
        let misspelled = json!({ "type": "object", "propertie": { "path": { "type": "string" } } });
        assert!(check.declaration(&misspelled).is_ok());
        assert!(
            check.check(&misspelled, &json!({ "anything": 1 })).is_ok(),
            "and it constrains nothing, exactly as the spec says"
        );
    }

    #[test]
    fn a_reference_outside_the_document_is_not_chased() {
        // The validator is built without the http resolver, so this compiles to
        // a refusal — which is the point: a declaration this build cannot
        // resolve is one it must not pretend to enforce.
        let check = SchemaCheck::new();
        let remote = json!({
            "type": "object",
            "properties": { "x": { "$ref": "https://example.test/schema.json" } }
        });
        let reason = check.declaration(&remote).expect_err("a remote reference is not resolved");
        assert!(!reason.is_empty());
    }

    #[test]
    fn an_enormous_declaration_is_refused_by_size() {
        let check = SchemaCheck::new();
        let filler = "x".repeat(MAX_SCHEMA_BYTES);
        let huge = json!({ "type": "object", "description": filler });
        let reason = check.declaration(&huge).expect_err("over budget");
        assert!(reason.contains("over the"), "{reason}");
    }

    #[test]
    fn enormous_arguments_are_refused_before_they_are_walked() {
        let check = SchemaCheck::new();
        // A schema that would accept them, so the refusal can only come from
        // the budget.
        let open = json!({ "type": "object" });
        let wide: Vec<u64> = (0..MAX_ARGUMENT_NODES as u64 + 1).collect();
        let gap = check.check(&open, &json!({ "items": wide })).expect_err("over budget");
        let SchemaGap::Arguments(problems) = gap else { panic!("{gap:?}") };
        assert!(problems[0].contains("more than"), "{problems:?}");
    }

    #[test]
    fn a_large_string_argument_is_not_a_large_argument_tree() {
        // `fs.write` carries a file in one value; the limit counts values, so
        // size in bytes is not what it refuses.
        let check = SchemaCheck::new();
        let open = json!({ "type": "object" });
        let content = "x".repeat(4 * 1024 * 1024);
        assert!(check.check(&open, &json!({ "content": content })).is_ok());
    }

    #[test]
    fn only_a_few_problems_are_listed() {
        let check = SchemaCheck::new();
        // One error per field, so the cap has something to cut: a model that
        // sent seven wrong values needs the fields named, not the same
        // sentence seven times.
        let strict = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" }, "b": { "type": "string" },
                "c": { "type": "string" }, "d": { "type": "string" },
                "e": { "type": "string" }, "f": { "type": "string" },
                "g": { "type": "string" }
            }
        });
        let many = json!({ "a": 1, "b": 2, "c": 3, "d": 4, "e": 5, "f": 6, "g": 7 });
        let gap = check.check(&strict, &many).expect_err("all wrong");
        let SchemaGap::Arguments(problems) = gap else { panic!("{gap:?}") };
        assert_eq!(problems.len(), MAX_PROBLEMS + 1, "{problems:?}");
        assert!(problems.last().expect("a last line").contains("not listed"));
    }

    #[test]
    fn a_changed_declaration_is_recompiled_rather_than_remembered() {
        let check = SchemaCheck::new();
        let first = json!({ "type": "object", "required": ["a"] });
        let second = json!({ "type": "object", "required": ["b"] });
        assert!(check.check(&first, &json!({ "a": 1 })).is_ok());
        assert!(check.check(&second, &json!({ "a": 1 })).is_err(), "the second rule applies");
        assert!(check.check(&first, &json!({ "a": 1 })).is_ok(), "and the first still does");
    }
}
