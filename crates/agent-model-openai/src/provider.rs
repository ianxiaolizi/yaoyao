//! The OpenAI-compatible `ModelProvider` adapter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_ports::model_provider::{
    ContentBlock, ModelCapability, ModelProvider, ModelProviderError, ModelRequest, ModelResponse,
    ModelStreamSink, ResolvedModelCapabilities,
};
use agent_ports::secret_provider::SecretProvider;
use agent_protocol::model::{ModelCapabilities, ModelCapabilitySource};
use futures_util::StreamExt;

use crate::catalog::{self, parse_model_capabilities};
use crate::config::OpenAiCompatConfig;
use crate::sse::{SseDecoder, SseFrame};
use crate::stream::StreamAccumulator;
use crate::transport::{ChunkStream, Transport, TransportError, TransportRequest};
use crate::wire;

/// Cap on how much of an error body is kept. Enough for a vendor's message,
/// small enough that a misbehaving endpoint cannot fill the log.
const MAX_ERROR_BODY_BYTES: usize = 2048;

/// How long to wait before the first retry. Doubles per attempt, up to
/// [`RETRY_CAP_MS`]: a 429 asks for patience, and asking again immediately is
/// how a client turns one throttled call into a burst.
const RETRY_BASE_MS: u64 = 250;
/// Ceiling on a single retry's wait.
const RETRY_CAP_MS: u64 = 5_000;

/// A provider for any endpoint that speaks the OpenAI chat-completions
/// protocol.
pub struct OpenAiCompatProvider {
    config: OpenAiCompatConfig,
    secrets: Arc<dyn SecretProvider>,
    transport: Arc<dyn Transport>,
    /// Capabilities the endpoint already told us, so assembly asking about the
    /// same model on several routes pays for one `GET /models`.
    discovered: Mutex<HashMap<String, ModelCapabilities>>,
}

