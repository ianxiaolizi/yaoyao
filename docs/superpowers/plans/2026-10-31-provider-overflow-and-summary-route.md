# Provider Overflow Recovery and Summary Route Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the two remaining context-budget acceptance gaps: recover once from a provider context-overflow rejection by performing a maximum safe compression, and give the fast summary route its own complete input, output, and reasoning budget.

**Architecture:** The OpenAI-compatible adapter classifies context-length rejections into a typed provider error; the agent loop owns the one-shot recovery because only it can estimate and rewrite the full request. Recovery first settles in-flight traffic without a model call, then deterministically removes the oldest complete message groups until the request fits a conservative safe target, records the removal as `context/compacted`, and retries the original request exactly once. Separately, assembly freezes a complete `SummaryRouteBudget` for the fast model, and compaction uses that route's input working set and effort rather than inheriting the main route's values.

**Tech Stack:** Rust workspace; `agent-ports`, `agent-model-openai`, `agent-protocol`, `agent-config`, `agent-loop`, `apps/agent`; Tokio; serde; reqwest.

**Spec:** `docs/superpowers/specs/2026-10-31-context-budget-auto-adaptation-design.md`

## Global Constraints

- No `unsafe` code.
- Provider-specific HTTP/body parsing stays inside `agent-model-openai`; the kernel and loop consume provider-neutral error values.
- A context-overflow recovery performs at most one retry per model step.
- Emergency compression itself makes no model request: a summarizer call while a context rejection is in flight can overflow for the same reason.
- Emergency compression never removes the current user input, never removes a mid-turn user message, and never splits a tool call from its result group.
- The retry uses a new `AttemptId` and records its own `model/request.prepared` and `model/attempt.started` events.
- The fast summary route uses its own `ResolvedContextBudget`, declared reasoning efforts, and route-specific completion cap.
- If the full summary request cannot fit even after selecting the largest leading atomic-group prefix, fall back to the main route; if it cannot fit there either, record a compaction failure and leave history unchanged.
- Old snapshots and old TOML keep deserializing. `summary_max_output_tokens` remains readable during one compatibility window, but new code reads `RunBudget.summary`.
- Every task ends with its focused tests passing before commit.
- Final gate: `powershell -ExecutionPolicy Bypass -File scripts\test.ps1`.

## Review Focus

- A generic 400 or an auth failure must still terminate as a normal HTTP error and must not trigger compression.
- A 413, an OpenAI `context_length_exceeded`, and vendor-equivalent context-window 400s must all enter the same recovery path.
- The current user input, run preamble, capability/state/task layers, and newest in-flight atomic group must survive emergency compression.
- A second context-overflow rejection after the recovery retry must stop after exactly two provider attempts, with no recursive recovery.
- A large summary candidate must be clipped on whole-message-group boundaries to the fast route's input budget; the summary request must never carry the main route's unsupported effort.

---

### Task 1: Add a Typed Provider Context-Overflow Error

**Files:**
- Modify: `crates/agent-ports/src/model_provider.rs`
- Modify: `crates/agent-model-openai/src/provider.rs`
- Test: `crates/agent-model-openai/tests/contract.rs`

**Interfaces:**
- Consumes: existing `ModelProviderError`, `OpenAiCompatProvider::run_attempt`, and the scripted transport in `tests/contract.rs`.
- Produces: `ModelProviderError::ContextOverflow { status: u16, body: String }` and `ModelProviderError::is_context_overflow() -> bool`.

- [x] **Step 1: Write the failing provider-classification tests**

Add tests asserting:

```rust
#[tokio::test]
async fn a_413_is_a_context_overflow() {
    let (provider, _) = provider_over(FakeTransport::status(413, "payload too large"));
    let error = provider.stream(&request(), &NullSink).await.unwrap_err();
    assert!(error.is_context_overflow(), "{error:?}");
}

#[tokio::test]
async fn a_context_length_400_is_a_context_overflow() {
    let body = r#"{"error":{"code":"context_length_exceeded","message":"maximum context length exceeded"}}"#;
    let (provider, _) = provider_over(FakeTransport::status(400, body));
    let error = provider.stream(&request(), &NullSink).await.unwrap_err();
    assert!(error.is_context_overflow(), "{error:?}");
}

#[tokio::test]
async fn an_unrelated_400_remains_an_http_error() {
    let (provider, _) = provider_over(FakeTransport::status(400, "bad tool schema"));
    let error = provider.stream(&request(), &NullSink).await.unwrap_err();
    assert!(matches!(error, ModelProviderError::Http { status: 400, .. }));
}
```

