//! A structured check: a program, its arguments, what is expected, and what
//! one run of it found.
//!
//! The completion criterion of [`crate::profile::CompletionSpec`] is a *shell
//! command*, and a shell command's exit status belongs to whatever ends its
//! pipeline — `node --test … | tail -5` exits with `tail`'s status, so a failing
//! suite reads as a pass (CONTRACT §7 #170). This module is the other shape: the
//! program is named and started directly, never through a shell, so the status
//! the check reports is the program's own.
//!
//! [`CheckRun`] is what a check observed, kept as facts — exit code, digests,
//! sizes, a parsed test count — rather than as a verdict, so the same run can be
//! judged again later without running anything. [`CheckRun::judge`] is the one
//! judgement, written as data: a caller cannot talk itself into a pass by having
//! read only the exit code.
//!
//! This module states what a check is. Running one is a port
//! (`agent_ports::verifier`) and recording the result is the kernel's
//! (`agent_kernel::verifier`); nothing here starts a process or reads a clock
//! that matters.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::evidence::EvidenceStatus;

/// The deadline a check gets when the caller names none.
///
/// Two minutes: long enough for a real test suite on a cold build, short enough
/// that a hung program does not look like a hung agent. A caller that knows its
/// suite is slower must say so — a silent clamp would make a long check fail in
/// a way that reads as a broken task.
pub const DEFAULT_CHECK_TIMEOUT_MS: u64 = 120_000;

fn default_timeout() -> u64 {
    DEFAULT_CHECK_TIMEOUT_MS
}

fn is_default_timeout(timeout_ms: &u64) -> bool {
    *timeout_ms == DEFAULT_CHECK_TIMEOUT_MS
}

/// One check, as the operator stated it.
///
/// `program` and `args` are kept apart rather than as one command line: keeping
/// them apart is what makes "no shell" a fact about the type instead of a
/// promise about the implementation. There is no field a shell could be tucked
/// into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    /// The executable, named as the environment resolves it.
    pub program: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Working directory, relative to the workspace root. `None` means the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default = "default_timeout", skip_serializing_if = "is_default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub expect: Expectation,
}

impl CheckSpec {
    /// The check as one line, for a person reading the log.
    ///
    /// Display only — nothing here is ever handed to a shell. It exists because
    /// an evidence record that named only the program would not say which check
    /// ran, and one that named only a command line would look like it could be
    /// run again that way.
    pub fn command_line(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            // Spaces inside an argument are quoted so that two different
            // argument lists cannot print identically.
            if arg.contains(' ') || arg.is_empty() {
                line.push('"');
                line.push_str(&arg.replace('"', "\\\""));
                line.push('"');
            } else {
                line.push_str(arg);
            }
        }
        line
    }
}

/// What the operator expects a passing check to look like.
///
/// Only shapes that can be read off facts are offered. "The tests make sense" is
/// not one of them: a checker that guesses is a checker whose pass does not mean
/// anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Expectation {
    /// The program's own exit status. Zero unless the operator says otherwise.
    #[serde(default)]
    pub exit_code: i32,
    /// A substring the program must have printed on stdout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_contains: Option<String>,
    /// The number of tests the program's summary must report. A program whose
    /// output carries no readable count cannot satisfy this — see
    /// [`CheckRun::judge`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_passed: Option<u64>,
}

