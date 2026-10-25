//! Model provider assembly.
//!
//! Two providers are available: a local echo model, which keeps the pipeline
//! exercised with no network and no credential, and the OpenAI-compatible
//! adapter, which talks to a real endpoint. `AGENT_MODEL_PROVIDER` picks one.

use std::sync::Arc;

use agent_model_openai::{ENV_API_KEY, OpenAiCompatConfig, OpenAiCompatProvider, ReqwestTransport};
use agent_ports::model_provider::{
    MessageRole, ModelCapability, ModelProvider, ModelProviderError, ModelRequest, ModelResponse,
    ModelStreamEvent, ModelStreamSink,
};
use agent_ports::model_router::{ModelRoute, ModelTier};
use agent_store_secret::EnvSecretProvider;

/// Environment variable selecting the provider: `echo` (default) or `openai`.
pub const ENV_PROVIDER: &str = "AGENT_MODEL_PROVIDER";

/// Returns the user's text back as the answer, with no tool calls.
pub struct EchoModelProvider;

#[async_trait::async_trait]
impl ModelProvider for EchoModelProvider {
    async fn stream(
        &self,
        request: &ModelRequest,
        sink: &dyn ModelStreamSink,
    ) -> Result<ModelResponse, ModelProviderError> {
        let text = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.text_content())
            .unwrap_or_default();

        // Emit the answer the way a real streaming provider would, so the live
        // path is exercised even with the placeholder model.
        sink.on_event(ModelStreamEvent::Started)
            .await
            .map_err(|e| ModelProviderError::Protocol(e.to_string()))?;
        sink.on_event(ModelStreamEvent::TextDelta { text: format!("echo: {text}") })
            .await
            .map_err(|e| ModelProviderError::Protocol(e.to_string()))?;

        Ok(ModelResponse::text(format!("echo: {text}")))
    }

    fn capabilities(&self) -> Vec<ModelCapability> {
        vec![ModelCapability::new(ModelCapability::STREAMING)]
    }

    fn provider_id(&self) -> String {
        "echo".to_string()
    }
}

/// Why the provider could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum ProviderConfigError {
    #[error("unknown {ENV_PROVIDER} '{0}': use 'echo' or 'openai'")]
    UnknownProvider(String),
    #[error("{0}")]
    Config(#[from] agent_model_openai::ConfigError),
    #[error("could not build the HTTP client: {0}")]
    Transport(#[from] agent_model_openai::TransportError),
}

/// A provider together with the route that names it.
///
/// The route is built from the same configuration the adapter reads, so the
/// model a request is sent to has exactly one source. A router assembled from a
/// second copy of the model name would be a way for the log to say one thing and
/// the wire another.
pub struct SelectedProvider {
    pub provider: Arc<dyn ModelProvider>,
    pub route: ModelRoute,
}

/// Build the provider named by `AGENT_MODEL_PROVIDER`.
///
/// The echo provider is the default: a bare `agent ask` never needs a
/// credential or a network. The OpenAI-compatible adapter reads its endpoint
/// from the environment and resolves the key through `EnvSecretProvider`, so no
/// credential ever passes through this function.
///
/// `model` is the configuration's model name, if it named one. It overrides the
/// adapter's own default rather than sitting beside it: a run must have exactly
/// one answer to "which model", and the configuration's is the one that was
/// resolved. For the echo provider — which has no model of its own to ask —
/// the name is still recorded in the route, because it is what was requested,
/// and a log that quietly dropped it would make a `--model` invisible.
pub fn select(model: Option<&str>) -> Result<SelectedProvider, ProviderConfigError> {
    select_kind(&env_provider(), model)
}

/// The provider named by `AGENT_MODEL_PROVIDER`, or `echo` when unset.
pub fn env_provider() -> String {
    std::env::var(ENV_PROVIDER).unwrap_or_else(|_| "echo".to_string())
}

/// The same, with the provider named by the caller rather than the environment.
///
/// `agent web --provider` passes a name here instead of writing one into the
/// environment: by the time the server runs a turn the process has spawned
/// threads, and a route should come from the decision the host made when it
/// started, not from a variable that could still change underneath it.
pub fn select_kind(
    kind: &str,
    model: Option<&str>,
) -> Result<SelectedProvider, ProviderConfigError> {
    match kind {
        "echo" => Ok(SelectedProvider {
            provider: Arc::new(EchoModelProvider),
            route: ModelRoute::new(ModelTier::Main, "echo", model.unwrap_or("echo")),
        }),
        "openai" => {
            let mut config = OpenAiCompatConfig::from_env()?;
            if let Some(model) = model {
                config.model = model.to_string();
            }
            let route =
                ModelRoute::new(ModelTier::Main, config.provider_id.clone(), config.model.clone());
            let secrets = Arc::new(EnvSecretProvider::new([ENV_API_KEY]));
            let transport = Arc::new(ReqwestTransport::new()?);
            Ok(SelectedProvider {
                provider: Arc::new(OpenAiCompatProvider::new(config, secrets, transport)),
                route,
            })
        }
        other => Err(ProviderConfigError::UnknownProvider(other.to_string())),
    }
}