Add one retry test using the existing `AttemptsTransport`:

```rust
#[tokio::test]
async fn the_adapter_does_not_repeat_a_context_overflow() {
    let attempts = AttemptsTransport::new(vec![
        Outcome::Response(413, vec![Ok(b"context too long".to_vec())]),
        AttemptsTransport::answered("must not be used"),
    ]);
    let (provider, transport) = provider_with_retries(attempts, 3);
    let error = provider.stream(&request(), &NullSink).await.unwrap_err();
    assert!(error.is_context_overflow());
    assert_eq!(transport.calls(), 1);
}
```

- [x] **Step 2: Run the failing tests**

Run:

```powershell
cargo test -p agent-model-openai a_413_is_a_context_overflow
cargo test -p agent-model-openai a_context_length_400_is_a_context_overflow
cargo test -p agent-model-openai an_unrelated_400_remains_an_http_error
cargo test -p agent-model-openai the_adapter_does_not_repeat_a_context_overflow
```

Expected: compile failure because `ContextOverflow` and `is_context_overflow` do not exist.

- [x] **Step 3: Add the provider-neutral error variant**

In `agent-ports`, add:

```rust
#[error("model context overflow (HTTP {status}): {body}")]
ContextOverflow { status: u16, body: String },
```

Update `kind()` to return `"context_overflow"` and add:

```rust
pub fn is_context_overflow(&self) -> bool {
    matches!(self, Self::ContextOverflow { .. })
}
```

Treat `ContextOverflow` as non-retryable in `agent-model-openai::provider::is_retryable`.

- [x] **Step 4: Classify responses in the OpenAI-compatible adapter**

When `run_attempt` sees a non-200 response, classify before constructing the ordinary HTTP error:

```rust
fn context_overflow(status: u16, body: &str) -> bool {
    if status == 413 {
        return true;
    }
    if status != 400 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "too many tokens",
        "prompt is too long",
        "input is too long",
        "request too large",
    ]
    .iter()
    .any(|signal| body.contains(signal))
}
```

Classify the raw bounded response body, then store the existing `describe_error_body` value in the typed error so the accepted `code` field is not discarded before classification. Do not parse vendor fields outside `agent-model-openai`.

- [x] **Step 5: Run the focused provider tests**

Run:

```powershell
cargo test -p agent-model-openai --test contract context
```

Expected: PASS, including the four new tests and all existing retry/timeout tests.

- [x] **Step 6: Commit**

```bash
git add crates/agent-ports/src/model_provider.rs crates/agent-model-openai/src/provider.rs crates/agent-model-openai/tests/contract.rs
git commit -m "feat: classify provider context overflow"
```

### Task 2: Freeze the Fast Summary Route's Complete Budget

**Files:**
- Modify: `crates/agent-protocol/src/profile.rs`
- Modify: `crates/agent-protocol/src/compaction.rs`
- Modify: `apps/agent/src/stack.rs`
- Modify: `crates/agent-loop/src/default.rs`
- Test: inline protocol tests and `apps/agent/src/stack.rs` tests
- Test: `crates/agent-loop/src/default/tests/summary_route.rs` (create)

**Interfaces:**
- Consumes: `ResolvedContextBudget`, `resolve_context_budget`, `preset_only_policy`, `reasoning_effort_for`, route capabilities discovered during assembly, and `ModelRouter::route(TaskClass::SUMMARIZE)`.
- Produces: `SummaryRouteBudget { context, reasoning_effort, supported_reasoning_efforts }`, `RunBudget.summary: Option<SummaryRouteBudget>`, and `agent_protocol::compaction::fit_leading_groups(messages, available_tokens) -> Vec<ModelMessage>`.

- [x] **Step 1: Write the failing protocol and fitted-prefix tests**

In `profile.rs`, assert old snapshots still deserialize and the new shape round-trips:

```rust
#[test]
fn an_old_budget_with_only_a_summary_cap_still_deserializes() {
    let budget: RunBudget = serde_json::from_value(serde_json::json!({
        "summary_max_output_tokens": 4096
    }))
    .unwrap();
    assert_eq!(budget.summary_max_output_tokens, Some(4096));
    assert!(budget.summary.is_none());
}
```

