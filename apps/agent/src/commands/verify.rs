//! `verify`: run a structured check and record what it found.
//!
//! The program and its arguments come after `--`, and everything before it is
//! this command's own flags. The separator is required rather than optional:
//! `cargo test --workspace` is a program followed by a flag that `verify` does
//! not define, and guessing which side of the line a word belongs to would make
//! two different checks print the same way.
//!
//! Why this exists at all: the completion criterion is a shell command, so a
//! command ending in `| tail` reports `tail`'s status and a failing suite reads
//! as a pass (CONTRACT §7 #170). This command starts the program directly and
//! records the program's own status beside digests of what it printed, so the
//! next completion claim has something to stand on that a pipe cannot fake.

use std::collections::BTreeMap;

use agent_cli::args::Args;
use agent_config::RecipeSpec;
use agent_protocol::apparatus::ApparatusSpec;
use agent_protocol::verification::{CheckSpec, DEFAULT_CHECK_TIMEOUT_MS, Expectation};

use crate::config;
use crate::support::{assemble, chain_start, watch};
use crate::{EXIT_DID_NOT_RUN, exit_on};

pub(crate) async fn verify(args: &Args) {
    let workspace_root = crate::support::workspace_root();
    let mut spec = parse(args, &workspace_root);
    let runtime = assemble(args).await;
    // A profile that names its apparatus is saying what every check of its runs
    // is graded against; a check that names its own overrides that, because the
    // operator asking for this one check is the more specific voice.
    if spec.apparatus.is_empty()
        && let Some(completion) = runtime.profile_snapshot().completion.as_ref()
    {
        spec.apparatus.include = completion.apparatus.clone();
    }
    let from = chain_start(runtime.store.as_ref(), args).await;

    let outcome = watch(
        &runtime,
        args,
        from,
        agent_cli::run_verify(
            runtime.store.as_ref(),
            runtime.verifier.as_ref(),
            &args.session,
            &spec,
            "operator",
            agent::clock::now(),
            args.json,
        ),
    )
    .await;
    exit_on(outcome.verdict);
}

/// Read the check off the command line, or say what is missing.
///
/// Every refusal names the word that was wrong and the shape that would have
/// worked: a caller who mistyped `--expect-test` has nothing to change if the
/// answer is only "usage: agent verify".
///
/// Two sources: a program after `--`, or a named recipe from the user
/// configuration. They are exclusive — a recipe already names a program, and
/// two programs on one line would leave which one ran a question the log could
/// not answer.
fn parse(args: &Args, workspace_root: &str) -> CheckSpec {
    let parsed = parse_words(args.command_args());
    match parsed.source {
        Source::Program { .. } => expand(parsed, None),
        Source::Recipe { .. } => {
            let recipes = config::verify_recipes(workspace_root)
                .unwrap_or_else(|error| usage_failure(&error.to_string()));
            expand(parsed, Some(&recipes))
        }
    }
}

/// What the command line said, before a recipe is looked up.
struct Parsed {
    source: Source,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    expect_exit: Option<i32>,
    expect_stdout: Option<String>,
    expect_tests: Option<u64>,
    apparatus: ApparatusSpec,
}

enum Source {
    /// The program and its arguments after `--`.
    Program { program: String, args: Vec<String> },
    /// A recipe name, and the artifact it is checked against.
    Recipe { name: String, target: Option<String> },
}

