//! Architecture boundary tests: encode the workspace crate set and the dependency
//! graph, both runtime and test-only.
//!
//! These read the workspace and per-crate `Cargo.toml` files and assert that:
//!   1. the workspace member set matches exactly the known crates + app,
//!   2. each crate's runtime (`[dependencies]`) `agent-*` deps match the spec,
//!   3. each crate's test-only (`[dev-dependencies]`) `agent-*` deps match the
//!      spec, and
//!   4. every `agent-*` dependency is a workspace path, not a registry crate
//!      that merely shares the prefix.
//!
//! The manifests are parsed as TOML, not scanned as lines. A line scanner sees
//! only the shapes it was written against: a dependency written as a dotted
//! table (`[dependencies.agent-ports]`) or renamed (`kernel = { package =
//! "agent-kernel" }`) is invisible to it, and so is a workspace member written
//! without a slash. A guard with blind spots reports on the manifests it can
//! read rather than on the graph that is actually built — the parser below has
//! no such blind spots, and the probes at the bottom of this file pin that.

use std::collections::BTreeSet;
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

fn read_manifest(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The workspace members, exactly as the root manifest lists them — every entry
/// kept, whether or not it happens to contain a slash.
fn workspace_members(root_manifest: &Value) -> BTreeSet<String> {
    root_manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(Value::as_array)
        .expect("`[workspace] members` is an array")
        .iter()
        .map(|member| member.as_str().expect("a workspace member is a path string").to_string())
        .collect()
}

/// The `agent-*` crates a manifest's `section` pulls in, by *package* name.
///
/// The key is the name the code says; `package` is the crate actually depended
/// on, so a rename is followed to the crate it renames. Any table shape works,
/// including the dotted form.
fn agent_deps(manifest: &Value, section: &str) -> BTreeSet<String> {
    let Some(table) = manifest.get(section) else {
        return BTreeSet::new();
    };
    let table = table.as_table().unwrap_or_else(|| panic!("`[{section}]` is a table"));
    table
        .iter()
        .filter_map(|(key, spec)| {
            let package = spec.get("package").and_then(Value::as_str).unwrap_or(key);
            package.starts_with("agent-").then(|| package.to_string())
        })
        .collect()
}

/// The `agent-*` deps in `section` that are *not* workspace paths — a name that
/// resolves to a registry crate instead of the crate of that name in this
/// repository. The exact-set assertions above cannot catch this: the name is
/// right, only the source is wrong.
fn registry_agent_deps(manifest: &Value, section: &str) -> BTreeSet<String> {
    let Some(table) = manifest.get(section).and_then(Value::as_table) else {
        return BTreeSet::new();
    };
    table
        .iter()
        .filter(|(key, spec)| {
            let package = spec.get("package").and_then(Value::as_str).unwrap_or(key.as_str());
            package.starts_with("agent-")
        })
        .filter(|(_, spec)| spec.get("path").and_then(Value::as_str).is_none())
        .map(|(key, spec)| {
            spec.get("package").and_then(Value::as_str).unwrap_or(key.as_str()).to_string()
        })
        .collect()
}

const SECTIONS: &[&str] = &["dependencies", "dev-dependencies"];

#[test]
fn workspace_members_match_spec() {
    let members = workspace_members(&read_manifest(&workspace_root().join("Cargo.toml")));
    let expected: BTreeSet<String> = [
        "crates/agent-protocol",
        "crates/agent-ports",
        "crates/agent-config",
        "crates/agent-store-jsonl",
        "crates/agent-store-artifact",
        "crates/agent-store-secret",
        "crates/agent-projection",
        "crates/agent-context",
        "crates/agent-decision",
        "crates/agent-decision-laya",
        "crates/agent-kernel",
        "crates/agent-loop",
        "crates/agent-tools-fs",
        "crates/agent-tools-git",
        "crates/agent-tools-mcp",
        "crates/agent-subagent-stdio",
        "crates/agent-sandbox-win",
        "crates/agent-model-openai",
        "crates/agent-index-sqlite",
        "crates/agent-testkit",
        "crates/agent-cli",
        "crates/agent-web",
        "apps/agent",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(members, expected, "workspace member set drifted from spec");
}

#[test]
fn dependency_direction_matches_spec() {
    // crate dir -> allowed runtime (non-dev) agent-* dependencies.
    let matrix: &[(&str, &[&str])] = &[
        ("crates/agent-protocol", &[]),
        ("crates/agent-ports", &["agent-protocol"]),
        // Configuration is data about a run, not a participant in it: it
        // resolves into protocol vocabulary — a capability set, a route, a
        // budget — and knows nothing about the kernel that enforces them.
        ("crates/agent-config", &["agent-protocol"]),
        ("crates/agent-store-jsonl", &["agent-protocol", "agent-ports"]),
        ("crates/agent-store-artifact", &["agent-protocol", "agent-ports"]),
        ("crates/agent-store-secret", &["agent-protocol", "agent-ports"]),
        ("crates/agent-projection", &["agent-protocol", "agent-ports"]),
        // The only crate that may depend on the projection: it implements the
        // context port for the loop, which itself only sees `agent-ports`.
        ("crates/agent-context", &["agent-protocol", "agent-ports", "agent-projection"]),
        // The decision broker implements a port and owns no vendor: it may see
        // the protocol and the seams, and nothing above them.
        ("crates/agent-decision", &["agent-protocol", "agent-ports"]),
        // A backend, not a broker: it implements the fast-decision port and
        // knows one vendor's wire, and it deliberately cannot see the broker
        // that decides whether to trust it, or the loop that records it.
        ("crates/agent-decision-laya", &["agent-protocol", "agent-ports"]),
        ("crates/agent-kernel", &["agent-protocol", "agent-ports"]),
        ("crates/agent-loop", &["agent-protocol", "agent-ports"]),
        ("crates/agent-tools-fs", &["agent-protocol", "agent-ports"]),
        // Like the fs tools: a tool adapter that sees the protocol and the
        // execution seam, and nothing above either.
        ("crates/agent-tools-git", &["agent-protocol", "agent-ports"]),
        // The one adapter that talks to something outside this process. It
        // holds the MCP SDK — the only crate that names it — and the crates
        // above it hold only the `ToolProvider` it implements.
        ("crates/agent-tools-mcp", &["agent-protocol", "agent-ports"]),
        // Like the MCP adapter: something outside this process, reached through
        // a port. It knows one wire and one process API and nothing above
        // either — in particular not the kernel that decides what a delegation
        // means, nor the session a child's history lives in.
        ("crates/agent-subagent-stdio", &["agent-protocol", "agent-ports"]),
        // A backend on the launcher seam and nothing else: it implements
        // `ProcessLauncher`, sees no tool surface, no policy and no kernel.
        ("crates/agent-sandbox-win", &["agent-ports"]),
        ("crates/agent-model-openai", &["agent-protocol", "agent-ports"]),
        // A disposable read model over the log: it implements one port and must
        // not see the kernel that owns the log, nor the projection that is the
        // model's history. Its only truth is what it reads through `EventStore`.
        ("crates/agent-index-sqlite", &["agent-protocol", "agent-ports"]),
        ("crates/agent-testkit", &["agent-protocol", "agent-ports"]),
        ("crates/agent-cli", &["agent-protocol", "agent-ports", "agent-kernel"]),
        // The front door: a transport and the read model over the log. It may
        // fold projections (the transcript is a view, not a second source) but
        // never names the kernel — writes go through the command layer.
        ("crates/agent-web", &["agent-protocol", "agent-ports", "agent-projection"]),
        (
            "apps/agent",
            &[
                "agent-protocol",
                "agent-ports",
                "agent-config",
                "agent-kernel",
                "agent-projection",
                "agent-cli",
                "agent-loop",
                "agent-store-jsonl",
                "agent-store-artifact",
                "agent-tools-fs",
                "agent-tools-git",
                "agent-tools-mcp",
                "agent-subagent-stdio",
                "agent-sandbox-win",
                "agent-model-openai",
                "agent-store-secret",
                "agent-context",
                "agent-decision",
                "agent-decision-laya",
                "agent-index-sqlite",
                "agent-web",
            ],
        ),
    ];

    let root = workspace_root();
    for (crate_dir, allowed) in matrix {
        let manifest = read_manifest(&root.join(crate_dir).join("Cargo.toml"));
        let actual = agent_deps(&manifest, "dependencies");
        let expected: BTreeSet<String> = allowed.iter().map(|s| s.to_string()).collect();
        assert_eq!(actual, expected, "dependency direction mismatch for {crate_dir}");
    }
}

/// crate dir -> allowed test-only agent-* dependencies.
///
/// The runtime matrix says what a crate may *be*; this one says what a crate may
/// be *tested through*. A dev-dependency is allowed to climb: a crate can be
/// verified with the pieces that drive it, which is the only way to test an
/// adapter against a real caller rather than against a fake of one. What is not
/// allowed is climbing by accident — an unnoticed dev-dep makes one layer's
/// tests fail when another layer changes, so the tests stop being about the
/// layer they are in. Every entry below is here on purpose, and the list is
/// closed: a new one has to be argued for, not discovered. It is also the list
/// of manifests the source check below walks, so a crate is added once.
const DEV_MATRIX: &[(&str, &[&str])] = &[
    ("crates/agent-protocol", &[]),
    ("crates/agent-ports", &[]),
    ("crates/agent-config", &[]),
    ("crates/agent-store-jsonl", &[]),
    ("crates/agent-store-artifact", &[]),
    ("crates/agent-store-secret", &[]),
    // Its m1 acceptance test replays a log the real store wrote: the point is
    // that the projection reads a log it did not author.
    ("crates/agent-projection", &["agent-testkit", "agent-store-jsonl"]),
    // The fork / model-context / resume acceptance tests drive real sessions
    // through the kernel and the loop — the projection's context is only
    // meaningful against the events those actually emit — and the fs tools
    // provide the workspace those sessions run in.
    (
        "crates/agent-context",
        &["agent-testkit", "agent-loop", "agent-kernel", "agent-store-jsonl", "agent-tools-fs"],
    ),
    // The broker's tests need a decision backend and a run to broker.
    ("crates/agent-decision", &["agent-testkit"]),
    ("crates/agent-decision-laya", &[]),
    // Approval, fork, resume and scheduler are only meaningful when a real loop
    // is at the other end of the ports they call.
    ("crates/agent-kernel", &["agent-testkit", "agent-loop"]),
    ("crates/agent-loop", &["agent-testkit"]),
    ("crates/agent-tools-fs", &[]),
    // Fixtures are built with libgit2 in process, the way the tools read them;
    // the integration test shells out to the real `git` for an independent
    // answer, which needs no crate from this workspace.
    ("crates/agent-tools-git", &[]),
    // The adapter's tests need a store to offload a non-text content block to,
    // and the in-memory one lives in the testkit rather than in a second
    // hand-rolled double here.
    ("crates/agent-tools-mcp", &["agent-testkit"]),
    ("crates/agent-subagent-stdio", &[]),
    // The backend's own tests touch the filesystem ACLs and the device DACL;
    // no other crate in the workspace is needed to drive it.
    ("crates/agent-sandbox-win", &[]),
    // The live-endpoint test resolves a key through the real secret store.
    ("crates/agent-model-openai", &["agent-testkit", "agent-store-secret"]),
    // The adapter is tested against a real log it did not author: the index is
    // only meaningful relative to the store's own records.
    ("crates/agent-index-sqlite", &["agent-testkit", "agent-store-jsonl"]),
    ("crates/agent-testkit", &[]),
    // End-to-end CLI tests run through the real loop.
    ("crates/agent-cli", &["agent-testkit", "agent-loop"]),
    // The read endpoints are tested against a real log on disk: listing sessions
    // is a filesystem question, and a fake store cannot answer it.
    ("crates/agent-web", &["agent-store-jsonl"]),
    ("apps/agent", &["agent-testkit"]),
];

#[test]
fn dev_dependency_direction_matches_spec() {
    let root = workspace_root();
    for (crate_dir, allowed) in DEV_MATRIX {
        let manifest = read_manifest(&root.join(crate_dir).join("Cargo.toml"));
        let actual = agent_deps(&manifest, "dev-dependencies");
        let expected: BTreeSet<String> = allowed.iter().map(|s| s.to_string()).collect();
        assert_eq!(actual, expected, "dev-dependency direction mismatch for {crate_dir}");
    }
}

#[test]
fn every_agent_dependency_is_a_workspace_path() {
    // `agent-ports = { version = "9" }` names a crate in this repository and
    // pulls in a stranger with the same name. The matrices above cannot see
    // that: the name is right, only the source is wrong.
    let root = workspace_root();
    for (crate_dir, _) in DEV_MATRIX {
        let manifest = read_manifest(&root.join(crate_dir).join("Cargo.toml"));
        for section in SECTIONS {
            let strangers = registry_agent_deps(&manifest, section);
            assert!(
                strangers.is_empty(),
                "{crate_dir} `[{section}]` takes {strangers:?} from a registry, not from the \
                 workspace"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Probes: the manifests in this repository are all written the ordinary way, so
// the shapes the old line scanner could not read do not occur in them. Feed the
// parser those shapes directly, and assert it sees them — otherwise "the parser
// is stronger now" is a claim about code nobody exercises.
// ---------------------------------------------------------------------------

// REVERSE-PROBE: the line scanner this guard replaced, verbatim.
fn legacy_agent_deps(text: &str, section: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let mut inside = false;
    let marker = format!("[{section}]");
    for line in text.lines() {
        let t = line.trim();
        if t == marker {
            inside = true;
        } else if t.starts_with('[') {
            inside = false;
        } else if inside && let Some(rest) = t.strip_prefix("agent-") {
            let name: String =
                rest.chars().take_while(|c| !c.is_whitespace() && *c != '=').collect();
            if !name.is_empty() {
                deps.insert(format!("agent-{name}"));
            }
        }
    }
    deps
}

fn legacy_members(text: &str) -> BTreeSet<String> {
    let mut members = BTreeSet::new();
    let mut in_members = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("members = [") {
            in_members = true;
            continue;
        }
        if in_members {
            if t.starts_with(']') {
                break;
            }
            let path = t.trim().trim_matches(',').trim_matches('"');
            if path.contains('/') {
                members.insert(path.to_string());
            }
        }
    }
    members
}

#[test]
fn a_dependency_written_as_a_dotted_table_is_seen() {
    let text = r#"
        [dependencies]
        serde = "1"
        [dependencies.agent-ports]
        path = "../agent-ports"
        "#;
    let manifest: Value = toml::from_str(text).expect("valid toml");

    assert_eq!(
        agent_deps(&manifest, "dependencies"),
        BTreeSet::from(["agent-ports".to_string()]),
        "a dotted table is the same dependency as an inline one"
    );
    assert!(
        legacy_agent_deps(text, "dependencies").is_empty(),
        "the old scanner did not see it either — this probe is the difference"
    );
}

#[test]
fn a_renamed_dependency_is_followed_to_its_package() {
    let text = r#"
        [dependencies]
        kernel = { package = "agent-kernel", path = "../agent-kernel" }
        "#;
    let manifest: Value = toml::from_str(text).expect("valid toml");

    assert_eq!(
        agent_deps(&manifest, "dependencies"),
        BTreeSet::from(["agent-kernel".to_string()]),
        "the crate depended on is the package, not the name the code calls it"
    );
    assert!(
        legacy_agent_deps(text, "dependencies").is_empty(),
        "the old scanner did not see it either — this probe is the difference"
    );
}

#[test]
fn a_workspace_member_without_a_slash_is_kept() {
    let text = r#"
        [workspace]
        members = ["crates/agent-ports", "agent-solo"]
        "#;
    let manifest: Value = toml::from_str(text).expect("valid toml");

    assert_eq!(
        workspace_members(&manifest),
        BTreeSet::from(["crates/agent-ports".to_string(), "agent-solo".to_string()]),
        "a member is whatever the manifest says it is"
    );
    assert!(
        legacy_members(text).is_empty(),
        "the old scanner dropped every member here — this probe is the difference"
    );
}

#[test]
fn a_registry_crate_sharing_the_prefix_is_caught() {
    let manifest: Value = toml::from_str(
        r#"
        [dependencies]
        agent-ports = { path = "../agent-ports" }
        [dev-dependencies]
        agent-fake = { version = "1" }
        "#,
    )
    .expect("valid toml");

    assert_eq!(
        registry_agent_deps(&manifest, "dev-dependencies"),
        BTreeSet::from(["agent-fake".to_string()]),
        "an `agent-*` dep with no `path` is a stranger"
    );
}
