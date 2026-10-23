//! Live endpoint test — ignored by default, since it needs the network and a
//! real credential.
//!
//! Run it explicitly once the environment is configured:
//!
//! ```text
//! AGENT_MODEL_BASE_URL=https://api.deepseek.com \
//! AGENT_MODEL_NAME=deepseek-flash \
//! AGENT_MODEL_API_KEY=*** \
//!   cargo test -p agent-model-openai --test live_endpoint -- --ignored
//! ```
//!
//! Nothing here prints the credential, and the assertions only look at the
//! settled response — never at raw headers or request bodies.

use std::sync::Arc;

use agent_model_openai::{
    ENV_API_KEY, ENV_BASE_URL, ENV_MODEL_NAME, OpenAiCompatConfig, OpenAiCompatProvider,
    ReqwestTransport,
};
use agent_ports::model_provider::{
    MessageRole, ModelBudget, ModelCapability, ModelMessage, ModelProvider, ModelRequest,
    ToolManifest,
};
use agent_protocol::ids::{SessionId, StepId, TurnId};
use agent_store_secret::EnvSecretProvider;
use agent_testkit::RecordingSink;

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn live_request(text: &str) -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from_string("live-smoke"),
        turn_id: TurnId::new(),
        step_id: StepId::new(),
        context_revision: 0,
        messages: vec![ModelMessage::text(MessageRole::User, text)],
        tool_manifest: ToolManifest::empty(),
        capabilities: vec![ModelCapability::new(ModelCapability::STREAMING)],
        response_format: None,
        model_route: None,
        // Keep the smoke test cheap and bounded.
        budget: ModelBudget { max_tokens: Some(64), ..ModelBudget::default() },
    }
}

#[tokio::test]
#[ignore = "needs a live endpoint and a real credential"]
async fn a_real_endpoint_streams_and_settles() {
    // This test is `#[ignore]`d because it needs a live endpoint. Running it
    // explicitly is a request for the real check, so missing configuration is a
    // failure, not a skip: a silent pass here is indistinguishable from a
    // verified endpoint and is the one outcome this file exists to prevent.
    let missing: Vec<&str> = [ENV_BASE_URL, ENV_MODEL_NAME, ENV_API_KEY]
        .into_iter()
        .filter(|name| env_value(name).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "this test is #[ignore]d for a live endpoint; run explicitly, it needs {}",
        missing.join(", ")
    );

    let config = OpenAiCompatConfig::from_env().expect("config from env");
    let model_name = config.model.clone();
    let secrets = Arc::new(EnvSecretProvider::new([ENV_API_KEY]));
    let transport = Arc::new(ReqwestTransport::new().expect("tls client"));
    let provider = OpenAiCompatProvider::new(config, secrets, transport);
    let sink = RecordingSink::new();

    let response = provider
        .stream(&live_request("Reply with exactly the word: pong"), &sink)
        .await
        .expect("a settled response");

    let text = response.text_content();
    eprintln!("settled: finish_reason={} blocks={}", response.finish_reason, text.len());
    assert!(!text.trim().is_empty(), "the endpoint must answer with text");
    assert!(
        text.to_ascii_lowercase().contains("pong"),
        "the answer should follow the instruction: {text}"
    );
    assert!(!sink.events().is_empty(), "a streamed answer must deliver at least one live delta");
    // A provider that reports token usage must actually report it.
    if provider.capabilities().iter().any(|c| c.as_str() == ModelCapability::TOKEN_USAGE) {
        assert!(response.usage.is_some(), "usage was declared but not reported");
    }
    assert_eq!(response.provider_metadata["model"], model_name);
}

#[tokio::test]
#[ignore = "needs a live endpoint and a real credential"]
async fn a_real_endpoint_rejects_a_bad_credential() {
    // Same rule as the test above: explicitly running an `#[ignore]`d test
    // asks for the real check, so absent configuration must fail loudly.
    let base_url = env_value(ENV_BASE_URL).unwrap_or_else(|| {
        panic!("this test is #[ignore]d for a live endpoint; set {ENV_BASE_URL}")
    });
    let model = env_value(ENV_MODEL_NAME).unwrap_or_else(|| "unused".to_string());

    // A deliberately invalid key: the adapter must report an HTTP error, and the
    // error text must not contain the key we sent.
    let bogus = "sk-not-a-valid-credential";
    let config = OpenAiCompatConfig::new(base_url, model);
    let secrets = Arc::new(agent_testkit::FixedSecret::new(bogus));
    let transport = Arc::new(ReqwestTransport::new().expect("tls client"));
    let provider = OpenAiCompatProvider::new(config, secrets, transport);
    let sink = RecordingSink::new();

    let error = provider.stream(&live_request("hi"), &sink).await.unwrap_err();
    match error {
        agent_ports::model_provider::ModelProviderError::Http { status, body } => {
            assert!(status == 401 || status == 403, "expected an auth rejection, got {status}");
            assert!(!body.contains(bogus), "the error body must not echo the credential");
        }
        other => panic!("expected an http error, got {other:?}"),
    }
}