fn parse_words(words: &[String]) -> Parsed {
    let mut cwd = None;
    let mut timeout_ms = None;
    let mut expect_exit = None;
    let mut expect_stdout = None;
    let mut expect_tests = None;
    let mut recipe: Option<String> = None;
    let mut target: Option<String> = None;
    let mut program: Option<String> = None;
    let mut program_args: Vec<String> = Vec::new();
    let mut apparatus = ApparatusSpec::default();
    let mut after_separator = false;

    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        if after_separator {
            match &program {
                None => program = Some(word.clone()),
                Some(_) => program_args.push(word.clone()),
            }
            index += 1;
            continue;
        }
        // `--json`, `--events`, `--session`, `--profile` and `--model` were taken
        // out by the argument parser before this command saw the line.
        if word == "--" {
            after_separator = true;
            index += 1;
            continue;
        }
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (word.as_str(), None),
        };
        match name {
            "--recipe" => recipe = Some(flag_value(words, &mut index, name, inline)),
            "--target" => target = Some(flag_value(words, &mut index, name, inline)),
            "--cwd" => cwd = Some(flag_value(words, &mut index, name, inline)),
            "--timeout-ms" => {
                let value = flag_value(words, &mut index, name, inline);
                timeout_ms = Some(parse_u64(&value, name));
            }
            "--expect-exit" => {
                let value = flag_value(words, &mut index, name, inline);
                expect_exit = Some(parse_i32(&value, name));
            }
            "--expect-stdout" => {
                expect_stdout = Some(flag_value(words, &mut index, name, inline));
            }
            "--expect-tests" => {
                let value = flag_value(words, &mut index, name, inline);
                expect_tests = Some(parse_u64(&value, name));
            }
            "--apparatus" => apparatus.include.push(flag_value(words, &mut index, name, inline)),
            "--exclude-apparatus" => {
                apparatus.exclude.push(flag_value(words, &mut index, name, inline));
            }
            other => usage_failure(&format!(
                "unknown flag '{other}'; the program and its arguments go after `--`"
            )),
        }
        index += 1;
    }

    let source = match (recipe, program) {
        (Some(_), Some(_)) => usage_failure(
            "a recipe already names the program; drop the program after `--`, or drop `--recipe`",
        ),
        (Some(name), None) => Source::Recipe { name, target },
        (None, Some(program)) => {
            if target.is_some() {
                usage_failure(
                    "--target belongs to a recipe; write `--recipe <name> --target <artifact>`",
                );
            }
            if !after_separator {
                usage_failure(
                    "the program must come after `--`; otherwise its own flags read as mine",
                );
            }
            Source::Program { program, args: program_args }
        }
        (None, None) => {
            if !after_separator {
                usage_failure(
                    "the program must come after `--`; otherwise its own flags read as mine",
                );
            }
            usage_failure(
                "nothing to check; write `agent verify [flags] -- <program> [args]` or \
                 `agent verify --recipe <name> [--target <artifact>]`",
            )
        }
    };

    Parsed { source, cwd, timeout_ms, expect_exit, expect_stdout, expect_tests, apparatus }
}

/// Turn the parsed line into the check it names.
///
/// Overrides are applied on top of a recipe rather than replacing it: a flag
/// the caller did not write keeps the recipe's answer, which is what makes
/// `agent verify --recipe web --target x --timeout-ms 5000` mean "the web
/// check, with a shorter deadline" rather than "a check with every other
/// expectation defaulted away".
fn expand(parsed: Parsed, recipes: Option<&BTreeMap<String, RecipeSpec>>) -> CheckSpec {
    let mut spec = match parsed.source {
        Source::Program { program, args } => CheckSpec {
            program,
            args,
            cwd: None,
            timeout_ms: DEFAULT_CHECK_TIMEOUT_MS,
            apparatus: ApparatusSpec::default(),
            expect: Expectation::default(),
        },
        Source::Recipe { name, target } => {
            let Some(recipes) = recipes else {
                usage_failure("a recipe needs the user configuration this command did not read");
            };
            let Some(recipe) = recipes.get(&name) else {
                usage_failure(&unknown_recipe(&name, recipes));
            };
            recipe
                .to_check_spec(&name, target.as_deref())
                .unwrap_or_else(|error| usage_failure(&error))
        }
    };

    if let Some(cwd) = parsed.cwd {
        spec.cwd = Some(cwd);
    }
    if let Some(timeout_ms) = parsed.timeout_ms {
        if timeout_ms == 0 {
            usage_failure("--timeout-ms must be a positive number of milliseconds");
        }
        spec.timeout_ms = timeout_ms;
    }
    if let Some(expect_exit) = parsed.expect_exit {
        spec.expect.exit_code = expect_exit;
    }
    if let Some(expect_stdout) = parsed.expect_stdout {
        spec.expect.stdout_contains = Some(expect_stdout);
    }
    if let Some(expect_tests) = parsed.expect_tests {
        spec.expect.tests_passed = Some(expect_tests);
    }
    if !parsed.apparatus.include.is_empty() {
        spec.apparatus.include = parsed.apparatus.include;
    }
    if !parsed.apparatus.exclude.is_empty() {
        spec.apparatus.exclude = parsed.apparatus.exclude;
    }
    spec
}

/// Say which recipes do exist, in the order a person can read.
fn unknown_recipe(name: &str, recipes: &BTreeMap<String, RecipeSpec>) -> String {
    if recipes.is_empty() {
        format!(
            "no recipe '{name}'; the user configuration defines none — add \
             [verify.recipes.{name}] to the file AGENT_USER_CONFIG names"
        )
    } else {
        let known: Vec<&str> = recipes.keys().map(String::as_str).collect();
        format!("no recipe '{name}'; the user configuration defines: {}", known.join(", "))
    }
}

/// The value of `--name value` or `--name=value`, in the words this command was
/// given. `index` moves past a value that came as the next word.
fn flag_value(words: &[String], index: &mut usize, name: &str, inline: Option<String>) -> String {
    match inline {
        Some(value) if !value.is_empty() => value,
        Some(_) => usage_failure(&format!("{name} needs a value")),
        None => match words.get(*index + 1) {
            Some(next) if !next.starts_with("--") => {
                *index += 1;
                next.clone()
            }
            _ => usage_failure(&format!("{name} needs a value")),
        },
    }
}

