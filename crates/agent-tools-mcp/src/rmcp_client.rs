//! The `rmcp`-backed side of the adapter: real sessions over stdio and
//! Streamable HTTP.
//!
//! This is the only place in the workspace that names `rmcp`. Everything a
//! caller above it sees is [`McpConnection`] and [`McpConnector`], so swapping
//! the SDK out — or the protocol version under it — reaches no other file.
//!
//! The handshake is pinned to protocol `2025-11-25`. `rmcp`'s `LATEST` is now
//! `2026-07-28`, whose stateless core reworks the connection lifecycle; pinning
//! to the last version that still opens with `initialize` keeps this adapter on
//! the handshake every existing server answers. Moving to `2026-07-28` is a
//! deliberate follow-up, not something to drift into by taking an SDK default.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use agent_ports::artifact_store::ArtifactStore;
use agent_ports::process_env::child_environment_with;
use agent_ports::tool_provider::ToolResult;
use agent_protocol::permissions::ArtifactRef;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderName, HeaderValue};
use rmcp::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ContentBlock, Implementation,
    ProtocolVersion, ResourceContents, ToolAnnotations,
};
use rmcp::service::{NotificationContext, RoleClient, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Notify;

use crate::connection::{McpAnnotations, McpConnection, McpConnector, McpError, McpTool};

/// The protocol version this adapter speaks (see the module note).
pub const PINNED_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

/// How to launch a stdio MCP server.
#[derive(Debug, Clone)]
pub struct StdioSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
}

/// How to reach a Streamable HTTP MCP server.
#[derive(Debug, Clone)]
pub struct HttpSpec {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// The transport a server is reached over.
#[derive(Debug, Clone)]
pub enum TransportSpec {
    Stdio(StdioSpec),
    Http(HttpSpec),
}

/// The environment a stdio server is started with.
///
/// The allowlist plus this one server's configured variables; the agent's own
/// `AGENT_*` names are refused even when configured (`agent_ports::process_env`
/// says why).
fn stdio_environment(spec: &StdioSpec) -> Vec<(OsString, OsString)> {
    let explicit: Vec<(OsString, OsString)> =
        spec.env.iter().map(|(key, value)| (key.into(), value.into())).collect();
    child_environment_with(&std::env::vars_os().collect::<Vec<_>>(), &explicit)
}

/// The client identity and the list-changed pump, handed to `rmcp`.
struct Client {
    info: ClientConfig,
    changed: Arc<Notify>,
}

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        // A permit, not a wake-up-now: the notification can arrive while a
        // re-sync is already running, and the permit means the change is not
        // lost because nobody was listening at that instant.
        self.changed.notify_one();
    }
}

