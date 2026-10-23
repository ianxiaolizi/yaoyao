//! Boundary test for CONTRACT.md §6 C6.1: the schema version has one home.
//!
//! `STYLE.md` §1 makes the rule — the event envelope's version is minted in one
//! place, `agent_protocol::ids::SchemaVersion::CURRENT` — and, like every rule
//! that is only a sentence, it is worth exactly as much as the thing that can
//! fail when it is broken. Before this guard the envelope version was written as
//! a bare `"0.1.0"` in eight draft builders across `agent-kernel` and
//! `agent-loop`, plus one local `const` in `fork.rs`: nine copies of one fact,
//! each independently editable. A bump that reached most of them would leave a
//! single log carrying two versions, and the moment a reader starts checking
//! (the work of YA-21/YA-20/YA-67) that becomes a refusal to read the log at
//! all.
//!
//! Two things are asserted:
//!
//!   1. **No production site names a version literal.** A
//!      `SchemaVersion::from_string("…")` outside a test module is a second
//!      answer to "which schema is this log?".
//!   2. **The one home still exists.** `ids.rs` carries
//!      `pub const CURRENT: &'static str`, so a bump that deletes the constant
//!      instead of moving it cannot pass by emptiness — the sites would fail to
//!      compile, but the guard names the *place* as well as the value.
//!
//! The guard is a **text** guard and says so: it reads source lines, not types.
//! A literal reached through a `concat!`, a `from_string` whose argument is
//! split across a newline, or a file under a directory this walk does not visit
//! would slip past it. The probes at the bottom pin those limits rather than
//! hide them. The export bundle's version (`agent-cli`, the bundle document's
//! own schema) and the MCP client's announced version are **different facts**
//! and are deliberately out of scope — they are not named `SchemaVersion`.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <workspace>/crates/agent-testkit.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `*.rs` under `<root>/{crates,apps}/*/src`, sorted, minus `src/bin`.
/// The rule is about the paths a build runs as the framework: the test-double
/// servers live under `examples/`, which this walk never visits, and `src/bin`
/// stays excluded for the day a package grows a second binary that *is* the
/// framework (same scope as `clock_and_id_boundaries.rs`).
fn production_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    for top in ["crates", "apps"] {
        let Ok(members) = fs::read_dir(root.join(top)) else {
            continue;
        };
        for member in members.flatten() {
            let src = member.path().join("src");
            if !src.is_dir() {
                continue;
            }
            let mut found = Vec::new();
            rust_sources(&src, &mut found);
            found.retain(|path| !path.strip_prefix(&src).is_ok_and(|rel| rel.starts_with("bin")));
            files.extend(found);
        }
    }
    files.sort();
    files
}

fn relative_to_root(path: &Path) -> String {
    path.strip_prefix(workspace_root()).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// The lines a normal build compiles: everything before the file's test module.
///
/// Same shape as `clock_and_id_boundaries.rs`: cut at the first standalone
/// `#[cfg(test)]` and **assert the next non-blank line opens `mod tests`**, so a
/// `#[cfg(test)]` that is not a tests module stops the guard instead of
/// mis-scoping it.
fn production_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().collect();
    if let Some(cut) = lines.iter().position(|line| line.trim() == "#[cfg(test)]") {
        let opens_tests = lines[cut + 1..]
            .iter()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| line.trim() == "mod tests {");
        assert!(
            opens_tests,
            "the first `#[cfg(test)]` does not open `mod tests`; `production_lines` \
             does not know how to scope this file"
        );
        lines.truncate(cut);
    }
    lines
}

/// A line that is entirely a comment. A doc or a comment may name the API
/// freely; a *trailing* comment on a line that also holds code is not skipped,
/// which is the strict direction.
fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// The one call whose literal argument is forbidden in production.
const CALL: &str = "SchemaVersion::from_string(";

/// Lines of `text` that hand `SchemaVersion::from_string` a bare string
/// literal. `rel` only names the file in the message.
fn version_literal_violations(rel: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (index, line) in production_lines(text).iter().enumerate() {
        if is_comment(line) {
            continue;
        }
        let Some(at) = line.find(CALL) else {
            continue;
        };
        let argument = line[at + CALL.len()..].trim_start();
        if argument.starts_with('"') {
            violations.push(format!("{}:{}: {}", rel, index + 1, line.trim()));
        }
    }
    violations
}

