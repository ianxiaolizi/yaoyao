//! Boundary test for STYLE.md §1: every crate root denies `unsafe_code`, with
//! one exact-name exception.
//!
//! The exception is `crates/agent-sandbox-win`: the AppContainer launcher, where
//! containment is the product and Windows offers no safe surface for a job
//! object, an attribute list or a DACL edit. It is named by **member path**, the
//! list is closed, and a second crate claiming the exception fails
//! [`the_unsafe_exception_list_has_exactly_one_entry`]. The exception must also
//! carry `#![allow(unsafe_code)]` — without it the crate would not compile, but
//! the guard says so rather than letting "this crate is special" mean anything
//! the next person wants it to.
//!
//! The rule is one line at the top of each member's crate root. It is only as
//! good as its enforcement — a convention that depends on remembering is one a
//! new crate can quietly miss, which is exactly how `agent-tools-mcp` came to be
//! the odd one out of twenty (YA-28). This guard makes "forgot the line" a
//! failing test rather than something a reviewer has to notice.
//!
//! It walks the members the root manifest *declares* — not the directories that
//! happen to exist — so a member is covered the moment it is listed, and reads
//! each crate root in the header only: the attribute is an inner attribute, so
//! it belongs above the first item, after the `//!` documentation. A member
//! that declares both a library and a binary has two roots (`src/lib.rs` and
//! `src/main.rs`); both are read, because "the crate root" is not one file when
//! the package ships two targets.
//!
//! The guard is a **text** guard, and says so. It is blind to a `/* … */` block
//! comment written before the attribute (read as the first item, so the header
//! ends early and the file is reported as missing a line it has), and it reads
//! only those two root files — a further target such as `src/bin/*` or an
//! `examples/*` test-double server is not read, the same scope the clock-and-id
//! guard keeps. The probes at the bottom pin these limits rather than hide them:
//! a guard whose blind spots are undocumented gets trusted for more than it does.

use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <workspace>/crates/agent-testkit.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// The members the root manifest declares, exactly as written — every entry
/// kept, so a member without a slash is not silently dropped.
fn workspace_members(root_manifest: &Value) -> Vec<String> {
    root_manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(Value::as_array)
        .expect("`[workspace] members` is an array")
        .iter()
        .map(|member| member.as_str().expect("a workspace member is a path string").to_string())
        .collect()
}

/// Every crate root a member declares: `src/lib.rs` and/or `src/main.rs`.
/// Empty means the member has neither and this guard cannot speak to it.
fn crate_roots(member_dir: &Path) -> Vec<PathBuf> {
    ["src/lib.rs", "src/main.rs"]
        .iter()
        .map(|candidate| member_dir.join(candidate))
        .filter(|path| path.is_file())
        .collect()
}

/// The members allowed to contain `unsafe` code, by member path.
///
/// Exactly one, and closed on purpose: the claim "this code needs `unsafe`" is
/// a security argument, and an allowlist that grows by adding a line is not an
/// argument. Adding a second entry means editing this list, the comment above
/// it and `STYLE.md` §1 in the same commit.
const UNSAFE_EXCEPTIONS: &[&str] = &["crates/agent-sandbox-win"];

/// Whether the crate root's header carries the named inner attribute.
///
/// The header is what precedes the first item: blank lines and `//` comments
/// (including the `//!` module docs) are skipped, `#![…]` lines are the inner
/// attributes, and anything else ends the header. Only a real `#![deny(…)]`
/// line counts, so the attribute named in a comment does not pass as itself.
fn header_carries(text: &str, attribute: &str) -> bool {
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(inner) = line.strip_prefix("#![") {
            if inner.starts_with(attribute) {
                return true;
            }
            continue;
        }
        return false; // the first item: the header ended without the attribute
    }
    false
}

fn header_denies_unsafe_code(text: &str) -> bool {
    header_carries(text, "deny(unsafe_code)")
}

fn header_allows_unsafe_code(text: &str) -> bool {
    header_carries(text, "allow(unsafe_code)")
}

