//! Boundary test for CONTRACT.md §7 裁决 #20: time is an input, ids come from
//! one place.
//!
//! Two rules, both stated in `STYLE.md` §1 and both only as good as their
//! enforcement — a sentence nobody can fail is decoration:
//!
//!   1. **A wall clock read is a stamp, or one of two edges.** In production
//!      source a `Utc::now()` may appear only as the initializer of a
//!      `created_at` / `recorded_at` field, on the line in
//!      `ApprovalService::fold` that hands `fold_at(session, now)` its instant,
//!      or in `apps/agent/src/clock.rs`, the process edge's one reading. Every
//!      decision that *uses* a time takes `now` as a parameter; those two edges
//!      are the only places the clock is read.
//!   2. **Ids are minted in one place.** `agent-protocol::ids::fresh_id()` is
//!      the only draw of randomness in the workspace, so `Uuid::new_v4` occurs
//!      exactly once in production source and no other random API occurs at all.
//!
//! The guard is a **text** guard, and says so: it reads source lines, not types.
//! The clock reached through a function pointer, a call split across a newline,
//! or a file under a directory this walk does not visit would slip past it. The
//! probes at the bottom pin that limit rather than hide it — a guard whose
//! blind spots are undocumented gets trusted for more than it does.

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
/// The rule is about the paths a build runs as the framework; the test-double
/// servers (`fake-openai-server`, `fake-subagent`) are `examples/` targets now,
/// a directory this walk never visits, and integration tests under `*/tests/`
/// are out of scope for the same reason — they call the framework, they are not
/// it. `src/bin` stays excluded for the day a package grows a second binary
/// that *is* the framework.
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

/// The lines of `text` that a normal build compiles: everything before the
/// file's test module.
///
/// It cuts at the first standalone `#[cfg(test)]` and **asserts the next
/// non-blank line opens `mod tests`**. The workspace holds that shape today
/// (checked across every `src` file when the guard was written). If a file ever
/// grows a `#[cfg(test)]` that is not a tests module — a conditional `use`, a
/// conditional helper — this panics instead of quietly counting test code as
/// production or the reverse. A guard that silently mis-scopes is worse than
/// one that stops.
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

/// A line that is entirely a comment. The rules are about code, and a doc or a
/// comment may name `Utc::now` or `Uuid::new_v4` freely — `fresh_id`'s own doc
/// explains why it draws a random uuid, and that sentence must not read as a
/// second mint. A *trailing* comment on a line that also holds code is not
/// skipped; that is the strict direction, and the case is rare enough to pay.
fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// A field initializer that records when something happened. These are stamps:
/// they carry a time into a log or a record, and nothing reads one back to
/// decide anything (checked by hand for 裁决 #20 — see CONTRACT.md §7).
///
/// `started_at` / `heartbeat_at` are the driver record's two
/// (`agent_ports::event_store::DriverRecord`, 裁决 #112). Stamps in the same
/// sense, and more strictly so: the record is a diagnostic, the driver claim is
/// decided by a file lock, and nothing in the workspace compares either field
/// to a clock.
const STAMP_SHAPES: [&str; 4] = [
    "created_at: Utc::now()",
    "recorded_at: Utc::now()",
    "started_at: Utc::now()",
    "heartbeat_at: Utc::now()",
];

/// The only places allowed to read a wall clock outside a stamp: the approval
/// boundary that hands `fold_at` its instant, and the app edge that hands
/// `now` to the scheduler.
const WALL_CLOCK_BOUNDARIES: [(&str, &str); 2] = [
    ("crates/agent-kernel/src/approval.rs", "self.fold_at(session_id, Utc::now()).await"),
    ("apps/agent/src/clock.rs", "Utc::now()"),
];

/// The one file allowed to draw randomness.
const MINT_FILE: &str = "crates/agent-protocol/src/ids.rs";

/// Lines of `text` that read a wall clock without being a stamp or the
/// boundary. `rel` only names the file in the message.
fn wall_clock_violations(rel: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (index, line) in production_lines(text).iter().enumerate() {
        if is_comment(line) || !line.contains("Utc::now()") {
            continue;
        }
        let mut rest = line.to_string();
        for shape in STAMP_SHAPES {
            rest = rest.replace(shape, "");
        }
        if !rest.contains("Utc::now()") {
            continue;
        }
        let trimmed = line.trim();
        if WALL_CLOCK_BOUNDARIES.iter().any(|(file, site)| rel == *file && trimmed == *site) {
            continue;
        }
        violations.push(format!("{}:{}: {}", rel, index + 1, line.trim()));
    }
    violations
}

