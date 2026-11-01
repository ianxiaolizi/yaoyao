//! Tests for secret references crossing the gateway.
//!
//! Split out of `guarded.rs` when the approval work pushed that file past its
//! module-size ratchet. They are tests, not a second executor: every one drives
//! `GuardedToolProvider` and reads what it recorded, so they keep living beside
//! the fixtures they share with the rest of the gateway's tests.
//!
//! The subject is an ordering as much as a value: a reference is *checked*
//! before a person is asked, *resolved* only after they answered, and *written
//! down* as a reference at every point in between (CONTRACT §7 #181).

use super::*;
use agent_ports::event_store::EventStore;
use agent_protocol::events::EventType;
use agent_protocol::ids::EventSeq;

/// A provider that records what it was handed and answers with `reply`.
///
/// `pub(super)` because one test that stayed in `guarded.rs` builds it too.
pub(super) struct RecordingTools {
    pub(super) defs: Vec<ToolDefinition>,
    pub(super) reply: ToolResult,
    pub(super) fail_with: Option<String>,
    pub(super) seen: Mutex<Vec<ToolInvocation>>,
}

impl RecordingTools {
    pub(super) fn new(reply: &str) -> Self {
        Self {
            defs: vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
            reply: ToolResult::completed(reply),
            fail_with: None,
            seen: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn failing(message: &str) -> Self {
        let mut tools = Self::new("");
        tools.fail_with = Some(message.to_string());
        tools
    }

    pub(super) fn seen(&self) -> Vec<ToolInvocation> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ToolProvider for RecordingTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.defs.clone()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        self.seen.lock().unwrap().push(invocation.clone());
        match &self.fail_with {
            Some(message) => Err(ToolProviderError::failed(&invocation.tool_name, message.clone())),
            None => Ok(self.reply.clone()),
        }
    }
}

/// A secret provider with one resolvable name.
struct OneSecret;

fn known(secret: &SecretRef) -> Result<(), SecretProviderError> {
    if secret.source == "env" && secret.name == "TOOL_TOKEN" {
        return Ok(());
    }
    Err(SecretProviderError::NotFound {
        provider: secret.source.clone(),
        name: secret.name.clone(),
    })
}

impl SecretProvider for OneSecret {
    fn has(&self, secret: &SecretRef) -> Result<(), SecretProviderError> {
        known(secret)
    }

    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError> {
        known(secret)?;
        Ok("s3cret-value-12345".to_string())
    }
}

/// The same one secret, counting the two questions separately.
///
/// "Was the value produced" is the property the ordering is about, and a count
/// is the only way to ask it: a test that only read the tool's arguments would
/// pass whether the value was made before the approval or after it.
struct CountingSecret {
    value: String,
    checks: std::sync::atomic::AtomicUsize,
    resolutions: std::sync::atomic::AtomicUsize,
}

impl CountingSecret {
    fn new(value: &str) -> Arc<Self> {
        Arc::new(Self {
            value: value.to_string(),
            checks: std::sync::atomic::AtomicUsize::new(0),
            resolutions: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn checks(&self) -> usize {
        self.checks.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn resolutions(&self) -> usize {
        self.resolutions.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl SecretProvider for CountingSecret {
    fn has(&self, secret: &SecretRef) -> Result<(), SecretProviderError> {
        self.checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        known(secret)
    }

    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError> {
        self.resolutions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        known(secret)?;
        Ok(self.value.clone())
    }
}

/// A provider that fails, quoting the value it failed on.
///
/// The shape a careless backend has: the message is built from the thing it
/// could not read. Nothing downstream may copy that message into a record.
struct FailingSecret;

impl SecretProvider for FailingSecret {
    fn has(&self, secret: &SecretRef) -> Result<(), SecretProviderError> {
        known(secret)
    }

    fn resolve(&self, _secret: &SecretRef) -> Result<String, SecretProviderError> {
        Err(SecretProviderError::Failed(
            "vault refused: the token was s3cret-value-12345".to_string(),
        ))
    }
}

/// A gateway over secret-using tools, with the ledger it writes to.
fn secret_harness(
    tools: Arc<RecordingTools>,
    secrets: Arc<dyn SecretProvider>,
) -> (GuardedToolProvider, Arc<FakeEventStore>) {
    let store = Arc::new(FakeEventStore::new());
    let gateway = GuardedToolProvider::new(
        tools,
        allowing(Capability::FS_READ),
        Arc::new(ApprovalService::new(store.clone())),
        ResourceGuard::new(workspace()),
        Arc::new(agent_testkit::InMemoryArtifactStore::default()),
    )
    .with_secrets(secrets)
    .with_permission_service(Arc::new(crate::permission::PermissionService::new(store.clone())));
    (gateway, store)
}

fn secret_gateway(tools: Arc<RecordingTools>) -> GuardedToolProvider {
    secret_harness(tools, Arc::new(OneSecret)).0
}

fn secret_invocation(arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation::proposed("fs.read", arguments)
}

/// Every payload of one event type, in the order the log holds them.
async fn payloads(store: &Arc<FakeEventStore>, event_type: &str) -> Vec<serde_json::Value> {
    let events = store.read_stream(&sid(), EventSeq::ZERO, usize::MAX).await.unwrap();
    events
        .iter()
        .filter(|stored| stored.event.event_type.as_str() == event_type)
        .map(|stored| stored.event.payload.clone())
        .collect()
}

/// Every payload in the log as one string, for "did this reach the log at all".
async fn log_text(store: &Arc<FakeEventStore>) -> String {
    let events = store.read_stream(&sid(), EventSeq::ZERO, usize::MAX).await.unwrap();
    events.iter().map(|stored| stored.event.payload.to_string()).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn a_secret_reference_is_resolved_at_the_tool_boundary_and_scrubbed_from_the_result() {
    let tools = Arc::new(RecordingTools::new("the token is s3cret-value-12345"));
    let gateway = secret_gateway(tools.clone());
    let invocation = secret_invocation(serde_json::json!({
        "path": "notes.txt",
        "command": "echo 'Bearer secret://env/TOOL_TOKEN'",
    }));

    // The call is a secret-using one, so it stops for approval first.
    let pending = gateway.execute(&sid(), &invocation).await.unwrap_err();
    assert!(matches!(pending, ToolGatewayError::ApprovalPending { .. }), "{pending:?}");
    assert!(tools.seen().is_empty(), "nothing may run before the approval");

    // What the log and the summary hold is the reference, untouched.
    assert_eq!(invocation.arguments["command"], "echo 'Bearer secret://env/TOOL_TOKEN'");

    let approval = approve_once(&gateway, pending).await;
    let result = gateway.execute_approved(&sid(), &approval, &invocation).await.unwrap();
    let seen = tools.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].arguments["command"], "echo 'Bearer s3cret-value-12345'");
    assert!(!result.content.contains("s3cret-value"), "secret leaked: {}", result.content);
    assert!(result.content.contains("***"));
    assert!(
        result
            .redaction_metadata
            .patterns
            .iter()
            .any(|pattern| pattern == "secret://env/TOOL_TOKEN")
    );
}

#[tokio::test]
async fn an_unapproved_secret_reference_stops_before_resolution() {
    // The ordering this whole file is about: what the gateway learns before a
    // person answers is *whether* the reference resolves, never what it is.
    let secrets = CountingSecret::new("s3cret-value-12345");
    let tools = Arc::new(RecordingTools::new("ok"));
    let (gateway, _store) = secret_harness(tools.clone(), secrets.clone());
    let invocation =
        secret_invocation(serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" }));

    let pending = gateway.execute(&sid(), &invocation).await.unwrap_err();
    assert!(matches!(pending, ToolGatewayError::ApprovalPending { .. }), "{pending:?}");

    assert_eq!(secrets.checks(), 1, "the run asked whether the reference resolves");
    assert_eq!(secrets.resolutions(), 0, "the value existed before anyone agreed to the call");
    assert!(tools.seen().is_empty(), "nothing runs without the answer");
}

#[tokio::test]
async fn an_approved_secret_is_resolved_only_at_dispatch() {
    let secrets = CountingSecret::new("s3cret-value-12345");
    let tools = Arc::new(RecordingTools::new("done"));
    let (gateway, store) = secret_harness(tools.clone(), secrets.clone());
    let invocation =
        secret_invocation(serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" }));

    let pending = gateway.execute(&sid(), &invocation).await.unwrap_err();
    let approval = approve_once(&gateway, pending).await;
    assert_eq!(secrets.resolutions(), 0, "the approval is not the dispatch");

    gateway.execute_approved(&sid(), &approval, &invocation).await.unwrap();
    assert_eq!(secrets.resolutions(), 1, "one dispatch, one resolution");
    assert_eq!(tools.seen().len(), 1);

    // The dispatch says which action it was and what it used — by reference.
    let accesses = payloads(&store, EventType::SECRET_ACCESSED).await;
    assert_eq!(accesses.len(), 1, "{accesses:?}");
    let access = &accesses[0];
    assert_eq!(access["tool"], "fs.read");
    assert_eq!(access["references"], serde_json::json!(["secret://env/TOOL_TOKEN"]));
    assert_eq!(access["authorization_source"], "user_approval");
    let digest = access["action_digest"].as_str().expect("a digest");
    assert_eq!(digest.len(), 64, "the digest is the one the result is recorded under: {digest}");
}

#[tokio::test]
async fn an_approval_event_contains_the_reference_not_the_value() {
    let secrets = CountingSecret::new("s3cret-value-12345");
    let tools = Arc::new(RecordingTools::new("ok"));
    let (gateway, store) = secret_harness(tools, secrets);
    let invocation =
        secret_invocation(serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" }));

    let _pending = gateway.execute(&sid(), &invocation).await.unwrap_err();

    let requests = payloads(&store, EventType::APPROVAL_REQUESTED).await;
    assert_eq!(requests.len(), 1);
    let text = requests[0].to_string();
    assert!(text.contains("secret://env/TOOL_TOKEN"), "the card names the reference: {text}");
    assert!(!text.contains("s3cret-value"), "the value reached the approval: {text}");

    let log = log_text(&store).await;
    assert!(!log.contains("s3cret-value"), "the value reached the log: {log}");
}

#[tokio::test]
async fn a_failed_secret_resolution_does_not_leave_a_plaintext_event() {
    // The provider's own message quotes the value. A refusal that copied it
    // would reach the model and `tool/failed`, so the message is dropped — and
    // nothing is written under the action, because nothing ran under it.
    let tools = Arc::new(RecordingTools::new("ok"));
    let (gateway, store) = secret_harness(tools.clone(), Arc::new(FailingSecret));
    let invocation =
        secret_invocation(serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" }));

    // `has` says yes, so the call is one a person is asked about; the failure
    // is at dispatch, which is where a provider that cannot answer honestly
    // lands.
    assert!(
        FailingSecret
            .has(&SecretRef { source: "env".to_string(), name: "TOOL_TOKEN".to_string() })
            .is_ok()
    );
    let approval = granted_for(&gateway, &invocation).await;
    let error = gateway.execute_approved(&sid(), &approval, &invocation).await.unwrap_err();

    let text = error.to_string();
    assert!(!text.contains("s3cret-value"), "the refusal echoed the value: {text}");
    assert!(text.contains("secret://env/TOOL_TOKEN"), "{text}");
    assert!(tools.seen().is_empty(), "the tool never saw a call that could not run");
    assert!(payloads(&store, EventType::SECRET_ACCESSED).await.is_empty(), "no access happened");
    let log = log_text(&store).await;
    assert!(!log.contains("s3cret-value"), "a plaintext reached the log: {log}");
}

#[tokio::test]
async fn a_secret_reference_cannot_be_reused_for_a_different_action_digest() {
    // The answer is bound to the fingerprint: one argument's worth of change is
    // a different call, and a different call is one nobody answered. The
    // refusal happens before the value is produced.
    let secrets = CountingSecret::new("s3cret-value-12345");
    let tools = Arc::new(RecordingTools::new("ok"));
    let (gateway, _store) = secret_harness(tools.clone(), secrets.clone());
    let named = secret_invocation(serde_json::json!({
        "path": "notes.txt",
        "command": "echo secret://env/TOOL_TOKEN",
    }));
    let pending = gateway.execute(&sid(), &named).await.unwrap_err();
    let approval = approve_once(&gateway, pending).await;

    let other = secret_invocation(serde_json::json!({
        "path": "elsewhere.txt",
        "command": "echo secret://env/TOOL_TOKEN",
    }));
    let error = gateway.execute_approved(&sid(), &approval, &other).await.unwrap_err();
    assert!(matches!(error, ToolGatewayError::PolicyDenied { .. }), "{error:?}");
    assert_eq!(secrets.resolutions(), 0, "a refused resume must not produce the value");
    assert!(tools.seen().is_empty());
}

#[tokio::test]
async fn an_unknown_secret_is_refused_before_the_tool_runs() {
    let tools = Arc::new(RecordingTools::new("ok"));
    let gateway = secret_gateway(tools.clone());

    let error = gateway
        .execute(
            &sid(),
            &secret_invocation(serde_json::json!({ "command": "echo secret://env/NOPE" })),
        )
        .await
        .unwrap_err();
    match error {
        ToolGatewayError::SecretUnavailable { reference, kind, .. } => {
            assert_eq!(reference, "secret://env/NOPE");
            assert_eq!(kind, ToolErrorKind::PermissionDenied);
        }
        other => panic!("expected a secret refusal, got {other:?}"),
    }
    assert!(tools.seen().is_empty(), "an unresolvable reference must not reach the tool");
}

#[tokio::test]
async fn a_malformed_secret_reference_is_refused() {
    let tools = Arc::new(RecordingTools::new("ok"));
    let gateway = secret_gateway(tools.clone());

    let error = gateway
        .execute(
            &sid(),
            &secret_invocation(serde_json::json!({ "command": "echo secret://TOOL_TOKEN" })),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ToolGatewayError::SecretUnavailable { kind: ToolErrorKind::InvalidArguments, .. }
        ),
        "{error:?}"
    );
    assert!(tools.seen().is_empty());
}

#[tokio::test]
async fn a_run_without_a_secret_provider_refuses_a_reference() {
    let tools = Arc::new(RecordingTools::new("ok"));
    let store = Arc::new(FakeEventStore::new());
    let gateway = GuardedToolProvider::new(
        tools.clone(),
        allowing(Capability::FS_READ),
        Arc::new(ApprovalService::new(store)),
        ResourceGuard::new(workspace()),
        Arc::new(agent_testkit::InMemoryArtifactStore::default()),
    );

    let error = gateway
        .execute(
            &sid(),
            &secret_invocation(serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" })),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ToolGatewayError::SecretUnavailable { kind: ToolErrorKind::PermissionDenied, .. }
        ),
        "{error:?}"
    );
    assert!(tools.seen().is_empty());
}

#[tokio::test]
async fn a_provider_error_that_echoes_the_secret_is_scrubbed() {
    let tools = Arc::new(RecordingTools::failing("rejected header: Bearer s3cret-value-12345"));
    let gateway = secret_gateway(tools);

    let invocation =
        secret_invocation(serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" }));
    let approval = granted_for(&gateway, &invocation).await;
    let error = gateway.execute_approved(&sid(), &approval, &invocation).await.unwrap_err();
    let text = error.to_string();
    assert!(!text.contains("s3cret-value"), "secret leaked: {text}");
    assert!(text.contains("***"));
}

/// The scrub runs before the offload, so an oversized result carries `***`
/// into the blob store rather than the value it replaced.
///
/// `finalize` redacts, then offloads — and until this test nothing pinned
/// that order. Reverse it and the result the model sees is clean while the
/// artifact keeps the plaintext: the model never notices, the log never
/// notices, and the secret sits on disk under a content hash. The artifact
/// is a durable surface, so the audit's "a secret does not appear in an
/// artifact" is checked on the bytes, not on what the caller was handed.
#[tokio::test]
async fn a_resolved_secret_is_scrubbed_before_the_result_becomes_an_artifact() {
    const SECRET: &str = "s3cret-value-12345";
    // Past the offload limit, with the value the tool echoes inside it.
    let mut reply = "echoed line\n".repeat(6_000);
    reply.push_str("the token is s3cret-value-12345\n");
    reply.push_str(&"trailing line\n".repeat(6_000));
    assert!(reply.len() > OFFLOAD_MAX_BYTES, "the premise: this is offloaded");

    let tools = Arc::new(RecordingTools::new(&reply));
    let artifacts = Arc::new(agent_testkit::InMemoryArtifactStore::default());
    let gateway = GuardedToolProvider::new(
        tools,
        allowing(Capability::FS_READ),
        Arc::new(ApprovalService::new(Arc::new(FakeEventStore::new()))),
        ResourceGuard::new(workspace()),
        artifacts.clone(),
    )
    .with_secrets(Arc::new(OneSecret));
    let invocation = secret_invocation(serde_json::json!({
        "path": "notes.txt",
        "command": "echo 'Bearer secret://env/TOOL_TOKEN'",
    }));

    // A secret-using call stops for approval; nothing runs before it.
    let pending = gateway.execute(&sid(), &invocation).await.unwrap_err();
    assert!(matches!(pending, ToolGatewayError::ApprovalPending { .. }), "{pending:?}");

    let approval = approve_once(&gateway, pending).await;
    let result = gateway.execute_approved(&sid(), &approval, &invocation).await.unwrap();
    assert_eq!(result.artifacts.len(), 1, "the full output was offloaded");
    assert!(
        !result.content.contains(SECRET),
        "the preview the model keeps still has the value: {}",
        result.content
    );

    let stored = artifacts.get(&result.artifacts[0]).expect("read the blob back");
    let stored = String::from_utf8(stored).expect("the artifact is text");
    assert!(!stored.contains(SECRET), "the resolved secret landed in the artifact store");
    assert!(stored.contains("***"), "the blob holds the scrubbed text");

    // What the metadata names is the reference, never the value, and the
    // metadata is what a summary or an export reads.
    assert!(
        result
            .redaction_metadata
            .patterns
            .iter()
            .any(|pattern| pattern == "secret://env/TOOL_TOKEN")
    );
    let metadata = serde_json::to_string(&result.redaction_metadata).unwrap();
    assert!(!metadata.contains(SECRET), "the value reached the metadata: {metadata}");
}

#[tokio::test]
async fn a_high_entropy_token_in_a_result_is_reported_but_not_removed() {
    // No known prefix: the value is only recognisable by shape, which is
    // exactly the case the prefix list cannot see.
    const TOKEN: &str = "Zk3vQ9mR7tY1pL5nB8cF2hJ6sD4gW0xE7uI9oP1a";
    let tools = Arc::new(RecordingTools::new(&format!("bearer {TOKEN}")));
    let gateway =
        gateway_any(ResourceGuard::new(workspace()), tools, allowing(Capability::FS_READ));

    let result = gateway.execute(&sid(), &invocation("fs.read")).await.expect("allow runs it");

    // The default layer reports; it does not edit an answer the caller did not
    // ask to have rewritten.
    assert_eq!(result.content, format!("bearer {TOKEN}"));
    assert!(!result.redaction_metadata.redacted);
    assert_eq!(result.redaction_metadata.warnings, vec!["high_entropy".to_string()]);
    let encoded = serde_json::to_string(&result.redaction_metadata).unwrap();
    assert!(!encoded.contains(TOKEN), "the value reached the metadata: {encoded}");
}