#[test]
fn every_member_crate_root_denies_unsafe_code() {
    let root = workspace_root();
    let manifest: Value = toml::from_str(
        &fs::read_to_string(root.join("Cargo.toml")).expect("read the root manifest"),
    )
    .expect("parse the root manifest");

    let mut missing = Vec::new();
    for member in workspace_members(&manifest) {
        if UNSAFE_EXCEPTIONS.contains(&member.as_str()) {
            continue;
        }
        let roots = crate_roots(&root.join(&member));
        if roots.is_empty() {
            missing.push(format!("{member} (no src/lib.rs or src/main.rs)"));
            continue;
        }
        for crate_root in roots {
            let text = fs::read_to_string(&crate_root)
                .unwrap_or_else(|error| panic!("read {}: {error}", crate_root.display()));
            if !header_denies_unsafe_code(&text) {
                let shown = crate_root.strip_prefix(&root).unwrap_or(&crate_root);
                missing.push(format!("{member} ({})", shown.display()));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "crate roots whose header lacks `#![deny(unsafe_code)]`: {missing:?}"
    );
}

#[test]
fn the_unsafe_exception_list_has_exactly_one_entry() {
    // The list is the exception to a rule, not a registry. A second entry is a
    // design change that needs its own argument in STYLE.md §1, which is what
    // editing this list in the same commit records.
    assert_eq!(
        UNSAFE_EXCEPTIONS,
        &["crates/agent-sandbox-win"],
        "a new `unsafe` crate needs an argued amendment to STYLE.md §1"
    );
}

#[test]
fn the_exception_crate_root_allows_unsafe_code() {
    // Without the attribute the crate would not compile, but the guard states
    // the requirement instead of relying on the compiler as the only witness:
    // the exception is "allow", never "silently permit everything".
    let root = workspace_root();
    for member in UNSAFE_EXCEPTIONS {
        let roots = crate_roots(&root.join(member));
        assert!(!roots.is_empty(), "{member} declares no crate root");
        for crate_root in roots {
            let text = fs::read_to_string(&crate_root)
                .unwrap_or_else(|error| panic!("read {}: {error}", crate_root.display()));
            assert!(
                header_allows_unsafe_code(&text),
                "{} is on the exception list but its header lacks \
                 `#![allow(unsafe_code)]`",
                crate_root.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Probes: every crate root in this repository already carries the line, so a
// guard that always answers "present" would pass and mean nothing. Feed the
// checker the shapes it has to tell apart, and assert it tells them apart.
// ---------------------------------------------------------------------------

#[test]
fn a_header_without_the_attribute_is_rejected() {
    assert!(!header_denies_unsafe_code("pub mod connection;\n"));
    assert!(!header_denies_unsafe_code("//! docs\n\nuse std::fs;\n"));
}

#[test]
fn the_attribute_named_only_in_a_comment_does_not_count() {
    assert!(!header_denies_unsafe_code("//! add #![deny(unsafe_code)] later\n\npub mod a;\n"));
    assert!(!header_denies_unsafe_code("// #![deny(unsafe_code)]\npub mod a;\n"));
}

#[test]
fn the_attribute_is_found_after_module_docs_and_other_inner_attributes() {
    assert!(header_denies_unsafe_code("//! docs\n\n#![deny(unsafe_code)]\n\npub mod a;\n"));
    assert!(header_denies_unsafe_code(
        "#![allow(clippy::module_name_repetitions)]\n#![deny(unsafe_code)]\npub mod a;\n"
    ));
}

#[test]
fn the_attribute_after_the_first_item_is_not_a_header_attribute() {
    // An inner attribute below the first item would not compile; the guard is a
    // text guard, so it asserts the rule it can actually keep — header only.
    assert!(!header_denies_unsafe_code("pub mod a;\n\n#![deny(unsafe_code)]\n"));
}

#[test]
fn allow_and_deny_are_told_apart() {
    // The exception check and the rule check read the same header with two
    // different attributes. A shared prefix match would make `allow` satisfy
    // `deny` (or the reverse) and both checks would pass on the wrong file.
    assert!(header_allows_unsafe_code("#![allow(unsafe_code)]\npub mod a;\n"));
    assert!(!header_allows_unsafe_code("#![deny(unsafe_code)]\npub mod a;\n"));
    assert!(header_denies_unsafe_code("#![deny(unsafe_code)]\npub mod a;\n"));
    assert!(!header_denies_unsafe_code("#![allow(unsafe_code)]\npub mod a;\n"));
    assert!(!header_allows_unsafe_code("//! #![allow(unsafe_code)]\npub mod a;\n"));
}