#[test]
fn a_wall_clock_is_read_only_as_a_stamp_or_at_the_one_boundary() {
    let mut violations = Vec::new();
    for path in production_files() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        violations.extend(wall_clock_violations(&relative_to_root(&path), &text));
    }
    assert!(
        violations.is_empty(),
        "a wall clock read that is neither a stamp nor the fold boundary. If it \
         belongs in a decision, take `now` as a parameter instead (see \
         `ApprovalService::fold_at`); if it is a new stamp, write it as a \
         `created_at`/`recorded_at` initializer. CONTRACT.md §7 裁决 #20.\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn ids_are_minted_in_exactly_one_place() {
    let mut sites = Vec::new();
    for path in production_files() {
        let rel = relative_to_root(&path);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (index, line) in production_lines(&text).iter().enumerate() {
            if !is_comment(line) && line.contains("Uuid::new_v4") {
                sites.push(format!("{}:{}", rel, index + 1));
            }
        }
    }
    assert_eq!(
        sites.len(),
        1,
        "`Uuid::new_v4` must appear exactly once in production source — every id \
         in the framework is drawn inside `{MINT_FILE}`'s `fresh_id`. A bare \
         `Uuid::new_v4().to_string()` elsewhere is a second answer to \"where do \
         ids come from?\". Found: {sites:?}. CONTRACT.md §7 裁决 #20."
    );
    assert!(
        sites[0].starts_with(&format!("{MINT_FILE}:")),
        "the one `Uuid::new_v4` is in {} — it must be in {MINT_FILE}. \
         CONTRACT.md §7 裁决 #20.",
        sites[0]
    );
}

#[test]
fn no_other_randomness_or_wall_clock_api_is_reached_for() {
    // The one id mint is asserted above; these would be a second source of
    // either randomness or wall time, and the workspace has none.
    const FORBIDDEN: [&str; 6] =
        ["rand::", "thread_rng", "getrandom", "SystemTime::now", "chrono::Local", "OsRng"];
    let mut hits = Vec::new();
    for path in production_files() {
        let rel = relative_to_root(&path);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (index, line) in production_lines(&text).iter().enumerate() {
            if is_comment(line) {
                continue;
            }
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    hits.push(format!("{}:{}: {}", rel, index + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "randomness or wall clock reached for outside the sanctioned mint. \
         CONTRACT.md §7 裁决 #20.\n  {}",
        hits.join("\n  ")
    );
}

// --- Probes: the guard's shape, pinned. -------------------------------------

#[test]
fn a_stamp_is_not_a_violation_but_a_second_read_on_the_same_line_is() {
    assert!(wall_clock_violations("x.rs", "            created_at: Utc::now(),\n").is_empty());
    assert!(
        wall_clock_violations("x.rs", "        Self { recorded_at: Utc::now(), event }\n")
            .is_empty()
    );
    let both = "let a = Utc::now(); let b = created_at: Utc::now();\n";
    assert_eq!(wall_clock_violations("x.rs", both).len(), 1);
}

#[test]
fn a_boundary_is_allowed_only_on_its_own_line_in_its_own_file() {
    for (file, site) in WALL_CLOCK_BOUNDARIES {
        let boundary = format!("        {site}\n");
        assert!(
            wall_clock_violations(file, &boundary).is_empty(),
            "{file}:{site} is a sanctioned edge"
        );
        // The same line anywhere else is a violation: the exception is the
        // site, not the text.
        assert_eq!(
            wall_clock_violations("crates/agent-kernel/src/inbox.rs", &boundary).len(),
            1,
            "{file}:{site} outside its file"
        );
    }
}

#[test]
fn a_call_inside_the_test_module_is_not_counted() {
    let text =
        "fn f() -> u8 { 0 }\n\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = Utc::now(); }\n}\n";
    assert!(wall_clock_violations("x.rs", text).is_empty());
    let text = "fn f() { let _ = Utc::now(); }\n\n#[cfg(test)]\nmod tests {\n}\n";
    assert_eq!(wall_clock_violations("x.rs", text).len(), 1);
}

#[test]
#[should_panic(expected = "does not open `mod tests`")]
fn a_cfg_test_that_is_not_a_tests_module_stops_the_guard() {
    // The scoping rule is an assumption about the tree's shape; when the tree
    // stops matching it, the guard must fail rather than guess.
    let _ = production_lines("fn f() {}\n\n#[cfg(test)]\nfn helper() {}\n");
}

#[test]
fn a_comment_may_name_the_api_it_talks_about() {
    assert!(wall_clock_violations("x.rs", "// reads Utc::now() nowhere, promise\n").is_empty());
    assert!(wall_clock_violations("x.rs", "/// why not Uuid::new_v4 here\n").is_empty());
    // A line that is code *and* a trailing comment is still read: the strict
    // direction is the one that fails closed.
    assert_eq!(wall_clock_violations("x.rs", "let _ = Utc::now(); // ok?\n").len(), 1);
}

#[test]
fn the_guard_is_line_based_and_that_is_its_limit() {
    // Two ways a real read would slip past a line scanner. Both are asserted so
    // nobody has to rediscover the limit — the guard is a ratchet on the shape
    // of the code, not a proof about it.
    assert!(wall_clock_violations("x.rs", "let f = Utc::now;\n").is_empty()); // a pointer, no call
    assert!(wall_clock_violations("x.rs", "let t = Utc::\n    now();\n").is_empty()); // split
}
