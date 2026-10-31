//! Model provider assembly.
//!
//! Two providers are available: a local echo model, which keeps the pipeline
//! exercised with no network and no credential, and the OpenAI-compatible
//! adapter, which talks to a real endpoint. [`ProviderRegistry`] is the list of
//! them — one entry per kind, each with the factory that builds it and the
//! capabilities it advertises without being built. `AGENT_MODEL_PROVIDER` names
//! the kind; nothing else in the binary decides which vendors exist.
//!
//! A run still talks to **one** provider kind. What the configuration may vary
//! is the model *name* per tier, and [`router`] is where a tier becomes a task
//! class the loop asks a route about.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_decision::TableModelRouter;
use agent_model_openai::{ENV_API_KEY, OpenAiCompatConfig, OpenAiCompatProvider, ReqwestTransport};
use agent_ports::model_provider::{
    MessageRole, ModelCapability, ModelProvider, ModelProviderError, ModelRequest, ModelResponse,
    ModelStreamEvent, ModelStreamSink, ResolvedModelCapabilities,
};
use agent_protocol::model::ModelCapabilitySource;
use agent_ports::model_router::{ModelRoute, ModelTier, TaskClass};
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
        // The user's words, not the harness's: the loop places its own tail
        // messages (the work state, the budget reminder) with the user role so
        // that every provider accepts them, but they are not what the user
        // said — a placeholder model that echoed them would answer its own
        // ledger.
        let text = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User && !is_harness_tail(m.message_uid.as_str()))
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

    /// A placeholder model has no endpoint to ask, so it answers with the
    /// conservative fallback and says so. It must not borrow the numbers of a
    /// real model: a budget derived from them would be a claim about echo's
    /// window that nothing backs.
    async fn model_capabilities(
        &self,
        _model: &str,
    ) -> Result<ResolvedModelCapabilities, ModelProviderError> {
        Ok(ResolvedModelCapabilities {
            capabilities: agent_model_openai::fallback_capabilities(),
            source: ModelCapabilitySource::Fallback,
        })
    }

    fn provider_id(&self) -> String {
        "echo".to_string()
    }
}

/// Whether a user-role message is the loop's own tail rather than the user's.
fn is_harness_tail(uid: &str) -> bool {
    uid.starts_with("msg/work-state/")
        || uid.starts_with("msg/budget/")
        || uid.starts_with("msg/stall/")
}

/// Why the provider could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum ProviderConfigError {
    #[error("unknown {ENV_PROVIDER} '{kind}': use {known}")]
    UnknownProvider { kind: String, known: String },
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
    ProviderRegistry::builtin().build(kind, model)
}

/// One provider kind: how it is built, and what it can do.
pub struct ProviderKind {
    /// The name `AGENT_MODEL_PROVIDER` and `agent web --provider` accept.
    pub kind: &'static str,
    /// Builds the provider and the route that names it.
    build: fn(Option<&str>) -> Result<SelectedProvider, ProviderConfigError>,
    /// What a provider of this kind advertises, without building one.
    ///
    /// Asked separately from `build` because the two questions have different
    /// costs: a kind's capabilities are a constant, while building it may dial
    /// a client or refuse for lack of a credential.
    capabilities: fn() -> Vec<ModelCapability>,
}

/// Every provider kind this build has.
///
/// Ordered by kind name so the list is the same on every run: a refusal that
/// says "use one of ..." reads the same twice, and a test can pin it.
pub struct ProviderRegistry {
    kinds: BTreeMap<&'static str, ProviderKind>,
}

impl ProviderRegistry {
    /// The providers this binary was built with.
    pub fn builtin() -> Self {
        let kinds = [
            ProviderKind {
                kind: "echo",
                build: build_echo,
                capabilities: || vec![ModelCapability::new(ModelCapability::STREAMING)],
            },
            ProviderKind {
                kind: "openai",
                build: build_openai,
                capabilities: OpenAiCompatProvider::supported_capabilities,
            },
        ];
        Self { kinds: kinds.into_iter().map(|kind| (kind.kind, kind)).collect() }
    }

    /// Every kind, in name order.
    pub fn kinds(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.kinds.keys().copied()
    }

