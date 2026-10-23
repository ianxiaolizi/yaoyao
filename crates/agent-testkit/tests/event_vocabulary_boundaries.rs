//! Boundary test for YA-38: the core event table names only events that exist.
//!
//! `EventType::ALL` is not documentation. It is what `is_core()` reads, and
//! `is_core()` is what a fold consults to tell "a type I know about but have no
//! use for" from "a type I have never heard of" — the first is skipped, the
//! second fails closed (`agent-projection/src/model_context.rs`,
//! `session.rs`). So an entry in that table makes a claim about what this build
//! can meet in a log, and six of them made it falsely: `inbox/promoted`,
//! `inbox/updated`, `inbox/wakeup`, `model/stream.completed`, `tool/proposed`
//! and `policy/decision` were declared from the project's first week and never
//! written by anything, nor read. Nothing failed, because nothing ever produced
//! one — which is exactly why only a guard will notice the seventh.
//!
//! The rule this keeps is the ledger's "done" criterion for that item: every
//! entry in the table is referenced somewhere outside the file that declares it
//! — a producer, a fold arm, a renderer, a test that asserts the type was
//! actually written. That is a **text** rule, and the guard says so: a mention
//! in a comment counts, and a name assembled at runtime (`EventType::new` over a
//! `format!`) is invisible. The probes at the bottom pin both limits rather than
//! hide them.
//!
//! It is also the *weaker* half of that criterion, on purpose: it asks for one
//! reference, not for a writer *and* a reader, because those cannot be told
//! apart in text. `session/closed` used to be the case that made the difference
//! visible — the projection read it and nothing in this build wrote it, so it
//! passed here. That asymmetry had its own ledger item (YA-73), and the item
//! deleted the name rather than giving it a writer: nothing in this product
//! closes a session, so the only honest fix was to stop claiming a close exists.
//! The limit remains: if a reader is ever added for a type nothing writes, this
//! guard will not notice.

use std::fs;
use std::path::{Path, PathBuf};

/// The file the table lives in — the one source a type may *not* be counted in.
const EVENTS: &str = "crates/agent-protocol/src/events.rs";

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <workspace>/crates/agent-testkit.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn events_source(root: &Path) -> String {
    let path = root.join(EVENTS);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Every `pub const NAME: &'static str = "value";` in the events module, as
/// `(name, value)` in declaration order.
fn declared(source: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once(": &'static str = \"") else {
            continue;
        };
        let Some((value, _)) = rest.split_once('"') else {
            continue;
        };
        out.push((name.to_string(), value.to_string()));
    }
    out
}

/// The entries of `EventType::ALL`, in order.
fn core_table(source: &str) -> Vec<String> {
    let start = source
        .find("pub const ALL: &'static [&'static str] = &[")
        .expect("the events module declares the core table")
        + "pub const ALL: &'static [&'static str] = &[".len();
    let body = &source[start..];
    let end = body.find("\n    ];").expect("the core table is a bracketed list");
    let mut out = Vec::new();
    for line in body[..end].lines() {
        let line = line.trim();
        let Some(name) = line.strip_prefix("Self::") else {
            continue;
        };
        out.push(name.trim_end_matches(',').to_string());
    }
    out
}

/// Every `.rs` file under `crates/` and `apps/`, with its text.
///
/// Both trees are walked rather than a member list read, because what a type
/// needs is a *reference* and that can sit in any package — the producer in the
/// kernel, a fold arm in the projection, a renderer in the CLI. `target/` hangs
/// off the workspace root and so is never visited.
fn sources(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                out.push((path, text));
            }
        }
    }

    let mut out = Vec::new();
    for tree in ["crates", "apps"] {
        walk(&root.join(tree), &mut out);
    }
    out.retain(|(path, _)| !path.ends_with(EVENTS));
    out
}

