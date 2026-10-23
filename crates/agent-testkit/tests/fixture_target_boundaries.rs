//! Boundary test for YA-37: a test double is not a product binary.
//!
//! The five servers that stand in for MCP, Laya, OpenAI and a subagent live
//! under their package's `examples/`, not `src/bin/`. The difference is not
//! style: Cargo builds `examples/` for `cargo test` and does **not** build them
//! for `cargo build --workspace`, so a file under `src/bin/` is auto-discovered
//! as a binary target and lands in `target/debug/` beside the product binaries —
//! which is where all five of them used to be.
//!
//! The guard is a **text** guard over paths, and says so. It reads `src/bin` by
//! path and knows a double by its `fake-` name, so a sixth double placed under a
//! different directory and never listed below is not caught. The probes at the
//! bottom pin that limit rather than hide it.

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
fn workspace_members(manifest: &Value) -> Vec<String> {
    manifest["workspace"]["members"]
        .as_array()
        .expect("the root manifest declares `workspace.members`")
        .iter()
        .map(|member| member.as_str().expect("a member is a string").to_string())
        .collect()
}

/// Every fixture server this workspace ships, as `<member>/<name>`.
///
/// Written out rather than discovered: what the guard holds the tree to is that
/// these five are *examples*, so the list has to be the thing the guard knows.
const FIXTURES: &[(&str, &str)] = &[
    ("crates/agent-decision-laya", "fake-laya-server"),
    ("crates/agent-model-openai", "fake-openai-server"),
    ("crates/agent-subagent-stdio", "fake-subagent"),
    ("crates/agent-tools-mcp", "fake-mcp-http-server"),
    ("crates/agent-tools-mcp", "fake-mcp-server"),
];

/// A file name that reads as one of these doubles.
fn is_double_file(name: &str) -> bool {
    name.starts_with("fake-") && name.ends_with(".rs")
}

#[test]
fn no_test_double_sits_where_cargo_builds_a_product_binary() {
    let root = workspace_root();
    let manifest: Value = toml::from_str(
        &fs::read_to_string(root.join("Cargo.toml")).expect("read the root manifest"),
    )
    .expect("parse the root manifest");

    let mut offenders = Vec::new();
    for member in workspace_members(&manifest) {
        let bin = root.join(&member).join("src").join("bin");
        let Ok(entries) = fs::read_dir(&bin) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_double_file(&name) {
                offenders.push(format!("{member}/src/bin/{name}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a test double is an auto-discovered binary target again, so `cargo build --workspace` \
         builds it as a product: {offenders:?} — move it to `examples/` and have the test locate \
         it there (the locators are in the adapter's own `tests/` and in `apps/agent/tests`)"
    );
}

#[test]
fn every_test_double_is_an_examples_target() {
    let root = workspace_root();
    let missing: Vec<String> = FIXTURES
        .iter()
        .filter(|(member, name)| {
            !root.join(member).join("examples").join(format!("{name}.rs")).is_file()
        })
        .map(|(member, name)| format!("{member}/examples/{name}.rs"))
        .collect();

    assert!(
        missing.is_empty(),
        "fixtures that are no longer `examples/` targets: {missing:?} — `cargo test` builds \
         examples, so a fixture moved anywhere else is built by neither gate"
    );
}

// ---------------------------------------------------------------------------
// Probes: the five doubles are where they should be, so a guard that always
// answers "fine" would pass and mean nothing. Feed the checker the shapes it has
// to tell apart, and pin the ones it cannot.
// ---------------------------------------------------------------------------

#[test]
fn a_double_name_is_recognised_and_a_neighbours_is_not() {
    assert!(is_double_file("fake-mcp-server.rs"));
    assert!(is_double_file("fake-laya-server.rs"));
    // Blind spot, pinned rather than hidden: a double under another name is not
    // this guard's business — it reads the name, not the file.
    assert!(!is_double_file("helper.rs"));
    // A backup left next to a double is not source Cargo builds.
    assert!(!is_double_file("fake-mcp-server.rs.bak"));
}