    /// What one kind advertises, if this build has it.
    pub fn capabilities(&self, kind: &str) -> Option<Vec<ModelCapability>> {
        self.kinds.get(kind).map(|entry| (entry.capabilities)())
    }

    /// Build one kind, or refuse by naming the kinds that exist.
    ///
    /// The refusal is the same for the environment variable and for
    /// `--provider`: whether a name came from the process or the command line,
    /// a typo is a typo, and the person who typed it needs the same list.
    pub fn build(
        &self,
        kind: &str,
        model: Option<&str>,
    ) -> Result<SelectedProvider, ProviderConfigError> {
        let entry = self.kinds.get(kind).ok_or_else(|| ProviderConfigError::UnknownProvider {
            kind: kind.to_string(),
            known: self.kinds.keys().copied().collect::<Vec<_>>().join(" or "),
        })?;
        (entry.build)(model)
    }
}

/// The echo provider: no credential, no network, and no model of its own.
///
/// The requested model name is still recorded in the route, because it is what
/// was asked for, and a log that quietly dropped it would make a `--model`
/// invisible.
fn build_echo(model: Option<&str>) -> Result<SelectedProvider, ProviderConfigError> {
    Ok(SelectedProvider {
        provider: Arc::new(EchoModelProvider),
        route: ModelRoute::new(ModelTier::Main, "echo", model.unwrap_or("echo")),
    })
}

/// The OpenAI-compatible adapter, reading its endpoint from the environment.
///
/// The key is resolved through `EnvSecretProvider` at call time, so no
/// credential passes through this function. `model` overrides the adapter's own
/// default rather than sitting beside it: a run must have exactly one answer to
/// "which model", and the configuration's is the one that was resolved.
fn build_openai(model: Option<&str>) -> Result<SelectedProvider, ProviderConfigError> {
    let mut config = OpenAiCompatConfig::from_env()?;
    if let Some(model) = model {
        config.model = model.to_string();
    }
    let route = ModelRoute::new(ModelTier::Main, config.provider_id.clone(), config.model.clone());
    let secrets = Arc::new(EnvSecretProvider::new([ENV_API_KEY]));
    let transport = Arc::new(ReqwestTransport::new()?);
    Ok(SelectedProvider {
        provider: Arc::new(OpenAiCompatProvider::new(config, secrets, transport)),
        route,
    })
}

/// Which task class each tier is meant to serve.
///
/// This is the whole of the tier policy, in one table. `review` has no producer
/// in this build — nothing opens a turn with that class — so a `[model]
/// thinking` entry is recorded and ready rather than in use; naming it here is
/// how that stays a documented gap instead of an unused branch.
const TIER_TASKS: [(ModelTier, &str); 3] = [
    (ModelTier::Main, TaskClass::INTERACTIVE),
    (ModelTier::Fast, TaskClass::SUMMARIZE),
    (ModelTier::Thinking, TaskClass::REVIEW),
];

/// The routes for the tiers the configuration named, minus `main`.
///
/// Every entry carries the provider this run selected: a run still speaks to one
/// provider kind, and only the model name varies by tier. `main` is left out
/// because the run's own route already is it; the map is what `profile/resolved`
/// records as the rest of the table.
pub fn route_table(
    main: &ModelRoute,
    tiers: &BTreeMap<ModelTier, String>,
) -> BTreeMap<String, ModelRoute> {
    tiers
        .iter()
        .filter(|(tier, _)| **tier != ModelTier::Main)
        .map(|(tier, model)| {
            (
                tier.as_str().to_string(),
                ModelRoute::new(*tier, main.provider.clone(), model.clone()),
            )
        })
        .collect()
}