In `compaction.rs`, add:

```rust
#[test]
fn fitting_leading_groups_keeps_whole_oldest_groups() {
    let messages = vec![text("old"), call("1"), result("1"), text("new")];
    let fitted = fit_leading_groups(&messages, 40);
    assert_eq!(uids(&fitted), vec!["old", "call_1", "result_1"]);
}

#[test]
fn fitting_leading_groups_never_splits_a_tool_call_from_its_result() {
    let messages = vec![call("1"), result("1"), text("new")];
    assert!(fit_leading_groups(&messages, 1).is_empty());
}
```

The first test must use messages sized so the call/result group fits but `new` does not. Write exact text lengths or helper constructors in the test so the boundary is deterministic.

- [x] **Step 2: Run the failing protocol tests**

Run:

```powershell
cargo test -p agent-protocol an_old_budget_with_only_a_summary_cap_still_deserializes
cargo test -p agent-protocol fitting_leading_groups
```

Expected: FAIL because `SummaryRouteBudget`, `RunBudget.summary`, and `fit_leading_groups` do not exist.

- [x] **Step 3: Add the summary budget type and fitted-prefix helper**

Add to `profile.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryRouteBudget {
    pub context: ResolvedContextBudget,
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_reasoning_efforts: Vec<ReasoningEffort>,
}
```

Add `pub summary: Option<SummaryRouteBudget>` to `RunBudget` with `serde(default, skip_serializing_if = "Option::is_none")`. Keep `summary_max_output_tokens` as the legacy read-through field; do not remove it in this change.

Implement `fit_leading_groups` in `compaction.rs` by walking `groups(messages)` from oldest to newest, skipping orphan groups, and including a group only when its whole cost still fits `available_tokens`. Return an empty vector when the first eligible group does not fit.

- [x] **Step 4: Write the failing assembly and summary-route tests**

In `apps/agent/src/stack.rs`, add a pure helper test with:

```text
main caps: window 1,048,576; output 393,216; efforts [low, high, max]; default high
fast caps: window 131,072; output 8,192; efforts [low]; default low
preset: balanced
```

Assert the frozen summary budget has `input_working_set_tokens == 32,768`, `completion_cap_tokens == 8,192`, `reasoning_effort == Some(Low)`, and `supported_reasoning_efforts == vec![Low]`. Also assert `budget_sources["summary.input_working_set_tokens"] == "preset:balanced"`.

In `crates/agent-loop/src/default/tests/summary_route.rs`, add a test named:

```rust
#[tokio::test]
async fn a_large_summary_candidate_is_clipped_to_the_fast_route_budget()
```

Use a router that maps `TaskClass::SUMMARIZE` to a fast route and a model script of `[summary_answer, main_answer]`. Give the turn a candidate history larger than the fast input budget. Assert:

- the first model request has `model_route.tier == ModelTier::Fast`;
- its estimated messages are at or below `summary.context.input_working_set_tokens`;
- its `budget.reasoning_effort` is a value declared by the fast model, never the main model's unsupported depth;
- all selected candidate messages are an oldest leading prefix of complete groups.

- [x] **Step 5: Resolve the complete summary budget during assembly**

In `apps/agent/src/stack.rs`, replace the cap-only branch with:

```rust
let summary_selection = routes
    .get(ModelTier::Fast.as_str())
    .and_then(|route| discovered.resolved.get(&route_key(route)).map(|caps| (route, caps)));

let (summary_resolution, summary_capabilities, summary_effort) = match summary_selection {
    Some((route, caps)) => {
        let resolution = resolve_context_budget(caps, &preset_only_policy(resolved.budget.preset))
            .map_err(|source| StackError::Budget { route: route_key(route), source })?;
        // A summary is its own request. Do not inherit the main route's effort.
        let (effort, _) = reasoning_effort_for(&None, caps, &route_key(route))?;
        (resolution, caps.clone(), effort)
    }
    None => (resolution.clone(), main_capabilities.clone(), reasoning_effort),
};
```

Set:

```rust
resolved.budget.summary = Some(SummaryRouteBudget {
    context: summary_resolution.budget,
    reasoning_effort: summary_effort,
    supported_reasoning_efforts: summary_capabilities
        .capabilities
        .reasoning_efforts
        .clone(),
});
resolved.budget.summary_max_output_tokens = Some(summary_resolution.budget.completion_cap_tokens);
```

