//! Deriving a run's context budget from what its model can do.
//!
//! Pure arithmetic with no clock, network, or filesystem: the resolver is
//! called once after the route is known, and the same inputs must always
//! produce the same numbers so a recorded budget can be reproduced from the
//! snapshot.
//!
//! Two kinds of number live here. The *preset* chooses how much of the window
//! a run is willing to pay for; the model's capabilities say what physically
//! fits. An explicit configuration overrides either, but never past the
//! physical ceiling — a value that cannot fit is refused at startup rather
//! than discovered as a context overflow mid-turn.

use std::collections::BTreeMap;

use agent_protocol::model::{BudgetPreset, ModelCapabilitySource, ResolvedModelCapabilities};
use agent_protocol::profile::ResolvedContextBudget;

use crate::{ConfigError, Result};

/// Percent of the physical window the working set plus completion cap must
/// leave as safety margin.
const WINDOW_SAFETY_PERCENT: u64 = 95;
/// The auto completion cap as a share of the window, in percent.
const COMPLETION_SHARE_PERCENT: u64 = 25;
/// The auto compaction headroom as a share of the window, in percent.
const HEADROOM_SHARE_PERCENT: u64 = 6;
/// Floor for the auto compaction headroom, in tokens.
const HEADROOM_FLOOR_TOKENS: u32 = 32_768;
/// The default share of the working set at which compaction triggers.
const COMPACTION_TRIGGER_PERCENT: u64 = 80;
/// How many recent tokens compaction keeps, as a share of the window minus
/// the completion cap.
const RETAIN_WINDOW_PERCENT: u64 = 16;

/// What the configuration says about budgets, before a model is involved.
///
/// Every `Option` is an explicit operator choice; `None` means "derive it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetPolicy {
    pub preset: BudgetPreset,
    /// A window smaller than the model's, when a layer states one.
    pub context_window_tokens: Option<u32>,
    pub input_working_set_tokens: Option<u32>,
    pub completion_cap_tokens: Option<u32>,
    pub compaction_headroom_tokens: Option<u32>,
    pub retain_recent_tokens: Option<u32>,
    pub compact_at_percent: Option<u32>,
}

impl BudgetPolicy {
    /// The shipped policy: a quarter of the window, everything else derived.
    pub fn balanced() -> Self {
        Self {
            preset: BudgetPreset::Balanced,
            context_window_tokens: None,
            input_working_set_tokens: None,
            completion_cap_tokens: None,
            compaction_headroom_tokens: None,
            retain_recent_tokens: None,
            compact_at_percent: None,
        }
    }

    pub fn with_input_working_set_tokens(mut self, tokens: u32) -> Self {
        self.input_working_set_tokens = Some(tokens);
        self
    }
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self::balanced()
    }
}

/// The derived budget together with where each number came from.
///
/// The sources travel with the budget so a snapshot can answer "was this
/// 262,144 a discovered fact or a guess?" without replaying the resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetResolution {
    pub budget: ResolvedContextBudget,
    pub sources: BTreeMap<String, String>,
}

