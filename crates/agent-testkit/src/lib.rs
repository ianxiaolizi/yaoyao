//! agent-testkit: Fakes, stubs, and test utilities for agent crate tests.

#![deny(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use futures_channel::mpsc;
use futures_util::StreamExt;

use agent_ports::agent_loop::{AgentLoop, AgentLoopError, StopReason, TurnContext, TurnStart};
use agent_ports::artifact_store::{ArtifactStore, ArtifactStoreError, is_blob_name};
use agent_ports::context_engine::{
    CompactionCandidate, ContextEngine, ContextEngineError, ContextUsage, DerivedContext,
};
use agent_ports::decision::{
    DecisionAdvisor, DecisionAnswer, DecisionError, DecisionMode, DecisionResult, DecisionState,
    DecisionTrace, DecisionValue, FastDecisionProvider, QuestionPack,
};
use agent_ports::doctor::DoctorCheck;
use agent_ports::event_store::{
    AppendReceipt, EventStore, EventStoreError, EventSubscription, FlushReceipt, SessionLease,
    is_safe_dir_name,
};
use agent_ports::model_provider::{
    ContentBlock, FinishReason, ModelCapability, ModelProvider, ModelProviderError, ModelRequest,
    ModelResponse, ModelStreamError, ModelStreamEvent, ModelStreamSink,
};
use agent_ports::model_router::{ModelRoute, ModelRouter, ModelTier, RouteRequest};
use agent_ports::secret_provider::{SecretProvider, SecretProviderError};
use agent_ports::tool_gateway::{ToolGateway, ToolGatewayError};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::diagnostics::{DiagnosticMessage, HealthCheckId, HealthStatus};
use agent_protocol::events::{EventDraft, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::model::ModelMessage;
use agent_protocol::permissions::{ArtifactRef, SecretRef};
use chrono::Utc;
use sha2::{Digest, Sha256};

/// Take a fake's recording lock for reading, poison included.
///
/// A recording is written by the code under test and read by the test, so a
/// poisoned lock means some thread panicked while a recording was being
/// touched. Reporting an empty collection then would say "it was never called"
/// — the one answer a poisoned recorder cannot support — and the assertion
/// failure would point at the count instead of at the panic that really
/// happened. Panicking keeps the original failure visible. This is the `T-8`
/// judgement applied to `T-9`: in a double, a panic is the correct signal, and
/// silence is the defect.
fn recorded<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().expect("a fake's recording lock was poisoned")
}

/// A fake EventStore backed by in-memory maps, for testing.
///
/// Mirrors the real store's contract: seq is assigned here (not guessed by the
/// producer), `expected_next_seq` must match, and duplicate `event_id`s are
/// idempotent rather than applied twice.
#[derive(Default)]
pub struct FakeEventStore {
    inner: Arc<Mutex<Inner>>,
    /// session_id -> lease nesting depth held by this store's callers.
    leases: Arc<Mutex<HashMap<String, usize>>>,
    /// session_id -> wake signals, mirroring the real store's notification.
    wakeups: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<()>>>>>,
    /// Dropped with the last clone of the store, which is how a live
    /// subscription learns there is nothing left to wait for. The real store
    /// gets this from its channels closing; the fake has no channels that close
    /// on their own, so it holds the lifetime explicitly.
    alive: Arc<()>,
    /// When set, `append` fails unless the caller already holds the session's
    /// lease. Off by default so the many tests that write directly still can.
    require_lease: bool,
    /// Sessions enforced individually, on top of `require_lease`. A flow that
    /// legitimately writes a *new* session without a lease — a fork creating its
    /// child — can be enforced only where the read → append actually happens.
    require_lease_sessions: HashSet<String>,
}

/// Releases the fake's lease nesting when dropped.
struct FakeLease {
    leases: Arc<Mutex<HashMap<String, usize>>>,
    key: String,
    session_id: SessionId,
}

impl SessionLease for FakeLease {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        let mut leases = self.leases.lock().unwrap();
        if let Some(depth) = leases.get_mut(&self.key) {
            *depth = depth.saturating_sub(1);
            if *depth == 0 {
                leases.remove(&self.key);
            }
        }
    }
}

#[derive(Default)]
struct Inner {
    /// session_id -> stored events in seq order.
    events: HashMap<String, Vec<StoredEvent>>,
    /// (session_id, event_id) -> seq, for idempotent duplicate detection.
    ///
    /// Keyed by session because the real store is: an `event_id` is unique
    /// within a log, not across the store. Fork copies a parent's events into a
    /// child keeping their ids, so a store-wide index would read the whole
    /// inherited prefix as duplicates and silently write an empty child.
    seen: HashMap<(String, String), EventSeq>,
    /// session_id -> next sequence to assign.
    next_seq: HashMap<String, u64>,
}

