//! Guard: every double the testkit exposes is named by a test.
//!
//! `STYLE.md` §6 requires a double to behave the way the real implementation
//! does and to have its differences written down. The half of that a text guard
//! can check is the weaker half, and it is the half that drifts: a new double
//! arrives with no test at all, and nothing notices, because the thing that is
//! missing is the test. So — every public type in `agent-testkit/src/lib.rs`
//! that a trait is implemented for has to be named somewhere under `tests/`.
//!
//! Blind spots, each pinned by a probe below:
//!
//! * It checks that a test *names* the double, not that the test asserts
//!   anything true about it. A mention in a comment counts.
//! * It knows only the doubles declared in `agent-testkit`'s own `lib.rs`. The
//!   `UnavailableArtifacts` doubles hand-rolled inside `agent-kernel` and
//!   `agent-tools-mcp` are out of its reach, as is any double in another crate.
//! * A name seen inside a longer word does not count (`FakeWidgetFactory` does
//!   not name `FakeWidget`), which is the one direction it does not over-count.
//!
//! This file is excluded from the search: it names every double in its own
//! expectations, so counting its text would let the check pass on itself.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn testkit_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The names of the public types declared at the top level of `source`.
fn public_types(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| {
            let rest =
                line.strip_prefix("pub struct ").or_else(|| line.strip_prefix("pub enum "))?;
            Some(identifier(rest))
        })
        .collect()
}

/// The names a trait is implemented for in `source`.
///
/// An inherent `impl Foo { … }` has no ` for ` and is not collected: a type
/// with methods but no trait behind it is data, not a double.
fn implemented_for(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("impl ")?;
            let (_, target) = rest.split_once(" for ")?;
            let name = identifier(target);
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// The leading identifier of `text`.
fn identifier(text: &str) -> String {
    text.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
}

/// The `(file name, text)` of every test file under `tests/`, minus `exclude`.
fn test_sources(exclude: &str) -> Vec<(String, String)> {
    let dir = testkit_dir().join("tests");
    let mut found: Vec<(String, String)> = fs::read_dir(&dir)
        .expect("the tests directory")
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("rs"))
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().to_string();
            (name != exclude).then(|| (name, fs::read_to_string(&path).expect("a test file")))
        })
        .collect();
    found.sort();
    found
}

/// Whether `text` names `name` — as a word, not as part of a longer identifier.
fn mentions(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The doubles in `source` that no file in `tests` names.
fn unexercised(source: &str, tests: &[(String, String)]) -> BTreeSet<String> {
    public_types(source)
        .intersection(&implemented_for(source))
        .filter(|name| !tests.iter().any(|(_, text)| mentions(text, name.as_str())))
        .cloned()
        .collect()
}

#[test]
fn every_double_in_the_testkit_is_named_by_a_test() {
    let source =
        fs::read_to_string(testkit_dir().join("src").join("lib.rs")).expect("the testkit source");
    let tests = test_sources("fake_coverage.rs");

    // A parser that stopped seeing the surface would pass by finding nothing.
    assert!(
        public_types(&source).len() >= 10,
        "the parser found almost no public types — it, not the doubles, is the problem"
    );

    let missing = unexercised(&source, &tests);

    assert!(
        missing.is_empty(),
        "no test names these doubles: {missing:?} — a double with no test of its own is a stand-in \
         for a real implementation that nothing ever compares the two of"
    );
}

// ---------------------------------------------------------------------------
// Probes. The doubles in this repository all have tests, so the check's
// failures are the part nobody exercises: feed it sources where it should
// complain, and assert it does.
// ---------------------------------------------------------------------------

const SAMPLE: &str = r#"
/// A public double.
pub struct FakeWidget {
    calls: Vec<String>,
}

impl ToolProvider for FakeWidget {
    async fn invoke(&self) {}
}

/// Not public: not part of the surface a test can reach.
struct FakeLease;

impl Drop for FakeLease {}
"#;

#[test]
fn a_new_double_with_no_test_is_reported() {
    let tests = vec![("fakes.rs".to_string(), "FakeEventStore::new()".to_string())];

    assert_eq!(unexercised(SAMPLE, &tests), BTreeSet::from(["FakeWidget".to_string()]));
}

#[test]
fn a_name_seen_only_inside_a_longer_word_does_not_count() {
    let tests = vec![("fakes.rs".to_string(), "FakeWidgetFactory::new()".to_string())];

    assert!(unexercised(SAMPLE, &tests).contains("FakeWidget"));
}

/// The blind spot it does have: a name mentioned anywhere in a test file counts,
/// so this checks that a double is known to a test, not that the test says
/// anything about it.
#[test]
fn a_mention_in_a_comment_counts_and_that_is_the_blind_spot() {
    let tests = vec![("fakes.rs".to_string(), "// TODO: cover FakeWidget".to_string())];

    assert!(unexercised(SAMPLE, &tests).is_empty());
}

/// The blind spot it does not have: privacy and "no trait" are what make a type
/// not a double, and both are checked directly rather than by a naming accident.
#[test]
fn a_private_type_and_a_type_with_no_trait_are_not_doubles() {
    assert!(public_types(SAMPLE).contains("FakeWidget"));
    assert!(!public_types(SAMPLE).contains("FakeLease"), "the private type is not in the surface");
    assert!(
        implemented_for(SAMPLE).contains("FakeLease"),
        "its impl *is* seen — privacy is what excludes it, so this probe stays honest"
    );

    let data = "pub struct DrivenTurn { pub events: Vec<u8> }\n";
    assert!(public_types(data).contains("DrivenTurn"));
    assert!(unexercised(data, &[]).is_empty(), "a type with no trait behind it is data");
}

/// If this file counted as a test, the expectation list right here would cover
/// every double in the crate and the guard would pass on its own text.
#[test]
fn the_guards_own_file_is_not_counted_as_a_test() {
    let guard = fs::read_to_string(testkit_dir().join("tests").join("fake_coverage.rs"))
        .expect("this file");
    assert!(mentions(&guard, "FakeWidget"), "this file names the sample double");

    let counting_itself = vec![("fake_coverage.rs".to_string(), guard)];
    assert!(
        unexercised(SAMPLE, &counting_itself).is_empty(),
        "counting this file would cover the sample — which is why it is excluded"
    );
    assert!(
        test_sources("fake_coverage.rs").iter().all(|(name, _)| name != "fake_coverage.rs"),
        "the live search has to be the one that excludes it"
    );
}