fn client_config() -> ClientConfig {
    // The version we announce to an MCP server in the `initialize` handshake.
    // This is the *client build*, not the event-log schema
    // (`agent_protocol::ids::SchemaVersion::CURRENT`): an MCP server has no
    // interest in the shape of our session log, and the two versions move for
    // unrelated reasons even though they coincide today.
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("yaoyao-agent", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(PINNED_PROTOCOL_VERSION)
}

/// A live session with one server.
pub struct RmcpConnection {
    server: String,
    service: RunningService<RoleClient, Client>,
    /// Where a block that has no text form is put. Held by the connection
    /// rather than passed to `call_tool`, because the conversion happens here
    /// and a caller above knows nothing about MCP content blocks.
    artifacts: Arc<dyn ArtifactStore>,
}

impl RmcpConnection {
    fn tool_from_rmcp(tool: rmcp::model::Tool) -> McpTool {
        McpTool {
            name: tool.name.to_string(),
            description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
            annotations: tool.annotations.as_ref().map(annotations_from_rmcp).unwrap_or_default(),
        }
    }
}

fn annotations_from_rmcp(annotations: &ToolAnnotations) -> McpAnnotations {
    McpAnnotations {
        read_only: annotations.read_only_hint,
        destructive: annotations.destructive_hint,
        idempotent: annotations.idempotent_hint,
        open_world: annotations.open_world_hint,
    }
}

/// One content block's contribution: the text it becomes, and whether output
/// reached this adapter and did not reach the model.
struct Carried {
    text: String,
    lost: bool,
    /// Set when the text is a locator rather than the content itself. The
    /// reference travels with the result as well as with the sentence: the
    /// locator tells the model where to ask, and the reference is what the
    /// loop records and what `agent export` lists. A marker with no reference
    /// behind it would name a blob nothing had admitted to storing.
    artifact: Option<ArtifactRef>,
}

fn carried(text: String) -> Carried {
    Carried { text, lost: false, artifact: None }
}

/// Output that was in hand and could not be carried onward. This is what
/// `ToolResult::truncated` is for, and the only thing that sets it here.
fn lost(text: String) -> Carried {
    Carried { text, lost: true, artifact: None }
}

/// Turn one MCP content block into text the model can read.
///
/// Every block the framework can carry as text is carried as text. The ones it
/// cannot — image and audio payloads, which arrive base64-encoded and have no
/// place in the `String` field a model reads — are decoded into the artifact
/// store and named there by a locator, so the model is told that something is
/// there and how to ask for it. That sentence is the point of this function:
/// before it, a picture came back as the word `[image]` and nothing anywhere
/// said the model was missing one.
fn carry_block(block: &ContentBlock, artifacts: &dyn ArtifactStore) -> Carried {
    match block {
        ContentBlock::Text(text) => carried(text.text.clone()),
        ContentBlock::Image(image) => {
            carry_binary(artifacts, &image.data, &image.mime_type, "image")
        }
        ContentBlock::Audio(audio) => {
            carry_binary(artifacts, &audio.data, &audio.mime_type, "audio")
        }
        // A resource is text or it is a blob, and rmcp carries `get_text` for
        // the first case because a server answering with a file's contents as
        // an embedded text resource is ordinary. Collapsing both arms to one
        // placeholder threw away text that was already in hand.
        ContentBlock::Resource(resource) => match &resource.resource {
            ResourceContents::TextResourceContents { uri, text, .. } => {
                carried(format!("[{uri}]\n{text}"))
            }
            ResourceContents::BlobResourceContents { uri, mime_type, blob, .. } => {
                // A blob that did not say what it is is still a blob; the
                // generic type is what the store records rather than an empty
                // mime, which would claim the server said something.
                let mut carried = carry_binary(
                    artifacts,
                    blob,
                    mime_type.as_deref().unwrap_or("application/octet-stream"),
                    "resource",
                );
                carried.text = format!("[{uri}] {}", carried.text);
                carried
            }
            // `ResourceContents` is `#[non_exhaustive]` like `ContentBlock`,
            // for the same reason and with the same answer.
            _ => lost("[resource of a kind this adapter cannot carry]".to_string()),
        },
        // The link *is* the content: a name to call it by, and the uri to go
        // get it with. Dropping the uri left the model with nothing it could
        // act on.
        ContentBlock::ResourceLink(link) => {
            carried(format!("[resource {}: {}]", link.name, link.uri))
        }
        // `ContentBlock` is `#[non_exhaustive]`, so a block a future SDK adds is
        // a block this adapter cannot carry. Saying so beats a silent blank —
        // and the block is not printed: it may hold a payload's worth of
        // base64, and the point is that it did not fit, not what was in it.
        _ => lost("[content block of a kind this adapter cannot carry]".to_string()),
    }
}

/// Put a base64 payload in the artifact store and describe where it went.
fn carry_binary(artifacts: &dyn ArtifactStore, data: &str, mime_type: &str, kind: &str) -> Carried {
    let Ok(bytes) = STANDARD.decode(data) else {
        // The server sent something that is not base64. Nothing to store and
        // nothing to show; the model is told the shape was wrong.
        return lost(format!("[{kind} {mime_type}, not base64-decodable]"));
    };
    match artifacts.put(&bytes, mime_type) {
        Ok(artifact) => Carried {
            text: format!(
                "[{kind} {mime_type}, {} bytes, saved to {}]",
                artifact.size,
                artifact.locator()
            ),
            lost: false,
            artifact: Some(artifact),
        },
        // Best effort, for the same reason the kernel's offload is: the call
        // ran and its output is in hand, so a storage problem is not a failed
        // tool. The size still goes out — it is the one fact left.
        Err(error) => {
            lost(format!("[{kind} {mime_type}, {} bytes, not saved: {error}]", bytes.len()))
        }
    }
}

fn to_tool_result(
    result: rmcp::model::CallToolResult,
    artifacts: &dyn ArtifactStore,
) -> ToolResult {
    let mut parts: Vec<String> = Vec::new();
    let mut references: Vec<ArtifactRef> = Vec::new();
    let mut lost_anything = false;
    for block in &result.content {
        let carried = carry_block(block, artifacts);
        lost_anything |= carried.lost;
        references.extend(carried.artifact);
        parts.push(carried.text);
    }

    // A server may answer with structured content and no text blocks; a tool
    // result that carries neither is still a result, just an empty one.
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(structured.to_string());
    }

    let content = parts.join("\n");
    let mut outcome = if result.is_error.unwrap_or(false) {
        ToolResult::failed(content)
    } else {
        ToolResult::completed(content)
    };
    outcome.artifacts = references;
    if lost_anything { outcome.truncated() } else { outcome }
}