impl FakeEventStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make `append` require that the caller holds the session lease.
    ///
    /// The fake has no second process, so it cannot stage the race it stands in
    /// for. What it can do is refuse the write that the race would have refused:
    /// a caller that reads the log's end and appends at it *without* holding the
    /// lease is a caller another process may overtake in between, and here the
    /// append is that overtaking. A writer that holds the lease across both
    /// calls appends cleanly, so a test built on this store passes exactly when
    /// the read → append is covered.
    ///
    /// Opt-in because most tests write directly, and the real store does not
    /// require a caller lease for a bare `append` either — it takes one itself.
    pub fn requiring_lease(mut self) -> Self {
        self.require_lease = true;
        self
    }

    /// Enforce the lease requirement for one session only.
    ///
    /// A flow can be right to write *some* sessions without a lease — a fork
    /// appends a brand-new child at seq 0, having read nothing to be overtaken
    /// on — while still having to lease the session it read to build the batch.
    /// Naming the sessions keeps the enforcement on the read → append and off
    /// the writes that have no read behind them.
    pub fn requiring_lease_for(mut self, session_id: &SessionId) -> Self {
        self.require_lease_sessions.insert(session_id.as_str().to_string());
        self
    }

    fn lease_required(&self, key: &str) -> bool {
        self.require_lease || self.require_lease_sessions.contains(key)
    }
}

#[async_trait::async_trait]
impl EventStore for FakeEventStore {
    async fn append(
        &self,
        session_id: &SessionId,
        expected_next_seq: EventSeq,
        drafts: Vec<EventDraft>,
    ) -> Result<AppendReceipt, EventStoreError> {
        let mut inner = self.inner.lock().unwrap();
        let key = session_id.as_str().to_string();
        if self.lease_required(&key)
            && self.leases.lock().unwrap().get(&key).copied().unwrap_or(0) == 0
        {
            return Err(EventStoreError::ConcurrentWriter(session_id.clone()));
        }
        let mut next = inner.next_seq.get(&key).copied().unwrap_or(0);
        if expected_next_seq.as_u64() != next {
            return Err(EventStoreError::SequenceConflict {
                expected: expected_next_seq,
                actual: EventSeq::new(next),
            });
        }

        let mut appended = Vec::new();
        let mut duplicates = 0;
        for draft in drafts {
            let id_key = draft.event_id.to_string();
            let seen_key = (key.clone(), id_key);
            if inner.seen.contains_key(&seen_key) {
                duplicates += 1;
                continue;
            }
            let seq = EventSeq::new(next);
            next += 1;
            inner.seen.insert(seen_key, seq);
            appended.push(StoredEvent { seq, recorded_at: Utc::now(), event: draft });
        }

        inner.next_seq.insert(key.clone(), next);
        inner.events.entry(key.clone()).or_default().extend(appended.clone());
        drop(inner);

        // After the events are in place, so a woken subscriber always finds
        // what it was woken for.
        if !appended.is_empty() {
            let mut wakeups = self.wakeups.lock().unwrap();
            if let Some(list) = wakeups.get_mut(&key) {
                list.retain_mut(|tx| match tx.try_send(()) {
                    Ok(()) => true,
                    Err(e) => e.is_full(),
                });
            }
        }

        Ok(AppendReceipt { session_id: session_id.clone(), appended, duplicates })
    }

    /// Hand out a lease that counts nesting and never contends.
    ///
    /// The fake has no second process to contend with, so there is nothing for a
    /// timeout to model. What it does model is re-entrancy, which is what lets a
    /// caller hold a lease and still call `append`.
    ///
    /// What it also models is the id check: the real store turns down a session
    /// id that cannot be a single safe directory name before there is any lease
    /// to hand out, and a fake that accepted one would let a test run against a
    /// name the product can never open. The predicate is the port's, so the two
    /// cannot disagree.
    async fn lease(
        &self,
        session_id: &SessionId,
    ) -> Result<Box<dyn SessionLease>, EventStoreError> {
        if !is_safe_dir_name(session_id.as_str()) {
            return Err(EventStoreError::unsafe_session_id());
        }
        let key = session_id.as_str().to_string();
        *self.leases.lock().unwrap().entry(key.clone()).or_insert(0) += 1;
        Ok(Box::new(FakeLease { leases: self.leases.clone(), key, session_id: session_id.clone() }))
    }

    async fn flush(&self, session_id: &SessionId) -> Result<FlushReceipt, EventStoreError> {
        let inner = self.inner.lock().unwrap();
        let key = session_id.as_str().to_string();
        let next = inner.next_seq.get(&key).copied().unwrap_or(0);
        Ok(FlushReceipt {
            session_id: session_id.clone(),
            durable_seq: EventSeq::new(next.saturating_sub(1)),
        })
    }