/// What one run of a check observed.
///
/// The output itself is not kept: a test suite's stdout is not a fact worth
/// carrying in every log read. The digests and sizes are, because they let two
/// runs of the same check be told apart and let a reader who has the artifact
/// confirm which text this was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// The program's own exit status. `None` when it never started or ended
    /// without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    pub stdout_sha256: String,
    pub stdout_bytes: u64,
    pub stderr_sha256: String,
    pub stderr_bytes: u64,
    /// Whether the expected substring was found. `None` when the operator asked
    /// for no substring: "did not look" and "looked and missed" are different
    /// facts and only one of them is a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains_expected: Option<bool>,
    /// The test count parsed out of the program's summary, when one could be
    /// read. The shapes understood are listed on the adapter that fills it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_passed: Option<u64>,
    /// Digest of what the check ran in: the program, its arguments, the working
    /// directory, and the `PATH` that decides which binary the name resolves to.
    pub environment_hash: String,
    /// Why the program could not be run at all. Set instead of an exit code, and
    /// never read as a failure of the task: a suite that never started says
    /// nothing about whether the work is right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// What a run's facts add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckJudgement {
    pub status: EvidenceStatus,
    /// The one sentence a person reads. Empty when the check passed.
    pub reason: String,
}

impl CheckRun {
    /// Judge this run against what the operator expected.
    ///
    /// The order is the order of how much a fact can be trusted:
    ///
    /// 1. a program that could not run, or that ran out of time, is `error` —
    ///    nothing about the task follows from it;
    /// 2. an exit status other than the expected one is `failed`;
    /// 3. a missing expected substring is `failed`;
    /// 4. an expectation about a test count the output did not carry is
    ///    `error` — the check ran but cannot be read, and calling that a pass
    ///    is exactly the silence this check exists to remove;
    /// 5. a count that is readable and different is `failed`;
    /// 6. otherwise `passed`.
    ///
    /// The ordering matters at one place in particular: a program that printed
    /// the right text but exited nonzero is `failed`, not `passed`, because the
    /// exit status is the program's verdict on itself.
    pub fn judge(&self, expect: &Expectation) -> CheckJudgement {
        if let Some(failure) = &self.failure {
            return CheckJudgement {
                status: EvidenceStatus::Error,
                reason: format!("the check could not run: {failure}"),
            };
        }
        if self.timed_out {
            return CheckJudgement {
                status: EvidenceStatus::Error,
                reason: "the check did not finish before its deadline".to_string(),
            };
        }
        let Some(code) = self.exit_code else {
            return CheckJudgement {
                status: EvidenceStatus::Error,
                reason: "the check ended without an exit status".to_string(),
            };
        };
        if code != expect.exit_code {
            return CheckJudgement {
                status: EvidenceStatus::Failed,
                reason: format!("the check exited {code}, not {}", expect.exit_code),
            };
        }
        if let Some(needle) = &expect.stdout_contains {
            match self.contains_expected {
                Some(true) => {}
                Some(false) => {
                    return CheckJudgement {
                        status: EvidenceStatus::Failed,
                        reason: format!("the check did not print {needle:?}"),
                    };
                }
                None => {
                    return CheckJudgement {
                        status: EvidenceStatus::Error,
                        reason: "the check's output was not searched for the expected text"
                            .to_string(),
                    };
                }
            }
        }
        if let Some(want) = expect.tests_passed {
            match self.tests_passed {
                None => {
                    return CheckJudgement {
                        status: EvidenceStatus::Error,
                        reason: format!(
                            "the check printed no test count this build can read; \
                             {want} were expected"
                        ),
                    };
                }
                Some(found) if found != want => {
                    return CheckJudgement {
                        status: EvidenceStatus::Failed,
                        reason: format!("the check reported {found} passing tests, not {want}"),
                    };
                }
                Some(_) => {}
            }
        }
        CheckJudgement { status: EvidenceStatus::Passed, reason: String::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> CheckRun {
        CheckRun {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            exit_code: Some(0),
            timed_out: false,
            stdout_sha256: "aa".to_string(),
            stdout_bytes: 3,
            stderr_sha256: "bb".to_string(),
            stderr_bytes: 0,
            contains_expected: None,
            tests_passed: None,
            environment_hash: "cc".to_string(),
            failure: None,
        }
    }

    #[test]
    fn a_clean_exit_is_a_pass() {
        assert_eq!(run().judge(&Expectation::default()).status, EvidenceStatus::Passed);
    }

    #[test]
    fn the_exit_status_is_the_programs_own_verdict() {
        let mut run = run();
        run.exit_code = Some(1);
        let verdict = run.judge(&Expectation::default());
        assert_eq!(verdict.status, EvidenceStatus::Failed);
        assert!(verdict.reason.contains("exited 1"), "{}", verdict.reason);

        // A non-zero status can be what the operator expects.
        assert_eq!(
            run.judge(&Expectation { exit_code: 1, ..Expectation::default() }).status,
            EvidenceStatus::Passed
        );
    }

    #[test]
    fn a_check_that_could_not_run_is_an_error_not_a_failure() {
        let mut run = run();
        run.exit_code = None;
        run.failure = Some("program not found".to_string());
        let verdict = run.judge(&Expectation::default());
        assert_eq!(verdict.status, EvidenceStatus::Error);
        assert!(verdict.reason.contains("program not found"), "{}", verdict.reason);

        let mut slow = self::run();
        slow.timed_out = true;
        assert_eq!(slow.judge(&Expectation::default()).status, EvidenceStatus::Error);
    }

    #[test]
    fn an_expected_test_count_that_cannot_be_read_is_an_error() {
        // The check ran and exited zero, but nothing in its output says how
        // many tests passed. Believing the exit code here is the exact silence
        // this check exists to remove.
        let expect = Expectation { tests_passed: Some(3), ..Expectation::default() };
        let verdict = run().judge(&expect);
        assert_eq!(verdict.status, EvidenceStatus::Error);
        assert!(verdict.reason.contains('3'), "{}", verdict.reason);
    }

    #[test]
    fn a_readable_test_count_is_compared() {
        let expect = Expectation { tests_passed: Some(3), ..Expectation::default() };
        let mut matching = run();
        matching.tests_passed = Some(3);
        assert_eq!(matching.judge(&expect).status, EvidenceStatus::Passed);

        let mut short = run();
        short.tests_passed = Some(2);
        assert_eq!(short.judge(&expect).status, EvidenceStatus::Failed);
    }

    #[test]
    fn an_expected_substring_is_compared_only_when_it_was_asked_for() {
        let expect =
            Expectation { stdout_contains: Some("3 passed".to_string()), ..Expectation::default() };
        let mut missed = run();
        missed.contains_expected = Some(false);
        assert_eq!(missed.judge(&expect).status, EvidenceStatus::Failed);

        let mut found = run();
        found.contains_expected = Some(true);
        assert_eq!(found.judge(&expect).status, EvidenceStatus::Passed);

        // No expectation means no search — and no failure for not searching.
        let mut unsearched = run();
        unsearched.contains_expected = None;
        assert_eq!(unsearched.judge(&Expectation::default()).status, EvidenceStatus::Passed);
    }

    #[test]
    fn a_spec_round_trips_and_keeps_its_defaults() {
        let spec: CheckSpec =
            serde_json::from_str(r#"{"program":"cargo","args":["test"]}"#).unwrap();
        assert_eq!(spec.timeout_ms, DEFAULT_CHECK_TIMEOUT_MS);
        assert_eq!(spec.expect, Expectation::default());
        assert_eq!(spec.cwd, None);

        let text = serde_json::to_string(&spec).unwrap();
        assert!(!text.contains("timeout_ms"), "a default is not written down: {text}");
        assert_eq!(serde_json::from_str::<CheckSpec>(&text).unwrap(), spec);
    }

    #[test]
    fn the_command_line_quotes_what_would_otherwise_be_ambiguous() {
        let spec = CheckSpec {
            program: "cargo".to_string(),
            args: vec!["test".to_string(), "--".to_string(), "a b".to_string()],
            cwd: None,
            timeout_ms: 1,
            expect: Expectation::default(),
        };
        assert_eq!(spec.command_line(), r#"cargo test -- "a b""#);
    }
}
