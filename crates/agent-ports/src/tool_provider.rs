//! ToolProvider trait — adapter for tool backends.
//!
//! The tool protocol types (`ToolDefinition`, `ToolInvocation`, `ToolResult`,
//! `ToolSideEffect`, `ToolStatus`, `ToolSource`) live in `agent_protocol::tool`;
//! they are re-exported here for the trait's public surface.

pub use agent_protocol::tool::{
    ToolDefinition, ToolInvocation, ToolResult, ToolSideEffect, ToolSource, ToolStatus,
};

/// Port for contributing tool execution.
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync {
    /// The tools this provider contributes, with their capability declarations.
    /// Providers with no declarative surface may return an empty list.
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    /// The side-effect class of a tool, if known. Callers must treat an unknown
    /// tool as `Destructive` (fail closed).
    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect>;

    /// The argument names that may hold a filesystem path for `tool_name`.
    ///
    /// The resource boundary has to see the path before the tool runs, but an
    /// invocation is arbitrary JSON: the engine cannot tell which string is a
    /// path without knowing the tool. Deriving it from the schema would mark
    /// types, not meanings, so the provider — the layer that wrote the tool —
    /// declares it here (K-08).
    ///
    /// The default is the convention the built-in tools follow. A tool that
    /// names its path argument something else (`file`, `dir`, `target`) must
    /// override this, or the boundary never sees it. An empty list is a
    /// declaration that the tool takes no path.
    fn path_arguments(&self, tool_name: &str) -> Vec<String> {
        let _ = tool_name;
        vec!["path".to_string()]
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError>;
}

/// Errors from a ToolProvider implementation.
///
/// Neither variant means "the call produced no result" the way a gateway
/// refusal does — a provider is only reached after the gateway admitted the
/// call. `Failed` is the tool's own answer, and `UnknownTool` reports that the
/// surface the gateway admitted and the surface this provider implements
/// disagree.
#[derive(Debug, thiserror::Error)]
pub enum ToolProviderError {
    #[error("tool '{tool}' failed: {message}")]
    Failed { tool: String, message: String },
    /// A name this provider does not implement, and what it implements instead.
    ///
    /// The gateway has already established that the name is registered, so this
    /// is not a caller's typo — it is a provider that fell short of the surface
    /// it was registered under, and the fix is in the wiring. Naming what this
    /// provider *does* implement is what tells that apart from a misspelling;
    /// "unknown tool" alone reads identically for both.
    #[error("tool '{tool}' is not implemented here; {}", offered(available))]
    UnknownTool { tool: String, available: Vec<String> },
}

/// Render a provider's surface for a refusal, in the same voice the gateway
/// uses one layer up: what there is, not merely that what was asked for is not.
fn offered(available: &[String]) -> String {
    if available.is_empty() {
        return "this provider declares no tools".to_string();
    }
    format!("this provider offers: {}", available.join(", "))
}
