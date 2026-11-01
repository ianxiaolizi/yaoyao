//! Tests for secret references crossing the gateway.
//!
//! Split out of `guarded.rs` when the approval work pushed that file past its
//! module-size ratchet. They are tests, not a second executor: every one drives
//! `GuardedToolProvider` and reads what it recorded, so they keep living beside
//! the fixtures they share with the rest of the gateway's tests.

use super::*;

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

impl SecretProvider for OneSecret {
    fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError> {
        if secret.source == "env" && secret.name == "TOOL_TOKEN" {
            Ok("s3cret-value-12345".to_string())
        } else {
            Err(SecretProviderError::NotFound {
                provider: secret.source.clone(),
                name: secret.name.clone(),
            })
        }
    }
}

fn secret_gateway(tools: Arc<RecordingTools>) -> GuardedToolProvider {
    let store = Arc::new(FakeEventStore::new());
    GuardedToolProvider::new(
        tools,
        allowing(Capability::FS_READ),
        Arc::new(ApprovalService::new(store)),
        ResourceGuard::new(workspace()),
        Arc::new(agent_testkit::InMemoryArtifactStore::default()),
    )
    .with_secrets(Arc::new(OneSecret))
}

fn secret_invocation(arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation::proposed("fs.read", arguments)
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

    // The default layer reports; it does not edit an answer the caller
    // did not ask to have rewritten.
    assert_eq!(result.content, format!("bearer {TOKEN}"));
    assert!(!result.redaction_metadata.redacted);
    assert_eq!(result.redaction_metadata.warnings, vec!["high_entropy".to_string()]);
    let encoded = serde_json::to_string(&result.redaction_metadata).unwrap();
    assert!(!encoded.contains(TOKEN), "the value reached the metadata: {encoded}");
}