/// Derive a run's context budget from its model's capabilities.
pub fn resolve_context_budget(
    caps: &ResolvedModelCapabilities,
    policy: &BudgetPolicy,
) -> Result<BudgetResolution> {
    let physical = caps.capabilities.context_window_tokens;
    if physical == 0 {
        return Err(invalid_budget(
            "context_window_tokens",
            "the model's context window is zero; no request can fit in it".to_string(),
        ));
    }

    // W: the physical window, or a smaller one the operator chose. A larger
    // one is a claim the model cannot honor, so it is refused rather than
    // trusted.
    let (window, window_source) = match policy.context_window_tokens {
        Some(window) if window == 0 => {
            return Err(invalid_budget(
                "context_window_tokens",
                "`context_window_tokens` is zero; omit it to use the model's own window".to_string(),
            ));
        }
        Some(window) if window > physical => {
            return Err(invalid_budget(
                "context_window_tokens",
                format!(
                    "`context_window_tokens` ({window}) is larger than the model's physical \
                     window ({physical}); the model cannot accept more than {physical}"
                ),
            ));
        }
        Some(window) => (window, "explicit".to_string()),
        None => (physical, source_label(caps.source)),
    };

    let window_limit = ratio(window, WINDOW_SAFETY_PERCENT, 100);
    // O: the completion cap, which includes reasoning tokens.
    let auto_completion = ratio(window, COMPLETION_SHARE_PERCENT, 100)
        .min(caps.capabilities.max_output_tokens)
        .max(1);
    let (completion, completion_source) = match policy.completion_cap_tokens {
        Some(0) => {
            return Err(invalid_budget(
                "completion_cap_tokens",
                "`completion_cap_tokens` is zero, which asks the provider for no answer at all; \
                 omit it to derive a cap"
                    .to_string(),
            ));
        }
        Some(completion) => {
            if completion > caps.capabilities.max_output_tokens {
                return Err(invalid_budget(
                    "completion_cap_tokens",
                    format!(
                        "`completion_cap_tokens` ({completion}) exceeds the model's maximum \
                         output ({})",
                        caps.capabilities.max_output_tokens
                    ),
                ));
            }
            if u64::from(completion) + 1 > u64::from(window_limit) {
                return Err(invalid_budget(
                    "completion_cap_tokens",
                    format!(
                        "`completion_cap_tokens` ({completion}) leaves no room for input under \
                         the {WINDOW_SAFETY_PERCENT}% window limit of {window_limit}"
                    ),
                ));
            }
            (completion, "explicit".to_string())
        }
        None => (auto_completion, "derived:0.25W".to_string()),
    };

    // B: headroom kept free for compaction text.
    let auto_headroom = HEADROOM_FLOOR_TOKENS.max(ratio(window, HEADROOM_SHARE_PERCENT, 100));
    let (headroom, headroom_source) = match policy.compaction_headroom_tokens {
        Some(headroom) => {
            if headroom == 0 || headroom >= window.saturating_sub(completion) {
                return Err(invalid_budget(
                    "compaction_headroom_tokens",
                    format!(
                        "`compaction_headroom_tokens` ({headroom}) leaves no usable window \
                         beside the completion cap ({completion}) in a {window}-token window"
                    ),
                ));
            }
            (headroom, "explicit".to_string())
        }
        None => (
            auto_headroom.min(window.saturating_sub(completion).saturating_sub(1)).max(1),
            "derived:0.0625W".to_string(),
        ),
    };

    // I: the input working set.
    let input_ceiling = window_limit.saturating_sub(completion);
    let auto_input = ratio(window, u64::from(policy.preset.window_eighths()), 8).min(input_ceiling);
    let (input, input_source) = match policy.input_working_set_tokens {
        Some(0) => {
            return Err(invalid_budget(
                "input_working_set_tokens",
                "`input_working_set_tokens` is zero, which leaves no room for even the turn's \
                 own input; omit it to derive a working set"
                    .to_string(),
            ));
        }
        Some(input) => {
            if u64::from(input) + u64::from(completion) > u64::from(window_limit) {
                return Err(invalid_budget(
                    "input_working_set_tokens",
                    format!(
                        "`input_working_set_tokens` ({input}) + completion cap ({completion}) \
                         exceeds the {WINDOW_SAFETY_PERCENT}% window limit of {window_limit} \
                         ({window}); keep the working set plus its completion cap under {window_limit}"
                    ),
                ));
            }
            if input == 0 {
                return Err(invalid_budget(
                    "input_working_set_tokens",
                    "`input_working_set_tokens` derives to zero; the preset leaves no input \
                     room beside the completion cap"
                        .to_string(),
                ));
            }
            (input, "explicit".to_string())
        }
        None => {
            if auto_input == 0 {
                return Err(invalid_budget(
                    "input_working_set_tokens",
                    format!(
                        "the completion cap ({completion}) leaves no input room under the \
                         {WINDOW_SAFETY_PERCENT}% limit of a {window}-token window; lower \
                         `completion_cap_tokens` or choose a smaller preset"
                    ),
                ));
            }
            (auto_input, format!("preset:{}", policy.preset.as_str()))
        }
    };

    // C: where compaction triggers, measured on the full request.
    let trigger_percent = policy.compact_at_percent.unwrap_or(COMPACTION_TRIGGER_PERCENT as u32);
    if trigger_percent == 0 || trigger_percent > 99 {
        return Err(invalid_budget(
            "compact_at_percent",
            format!(
                "`compact_at_percent` is {trigger_percent}, which is not a share of the budget; \
                 use 1..=99"
            ),
        ));
    }
    let structural_room = window.saturating_sub(completion).saturating_sub(headroom);
    let trigger = ratio(window, u64::from(trigger_percent), 100)
        .min(ratio(input, u64::from(trigger_percent), 100))
        .min(structural_room);
    if trigger == 0 {
        return Err(invalid_budget(
            "compact_at_percent",
            format!(
                "the compaction trigger derives to zero in a {window}-token window; the \
                 completion cap and headroom leave no history to compact"
            ),
        ));
    }
    let trigger_source = match policy.compact_at_percent {
        Some(percent) => format!("compact_at_percent:{percent}"),
        None => "default:0.8".to_string(),
    };

    // R: how much recent history compaction keeps whole.
    let auto_retained = ratio(
        window.saturating_sub(completion),
        RETAIN_WINDOW_PERCENT,
        100,
    )
    .min(input / 2);
    let (retained, retained_source) = match policy.retain_recent_tokens {
        Some(retained) => {
            if retained >= trigger {
                return Err(invalid_budget(
                    "retain_recent_tokens",
                    format!(
                        "`retain_recent_tokens` ({retained}) is not smaller than the compaction \
                         trigger ({trigger}); retention must leave something to compact"
                    ),
                ));
            }
            (retained, "explicit".to_string())
        }
        None => (auto_retained, "derived:0.16W".to_string()),
    };

    let budget = ResolvedContextBudget {
        context_window_tokens: window,
        input_working_set_tokens: input,
        completion_cap_tokens: completion,
        completion_cap_ceiling_tokens: caps.capabilities.max_output_tokens,
        compaction_headroom_tokens: headroom,
        compaction_trigger_tokens: trigger,
        retain_recent_tokens: retained,
    };
    let mut sources = BTreeMap::new();
    sources.insert("context_window_tokens".to_string(), window_source);
    sources.insert("completion_cap_tokens".to_string(), completion_source);
    sources.insert("compaction_headroom_tokens".to_string(), headroom_source);
    sources.insert("input_working_set_tokens".to_string(), input_source);
    sources.insert("compaction_trigger_tokens".to_string(), trigger_source);
    sources.insert("retain_recent_tokens".to_string(), retained_source);
    Ok(BudgetResolution { budget, sources })
}