    async fn read_stream(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, EventStoreError> {
        let inner = self.inner.lock().unwrap();
        let key = session_id.as_str().to_string();
        let events = inner.events.get(&key).cloned().unwrap_or_default();
        Ok(events.into_iter().filter(|e| e.seq >= from_seq).take(limit).collect())
    }

    async fn subscribe(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
    ) -> Result<Box<dyn EventSubscription>, EventStoreError> {
        let key = session_id.as_str().to_string();
        let (tx, wake) = mpsc::channel(1);
        self.wakeups.lock().unwrap().entry(key.clone()).or_default().push(tx);
        Ok(Box::new(FakeSubscription {
            inner: self.inner.clone(),
            key,
            next_expected: from_seq.as_u64(),
            wake,
            alive: Arc::downgrade(&self.alive),
        }))
    }
}

/// A live view of the fake store's session.
///
/// It waits, like the real one, and unlike a replay it does not end when it has
/// caught up: a fake that returned `None` there would teach every test built on
/// it the semantics this milestone removed.
struct FakeSubscription {
    inner: Arc<Mutex<Inner>>,
    key: String,
    next_expected: u64,
    wake: mpsc::Receiver<()>,
    alive: Weak<()>,
}

impl FakeSubscription {
    fn take(&mut self) -> Vec<StoredEvent> {
        let inner = self.inner.lock().unwrap();
        let events = inner.events.get(&self.key).cloned().unwrap_or_default();
        events.into_iter().filter(|e| e.seq.as_u64() >= self.next_expected).collect()
    }
}

#[async_trait::async_trait]
impl EventSubscription for FakeSubscription {
    async fn next(&mut self) -> Result<Option<StoredEvent>, EventStoreError> {
        loop {
            let mut events = self.take();
            if !events.is_empty() {
                let first = events.remove(0);
                self.next_expected = first.seq.as_u64() + 1;
                return Ok(Some(first));
            }
            if self.alive.upgrade().is_none() {
                return Ok(None);
            }
            // No timer here: the fake is woken by its own appends, and a test
            // that needs a foreign writer has the real store's test module.
            if self.wake.next().await.is_none() {
                return Ok(None);
            }
        }
    }
}

/// A fake DoctorCheck that returns a fixed result, for testing.
pub struct FakeDoctorCheck {
    id: HealthCheckId,
    status: HealthStatus,
    message: String,
}

impl FakeDoctorCheck {
    pub fn new(id: impl Into<String>, status: HealthStatus, message: impl Into<String>) -> Self {
        Self { id: HealthCheckId::new(id), status, message: message.into() }
    }

    pub fn passing(id: impl Into<String>) -> Self {
        Self::new(id, HealthStatus::Passed, "ok")
    }

    pub fn failing(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(id, HealthStatus::Failed, message)
    }
}

#[async_trait::async_trait]
impl DoctorCheck for FakeDoctorCheck {
    fn id(&self) -> HealthCheckId {
        self.id.clone()
    }

    async fn run(&self) -> Vec<DiagnosticMessage> {
        vec![DiagnosticMessage {
            check_id: self.id.clone(),
            status: self.status.clone(),
            message: self.message.clone(),
        }]
    }
}

/// One scripted model attempt.
#[derive(Debug, Clone)]
pub enum FakeModelStep {
    /// Emits these deltas, then settles this response.
    Settled { deltas: Vec<ModelStreamEvent>, response: ModelResponse },
    /// Emits these deltas, then fails. An interrupted stream never settles.
    Failed { deltas: Vec<ModelStreamEvent>, error: ModelProviderError },
}

impl FakeModelStep {
    /// A settled attempt that streams nothing.
    pub fn settled(response: ModelResponse) -> Self {
        Self::Settled { deltas: Vec::new(), response }
    }

    /// A settled attempt that streams `deltas` before answering.
    pub fn streaming(deltas: Vec<ModelStreamEvent>, response: ModelResponse) -> Self {
        Self::Settled { deltas, response }
    }

    /// An attempt that fails without streaming anything.
    pub fn failed(error: ModelProviderError) -> Self {
        Self::Failed { deltas: Vec::new(), error }
    }

    /// An attempt that streams `deltas` and then drops the connection.
    pub fn interrupted(deltas: Vec<ModelStreamEvent>) -> Self {
        Self::Failed {
            deltas,
            error: ModelProviderError::StreamInterrupted { received_deltas: 0, cause: None },
        }
    }
}

/// A scripted ModelProvider: serves attempts from a cursor, then repeats the
/// last attempt once the script is exhausted.
pub struct FakeModelProvider {
    steps: Vec<FakeModelStep>,
    cursor: Mutex<usize>,
    /// Call ids handed out to `tool_call` responses, unique per fake instance.
    next_call_id: Mutex<u64>,
    capabilities: Vec<ModelCapability>,
    /// Every request the loop sent, in order. This is the model's own view of
    /// the conversation, which is what a history test has to compare against.
    requests: Mutex<Vec<ModelRequest>>,
}

impl FakeModelProvider {
    /// A provider replaying the given responses, streaming nothing.
    pub fn new(responses: Vec<ModelResponse>) -> Self {
        Self::script(responses.into_iter().map(FakeModelStep::settled).collect())
    }

    /// A provider replaying a full attempt script.
    pub fn script(steps: Vec<FakeModelStep>) -> Self {
        Self {
            steps,
            cursor: Mutex::new(0),
            next_call_id: Mutex::new(0),
            capabilities: vec![
                ModelCapability::new(ModelCapability::STREAMING),
                ModelCapability::new(ModelCapability::TOOL_CALL),
            ],
            requests: Mutex::new(Vec::new()),
        }
    }

