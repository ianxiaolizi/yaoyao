//! McpToolProvider — an MCP server's tools as one `ToolProvider`.
//!
//! The provider holds a *last-good* tool set rather than asking the server on
//! every `definitions()` call. Two reasons, and they point the same way: the
//! surface must not change shape underneath a frozen `ToolManifest` just because
//! a server is briefly down, and a model-facing schema that flickers is worse
//! than one that lags. `resync` is the only thing that replaces the set, and a
//! sync that fails leaves the previous one in place.
//!
//! Reads are retried; calls are not. `list_tools` is idempotent, so retrying it
//! through a reconnect is free. A tool call may have side effects and a failed
//! call is no evidence that nothing happened, so it runs exactly once.
//!
//! A failed call is also no evidence that the *session* is still alive, so it
//! drops the connection and the next call reconnects rather than reusing one
//! that may be gone. Without that, a server that dies mid-run stays unreachable
//! for the life of the process: the only other thing that ever clears a
//! connection is a `tools/list_changed` notification, which is exactly what a
//! dead server cannot send.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
};
use agent_protocol::permissions::Capability;
use tokio::sync::Notify;

use crate::connection::{McpConnection, McpConnector, McpError, McpTool};
use crate::naming;

/// How long a tool call may run when the configuration names no timeout.
///
/// This is the ceiling the transport used to hardcode. It lives here now
/// because the configured number does: a deadline a caller cannot set and a
/// deadline a caller can set have to be the same mechanism, or the one that is
/// not configurable wins by being closer to the call.
pub const DEFAULT_TOOL_CALL_TIMEOUT_MS: u64 = 120_000;

/// How reconnection is paced.
///
/// Bounded in both directions: backoff doubles per attempt up to `cap`, and the
/// whole thing stops after `attempts` so a server that is simply gone does not
/// keep a caller waiting forever.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    pub attempts: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { base: Duration::from_millis(100), cap: Duration::from_secs(5), attempts: 5 }
    }
}

impl Backoff {
    /// The wait before retry `attempt` (zero-based).
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = 2u64.saturating_pow(attempt);
        let millis = self.base.as_millis().saturating_mul(u128::from(factor));
        let millis = u64::try_from(millis).unwrap_or(u64::MAX);
        Duration::from_millis(millis).min(self.cap)
    }
}

#[derive(Default)]
struct State {
    /// The live session, if one is established.
    connection: Option<Arc<dyn McpConnection>>,
    /// Namespaced name -> tool. The last set that synced.
    tools: BTreeMap<String, McpTool>,
}

/// One MCP server's tools, as a `ToolProvider`.
pub struct McpToolProvider {
    server: String,
    requires: Vec<Capability>,
    connector: Arc<dyn McpConnector>,
    timeout_ms: Option<u64>,
    backoff: Backoff,
    state: RwLock<State>,
}

