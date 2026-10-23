//! Boundary test for YA-41: every command the binary dispatches is one it
//! documents.
//!
//! The rule is about a specific kind of silence. A command can be perfectly
//! implemented and still be uncallable in practice, because the only place its
//! name exists is the dispatch table — and the only way to find it is to read
//! the source. `interrupt` was exactly that shape: the kernel had the whole
//! mechanism (`Inbox::interrupt`, `Scheduler::pending_interrupt`,
//! `Ending::Interrupted`), the scheduler's own test drove it end to end, and no
//! command could produce one. A user reading `agent` help learned nothing about
//! it; a user who guessed the word learned that guessing was how you had to do
//! this.
//!
//! The guard is a **text** guard, and says so. It reads two places in
//! `apps/agent/src/main.rs` — the `match args.command.as_str()` table and the
//! string `usage()` prints — and compares them by first word. That makes it
//! blind in two directions: a name the guard cannot see it treats as absent
//! (there is no name here built by a macro or a constant), and a name it does
//! see counts even when the surrounding prose is about something else (the
//! `plugin list` line documents `plugin` as far as this can tell). The probes at
//! the bottom pin those limits rather than hide them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The one file this guards, relative to the workspace root.
const APP: &str = "apps/agent/src/main.rs";

/// The line that opens the dispatch table. Matching on it means a rewrite that
/// moves the arms elsewhere fails here, loudly, rather than silently guarding
/// nothing.
const DISPATCH: &str = "match args.command.as_str() {";

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <workspace>/crates/agent-testkit.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn app_source() -> String {
    let path = workspace_root().join(APP);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The command names in the dispatch table.
///
/// Every arm before the catch-all is `"name" | "alias" => …`, so the names are
/// exactly the quoted strings up to the first line that is not one of those.
/// The catch-all's binding is not quoted, which is what ends the scan.
fn dispatch_names(source: &str) -> Vec<String> {
    let start =
        source.find(DISPATCH).unwrap_or_else(|| panic!("{APP} no longer contains `{DISPATCH}`"));
    let mut names = Vec::new();
    for line in source[start..].lines().skip(1) {
        let trimmed = line.trim();
        if trimmed.starts_with('}') || !trimmed.starts_with('"') {
            break;
        }
        names.extend(quoted(trimmed));
    }
    names
}

/// The double-quoted strings on one line, in order.
fn quoted(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    out
}

/// The usage message as it will actually read when printed.
///
/// It is one Rust string literal continued across source lines, so the source
/// carries `\n` as two characters and the continuation backslash strips the
/// newline and indentation that follow it. Undoing both here — rather than
/// pattern-matching the source — is what makes the rule about the **printed**
/// line, which is the line a person reads.
fn usage_lines(source: &str) -> Vec<String> {
    let start =
        source.find("fn usage()").unwrap_or_else(|| panic!("{APP} no longer has `fn usage()`"));
    let body = &source[start..];
    let end = body.find(");").unwrap_or(body.len());
    body[..end]
        .split("\\n")
        // Whitespace (the source indentation), then the continuation backslash,
        // then the real newline and the indentation behind it.
        .map(|chunk| chunk.trim().trim_start_matches('\\').trim().to_string())
        .collect()
}

/// What a usage line documents: its first word.
///
/// Not "the line mentions it". The block is a two-column table and the command
/// column is the first word, which is the only reading under which
/// `deny <id> [--only]` documents `deny` and not `id`, and under which the
/// `runtime` in some future description does not document a command named `run`.
fn documented_name(line: &str) -> Option<&str> {
    line.split_whitespace().next()
}

fn documented_names<'a, I: IntoIterator<Item = &'a str>>(lines: I) -> BTreeSet<String> {
    lines.into_iter().filter_map(documented_name).map(str::to_string).collect()
}

/// Dispatched but not documented, in dispatch order.
fn undocumented(dispatched: &[String], documented: &BTreeSet<String>) -> Vec<String> {
    dispatched.iter().filter(|name| !documented.contains(*name)).cloned().collect()
}

#[test]
fn every_dispatched_command_is_documented() {
    let source = app_source();
    let dispatched = dispatch_names(&source);

    // A parser that quietly found nothing would make this guard pass
    // vacuously, so the parsing itself is asserted before it is trusted.
    assert!(
        dispatched.len() >= 10,
        "{DISPATCH} in {APP} parsed as {} names: {dispatched:?} — the guard would pass on an empty \
         list",
        dispatched.len()
    );

    let documented = documented_names(usage_lines(&source).iter().map(String::as_str));
    let missing = undocumented(&dispatched, &documented);
    assert!(
        missing.is_empty(),
        "commands {APP} dispatches but `usage()` never prints: {missing:?} — a command whose name \
         exists only in the dispatch table is one a user can find only by reading the source. \
         Either document it, or take it out"
    );
}

// ---------------------------------------------------------------------------
// Probes: every command today is documented, so a checker that always answered
// "documented" would pass and mean nothing. Feed it the shapes it has to tell
// apart, and pin the ones it cannot.
// ---------------------------------------------------------------------------

#[test]
fn the_table_is_parsed_as_names_and_not_as_source() {
    // The property that makes the live test mean anything: aliases are separate
    // names, and the catch-all is not a command.
    let names = dispatch_names(&app_source());
    assert!(names.contains(&"run".to_string()), "{names:?}");
    assert!(names.contains(&"ask".to_string()), "an alias is a name a user can type: {names:?}");
    assert!(names.contains(&"interrupt".to_string()), "{names:?}");
    assert!(!names.contains(&"other".to_string()), "the catch-all is not a command: {names:?}");
    assert!(
        !names.iter().any(|name| name.contains(' ')),
        "a name was read out of the arm body rather than the pattern: {names:?}"
    );
}

#[test]
fn a_name_that_only_appears_as_an_argument_documents_nothing() {
    // `id` is not a command and must not be read as one, which is the whole
    // reason the rule is about the first word.
    let documented = documented_names(["deny <id> [--only]         Deny a pending request"]);
    assert!(documented.contains("deny"), "{documented:?}");
    assert!(!documented.contains("id"), "{documented:?}");
    assert!(!documented.contains("<id>"), "{documented:?}");
}

#[test]
fn a_name_inside_a_longer_word_documents_nothing() {
    // The false positive a `contains` check would produce, pinned so nobody
    // "simplifies" the rule into one: `run` is in `runtime`, and a guard that
    // counted it would keep passing after the `run` line was deleted.
    let documented = documented_names(["status                     Show session state"]);
    assert!(documented.contains("status"), "{documented:?}");
    assert!(!documented.contains("run"), "{documented:?}");
    // And the same statement about the prose itself, not just the table.
    let prose = "         export [--out <dir>]       Write the session as session.md";
    assert!(!documented_names([prose]).contains("session"));
}

#[test]
fn a_command_named_after_a_plugin_subcommand_would_read_as_undocumented() {
    // The live blind spot, pinned rather than hidden. The dispatch table has one
    // `plugin` arm; the usage block spells it out as three lines (`plugin list`,
    // `plugin disable <id>`, `plugin enable <id>`) because that is what a user
    // types. By the first-word rule all three document `plugin` — which is
    // right — and none of them documents a command whose name is `plugin list`.
    // This guard's scope is the top-level name, and a rule that read further
    // into the line would have to know the subcommands, which live in
    // `main.rs`'s argument handling rather than in the table.
    let documented = documented_names(["plugin list                What the configuration says"]);
    assert!(documented.contains("plugin"), "{documented:?}");
    assert!(!documented.contains("plugin list"), "{documented:?}");
}

#[test]
fn an_empty_page_documents_nothing() {
    // The refusal test: the rule has to be able to say no at all.
    let documented = documented_names(["", "   ", "Usage: agent <command> [options]"]);
    assert!(!documented.contains("run"), "{documented:?}");
    assert!(
        !undocumented(&["steer".to_string()], &documented).is_empty(),
        "a command absent from every line must be reported"
    );
}