    /// The requests the loop sent so far, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        recorded(&self.requests).clone()
    }

    /// A provider whose single attempt fails.
    pub fn failing(error: ModelProviderError) -> Self {
        Self::script(vec![FakeModelStep::failed(error)])
    }

    /// A provider that streams `count` text deltas and then drops the stream.
    pub fn interrupted(count: u64) -> Self {
        let deltas =
            (0..count).map(|i| ModelStreamEvent::TextDelta { text: format!("chunk{i}") }).collect();
        Self::script(vec![FakeModelStep::interrupted(deltas)])
    }

    /// Override the capabilities this provider declares. An empty set makes it
    /// fail the loop's capability check.
    pub fn with_capabilities(mut self, capabilities: Vec<ModelCapability>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// A plain final answer with no tool calls.
    pub fn answer(content: &str) -> ModelResponse {
        ModelResponse::text(content)
    }

    /// A response that proposes a single tool call.
    pub fn tool_call(name: &str, arguments: serde_json::Value) -> ModelResponse {
        // A placeholder call id; `stream` rewrites it to a unique one so two
        // calls to the same tool never collide.
        Self::tool_call_with_id(name, arguments, String::new())
    }

    /// A response proposing a tool call with an explicit provider call id.
    pub fn tool_call_with_id(
        name: &str,
        arguments: serde_json::Value,
        tool_call_id: String,
    ) -> ModelResponse {
        ModelResponse {
            content_blocks: vec![ContentBlock::tool_use(tool_call_id, name, arguments)],
            usage: None,
            finish_reason: FinishReason::new(FinishReason::TOOL_CALLS),
            provider_metadata: serde_json::Value::Null,
        }
    }

    fn next_step(&self) -> FakeModelStep {
        let mut cursor = self.cursor.lock().unwrap();
        let idx = *cursor;
        if idx + 1 < self.steps.len() {
            *cursor += 1;
        }
        self.steps.get(idx).cloned().unwrap_or_else(|| {
            FakeModelStep::settled(ModelResponse {
                content_blocks: Vec::new(),
                usage: None,
                finish_reason: FinishReason::new(FinishReason::STOP),
                provider_metadata: serde_json::Value::Null,
            })
        })
    }
}

#[async_trait::async_trait]
impl ModelProvider for FakeModelProvider {
    async fn stream(
        &self,
        request: &ModelRequest,
        sink: &dyn ModelStreamSink,
    ) -> Result<ModelResponse, ModelProviderError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        match self.next_step() {
            FakeModelStep::Settled { deltas, mut response } => {
                for delta in deltas {
                    sink.on_event(delta)
                        .await
                        .map_err(|e| ModelProviderError::Protocol(e.to_string()))?;
                }
                // Assign unique call ids to scripted tool calls that brought none.
                let mut counter = self.next_call_id.lock().unwrap();
                for block in &mut response.content_blocks {
                    if let ContentBlock::ToolUse { tool_call_id, .. } = block
                        && tool_call_id.is_empty()
                    {
                        *counter += 1;
                        *tool_call_id = format!("call_{}", *counter);
                    }
                }
                Ok(response)
            }
            FakeModelStep::Failed { deltas, error } => {
                let mut sent = 0u64;
                for delta in deltas {
                    sink.on_event(delta)
                        .await
                        .map_err(|e| ModelProviderError::Protocol(e.to_string()))?;
                    sent += 1;
                }
                Err(match error {
                    ModelProviderError::StreamInterrupted { cause, .. } => {
                        ModelProviderError::StreamInterrupted { received_deltas: sent, cause }
                    }
                    other => other,
                })
            }
        }
    }

    fn capabilities(&self) -> Vec<ModelCapability> {
        self.capabilities.clone()
    }

    fn provider_id(&self) -> String {
        "fake".to_string()
    }
}

/// Records every delta a provider emits, for assertions about live output.
#[derive(Default)]
pub struct RecordingSink {
    events: Mutex<Vec<ModelStreamEvent>>,
}

impl RecordingSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// The deltas received so far, in order.
    pub fn events(&self) -> Vec<ModelStreamEvent> {
        recorded(&self.events).clone()
    }
}

#[async_trait::async_trait]
impl ModelStreamSink for RecordingSink {
    async fn on_event(&self, event: ModelStreamEvent) -> Result<(), ModelStreamError> {
        match self.events.lock() {
            Ok(mut events) => {
                events.push(event);
                Ok(())
            }
            Err(_) => Err(ModelStreamError::Closed),
        }
    }
}

/// A secret provider that resolves every reference to one fixed value.
pub struct FixedSecret {
    value: String,
}

impl FixedSecret {
    /// Always resolve to `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self { value: value.into() }
    }
}

impl SecretProvider for FixedSecret {
    fn resolve(&self, _secret: &SecretRef) -> Result<String, SecretProviderError> {
        Ok(self.value.clone())
    }
}

