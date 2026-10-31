# Model-Adaptive Context Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Replace 爻爻's hard-coded 8k context/output budgets with model-derived budgets, explicit reasoning effort, idle-based streaming timeouts, and one-shot recovery from reasoning-only output truncation.

**Architecture:** Add provider-neutral model capability types, let the OpenAI-compatible adapter resolve `/models` capabilities, resolve a frozen `ContextBudget` after the run route is known, and make the loop/context engine consume only that resolved budget. Keep run spend and transport timing as separate axes: run limits act at step boundaries, while stream progress is protected by first-event and idle timeouts.

**Tech Stack:** Rust workspace; `agent-protocol`, `agent-ports`, `agent-config`, `agent-model-openai`, `agent-context`, `agent-loop`, `apps/agent`; Tokio; serde; reqwest.

**Spec:** `docs/superpowers/specs/2026-10-31-context-budget-auto-adaptation-design.md`

## Global Constraints

- No `unsafe` code.
- Model capability discovery and HTTP remain inside `agent-model-openai`; kernel and loop stay vendor-neutral.
- Budget resolution is pure: no network, clock, or filesystem access inside the resolver.
- The resolved budget is frozen after the route is selected and must not change inside a turn.
- `completion_cap_tokens` includes reasoning tokens; it is never derived from the old reservation field.
- `max_total_tokens` and run wall clock are checked only at step boundaries and never abort an in-flight model request.
- Automatic values for DeepSeek V4 Flash: `W=1048576`, `O=262144`, `B=65536`, `I=262144`, `C=209715`, `R=125829` under the `balanced` preset.
- Unknown model fallback: `W=131072`, `O=32768`, and the source must be recorded as `fallback`.
- Old TOML names `max_input_tokens` and `max_output_tokens` remain accepted as aliases; `reserved_output_tokens` is accepted but ignored with a note.
- Every task ends with its focused tests passing before commit.
- Final gate: `powershell -ExecutionPolicy Bypass -File scripts\test.ps1`.

## Review Focus

- A partial or malformed `/models` response must not silently become an unsafe 8k cap; the fallback and source must be explicit.
- Fast/thinking routes may have different windows; summary requests must not reuse the main route's capacity blindly.
- A stream that keeps producing events must survive past the old 120-second total timeout, while a stalled stream still terminates.
- A reasoning-only `finish_reason=length` must never be reported as a final answer or duplicate settled output.
- Old configs and old snapshots must still deserialize after the budget field refactor.

---

### Task 1: Protocol Capability and Reasoning Types

**Files:**
- Modify: `crates/agent-protocol/src/model.rs`
- Modify: `crates/agent-protocol/src/profile.rs`
- Test: inline `#[cfg(test)]` modules in both files

**Interfaces:**
- Consumes: existing serde and protocol conventions.
- Produces: `ReasoningEffort`, `ModelCapabilities`, `ResolvedModelCapabilities`, `ModelCapabilitySource`, `BudgetPreset`, `ResolvedContextBudget`, and `ModelBudget.reasoning_effort`.

- [x] **Step 1: Write failing protocol tests**

Add tests asserting:

```rust
#[test]
fn model_capabilities_round_trip_through_json() {
    let caps = ModelCapabilities {
        context_window_tokens: 1_048_576,
        max_output_tokens: 393_216,
        reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High, ReasoningEffort::Max],
        default_reasoning_effort: Some(ReasoningEffort::High),
    };
    let encoded = serde_json::to_string(&caps).unwrap();
    assert_eq!(serde_json::from_str::<ModelCapabilities>(&encoded).unwrap(), caps);
}

#[test]
fn a_model_budget_carries_reasoning_effort_without_changing_max_tokens() {
    let budget = ModelBudget {
        max_tokens: Some(262_144),
        reasoning_effort: Some(ReasoningEffort::High),
        ..ModelBudget::default()
    };
    assert_eq!(budget.max_tokens, Some(262_144));
    assert_eq!(budget.reasoning_effort, Some(ReasoningEffort::High));
}
```

- [x] **Step 2: Run the failing tests**

Run: `cargo test -p agent-protocol model_capabilities_round_trip_through_json`