impl McpToolProvider {
    pub fn new(
        server: impl Into<String>,
        requires: Vec<Capability>,
        connector: Arc<dyn McpConnector>,
        timeout_ms: Option<u64>,
        backoff: Backoff,
    ) -> Self {
        Self {
            server: server.into(),
            requires,
            connector,
            timeout_ms,
            backoff,
            state: RwLock::new(State::default()),
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    /// The capabilities every tool under this server inherits.
    pub fn requires(&self) -> &[Capability] {
        &self.requires
    }

    /// Establish the session and load the tool set for the first time.
    pub async fn start(&self) -> Result<(), McpError> {
        self.resync().await
    }

    /// Reload the tool set, reconnecting if there is no live session.
    ///
    /// On success the connection and the fresh set are both installed. On
    /// failure neither changes: the previous tool set stays on the surface, and
    /// the previous connection — if any was dropped during the attempt — stays
    /// dropped so the next call tries again rather than using a dead one.
    ///
    /// Every failed attempt is reported on stderr, the same channel the
    /// background re-sync pump uses. Without it a server that was down for six
    /// seconds and one that was never configured produced the same observable:
    /// a tool set that is simply missing.
    pub async fn resync(&self) -> Result<(), McpError> {
        let mut last_error = None;
        for attempt in 0..=self.backoff.attempts {
            let connection = match self.current_connection() {
                Some(connection) => connection,
                None => match self.connector.connect().await {
                    Ok(connection) => {
                        self.set_connection(connection.clone());
                        connection
                    }
                    Err(error) => {
                        last_error = Some(error);
                        self.retry_or_stop(attempt, last_error.as_ref()).await;
                        continue;
                    }
                },
            };

            match connection.list_tools().await {
                Ok(tools) => {
                    // A naming collision is deterministic, so it is reported
                    // rather than retried: asking the same server again would
                    // produce the same two tools.
                    return self.store(connection, tools);
                }
                Err(error) => {
                    last_error = Some(error);
                    self.clear_connection();
                    self.retry_or_stop(attempt, last_error.as_ref()).await;
                }
            }
        }
        Err(last_error.expect("a loop that ran at least once records its failure"))
    }

    /// Report a failed attempt and wait before the next one — unless there is
    /// no next one.
    ///
    /// Sleeping after the final failure delays the caller by a backoff nobody
    /// is going to use. At the default settings that was 3.2s added to a
    /// startup that was already giving up.
    async fn retry_or_stop(&self, attempt: u32, error: Option<&McpError>) {
        let failure = error.map(ToString::to_string).unwrap_or_else(|| "unknown error".to_string());
        if attempt >= self.backoff.attempts {
            eprintln!(
                "mcp server '{}': attempt {}/{} failed ({failure}); no attempts left",
                self.server,
                attempt + 1,
                self.backoff.attempts + 1,
            );
            return;
        }
        let delay = self.backoff.delay(attempt);
        eprintln!(
            "mcp server '{}': attempt {}/{} failed ({failure}); retrying in {delay:?}",
            self.server,
            attempt + 1,
            self.backoff.attempts + 1,
        );
        tokio::time::sleep(delay).await;
    }

    /// Watch `changed` and re-sync whenever it fires — how `tools/list_changed`
    /// reaches the tool surface. The subscription lives on the connection; this
    /// is the pump that connects it to the last-good set.
    ///
    /// Spawned rather than awaited: the notification arrives on a task the
    /// runtime owns, and blocking a caller on it would be blocking on the wrong
    /// thing. A re-sync that fails keeps the pump running and its error goes to
    /// stderr rather than being dropped — a surface that has stopped tracking
    /// its server is news, and this pump is the only thing that knows.
    pub fn spawn_resync_listener(self: &Arc<Self>, changed: Arc<Notify>) {
        // Weak, not strong: the pump is a subscriber, not an owner. A strong
        // reference would keep the provider — and through it the child process
        // — alive after the surface that owned it is gone, which is exactly
        // the orphan this listener used to create on every shutdown that
        // dropped the registry without shutting the runtime down (AA-07).
        // The notification channel is held separately, so waiting on it does
        // not require the provider to exist.
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                changed.notified().await;
                let Some(this) = this.upgrade() else {
                    // The surface that owned this server is gone. Nothing can
                    // reach the tool set any more, so there is nothing to
                    // re-sync and nothing to keep alive.
                    return;
                };
                // A failed re-sync is not fatal: the previous set stays and the
                // next notification (or a call) tries again. It is not silent
                // either. By the time `resync` returns, it has already spent a
                // whole round of retries, and this pump is the only place that
                // round is ever reported — discarding it leaves the surface
                // quietly no longer tracking the server, with the first sign
                // being a tool that is simply missing.
                //
                // stderr, not an event: this adapter holds neither a store nor
                // a session, and wiring one in to carry a sentence would cost
                // more than the sentence is worth. `apps/agent` reports this
                // class of news the same way.
                if let Err(error) = this.resync().await {
                    eprintln!(
                        "mcp server '{}': re-sync failed, the last good tool set stays: {error}",
                        this.server
                    );
                }
            }
        });
    }

    fn store(
        &self,
        connection: Arc<dyn McpConnection>,
        mut tools: Vec<McpTool>,
    ) -> Result<(), McpError> {
        // Sorted so that two tools which collide after sanitizing are resolved
        // by name and not by whatever order the server happened to list them in.
        tools.sort_by(|a, b| a.name.cmp(&b.name));

        let mut used: BTreeSet<String> = BTreeSet::new();
        let mut map = BTreeMap::new();
        for tool in tools {
            let plain = naming::plain_name(&self.server, &tool.name);
            let name = if plain.len() > naming::MAX_TOOL_NAME_LEN || used.contains(&plain) {
                naming::disambiguated_name(&self.server, &tool.name, &plain)
            } else {
                plain
            };
            if !used.insert(name.clone()) {
                // The disambiguating suffix is derived and can in principle
                // repeat (or a final name can land on another tool's). The
                // earlier set stays in place and the caller is told: a tool
                // that silently vanished from the surface is the one failure
                // here nobody would ever see.
                return Err(McpError::NameCollision { server: self.server.clone(), tool: name });
            }
            map.insert(name, tool);
        }

        let mut state = self.write();
        state.connection = Some(connection);
        state.tools = map;
        Ok(())
    }

    fn current_connection(&self) -> Option<Arc<dyn McpConnection>> {
        self.read().connection.clone()
    }

    fn set_connection(&self, connection: Arc<dyn McpConnection>) {
        self.write().connection = Some(connection);
    }

    fn clear_connection(&self) {
        self.write().connection = None;
    }

    /// The live connection to call `tool_name` on, and the server's own name for
    /// it, reconnecting first if there is no session.
    ///
    /// This is where a dead session becomes recoverable: a failed call drops the
    /// connection (see `invoke`), and the next call picks one back up here.
    /// `resync` reloads the tool set as it reconnects, so a call is a chance to
    /// catch up as well as to heal — and it is safe to do so, because it is
    /// idempotent, which the call it precedes is not.
    async fn session_for(
        &self,
        tool_name: &str,
    ) -> Result<(Arc<dyn McpConnection>, String), ToolProviderError> {
        let original_name = {
            let state = self.read();
            match state.tools.get(tool_name) {
                Some(tool) => tool.name.clone(),
                None => {
                    return Err(ToolProviderError::UnknownTool {
                        tool: tool_name.to_string(),
                        available: state.tools.keys().cloned().collect(),
                    });
                }
            }
        };

        if self.current_connection().is_none() {
            self.resync().await.map_err(|error| ToolProviderError::Failed {
                tool: tool_name.to_string(),
                message: error.to_string(),
            })?;
        }

        // `resync` installs a connection on success, so the fallback is only
        // reachable if a caller established a session some other way and then
        // lost it between this call's two reads — cheap insurance, not a path.
        match self.current_connection() {
            Some(connection) => Ok((connection, original_name)),
            None => Err(ToolProviderError::Failed {
                tool: tool_name.to_string(),
                message: format!("mcp server '{}' has no live session", self.server),
            }),
        }
    }

    /// How long one tool call may run.
    ///
    /// The configured value when there is one, the documented default when
    /// there is not — and it is the same number `definitions()` puts in each
    /// tool's `timeout_ms`, so what the caller is told and what the call gets
    /// cannot drift apart.
    fn call_deadline(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TOOL_CALL_TIMEOUT_MS))
    }

    /// The provider's state, read.
    ///
    /// A poisoned lock means a panic was raised while the state was held.
    /// Taking the panic again here would turn that one fault into a permanent
    /// one — every later read and write of this server's tool set would panic
    /// too, and one server's fault would take down a run that has others. The
    /// poison is read through instead, as the kernel's live bus reads its own
    /// state: the mutations guarded here are single map operations with no user
    /// code between them, so the state is as consistent as the last call that
    /// finished. (YA-24)
    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The provider's state, written — read through a poison for the reason
    /// `read` gives just above.
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait::async_trait]
impl ToolProvider for McpToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let state = self.read();
        state
            .tools
            .iter()
            .map(|(name, tool)| naming::definition(name, tool, &self.requires, self.timeout_ms))
            .collect()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.read().tools.get(tool_name).map(|tool| naming::side_effect(&tool.annotations))
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let (connection, original_name) = self.session_for(&invocation.tool_name).await?;

        // The deadline a call gets is the deadline the configuration declared.
        // It is armed at the layer the configured number lives on, so the
        // `timeout` a `ToolDefinition` advertises and the clock that actually
        // cuts the call off are the same number. The transport arms none of its
        // own — two clocks mean the shorter one wins, which is how a configured
        // `timeout = "5m"` silently became whatever the transport had chosen.
        let deadline = self.call_deadline();
        let call = connection.call_tool(&original_name, invocation.arguments.clone());
        let outcome = match tokio::time::timeout(deadline, call).await {
            Ok(outcome) => outcome,
            Err(_) => {
                // A call cut off by its deadline may still be running on the
                // server, so it is no more evidence that the session survived
                // than a plain failure is. Same treatment: drop the connection.
                self.clear_connection();
                return Err(ToolProviderError::Failed {
                    tool: invocation.tool_name.clone(),
                    message: format!("timed out after {} ms", deadline.as_millis()),
                });
            }
        };

        match outcome {
            Ok(result) => Ok(result),
            Err(error) => {
                // The call ran once and is not replayed: a failure is no proof
                // it did not reach the server. Whether the *session* survived is
                // a different question and one this crate can answer — whatever
                // the error means, it is no evidence the session is still good,
                // and a connection kept past its death is how a server that has
                // come back stays unreachable. Drop it; the next call reconnects
                // (`session_for`), so the surface heals one call later rather
                // than staying broken until the process restarts.
                self.clear_connection();
                Err(ToolProviderError::Failed {
                    tool: invocation.tool_name.clone(),
                    message: error.to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use serde_json::{Value, json};

    use super::*;
    use crate::connection::McpAnnotations;

    /// Set in the child half of `a_failed_re_sync_is_reported_on_stderr`, so the
    /// two halves of that test can tell each other apart.
    const CHILD: &str = "YAOYAO_MCP_RESYNC_CHILD";

    fn tool(name: &str, annotations: McpAnnotations) -> McpTool {
        McpTool {
            name: name.to_string(),
            description: format!("{name} tool"),
            input_schema: json!({ "type": "object" }),
            annotations,
        }
    }

    /// A connection whose behaviour a test can steer.
    #[derive(Default)]
    struct FakeConnection {
        tools: Mutex<Vec<McpTool>>,
        list_fails: AtomicBool,
        call_fails: AtomicBool,
        /// How long `call_tool` takes before it answers, so a test can put a
        /// call on either side of the deadline it configured.
        call_delay_ms: AtomicU64,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeConnection {
        fn with_tools(tools: Vec<McpTool>) -> Arc<Self> {
            Arc::new(Self { tools: Mutex::new(tools), ..Default::default() })
        }
    }

    #[async_trait::async_trait]
    impl McpConnection for FakeConnection {
        async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
            if self.list_fails.load(Ordering::SeqCst) {
                return Err(McpError::Unreachable {
                    server: "fake".to_string(),
                    message: "listing refused".to_string(),
                });
            }
            Ok(self.tools.lock().unwrap().clone())
        }

        async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult, McpError> {
            self.calls.lock().unwrap().push((name.to_string(), arguments));
            // Recorded before the wait, so a call cut off by its deadline is
            // still visible as an attempt that reached the server.
            let delay = self.call_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if self.call_fails.load(Ordering::SeqCst) {
                return Err(McpError::ToolCall {
                    server: "fake".to_string(),
                    tool: name.to_string(),
                    message: "connection lost".to_string(),
                });
            }
            Ok(ToolResult::completed("done"))
        }
    }

    struct FakeConnector {
        connection: Arc<FakeConnection>,
        connect_failures: AtomicUsize,
        connects: AtomicUsize,
    }

    impl FakeConnector {
        fn new(connection: Arc<FakeConnection>) -> Arc<Self> {
            Arc::new(Self {
                connection,
                connect_failures: AtomicUsize::new(0),
                connects: AtomicUsize::new(0),
            })
        }

        fn failing(connection: Arc<FakeConnection>, times: usize) -> Arc<Self> {
            Arc::new(Self {
                connection,
                connect_failures: AtomicUsize::new(times),
                connects: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl McpConnector for FakeConnector {
        async fn connect(&self) -> Result<Arc<dyn McpConnection>, McpError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let remaining = self.connect_failures.load(Ordering::SeqCst);
            if remaining > 0 {
                self.connect_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(McpError::Unreachable {
                    server: "fake".to_string(),
                    message: "not yet".to_string(),
                });
            }
            Ok(self.connection.clone())
        }
    }

    fn fast_backoff() -> Backoff {
        Backoff { base: Duration::from_millis(1), cap: Duration::from_millis(2), attempts: 3 }
    }

    async fn provider_with(connector: Arc<FakeConnector>) -> Arc<McpToolProvider> {
        provider_with_timeout(connector, Some(1000)).await
    }

    async fn provider_with_timeout(
        connector: Arc<FakeConnector>,
        timeout_ms: Option<u64>,
    ) -> Arc<McpToolProvider> {
        let provider = Arc::new(McpToolProvider::new(
            "files",
            vec![Capability::new(Capability::FS_READ)],
            connector,
            timeout_ms,
            fast_backoff(),
        ));
        provider.start().await.unwrap();
        provider
    }

    /// The names currently on offer, in order — read through `definitions()`,
    /// which is the door production reads them through. A second accessor
    /// returning the same keys was one more thing that had to stay in step with
    /// this one for no reader's benefit (YA-47).
    fn names_of(provider: &McpToolProvider) -> Vec<String> {
        provider.definitions().into_iter().map(|definition| definition.name).collect()
    }

    #[tokio::test]
    async fn a_sync_exposes_namespaced_tools_with_the_servers_requires() {
        let connection = FakeConnection::with_tools(vec![tool(
            "read",
            McpAnnotations { read_only: Some(true), ..Default::default() },
        )]);
        let provider = provider_with(FakeConnector::new(connection)).await;

        let definitions = provider.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "mcp__files__read");
        assert_eq!(definitions[0].side_effect, ToolSideEffect::Read);
        assert_eq!(
            definitions[0].capability_requirements,
            vec![Capability::new(Capability::FS_READ)]
        );
    }

    #[tokio::test]
    async fn tools_that_collide_after_sanitizing_get_distinct_names() {
        let connection = FakeConnection::with_tools(vec![
            tool("a b", McpAnnotations::default()),
            tool("a_b", McpAnnotations::default()),
        ]);
        let provider = provider_with(FakeConnector::new(connection)).await;

        let names = names_of(&provider);
        assert_eq!(names.len(), 2);
        // Both sanitize to `mcp__files__a_b`; one keeps the plain name and the
        // other takes the disambiguator, so neither is unreachable.
        assert!(names.contains(&"mcp__files__a_b".to_string()));
        assert!(names.iter().any(|n| n.starts_with("mcp__files__a_b__")));
    }

    #[tokio::test]
    async fn a_call_reaches_the_server_under_its_own_name() {
        let connection =
            FakeConnection::with_tools(vec![tool("read/file", McpAnnotations::default())]);
        let provider = provider_with(FakeConnector::new(connection.clone())).await;

        let names = names_of(&provider);
        assert_eq!(names, vec!["mcp__files__read_file"]);

        let result = provider
            .invoke(&ToolInvocation::proposed("mcp__files__read_file", json!({ "path": "a" })))
            .await
            .unwrap();
        assert_eq!(result.content, "done");

        // The server saw its own name and the arguments, not the mangled one.
        let calls = connection.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read/file");
        assert_eq!(calls[0].1, json!({ "path": "a" }));
    }

    #[tokio::test]
    async fn a_failed_resync_keeps_the_last_good_tool_set() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);
        let provider = provider_with(FakeConnector::new(connection.clone())).await;
        assert_eq!(names_of(&provider), vec!["mcp__files__read"]);

        connection.list_fails.store(true, Ordering::SeqCst);
        assert!(provider.resync().await.is_err());

        // The surface did not flicker: the last set that synced is still here.
        assert_eq!(names_of(&provider), vec!["mcp__files__read"]);
    }

    #[tokio::test]
    async fn resync_retries_through_a_transient_connect_failure() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);
        let connector = FakeConnector::failing(connection, 2);
        let provider = Arc::new(McpToolProvider::new(
            "files",
            Vec::new(),
            connector.clone(),
            None,
            fast_backoff(),
        ));

        provider.start().await.unwrap();

        assert_eq!(names_of(&provider), vec!["mcp__files__read"]);
        assert_eq!(connector.connects.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn resync_gives_up_once_the_backoff_is_exhausted() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);
        let connector = FakeConnector::failing(connection, 10);
        let provider =
            McpToolProvider::new("files", Vec::new(), connector.clone(), None, fast_backoff());

        assert!(provider.start().await.is_err());
        // attempts + 1 tries, then it stops rather than looping forever.
        assert_eq!(connector.connects.load(Ordering::SeqCst), 4);
        assert!(names_of(&provider).is_empty());
    }

    #[tokio::test]
    async fn the_last_attempt_does_not_wait_before_giving_up() {
        // One attempt, and a one-second backoff: giving up has no next attempt
        // to wait for, so it must not sleep at all. Before this, the final
        // failure slept and then returned the error a second late.
        let connection = FakeConnection::with_tools(vec![]);
        let connector = FakeConnector::failing(connection, 10);
        let backoff =
            Backoff { base: Duration::from_secs(1), cap: Duration::from_secs(1), attempts: 0 };
        let provider = McpToolProvider::new("files", Vec::new(), connector.clone(), None, backoff);

        let started = std::time::Instant::now();
        assert!(provider.start().await.is_err());

        assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "gave up without sleeping a backoff nobody would use"
        );
    }

    #[test]
    fn a_name_collision_is_an_error_rather_than_a_silently_lost_tool() {
        // The reachable path from the audit: one tool's plain name is exactly
        // what another tool's disambiguated name resolves to. Build that pair
        // deliberately rather than hoping for a hash collision — the guard is
        // what is under test, not the odds of reaching it.
        let common = "a".repeat(42);
        let long_tool = format!("{common}{}", "z".repeat(30));
        let plain = naming::plain_name("files", &long_tool);
        let resolved = naming::disambiguated_name("files", &long_tool, &plain);
        // The long tool truncates to a 54-character head; a second tool whose
        // plain name is that head plus the same suffix lands on the same
        // final name, and is short enough to keep its plain form.
        let collision_tool = resolved
            .strip_prefix("mcp__files__")
            .expect("the disambiguated name is namespaced")
            .to_string();
        assert_eq!(naming::plain_name("files", &collision_tool), resolved);

        let provider = McpToolProvider::new(
            "files",
            Vec::new(),
            FakeConnector::new(FakeConnection::with_tools(vec![])),
            None,
            fast_backoff(),
        );
        let error = provider
            .store(
                FakeConnection::with_tools(vec![]),
                vec![
                    tool(&collision_tool, McpAnnotations::default()),
                    tool(&long_tool, McpAnnotations::default()),
                ],
            )
            .unwrap_err();

        assert!(matches!(error, McpError::NameCollision { .. }), "{error}");
        // The refused set never reaches the surface.
        assert!(names_of(&provider).is_empty());
    }

    #[tokio::test]
    async fn an_unknown_tool_is_an_error_and_never_reaches_the_server() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);
        let provider = provider_with(FakeConnector::new(connection.clone())).await;

        let error = provider
            .invoke(&ToolInvocation::proposed("mcp__files__write", json!({})))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolProviderError::UnknownTool { .. }), "{error:?}");
        assert!(connection.calls.lock().unwrap().is_empty());
    }

    /// A call that fails drops the session, and the next call reconnects rather
    /// than reusing a connection that may be gone — the difference between a
    /// server that dies mid-run and one that is unreachable for the rest of the
    /// process. Nothing here calls `resync`: the next call is what heals.
    #[tokio::test]
    async fn a_failed_call_drops_the_session_and_the_next_call_reconnects() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);
        let connector = FakeConnector::new(connection.clone());
        let provider = provider_with(connector.clone()).await;
        assert_eq!(connector.connects.load(Ordering::SeqCst), 1);

        connection.call_fails.store(true, Ordering::SeqCst);
        let error = provider
            .invoke(&ToolInvocation::proposed("mcp__files__read", json!({ "path": "a" })))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolProviderError::Failed { .. }), "{error:?}");
        // The failed call is not replayed: the server saw it exactly once.
        assert_eq!(connection.calls.lock().unwrap().len(), 1);

        // The server comes back. The next call reconnects on its own.
        connection.call_fails.store(false, Ordering::SeqCst);
        let result = provider
            .invoke(&ToolInvocation::proposed("mcp__files__read", json!({ "path": "a" })))
            .await
            .expect("the next call reconnects and runs");
        assert_eq!(result.content, "done");
        assert_eq!(connector.connects.load(Ordering::SeqCst), 2, "the session was re-established");
    }

    #[tokio::test]
    async fn a_tool_list_changed_notification_triggers_a_resync() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);
        let provider = provider_with(FakeConnector::new(connection.clone())).await;
        assert_eq!(names_of(&provider).len(), 1);

        let changed = Arc::new(Notify::new());
        provider.spawn_resync_listener(changed.clone());

        connection
            .tools
            .lock()
            .unwrap()
            .push(tool("write", McpAnnotations { destructive: Some(true), ..Default::default() }));

        // The notification is the only thing that tells the provider the server
        // grew a tool; nothing else polls.
        changed.notify_one();
        for _ in 0..100 {
            if names_of(&provider).len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(names_of(&provider).len(), 2);
        assert_eq!(provider.side_effect("mcp__files__write"), Some(ToolSideEffect::Destructive));
    }

    /// A re-sync that fails from the listener is news, not an abort: the pump
    /// survives it and the next notification still lands.
    #[tokio::test]
    async fn a_failed_re_sync_from_a_notification_leaves_the_pump_alive() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);
        let provider = provider_with(FakeConnector::new(connection.clone())).await;

        let changed = Arc::new(Notify::new());
        provider.spawn_resync_listener(changed.clone());

        // The server is down and the notification fires anyway.
        connection.list_fails.store(true, Ordering::SeqCst);
        changed.notify_one();
        for _ in 0..100 {
            if provider.read().connection.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // It re-synced and it failed — the session was dropped by the attempt,
        // going by the same rule a direct `resync` follows.
        assert!(provider.read().connection.is_none(), "the failed re-sync did not run");
        assert_eq!(names_of(&provider), vec!["mcp__files__read"]);

        // The server comes back with a new tool. If the failed round had killed
        // the pump, nothing would ever pick this up.
        connection.list_fails.store(false, Ordering::SeqCst);
        connection
            .tools
            .lock()
            .unwrap()
            .push(tool("write", McpAnnotations { destructive: Some(true), ..Default::default() }));
        changed.notify_one();
        for _ in 0..100 {
            if names_of(&provider).len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(names_of(&provider).len(), 2);
    }

    /// The error is *reported*, which a test cannot read from inside the
    /// process: the harness shares one stderr across tests that run at the same
    /// time, so redirecting it here would be a race rather than a measurement.
    ///
    /// So the assertion is made where the pipe is — this test re-runs itself as
    /// a child process and reads its stderr.
    #[tokio::test]
    async fn a_failed_re_sync_is_reported_on_stderr() {
        if std::env::var(CHILD).is_ok() {
            // The child: fire the notification, let the re-sync fail, leave the
            // line on stderr for the parent to read.
            let connection = FakeConnection::with_tools(Vec::new());
            let connector = FakeConnector::failing(connection.clone(), usize::MAX);
            let provider = Arc::new(McpToolProvider::new(
                "vanished",
                Vec::new(),
                connector,
                None,
                fast_backoff(),
            ));
            let changed = Arc::new(Notify::new());
            provider.spawn_resync_listener(changed.clone());
            changed.notify_one();
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }

        let exe = std::env::current_exe().expect("a test binary knows where it is");
        let output = std::process::Command::new(exe)
            .args(["--nocapture", "a_failed_re_sync_is_reported_on_stderr"])
            .env(CHILD, "1")
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("the child runs");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "the child failed: {stderr}");
        assert!(
            stderr.contains("re-sync failed") && stderr.contains("vanished"),
            "a failed re-sync reported nothing: {stderr:?}"
        );
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let backoff = Backoff {
            base: Duration::from_millis(100),
            cap: Duration::from_millis(1000),
            attempts: 6,
        };
        assert_eq!(backoff.delay(0), Duration::from_millis(100));
        assert_eq!(backoff.delay(1), Duration::from_millis(200));
        assert_eq!(backoff.delay(2), Duration::from_millis(400));
        assert_eq!(backoff.delay(3), Duration::from_millis(800));
        // Capped, and it stays capped however far it is pushed.
        assert_eq!(backoff.delay(4), Duration::from_millis(1000));
        assert_eq!(backoff.delay(40), Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn a_call_is_cut_off_by_the_deadline_the_configuration_declared() {
        let connection = FakeConnection::with_tools(vec![tool("slow", McpAnnotations::default())]);
        // Two orders of magnitude past the deadline this provider was given.
        connection.call_delay_ms.store(2_000, Ordering::SeqCst);
        let connector = FakeConnector::new(connection.clone());
        let provider = provider_with_timeout(connector.clone(), Some(20)).await;

        let error = provider
            .invoke(&ToolInvocation::proposed("mcp__files__slow", json!({})))
            .await
            .unwrap_err();

        let message = match error {
            ToolProviderError::Failed { tool, message } => {
                assert_eq!(tool, "mcp__files__slow");
                message
            }
            other => panic!("expected Failed, got {other:?}"),
        };
        // The number in the message is the number that was configured. That is
        // the whole point: the deadline a call gets is the declared one, not a
        // clock of the transport's own choosing.
        assert_eq!(message, "timed out after 20 ms");

        // It was the wait the deadline ended, not the routing: the call reached
        // the server, and a 20 ms deadline cut off a call that would have taken
        // two seconds.
        assert_eq!(connection.calls.lock().unwrap().len(), 1);

        // A call cut off by its deadline says nothing about the session having
        // survived, so the connection was dropped — the next call reconnects.
        assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
        connection.call_delay_ms.store(0, Ordering::SeqCst);
        let result = provider
            .invoke(&ToolInvocation::proposed("mcp__files__slow", json!({})))
            .await
            .unwrap();
        assert_eq!(result.content, "done");
        assert_eq!(connector.connects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_call_inside_the_declared_deadline_is_not_cut_off() {
        let connection = FakeConnection::with_tools(vec![tool("slow", McpAnnotations::default())]);
        // Long enough that a short deadline would end it, well inside the one
        // this provider was told to use.
        connection.call_delay_ms.store(200, Ordering::SeqCst);
        let connector = FakeConnector::new(connection.clone());
        let provider = provider_with_timeout(connector.clone(), Some(5_000)).await;

        let result = provider
            .invoke(&ToolInvocation::proposed("mcp__files__slow", json!({})))
            .await
            .unwrap();
        assert_eq!(result.content, "done");
        // Nothing was dropped: the session that answered is the session the next
        // call will use.
        assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_deadline_is_the_number_the_definition_advertises() {
        let connection = FakeConnection::with_tools(vec![tool("read", McpAnnotations::default())]);

        let configured =
            provider_with_timeout(FakeConnector::new(connection.clone()), Some(5_000)).await;
        assert_eq!(configured.call_deadline(), Duration::from_millis(5_000));
        assert_eq!(configured.definitions()[0].timeout_ms, Some(5_000));

        // Unconfigured: a call still gets a deadline — the documented default,
        // not none. The definition keeps advertising `None`, which is the honest
        // report that the operator declared nothing; `None` reads as
        // "undeclared", not as "unbounded".
        let unconfigured = provider_with_timeout(FakeConnector::new(connection), None).await;
        assert_eq!(
            unconfigured.call_deadline(),
            Duration::from_millis(DEFAULT_TOOL_CALL_TIMEOUT_MS)
        );
        assert_eq!(unconfigured.definitions()[0].timeout_ms, None);
    }

    /// A poisoned provider lock must not turn one thread's death into a dead
    /// tool surface — this server's tools would vanish for the rest of a run
    /// that may have others. (YA-24)
    #[tokio::test]
    async fn a_poisoned_provider_lock_does_not_stop_the_provider() {
        let connection = FakeConnection::with_tools(vec![tool("thing", McpAnnotations::default())]);
        let provider = provider_with(FakeConnector::new(connection)).await;

        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = provider.write();
            panic!("a thread died while holding the provider lock");
        }));
        assert!(died.is_err(), "the premise: the lock is poisoned now");

        // What used to panic now answers with the tool set the sync loaded.
        assert_eq!(provider.definitions().len(), 1);
        assert_eq!(provider.server(), "files");
    }
}