impl OpenAiCompatProvider {
    /// Wire an adapter over its config, credential source, and transport.
    pub fn new(
        config: OpenAiCompatConfig,
        secrets: Arc<dyn SecretProvider>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self { config, secrets, transport, discovered: Mutex::new(HashMap::new()) }
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &OpenAiCompatConfig {
        &self.config
    }

    /// The capabilities this adapter implements.
    ///
    /// Deliberately short. `reasoning` is **not** claimed: reasoning text is
    /// streamed into the accumulator but never becomes a content block, so the
    /// adapter cannot hand it back on the next request the way some endpoints
    /// require once tools are in play. Claiming it would promise a round trip
    /// this adapter does not perform.
    pub fn supported_capabilities() -> Vec<ModelCapability> {
        vec![
            ModelCapability::new(ModelCapability::STREAMING),
            ModelCapability::new(ModelCapability::TOOL_CALL),
            ModelCapability::new(ModelCapability::PARALLEL_TOOL_CALL),
            ModelCapability::new(ModelCapability::TOKEN_USAGE),
        ]
    }
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompatProvider {
    async fn stream(
        &self,
        request: &ModelRequest,
        sink: &dyn ModelStreamSink,
    ) -> Result<ModelResponse, ModelProviderError> {
        // A missing credential names the reference, never the value.
        let key = self
            .secrets
            .resolve(self.config.api_key())
            .map_err(|e| ModelProviderError::Unavailable(e.to_string()))?;

        let body = wire::chat_request(&self.config, request)
            .map_err(|e| ModelProviderError::Protocol(e.to_string()))?;
        let requested_model = body.model.clone();
        let body = serde_json::to_vec(&body).map_err(|e| {
            ModelProviderError::Protocol(format!("could not encode the request: {e}"))
        })?;

        // The credential and the encoded body are settled once, before anything
        // is sent: neither can be fixed by asking again, so a failure in them is
        // never a retry. That also keeps the only `Unavailable` left inside the
        // loop a connection problem, which is exactly what is worth repeating.
        let mut attempt = 0;
        loop {
            match self.run_attempt(request, sink, &key, &body, &requested_model).await {
                Err(error) if attempt < self.config.retries && is_retryable(&error) => {
                    // A second attempt only makes sense while the failure could
                    // plausibly answer differently — see `is_retryable`.
                    tokio::time::sleep(retry_delay(attempt)).await;
                    attempt += 1;
                }
                settled => return settled,
            }
        }
    }

    fn capabilities(&self) -> Vec<ModelCapability> {
        Self::supported_capabilities()
    }

    fn provider_id(&self) -> String {
        self.config.provider_id.clone()
    }

    async fn model_capabilities(
        &self,
        model: &str,
    ) -> Result<ResolvedModelCapabilities, ModelProviderError> {
        if let Some(capabilities) =
            self.discovered.lock().ok().and_then(|cache| cache.get(model).cloned())
        {
            return Ok(ResolvedModelCapabilities {
                capabilities,
                source: ModelCapabilitySource::Provider,
            });
        }

        match self.discover_capabilities(model).await {
            Some(capabilities) => {
                if let Ok(mut cache) = self.discovered.lock() {
                    cache.insert(model.to_string(), capabilities.clone());
                }
                Ok(ResolvedModelCapabilities {
                    capabilities,
                    source: ModelCapabilitySource::Provider,
                })
            }
            // An endpoint that is unreachable, refuses the request, or answers
            // without a usable entry is not a reason to run with no budget. It
            // is a reason to say the numbers are a guess.
            None => Ok(catalog::capabilities_without_provider(model)),
        }
    }
}

impl OpenAiCompatProvider {
    /// Ask `GET /models` what `model` can do.
    ///
    /// Every failure mode collapses to `None`: an unreachable endpoint, a
    /// non-200, an unreadable body, or an entry that names no window. The
    /// caller turns that into an explicit bundled-or-fallback answer, so a
    /// malformed catalog can never become a silent zero-token budget.
    async fn discover_capabilities(&self, model: &str) -> Option<ModelCapabilities> {
        let key = self.secrets.resolve(self.config.api_key()).ok()?;
        let response = self
            .transport
            .get(TransportRequest {
                url: self.config.models_url(),
                bearer_token: key,
                body: Vec::new(),
                timeout_ms: self.config.first_event_timeout_ms,
                connect_timeout_ms: self.config.connect_timeout_ms,
            })
            .await
            .ok()?;
        if response.status != 200 {
            return None;
        }
        let body = read_bounded(response.body).await;
        parse_model_capabilities(&body, model)
    }

    /// One attempt: post the prepared request and stream it to a settled
    /// response or a failure.
    ///
    /// `stream` owns the retry; this is the part that may be repeated, so it
    /// must leave nothing of its own behind — the decoder, the accumulator, and
    /// the interruption record are all built here.
    async fn run_attempt(
        &self,
        request: &ModelRequest,
        sink: &dyn ModelStreamSink,
        key: &str,
        body: &[u8],
        requested_model: &str,
    ) -> Result<ModelResponse, ModelProviderError> {
        // The endpoint gets one first-event window to answer at all. Note the
        // request still carries no total deadline: a stream that has started
        // talking is governed by the idle window below, chunk by chunk.
        let first_event = Duration::from_millis(self.config.first_event_timeout_ms);
        let response = tokio::time::timeout(
            first_event,
            self.transport.post(TransportRequest {
                url: self.config.chat_completions_url(),
                bearer_token: key.to_string(),
                body: body.to_vec(),
                timeout_ms: self.config.first_event_timeout_ms,
                connect_timeout_ms: self.config.connect_timeout_ms,
            }),
        )
        .await
        .map_err(|_| ModelProviderError::Timeout {
            after_ms: self.config.first_event_timeout_ms,
        })?
        .map_err(|e| map_transport_error(e, self.config.first_event_timeout_ms))?;

        if response.status != 200 {
            let detail = read_bounded(response.body).await;
            return Err(ModelProviderError::Http {
                status: response.status,
                body: wire::describe_error_body(&detail),
            });
        }

        let mut stream_body = response.body;
        let mut decoder = SseDecoder::new();
        let mut accumulator = StreamAccumulator::new();
        let mut interrupted: Option<TransportError> = None;
        let mut settled = false;
        let idle = Duration::from_millis(self.config.stream_idle_timeout_ms);

        'chunks: loop {
            // Each iteration starts a fresh window, so any chunk — including one
            // that carries no event — resets the patience. What is measured is
            // silence, not duration.
            let chunk = tokio::time::timeout(idle, stream_body.next()).await.map_err(|_| {
                ModelProviderError::Timeout { after_ms: self.config.stream_idle_timeout_ms }
            })?;
            let Some(chunk) = chunk else { break };
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    interrupted = Some(error);
                    break;
                }
            };
            for frame in decoder.push(&bytes) {
                if absorb_frame(&frame, &mut accumulator, sink).await? {
                    settled = true;
                    break 'chunks;
                }
            }
        }

        if let Some(error) = interrupted {
            return Err(match error {
                // The endpoint stopped answering on our own timeout: keep its
                // identity rather than degrading into a generic interruption.
                TransportError::Timeout => {
                    ModelProviderError::Timeout { after_ms: self.config.stream_idle_timeout_ms }
                }
                // A connection that dropped or failed mid-stream. The
                // interruption keeps the transport's own text — the one clue to
                // what went wrong — instead of collapsing to a bare delta count.
                // The deltas that did arrive stay live-only either way.
                TransportError::Connect(cause) | TransportError::Io(cause) => {
                    ModelProviderError::StreamInterrupted {
                        received_deltas: accumulator.deltas(),
                        cause: Some(cause),
                    }
                }
            });
        }

        // The connection closed cleanly. A frame whose terminating blank line
        // never arrived — the endpoint closed right after the last `data:` line
        // — is still a frame, so drain the decoder before judging settlement.
        for frame in decoder.flush() {
            if absorb_frame(&frame, &mut accumulator, sink).await? {
                settled = true;
                break;
            }
        }