fn source_label(source: ModelCapabilitySource) -> String {
    match source {
        ModelCapabilitySource::Provider => "provider:/models".to_string(),
        ModelCapabilitySource::Bundled => "bundled".to_string(),
        ModelCapabilitySource::Fallback => "fallback".to_string(),
    }
}

/// `value * numerator / denominator`, in 64-bit, floored.
fn ratio(value: u32, numerator: u64, denominator: u64) -> u32 {
    u32::try_from(u64::from(value) * numerator / denominator).unwrap_or(u32::MAX)
}

fn invalid_budget(field: &str, detail: String) -> ConfigError {
    ConfigError::InvalidBudget {
        layer: format!("budget.{field}"),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::model::{ModelCapabilities, ReasoningEffort};

    fn deepseek_flash_caps() -> ResolvedModelCapabilities {
        ResolvedModelCapabilities {
            capabilities: ModelCapabilities {
                context_window_tokens: 1_048_576,
                max_output_tokens: 393_216,
                reasoning_efforts: vec![
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ],
                default_reasoning_effort: Some(ReasoningEffort::High),
            },
            source: ModelCapabilitySource::Provider,
        }
    }

    #[test]
    fn balanced_preset_derives_the_deepseek_budget() {
        let caps = deepseek_flash_caps();
        let resolution = resolve_context_budget(&caps, &BudgetPolicy::balanced()).unwrap();
        assert_eq!(resolution.budget.context_window_tokens, 1_048_576);
        assert_eq!(resolution.budget.completion_cap_tokens, 262_144);
        assert_eq!(resolution.budget.input_working_set_tokens, 262_144);
        assert_eq!(resolution.budget.compaction_trigger_tokens, 209_715);
        assert_eq!(resolution.budget.retain_recent_tokens, 125_829);
    }

    #[test]
    fn explicit_input_override_is_rejected_when_it_cannot_fit_with_output() {
        let caps = deepseek_flash_caps();
        let policy = BudgetPolicy::balanced().with_input_working_set_tokens(900_000);
        let error = resolve_context_budget(&caps, &policy).unwrap_err().to_string();
        assert!(error.contains("input_working_set_tokens"));
        assert!(error.contains("95%"));
    }

    #[test]
    fn the_preset_moves_the_working_set_without_touching_the_cap() {
        let caps = deepseek_flash_caps();
        let frugal = resolve_context_budget(
            &caps,
            &BudgetPolicy { preset: BudgetPreset::Frugal, ..BudgetPolicy::balanced() },
        )
        .unwrap();
        assert_eq!(frugal.budget.input_working_set_tokens, 131_072);
        assert_eq!(frugal.budget.completion_cap_tokens, 262_144);
    }

    #[test]
    fn an_explicit_cap_larger_than_the_model_allows_is_refused() {
        let caps = deepseek_flash_caps();
        let policy = BudgetPolicy { completion_cap_tokens: Some(400_000), ..BudgetPolicy::balanced() };
        let error = resolve_context_budget(&caps, &policy).unwrap_err().to_string();
        assert!(error.contains("completion_cap_tokens"));
    }

    #[test]
    fn a_fallback_window_still_derives_a_usable_budget() {
        let caps = ResolvedModelCapabilities {
            capabilities: ModelCapabilities {
                context_window_tokens: 131_072,
                max_output_tokens: 32_768,
                reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
                default_reasoning_effort: Some(ReasoningEffort::High),
            },
            source: ModelCapabilitySource::Fallback,
        };
        let resolution = resolve_context_budget(&caps, &BudgetPolicy::balanced()).unwrap();
        assert_eq!(resolution.budget.context_window_tokens, 131_072);
        assert_eq!(resolution.budget.input_working_set_tokens, 32_768);
        assert_eq!(resolution.budget.completion_cap_tokens, 32_768);
        assert_eq!(resolution.sources["context_window_tokens"], "fallback");
    }
}
