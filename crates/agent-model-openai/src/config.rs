//! Adapter configuration.
//!
//! The API key is never a config field: only a [`SecretRef`] naming where to
//! find it. Resolution happens at call time through a `SecretProvider`, so the
//! key cannot reach a durable event, a log line, or a debug dump by accident.

use agent_protocol::permissions::SecretRef;

/// Environment variable holding the endpoint base, e.g. `https://api.deepseek.com`.
pub const ENV_BASE_URL: &str = "AGENT_MODEL_BASE_URL";
/// Environment variable holding the model name, e.g. `deepseek-flash`.
pub const ENV_MODEL_NAME: &str = "AGENT_MODEL_NAME";
/// Environment variable holding the API key.
pub const ENV_API_KEY: &str = "AGENT_MODEL_API_KEY";

/// How long establishing a connection may take. A connection that cannot be
/// made in this window is not going to be made by waiting longer.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 30_000;
/// How long the endpoint may take to answer at all — response headers first,
/// then anything at all — before the attempt is considered dead.
pub const DEFAULT_FIRST_EVENT_TIMEOUT_MS: u64 = 60_000;
/// How long a stream that has already started may stay silent before it is
/// considered dead. Every chunk restarts the window, so a reasoning model that
/// thinks for minutes but keeps the connection warm is not killed; a stream
/// that stops talking is.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
/// The old single per-attempt wall clock.
///
/// There is no whole-request deadline any more: a long answer is not a hung
/// endpoint, and the first-event and idle watchdogs are what separate the two.
/// Kept, equal to the first-event default, for callers that still name it.
pub const DEFAULT_TIMEOUT_MS: u64 = DEFAULT_FIRST_EVENT_TIMEOUT_MS;
/// Default provider id, used in attempt events and doctor output.
pub const DEFAULT_PROVIDER_ID: &str = "openai-compat";
/// Default number of *extra* attempts after a transient failure. Two means a
/// call may be made three times in all; the budget is per call, and the waits
/// between attempts are the adapter's own, not the caller's deadline.
pub const DEFAULT_RETRIES: u32 = 2;

/// Everything needed to reach one OpenAI-compatible endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatConfig {
    /// Stable id reported by `ModelProvider::provider_id`.
    pub provider_id: String,
    /// Endpoint base without a trailing slash; the adapter appends
    /// `/chat/completions`. Any path prefix the vendor requires belongs here.
    pub base_url: String,
    /// Default model, used when the request carries no `model_route`.
    pub model: String,
    /// Where to find the credential — never the credential itself.
    pub api_key: SecretRef,
    /// How long establishing a connection may take, in milliseconds.
    pub connect_timeout_ms: u64,
    /// How long the endpoint may take to return response headers, in
    /// milliseconds.
    pub first_event_timeout_ms: u64,
    /// How long a started stream may stay silent between chunks, in
    /// milliseconds.
    pub stream_idle_timeout_ms: u64,
    /// Extra attempts allowed after a transient failure, on top of the first.
    ///
    /// Only failures that could plausibly answer differently are retried — a
    /// dropped connection, a 429, a 5xx, a stream that died before saying
    /// anything. An attempt that already reached the sink is never repeated.
    pub retries: u32,
    /// `max_tokens` to send when the request's budget does not set one.
    pub default_max_tokens: Option<u32>,
}

impl OpenAiCompatConfig {
    /// A config for `base_url`/`model`, with the default credential reference
    /// and the process environment as the intended secret source.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider_id: DEFAULT_PROVIDER_ID.to_string(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key: SecretRef { source: "env".to_string(), name: ENV_API_KEY.to_string() },
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            first_event_timeout_ms: DEFAULT_FIRST_EVENT_TIMEOUT_MS,
            stream_idle_timeout_ms: DEFAULT_STREAM_IDLE_TIMEOUT_MS,
            retries: DEFAULT_RETRIES,
            default_max_tokens: None,
        }
    }

    /// Read the endpoint from the environment.
    ///
    /// Only the base URL and model name come from here; the credential stays a
    /// reference and is resolved later, at call time.
    pub fn from_env() -> Result<Self, ConfigError> {
        let base_url = required_env(ENV_BASE_URL)?;
        let model = required_env(ENV_MODEL_NAME)?;
        Ok(Self::new(base_url, model))
    }

    /// Override the provider id.
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = provider_id.into();
        self
    }

    /// Resolve the credential from somewhere other than the `env` backend.
    pub fn with_api_key_secret(mut self, api_key: SecretRef) -> Self {
        self.api_key = api_key;
        self
    }

    /// Override the legacy single timeout: both the first-event limit and the
    /// stream idle limit become `timeout_ms`.
    ///
    /// Kept so a caller that configured "the timeout" keeps the patience it
    /// asked for. New code should name the limit it means.
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.first_event_timeout_ms = timeout_ms;
        self.stream_idle_timeout_ms = timeout_ms;
        self
    }

    /// Override how long response headers may take.
    pub fn with_first_event_timeout_ms(mut self, first_event_timeout_ms: u64) -> Self {
        self.first_event_timeout_ms = first_event_timeout_ms;
        self
    }

    /// Override how long a started stream may stay silent between chunks.
    pub fn with_stream_idle_timeout_ms(mut self, stream_idle_timeout_ms: u64) -> Self {
        self.stream_idle_timeout_ms = stream_idle_timeout_ms;
        self
    }

    /// Override how long establishing a connection may take.
    pub fn with_connect_timeout_ms(mut self, connect_timeout_ms: u64) -> Self {
        self.connect_timeout_ms = connect_timeout_ms;
        self
    }

    /// Override how many extra attempts a transient failure earns.
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    /// Set the `max_tokens` fallback.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = Some(max_tokens);
        self
    }

    /// The credential reference, for error messages that name a secret without
    /// revealing one.
    pub fn api_key(&self) -> &SecretRef {
        &self.api_key
    }

    /// The full chat-completions URL.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// The capability-discovery URL.
    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url.trim_end_matches('/'))
    }
}

/// Errors loading configuration from the environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("environment variable {0} is not set")]
    MissingEnv(&'static str),
    #[error("environment variable {0} is empty")]
    EmptyEnv(&'static str),
}

fn required_env(name: &'static str) -> Result<String, ConfigError> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        Ok(_) => Err(ConfigError::EmptyEnv(name)),
        Err(_) => Err(ConfigError::MissingEnv(name)),
    }
}