Run: `cargo test -p agent-protocol a_model_budget_carries_reasoning_effort_without_changing_max_tokens`

Expected: FAIL because the new types and field do not exist.

- [x] **Step 3: Implement the protocol types**

Add `ReasoningEffort` with `Off`, `Low`, `High`, `Max`, serde `snake_case`, and a stable `as_str()`. Add `ModelCapabilities` with the exact fields from the spec. Add `ResolvedModelCapabilities { capabilities, source }` and `ModelCapabilitySource { Provider, Bundled, Fallback }`. Add `BudgetPreset` with `Frugal`, `Balanced`, `Deep`, `Max`. Add `ResolvedContextBudget` with:

```rust
pub struct ResolvedContextBudget {
    pub context_window_tokens: u32,
    pub input_working_set_tokens: u32,
    pub completion_cap_tokens: u32,
    pub completion_cap_ceiling_tokens: u32,
    pub compaction_headroom_tokens: u32,
    pub compaction_trigger_tokens: u32,
    pub retain_recent_tokens: u32,
}
```

Add `reasoning_effort: Option<ReasoningEffort>` to `ModelBudget` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.

- [x] **Step 4: Run protocol tests**

Run: `cargo test -p agent-protocol`

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/agent-protocol/src/model.rs crates/agent-protocol/src/profile.rs
git commit -m "feat: add model capability and reasoning protocol types"
```

### Task 2: Discover Model Capabilities and Send Reasoning Effort

**Files:**
- Modify: `crates/agent-ports/src/model_provider.rs`
- Create: `crates/agent-model-openai/src/catalog.rs`
- Modify: `crates/agent-model-openai/src/config.rs`
- Modify: `crates/agent-model-openai/src/transport.rs`
- Modify: `crates/agent-model-openai/src/provider.rs`
- Modify: `crates/agent-model-openai/src/wire.rs`
- Modify: `crates/agent-model-openai/src/lib.rs`
- Modify: `crates/agent-model-openai/tests/contract.rs`

**Interfaces:**
- Consumes: `ModelCapabilities` and `ReasoningEffort` from Task 1.
- Produces: `ModelProvider::model_capabilities(&self, model: &str) -> Result<ResolvedModelCapabilities, ModelProviderError>` and `Transport::get(...)`.

- [x] **Step 1: Write failing adapter tests**

Add a fake catalog response for `deepseek-flash` and tests asserting:

```rust
#[tokio::test]
async fn models_endpoint_maps_deepseek_capabilities() {
    let provider = catalog_provider(r#"{"data":[{"id":"deepseek-flash","context_window":1048576,"max_output_tokens":393216,"reasoning_efforts":["low","high","max"],"default_reasoning_effort":"high"}]}"#);
    let caps = provider.model_capabilities("deepseek-flash").await.unwrap().capabilities;
    assert_eq!(caps.context_window_tokens, 1_048_576);
    assert_eq!(caps.max_output_tokens, 393_216);
    assert_eq!(caps.default_reasoning_effort, Some(ReasoningEffort::High));
}

#[tokio::test]
async fn an_unusable_models_response_uses_the_bundled_catalog() {
    let provider = catalog_provider("not json");
    let caps = provider.model_capabilities("deepseek-flash").await.unwrap().capabilities;
    assert_eq!(caps.context_window_tokens, 1_048_576);
}

#[test]
fn chat_request_sends_reasoning_effort_but_not_the_reservation_as_max_tokens() {
    let request = request_with_budget(ModelBudget {
        max_tokens: Some(262_144),
        reasoning_effort: Some(ReasoningEffort::Max),
        ..ModelBudget::default()
    });
    let wire = chat_request(&config(), &request).unwrap();
    assert_eq!(wire.max_tokens, Some(262_144));
    assert_eq!(wire.reasoning_effort, Some(ReasoningEffort::Max));
}
```

- [x] **Step 2: Run the failing adapter tests**

Run: `cargo test -p agent-model-openai models_endpoint_maps_deepseek_capabilities`

Run: `cargo test -p agent-model-openai an_unusable_models_response_uses_the_bundled_catalog`

Run: `cargo test -p agent-model-openai chat_request_sends_reasoning_effort_but_not_the_reservation_as_max_tokens`

Expected: FAIL because discovery and the wire field do not exist.

- [x] **Step 3: Extend the provider and transport seam**

Add an async `model_capabilities` method to `ModelProvider`. Give it a default implementation returning `ModelProviderError::Protocol("model capabilities are not supported")`, and override it for echo with the fallback capability set. Extend `Transport` with:

```rust
async fn get(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
    Err(TransportError::Connect("GET is not supported by this transport".to_string()))
}
```

Implement `get` in `ReqwestTransport`.

- [x] **Step 4: Implement catalog parsing and reasoning serialization**

Create `catalog.rs` with a `WireModelsResponse` that accepts both `{"data":[...]}` and a top-level array. Parse `context_window`, `max_context_window`, `max_output_tokens`, `reasoning_efforts`, and `default_reasoning_effort`. Cache successful results in a `Mutex<HashMap<String, ModelCapabilities>>` on the provider. On HTTP or parse failure, use the bundled DeepSeek/echo catalog.

Add `reasoning_effort: Option<ReasoningEffort>` to `ChatRequest` with `skip_serializing_if`.

- [x] **Step 5: Run adapter tests**

Run: `cargo test -p agent-model-openai`

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add crates/agent-ports/src/model_provider.rs crates/agent-model-openai
git commit -m "feat: discover model capabilities and send reasoning effort"
```
### Task 3: Pure Model-Adaptive Budget Resolver

**Files:**
- Modify: `crates/agent-config/src/layers.rs`
- Modify: `crates/agent-config/src/resolve.rs`
- Modify: `crates/agent-config/src/lib.rs`
- Create: `crates/agent-config/src/budget.rs`
- Modify: `crates/agent-protocol/src/profile.rs`
- Test: inline tests in `crates/agent-config/src/lib.rs` and `crates/agent-config/src/budget.rs`

**Interfaces:**
- Consumes: `ResolvedModelCapabilities`, `BudgetPreset`, `ResolvedContextBudget`, `ReasoningEffort`.
- Produces: `resolve_context_budget(caps: &ResolvedModelCapabilities, policy: &BudgetPolicy) -> Result<BudgetResolution, ConfigError>` and `RunBudget.context: Option<ResolvedContextBudget>`.

`BudgetPolicy` has these exact fields:

```rust
pub struct BudgetPolicy {
    pub preset: BudgetPreset,
    pub context_window_tokens: Option<u32>,
    pub input_working_set_tokens: Option<u32>,
    pub completion_cap_tokens: Option<u32>,
    pub compaction_headroom_tokens: Option<u32>,
    pub retain_recent_tokens: Option<u32>,
    pub compact_at_percent: Option<u32>,
}
```

`BudgetResolution` has these exact fields:

```rust
pub struct BudgetResolution {
    pub budget: ResolvedContextBudget,
    pub sources: BTreeMap<String, String>,
}
```

- [x] **Step 1: Write failing resolver tests**

Add tests with these exact assertions:

```rust
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
fn old_budget_names_still_deserialize_as_aliases() {
    let budget: BudgetSpec = toml::from_str(
        "max_input_tokens = 24000\nmax_output_tokens = 32768\nreserved_output_tokens = 2000",
    ).unwrap();
    assert_eq!(budget.input_working_set_tokens, Some(24_000));
    assert_eq!(budget.completion_cap_tokens, Some(32_768));
}
```

- [x] **Step 2: Run the failing resolver tests**

Run: `cargo test -p agent-config balanced_preset_derives_the_deepseek_budget`

Run: `cargo test -p agent-config explicit_input_override_is_rejected_when_it_cannot_fit_with_output`

Run: `cargo test -p agent-config old_budget_names_still_deserialize_as_aliases`

Expected: FAIL because `BudgetPolicy`, the resolver, and the aliases do not exist.

- [x] **Step 3: Add configuration fields and compatibility aliases**

In `BudgetSpec`, add:
- `preset: Option<BudgetPreset>`
- `input_working_set_tokens: Option<u32>` with `#[serde(alias = "max_input_tokens")]`
- `completion_cap_tokens: Option<u32>` with `#[serde(alias = "max_output_tokens")]`
- `compaction_headroom_tokens: Option<u32>`
- `retain_recent_tokens: Option<u32>`
- keep `reserved_output_tokens: Option<u32>` for compatibility only.

In `ModelSpec`, add `reasoning_effort: Option<ReasoningEffort>`.

In `RunBudget`, add:
- `preset: BudgetPreset` with serde default `Balanced`
- `reasoning_effort: Option<ReasoningEffort>`
- `context: Option<ResolvedContextBudget>`
- `summary_max_output_tokens: Option<u32>`
- `budget_sources: BTreeMap<String, String>` with serde default and skip when empty.

Keep the old struct fields during this task so old snapshots compile; the resolver will become the only authority for new runs.

- [x] **Step 4: Implement the pure resolver**

Create `budget.rs` containing `BudgetPolicy`, `BudgetResolution`, and `resolve_context_budget`. Use integer arithmetic for every ratio. Apply the formulas and constraints from the spec exactly. Return `ConfigError::InvalidBudget` for invalid explicit overrides. Automatic fallback values are clamped, not rejected.

- [x] **Step 5: Wire resolved fields into config resolution**

`resolve.rs` must:
- merge the new fields across layers;
- emit a deprecation note for `reserved_output_tokens`;
- leave `input_working_set_tokens` and `completion_cap_tokens` as `None` when auto resolution is requested;
- carry `preset` and `reasoning_effort` into `ResolvedProfile`;
- stop filling the old 8k defaults when a model capability source is expected.

- [x] **Step 6: Run config tests**

Run: `cargo test -p agent-config`

Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add crates/agent-protocol/src/profile.rs crates/agent-config
git commit -m "feat: derive context budgets from model capabilities"
```

### Task 4: Resolve and Audit Budgets During Stack Assembly

**Files:**
- Modify: `apps/agent/src/providers.rs`
- Modify: `apps/agent/src/stack.rs`
- Modify: `apps/agent/src/support.rs`
- Modify: `crates/agent-protocol/src/profile.rs`
- Modify: `crates/agent-config/src/resolve.rs`
- Test: `apps/agent/tests/m4_acceptance.rs`
- Test: `apps/agent/tests/cli_end_to_end.rs`

**Interfaces:**
- Consumes: `ModelProvider::model_capabilities`, `resolve_context_budget`, `BudgetPolicy`.
- Produces: `ResolvedProfileSnapshot.model_capabilities: BTreeMap<String, ModelCapabilities>`, `ResolvedProfileSnapshot.budget_sources: BTreeMap<String, String>`, and `RunBudget.summary_max_output_tokens: Option<u32>`.

- [x] **Step 1: Write failing assembly tests**

Add a test provider with discovered capabilities and assert:

```rust
#[tokio::test]
async fn assembly_freezes_discovered_capabilities_and_budget_sources() {
    let runtime = assemble_with_fake_caps(deepseek_flash_caps()).await.unwrap();
    let profile = runtime.snapshot_for_test();
    assert_eq!(profile.budget.context.unwrap().completion_cap_tokens, 262_144);
    assert_eq!(
        profile.budget_sources.get("context_window_tokens").map(String::as_str),
        Some("provider:/models")
    );
    assert_eq!(
        profile.budget_sources.get("input_working_set_tokens").map(String::as_str),
        Some("preset:balanced")
    );
}
```

Also assert that an injected provider whose discovery fails still assembles with `fallback` recorded.

- [x] **Step 2: Run the failing assembly tests**

Run: `cargo test -p agent assembly_freezes_discovered_capabilities_and_budget_sources`

Expected: FAIL because assembly does not resolve capabilities and the snapshot lacks the new fields.

- [x] **Step 3: Discover capabilities for every route**

In `stack::assemble`, after the selected provider and route table exist, call `model_capabilities` for:
- `selected.route.model`;
- every model in `routes` (fast/thinking tiers).

Build `BTreeMap<String, ModelCapabilities>` keyed by `provider/model`. If a route's discovery fails, use its bundled fallback and record the source. Set `RunBudget.summary_max_output_tokens` from the `fast` route's capability when that tier exists; otherwise use the main capability.

- [x] **Step 4: Resolve the main budget before building consumers**

Resolve the main route's `ContextBudget` before:
- `ContextPolicy` is constructed;
- `GuardedToolProvider::with_offload_limit` is called;
- the `ResolvedProfileSnapshot` is sealed.

Set `RunBudget.context`, `RunBudget.reasoning_effort`, and `budget_sources` on the resolved profile. Reject explicit overrides that exceed discovered capabilities with a message naming the route and field.

- [x] **Step 5: Put capabilities and sources in the snapshot**

Add `model_capabilities` and `budget_sources` to `ResolvedProfileSnapshot` with `serde(default, skip_serializing_if = ...)` so older snapshots still deserialize. Update `into_snapshot` to copy both maps. Update the `config_hash` expectations in tests to include these fields.

- [x] **Step 6: Run focused assembly tests**

Run: `cargo test -p agent assembly_ -- --nocapture`

Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add apps/agent/src/providers.rs apps/agent/src/stack.rs apps/agent/src/support.rs crates/agent-protocol/src/profile.rs crates/agent-config/src/resolve.rs apps/agent/tests/m4_acceptance.rs apps/agent/tests/cli_end_to_end.rs
git commit -m "feat: resolve and audit budgets during stack assembly"
```

### Task 5: Replace Total Request Timeout with First-Event and Idle Timeouts

**Files:**
- Modify: `crates/agent-model-openai/src/config.rs`
- Modify: `crates/agent-model-openai/src/transport.rs`
- Modify: `crates/agent-model-openai/src/provider.rs`
- Modify: `crates/agent-model-openai/src/lib.rs`
- Modify: `crates/agent-model-openai/tests/contract.rs`
- Modify: `apps/agent/src/providers.rs`
- Test: `crates/agent-model-openai/tests/live_endpoint.rs`

**Interfaces:**
- Consumes: existing transport and streaming code.
- Produces: `OpenAiCompatConfig.first_event_timeout_ms`, `OpenAiCompatConfig.stream_idle_timeout_ms`, and `TransportRequest.connect_timeout_ms`.

- [x] **Step 1: Write failing timeout tests**

Add a fake chunk stream and tests asserting:

```rust
#[tokio::test]
async fn a_continually_streaming_response_is_not_killed_by_the_old_total_timeout() {
    let provider = provider_with_timeouts(20, 20, events_every(10, 5));
    let response = provider.stream(&request(), &NullSink).await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::STOP);
}

#[tokio::test]
async fn a_stream_with_no_event_after_the_idle_limit_times_out() {
    let provider = provider_with_timeouts(20, 20, silent_after_first());
    let error = provider.stream(&request(), &NullSink).await.unwrap_err();
    assert!(matches!(error, ModelProviderError::Timeout { .. }));
}

#[tokio::test]
async fn the_first_event_limit_applies_before_headers_are_returned() {
    let provider = provider_with_slow_post(50, 20, 20);
    let error = provider.stream(&request(), &NullSink).await.unwrap_err();
    assert!(matches!(error, ModelProviderError::Timeout { .. }));
}
```

- [x] **Step 2: Run the failing timeout tests**

Run: `cargo test -p agent-model-openai a_continually_streaming_response_is_not_killed_by_the_old_total_timeout`

Run: `cargo test -p agent-model-openai a_stream_with_no_event_after_the_idle_limit_times_out`

Run: `cargo test -p agent-model-openai the_first_event_limit_applies_before_headers_are_returned`

Expected: FAIL because the provider still has one whole-request timeout.

- [x] **Step 3: Split the timeout configuration**

Replace the single `timeout_ms` field with:
- `connect_timeout_ms`, default `30_000`;
- `first_event_timeout_ms`, default `60_000`;
- `stream_idle_timeout_ms`, default `300_000`.

Keep `DEFAULT_TIMEOUT_MS` as a deprecated alias for `60_000` until all callers migrate. Set `TransportRequest.connect_timeout_ms` and configure the reqwest client/request with only a connect timeout; do not call `.timeout()` on the streaming request.

- [x] **Step 4: Apply the two-stage watchdog in the provider**

Wrap `transport.post(...)` in `tokio::time::timeout(first_event_timeout_ms, ...)`. After headers, wrap each `stream_body.next()` in `tokio::time::timeout(stream_idle_timeout_ms, ...)`. A received chunk resets the idle window implicitly because the next iteration starts a new timeout. Map both to `ModelProviderError::Timeout` with the corresponding duration.

- [x] **Step 5: Run adapter and app provider tests**

Run:

```powershell
cargo test -p agent-model-openai
cargo test -p agent providers::tests
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add crates/agent-model-openai apps/agent/src/providers.rs
git commit -m "fix: use first-event and stream-idle model timeouts"
```

### Task 6: Recover Once from Reasoning-Only Output Truncation

**Files:**
- Create: `crates/agent-loop/src/default/output_recovery.rs`
- Modify: `crates/agent-loop/src/default.rs`
- Modify: `crates/agent-loop/src/verdict.rs`
- Modify: `crates/agent-loop/src/default/tests/limits.rs`
- Test: `crates/agent-loop/tests/halted_response.rs`

**Interfaces:**
- Consumes: `ResolvedContextBudget.completion_cap_tokens`, `.completion_cap_ceiling_tokens`, `ModelBudget.reasoning_effort`, `ModelResponse.usage.reasoning_tokens`, and the route's supported reasoning efforts.
- Produces: `output_recovery::decision(response: &ModelResponse, budget: &ResolvedContextBudget, supported_efforts: &[ReasoningEffort], retry_used: bool) -> RecoveryDecision`.

- [x] **Step 1: Write the failing pure and loop tests**

Add assertions for:

```rust
#[test]
fn a_reasoning_only_length_response_requests_a_larger_cap() {
    let decision = decision(&reasoning_only(8_000), &budget(262_144, 393_216), &all_efforts(), false);
    assert_eq!(decision, RecoveryDecision::Retry {
        max_tokens: 393_216,
        reasoning_effort: None,
    });
}

#[tokio::test]
async fn the_loop_retries_once_before_treating_output_as_final() {
    let model = FakeModelProvider::new(vec![reasoning_only(8_000), FakeModelProvider::answer("done")]);
    let outcome = run_turn_with_caps(&model, budget(262_144, 393_216)).await;
    assert_eq!(model.requests().len(), 2);
    assert_eq!(model.requests()[1].budget.max_tokens, Some(393_216));
    assert_eq!(outcome.stop, Some(StopReason::FinalAnswer));
}

#[tokio::test]
async fn a_second_reasoning_only_truncation_ends_as_max_output_tokens() {
    let model = FakeModelProvider::new(vec![reasoning_only(8_000), reasoning_only(16_000)]);
    let outcome = run_turn_with_caps(&model, budget(262_144, 393_216)).await;
    assert_eq!(outcome.stop, Some(StopReason::MaxOutputTokens));
    assert!(outcome.verdict_claim.contains("reasoning budget exhausted"));
}
```

- [x] **Step 2: Run the failing recovery tests**

Run: `cargo test -p agent-loop reasoning_only`

Expected: FAIL because no recovery path exists.

- [x] **Step 3: Implement the pure recovery decision**

Create `output_recovery.rs`. Return `Retry` only when all of the spec's conditions hold. Choose the next cap as `min(current * 2, completion_cap_ceiling_tokens)`. If the cap is already at the ceiling, return `Retry` with the next lower supported reasoning effort; if no lower supported effort exists, return `Stop`.

- [x] **Step 4: Apply recovery in the loop**

After receiving a truncated response and before dispatching tool calls or producing a verdict:
1. if `RecoveryDecision::Retry`, clone the request into a second request with the changed budget;
2. send it through the provider with `NullSink`;
3. record the retry in the second `model/request.prepared` payload as `"output_recovery": "reasoning_only"`;
4. discard the first response and continue with the second settled response;
5. never retry more than once for the step.

- [x] **Step 5: Improve the terminal verdict**

If the second response is still reasoning-only and truncated, use `verdict/output-truncated` with a claim beginning `reasoning budget exhausted`. It remains `StopReason::MaxOutputTokens`, not `FinalAnswer`.

- [x] **Step 6: Run loop tests**

Run: `cargo test -p agent-loop`

Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add crates/agent-loop
git commit -m "fix: recover once from reasoning-only output truncation"
```
### Task 7: Make Compaction and Notices Consume the Resolved Context Budget

**Files:**
- Modify: `crates/agent-context/src/trim.rs`
- Modify: `crates/agent-context/src/lib.rs`
- Modify: `crates/agent-ports/src/context_engine.rs`
- Modify: `crates/agent-loop/src/default/settle.rs`
- Modify: `crates/agent-loop/src/default/settlement.rs`
- Modify: `crates/agent-loop/src/default/budget.rs`
- Modify: `crates/agent-loop/src/context_layers.rs`
- Modify: `apps/agent/src/stack.rs`
- Modify: `apps/agent/src/commands/inspect.rs`
- Test: `crates/agent-context/tests/model_context_acceptance.rs`
- Test: `crates/agent-loop/src/default/tests/limits.rs`
- Test: `crates/agent-loop/src/default/tests/state.rs`

**Interfaces:**
- Consumes: `ResolvedContextBudget` from Task 3 and the resolved budget from Task 4.
- Produces: `ContextPolicy { context_window_tokens, input_working_set_tokens, completion_cap_tokens, compaction_trigger_tokens, retain_recent_tokens }`.

- [x] **Step 1: Write failing context tests**

Add tests asserting:

```rust
#[test]
fn compaction_uses_the_resolved_trigger_not_a_percentage_of_the_old_reservation() {
    let policy = ContextPolicy::resolved(budget());
    let candidate = compaction_candidate(&messages(300_000), &policy, None).unwrap();
    assert_eq!(candidate.budget_tokens, 262_144);
    assert_eq!(candidate.trigger_tokens, 209_715);
}

#[test]
fn retention_keeps_the_newest_balanced_groups() {
    let policy = ContextPolicy::resolved(budget());
    let trimmed = apply(messages_with_pairs(300_000), Some(policy));
    assert!(trimmed.usage.estimated_tokens <= 262_144);
    assert!(kept_pairs_are_balanced(&trimmed.messages));
}

#[test]
fn the_context_notice_reports_the_input_working_set_not_the_physical_window() {
    let notice = context_notice(&usage_with_budget(budget()), 240_000).unwrap();
    assert!(notice.text.contains("262144-token input budget"));
    assert!(!notice.text.contains("1048576-token input budget"));
}
```

- [x] **Step 2: Run the failing context tests**

Run: `cargo test -p agent-context compaction_uses_the_resolved_trigger_not_a_percentage_of_the_old_reservation`

Run: `cargo test -p agent-context retention_keeps_the_newest_balanced_groups`

Run: `cargo test -p agent-loop the_context_notice_reports_the_input_working_set_not_the_physical_window`

Expected: FAIL because the current policy derives from `max_input_tokens - reserved_output_tokens` and percentages.

- [x] **Step 3: Replace the policy arithmetic**

Change `ContextPolicy` to a concrete resolved form. `trim::apply` must compare kept messages against `input_working_set_tokens`; `compaction_candidate` must compare the full request against `compaction_trigger_tokens`; `ContextUsage` must report the same concrete numbers.

Remove all new-run use of `reserved_output_tokens`. The deprecated field may remain on old snapshots but must not enter arithmetic.

- [x] **Step 4: Update loop settlement and notices**

Update `settle.rs`, `settlement.rs`, `default/budget.rs`, and `context_layers.rs` to read `turn.budget.context.as_ref()` and the concrete fields. Change `ContextUsage.max_input_tokens` to `input_working_set_tokens`, remove the new-run use of `reserved_output_tokens`, and update every constructor. `tool_output_limit_bytes()` uses the resolved input working set and returns the same historical 8 KiB..50 KiB clamp. `DefaultAgentLoop::compact` uses `turn.budget.summary_max_output_tokens` (or the main completion cap when absent) for the summary request.

- [x] **Step 5: Update the inspection path**

`apps/agent/src/commands/inspect.rs` must construct the same resolved policy from the snapshot and display:
- physical window;
- input working set;
- completion cap;
- compaction trigger;
- retained recent tokens;
- source per field.

- [x] **Step 6: Run context and loop tests**

Run:

```powershell
cargo test -p agent-context
cargo test -p agent-loop
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add crates/agent-context crates/agent-ports/src/context_engine.rs crates/agent-loop apps/agent/src/stack.rs apps/agent/src/commands/inspect.rs
git commit -m "refactor: use resolved context budgets for compaction"
```

### Task 8: End-to-End Acceptance, Contracts, and Project Documentation

**Files:**
- Create: `apps/agent/tests/model_budget_acceptance.rs`
- Modify: `apps/agent/tests/cli_end_to_end.rs`
- Modify: `CONTRACT.md`
- Modify: `PROJECT.md`
- Modify: `README.md` if CLI-visible defaults changed
- Test: `crates/agent-testkit/tests/architecture_boundaries.rs`

**Interfaces:**
- Consumes: all previous tasks.
- Produces: acceptance coverage proving model-derived budgets and reasoning recovery in the assembled product.

- [x] **Step 1: Add assembled-product acceptance tests**

Add tests that call the real `stack::assemble`:

```rust
#[tokio::test]
async fn a_one_million_window_model_gets_a_model_derived_budget() {
    let runtime = assemble_with_caps(deepseek_flash_caps(), FakeModelProvider::answer("ok")).await;
    let profile = runtime.snapshot_for_test();
    let budget = profile.budget.context.unwrap();
    assert_eq!(budget.completion_cap_tokens, 262_144);
    assert_eq!(budget.input_working_set_tokens, 262_144);
    assert_eq!(budget.compaction_trigger_tokens, 209_715);
}

#[tokio::test]
async fn reasoning_only_length_is_retried_and_not_reported_as_an_answer() {
    let model = FakeModelProvider::new(vec![
        reasoning_only(8_000),
        FakeModelProvider::answer("finished"),
    ]);
    let runtime = assemble_with_caps(deepseek_flash_caps(), model).await;
    let outcome = run_one_turn(&runtime, "finish the task").await;
    assert_eq!(outcome.stop_reason, "final_answer");
    assert_eq!(outcome.requests, 2);
}
```

- [x] **Step 2: Run the failing acceptance tests**

Run: `cargo test -p agent model_budget_acceptance -- --nocapture`

Expected: FAIL until the assembly and loop changes are connected and test helpers exist.

- [x] **Step 3: Add the architecture and old-config guards**

Extend `crates/agent-testkit/tests/architecture_boundaries.rs` only if a new crate dependency was introduced. Add a config test proving a pre-change TOML file with `max_input_tokens`, `max_output_tokens`, and `reserved_output_tokens` still loads, with a deprecation note for the reservation.

- [x] **Step 4: Update durable documentation**

Add a `CONTRACT.md` §7 decision row recording:
- capability-derived budget;
- `completion_cap` includes reasoning;
- idle/first-event timeout;
- one-shot reasoning truncation recovery;
- old field aliases and ignored reservation.

Update `PROJECT.md` to describe the new model capability flow and budget fields.

- [x] **Step 5: Run focused acceptance and architecture tests**

Run:

```powershell
cargo test -p agent model_budget_acceptance -- --nocapture
cargo test -p agent-testkit architecture_boundaries
```

Expected: PASS.

- [x] **Step 6: Run the full repository gate**

Run: `powershell -ExecutionPolicy Bypass -File scripts\test.ps1`

Expected: `fmt`, `clippy`, and `cargo test --workspace` all pass; script ends with `gate: all requested steps passed`.

- [x] **Step 7: Commit**

```bash
git add apps/agent/tests/model_budget_acceptance.rs apps/agent/tests/cli_end_to_end.rs crates/agent-testkit/tests/architecture_boundaries.rs CONTRACT.md PROJECT.md README.md
git commit -m "docs: record model-adaptive budget contracts and acceptance"
```

---

## Execution Handoff

The tasks are ordered as vertical increments. Tasks 1-4 must land before Task 7; Task 5 and Task 6 can proceed after Task 1 and may be implemented in separate branches if desired. Task 8 is the final integration gate.

Recommended execution method: **subagent-driven development**, because Tasks 5-7 touch independent boundaries and each should receive a fresh review before the final assembled-product acceptance run.
