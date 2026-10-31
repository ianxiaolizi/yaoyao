//! Model capability discovery.
//!
//! `GET /models` is the only place an OpenAI-compatible endpoint states what a
//! model can actually hold; when it answers usefully that answer outranks
//! anything this build remembers. The parsing here is deliberately forgiving
//! about shapes and strict about meaning: a response that names no usable
//! window is not a capability, and the caller falls back rather than reading a
//! zero as a budget.

use agent_protocol::model::{
    ModelCapabilitySource, ModelCapabilities, ReasoningEffort, ResolvedModelCapabilities,
};
use serde::Deserialize;

/// Context window of the fallback capability set.
///
/// Conservative on purpose: a wrong guess that is too small costs a retry, and
/// a wrong guess that is too large loses the answer to a context overflow.
pub const FALLBACK_CONTEXT_WINDOW_TOKENS: u32 = 131_072;
/// Completion ceiling of the fallback capability set.
pub const FALLBACK_MAX_OUTPUT_TOKENS: u32 = 32_768;

/// The capability set used when nothing better is known.
pub fn fallback_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        context_window_tokens: FALLBACK_CONTEXT_WINDOW_TOKENS,
        max_output_tokens: FALLBACK_MAX_OUTPUT_TOKENS,
        reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
        default_reasoning_effort: Some(ReasoningEffort::High),
    }
}

/// What this build knows about a model without asking the endpoint.
///
/// Only models whose numbers were verified against a real endpoint live here.
/// `None` means "ask, or fall back", never "0".
pub fn bundled_capabilities(model: &str) -> Option<ModelCapabilities> {
    match model {
        "deepseek-flash" | "deepseek-v4-flash" => Some(ModelCapabilities {
            context_window_tokens: 1_048_576,
            max_output_tokens: 393_216,
            reasoning_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
            default_reasoning_effort: Some(ReasoningEffort::High),
        }),
        _ => None,
    }
}

/// One entry of a `/models` answer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireModel {
    #[serde(default)]
    pub id: Option<String>,
    /// The vendor's spelling for the physical window. Some send both this and
    /// `max_context_window`; either is accepted.
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_efforts: Option<Vec<String>>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
}

/// A `/models` answer: `{"data":[...]}` or a bare array.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WireModelsResponse {
    Envelope { data: Vec<WireModel> },
    List(Vec<WireModel>),
}

impl WireModelsResponse {
    fn entries(self) -> Vec<WireModel> {
        match self {
            Self::Envelope { data } => data,
            Self::List(list) => list,
        }
    }
}

/// Parse a `/models` body and pick the entry for `model`.
///
/// Returns `None` for every reason the answer cannot be used as fact: not JSON,
/// no entry for the model, or an entry without a positive window and output
/// ceiling. The caller then moves to the bundled catalog rather than to zero.
pub fn parse_model_capabilities(body: &str, model: &str) -> Option<ModelCapabilities> {
    let response: WireModelsResponse = serde_json::from_str(body).ok()?;
    let entry = response.entries().into_iter().find(|entry| {
        entry.id.as_deref().is_some_and(|id| id == model)
    })?;
    let context_window_tokens = entry.context_window.or(entry.max_context_window)?;
    let max_output_tokens = entry.max_output_tokens?;
    if context_window_tokens == 0 || max_output_tokens == 0 {
        return None;
    }
    let reasoning_efforts = entry
        .reasoning_efforts
        .unwrap_or_default()
        .iter()
        .filter_map(|name| parse_effort(name))
        .collect();
    Some(ModelCapabilities {
        context_window_tokens,
        // A ceiling larger than the window can only be a vendor mistake; the
        // window is the tighter truth.
        max_output_tokens: max_output_tokens.min(context_window_tokens),
        reasoning_efforts,
        default_reasoning_effort: entry
            .default_reasoning_effort
            .as_deref()
            .and_then(parse_effort),
    })
}

fn parse_effort(name: &str) -> Option<ReasoningEffort> {
    match name.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => Some(ReasoningEffort::Off),
        "low" | "minimal" => Some(ReasoningEffort::Low),
        "high" | "medium" => Some(ReasoningEffort::High),
        "max" | "maximum" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

/// Resolve capabilities without a network: the bundled catalog, then fallback.
pub fn capabilities_without_provider(model: &str) -> ResolvedModelCapabilities {
    match bundled_capabilities(model) {
        Some(capabilities) => {
            ResolvedModelCapabilities { capabilities, source: ModelCapabilitySource::Bundled }
        }
        None => ResolvedModelCapabilities {
            capabilities: fallback_capabilities(),
            source: ModelCapabilitySource::Fallback,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_and_a_bare_list_parse_the_same() {
        let envelope = r#"{"data":[{"id":"m","context_window":1000,"max_output_tokens":100}]}"#;
        let bare = r#"[{"id":"m","context_window":1000,"max_output_tokens":100}]"#;
        assert_eq!(
            parse_model_capabilities(envelope, "m"),
            parse_model_capabilities(bare, "m")
        );
    }

    #[test]
    fn max_context_window_is_accepted_when_context_window_is_absent() {
        let body = r#"{"data":[{"id":"m","max_context_window":2048,"max_output_tokens":128}]}"#;
        let caps = parse_model_capabilities(body, "m").expect("a usable entry");
        assert_eq!(caps.context_window_tokens, 2_048);
    }

    #[test]
    fn an_entry_without_a_window_is_not_a_capability() {
        assert!(parse_model_capabilities(r#"{"data":[{"id":"m"}]}"#, "m").is_none());
        assert!(
            parse_model_capabilities(
                r#"{"data":[{"id":"m","context_window":0,"max_output_tokens":10}]}"#,
                "m"
            )
            .is_none()
        );
    }

    #[test]
    fn the_bundled_catalog_holds_the_verified_deepseek_numbers() {
        let caps = bundled_capabilities("deepseek-flash").expect("known model");
        assert_eq!(caps.context_window_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 393_216);
        assert_eq!(caps.default_reasoning_effort, Some(ReasoningEffort::High));
        // An unknown model must not inherit those numbers.
        assert!(bundled_capabilities("something-else").is_none());
    }

    #[test]
    fn without_a_provider_the_source_says_which_kind_of_guess_it_was() {
        let known = capabilities_without_provider("deepseek-flash");
        assert_eq!(known.source, ModelCapabilitySource::Bundled);
        let unknown = capabilities_without_provider("something-else");
        assert_eq!(unknown.source, ModelCapabilitySource::Fallback);
        assert_eq!(unknown.capabilities.context_window_tokens, FALLBACK_CONTEXT_WINDOW_TOKENS);
    }
}