/// A secret provider that resolves nothing, for missing-credential paths.
pub struct MissingSecret;

impl SecretProvider for MissingSecret {
    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError> {
        Err(SecretProviderError::NotFound {
            provider: secret.source.clone(),
            name: secret.name.clone(),
        })
    }
}

/// A ContextEngine that derives nothing.
///
/// Loop tests that care about one turn in isolation use this: an empty history
/// is what a session looks like at its first turn, so these tests exercise the
/// loop exactly as production does rather than through a test-only path.
#[derive(Default)]
pub struct NullContextEngine;

impl NullContextEngine {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ContextEngine for NullContextEngine {
    async fn derive(&self, _session_id: &SessionId) -> Result<DerivedContext, ContextEngineError> {
        Ok(DerivedContext {
            messages: Vec::new(),
            revision: EventSeq::ZERO,
            usage: ContextUsage::default(),
            observed_input_tokens: None,
            compaction: None,
        })
    }
}

/// A ContextEngine serving a fixed history, for tests about how the loop uses
/// what it is handed (prepending the current input, carrying the revision).
pub struct FakeContextEngine {
    messages: Vec<ModelMessage>,
    revision: EventSeq,
    usage: ContextUsage,
    observed_input_tokens: Option<u32>,
    compaction: Option<CompactionCandidate>,
}

impl FakeContextEngine {
    /// Derive `messages` at log position `revision`.
    pub fn new(messages: Vec<ModelMessage>, revision: EventSeq) -> Self {
        Self {
            messages,
            revision,
            usage: ContextUsage::default(),
            observed_input_tokens: None,
            compaction: None,
        }
    }

    /// Report this provider-measured input count alongside the derivation, as
    /// an engine whose log records usage does.
    pub fn with_observed_input_tokens(mut self, tokens: u32) -> Self {
        self.observed_input_tokens = Some(tokens);
        self
    }

    /// Report this usage alongside the derivation, as a windowed engine does.
    pub fn with_usage(mut self, usage: ContextUsage) -> Self {
        self.usage = usage;
        self
    }

    /// Propose this compaction alongside the derivation.
    pub fn with_compaction(mut self, candidate: CompactionCandidate) -> Self {
        self.compaction = Some(candidate);
        self
    }
}

#[async_trait::async_trait]
impl ContextEngine for FakeContextEngine {
    async fn derive(&self, _session_id: &SessionId) -> Result<DerivedContext, ContextEngineError> {
        Ok(DerivedContext {
            messages: self.messages.clone(),
            revision: self.revision,
            usage: self.usage,
            observed_input_tokens: self.observed_input_tokens,
            compaction: self.compaction.clone(),
        })
    }
}

/// A FakeToolProvider with a name -> (result, side_effect) map.
///
/// Every invocation it is handed is recorded, including the ones it refuses.
/// The map answers every call with the same frozen result, so without a record
/// a test cannot tell a second call from a first — and the question worth
/// asking of a provider is what it was handed, not only what it accepted.
pub struct FakeToolProvider {
    tools: HashMap<String, (ToolResult, ToolSideEffect)>,
    /// Tools the provider declares as unsafe to retry. The judgment gates key
    /// on the declaration, not on the call, so a test that exercises them has
    /// to be able to say a tool is not idempotent.
    non_idempotent: HashSet<String>,
    calls: Mutex<Vec<ToolInvocation>>,
}

impl Default for FakeToolProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeToolProvider {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            non_idempotent: HashSet::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Register a tool as not safe to retry.
    pub fn with_non_idempotent(mut self, name: &str) -> Self {
        self.non_idempotent.insert(name.to_string());
        self
    }

    /// Register a tool that always completes with `content`.
    pub fn with_tool(mut self, name: &str, content: &str, side_effect: ToolSideEffect) -> Self {
        self.tools.insert(name.to_string(), (ToolResult::completed(content), side_effect));
        self
    }

    /// Register a tool that always returns this exact result, for the parts of
    /// a result `with_tool` cannot express — artifacts, truncation, redaction.
    pub fn with_result(
        mut self,
        name: &str,
        result: ToolResult,
        side_effect: ToolSideEffect,
    ) -> Self {
        self.tools.insert(name.to_string(), (result, side_effect));
        self
    }

    /// Every invocation this provider was handed, in order.
    pub fn invocations(&self) -> Vec<ToolInvocation> {
        recorded(&self.calls).clone()
    }

    /// How many times this provider was invoked.
    pub fn call_count(&self) -> usize {
        recorded(&self.calls).len()
    }
}

#[async_trait::async_trait]
impl ToolProvider for FakeToolProvider {
    /// Definitions for everything registered, in name order so a manifest built
    /// from them is stable across runs.
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.clone(),
                description: format!("test tool {name}"),
                source: ToolSource::Builtin,
                side_effect: self.tools[name].1,
                capability_requirements: Vec::new(),
                idempotent: !self.non_idempotent.contains(name),
                timeout_ms: None,
                input_schema: serde_json::json!({ "type": "object" }),
            })
            .collect()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.tools.get(tool_name).map(|(_, side_effect)| *side_effect)
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        // Recorded before the lookup, the way the decision fakes record before
        // they decide: a call this provider turned away was still handed over,
        // and a record that skipped it would answer a different question.
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(invocation.clone());
        }
        self.tools.get(&invocation.tool_name).map(|(result, _)| result.clone()).ok_or_else(|| {
            ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: self.tools.keys().cloned().collect(),
            }
        })
    }
}

