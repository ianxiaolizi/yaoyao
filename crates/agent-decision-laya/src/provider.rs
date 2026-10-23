//! The provider: one Laya sidecar behind [`FastDecisionProvider`].
//!
//! The whole adapter is this file plus the two it leans on — the endpoint that
//! refuses to be remote, and the mapping that turns one vocabulary into the
//! other. What it does *not* own is the deadline: `laya-serve` runs inference
//! synchronously in a single worker, so a slow request stalls everything behind
//! it, and the design puts the bound for that on the broker (250ms per backend)
//! and the advisor (500ms for the call). The timeout here is a socket bound
//! only — enough that a wedged connection cannot hold a file descriptor for the
//! life of the process, and deliberately far larger than any decision deadline,
//! so it never becomes the thing that decides when a decision is late.

use std::time::Duration;

use agent_ports::decision::{
    DecisionError, DecisionResult, DecisionState, FastDecisionProvider, QuestionPack,
};
use async_trait::async_trait;

use crate::endpoint::{EndpointError, LayaEndpoint};
use crate::mapping::{self, DEFAULT_ORDINAL_LEVELS};
use crate::wire::{self, SystemOneResponse};

/// The confidence below which an answer is read as "no opinion".
///
/// Laya answers everything, so this is where the sidecar's certainty becomes
/// the project's [`DecisionBackendOutcome::Abstained`], and an abstention is
/// what hands the decision back to the rule baseline. Half is the neutral
/// choice: it is the only threshold that does not prefer one side of a binary,
/// and it is a starting point a profile may move.
///
/// [`DecisionBackendOutcome::Abstained`]: agent_ports::decision::DecisionBackendOutcome::Abstained
pub const DEFAULT_ABSTAIN_THRESHOLD: f64 = 0.5;

/// The socket bound described above. Not a decision deadline.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How to reach and configure one sidecar.
///
/// The product configuration surface (`[decisions.laya]` in a profile, parsed
/// into `agent_config::ResolvedLaya`) carries `url`, `model`, and `threshold`
/// only. The remaining three fields exist for library embedders: a binary that
/// builds this provider itself can set them, but a profile cannot, so the
/// ordinal scale and the socket bound are fixed at their defaults in the
/// shipped assembly.
#[derive(Debug, Clone)]
pub struct LayaConfig {
    /// The endpoint. Must be loopback; anything else is refused.
    pub url: String,
    /// The checkpoint to name (`multilingual`, `english`, `typed-decisions`).
    /// `None` lets the server route, which is the right default when only one
    /// checkpoint is installed.
    pub model: Option<String>,
    /// The abstention threshold. Defaults to [`DEFAULT_ABSTAIN_THRESHOLD`].
    pub threshold: Option<f64>,
    /// The levels an ordinal question is asked with when it names none.
    /// Library-only; not exposed through profiles.
    pub ordinal_levels: Option<Vec<String>>,
    /// What the trace should record as this backend's revision. Defaults to the
    /// model name, since that is the artifact the answers are attributable to.
    ///
    /// This is a **pin, not an observation**: the response's own `model` field
    /// is not read, so a server that routed to a different checkpoint than the
    /// one configured would still be recorded here under the configured name.
    /// Library-only; not exposed through profiles.
    pub revision: Option<String>,
    /// The socket bound. Defaults to [`DEFAULT_REQUEST_TIMEOUT`].
    /// Library-only; not exposed through profiles.
    pub request_timeout: Option<Duration>,
}

impl LayaConfig {
    /// A configuration for one endpoint, with every other field at its default.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            model: None,
            threshold: None,
            ordinal_levels: None,
            revision: None,
            request_timeout: None,
        }
    }
}

/// Why a provider could not be built.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LayaConfigError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error("abstention threshold {0} is outside 0..=1")]
    Threshold(f64),
    #[error("ordinal levels must number at least two, got {0}")]
    OrdinalLevels(usize),
    #[error("the HTTP client could not be built: {0}")]
    Client(String),
}

/// A fast-decision backend that asks a local Laya sidecar.
pub struct LayaDecisionProvider {
    endpoint: LayaEndpoint,
    client: reqwest::Client,
    model: Option<String>,
    revision: Option<String>,
    threshold: f64,
    /// The levels an ordinal question is asked with when it names none. Held
    /// once so every call normalizes against the same scale.
    ordinal_levels: Vec<String>,
}

impl LayaDecisionProvider {
    /// Validate the configuration and build the provider.
    pub fn new(config: LayaConfig) -> Result<Self, LayaConfigError> {
        let endpoint = LayaEndpoint::parse(&config.url)?;

        let threshold = config.threshold.unwrap_or(DEFAULT_ABSTAIN_THRESHOLD);
        if !(0.0..=1.0).contains(&threshold) {
            return Err(LayaConfigError::Threshold(threshold));
        }

        let ordinal_levels = config
            .ordinal_levels
            .unwrap_or_else(|| DEFAULT_ORDINAL_LEVELS.iter().map(|s| (*s).to_string()).collect());
        if ordinal_levels.len() < 2 {
            return Err(LayaConfigError::OrdinalLevels(ordinal_levels.len()));
        }

        let timeout = config.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| LayaConfigError::Client(error.to_string()))?;