Add audit keys for `summary.context_window_tokens`, `summary.input_working_set_tokens`, `summary.completion_cap_tokens`, and `summary.reasoning_effort`. When no fast route is configured, derive the summary budget from the main capabilities and reuse the main effort.

- [x] **Step 6: Make compaction consume the summary route budget**

In `DefaultAgentLoop::compact`, after selecting the summarize route:

1. Choose the route profile:
   - `route.tier == Fast` and `SummaryRouteBudget` exists: use the summary profile.
   - Otherwise: use `turn.budget.context` plus `turn.budget.reasoning_effort` and `supported_reasoning_efforts`.
2. Build the fixed summary messages (system prompt, optional in-flight input, instruction).
3. Compute `available = profile.context.input_working_set_tokens - estimated(fixed_messages)` using checked/saturating arithmetic.
4. Call `fit_leading_groups(&candidate.messages, available)`.
5. If the fast route selects no group, fall back to `turn.route` and retry the fit against the main profile.
6. If the main route also selects no group, emit `CONTEXT_COMPACTION_FAILED` with `error_kind = "summary_input_too_large"` and leave the transcript unchanged.
7. When a prefix is selected, remove only those messages from the candidate and set:

```rust
candidate.estimated_tokens = selected.iter().map(estimate_message).sum();
candidate.retained_tokens = candidate.retained_tokens
    + original_candidate_tokens
    - candidate.estimated_tokens;
```

Set the summary request's `budget.max_tokens` to the selected profile's `completion_cap_tokens` and `budget.reasoning_effort` to its route-specific effort. Record `"route_fallback": "input_working_set"` when the fast route had no fitting prefix and the main route was used.

- [x] **Step 7: Run the focused budget and loop tests**

Run:

```powershell
cargo test -p agent-protocol fitting_leading_groups
cargo test -p agent-protocol old_budget_with_only_a_summary_cap
cargo test -p agent --lib stack
cargo test -p agent-loop summary_route
```

Expected: PASS. Inspect the recorded fast summary request and confirm its message estimate is within the fast input working set.

- [x] **Step 8: Commit**

```bash
git add crates/agent-protocol/src/profile.rs crates/agent-protocol/src/compaction.rs apps/agent/src/stack.rs crates/agent-loop/src/default.rs crates/agent-loop/src/default/tests/summary_route.rs
git commit -m "fix: budget summary route independently"
```

### Task 3: Compress Once and Retry a Context Overflow

**Files:**
- Create: `crates/agent-loop/src/default/context_recovery.rs`
- Modify: `crates/agent-loop/src/default.rs`
- Modify: `crates/agent-loop/src/default/settlement.rs`
- Test: `crates/agent-loop/tests/context_overflow_recovery.rs` (create)

**Interfaces:**
- Consumes: `ModelProviderError::ContextOverflow`, `ResolvedContextBudget`, `budget::estimated_request_tokens`, `settlement::settle`, `agent_protocol::compaction::groups`, and the current `LoopTurn` history bounds.
- Produces: `context_recovery::safe_target(...)`, `context_recovery::compress_for_overflow(...) -> bool`, and a one-shot retry branch in `DefaultAgentLoop::step`.

- [x] **Step 1: Write the failing integration tests**

Create a static context engine in the test that returns a long settled history and a medium-sized current user input. Add these tests:

```rust
#[tokio::test]
async fn context_overflow_compresses_and_retries_once()
```

Script the provider as `[Failed(ContextOverflow), answer("finished")]`. Assert:

- exactly two provider requests were made;
- the second request preserves the current user message and every run/layer message;
- the second request's estimated size is at or below the resolved compaction trigger;
- the dropped history is recorded by one `context/compacted` event with `strategy == "overflow-trim"`;
- the second `model/request.prepared` carries `context_recovery == "max_safe_compression"`;
- the final stop reason is `FinalAnswer`.

```rust
#[tokio::test]
async fn a_second_context_overflow_stops_without_a_third_attempt()
```

Script two `ContextOverflow` failures. Assert exactly two requests, both attempt-failed events present, and one terminal step failure.

```rust
#[tokio::test]
async fn emergency_compression_never_splits_a_tool_group_or_drops_user_text()
```

Seed settled history containing a call/result group and a later user message, then overflow. Assert the kept call and result are both present or both absent, and every current-turn user message remains.

```rust
#[tokio::test]
async fn a_generic_http_400_does_not_enter_context_recovery()
```

