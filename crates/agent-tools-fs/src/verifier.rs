//! Running a structured check through the workspace's execution environment.
//!
//! A check is `program + args`, and [`ExecutionEnvironment::run_command`] starts
//! exactly that — no shell, no command line to re-parse. That is the whole point
//! of the verifier: `node --test | tail -5` reports `tail`'s status, and a check
//! that reports the wrong program's status is worse than no check, because it is
//! believed (CONTRACT §7 #170).
//!
//! What this adapter adds on top of `run_command` is measurement: digests of
//! stdout and stderr, a parsed test count, and the hash of what the check ran in.
//! The output itself is not returned — a suite's stdout is not something to
//! carry into every log read, and its digest answers the question a reader
//! actually has ("was this the same output as last time?").
//!
//! Every way a check can end is a [`CheckRun`]. A program that cannot start, a
//! working directory outside the workspace, a read-only backend: each becomes a
//! run with `failure` set, which the judge reads as `error` — never as a pass
//! and never as a failed task.

use std::sync::Arc;
use std::time::Instant;

use agent_ports::execution_environment::{CommandSpec, DirEntryKind, ExecutionEnvironment};
use agent_ports::verifier::CommandVerifier;
use agent_protocol::apparatus::{ApparatusEntry, ApparatusManifest, ApparatusSpec};
use agent_protocol::digest;
use agent_protocol::verification::{CheckRun, CheckSpec};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::env::TIMEOUT_EXIT_CODE;

/// A [`CommandVerifier`] over one execution environment.
pub struct EnvCommandVerifier {
    env: Arc<dyn ExecutionEnvironment>,
}

impl EnvCommandVerifier {
    pub fn new(env: Arc<dyn ExecutionEnvironment>) -> Self {
        Self { env }
    }
}

impl CommandVerifier for EnvCommandVerifier {
    fn verify(&self, spec: &CheckSpec, started_at: DateTime<Utc>) -> CheckRun {
        // A monotonic clock, used only to measure how long the run took: the
        // instant it started is the caller's, so nothing here reads a wall
        // clock (STYLE §1).
        let began = Instant::now();
        let command = CommandSpec {
            program: spec.program.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            timeout_ms: Some(spec.timeout_ms),
        };
        let outcome = self.env.run_command(&command);
        let elapsed = chrono::Duration::from_std(began.elapsed()).unwrap_or_default();
        let finished_at = started_at + elapsed;
        let environment_hash = environment_hash(spec);

        match outcome {
            Ok(output) => {
                let contains_expected = spec
                    .expect
                    .stdout_contains
                    .as_deref()
                    .map(|needle| output.stdout.contains(needle));
                CheckRun {
                    started_at,
                    finished_at,
                    // The environment reports a deadline kill as `timeout(1)`
                    // does — exit 124. A program that exits 124 on its own is
                    // indistinguishable from that here, and is read as a
                    // timeout: the conservative direction is "the check did not
                    // finish", which is an error rather than a pass.
                    exit_code: Some(output.exit_code),
                    timed_out: output.exit_code == TIMEOUT_EXIT_CODE,
                    stdout_sha256: sha256_hex(output.stdout.as_bytes()),
                    stdout_bytes: output.stdout.len() as u64,
                    stderr_sha256: sha256_hex(output.stderr.as_bytes()),
                    stderr_bytes: output.stderr.len() as u64,
                    contains_expected,
                    tests_passed: tests_passed(&output.stdout),
                    environment_hash,
                    failure: None,
                }
            }
            Err(error) => CheckRun {
                started_at,
                finished_at,
                exit_code: None,
                timed_out: false,
                stdout_sha256: sha256_hex(b""),
                stdout_bytes: 0,
                stderr_sha256: sha256_hex(b""),
                stderr_bytes: 0,
                contains_expected: None,
                tests_passed: None,
                environment_hash,
                failure: Some(error.to_string()),
            },
        }
    }

    fn manifest(&self, spec: &ApparatusSpec) -> Result<ApparatusManifest, String> {
        let mut entries = Vec::new();
        walk(&*self.env, "", spec, &mut entries)?;
        Ok(ApparatusManifest::build(spec.clone(), entries))
    }
}