        Ok(Self {
            endpoint,
            client,
            revision: config.revision.or_else(|| config.model.clone()),
            model: config.model,
            threshold,
            ordinal_levels,
        })
    }

    /// The endpoint this provider dials.
    pub fn endpoint(&self) -> &str {
        self.endpoint.url()
    }

    /// The checkpoint this provider names, when it names one.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// One request, with the transport-level outcomes kept apart from the
    /// protocol ones.
    ///
    /// The split is the one the trace depends on: **no HTTP response at all**
    /// is `unavailable` (the sidecar is not there), while **any response** —
    /// a 4xx, a 5xx, or a body that breaks off after the headers — is
    /// `protocol`, because the server was reached and the exchange failed on
    /// its side of the wire. That distinction matters more than it looks:
    /// `laya-serve` answers a malformed question with a 500, so a 5xx read as
    /// "the sidecar is down" would be the wrong story told every time this
    /// adapter asked something the server disliked. The same goes for a body
    /// cut short: the status line and headers arrived, so the sidecar was up;
    /// calling that an outage would hide a truncated response behind a
    /// reachability label.
    async fn post(
        &self,
        body: &wire::SystemOneRequest<'_>,
    ) -> Result<SystemOneResponse, DecisionError> {
        let response = self
            .client
            .post(self.endpoint.request_url())
            .json(body)
            .send()
            .await
            .map_err(|error| DecisionError::Unavailable(error.to_string()))?;

        let status = response.status();
        // A failure here is not an outage: the response has arrived and only
        // its body was lost, which is a protocol-level failure on the wire.
        let bytes = response.bytes().await.map_err(|error| {
            DecisionError::Protocol(format!(
                "the sidecar answered with headers but its body could not be read: {error}"
            ))
        })?;

        if !status.is_success() {
            return Err(DecisionError::Protocol(format!(
                "the sidecar answered HTTP {status} for {} question(s)",
                body.questions.len()
            )));
        }

        serde_json::from_slice(&bytes).map_err(|error| {
            DecisionError::Protocol(format!(
                "the sidecar answered something that is not a systemone response: {error}"
            ))
        })
    }
}

#[async_trait]
impl FastDecisionProvider for LayaDecisionProvider {
    async fn decide(
        &self,
        pack: &QuestionPack,
        state: &DecisionState,
    ) -> Result<DecisionResult, DecisionError> {
        // A pack with nothing in it is a question nobody asked. The server
        // refuses an empty `questions` object outright (500), so the honest
        // answer is the one the protocol already has for it.
        if pack.questions.is_empty() {
            return Ok(DecisionResult::abstained(pack));
        }

        let levels: Vec<&str> = self.ordinal_levels.iter().map(String::as_str).collect();
        let questions = mapping::questions_of(pack, &levels)?;

        // The restricted state, as facts alone: the ids and the log revision
        // are this project's accounting, and a side model has no use for them.
        let state_value = serde_json::Value::Object(state.facts.clone());
        let body =
            wire::SystemOneRequest { state: &state_value, questions, model: self.model.as_deref() };

        let response = self.post(&body).await?;
        mapping::answers_of(pack, &levels, &response.answers, self.threshold)
    }

    fn provider_id(&self) -> String {
        "laya".to_string()
    }

    fn revision(&self) -> Option<String> {
        // A pin, not an observation: `revision` is asked with no question in
        // hand, so it can only be what the configuration said this backend is.
        self.revision.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_endpoint_has_no_provider() {
        for url in ["http://0.0.0.0:8000", "http://laya.internal:8000"] {
            let error = LayaDecisionProvider::new(LayaConfig::new(url)).err().unwrap();
            assert!(
                matches!(error, LayaConfigError::Endpoint(EndpointError::NotLoopback(_))),
                "{url}: {error}"
            );
        }
    }

    #[test]
    fn a_threshold_outside_the_unit_interval_is_refused() {
        for threshold in [-0.1, 1.5] {
            let config = LayaConfig {
                threshold: Some(threshold),
                ..LayaConfig::new("http://127.0.0.1:8000")
            };
            assert_eq!(
                LayaDecisionProvider::new(config).err().unwrap(),
                LayaConfigError::Threshold(threshold)
            );
        }
    }

    #[test]
    fn too_few_ordinal_levels_is_refused() {
        let config = LayaConfig {
            ordinal_levels: Some(vec!["only".to_string()]),
            ..LayaConfig::new("http://127.0.0.1:8000")
        };
        assert_eq!(
            LayaDecisionProvider::new(config).err().unwrap(),
            LayaConfigError::OrdinalLevels(1)
        );
    }

    #[test]
    fn the_revision_defaults_to_the_checkpoint_it_was_pinned_to() {
        let named = LayaDecisionProvider::new(LayaConfig {
            model: Some("multilingual".to_string()),
            ..LayaConfig::new("http://127.0.0.1:8000")
        })
        .unwrap();
        assert_eq!(named.revision().as_deref(), Some("multilingual"));
        assert_eq!(named.model(), Some("multilingual"));

        // An explicit pin wins, because a checkpoint name is not a revision.
        let pinned = LayaDecisionProvider::new(LayaConfig {
            model: Some("multilingual".to_string()),
            revision: Some("55cf4c4ebb4ebe31b2550e8bdf3bd21b99753851".to_string()),
            ..LayaConfig::new("http://127.0.0.1:8000")
        })
        .unwrap();
        assert_eq!(pinned.revision().as_deref(), Some("55cf4c4ebb4ebe31b2550e8bdf3bd21b99753851"));

        // A server that routes on its own has nothing to be attributable to.
        let unrouted = LayaDecisionProvider::new(LayaConfig::new("http://127.0.0.1:8000")).unwrap();
        assert_eq!(unrouted.revision(), None);
    }

    #[test]
    fn the_provider_keeps_a_stable_id() {
        let provider = LayaDecisionProvider::new(LayaConfig::new("http://127.0.0.1:8000")).unwrap();
        assert_eq!(provider.provider_id(), "laya");
        // The endpoint is the parsed URL, so a bare origin comes back with the
        // path the parser filled in.
        assert_eq!(provider.endpoint(), "http://127.0.0.1:8000/");
    }
}
