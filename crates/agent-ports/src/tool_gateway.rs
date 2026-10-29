//! ToolGateway trait — the guarded tool-execution seam.
//!
//! A [`crate::tool_provider::ToolProvider`] executes whatever it is asked; a
//! `ToolGateway` adds the policy, approval, resource, and redaction boundary
//! around it. It carries the session context (which the bare
//! `ToolProvider::invoke` lacks) so policy and approval can consult durable
//! session state.
//!
//! Errors here are decoupled from the kernel: the gateway implementation maps
//! its own errors (approval, resource guard) onto these variants rather than
//! leaking kernel types across the port. The one structured error that does
//! cross is the inner [`ToolProviderError`], carried whole — flattening it to a
//! string would erase the difference between a tool that ran and failed and a
//! call that never started, and that difference is the gate's own exit-code
//! distinction (C3.1).
//!
//! Offloading oversized output to an artifact store is **best effort and is
//! not a gateway error**. The tool has already run and its result is in hand;
//! if storage fails, the full text stays inline and the only cost is context.
//! There is deliberately no "artifact store failed" variant here: an error
//! would mark a successful call failed and throw away the answer.

use crate::tool_provider::ToolProviderError;
use agent_protocol::ids::SessionId;
use agent_protocol::tool::ToolErrorKind;
use agent_protocol::tool::{ToolDefinition, ToolInvocation, ToolResult};

/// The seam the loop uses to execute tool calls under policy and approval.
#[async_trait::async_trait]
pub trait ToolGateway: Send + Sync {
    /// Execute a tool call. May return a result, or fail on policy/approval.
    async fn execute(
        &self,
        session_id: &SessionId,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError>;

    /// The tools this gateway can execute, for building a model-facing manifest.
    ///
    /// This is registration-level filtering only. Most policy decisions need an
    /// invocation's own arguments (a path, a command) and cannot be made from a
    /// definition, so per-call enforcement stays in [`Self::execute`].
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    /// The part of [`Self::definitions`] worth advertising to a model.
    ///
    /// A gateway that can tell, from a definition alone, that every possible
    /// call would be refused narrows this: a tool the model can see but never
    /// use spends steps discovering a decision the host already made. The
    /// executable surface stays whole — a caller that names a tool directly
    /// still gets the refusal and its reason — and per-call enforcement is
    /// unchanged.
    fn model_definitions(&self) -> Vec<ToolDefinition> {
        self.definitions()
    }
}

/// Errors from a [`ToolGateway`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum ToolGatewayError {
    #[error("policy denied tool '{tool}': {reason}")]
    PolicyDenied { tool: String, reason: String },
    #[error("approval pending for tool '{tool}': {approval_id}")]
    ApprovalPending { tool: String, approval_id: String },
    #[error("resource boundary violation for tool '{tool}': {reason}")]
    ResourceDenied { tool: String, reason: String },
    /// The name that was called, and what this run actually offers instead.
    ///
    /// "unknown tool" alone is unfalsifiable from the outside: it is the same
    /// message whether the name is a typo, belongs to a server that is not
    /// enabled, or was never registered at all — and the fix differs for each.
    /// The reason is what the gateway knows about its own surface, which is the
    /// only place that knows it.
    #[error("unknown tool '{tool}': {reason}")]
    UnknownTool { tool: String, reason: String },
    /// The call's arguments do not match the schema the tool declares.
    ///
    /// A refusal, not a provider failure: the tool was never handed the call.
    /// It is also the one refusal a model can fix by itself, which is why the
    /// text names the field and what was expected of it rather than saying
    /// "invalid arguments" and stopping there.
    #[error("tool '{tool}' was not run: {}", .problems.join("; "))]
    InvalidArguments { tool: String, problems: Vec<String> },
    /// The tool's own declaration cannot be enforced by this build — a schema
    /// that is not a schema, one too large to compile, or one that reaches for
    /// something outside its own document.
    ///
    /// A wiring fault rather than a caller's mistake, so it is kept apart from
    /// `InvalidArguments`: nobody who called this tool can fix it, and a
    /// provider that ships one is what has to change.
    #[error("tool '{tool}' cannot be called as declared: {reason}")]
    InvalidSchema { tool: String, reason: String },
    /// A tool argument named a secret this run could not resolve.
    ///
    /// A refusal, not a provider failure: the tool was never handed the call.
    /// The text names the reference, never the value, and `kind` says which
    /// side has to change — a reference the run was not granted is a
    /// permission problem, one the provider itself failed to answer is
    /// unavailability, and a malformed reference is the caller's own mistyping.
    #[error(
        "tool '{tool}' was not run: secret reference '{reference}' could not be resolved: {reason}"
    )]
    SecretUnavailable { tool: String, reference: String, reason: String, kind: ToolErrorKind },
    /// The provider was reached, and its own answer was a failure.
    ///
    /// Carried whole rather than flattened to a string, because the inner error
    /// is what says whether the tool ran and failed or never started — the
    /// difference between exit 1 and exit 2 (C3.1). The provider error already
    /// names the tool, so nothing is lost by not repeating it here.
    #[error("tool execution failed: {0}")]
    Tool(ToolProviderError),
    #[error("approval service failed: {0}")]
    Approval(String),
}

impl ToolGatewayError {
    /// The class a `tool/failed` event records for this refusal.
    ///
    /// `None` for the two that are not failures of a call: an approval pending
    /// is a call waiting to run (the log says `approval_pending` instead), and
    /// an approval-service fault is a fault of this run's own machinery, not an
    /// answer about the call.
    pub fn kind(&self) -> Option<ToolErrorKind> {
        match self {
            Self::PolicyDenied { .. } | Self::ResourceDenied { .. } => {
                Some(ToolErrorKind::PermissionDenied)
            }
            Self::UnknownTool { .. } => Some(ToolErrorKind::UnknownTool),
            Self::InvalidArguments { .. } => Some(ToolErrorKind::InvalidArguments),
            Self::InvalidSchema { .. } => Some(ToolErrorKind::Protocol),
            Self::SecretUnavailable { kind, .. } => Some(*kind),
            Self::Tool(error) => Some(error.kind()),
            Self::ApprovalPending { .. } | Self::Approval(_) => None,
        }
    }
}