fn parse_u64(value: &str, flag: &str) -> u64 {
    value
        .parse()
        .unwrap_or_else(|_| usage_failure(&format!("{flag} needs a whole number, not '{value}'")))
}

fn parse_i32(value: &str, flag: &str) -> i32 {
    value
        .parse()
        .unwrap_or_else(|_| usage_failure(&format!("{flag} needs a whole number, not '{value}'")))
}

fn usage_failure(message: &str) -> ! {
    eprintln!("verify: {message}");
    std::process::exit(EXIT_DID_NOT_RUN);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_cli::args;

    fn parsed(argv: &[&str]) -> CheckSpec {
        let argv: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
        let parsed =
            parse_words(args::parse(&argv).expect("a readable command line").command_args());
        expand(parsed, None)
    }

    fn with_recipes(argv: &[&str], recipes: &BTreeMap<String, RecipeSpec>) -> CheckSpec {
        let argv: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
        let parsed =
            parse_words(args::parse(&argv).expect("a readable command line").command_args());
        expand(parsed, Some(recipes))
    }

    fn recipes(text: &str) -> BTreeMap<String, RecipeSpec> {
        agent_config::UserConfig::from_toml_str(text)
            .expect("a user configuration")
            .verify
            .expect("a verify section")
            .recipes
    }

    #[test]
    fn the_program_and_its_own_flags_survive_the_separator() {
        let spec = parsed(&["verify", "--", "cargo", "test", "--workspace"]);
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, ["test", "--workspace"]);
        assert_eq!(spec.timeout_ms, DEFAULT_CHECK_TIMEOUT_MS);
        assert_eq!(spec.expect, Expectation::default());
    }

    #[test]
    fn shared_flags_after_the_separator_are_program_arguments() {
        let spec = parsed(&[
            "verify",
            "--",
            "node",
            "check.mjs",
            "--json",
            "--task",
            "video",
            "--session",
            "other",
        ]);
        assert_eq!(spec.program, "node");
        assert_eq!(spec.args, ["check.mjs", "--json", "--task", "video", "--session", "other"]);
    }

    #[test]
    fn flags_before_the_separator_are_the_commands_own() {
        let spec = parsed(&[
            "verify",
            "--expect-exit",
            "3",
            "--expect-tests=7",
            "--expect-stdout",
            "7 passed",
            "--timeout-ms",
            "5000",
            "--cwd",
            "sub",
            "--",
            "cargo",
            "test",
        ]);
        assert_eq!(spec.expect.exit_code, 3);
        assert_eq!(spec.expect.tests_passed, Some(7));
        assert_eq!(spec.expect.stdout_contains.as_deref(), Some("7 passed"));
        assert_eq!(spec.timeout_ms, 5_000);
        assert_eq!(spec.cwd.as_deref(), Some("sub"));
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, ["test"]);
    }

    #[test]
    fn a_recipe_names_the_program_and_takes_the_target() {
        let table = recipes(
            r#"
[verify.recipes.web]
program = "node"
args = ["scripts/verify/web.mjs", "--target", "{target}"]
expect_stdout = "CHECK_OK"
"#,
        );
        let spec =
            with_recipes(&["verify", "--recipe", "web", "--target", "out/index.html"], &table);
        assert_eq!(spec.program, "node");
        assert_eq!(spec.args, ["scripts/verify/web.mjs", "--target", "out/index.html"]);
        assert_eq!(spec.expect.stdout_contains.as_deref(), Some("CHECK_OK"));
    }

    #[test]
    fn a_command_line_flag_overrides_what_the_recipe_said() {
        let table = recipes(
            "[verify.recipes.web]\nprogram = \"node\"\ntimeout_ms = 1000\nexpect_exit = 0\n",
        );
        let spec = with_recipes(
            &["verify", "--recipe", "web", "--timeout-ms", "9000", "--expect-exit", "2"],
            &table,
        );
        assert_eq!(spec.timeout_ms, 9_000, "the flag wins over the recipe");
        assert_eq!(spec.expect.exit_code, 2);
        assert_eq!(spec.program, "node", "and what the flag did not name is untouched");
    }

    #[test]
    fn an_unknown_recipe_names_the_ones_that_exist() {
        let table = recipes(
            "[verify.recipes.web]\nprogram = \"node\"\n[verify.recipes.video]\nprogram = \"node\"\n",
        );
        let message = unknown_recipe("webm", &table);
        assert!(message.contains("webm"), "{message}");
        assert!(message.contains("video, web"), "the names come sorted: {message}");

        // And a machine with no recipes at all says where one would go.
        let empty = unknown_recipe("web", &BTreeMap::new());
        assert!(empty.contains("AGENT_USER_CONFIG"), "{empty}");
        assert!(empty.contains("[verify.recipes.web]"), "{empty}");
    }
}
