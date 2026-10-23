//! End-to-end: a real MCP server process, spoken to by the real `rmcp` client.
//!
//! Everything here is the production path — `RmcpConnector`, a real child
//! process, newline-delimited JSON-RPC on its stdio, `rmcp`'s `initialize`
//! handshake — with only the server itself standing in for a vendor. The unit
//! tests next to the code cover the mapping rules against a fake connection;
//! this is where those rules are checked against the wire.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_ports::artifact_store::ArtifactStore;
use agent_ports::tool_provider::{ToolInvocation, ToolProvider, ToolSideEffect};
use agent_protocol::permissions::Capability;
use agent_protocol::tool::ToolSource;
use agent_testkit::InMemoryArtifactStore;
use agent_tools_mcp::{
    Backoff, McpConnector, McpToolProvider, RmcpConnector, StdioSpec, TransportSpec,
};
use serde_json::json;

/// The `fake-mcp-server` example Cargo built for this package's tests.
///
/// It is an example rather than a `src/bin/` binary so that `cargo build
/// --workspace` does not put a test double where product binaries go; `cargo
/// test` builds examples, and that is how the gates run. Cargo exports no
/// `CARGO_BIN_EXE_*` for an example, so the path is worked out from where this
/// test binary sits (`<target>/<profile>/deps/`), and the example is built on
/// the spot when a narrower `--test` selection did not build it.
fn server_path() -> String {
    let deps = std::env::current_exe().expect("the test binary has a path");
    let profile = deps.parent().and_then(Path::parent).expect("<target>/<profile>");
    let examples = profile.join("examples");
    let path = examples.join(if cfg!(windows) { "fake-mcp-server.exe" } else { "fake-mcp-server" });
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        // Build into the same target directory this test binary lives under: a
        // coverage build relocates it, and a default build would not be found here.
        let mut build = std::process::Command::new(cargo);
        build
            .args(["build", "-p", "agent-tools-mcp", "--example", "fake-mcp-server"])
            .arg("--target-dir")
            .arg(profile.parent().expect("the target directory"))
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        if profile.ends_with("release") {
            build.arg("--release");
        }
        let built = build.status().expect("cargo is on the path");
        assert!(built.success(), "the fake-mcp-server example could not be built");
        assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    }
    path.to_string_lossy().into_owned()
}

/// A store for the connector to offload non-text result blocks to. These tests
/// exercise the transport, not offloading, so a fresh one per connector is all
/// they need.
fn artifacts() -> Arc<dyn ArtifactStore> {
    Arc::new(InMemoryArtifactStore::default())
}

fn connector() -> Arc<dyn McpConnector> {
    let spec = StdioSpec { command: server_path(), args: Vec::new(), env: Vec::new(), cwd: None };
    RmcpConnector::new("fake", TransportSpec::Stdio(spec), artifacts())
}

fn provider(connector: Arc<dyn McpConnector>) -> Arc<McpToolProvider> {
    Arc::new(McpToolProvider::new(
        "fake",
        vec![Capability::new(Capability::NETWORK_REQUEST)],
        connector,
        Some(5_000),
        Backoff::default(),
    ))
}

