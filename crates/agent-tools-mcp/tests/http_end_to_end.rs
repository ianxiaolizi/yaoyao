//! End to end over **Streamable HTTP**: a real server process on a real socket,
//! spoken to by the real `rmcp` client.
//!
//! The sibling suite does this for stdio. What only this file can check is the
//! half of the adapter that has no process to inherit: the HTTP handshake, the
//! session header, the server-initiated event stream, and what happens when a
//! session is gone. Four things the ledger recorded as unverified —
//!
//!   * the Streamable HTTP transport branch,
//!   * `tools/list_changed` arriving over HTTP,
//!   * the shape of the configured `headers` on an actual request,
//!   * recovery after the session or the connection is gone,
//!
//! — and each is checked against a fixture that encodes the server's side of the
//! wire independently of the client being tested.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_ports::artifact_store::ArtifactStore;
use agent_ports::tool_provider::{ToolInvocation, ToolProvider};
use agent_protocol::permissions::Capability;
use agent_testkit::InMemoryArtifactStore;
use agent_tools_mcp::{
    Backoff, HttpSpec, McpConnection, McpConnector, McpToolProvider, RmcpConnector, TransportSpec,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// The header the fixture requires on every request, and the value it wants.
/// The fixture's default, spelled out here so a change to one is not silently
/// absorbed by the other.
const REQUIRED: (&str, &str) = ("x-yaoyao-test", "present");

/// The `fake-mcp-http-server` example Cargo built for this package's tests.
///
/// It is an example rather than a `src/bin/` binary so that `cargo build
/// --workspace` does not put a test double where product binaries go; `cargo
/// test` builds examples, and that is how the gates run. Cargo exports no
/// `CARGO_BIN_EXE_*` for an example, so the path is worked out from where this
/// test binary sits, and the example is built on the spot when a narrower
/// `--test` selection did not build it.
fn server_path() -> String {
    let deps = std::env::current_exe().expect("the test binary has a path");
    let profile = deps.parent().and_then(Path::parent).expect("<target>/<profile>");
    let examples = profile.join("examples");
    let file = if cfg!(windows) { "fake-mcp-http-server.exe" } else { "fake-mcp-http-server" };
    let path = examples.join(file);
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        // Build into the same target directory this test binary lives under: a
        // coverage build relocates it, and a default build would not be found here.
        let mut build = std::process::Command::new(cargo);
        build
            .args(["build", "-p", "agent-tools-mcp", "--example", "fake-mcp-http-server"])
            .arg("--target-dir")
            .arg(profile.parent().expect("the target directory"))
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        if profile.ends_with("release") {
            build.arg("--release");
        }
        let built = build.status().expect("cargo is on the path");
        assert!(built.success(), "the fake-mcp-http-server example could not be built");
        assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    }
    path.to_string_lossy().into_owned()
}

/// A running fixture, killed when this is dropped.
struct Fixture {
    child: Child,
    url: String,
}

impl Fixture {
    async fn start() -> Self {
        let mut child = Command::new(server_path())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the fixture starts");

        let stdout = child.stdout.take().expect("stdout was piped");
        let mut lines = BufReader::new(stdout).lines();
        let announced = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("the fixture announces its port within ten seconds")
            .expect("the fixture's stdout is readable")
            .expect("the fixture printed a line");
        let address = announced.strip_prefix("listening ").unwrap_or_else(|| panic!("{announced}"));

        // The read end of stdout is dropped here. The fixture writes exactly one
        // line and never again, so nothing above is waiting on it.
        Self { child, url: format!("http://{address}{}", "/mcp") }
    }