/// Collect every apparatus file below `dir`, through the environment.
///
/// Reads and listings go through the `ExecutionEnvironment`, so the walk is
/// confined to the workspace by the same boundary a tool call is. A directory no
/// include pattern could match below is never listed (`may_contain`), which is
/// what keeps `tests/**` from walking a build tree.
///
/// Links are reported by the listing and never followed: a link is the one entry
/// whose target the listing does not vouch for, and following one would let a
/// manifest be built out of files the workspace boundary never approved.
fn walk(
    env: &dyn ExecutionEnvironment,
    dir: &str,
    spec: &ApparatusSpec,
    out: &mut Vec<ApparatusEntry>,
) -> Result<(), String> {
    let listing = env
        .list_dir(dir)
        .map_err(|e| format!("the apparatus directory {} could not be read: {e}", display(dir)))?;
    for entry in listing {
        let path =
            if dir.is_empty() { entry.name.clone() } else { format!("{dir}/{}", entry.name) };
        match entry.kind {
            DirEntryKind::File => {
                if spec.matches(&path) {
                    let content = env
                        .read_file(&path)
                        .map_err(|e| format!("the apparatus file {path} could not be read: {e}"))?;
                    out.push(ApparatusEntry::of(path, content.as_bytes()));
                }
            }
            DirEntryKind::Directory if spec.may_contain(&path) => {
                walk(env, &path, spec, out)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn display(dir: &str) -> &str {
    if dir.is_empty() { "." } else { dir }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The digest of what the check ran in.
///
/// The program and arguments, the working directory, and `PATH` — the variable
/// that decides which binary a bare program name resolves to. Two checks of the
/// same command against different tool chains therefore do not share a hash,
/// which is the question this answers: "did this run under the same conditions
/// as the one I am comparing it with?"
fn environment_hash(spec: &CheckSpec) -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    let material = format!(
        "program={}\nargs={:?}\ncwd={}\nPATH={}",
        spec.program,
        spec.args,
        spec.cwd.as_deref().unwrap_or(""),
        path
    );
    digest::fnv1a_hex(material.as_bytes())
}

/// The passing-test count in a program's summary, when it printed one.
///
/// Two shapes are understood, both of which state a count in plain words:
/// TAP's `# pass 12` (what `node --test` prints) and the `12 passed` pair
/// printed by cargo's test harness, pytest and jest. Anything else — a custom
/// runner, a summary in another language — is reported as "no count", and an
/// expectation that wanted a count is then an *error* rather than a silent
/// pass: a count this build cannot read is not a count of zero.
fn tests_passed(stdout: &str) -> Option<u64> {
    let mut found = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# pass ") {
            found = rest.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()).or(found);
            continue;
        }
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        for pair in words.windows(2) {
            if pair[1].trim_end_matches([';', ',', '.']) == "passed"
                && let Ok(count) = pair[0].parse::<u64>()
            {
                found = Some(count);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::LocalExecutionEnvironment;
    use agent_protocol::apparatus::ApparatusSpec;
    use agent_protocol::verification::Expectation;
    use tempfile::tempdir;

    fn spec(program: &str, args: &[&str]) -> CheckSpec {
        CheckSpec {
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
            timeout_ms: 30_000,
            apparatus: ApparatusSpec::default(),
            expect: Expectation::default(),
        }
    }

    fn verifier(root: &std::path::Path) -> EnvCommandVerifier {
        EnvCommandVerifier::new(Arc::new(LocalExecutionEnvironment::new(root.to_path_buf())))
    }

    #[cfg(windows)]
    fn exiting(code: i32) -> CheckSpec {
        spec("cmd", &["/C", &format!("echo hello & exit {code}")])
    }

    #[cfg(not(windows))]
    fn exiting(code: i32) -> CheckSpec {
        spec("sh", &["-c", &format!("echo hello; exit {code}")])
    }

    #[test]
    fn a_real_program_is_measured_not_summarised() {
        let dir = tempdir().unwrap();
        let run = verifier(dir.path()).verify(&exiting(0), Utc::now());

        assert_eq!(run.exit_code, Some(0));
        assert!(!run.timed_out);
        assert!(run.failure.is_none(), "{:?}", run.failure);
        assert!(run.stdout_bytes > 0);
        assert_eq!(run.stdout_sha256.len(), 64, "a sha256 digest is 64 hex characters");
        assert_ne!(run.stdout_sha256, sha256_hex(b""), "the output was hashed, not skipped");
        assert_eq!(run.environment_hash.len(), 16);
    }

    #[test]
    fn the_run_is_stamped_from_the_instant_it_was_handed() {
        let dir = tempdir().unwrap();
        // An hour ago: a reading of the wall clock inside the adapter would
        // land nowhere near this, so the assertion tells the two apart.
        let asked = Utc::now() - chrono::Duration::hours(1);
        let run = verifier(dir.path()).verify(&exiting(0), asked);

        assert_eq!(run.started_at, asked);
        assert!(run.finished_at >= run.started_at);
        assert!(
            run.finished_at - asked < chrono::Duration::minutes(1),
            "the end is the handed start plus how long the run took, not a fresh clock read"
        );
    }

    #[test]
    fn a_program_that_is_not_there_is_a_run_that_could_not_start() {
        let dir = tempdir().unwrap();
        let run = verifier(dir.path())
            .verify(&spec("definitely-not-a-real-program-xyz", &[]), Utc::now());

        assert_eq!(run.exit_code, None);
        let failure = run.failure.expect("a check that could not start says why");
        assert!(!failure.is_empty());
    }

    #[test]
    fn a_nonzero_exit_is_a_failed_check() {
        let dir = tempdir().unwrap();
        let run = verifier(dir.path()).verify(&exiting(3), Utc::now());

        assert_eq!(run.exit_code, Some(3));
    }

    #[test]
    fn the_expected_text_is_looked_for_in_stdout() {
        let dir = tempdir().unwrap();
        let mut spec = exiting(0);
        spec.expect.stdout_contains = Some("hello".to_string());
        assert_eq!(verifier(dir.path()).verify(&spec, Utc::now()).contains_expected, Some(true));

        spec.expect.stdout_contains = Some("goodbye".to_string());
        assert_eq!(verifier(dir.path()).verify(&spec, Utc::now()).contains_expected, Some(false));

        // No expectation, no search.
        assert_eq!(verifier(dir.path()).verify(&exiting(0), Utc::now()).contains_expected, None);
    }

    fn apparatus(include: &[&str]) -> ApparatusSpec {
        ApparatusSpec {
            include: include.iter().map(|glob| glob.to_string()).collect(),
            exclude: Vec::new(),
        }
    }

    fn write(root: &std::path::Path, path: &str, content: &str) {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    #[test]
    fn the_manifest_covers_exactly_the_files_the_globs_name() {
        let dir = tempdir().unwrap();
        write(dir.path(), "tests/a.rs", "assert!(true);");
        write(dir.path(), "tests/nested/b.rs", "assert!(false);");
        write(dir.path(), "src/lib.rs", "pub fn f() {}");

        let manifest = verifier(dir.path()).manifest(&apparatus(&["tests/**"])).unwrap();
        let paths: Vec<&str> = manifest.entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, ["tests/a.rs", "tests/nested/b.rs"]);
    }

    #[test]
    fn editing_an_apparatus_file_moves_the_digest_and_editing_another_does_not() {
        let dir = tempdir().unwrap();
        write(dir.path(), "tests/a.rs", "assert!(true);");
        write(dir.path(), "src/lib.rs", "pub fn f() {}");
        let spec = apparatus(&["tests/**"]);

        let before = verifier(dir.path()).manifest(&spec).unwrap();
        write(dir.path(), "src/lib.rs", "pub fn f() { /* the work */ }");
        let after_work = verifier(dir.path()).manifest(&spec).unwrap();
        assert_eq!(before.hash, after_work.hash, "work is not apparatus");

        write(dir.path(), "tests/a.rs", "assert!(true); // weakened");
        let after_edit = verifier(dir.path()).manifest(&spec).unwrap();
        assert_ne!(before.hash, after_edit.hash, "an edited test is a different apparatus");
    }

    #[test]
    fn a_file_the_environment_will_not_read_is_an_error_not_a_gap() {
        // A glob that names something binary: `read_file` refuses it, and a
        // manifest that quietly skipped it would be a digest of an apparatus
        // nobody can enumerate.
        let dir = tempdir().unwrap();
        write(dir.path(), "tests/a.rs", "assert!(true);");
        std::fs::write(dir.path().join("tests/binary.bin"), [0xff, 0xfe, 0x00]).unwrap();

        let error = verifier(dir.path()).manifest(&apparatus(&["tests/**"])).unwrap_err();
        assert!(error.contains("tests/binary.bin"), "{error}");
    }

    #[test]
    fn a_count_is_read_from_the_shapes_a_summary_actually_prints() {
        assert_eq!(tests_passed("# pass 12\n# fail 0\n"), Some(12));
        assert_eq!(tests_passed("test result: ok. 3 passed; 0 failed; 0 ignored"), Some(3));
        assert_eq!(tests_passed("Tests:       3 passed, 3 total"), Some(3));
        assert_eq!(tests_passed("no summary here"), None);
        assert_eq!(tests_passed("0 failed; 0 ignored"), None, "only passes are counted");
    }

    #[test]
    fn two_checks_differ_only_when_what_they_run_in_differs() {
        let a = spec("cargo", &["test"]);
        let b = spec("cargo", &["test"]);
        let c = spec("cargo", &["test", "--workspace"]);
        assert_eq!(environment_hash(&a), environment_hash(&b));
        assert_ne!(environment_hash(&a), environment_hash(&c));
    }
}