/// A router whose table is `routes`, falling back to `main`.
///
/// A tier nobody named contributes no entry, so the task class it serves falls
/// through to `main` through the router's own default — the same answer, reached
/// without a table entry claiming a decision that was never made.
pub fn router(main: ModelRoute, routes: &BTreeMap<String, ModelRoute>) -> TableModelRouter {
    let mut router = TableModelRouter::new(main);
    for (tier, task) in TIER_TASKS {
        if let Some(route) = routes.get(tier.as_str()) {
            router = router.with_task(task, route.clone());
        }
    }
    router
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_names_every_kind_it_has_in_one_order() {
        let registry = ProviderRegistry::builtin();
        assert_eq!(registry.kinds().collect::<Vec<_>>(), vec!["echo", "openai"]);
    }

    #[test]
    fn an_unknown_kind_is_refused_with_the_list_of_real_ones() {
        let error = match ProviderRegistry::builtin().build("gemini", None) {
            Ok(_) => panic!("no such kind"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "unknown AGENT_MODEL_PROVIDER 'gemini': use echo or openai");
    }

    #[test]
    fn capabilities_are_known_without_building_a_provider() {
        // The point of the catalog: `openai` can be asked what it supports even
        // though building it here would fail for lack of an endpoint.
        let registry = ProviderRegistry::builtin();
        let echo = registry.capabilities("echo").expect("echo is a kind");
        assert_eq!(echo.len(), 1);
        let openai = registry.capabilities("openai").expect("openai is a kind");
        assert!(openai.iter().any(|cap| cap.as_str() == ModelCapability::TOOL_CALL));
        assert!(registry.capabilities("gemini").is_none());
    }

    #[test]
    fn the_echo_kind_records_the_model_it_was_asked_for() {
        let selected = select_kind("echo", Some("named")).expect("echo always builds");
        assert_eq!(selected.route.provider, "echo");
        assert_eq!(selected.route.model, "named");
        assert_eq!(selected.route.tier, ModelTier::Main);
    }

    fn tiers(pairs: &[(ModelTier, &str)]) -> BTreeMap<ModelTier, String> {
        pairs.iter().map(|(tier, model)| (*tier, model.to_string())).collect()
    }

    #[test]
    fn the_route_table_carries_every_tier_but_main() {
        let main = ModelRoute::new(ModelTier::Main, "openai-compat", "big");
        let table =
            route_table(&main, &tiers(&[(ModelTier::Main, "big"), (ModelTier::Fast, "small")]));
        assert_eq!(table.len(), 1, "main is the run's route, not a table entry: {table:?}");
        let fast = &table["fast"];
        assert_eq!(fast.tier, ModelTier::Fast);
        assert_eq!(fast.model, "small");
        // Same provider: this build does not switch vendors mid-run.
        assert_eq!(fast.provider, "openai-compat");
    }

    #[test]
    fn a_summarize_task_gets_the_fast_route_and_anything_else_gets_main() {
        use agent_ports::model_router::{ModelBudget, ModelRouter, RouteRequest};
        use agent_protocol::ids::{EventSeq, SessionId, TurnId};

        let main = ModelRoute::new(ModelTier::Main, "openai-compat", "big");
        let table = route_table(
            &main,
            &tiers(&[(ModelTier::Fast, "small"), (ModelTier::Thinking, "deep")]),
        );
        let router = router(main, &table);

        let ask = |task: &str| {
            router
                .route(&RouteRequest {
                    session_id: SessionId::from_string("s"),
                    turn_id: TurnId::from_string("t"),
                    task: TaskClass::new(task),
                    context_revision: EventSeq::ZERO,
                    message_count: 1,
                    required_capabilities: Vec::new(),
                    budget: ModelBudget::default(),
                })
                .model
        };
        assert_eq!(ask(TaskClass::SUMMARIZE), "small");
        assert_eq!(ask(TaskClass::REVIEW), "deep");
        assert_eq!(ask(TaskClass::INTERACTIVE), "big");
        assert_eq!(ask("something-new"), "big");
    }

    #[test]
    fn a_configuration_that_named_no_tier_routes_everything_to_main() {
        use agent_ports::model_router::{ModelBudget, ModelRouter, RouteRequest};
        use agent_protocol::ids::{EventSeq, SessionId, TurnId};

        let main = ModelRoute::new(ModelTier::Main, "echo", "echo");
        let router = router(main, &BTreeMap::new());
        let route = router.route(&RouteRequest {
            session_id: SessionId::from_string("s"),
            turn_id: TurnId::from_string("t"),
            task: TaskClass::new(TaskClass::SUMMARIZE),
            context_revision: EventSeq::ZERO,
            message_count: 1,
            required_capabilities: Vec::new(),
            budget: ModelBudget::default(),
        });
        // A summarization turn on a run with one model is that model, which is
        // also what the log records — not a tier that was never configured.
        assert_eq!(route.model, "echo");
        assert_eq!(route.tier, ModelTier::Main);
    }
}