/// The names on offer, read through `definitions()` — the surface production
/// reads them through (YA-47).
fn names_of(provider: &McpToolProvider) -> Vec<String> {
    provider.definitions().into_iter().map(|definition| definition.name).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_stdio_server_is_discovered_and_its_tools_are_classified() {
    let provider = provider(connector());
    provider.start().await.expect("the fake server starts and lists its tools");

    let names = names_of(&provider);
    for expected in [
        "mcp__fake__echo",
        "mcp__fake__note_write",
        "mcp__fake__fetch",
        "mcp__fake__grow",
        "mcp__fake__weird_name_",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected} in {names:?}");
    }
    // The two names that sanitize to the same thing both survive, one of them
    // carrying a disambiguator.
    assert!(names.iter().any(|n| n == "mcp__fake__a_b"));
    assert!(names.iter().any(|n| n.starts_with("mcp__fake__a_b__")));

    // Classification came from the server's annotations, through the client.
    let definitions = provider.definitions();
    let side_effect =
        |name: &str| definitions.iter().find(|d| d.name == name).map(|d| d.side_effect).unwrap();
    assert_eq!(side_effect("mcp__fake__echo"), ToolSideEffect::Read);
    assert_eq!(side_effect("mcp__fake__note_write"), ToolSideEffect::Destructive);
    assert_eq!(side_effect("mcp__fake__fetch"), ToolSideEffect::Network);
    assert_eq!(side_effect("mcp__fake__grow"), ToolSideEffect::Write);

    // Provenance and the inherited capability set.
    let echo = definitions.iter().find(|d| d.name == "mcp__fake__echo").unwrap();
    assert_eq!(echo.source, ToolSource::Mcp);
    assert_eq!(echo.capability_requirements, vec![Capability::new(Capability::NETWORK_REQUEST)]);
    assert!(echo.idempotent, "echo said idempotentHint");

    // The server saw its own name and the arguments the caller gave.
    let result = provider
        .invoke(&ToolInvocation::proposed("mcp__fake__echo", json!({ "text": "hello wire" })))
        .await
        .unwrap();
    assert_eq!(result.content, "hello wire");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_notification_grows_the_tool_surface() {
    // The notify the server's `list_changed` fires is the one the pump listens
    // on: one handle, held by the connector and lent to the resync listener.
    let connector = RmcpConnector::new(
        "fake",
        TransportSpec::Stdio(StdioSpec {
            command: server_path(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }),
        artifacts(),
    );
    let changed = connector.tool_list_changed();
    let provider = provider(connector);
    provider.start().await.unwrap();
    provider.spawn_resync_listener(changed);
    assert!(!names_of(&provider).iter().any(|n| n == "mcp__fake__grown"));

    // Calling `grow` makes the server emit `notifications/tools/list_changed`.
    provider.invoke(&ToolInvocation::proposed("mcp__fake__grow", json!({}))).await.unwrap();

    let mut grown = false;
    for _ in 0..200 {
        if names_of(&provider).iter().any(|n| n == "mcp__fake__grown") {
            grown = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        grown,
        "the list_changed notification did not grow the surface: {:?}",
        names_of(&provider)
    );
    assert_eq!(provider.side_effect("mcp__fake__grown"), Some(ToolSideEffect::Write));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_tool_is_refused_without_asking_the_server() {
    let provider = provider(connector());
    provider.start().await.unwrap();

    let error =
        provider.invoke(&ToolInvocation::proposed("mcp__fake__nope", json!({}))).await.unwrap_err();
    assert!(matches!(error, agent_ports::tool_provider::ToolProviderError::UnknownTool { .. }));
}

/// A server that dies mid-run is recovered from by the provider itself.
///
/// `exit` ends the server process while the call is in flight, so the call
/// fails the way a real crash does — and over stdio there is no transport-level
/// re-initialization to paper over it (the HTTP client does re-init itself on a
/// session-expired 404, but a process that is gone cannot be talked to at all).
/// The *next* call has to bring the session back, and over stdio that means a
/// new process, which only a reconnect does. Nothing here calls `resync`: if the
/// provider had kept the dead connection, every later call would fail for the
/// life of the process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_after_the_server_dies_heals_the_next_one() {
    let provider = provider(connector());
    provider.start().await.expect("the fake server starts and lists its tools");
    assert!(
        names_of(&provider).iter().any(|n| n == "mcp__fake__exit"),
        "the fixture no longer offers the tool this test drives: {:?}",
        names_of(&provider)
    );

    // The server goes away while answering: the call fails and is not replayed.
    assert!(
        provider.invoke(&ToolInvocation::proposed("mcp__fake__exit", json!({}))).await.is_err(),
        "a call into a server that has just died is not an answer"
    );

    // The next call reconnects on its own — a new process, a new session.
    let result = provider
        .invoke(&ToolInvocation::proposed("mcp__fake__echo", json!({ "text": "back" })))
        .await
        .expect("the next call reconnects and runs");
    assert_eq!(result.content, "back");
}
