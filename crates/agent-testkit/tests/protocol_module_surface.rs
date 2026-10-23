//! Boundary test for YA-39: every module `agent-protocol` declares is used by
//! something outside it.
//!
//! The rule is scoped to that one crate on purpose. `agent-protocol` is the
//! crate whose public surface is a *contract other crates read*: its `pub`
//! modules are the vocabulary the rest of the workspace speaks, so a module
//! nothing reaches does not merely sit unused — it sends a reader to the wrong
//! place. `commands.rs` was exactly that: a whole request face
//! (`Command` / `CommandKind` / `CommandPayload`) with zero producers and zero
//! consumers, while the real request face was `InboxItem` all along. Somebody
//! reading `CommandKind::InboxInterrupt` had no way to learn it was fiction.
//!
//! The guard is a **text** guard, and says so. It recognises a module by the
//! `agent_protocol::<module>` shape, so a mention inside a comment counts (the
//! doc comment at `agent-cli/src/lib.rs` citing `agent_protocol::ids::…` is a
//! live example, not a hypothetical), and a reference assembled by a macro or
//! re-export is invisible. The probes at the bottom pin those limits rather than
//! hide them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The crate whose surface this guards, and the file that declares it.
const CRATE: &str = "crates/agent-protocol";
const LIB: &str = "crates/agent-protocol/src/lib.rs";

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <workspace>/crates/agent-testkit.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Every module the crate root declares as `pub mod NAME;`.
fn module_names(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub mod "))
        .filter_map(|rest| rest.strip_suffix(';'))
        .map(str::to_string)
        .collect()
}

/// Every `.rs` file under `crates/` and `apps/` **outside** the guarded crate,
/// with its text. The crate's own files are what a module may not count itself
/// in: a `pub mod` line is not a use of the module.
fn outside_sources(root: &Path) -> Vec<(PathBuf, String)> {
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
    out.retain(|(path, _)| !path.starts_with(root.join(CRATE)));
    out
}

/// The first path segment of each entry in a `{…}` list starting at `open`,
/// which indexes the `{`. Nested braces are skipped over, so
/// `{a::B, c::{d::E, f::G}}` yields `a` and `c`.
fn entries_of(text: &[u8], open: usize) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut depth = 0usize;
    let mut entry = String::new();
    for &byte in &text[open..] {
        match byte {
            b'{' => {
                depth += 1;
                if depth > 1 {
                    entry.push(byte as char);
                }
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                entry.push(byte as char);
            }
            b',' if depth == 1 => {
                out.extend(first_segment(&entry));
                entry.clear();
            }
            _ => {
                if depth >= 1 {
                    entry.push(byte as char);
                }
            }
        }
    }
    out.extend(first_segment(&entry));
    out
}

/// The module name an import entry names, if any: `inbox::InboxItem` yields
/// `inbox`, and `ids` yields `ids`. A glob (`events::*`) yields `events`.
fn first_segment(entry: &str) -> Option<String> {
    let entry = entry.trim();
    let name: String = entry.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if name.is_empty() { None } else { Some(name) }
}

/// Every module of the guarded crate named by a path in `text`.
///
/// Only the `agent_protocol::…` shape counts — reaching a module any other way
/// is not something this text guard can see, and a bare word is not a path (the
/// lesson `Command` taught: `std::process::Command` appears everywhere in this
/// workspace and says nothing about the protocol's `commands` module).
fn referenced_modules(text: &str) -> BTreeSet<String> {
    const PREFIX: &str = "agent_protocol";
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut at = 0usize;
    while let Some(found) = text[at..].find(PREFIX) {
        let mut cursor = at + found + PREFIX.len();
        at = cursor;
        let skip_ws = |from: usize| -> usize {
            let mut i = from;
            while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                i += 1;
            }
            i
        };
        cursor = skip_ws(cursor);
        if !text[cursor..].starts_with("::") {
            continue;
        }
        cursor = skip_ws(cursor + 2);
        match bytes.get(cursor) {
            Some(b'{') => out.extend(entries_of(bytes, cursor)),
            Some(_) => {
                let rest: String = text[cursor..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !rest.is_empty() {
                    out.insert(rest);
                }
            }
            None => {}
        }
    }
    out
}

#[test]
fn every_protocol_module_is_reached_from_outside_the_crate() {
    let root = workspace_root();
    let declared = module_names(
        &fs::read_to_string(root.join(LIB)).unwrap_or_else(|error| panic!("read {LIB}: {error}")),
    );

    // A parser that quietly found nothing would make this guard pass
    // vacuously, so the parsing itself is asserted before it is trusted.
    assert!(
        declared.len() >= 10,
        "{LIB} parsed as {} modules: {declared:?} — the guard would pass on an empty list",
        declared.len()
    );

    let used: BTreeSet<String> =
        outside_sources(&root).iter().flat_map(|(_, text)| referenced_modules(text)).collect();

    let unreached: Vec<&String> =
        declared.iter().filter(|module| !used.contains(*module)).collect();
    assert!(
        unreached.is_empty(),
        "modules of {CRATE} that nothing outside the crate reaches: {unreached:?} — this crate's \
         `pub` surface is what the rest of the workspace speaks, so a module nobody uses sends a \
         reader to the wrong place (that is how a whole invented request face — `Command` / \
         `CommandKind` — outlived its own irrelevance). Either wire it up, or take it out"
    );
}

// ---------------------------------------------------------------------------
// Probes: every module today is reached, so a guard that always answers "used"
// would pass and mean nothing. Feed the checker the shapes it has to tell apart,
// and pin the ones it cannot.
// ---------------------------------------------------------------------------

#[test]
fn a_path_reaches_the_module_it_names() {
    let used = referenced_modules("use agent_protocol::inbox::InboxItem;\n");
    assert!(used.contains("inbox"));
    // Not a module it was asked about.
    assert!(!used.contains("events"));
}

#[test]
fn a_braced_import_counts_across_lines() {
    let used = referenced_modules(
        "use agent_protocol::{\n    inbox::InboxItem,\n    ids::{SessionId, TurnId},\n};\n",
    );
    assert!(used.contains("inbox"));
    assert!(used.contains("ids"));
    assert!(!used.contains("TurnId"), "a nested entry's inner name is not a module");
}

#[test]
fn a_bare_word_is_not_a_path() {
    // The lesson YA-39 itself taught: `Command` was reported as unreferenced
    // only because this workspace is full of `std::process::Command`. A rule
    // that read bare words would have called the module used and kept it.
    let used =
        referenced_modules("use std::process::Command;\nlet mut c = Command::new(\"git\");\n");
    assert!(used.is_empty(), "a bare name must not count as a module reference: {used:?}");
}

#[test]
fn a_mention_in_a_comment_counts_and_that_is_the_blind_spot() {
    // Pinned rather than hidden: the rule is textual, so prose citing a module
    // path is indistinguishable from use of it. This one is live — the doc
    // comment at `agent-cli/src/lib.rs` cites `agent_protocol::ids::…` and is
    // counted. The guard errs toward silence, and a guard that is too easy to
    // satisfy is worth knowing about, not worth pretending about.
    let used = referenced_modules("/// (`agent_protocol::ids::SchemaVersion::CURRENT`)\n");
    assert!(used.contains("ids"));
}
