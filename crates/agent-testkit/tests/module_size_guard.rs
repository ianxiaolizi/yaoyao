//! Module size guard: the split files may shrink, and nothing may grow back
//! into one.
//!
//! P2 split `agent-loop`'s per-turn context, the scheduler and the CLI entry
//! point into modules. A split is only durable if the tree that results is
//! cheaper to keep than to undo, so this test pins it two ways:
//!
//!   1. no source file under `crates/*/src` or `apps/*/src` may pass 2,000
//!      lines, and
//!   2. the files that already passed it have an exact ceiling — their size on
//!      the day the guard landed — so they can shrink but never grow. A file
//!      that shrinks back under the ceiling leaves the list: `resume.rs` did
//!      that in B4, when its tests moved out of line.
//!
//! The ceilings are line counts, not byte counts: a line is the unit the
//! reader pays for and the one `wc -l` reports, so the number here is one a
//! person can check by hand. Test files are out of scope — a long fixture list
//! is not the monolith this guard is about.

use std::fs;
use std::path::{Path, PathBuf};

/// What every source file not named in [`RATCHETS`] may be.
const ORDINARY_CEILING: usize = 2000;

/// Files that were already over the ceiling when the guard landed, at their
/// size on that day. Shrink freely; growth is the regression.
const RATCHETS: &[(&str, usize)] = &[("crates/agent-loop/src/default/agent_loop_impl.rs", 2190)];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <workspace>/crates/agent-testkit.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Every `.rs` file under `<root>/<dir>`, recursively, as a workspace-relative
/// path with forward slashes.
fn sources(root: &Path, dir: &Path) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let relative = path
                .strip_prefix(root)
                .expect("a source file is inside the workspace")
                .display()
                .to_string()
                .replace('\\', "/");
            found.push((relative, text.lines().count()));
        }
    }
    found.sort();
    found
}

/// All source files the guard covers: each crate's `src` and each app's `src`.
fn covered_sources() -> Vec<(String, usize)> {
    let root = workspace_root();
    let mut all = Vec::new();
    for family in ["crates", "apps"] {
        let Ok(entries) = fs::read_dir(root.join(family)) else { continue };
        for entry in entries.flatten() {
            let src = entry.path().join("src");
            if src.is_dir() {
                all.extend(sources(&root, &src));
            }
        }
    }
    all
}

/// Test modules are fixtures and assertions, not production surface.
///
/// They still live under `src` so they can reach private items, but their size
/// is not the monolith this guard is about.
fn is_test_source(path: &str) -> bool {
    path.ends_with("/tests.rs") || path.contains("/tests/")
}

fn ratchet(path: &str) -> Option<usize> {
    RATCHETS.iter().find(|(name, _)| *name == path).map(|(_, ceiling)| *ceiling)
}

/// The guard would be worthless if a rename dropped a file out of the table.
#[test]
fn every_ratcheted_file_still_exists() {
    let files = covered_sources();
    for (path, _) in RATCHETS {
        assert!(
            files.iter().any(|(name, _)| name == path),
            "{path} is ratcheted but no longer exists; update RATCHETS or restore the file"
        );
    }
}

/// The whole point: a new file may not become the next monolith.
#[test]
fn no_unratcheted_source_file_passes_the_ordinary_ceiling() {
    let offenders: Vec<String> = covered_sources()
        .into_iter()
        .filter(|(path, lines)| {
            *lines > ORDINARY_CEILING && ratchet(path).is_none() && !is_test_source(path)
        })
        .map(|(path, lines)| format!("{path}: {lines} lines"))
        .collect();

    assert!(
        offenders.is_empty(),
        "source files grew past {ORDINARY_CEILING} lines; split them rather than raise this \
         number: {offenders:#?}"
    );
}

/// A ratcheted file can only move one way.
#[test]
fn a_ratcheted_file_never_grows() {
    for (path, lines) in covered_sources() {
        if is_test_source(&path) {
            continue;
        }
        let Some(ceiling) = ratchet(&path) else { continue };
        assert!(
            lines <= ceiling,
            "{path} is {lines} lines and may not pass its ratchet of {ceiling}; split it"
        );
    }
}

/// The entry point holds the dispatch and nothing that dispatches.
///
/// `main.rs` is the one file every change touches, so it is the one that grows
/// back first. The split moved every handler into `commands/`; this keeps them
/// there by naming what may live at the top level of the binary.
#[test]
fn the_entry_point_holds_no_command() {
    const ALLOWED: &[&str] = &["exit_status", "exit_on", "main", "usage", "positionals"];

    let path = workspace_root().join("apps/agent/src/main.rs");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let functions: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let rest = line
                .strip_prefix("pub(crate) fn ")
                .or_else(|| line.strip_prefix("pub fn "))
                .or_else(|| line.strip_prefix("async fn "))
                .or_else(|| line.strip_prefix("fn "))?;
            let name: String =
                rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            Some(name)
        })
        .filter(|name| !name.is_empty())
        .collect();

    let unexpected: Vec<&String> =
        functions.iter().filter(|name| !ALLOWED.contains(&name.as_str())).collect();
    assert!(
        unexpected.is_empty(),
        "these functions belong in a commands/ module, not in main.rs: {unexpected:#?}"
    );
}
