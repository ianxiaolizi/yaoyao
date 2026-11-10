//! Verifier recipes: named checks an operator writes once and runs by name.
//!
//! A recipe is a [`CheckSpec`] with one hole in it — the artifact to check —
//! and it lives in the **user** configuration only. That placement is the
//! point: the workspace layer is written by whoever owns the repository, and a
//! check a workspace could define is a check the work being graded could write
//! for itself. The same reasoning that keeps `capabilities` out of
//! `[workspace]` keeps `[verify]` out of it, and `deny_unknown_fields` turns
//! the attempt into a parse error rather than a section that quietly does
//! nothing.
//!
//! The program is named and its arguments are kept apart, exactly as in
//! [`CheckSpec`]: this type expands into one, and it has no field a shell could
//! be tucked into.

use std::collections::BTreeMap;

use agent_protocol::apparatus::ApparatusSpec;
use agent_protocol::verification::{CheckSpec, DEFAULT_CHECK_TIMEOUT_MS, Expectation};
use serde::Deserialize;

/// The one placeholder a recipe argument may carry.
///
/// It is substituted with the `--target` value at expansion time. Exactly one
/// placeholder is supported on purpose: a pattern language here would be a
/// second command syntax, and a check whose arguments cannot be read off the
/// configuration is a check nobody can review.
pub const TARGET_PLACEHOLDER: &str = "{target}";

/// The `[verify]` section of a user configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifySpec {
    /// Checks by name, run with `agent verify --recipe <name>`.
    #[serde(default)]
    pub recipes: BTreeMap<String, RecipeSpec>,
}

/// One recipe, as the user configuration spells it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSpec {
    /// The executable, named as the environment resolves it.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory, relative to the workspace root.
    pub cwd: Option<String>,
    /// What one run may spend, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// The exit status a passing run reports. Zero when unstated.
    pub expect_exit: Option<i32>,
    /// A substring a passing run must print on stdout.
    pub expect_stdout: Option<String>,
    /// The test count a passing run must report.
    pub expect_tests: Option<u64>,
    /// The files the check is graded against, relative to the workspace root.
    #[serde(default)]
    pub apparatus: ApparatusSpec,
}

impl RecipeSpec {
    /// Expand this recipe into the check it names.
    ///
    /// `recipe` is the name it was looked up under, so every refusal can say
    /// which recipe could not be used. `target` is the artifact the caller
    /// named; when an argument carries [`TARGET_PLACEHOLDER`] it is substituted
    /// there, and when it does not the target is appended as the last argument.
    /// A recipe that names no placeholder and is given no target is still a
    /// check — it just does not depend on an artifact.
    pub fn to_check_spec(&self, recipe: &str, target: Option<&str>) -> Result<CheckSpec, String> {
        if recipe.is_empty() || recipe.chars().any(char::is_whitespace) {
            return Err(format!(
                "recipe name '{recipe}' cannot be used: a name is one word, so it can be written \
                 after `--recipe`"
            ));
        }
        if self.program.trim().is_empty() {
            return Err(format!("recipe '{recipe}' names no program to run"));
        }
        let timeout_ms = self.timeout_ms.unwrap_or(DEFAULT_CHECK_TIMEOUT_MS);
        if timeout_ms == 0 {
            return Err(format!(
                "recipe '{recipe}' sets a timeout of zero, which would cut every check off \
                 before it starts"
            ));
        }

        let mut args = Vec::with_capacity(self.args.len() + 1);
        let mut used_target = false;
        for arg in &self.args {
            check_placeholders(recipe, arg)?;
            match target {
                Some(target) => {
                    if arg.contains(TARGET_PLACEHOLDER) {
                        used_target = true;
                    }
                    args.push(arg.replace(TARGET_PLACEHOLDER, target));
                }
                None => args.push(arg.clone()),
            }
        }
        match target {
            Some(target) => {
                if !used_target {
                    args.push(target.to_string());
                }
            }
            None => {
                if self.args.iter().any(|arg| arg.contains(TARGET_PLACEHOLDER)) {
                    return Err(format!(
                        "recipe '{recipe}' names {TARGET_PLACEHOLDER}, so it needs a target; \
                         write `--target <artifact>`"
                    ));
                }
            }
        }

        Ok(CheckSpec {
            program: self.program.clone(),
            args,
            cwd: self.cwd.clone(),
            timeout_ms,
            apparatus: self.apparatus.clone(),
            expect: Expectation {
                exit_code: self.expect_exit.unwrap_or(0),
                stdout_contains: self.expect_stdout.clone(),
                tests_passed: self.expect_tests,
            },
        })
    }
}

