//! ModelProvider trait — the adapter seam for model backends.
//!
//! The protocol types live in `agent_protocol::model` (protocol-first) and are
//! re-exported here so existing import paths keep working.
//!
//! Providers stream deltas to a sink for the UI and return a settled response.
//! Deltas are live-only; only the settled response may become durable truth.
//! An implementation that cannot finish must return an error — never a
//! half-assembled response.

pub use agent_protocol::model::{
    ContentBlock, FinishReason, ManifestEntry, MessageRole, ModelBudget, ModelCapability,
    ModelMessage, ModelRequest, ModelResponse, ModelRoute, ModelStreamEvent, ModelTier,
    ReasoningEffort, ResolvedModelCapabilities, ResponseFormat, ToolCall, ToolManifest, Usage,
};

/// Receives live deltas while an attempt is in flight.
///
/// A sink that never sees a settling response leaves no durable trace: that is
/// the intended behavior, not a lost write.
#[async_trait::async_trait]
pub trait ModelStreamSink: Send + Sync {
    async fn on_event(&self, event: ModelStreamEvent) -> Result<(), ModelStreamError>;
}

/// Errors from a ModelStreamSink implementation.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelStreamError {
    #[error("stream sink closed")]
    Closed,
}

/// Port for model inference.
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    /// Run one attempt.
    ///
    /// Deltas are handed to `sink` as they arrive. The returned response is the
    /// attempt's settled result.
    async fn stream(
        &self,
        request: &ModelRequest,
        sink: &dyn ModelStreamSink,
    ) -> Result<ModelResponse, ModelProviderError>;

    /// The capabilities this provider supports. A caller must disable
    /// or degrade explicitly when a required capability is missing — never let
    /// the provider reshape the request silently.
    fn capabilities(&self) -> Vec<ModelCapability>;

    /// What one model on this provider can do.
    ///
    /// The answer carries its own source, so a conservative guess is never
    /// mistaken for a discovered fact. The default refuses: a provider that
    /// cannot discover capabilities must be asked through the assembly's own
    /// fallback rather than silently inheriting a made-up window.
    async fn model_capabilities(
        &self,
        _model: &str,
    ) -> Result<ResolvedModelCapabilities, ModelProviderError> {
        Err(ModelProviderError::Protocol(
            "model capabilities are not supported by this provider".to_string(),
        ))
    }

    /// A short stable id, used in attempt events and doctor output.
    fn provider_id(&self) -> String;
}

/// Errors from a ModelProvider implementation.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelProviderError {
    /// No usable endpoint: missing credentials, unset base URL, DNS failure.
    #[error("model provider unavailable: {0}")]
    Unavailable(String),
    #[error("model request timed out after {after_ms}ms")]
    Timeout { after_ms: u64 },
    #[error("model endpoint returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    /// The endpoint rejected the request for its context length. This is a
    /// typed failure rather than a bare 400/413 so the loop can recover by
    /// compressing the request once — repeating the same body would only be
    /// rejected again.
    #[error("model context overflow (HTTP {status}): {body}")]
    ContextOverflow { status: u16, body: String },
    /// The response did not match the protocol (unparseable body, missing
    /// fields, unknown shape).
    #[error("model protocol error: {0}")]
    Protocol(String),
    /// The stream ended before it settled. Whatever deltas were emitted stay
    /// live-only; no partial response is written.
    ///
    /// `cause` carries the transport failure's own text when one cut the stream
    /// short; it is `None` for a clean EOF that simply never sent an
    /// end-of-answer signal. Collapsing both cases to a bare delta count threw
    /// away the one clue a truncation leaves behind (`YA-65`).
    #[error(
        "model stream interrupted after {received_deltas} deltas{}",
        .cause.as_ref().map(|c| format!(": {c}")).unwrap_or_default()
    )]
    StreamInterrupted { received_deltas: u64, cause: Option<String> },
    /// The attempt was ended by the caller's side, not by the endpoint. Today
    /// that is a `ModelStreamSink` that stopped accepting deltas mid-stream: the
    /// endpoint was reached and answered, so this must not be reported as an
    /// unreachable backend. `is_cancelled()` routes it away from `FatalError`.
    #[error("model request cancelled")]
    Cancelled,
}

impl ModelProviderError {
    /// A stable machine-readable kind, safe to persist in durable attempt
    /// events — unlike the display message, which may embed provider text.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::Timeout { .. } => "timeout",
            Self::Http { .. } => "http",
            Self::ContextOverflow { .. } => "context_overflow",
            Self::Protocol(_) => "protocol",
            Self::StreamInterrupted { .. } => "stream_interrupted",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether this error ended the attempt by cancellation rather than failure.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Whether the endpoint rejected the request for its context length.
    ///
    /// Only this typed error may enter the loop's one-shot overflow recovery;
    /// a generic HTTP failure must keep its ordinary fatal path.
    pub fn is_context_overflow(&self) -> bool {
        matches!(self, Self::ContextOverflow { .. })
    }
}