/// A fake in-memory content-addressed ArtifactStore for tests.
///
/// The address is the real one. The port says a blob's name is the 64-hex
/// SHA-256 of its content, so this names blobs the same way the filesystem
/// store does — which matters beyond tidiness: the kernel puts tool output here
/// and writes `ArtifactRef::locator()` into what the model sees, and a locator
/// reading `artifact://sha256/…` over a name that is not a SHA-256 is a string
/// naming an algorithm nobody used.
#[derive(Default)]
pub struct InMemoryArtifactStore {
    blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl InMemoryArtifactStore {
    /// Put `bytes` at `hash` without hashing them — the in-memory equivalent of
    /// a blob damaged behind the store's back.
    ///
    /// `put` is the only writer this map has, so without a way in from the side
    /// the content-mismatch refusal below would be a branch no test can reach,
    /// and the real store has exactly that case (a torn write, or a file edited
    /// by hand) among its own tests.
    pub fn damage(&self, hash: &str, bytes: &[u8]) {
        self.blobs.lock().unwrap().insert(hash.to_string(), bytes.to_vec());
    }
}

impl ArtifactStore for InMemoryArtifactStore {
    fn put(&self, bytes: &[u8], mime_type: &str) -> Result<ArtifactRef, ArtifactStoreError> {
        let hash = hex(&Sha256::digest(bytes));
        // `or_insert`, not `insert`: a blob already under this name is the same
        // blob, and the real store does not rewrite one either.
        self.blobs.lock().unwrap().entry(hash.clone()).or_insert_with(|| bytes.to_vec());
        Ok(ArtifactRef { hash, mime_type: mime_type.to_string(), size: bytes.len() as u64 })
    }

    fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, ArtifactStoreError> {
        // The same two checks on the same two facts as the filesystem store:
        // the reference has to be a blob name before it is looked up, and the
        // bytes found under it have to hash back to it.
        if !is_blob_name(&artifact.hash) {
            return Err(ArtifactStoreError::InvalidHash(artifact.hash.clone()));
        }
        let bytes = self
            .blobs
            .lock()
            .unwrap()
            .get(&artifact.hash)
            .cloned()
            .ok_or_else(|| ArtifactStoreError::NotFound(artifact.hash.clone()))?;
        let actual = hex(&Sha256::digest(&bytes));
        if actual != artifact.hash {
            return Err(ArtifactStoreError::ContentMismatch {
                expected: artifact.hash.clone(),
                actual,
            });
        }
        Ok(bytes)
    }
}

/// Lowercase hex — the spelling [`is_blob_name`] accepts.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A passthrough ToolGateway over a [`ToolProvider`], with no policy. For tests
/// that need the gateway seam but not policy enforcement.
pub struct FakeToolGateway {
    inner: Arc<dyn ToolProvider>,
    /// When set, every call is held for approval instead of running.
    approval_pending: Option<String>,
}

impl FakeToolGateway {
    pub fn new(inner: Arc<dyn ToolProvider>) -> Self {
        Self { inner, approval_pending: None }
    }

    /// A gateway that never runs anything: every call comes back
    /// `ApprovalPending` with `approval_id`.
    pub fn requiring_approval(inner: Arc<dyn ToolProvider>, approval_id: &str) -> Self {
        Self { inner, approval_pending: Some(approval_id.to_string()) }
    }
}

#[async_trait::async_trait]
impl ToolGateway for FakeToolGateway {
    async fn execute(
        &self,
        _session_id: &SessionId,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError> {
        if let Some(approval_id) = &self.approval_pending {
            return Err(ToolGatewayError::ApprovalPending {
                tool: invocation.tool_name.clone(),
                approval_id: approval_id.clone(),
            });
        }
        self.inner.invoke(invocation).await.map_err(ToolGatewayError::Tool)
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }
}

/// One scripted fast-decision backend behaviour.
#[derive(Debug, Clone)]
pub enum FakeDecisionBehaviour {
    /// Answer each named question with the given value, built against the pack
    /// the fake is handed. A question the pack does not ask is skipped.
    Answer(Vec<(String, DecisionValue)>),
    /// Decline to answer.
    Abstain,
    /// Fail with this error.
    Fail(DecisionError),
    /// Take longer than any caller's deadline.
    Hang(std::time::Duration),
    /// Answer correctly, but for a pack other than the one asked — a stale or
    /// cached result.
    AnswerForAnotherPack(Vec<(String, DecisionValue)>),
}

/// A scripted [`FastDecisionProvider`] that records every call it was handed.
pub struct FakeDecisionProvider {
    id: String,
    revision: Option<String>,
    behaviour: FakeDecisionBehaviour,
    calls: Mutex<Vec<(QuestionPack, DecisionState)>>,
}

impl FakeDecisionProvider {
    /// A backend with the given id and behaviour.
    pub fn new(id: impl Into<String>, behaviour: FakeDecisionBehaviour) -> Self {
        Self { id: id.into(), revision: None, behaviour, calls: Mutex::new(Vec::new()) }
    }

