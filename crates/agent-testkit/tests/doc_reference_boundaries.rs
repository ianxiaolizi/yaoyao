//! Boundary test for `STYLE.md` §5: code does not point at a document that is
//! no longer in this repository.
//!
//! The package used to carry a long design spec, and comments cited it by
//! section — a section mark followed by a dotted subsection, or the acceptance
//! appendix. The spec left the tree and the citations stayed, so a reader
//! following one found nothing. The rule is in `STYLE.md` §5; this guard is what
//! makes it fail when broken.
//!
//! Two shapes are refused outright:
//!
//!   1. **A dotted section reference** — the section mark, digits, and a dot
//!      followed by another digit. The in-repo anchors are bare sections
//!      (`CONTRACT.md §7`, `§7 row 4`, `§3 C3.5`); the out-of-tree spec is the
//!      only thing that had subsections, so a dotted citation is one nobody can
//!      follow.
//!   2. **The milestone appendix** — the section mark followed by `18`. That was
//!      the spec's acceptance-criteria appendix; milestone names (`M4`, `M6`)
//!      are still fine as labels, but a citation of the appendix is not.
//!
//! A third scan refuses the stale progress pointer this cleanup removed: a
//! comment that defers a capability to M6 as a future milestone. M6 is
//! delivered, so such a comment is either wrong or owes the reader the actual
//! gap.
//!
//! The guard is a **text** guard. It does not know whether a section mark is in
//! a comment or a string — deliberately, since the cleanup found one inside a
//! user-facing error message — and it reads characters, not Markdown structure.
//! The probes at the bottom pin what it does not see: a bare two-digit section
//! other than 18, a dot separated from the number by a space, and a phrase split
//! across a line break. A citation split across two comment lines as `12` /
//! `.6` is likewise invisible; that is the ratchet's limit, stated rather than
//! hidden.

use std::fs;
use std::path::{Path, PathBuf};

/// Written as an escape so this file does not trip its own scan.
const SECTION_MARK: char = '\u{a7}';

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

/// Every `*.rs` under `<root>/{crates,apps}`, sorted. Unlike the production-only
/// guards this one includes tests and examples: a citation in a test comment is
/// just as broken as one in `src`, and the acceptance tests are where most of
/// the appendix references lived.
fn all_rust_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = Vec::new();
    for top in ["crates", "apps"] {
        rust_sources(&root.join(top), &mut files);
    }
    files.sort();
    files
}

fn relative_to_root(path: &Path) -> String {
    path.strip_prefix(workspace_root()).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Lines carrying a section mark that the in-repo documentation cannot answer.
fn section_reference_violations(rel: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        for (at, &ch) in chars.iter().enumerate() {
            if ch != SECTION_MARK {
                continue;
            }
            let rest: String = chars[at + 1..].iter().collect();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                continue;
            }
            let mut after = rest[digits.len()..].chars();
            // A sentence-ending period (`STYLE.md §5.`) is not a subsection;
            // a subsection is a dot followed by a digit (`12.8`).
            let subsection =
                after.next() == Some('.') && after.next().is_some_and(|c| c.is_ascii_digit());
            let appendix = digits == "18";
            if subsection || appendix {
                violations.push(format!("{}:{}: {}", rel, index + 1, line.trim()));
            }
        }
    }
    violations
}

/// The stale pointers, spelled with escapes so this file does not trip its own
/// scan.
const STALE_POINTERS: [&str; 3] = ["M6\x20progress", "M6\x27s problem", "for\x20M6"];

fn stale_milestone_violations(rel: &str, text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if STALE_POINTERS.iter().any(|needle| line.contains(needle)) {
            violations.push(format!("{}:{}: {}", rel, index + 1, line.trim()));
        }
    }
    violations
}

