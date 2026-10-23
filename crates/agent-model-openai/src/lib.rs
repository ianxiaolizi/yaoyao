//! agent-model-openai: an OpenAI-compatible model adapter.
//!
//! This is the only crate that knows a vendor's wire format. It normalizes
//! requests and streaming chunks at the edge, so the kernel, the loop, and the
//! event log only ever see the provider-neutral protocol types.
//!
//! Tool names are the one place the two vocabularies genuinely differ. An
//! internal id may be dotted (`fs.read`); the endpoint accepts only
//! `[a-zA-Z0-9_-]` in a function name. Every name that leaves this crate goes
//! through [`to_wire_name`], and every name that comes back is looked up in the
//! manifest through [`wire_name_index`] — in both directions, so nothing above
//! this crate ever has to know a wire name exists.

#![deny(unsafe_code)]

mod config;
mod provider;
mod sse;
mod stream;
mod transport;
mod wire;

pub use config::{
    ConfigError, DEFAULT_PROVIDER_ID, DEFAULT_RETRIES, DEFAULT_TIMEOUT_MS, ENV_API_KEY,
    ENV_BASE_URL, ENV_MODEL_NAME, OpenAiCompatConfig,
};
pub use provider::OpenAiCompatProvider;
pub use sse::{SseDecoder, SseFrame};
pub use stream::{DecodeError, StreamAccumulator};
pub use transport::{
    ChunkStream, ReqwestTransport, Transport, TransportError, TransportRequest, TransportResponse,
};
pub use wire::{
    ChatRequest, WireChunk, WireDelta, WireError, WireMessage, WireToolCall, WireUsage,
    chat_request, chunk_deltas, describe_error_body, normalize_finish_reason, to_wire_messages,
    to_wire_name, to_wire_tools, wire_name_index,
};