#[async_trait::async_trait]
impl McpConnection for RmcpConnection {
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let tools = self.service.list_all_tools().await.map_err(|error| McpError::Unreachable {
            server: self.server.clone(),
            message: error.to_string(),
        })?;
        Ok(tools.into_iter().map(RmcpConnection::tool_from_rmcp).collect())
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult, McpError> {
        let mut params = CallToolRequestParams::new(name.to_string());
        // MCP takes an object or nothing; a non-object argument is not a shape
        // this protocol expresses, so it is passed as no arguments rather than
        // coerced into one.
        if let Value::Object(map) = arguments {
            params.arguments = Some(map);
        }

        // No deadline is armed here on purpose. How long a call may run is a
        // configured number, and this layer has no view of the configuration;
        // the deadline is armed one layer up, in `McpToolProvider::invoke`,
        // where the configured value is in hand. Two clocks would mean the
        // shorter one silently overrides the other.
        let result = self.service.call_tool(params).await.map_err(|error| McpError::ToolCall {
            server: self.server.clone(),
            tool: name.to_string(),
            message: error.to_string(),
        })?;

        Ok(to_tool_result(result, self.artifacts.as_ref()))
    }
}

/// Opens real sessions for one server, over whichever transport it named.
pub struct RmcpConnector {
    server: String,
    transport: TransportSpec,
    changed: Arc<Notify>,
    artifacts: Arc<dyn ArtifactStore>,
}

impl RmcpConnector {
    /// A connector for one server, over one transport, with the store a
    /// non-text result block is offloaded to.
    ///
    /// The store is handed in rather than built here because it is the run's,
    /// not this server's: a blob saved by one server's tool is the same kind of
    /// thing as a blob saved by the kernel's own offload, and two stores would
    /// mean two answer sets for "where did that output go?".
    pub fn new(
        server: impl Into<String>,
        transport: TransportSpec,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            server: server.into(),
            transport,
            changed: Arc::new(Notify::new()),
            artifacts,
        })
    }

    /// The signal this server's `tools/list_changed` notifications arrive on.
    ///
    /// Handed out rather than passed in, because it is one handle shared by two
    /// halves of one thing: the client fires it when the server says its list
    /// changed, and the provider's re-sync pump waits on it. Neither owns it,
    /// and a caller that had to build it would be building the seam.
    pub fn tool_list_changed(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    fn unreachable(&self, error: impl std::fmt::Display) -> McpError {
        McpError::Unreachable { server: self.server.clone(), message: error.to_string() }
    }
}