        // An attempt settles on an end-of-answer signal: the `[DONE]` sentinel,
        // or the protocol's own `finish_reason`. A clean EOF that carries
        // neither was truncated mid-answer, not finished. There is no transport
        // text to add here: the endpoint simply stopped talking.
        if !settled && !accumulator.has_finish_reason() {
            return Err(ModelProviderError::StreamInterrupted {
                received_deltas: accumulator.deltas(),
                cause: None,
            });
        }

        let mut response = accumulator.finish();
        // The model answers with the names it was offered. Hand back the
        // internal ids so policy, events, and the gateway never see a wire name.
        restore_tool_names(&mut response, request);
        response.provider_metadata = serde_json::json!({
            "provider": self.config.provider_id,
            "model": requested_model,
        });
        Ok(response)
    }
}

/// Whether asking again could plausibly get a different answer.
///
/// Three kinds of failure are permanent for this attempt. One is about the
/// request itself — a 4xx, an unparseable body, our own encoding — and the same
/// request will fail the same way. One is our own deadline firing: retrying a
/// timeout multiplies the very clock the timeout was set to bound, and a
/// deadline that fired has already spent the patience a retry would ask for.
/// The third is a sink that has already been shown deltas, which a second
/// attempt would repeat with no way to take them back.
fn is_retryable(error: &ModelProviderError) -> bool {
    match error {
        // Reached from here only for a connection that never opened: the
        // credential and the encoding are settled before the first attempt.
        ModelProviderError::Unavailable(_) => true,
        // 429 is "slow down, ask again"; 5xx is "I broke". A 4xx is about the
        // request, and repeating it just repeats the rejection.
        ModelProviderError::Http { status, .. } => *status == 429 || (500..600).contains(status),
        // A stream that died before saying anything. Once a delta has reached
        // the sink there is nothing to retract, so a second attempt would show
        // the consumer the same beginning twice.
        ModelProviderError::StreamInterrupted { received_deltas, .. } => *received_deltas == 0,
        ModelProviderError::Timeout { .. }
        | ModelProviderError::Protocol(_)
        | ModelProviderError::Cancelled => false,
    }
}

/// How long to wait before the attempt after `attempt`, doubling up to the cap.
fn retry_delay(attempt: u32) -> Duration {
    // The shift is bounded before it runs: a deliberately huge `retries` must
    // cost time, not a panic on overflow.
    let factor = 1u64 << attempt.min(16);
    Duration::from_millis(RETRY_BASE_MS.saturating_mul(factor).min(RETRY_CAP_MS))
}

/// Feed one decoded SSE frame to the accumulator and the live sink.
///
/// Returns `true` when the frame is the `[DONE]` sentinel, which settles the
/// attempt. Shared by the read loop and the end-of-stream flush so a frame is
/// handled identically whether its blank-line terminator arrived or not.
async fn absorb_frame(
    frame: &SseFrame,
    accumulator: &mut StreamAccumulator,
    sink: &dyn ModelStreamSink,
) -> Result<bool, ModelProviderError> {
    if frame.is_done() {
        return Ok(true);
    }
    let deltas =
        accumulator.absorb(&frame.data).map_err(|e| ModelProviderError::Protocol(e.to_string()))?;
    for delta in deltas {
        // A sink that stopped accepting is not a model failure. The endpoint was
        // reached and answered; the *local* consumer ended the attempt. That is
        // a cancellation, not an outage — reporting it as `Unavailable` would
        // dress a stopped listener up as an unreachable backend. Whatever text
        // the sink carried is live-only and not worth a new public variant.
        sink.on_event(delta).await.map_err(|_| ModelProviderError::Cancelled)?;
    }
    Ok(false)
}

/// Translate tool names on a settled response back to their internal ids.
///
/// A name the manifest never offered is left untouched: the model invented it,
/// and the gateway should answer with a real "unknown tool" result rather than
/// this layer quietly pointing the call at some other tool.
fn restore_tool_names(response: &mut ModelResponse, request: &ModelRequest) {
    let index = wire::wire_name_index(&request.tool_manifest);
    if index.is_empty() {
        return;
    }
    for block in &mut response.content_blocks {
        if let ContentBlock::ToolUse { tool_name, .. } = block
            && let Some(internal) = index.get(tool_name.as_str())
        {
            *tool_name = internal.clone();
        }
    }
}

/// Map a transport failure to a provider error.
fn map_transport_error(error: TransportError, timeout_ms: u64) -> ModelProviderError {
    match error {
        TransportError::Timeout => ModelProviderError::Timeout { after_ms: timeout_ms },
        TransportError::Connect(message) | TransportError::Io(message) => {
            ModelProviderError::Unavailable(message)
        }
    }
}

/// Read at most [`MAX_ERROR_BODY_BYTES`] from a response body.
async fn read_bounded(mut body: ChunkStream) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next().await {
        let Ok(bytes) = chunk else { break };
        let room = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
        if room == 0 {
            break;
        }
        buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }
    String::from_utf8_lossy(&buf).to_string()
}
