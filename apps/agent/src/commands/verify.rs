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

use agent_cli::args::Args;
use agent_protocol::apparatus::ApparatusSpec;
use agent_protocol::verification::{CheckSpec, DEFAULT_CHECK_TIMEOUT_MS, Expectation};

use crate::support::{assemble, chain_start, watch};
use crate::{EXIT_DID_NOT_RUN, exit_on};

pub(crate) async fn verify(args: &Args) {
    let mut spec = parse(args);
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
fn parse(args: &Args) -> CheckSpec {
    let words = args.command_args();
    let mut cwd = None;
    let mut timeout_ms = DEFAULT_CHECK_TIMEOUT_MS;
    let mut expect_exit = 0;
    let mut expect_stdout = None;
    let mut expect_tests = None;
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
            "--cwd" => cwd = Some(flag_value(words, &mut index, name, inline)),
            "--timeout-ms" => {
                let value = flag_value(words, &mut index, name, inline);
                timeout_ms = parse_u64(&value, name, args);
            }
            "--expect-exit" => {
                let value = flag_value(words, &mut index, name, inline);
                expect_exit = parse_i32(&value, name, args);
            }
            "--expect-stdout" => {
                expect_stdout = Some(flag_value(words, &mut index, name, inline));
            }
            "--expect-tests" => {
                let value = flag_value(words, &mut index, name, inline);
                expect_tests = Some(parse_u64(&value, name, args));
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

    if !after_separator {
        usage_failure("the program must come after `--`; otherwise its own flags read as mine");
    }
    let Some(program) = program else {
        usage_failure("nothing to check; write `agent verify [flags] -- <program> [args]`");
    };
    if timeout_ms == 0 {
        usage_failure("--timeout-ms must be a positive number of milliseconds");
    }

    CheckSpec {
        program,
        args: program_args,
        cwd,
        timeout_ms,
        apparatus,
        expect: Expectation {
            exit_code: expect_exit,
            stdout_contains: expect_stdout,
            tests_passed: expect_tests,
        },
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

fn parse_u64(value: &str, flag: &str, args: &Args) -> u64 {
    value.parse().unwrap_or_else(|_| {
        crate::support::fail(args, &format!("{flag} needs a whole number, not '{value}'"))
    })
}

fn parse_i32(value: &str, flag: &str, args: &Args) -> i32 {
    value.parse().unwrap_or_else(|_| {
        crate::support::fail(args, &format!("{flag} needs a whole number, not '{value}'"))
    })
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
        parse(&args::parse(&argv).expect("a readable command line"))
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
}