#[async_trait::async_trait]
impl McpConnector for RmcpConnector {
    async fn connect(&self) -> Result<Arc<dyn McpConnection>, McpError> {
        let client = Client { info: client_config(), changed: self.changed.clone() };

        match &self.transport {
            TransportSpec::Stdio(spec) => {
                let env = stdio_environment(spec);
                let command = Command::new(&spec.command).configure(|command| {
                    command.args(&spec.args).env_clear().envs(env);
                    if let Some(cwd) = &spec.cwd {
                        command.current_dir(cwd);
                    }
                    // The server is this run's child, not a service of its
                    // own: when the connection is dropped the process goes
                    // with it. Without this a refused assembly, or any exit
                    // path that drops the stack without a shutdown call, would
                    // leave an MCP server running with nothing on the other
                    // end of its pipes (AA-07). The subagent adapter makes the
                    // same promise for the same reason.
                    command.kill_on_drop(true);
                });
                let transport = TokioChildProcess::new(command).map_err(|e| self.unreachable(e))?;
                let service = client.serve(transport).await.map_err(|e| self.unreachable(e))?;
                Ok(Arc::new(RmcpConnection {
                    server: self.server.clone(),
                    service,
                    artifacts: self.artifacts.clone(),
                }))
            }
            TransportSpec::Http(spec) => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(spec.url.clone());
                if !spec.headers.is_empty() {
                    let mut headers = HashMap::new();
                    for (name, value) in &spec.headers {
                        let name = HeaderName::from_bytes(name.as_bytes())
                            .map_err(|e| self.unreachable(e))?;
                        let value =
                            HeaderValue::from_str(value).map_err(|e| self.unreachable(e))?;
                        headers.insert(name, value);
                    }
                    config = config.custom_headers(headers);
                }
                let transport = StreamableHttpClientTransport::from_config(config);
                let service = client.serve(transport).await.map_err(|e| self.unreachable(e))?;
                Ok(Arc::new(RmcpConnection {
                    server: self.server.clone(),
                    service,
                    artifacts: self.artifacts.clone(),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::artifact_store::ArtifactStoreError;
    use agent_ports::tool_provider::ToolStatus;
    use agent_testkit::InMemoryArtifactStore;
    use serde_json::json;

    fn store() -> InMemoryArtifactStore {
        InMemoryArtifactStore::default()
    }

    #[test]
    fn a_stdio_server_is_given_its_own_variables_and_not_the_agents() {
        let spec = StdioSpec {
            command: "server".to_string(),
            args: Vec::new(),
            env: vec![
                ("SERVER_MODE".to_string(), "test".to_string()),
                ("AGENT_MODEL_API_KEY".to_string(), "sk-live-secret".to_string()),
            ],
            cwd: None,
        };

        let env = stdio_environment(&spec);
        let names: Vec<String> =
            env.iter().map(|(name, _)| name.to_string_lossy().into_owned()).collect();

        assert!(names.iter().any(|name| name == "SERVER_MODE"));
        assert!(
            !names.iter().any(|name| name == "AGENT_MODEL_API_KEY"),
            "a configured server was handed the agent's key: {names:?}"
        );
    }

    /// A store that refuses everything, for the arms where saving fails. The
    /// same stand-in the kernel's offload tests use.
    struct UnavailableArtifacts;

    impl ArtifactStore for UnavailableArtifacts {
        fn put(&self, _: &[u8], _: &str) -> Result<ArtifactRef, ArtifactStoreError> {
            Err(ArtifactStoreError::NotFound("no store".to_string()))
        }

        fn get(&self, _: &ArtifactRef) -> Result<Vec<u8>, ArtifactStoreError> {
            Err(ArtifactStoreError::NotFound("no store".to_string()))
        }
    }

    #[test]
    fn annotations_are_read_from_the_rmcp_shape() {
        let rmcp = ToolAnnotations::default().read_only(true).destructive(false).idempotent(true);
        let annotations = annotations_from_rmcp(&rmcp);
        assert_eq!(annotations.read_only, Some(true));
        assert_eq!(annotations.destructive, Some(false));
        assert_eq!(annotations.idempotent, Some(true));
        assert_eq!(annotations.open_world, None);
    }

    #[test]
    fn the_pinned_protocol_version_is_the_last_with_initialize() {
        // The notes above this constant make claims about the SDK; this is
        // where they are checked rather than believed.
        assert_eq!(PINNED_PROTOCOL_VERSION, ProtocolVersion::V_2025_11_25);
        assert_eq!(PINNED_PROTOCOL_VERSION.as_str(), "2025-11-25");
        assert!(PINNED_PROTOCOL_VERSION.has_initialize());

        // "The last version that still opens with `initialize`" is derived from
        // the SDK's own table rather than trusted to any one constant — the same
        // derivation the SDK's guard for its own `LATEST_WITH_INITIALIZE` uses,
        // so a later version that still answers the handshake is what this
        // notices. The negation is the other half of the claim, and unlike the
        // derivation it also holds for a `LATEST` that is not in the table.
        let newest_with_initialize = ProtocolVersion::KNOWN_VERSIONS
            .iter()
            .filter(|version| version.has_initialize())
            .max_by(|left, right| left.as_str().cmp(right.as_str()))
            .expect("the SDK knows a version that opens with `initialize`");
        assert_eq!(newest_with_initialize, &PINNED_PROTOCOL_VERSION);
        assert!(!ProtocolVersion::LATEST.has_initialize());

        // The module note says `LATEST` is `2026-07-28`; that is the whole
        // reason for the pin, so it is checked here too. A ratchet on purpose:
        // it goes red when an upgrade moves `LATEST`, which is when the
        // sentence — and the decision under it — needs rereading.
        assert_eq!(ProtocolVersion::LATEST.as_str(), "2026-07-28");
    }

    #[test]
    fn a_text_call_result_becomes_completed_and_an_error_becomes_failed() {
        let ok = rmcp::model::CallToolResult::success(vec![ContentBlock::text("hello")]);
        let result = to_tool_result(ok, &store());
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.content, "hello");
        // Nothing was dropped on the way, so nothing says output was lost.
        assert!(!result.truncated);
        assert!(result.artifacts.is_empty());

        let bad = rmcp::model::CallToolResult::error(vec![ContentBlock::text("nope")]);
        let result = to_tool_result(bad, &store());
        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(result.content, "nope");
    }

    #[test]
    fn structured_content_is_used_when_there_are_no_text_blocks() {
        let mut result = rmcp::model::CallToolResult::success(vec![]);
        result.structured_content = Some(json!({ "answer": 42 }));
        let result = to_tool_result(result, &store());
        assert_eq!(result.content, "{\"answer\":42}");
    }

    #[test]
    fn an_image_is_stored_and_the_content_names_where_it_went() {
        // The bytes an MCP image block carries are base64 on the wire; what
        // lands in the store is the image, not its encoding.
        let png = [0x89, 0x50, 0x4E, 0x47_u8];
        let store = store();
        let result = to_tool_result(
            rmcp::model::CallToolResult::success(vec![ContentBlock::image(
                STANDARD.encode(png),
                "image/png",
            )]),
            &store,
        );

        assert_eq!(result.artifacts.len(), 1, "{}", result.content);
        let reference = &result.artifacts[0];
        assert_eq!(reference.mime_type, "image/png");
        assert_eq!(reference.size, png.len() as u64);
        assert_eq!(store.get(reference).expect("the image was stored"), png.to_vec());
        // The model is told there is an image and how to ask for it — the whole
        // point of the fix. A bare `[image]` is what this replaces.
        assert!(result.content.contains("image/png"), "{}", result.content);
        assert!(result.content.contains(&reference.locator()), "{}", result.content);
        // Kept and referenced, so this is not truncation: nothing was lost.
        assert!(!result.truncated);
    }

    #[test]
    fn text_inside_an_embedded_resource_is_carried_rather_than_flattened() {
        // A server answering with a file's contents as a text resource is
        // ordinary, and the text is right here. This used to come back as the
        // word `[resource]`.
        let store = store();
        let result = to_tool_result(
            rmcp::model::CallToolResult::success(vec![ContentBlock::resource(
                ResourceContents::TextResourceContents {
                    uri: "file:///notes.md".to_string(),
                    mime_type: Some("text/markdown".to_string()),
                    text: "# hello".to_string(),
                    meta: None,
                },
            )]),
            &store,
        );

        assert!(result.content.contains("file:///notes.md"), "{}", result.content);
        assert!(result.content.contains("# hello"), "{}", result.content);
        assert!(result.artifacts.is_empty());
        assert!(!result.truncated);
    }

    #[test]
    fn a_blob_resource_is_stored_like_an_image() {
        let blob = [0x00, 0x01, 0x02_u8];
        let store = store();
        let result = to_tool_result(
            rmcp::model::CallToolResult::success(vec![ContentBlock::resource(
                ResourceContents::BlobResourceContents {
                    uri: "file:///data.bin".to_string(),
                    mime_type: Some("application/octet-stream".to_string()),
                    blob: STANDARD.encode(blob),
                    meta: None,
                },
            )]),
            &store,
        );

        assert_eq!(result.artifacts.len(), 1, "{}", result.content);
        assert_eq!(store.get(&result.artifacts[0]).expect("stored"), blob.to_vec());
        assert!(result.content.contains("file:///data.bin"), "{}", result.content);
    }

    #[test]
    fn a_resource_link_keeps_the_uri_it_is_the_only_reason_to_have() {
        let store = store();
        let result = to_tool_result(
            rmcp::model::CallToolResult::success(vec![ContentBlock::ResourceLink(
                rmcp::model::Resource::new("file:///readme.md", "readme"),
            )]),
            &store,
        );

        assert!(result.content.contains("readme"), "{}", result.content);
        assert!(result.content.contains("file:///readme.md"), "{}", result.content);
        assert!(!result.truncated);
    }

    #[test]
    fn a_payload_that_is_not_base64_is_reported_and_counts_as_lost() {
        let result = to_tool_result(
            rmcp::model::CallToolResult::success(vec![ContentBlock::image(
                "not base64 at all!!",
                "image/png",
            )]),
            &store(),
        );

        assert!(result.content.contains("not base64-decodable"), "{}", result.content);
        assert!(result.artifacts.is_empty());
        // Output reached this adapter and did not reach the model: that is the
        // fact `truncated` carries.
        assert!(result.truncated);
    }

    #[test]
    fn a_store_that_refuses_leaves_the_size_and_counts_as_lost() {
        let result = to_tool_result(
            rmcp::model::CallToolResult::success(vec![ContentBlock::image(
                STANDARD.encode([0x00_u8, 0x01, 0x02, 0x03, 0x04]),
                "image/png",
            )]),
            &UnavailableArtifacts,
        );

        assert!(result.content.contains("5 bytes"), "{}", result.content);
        assert!(result.content.contains("not saved"), "{}", result.content);
        // The call still succeeded — the tool ran and its output is in hand.
        // A storage problem is not a failed tool.
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.truncated);
    }

    #[test]
    fn one_lost_block_marks_the_whole_result_but_a_carried_one_does_not() {
        // Mixed content is the normal case, and the flag is about the result
        // the model receives, not about any one block.
        let both_kept = rmcp::model::CallToolResult::success(vec![
            ContentBlock::text("here it is"),
            ContentBlock::image(STANDARD.encode([1_u8, 2, 3]), "image/png"),
        ]);
        let result = to_tool_result(both_kept, &store());
        assert!(!result.truncated);
        assert!(result.content.starts_with("here it is\n"), "{}", result.content);

        let one_lost = rmcp::model::CallToolResult::success(vec![
            ContentBlock::text("here it is"),
            ContentBlock::image("!!not base64!!", "image/png"),
        ]);
        let result = to_tool_result(one_lost, &store());
        assert!(result.truncated);
        assert!(result.content.starts_with("here it is\n"), "{}", result.content);
    }
}