#[test]
fn no_production_site_writes_a_bare_schema_version_literal() {
    let mut violations = Vec::new();
    for path in production_files() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        violations.extend(version_literal_violations(&relative_to_root(&path), &text));
    }
    assert!(
        violations.is_empty(),
        "a schema version written as a bare literal. The event envelope's version \
         has one home — `agent_protocol::ids::SchemaVersion::current()` (which \
         carries `SchemaVersion::CURRENT`). A second `\"0.1.0\"` is a second \
         answer to \"which schema is this log?\", and two answers drift: a bump \
         that misses one leaves a log with two versions. CONTRACT.md §6 C6.1 / \
         STYLE.md §1.\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn the_one_home_still_exists() {
    let home = workspace_root().join("crates/agent-protocol/src/ids.rs");
    let text = fs::read_to_string(&home).expect("the schema version's home");
    assert!(
        text.contains("pub const CURRENT: &'static str"),
        "`SchemaVersion::CURRENT` is gone from `crates/agent-protocol/src/ids.rs` — \
         the events' version must still be declared in exactly one place \
         (CONTRACT.md §6 C6.1)."
    );
}

// --- Probes: the guard's shape, pinned. -------------------------------------

#[test]
fn a_literal_argument_is_a_violation_but_a_name_is_not() {
    assert_eq!(
        version_literal_violations(
            "x.rs",
            "    schema_version: SchemaVersion::from_string(\"0.1.0\"),\n"
        )
        .len(),
        1
    );
    // The single home names a constant, not a literal.
    assert!(
        version_literal_violations("x.rs", "        Self::from_string(Self::CURRENT)\n").is_empty()
    );
    // Loading a version read back from a log is fine — it is not a second home.
    assert!(
        version_literal_violations("x.rs", "    let v = SchemaVersion::from_string(recorded);\n")
            .is_empty()
    );
}

#[test]
fn a_call_inside_the_test_module_is_not_counted() {
    let text = "fn f() -> u8 { 0 }\n\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = SchemaVersion::from_string(\"0.1.0\"); }\n}\n";
    assert!(version_literal_violations("x.rs", text).is_empty());
    let text = "fn f() { let _ = SchemaVersion::from_string(\"0.1.0\"); }\n\n#[cfg(test)]\nmod tests {\n}\n";
    assert_eq!(version_literal_violations("x.rs", text).len(), 1);
}

#[test]
#[should_panic(expected = "does not open `mod tests`")]
fn a_cfg_test_that_is_not_a_tests_module_stops_the_guard() {
    let _ = production_lines("fn f() {}\n\n#[cfg(test)]\nfn helper() {}\n");
}

#[test]
fn a_comment_may_name_the_call_it_talks_about() {
    assert!(
        version_literal_violations("x.rs", "// never SchemaVersion::from_string(\"0.1.0\")\n")
            .is_empty()
    );
    assert!(
        version_literal_violations("x.rs", "/// why not SchemaVersion::from_string(\"0.1.0\")\n")
            .is_empty()
    );
    // Code with a trailing comment is still read.
    assert_eq!(
        version_literal_violations(
            "x.rs",
            "let s = SchemaVersion::from_string(\"0.1.0\"); // ok?\n"
        )
        .len(),
        1
    );
}

#[test]
fn the_guard_is_line_based_and_that_is_its_limit() {
    // Two ways a real literal would slip past a line scanner, asserted so nobody
    // has to rediscover the limit: the guard is a ratchet on the shape of the
    // code, not a proof about it.
    assert!(
        version_literal_violations(
            "x.rs",
            "let s = SchemaVersion::\n    from_string(\"0.1.0\");\n"
        )
        .is_empty()
    );
    assert!(
        version_literal_violations(
            "x.rs",
            "let s = SchemaVersion::from_string(\n    \"0.1.0\");\n"
        )
        .is_empty()
    );
}