Script one `Http { status: 400, .. }`. Assert one request and a normal fatal result.

- [x] **Step 2: Run the failing integration tests**

Run:

```powershell
cargo test -p agent-loop --test context_overflow_recovery
```

Expected: FAIL because the typed error branch and recovery module do not exist.

- [x] **Step 3: Implement the safe target and deterministic compression**

In `context_recovery.rs`, add:

```rust
pub(super) fn safe_target(context: &ResolvedContextBudget) -> usize {
    usize::try_from(
        context
            .input_working_set_tokens
            .min(context.compaction_trigger_tokens),
    )
    .unwrap_or(usize::MAX)
}
```

`compress_for_overflow(turn, target, step_id, events) -> bool` performs these actions in order:

1. If a context budget exists, attempt `settlement::settle` on this turn's in-flight traffic. Record the existing settlement event and recompute the request estimate.
2. If the request now fits `target`, return `true`.
3. Identify removable atomic groups in order:
   - settled history from `history_start..history_end`, oldest first;
   - then in-flight traffic after the current user input, oldest first.
4. Exclude every group containing a `User` message and always keep the newest in-flight group.
5. Tentatively remove groups oldest-first. After each removal, estimate the request with the would-be replacement message included, and stop only when that estimate is `<= target`.
6. Build a deterministic replacement message:

```text
Overflow recovery: N older messages were omitted because the provider rejected the request for its context length. The run instructions, current user input, newest tool cycle, and this record remain. The omitted messages are still present in the event log.
```

7. Insert the replacement at the position of the earliest removed message.
8. Emit one `CONTEXT_COMPACTED` event with `attempt_id`, `summary`, `summarized` refs, `strategy = "overflow-trim"`, `fallback = "provider_context_overflow"`, `scope = "overflow"`, `estimated_before`, and `estimated_after`.
9. Update `history_end` and `context_message_count` from the actual messages remaining after removal and insertion.

Return `false` when no removable group exists or the protected request still exceeds `target`; in that case do not mutate `turn.messages`.

- [x] **Step 4: Wire the one-shot retry into the step**

In `DefaultAgentLoop::step`, on a `ContextOverflow` error from the first `model.stream` attempt:

1. Emit the ordinary `MODEL_ATTEMPT_FAILED` event with `error_kind = "context_overflow"`.
2. Compute `safe_target` from `turn.budget.context`. If absent, do not recover.
3. Call `context_recovery::compress_for_overflow`.
4. If it returns `false`, emit the step failure and stop as `FatalError`.
5. If it returns `true`, rebuild the `ModelRequest` from the compressed `turn.messages`, assign a new `AttemptId`, and emit `MODEL_REQUEST_PREPARED` with:

```json
{"context_recovery":"max_safe_compression","original_error_kind":"context_overflow"}
```

6. Emit `MODEL_ATTEMPT_STARTED` for the new attempt and send the request once.
7. A success continues through normal output recovery and settlement. A second `ContextOverflow` emits `MODEL_ATTEMPT_FAILED` and returns `FatalError`; no further retry is allowed.
8. Only `ModelProviderError::ContextOverflow` enters this branch. `Http { status: 400, .. }`, cancellation, timeout, protocol errors, and stream interruptions keep their existing behavior.

Do not call `output_recovery` until the context retry has succeeded. Keep token accounting unchanged: only settled responses add spend.

- [x] **Step 5: Run the focused recovery tests**

Run:

```powershell
cargo test -p agent-loop --test context_overflow_recovery -- --nocapture
cargo test -p agent-loop halted_response
```

Expected: PASS. The recovery suite must show exactly one retry and one `overflow-trim` event.

- [x] **Step 6: Commit**

```bash
git add crates/agent-loop/src/default/context_recovery.rs crates/agent-loop/src/default.rs crates/agent-loop/src/default/settlement.rs crates/agent-loop/tests/context_overflow_recovery.rs
git commit -m "feat: recover once from provider context overflow"
```

### Task 4: Assembled Acceptance, Contracts, and Documentation

**Files:**
- Modify: `apps/agent/tests/model_budget_acceptance.rs`
- Modify: `CONTRACT.md`
- Modify: `PROJECT.md`
- Test: `crates/agent-testkit/tests/architecture_boundaries.rs` only if a new crate dependency is added

