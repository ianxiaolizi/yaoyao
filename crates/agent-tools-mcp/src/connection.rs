//! The seam between this adapter's logic and a live MCP session.
//!
//! Above this trait are the decisions this crate owns and can be tested for
//! without a server: namespacing, side-effect classification, last-good caching,
//! reconnection. Below it is `rmcp` and somebody else's process. Splitting there
//! is what lets the mapping rules be unit-tested against a fake connection while
//! one end-to-end test drives a real stdio child.

use std::sync::Arc;

use agent_ports::tool_provider::ToolResult;
use serde_json::Value;

/// The annotation hints an MCP server may attach to a tool.
///
/// The MCP spec is explicit that these are *hints* and that a client must not
/// make tool-use decisions on them from an untrusted server. They are still the
/// only classification signal a server gives, so they are read as a first
/// approximation and the policy engine — which sees the real arguments — keeps
/// the final say. Kept as a local DTO rather than `rmcp::model::ToolAnnotations`
/// so the classification rule is a function of this crate, not of the SDK's
/// shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpAnnotations {
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
    pub idempotent: Option<bool>,
    pub open_world: Option<bool>,
}

/// One tool as an MCP server describes it, before namespacing.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// The name the server knows this tool by. Namespacing happens above this
    /// type; the value here is what goes back on the wire when the tool runs.
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub annotations: McpAnnotations,
}

/// A live session with one MCP server.
#[async_trait::async_trait]
pub trait McpConnection: Send + Sync {
    /// The server's current tool list.
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError>;

    /// Call `name` — the server's own name, not the namespaced one.
    ///
    /// Not retried by the caller: a tool call may have side effects, and a
    /// failed call is not evidence that nothing happened on the far side.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult, McpError>;
}

/// Opens sessions.
///
/// Held by the provider instead of a fixed connection so that an outage can be
/// recovered from without rebuilding the provider or losing its last-good tool
/// set.
///
/// Nothing here names the server. A failed `connect` is an
/// [`McpError::Unreachable`], which already carries the name, and the provider
/// holds one too — a third accessor for the same string would be one more thing
/// that has to stay in step, and nothing was reading it (YA-47).
#[async_trait::async_trait]
pub trait McpConnector: Send + Sync {
    async fn connect(&self) -> Result<Arc<dyn McpConnection>, McpError>;
}

/// Errors from talking to an MCP server.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("mcp server '{server}' is unreachable: {message}")]
    Unreachable { server: String, message: String },

    #[error("mcp server '{server}' failed while calling tool '{tool}': {message}")]
    ToolCall { server: String, tool: String, message: String },

    /// Two tools under one server resolved to the same namespaced name.
    ///
    /// The suffix that disambiguates is derived and finite, so a repeat is
    /// possible in principle; silently overwriting one tool with the other
    /// would remove it from the surface with no error anywhere.
    #[error("mcp server '{server}' listed two tools that both resolve to '{tool}'")]
    NameCollision { server: String, tool: String },
}