    /// A backend answering one question.
    pub fn answering(id: impl Into<String>, question_id: &str, value: DecisionValue) -> Self {
        Self::new(id, FakeDecisionBehaviour::Answer(vec![(question_id.to_string(), value)]))
    }

    /// Pin a revision, the way a model-backed backend would.
    pub fn with_revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = Some(revision.into());
        self
    }

    /// Every call this backend received, in order.
    pub fn calls(&self) -> Vec<(QuestionPack, DecisionState)> {
        recorded(&self.calls).clone()
    }

    /// How many times this backend was asked.
    pub fn call_count(&self) -> usize {
        recorded(&self.calls).len()
    }

    fn build(&self, pack: &QuestionPack, pairs: &[(String, DecisionValue)]) -> DecisionResult {
        let answers = pairs
            .iter()
            .filter(|(question_id, _)| pack.questions.iter().any(|q| q.question_id == *question_id))
            .map(|(question_id, value)| DecisionAnswer::new(question_id.clone(), value.clone()))
            .collect();
        DecisionResult::answered(pack, answers)
    }
}

#[async_trait::async_trait]
impl FastDecisionProvider for FakeDecisionProvider {
    async fn decide(
        &self,
        pack: &QuestionPack,
        state: &DecisionState,
    ) -> Result<DecisionResult, DecisionError> {
        match self.calls.lock() {
            Ok(mut calls) => calls.push((pack.clone(), state.clone())),
            Err(_) => return Err(DecisionError::Unavailable("fake lock poisoned".to_string())),
        }

        match &self.behaviour {
            FakeDecisionBehaviour::Answer(pairs) => Ok(self.build(pack, pairs)),
            FakeDecisionBehaviour::Abstain => Ok(DecisionResult::abstained(pack)),
            FakeDecisionBehaviour::Fail(error) => Err(error.clone()),
            FakeDecisionBehaviour::Hang(duration) => {
                tokio::time::sleep(*duration).await;
                Ok(DecisionResult::abstained(pack))
            }
            FakeDecisionBehaviour::AnswerForAnotherPack(pairs) => {
                let mut elsewhere = QuestionPack::new(
                    pack.family.clone(),
                    pack.version.clone(),
                    pack.questions.clone(),
                );
                elsewhere.pack_id = format!("{}-other", pack.pack_id);
                Ok(self.build(&elsewhere, pairs))
            }
        }
    }

    fn provider_id(&self) -> String {
        self.id.clone()
    }

    fn revision(&self) -> Option<String> {
        self.revision.clone()
    }
}

/// A router that always names one model.
///
/// For tests that are not about routing: the loop requires a router, and this
/// is the honest "one model for everything" answer rather than a placeholder
/// that pretends to choose.
pub fn one_model_router() -> Arc<dyn ModelRouter> {
    Arc::new(FixedModelRouter::new(ModelRoute::new(ModelTier::Main, "test-provider", "test-model")))
}

/// Everything one driven turn produced.
#[derive(Debug)]
pub struct DrivenTurn {
    pub events: Vec<EventDraft>,
    pub stop_reason: StopReason,
    pub final_content: Option<String>,
}

/// Drive a turn to its end, the way the Scheduler does but with nobody
/// steering.
///
/// For tests about what a *turn* did rather than about how the kernel drives
/// one. It deliberately mirrors the Scheduler's loop, and the Scheduler's own
/// tests are what pin that loop: if the two ever disagree, the kernel's tests
/// are the ones that mean something.
pub async fn drive_turn(
    loop_: &dyn AgentLoop,
    ctx: &TurnContext,
    max_steps: u32,
) -> Result<DrivenTurn, AgentLoopError> {
    let TurnStart { mut state, events } = loop_.begin_turn(ctx).await?;
    let mut all = events;
    let mut steps = 0u32;
    loop {
        if steps >= max_steps {
            return Ok(DrivenTurn {
                events: all,
                stop_reason: StopReason::MaxSteps,
                final_content: None,
            });
        }
        let outcome = loop_.step(&mut state, &[]).await?;
        all.extend(outcome.events);
        steps += 1;
        if let Some(stop_reason) = outcome.stop {
            return Ok(DrivenTurn {
                events: all,
                stop_reason,
                final_content: outcome.final_content,
            });
        }
    }
}

/// A [`ModelRouter`] returning one fixed route.
pub struct FixedModelRouter {
    route: ModelRoute,
    requests: Mutex<Vec<RouteRequest>>,
}

impl FixedModelRouter {
    /// A router that always answers with `route`.
    pub fn new(route: ModelRoute) -> Self {
        Self { route, requests: Mutex::new(Vec::new()) }
    }