**Interfaces:**
- Consumes: all previous tasks.
- Produces: assembled-product proof that a real OpenAI-compatible `413`/context-length rejection compresses and retries, plus durable documentation of the fast summary budget and overflow path.

- [x] **Step 1: Make the assembled scripted transport status-aware**

Extend `ScriptedTransport` so each reply carries `{ status, body }` while existing `answer_body(...)` call sites continue to construct `200` replies. Keep `GET /models` returning the scripted catalog without consuming the chat reply queue.

- [x] **Step 2: Add the assembled overflow acceptance test**

Add:

```rust
#[tokio::test]
async fn context_overflow_is_compressed_and_retried_once_by_the_assembled_stack()
```

Drive three replies:

1. a normal first-turn answer;
2. a `413` context-length error on the second turn;
3. a successful answer on the retry.

Assert:

- the second turn reaches `StopReason::FinalAnswer`;
- the transport saw exactly three POSTs;
- the retry body contains the current second-turn user message;
- the retry body no longer contains the first-turn settled history verbatim;
- exactly one `context/compacted` event has `strategy == "overflow-trim"`;
- the retry prepared event has `context_recovery == "max_safe_compression"`.

- [x] **Step 3: Add the assembled summary-route snapshot test**

With a profile naming `model = { main = "big", fast = "small" }` and a catalog where `small` declares a 131,072 window, 8,192 output cap, and only `low` effort, assert `profile/resolved` records:

```json
{
  "budget": {
    "summary": {
      "context": {
        "context_window_tokens": 131072,
        "input_working_set_tokens": 32768,
        "completion_cap_tokens": 8192
      },
      "reasoning_effort": "low",
      "supported_reasoning_efforts": ["low"]
    }
  }
}
```

Also assert `budget_sources["summary.input_working_set_tokens"] == "preset:balanced"` and that the main route still records its own independent values.

- [x] **Step 4: Update durable documentation**

Add CONTRACT §7 rows recording:

- `ContextOverflow` classification and the accepted 400 signals;
- full `SummaryRouteBudget` derivation and route-specific reasoning effort;
- deterministic `overflow-trim` behavior, protected messages, and the one-retry limit.

Update `PROJECT.md` to state that a provider context rejection causes one maximum-safe deterministic compression followed by one retry, and that fast summaries use the fast route's own input window and effort.

- [x] **Step 5: Run assembled acceptance and boundary checks**

Run:

```powershell
cargo test -p agent context_overflow_is_compressed_and_retried_once_by_the_assembled_stack -- --nocapture
cargo test -p agent assembled_summary_route -- --nocapture
cargo test -p agent-testkit architecture_boundaries
```

Expected: PASS.

- [x] **Step 6: Run the full repository gate**

Run:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\test.ps1
```

Expected: `fmt`, `clippy`, and `cargo test --workspace` all pass; the script ends with `gate: all requested steps passed`.

- [x] **Step 7: Commit**

```bash
git add apps/agent/tests/model_budget_acceptance.rs CONTRACT.md PROJECT.md
git commit -m "test: close provider overflow and summary route gaps"
```

---

## Execution Handoff

Tasks 1 and 2 are independent and may be implemented in parallel. Task 3 consumes Task 1's typed error and is the critical path. Task 4 consumes all three and is the final acceptance gate.

Recommended execution method: **subagent-driven development**, because the provider classification, route-budget assembly, and loop recovery touch separate boundaries and each benefits from an independent review before the assembled-product acceptance run.

---

## Post-Review Hardening

- [x] **Mixed overflow trimming preserves the history boundary.** `history_end` now subtracts only the removed ranges that overlap the original settled-history range; in-flight removals no longer shrink it. Regression tests: `a_mixed_removal_counts_only_messages_that_were_in_history`, `a_replacement_outside_history_does_not_change_history_end`.
- [x] **Explicit fast reasoning effort is honored or refused.** Assembly resolves the configured depth against the fast model's declared list; supported values win, unsupported values refuse startup. Regression tests: `an_explicit_summary_depth_the_fast_model_supports_is_honored`, `an_explicit_summary_depth_the_fast_model_lacks_is_refused`.
- [x] **Cap-only old fast routes fall back to main.** A resumed snapshot that has `summary_max_output_tokens` but lacks `RunBudget.summary` no longer sends a summary to the fast route with main-route numbers; it records `route_fallback = "missing_summary_budget"`. Regression test: `a_fast_summary_without_a_frozen_budget_falls_back_to_main`.