/// Refuse any brace-delimited word that is not [`TARGET_PLACEHOLDER`].
///
/// Recipes are configuration, and a misspelled placeholder (`{targt}`) would
/// otherwise reach the program as a literal argument that reads as a file name.
/// The refusal names the one placeholder this build understands.
fn check_placeholders(recipe: &str, arg: &str) -> Result<(), String> {
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(format!(
                "recipe '{recipe}' has an unclosed '{{' in argument '{arg}'; the only placeholder \
                 this build substitutes is {TARGET_PLACEHOLDER}"
            ));
        };
        let name = &after[..close];
        if !name.is_empty() && !name.contains('{') && format!("{{{name}}}") != TARGET_PLACEHOLDER {
            return Err(format!(
                "recipe '{recipe}' uses unknown placeholder '{{{name}}}'; the only placeholder \
                 this build substitutes is {TARGET_PLACEHOLDER}"
            ));
        }
        rest = &after[close + 1..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfigError, UserConfig, WorkspaceConfig};

    fn recipe(text: &str) -> Result<RecipeSpec, ConfigError> {
        let config = UserConfig::from_toml_str(&format!("[verify.recipes.web]\n{text}\n"))?;
        Ok(config.verify.expect("the section").recipes["web"].clone())
    }

    #[test]
    fn a_user_configuration_defines_a_recipe() {
        let config = UserConfig::from_toml_str(
            r#"
[verify.recipes.web]
program = "node"
args = ["scripts/verify/web.mjs", "--target", "{target}"]
timeout_ms = 60000
expect_stdout = "CHECK_OK"
"#,
        )
        .expect("a recipe in the user layer");
        let web = &config.verify.expect("the section").recipes["web"];
        assert_eq!(web.program, "node");
        assert_eq!(web.args, ["scripts/verify/web.mjs", "--target", "{target}"]);
        assert_eq!(web.timeout_ms, Some(60_000));
        assert_eq!(web.expect_stdout.as_deref(), Some("CHECK_OK"));
    }

    #[test]
    fn a_project_may_not_define_a_check() {
        // A project that could name a program to run would be a project that
        // grades itself. The refusal is a parse error, not a silent drop.
        let error = WorkspaceConfig::from_toml_str(
            "[verify.recipes.web]\nprogram = \"node\"\nargs = [\"pass.mjs\"]\n",
        )
        .expect_err("a workspace may not define a recipe");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn a_target_fills_the_placeholder_in_place() {
        let spec = recipe(
            "program = \"node\"\nargs = [\"check.mjs\", \"--target\", \"{target}\", \"--json\"]",
        )
        .expect("a recipe")
        .to_check_spec("web", Some("out/index.html"))
        .expect("a spec");
        assert_eq!(spec.args, ["check.mjs", "--target", "out/index.html", "--json"]);
        assert_eq!(spec.timeout_ms, DEFAULT_CHECK_TIMEOUT_MS, "the shipped deadline");
        assert_eq!(spec.expect, Expectation::default());
    }

    #[test]
    fn a_target_with_no_placeholder_is_appended() {
        let spec = recipe("program = \"node\"\nargs = [\"check.mjs\"]")
            .expect("a recipe")
            .to_check_spec("web", Some("out/index.html"))
            .expect("a spec");
        assert_eq!(spec.args, ["check.mjs", "out/index.html"]);

        // And a recipe that names no artifact is still a check.
        let no_target = recipe("program = \"node\"\nargs = [\"check.mjs\"]")
            .expect("a recipe")
            .to_check_spec("web", None)
            .expect("a spec");
        assert_eq!(no_target.args, ["check.mjs"]);
    }

    #[test]
    fn an_unknown_placeholder_is_refused_and_the_one_that_works_is_named() {
        let error = recipe("program = \"node\"\nargs = [\"--file\", \"{targt}\"]")
            .expect("a recipe")
            .to_check_spec("web", Some("out/index.html"))
            .expect_err("a misspelled placeholder must not reach the program");
        assert!(error.contains("{targt}"), "{error}");
        assert!(error.contains(TARGET_PLACEHOLDER), "{error}");

        let unclosed = recipe("program = \"node\"\nargs = [\"--file\", \"{target\"]")
            .expect("a recipe")
            .to_check_spec("web", Some("x"))
            .expect_err("an unclosed brace is a typo");
        assert!(unclosed.contains("unclosed"), "{unclosed}");
    }

    #[test]
    fn a_recipe_that_needs_a_target_says_so_when_none_is_given() {
        let error = recipe("program = \"node\"\nargs = [\"--target\", \"{target}\"]")
            .expect("a recipe")
            .to_check_spec("web", None)
            .expect_err("the placeholder has nothing to hold");
        assert!(error.contains("--target"), "{error}");
    }

    #[test]
    fn a_recipe_with_nothing_to_run_is_refused() {
        let empty = recipe("program = \"\"\nargs = []")
            .expect("a recipe")
            .to_check_spec("web", None)
            .expect_err("an empty program is not a check");
        assert!(empty.contains("no program"), "{empty}");

        let zero = recipe("program = \"node\"\ntimeout_ms = 0")
            .expect("a recipe")
            .to_check_spec("web", None)
            .expect_err("zero is a check that never starts");
        assert!(zero.contains("zero"), "{zero}");

        let spaced = recipe("program = \"node\"")
            .expect("a recipe")
            .to_check_spec("my web", None)
            .expect_err("a name with a space cannot be written after --recipe");
        assert!(spaced.contains("one word"), "{spaced}");
    }

    #[test]
    fn the_expectations_and_apparatus_come_from_the_recipe() {
        let spec = recipe(
            "program = \"node\"\n\
             expect_exit = 2\nexpect_stdout = \"CHECK_OK\"\nexpect_tests = 3\n\
             apparatus = { include = [\"checks/**\"] }\n",
        )
        .expect("a recipe")
        .to_check_spec("web", None)
        .expect("a spec");
        assert_eq!(spec.expect.exit_code, 2);
        assert_eq!(spec.expect.stdout_contains.as_deref(), Some("CHECK_OK"));
        assert_eq!(spec.expect.tests_passed, Some(3));
        assert_eq!(spec.apparatus.include, ["checks/**"]);
    }

    #[test]
    fn a_misspelled_recipe_key_is_a_refusal() {
        let error =
            UserConfig::from_toml_str("[verify.recipes.web]\nprogram = \"node\"\nargz = []\n")
                .expect_err("a field nobody reads is a field somebody meant");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }
}