    /// Every request this router was asked to route.
    pub fn requests(&self) -> Vec<RouteRequest> {
        recorded(&self.requests).clone()
    }
}

impl ModelRouter for FixedModelRouter {
    fn route(&self, request: &RouteRequest) -> ModelRoute {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        self.route.clone()
    }
}

/// A [`DecisionAdvisor`] that returns a canned trace and records what it was
/// asked. The loop tests use it to hold every shadow outcome constant.
pub struct FakeDecisionAdvisor {
    family: String,
    trace: Mutex<Option<DecisionTrace>>,
    /// How long to sit before answering. A real backend that blocks looks like
    /// this from the caller's side.
    hang: Option<std::time::Duration>,
    calls: Mutex<Vec<(RouteRequest, ModelRoute)>>,
}

impl Default for FakeDecisionAdvisor {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDecisionAdvisor {
    /// An advisor whose trace is empty.
    pub fn new() -> Self {
        Self {
            family: agent_protocol::decision::DecisionFamily::MODEL_ROUTE.to_string(),
            trace: Mutex::new(None),
            hang: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// An advisor returning `trace` verbatim.
    pub fn returning(trace: DecisionTrace) -> Self {
        let advisor = Self::new();
        if let Ok(mut slot) = advisor.trace.lock() {
            *slot = Some(trace);
        }
        advisor
    }

    /// An advisor that takes longer than any caller's deadline to answer.
    pub fn hanging() -> Self {
        let mut advisor = Self::new();
        advisor.hang = Some(std::time::Duration::from_secs(30));
        advisor
    }

    /// Every `(request, chosen route)` pair the loop handed over.
    pub fn calls(&self) -> Vec<(RouteRequest, ModelRoute)> {
        recorded(&self.calls).clone()
    }

    /// How many times the loop asked for advice.
    pub fn call_count(&self) -> usize {
        recorded(&self.calls).len()
    }
}

#[async_trait::async_trait]
impl DecisionAdvisor for FakeDecisionAdvisor {
    async fn advise_model_route(
        &self,
        request: &RouteRequest,
        chosen: &ModelRoute,
    ) -> DecisionTrace {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((request.clone(), chosen.clone()));
        }
        if let Some(hang) = self.hang {
            tokio::time::sleep(hang).await;
        }
        let canned = self.trace.lock().ok().and_then(|t| t.clone());
        canned.unwrap_or_else(|| {
            let pack = QuestionPack::new(
                agent_protocol::decision::DecisionFamily::new(&self.family),
                "fake",
                Vec::new(),
            );
            DecisionTrace::new(&pack, DecisionMode::Shadow)
        })
    }
}

#[cfg(test)]
mod tests {
    //! T-9: a poisoned recorder must not read as an empty one.
    //!
    //! This lives inside the crate because poisoning a lock means taking it and
    //! panicking, and every recording lock is private. From a test target the
    //! same check would only be reachable by widening a fake's public surface,
    //! which is a worse trade than one unit test.

    use super::*;

    /// Poison `lock` by panicking while holding it.
    fn poison<T>(lock: &Mutex<T>) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = lock.lock().expect("a fresh lock cannot be poisoned");
            panic!("poison the recording on purpose");
        }));
        assert!(outcome.is_err(), "the poisoning panic must actually happen");
    }

    fn assert_panics(what: &str, read: impl FnOnce()) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read));
        assert!(outcome.is_err(), "{what} read a poisoned recording as an empty one");
    }

    #[test]
    fn every_recording_accessor_panics_instead_of_reading_an_empty_recording() {
        let model = FakeModelProvider::new(Vec::new());
        poison(&model.requests);
        assert_panics("FakeModelProvider::requests", || drop(model.requests()));

        let sink = RecordingSink::new();
        poison(&sink.events);
        assert_panics("RecordingSink::events", || drop(sink.events()));

        let tools = FakeToolProvider::new();
        poison(&tools.calls);
        assert_panics("FakeToolProvider::invocations", || drop(tools.invocations()));
        assert_panics("FakeToolProvider::call_count", || {
            let _ = tools.call_count();
        });

        let decisions = FakeDecisionProvider::new("poisoned", FakeDecisionBehaviour::Abstain);
        poison(&decisions.calls);
        assert_panics("FakeDecisionProvider::calls", || drop(decisions.calls()));
        assert_panics("FakeDecisionProvider::call_count", || {
            let _ = decisions.call_count();
        });

        let router = FixedModelRouter::new(ModelRoute::new(ModelTier::Main, "p", "m"));
        poison(&router.requests);
        assert_panics("FixedModelRouter::requests", || drop(router.requests()));

        let advisor = FakeDecisionAdvisor::new();
        poison(&advisor.calls);
        assert_panics("FakeDecisionAdvisor::calls", || drop(advisor.calls()));
        assert_panics("FakeDecisionAdvisor::call_count", || {
            let _ = advisor.call_count();
        });
    }
}