    fn connector(&self, headers: Vec<(String, String)>) -> Arc<RmcpConnector> {
        // These tests exercise the HTTP transport, not offloading, so any store
        // will do — but the connector is handed the run's store rather than
        // building one, which is the shape a real assembly uses too.
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(InMemoryArtifactStore::default());
        RmcpConnector::new(
            "httpfake",
            TransportSpec::Http(HttpSpec { url: self.url.clone(), headers }),
            artifacts,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn configured_headers() -> Vec<(String, String)> {
    vec![(REQUIRED.0.to_string(), REQUIRED.1.to_string())]
}

fn provider(connector: Arc<dyn McpConnector>) -> Arc<McpToolProvider> {
    Arc::new(McpToolProvider::new(
        "httpfake",
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

/// What the server says it has seen, as an object.
async fn stats(connection: &Arc<dyn McpConnection>) -> Value {
    let result = connection.call_tool("stats", json!({})).await.expect("the server answers stats");
    serde_json::from_str(&result.content)
        .unwrap_or_else(|error| panic!("stats was not JSON ({error}): {}", result.content))
}

fn counter(stats: &Value, name: &str) -> u64 {
    stats.get(name).and_then(Value::as_u64).unwrap_or_else(|| panic!("{name} in {stats}"))
}

/// Wait until a counter reaches `at_least`, or say what it was instead.
async fn wait_for_counter(connection: &Arc<dyn McpConnection>, name: &str, at_least: u64) -> Value {
    let mut last = Value::Null;
    for _ in 0..400 {
        last = stats(connection).await;
        if counter(&last, name) >= at_least {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the server never counted {at_least} {name}; it saw {last}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http_server_is_reached_and_its_tools_run_through_the_client() {
    let fixture = Fixture::start().await;
    let connection = fixture
        .connector(configured_headers())
        .connect()
        .await
        .expect("the fixture answers the handshake over HTTP");

    let tools = connection.list_tools().await.expect("the tool list arrives over HTTP");
    assert!(tools.iter().any(|tool| tool.name == "echo"), "{tools:?}");

    let result = connection
        .call_tool("echo", json!({ "text": "hello over the wire" }))
        .await
        .expect("the call reaches the server");
    assert_eq!(result.content, "hello over the wire");

    // The server's own accounting, read back through a tool. Both halves are
    // claims about the client that no assertion on this side could make:
    // `initialize` happened exactly once, and the session it named was then
    // used to open the server-initiated stream, which is the only way a
    // `list_changed` can ever reach this client.
    let observed = wait_for_counter(&connection, "gets", 1).await;
    assert_eq!(counter(&observed, "inits"), 1, "{observed}");
    let accept = observed["accept"].as_str().unwrap_or_default();
    assert!(
        accept.contains("text/event-stream") && accept.contains("application/json"),
        "the client asked for neither media type: {accept:?}"
    );

    // ...and it is held open rather than churning. A stream the client keeps
    // failing to read is invisible from the tool surface — it shows up only as
    // a `list_changed` that goes missing — so it is checked here, with a wait
    // longer than the client's own one-second reconnect backoff.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let settled = stats(&connection).await;
    assert_eq!(counter(&settled, "gets"), 1, "the stream was re-established: {settled}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_header_has_to_actually_arrive() {
    let fixture = Fixture::start().await;

    // Nothing configured: the server refuses, and says which header it wanted.
    let absent = fixture
        .connector(Vec::new())
        .connect()
        .await
        .err()
        .expect("a server that requires a header refuses a client that sends none")
        .to_string();
    assert!(absent.contains("403"), "{absent}");
    assert!(absent.contains(REQUIRED.0), "{absent}");

    // Configured, and wrong. A header that is sent but misspelled is the same
    // failure as one that is not sent, which is the point: this is a check on
    // the wire, not on the configuration having been read.
    let wrong = fixture
        .connector(vec![(REQUIRED.0.to_string(), "not-the-value".to_string())])
        .connect()
        .await
        .err()
        .expect("a wrong header value is refused")
        .to_string();
    assert!(wrong.contains("403"), "{wrong}");

    // And the same fixture, with the header it asked for, is reachable — so the
    // two refusals above are the header and not something else about the server.
    let connection =
        fixture.connector(configured_headers()).connect().await.expect("with the header, it works");
    let observed = stats(&connection).await;
    assert_eq!(observed["header"].as_str(), Some(REQUIRED.1), "{observed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_list_changed_notification_over_http_grows_the_surface() {
    let fixture = Fixture::start().await;
    let connector = fixture.connector(configured_headers());
    let changed = connector.tool_list_changed();

    let provider = provider(connector);
    provider.start().await.expect("the server lists its tools");
    provider.spawn_resync_listener(changed);
    assert!(!names_of(&provider).iter().any(|name| name == "mcp__httpfake__grown"));

    provider
        .invoke(&ToolInvocation::proposed("mcp__httpfake__grow", json!({})))
        .await
        .expect("the server grew");

    // The notification has exactly one route to this client: the POST that
    // called `grow` is already answered, so it can only have come in on the
    // event stream the client opened after the handshake.
    let mut grown = false;
    for _ in 0..400 {
        if names_of(&provider).iter().any(|name| name == "mcp__httpfake__grown") {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_the_server_has_forgotten_is_re_established() {
    let fixture = Fixture::start().await;
    let provider = provider(fixture.connector(configured_headers()));
    provider.start().await.expect("the first session is established");
    assert_eq!(names_of(&provider).len(), 5, "{:?}", names_of(&provider));

    // The server drops the session. Every later request that names it is a 404,
    // which is how a server reports expiry.
    provider
        .invoke(&ToolInvocation::proposed("mcp__httpfake__expire", json!({})))
        .await
        .expect("the server forgot the session");

    // The surface is not what recovers first: the client has to notice, and the
    // provider's own re-sync is what has to survive it.
    provider.resync().await.expect("the tool set comes back after the session expires");

    let connection = fixture.connector(configured_headers()).connect().await.unwrap();
    let observed = wait_for_counter(&connection, "inits", 2).await;
    assert!(counter(&observed, "inits") >= 2, "the client never re-initialized: {observed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_event_stream_comes_back_and_still_delivers() {
    let fixture = Fixture::start().await;
    let connector = fixture.connector(configured_headers());
    let changed = connector.tool_list_changed();
    let connection = connector.connect().await.expect("the handshake");
    wait_for_counter(&connection, "gets", 1).await;

    // The server drops the stream where it stands, the way a proxy or a restart
    // would.
    connection.call_tool("disconnect", json!({})).await.expect("the server dropped the stream");

    // The client re-opens it on its own, without being asked and without
    // rebuilding the session — `inits` is the assertion that this is a stream
    // that came back and not a client that gave up and started over.
    let reconnected = wait_for_counter(&connection, "gets", 2).await;
    assert_eq!(
        counter(&reconnected, "inits"),
        1,
        "it re-handshook, not reconnected: {reconnected}"
    );

    // The notification sent from here on can only reach the client over the new
    // stream. If the old one had been left closed, this would never arrive.
    let delivered = Arc::new(AtomicUsize::new(0));
    {
        let delivered = Arc::clone(&delivered);
        tokio::spawn(async move {
            changed.notified().await;
            delivered.store(1, Ordering::SeqCst);
        });
    }
    connection.call_tool("grow", json!({})).await.expect("the server grew");

    for _ in 0..400 {
        if delivered.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        delivered.load(Ordering::SeqCst),
        1,
        "no notification arrived on the re-established stream"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_session_tells_the_server() {
    let fixture = Fixture::start().await;

    let first = fixture.connector(configured_headers()).connect().await.expect("the first session");
    first.list_tools().await.expect("its tools");
    drop(first);

    // A second session reads the server's counters, which is the only place the
    // first session's DELETE is observable from this side of the wire.
    let second = fixture.connector(configured_headers()).connect().await.expect("the second");
    let observed = wait_for_counter(&second, "deletes", 1).await;
    assert!(counter(&observed, "sessions") <= 1, "the closed session was left behind: {observed}");
}