/// The first file that mentions `name` or its wire `value`, if any.
///
/// Two spellings, because a producer may write the constant or build the type
/// from its string; both mean the type is met with in this repository.
fn referenced(name: &str, value: &str, files: &[(PathBuf, String)]) -> Option<PathBuf> {
    let constant = format!("EventType::{name}");
    let literal = format!("\"{value}\"");
    files
        .iter()
        .find(|(_, text)| text.contains(&constant) || text.contains(&literal))
        .map(|(path, _)| path.clone())
}

#[test]
fn every_core_event_type_is_referenced_outside_its_own_file() {
    let root = workspace_root();
    let source = events_source(&root);
    let files = sources(&root);

    let known = declared(&source);
    let mut unwritten = Vec::new();
    for name in core_table(&source) {
        let Some((_, value)) = known.iter().find(|(declared, _)| *declared == name) else {
            unwritten.push(format!("{name} (not declared in this file)"));
            continue;
        };
        if referenced(&name, value, &files).is_none() {
            unwritten.push(format!("{name} = {value:?}"));
        }
    }

    assert!(
        unwritten.is_empty(),
        "core event types nothing writes or reads: {unwritten:?} — the table says this build \
         knows them; either give them a producer and a reader, or take them out of `EventType::ALL` \
         (a type this build does not know fails closed, which is the honest answer)"
    );
}

#[test]
fn the_core_table_matches_the_types_this_module_declares() {
    let source = events_source(&workspace_root());
    let entries = core_table(&source);
    let declared = declared(&source);

    // A parser that quietly returns nothing would make the guard above pass
    // vacuously, so the parsing itself is asserted before it is trusted.
    assert!(entries.len() > 40, "the core table parsed as {} entries", entries.len());

    let missing: Vec<&String> =
        entries.iter().filter(|entry| !declared.iter().any(|(name, _)| name == *entry)).collect();
    assert!(
        missing.is_empty(),
        "entries in the core table that this module does not declare: {missing:?} — a typo here \
         is a type no fold can ever match"
    );

    let outside: Vec<&String> =
        declared.iter().map(|(name, _)| name).filter(|name| !entries.contains(name)).collect();
    assert!(
        outside.is_empty(),
        "declared event types missing from the core table: {outside:?} — a constant the table does \
         not list makes `is_core()` answer no for an event the build itself wrote"
    );
}

// ---------------------------------------------------------------------------
// Probes: every type in the table today is referenced, so a guard that always
// answers "written" would pass and mean nothing. Feed the checker the shapes it
// has to tell apart, and pin the ones it cannot.
// ---------------------------------------------------------------------------

fn files_with(text: &str) -> Vec<(PathBuf, String)> {
    vec![(PathBuf::from("probe.rs"), text.to_string())]
}

#[test]
fn a_mentioned_type_is_told_from_an_unmentioned_one() {
    let files = files_with("draft(ctx, EventType::TOOL_STARTED, json!({}), Actor::Agent);\n");
    assert!(referenced("TOOL_STARTED", "tool/started", &files).is_some());
    // Not a name it was asked about.
    assert!(referenced("TOOL_COMPLETED", "tool/completed", &files).is_none());
}

#[test]
fn a_type_built_from_its_wire_string_counts() {
    // A producer may spell the type rather than the constant; that is still a
    // place the type is met with in this repository.
    let files = files_with("EventType::new(\"inbox/wakeup\")\n");
    assert!(referenced("INBOX_WAKEUP", "inbox/wakeup", &files).is_some());
}

#[test]
fn a_mention_in_a_comment_counts_and_that_is_the_blind_spot() {
    // Pinned rather than hidden: the rule is textual, so prose about a type is
    // indistinguishable from a use of it. It errs toward silence — a guard that
    // is too easy to satisfy is worth knowing about, not worth pretending about.
    let files = files_with("// someday the fold will write \"inbox/promoted\"\n");
    assert!(referenced("INBOX_PROMOTED", "inbox/promoted", &files).is_some());
}
