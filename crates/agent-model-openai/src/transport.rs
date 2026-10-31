//! The HTTP seam.
//!
//! The provider never touches `reqwest` directly. It posts through [`Transport`]
//! and reads a chunk stream back, which keeps every provider test offline: the
//! only thing a test has to fake is the byte stream.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::Stream;
use futures_util::StreamExt;

/// A stream of response body chunks.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send>>;

/// One POST to perform.
#[derive(Clone)]
pub struct TransportRequest {
    pub url: String,
    /// The bearer credential. Redacted by this type's `Debug` — a transport
    /// request must be safe to log.
    pub bearer_token: String,
    pub body: Vec<u8>,
    /// Wall clock for a small, non-streamed request such as `GET /models`.
    ///
    /// Streaming POSTs must not be bounded by this: the provider watches for
    /// the first event and for silence between chunks, so a long answer is
    /// allowed to take as long as it keeps talking.
    pub timeout_ms: u64,
    /// How long establishing the connection may take. Applies to both methods:
    /// a connection that cannot be made is not a request that is late.
    pub connect_timeout_ms: u64,
}

impl std::fmt::Debug for TransportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportRequest")
            .field("url", &self.url)
            .field("bearer_token", &"***")
            .field("body_bytes", &self.body.len())
            .field("timeout_ms", &self.timeout_ms)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .finish()
    }
}

/// A response whose body is still arriving.
pub struct TransportResponse {
    pub status: u16,
    pub body: ChunkStream,
}

/// Errors a transport can produce.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("could not reach the endpoint: {0}")]
    Connect(String),
    #[error("the request timed out")]
    Timeout,
    #[error("transport error: {0}")]
    Io(String),
}

/// Performs one HTTP POST and hands back the body as a stream.
///
/// `get` exists for capability discovery (`GET /models`), which is a different
/// request than an attempt and must not be retried or streamed like one.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn post(&self, request: TransportRequest) -> Result<TransportResponse, TransportError>;

    /// Perform a GET. A transport that only carries chat traffic refuses.
    async fn get(&self, _request: TransportRequest) -> Result<TransportResponse, TransportError> {
        Err(TransportError::Connect("GET is not supported by this transport".to_string()))
    }
}

/// The real transport.
///
/// A `reqwest::Client` fixes its connect timeout when it is built, so honoring
/// the per-request value means holding one client per timeout rather than
/// rebuilding a client per call: rebuilding would throw away the connection
/// pool, and a single client would silently apply the first timeout it saw to
/// every later request.
pub struct ReqwestTransport {
    /// Clients built by this transport, keyed by connect timeout.
    clients: Mutex<HashMap<u64, reqwest::Client>>,
    /// A caller-supplied client, used as-is. Whoever built it decided its
    /// connect timeout; a request cannot change that, and pretending otherwise
    /// would be worse than saying so.
    fixed: Option<reqwest::Client>,
}

impl ReqwestTransport {
    /// Build a transport. Fails if the TLS backend cannot be initialized at
    /// the default connect timeout, which is worth surfacing at assembly time
    /// rather than at first request.
    pub fn new() -> Result<Self, TransportError> {
        let mut clients = HashMap::new();
        clients.insert(
            crate::config::DEFAULT_CONNECT_TIMEOUT_MS,
            build_client(crate::config::DEFAULT_CONNECT_TIMEOUT_MS)?,
        );
        Ok(Self { clients: Mutex::new(clients), fixed: None })
    }

    /// Wrap an existing client, for callers that configured one themselves.
    /// Its connect timeout is the client's, not the request's.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { clients: Mutex::new(HashMap::new()), fixed: Some(client) }
    }

    fn client_for(&self, connect_timeout_ms: u64) -> Result<reqwest::Client, TransportError> {
        if let Some(client) = &self.fixed {
            return Ok(client.clone());
        }
        let mut clients = self.clients.lock().map_err(|_| {
            TransportError::Io("the transport's client cache was poisoned".to_string())
        })?;
        if let Some(client) = clients.get(&connect_timeout_ms) {
            return Ok(client.clone());
        }
        let client = build_client(connect_timeout_ms)?;
        clients.insert(connect_timeout_ms, client.clone());
        Ok(client)
    }
}

fn build_client(connect_timeout_ms: u64) -> Result<reqwest::Client, TransportError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(connect_timeout_ms))
        .build()
        .map_err(|e| TransportError::Connect(e.to_string()))
}

#[async_trait::async_trait]
impl Transport for ReqwestTransport {
    async fn post(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        let response = self
            .client_for(request.connect_timeout_ms)?
            .post(&request.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .bearer_auth(&request.bearer_token)
            // Only the handshake is bounded by the client. A total deadline on
            // a streaming POST kills a long answer and hides the difference
            // between "thinking" and "hung"; the provider watches the stream
            // instead.
            .body(request.body)
            .send()
            .await
            .map_err(connect_error)?;

        let status = response.status().as_u16();
        let body = response.bytes_stream().map(|chunk| match chunk {
            Ok(bytes) => Ok(bytes.to_vec()),
            Err(e) => Err(connect_error(e)),
        });
        Ok(TransportResponse { status, body: Box::pin(body) })
    }

    async fn get(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        // Discovery is bounded by the caller's timeout and is never streamed:
        // a `/models` answer is small, and an endpoint that streams one is not
        // an endpoint this adapter can learn anything from.
        let response = self
            .client_for(request.connect_timeout_ms)?
            .get(&request.url)
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(&request.bearer_token)
            .timeout(Duration::from_millis(request.timeout_ms))
            .send()
            .await
            .map_err(connect_error)?;

        let status = response.status().as_u16();
        let body = response.bytes_stream().map(|chunk| match chunk {
            Ok(bytes) => Ok(bytes.to_vec()),
            Err(e) => Err(connect_error(e)),
        });
        Ok(TransportResponse { status, body: Box::pin(body) })
    }
}

fn connect_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else if error.is_connect() || error.is_request() {
        TransportError::Connect(error.to_string())
    } else {
        TransportError::Io(error.to_string())
    }
}