#[test]
fn no_rust_source_cites_an_out_of_tree_section() {
    let mut violations = Vec::new();
    for path in all_rust_files() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        violations.extend(section_reference_violations(&relative_to_root(&path), &text));
    }
    assert!(
        violations.is_empty(),
        "a comment cites a section of a document that is not in this repository. \
         The in-repo anchors are bare CONTRACT.md sections — `CONTRACT.md §7`, \
         `§7 row 4`, `§3 C3.5` — or the thing can simply be said. A dotted \
         section or the appendix number cannot be looked up by anyone reading \
         this tree (STYLE.md §5).\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn no_rust_source_defers_work_to_a_finished_milestone() {
    let mut violations = Vec::new();
    for path in all_rust_files() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        violations.extend(stale_milestone_violations(&relative_to_root(&path), &text));
    }
    assert!(
        violations.is_empty(),
        "a comment points at M6 as a future event or at its progress notes. M6 is \
         delivered; if something is still missing, name the gap itself.\n  {}",
        violations.join("\n  ")
    );
}

// --- Probes: the guard's shape and its blind spots, pinned. -----------------

#[test]
fn a_dotted_reference_is_a_violation() {
    let decisions = format!("//! decisions ({SECTION_MARK}12.8).\n");
    assert_eq!(section_reference_violations("x.rs", &decisions).len(), 1);
    let limits = format!("//! limits ({SECTION_MARK}10.3–10.5).\n");
    assert_eq!(section_reference_violations("x.rs", &limits).len(), 1);
    // Inside a string, not a comment — the cleanup found one there.
    let in_string = format!("let e = \"needs the {SECTION_MARK}12.8 gates\";\n");
    assert_eq!(section_reference_violations("x.rs", &in_string).len(), 1);
}

#[test]
fn the_milestone_appendix_is_a_violation() {
    let appendix = format!("//! ({SECTION_MARK}18 M5 criterion 3)\n");
    assert_eq!(section_reference_violations("x.rs", &appendix).len(), 1);
    // A milestone name on its own is a label, not a citation.
    assert!(section_reference_violations("x.rs", "//! M5 criterion 3\n").is_empty());
}

#[test]
fn the_in_repo_anchors_are_allowed() {
    let anchors = [
        "/// the widest refusal (CONTRACT.md §7 row 3).\n".to_string(),
        "// it must not re-introduce the secret (§7 row 4).\n".to_string(),
        "/// it exits `2` (§3 C3.5).\n".to_string(),
        format!("//! Boundary test for STYLE.md {SECTION_MARK}5.\n"),
        "// CONTRACT §4 C4.5)\n".to_string(),
    ];
    for line in anchors {
        assert!(
            section_reference_violations("x.rs", &line).is_empty(),
            "an in-repo anchor was refused: {line:?}"
        );
    }
}

#[test]
fn the_appendix_check_narrows_the_rule_to_18() {
    // Bare two-digit sections other than the appendix are not seen. No such
    // citation exists today; the probe records that the guard is about the
    // shapes the cleanup removed, not about every possible section number.
    let bare = format!("//! see {SECTION_MARK}15\n");
    assert!(section_reference_violations("x.rs", &bare).is_empty());
}

#[test]
fn a_dot_after_whitespace_is_invisible() {
    // The guard reads a dot immediately after the digits. A citation written as
    // `12 .8` would slip past — a cost of staying a simple text ratchet.
    let spaced = format!("//! see {SECTION_MARK}12 .8\n");
    assert!(section_reference_violations("x.rs", &spaced).is_empty());
}

#[test]
fn a_citation_split_across_lines_is_invisible() {
    let split = format!("//! see {SECTION_MARK}12\n//! .8\n");
    assert!(section_reference_violations("x.rs", &split).is_empty());
}

#[test]
fn a_stale_milestone_pointer_is_a_violation() {
    let progress = format!("// see the M6{}progress notes\n", '\u{20}');
    assert_eq!(stale_milestone_violations("x.rs", &progress).len(), 1);
    let problem = format!("// Containment is M6{}s problem\n", '\u{27}');
    assert_eq!(stale_milestone_violations("x.rs", &problem).len(), 1);
    let planned = format!("// Committing is planned for{}M6\n", '\u{20}');
    assert_eq!(stale_milestone_violations("x.rs", &planned).len(), 1);
    // The acceptance suite's own labels are not pointers.
    assert!(stale_milestone_violations("x.rs", "// M6 anchor 1 — a plugin\n").is_empty());
}
