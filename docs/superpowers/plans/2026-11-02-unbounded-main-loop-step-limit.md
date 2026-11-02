# Unbounded Main Loop Step Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the implicit main-loop step ceiling while preserving explicit `max_steps`, durable parking, stall/cancel/token/time safety valves, and partial subagent outcomes.

**Architecture:** Carry the existing config distinction between omitted and explicit `max_steps` through `RunBudget`, `Scheduler`, `TurnStateFacts`, reason snapshots, prompt rendering, and subagent settlement. Old numeric snapshots remain bounded; JSON `null` is the new explicit unbounded value. Step count stays telemetry and never defaults to 8 or 300.

**Tech Stack:** Rust workspace, Serde/TOML, Tokio tests, agent-kernel Scheduler, agent-loop context layers, agent-web TypeScript.

**Spec:** `docs/superpowers/specs/2026-11-02-unbounded-main-loop-step-limit-design.md`

## Global Constraints

- Omitted `budget.max_steps` means no fixed step ceiling.
- `max_steps = 0` remains an invalid configuration.
- Do not invent default step, wall-clock, or token budgets.
- Keep user cancellation, `stall_steps`, `max_tool_calls`, `max_total_tokens`, `wall_clock_timeout`, completion criteria, provider errors, and content filtering unchanged.
- Old numeric `max_steps` snapshots decode as `Some(n)`.
- Old snapshots missing `max_steps` decode as `Some(8)`, preserving historical behavior.
- New unbounded snapshots serialize `"max_steps": null`, not `0` and not a missing field.
- Preserve all pre-existing uncommitted changes. Never stage, revert, or commit unrelated work. When a task touches an already-dirty file, stage only task-owned hunks or leave the commit uncreated and report it.
- Final gate: `powershell -ExecutionPolicy Bypass -File scripts\test.ps1` when available; otherwise run fmt, clippy, and workspace tests separately as documented in `STYLE.md`.

## Review Focus

- **Legacy snapshots:** Missing `max_steps` stays `Some(8)`; explicit JSON `null` means unbounded.
- **Live boundary:** An unbounded Scheduler crosses 8, 20, and 300 steps; `Some(3)` still stops before step 4.
- **Other limits:** `None` step ceiling must not disable token, wall-clock, tool-call, stall, or cancellation endings.
- **Subagent partials:** A step-capped child with no final text is `Partial`, never `Completed` or generic `Failed`.
- **Serialization drift:** No path serializes unbounded as `0`; parking percentages never divide by a missing step ceiling.

---

### Task 1: Make `RunBudget` step ceiling optional

**Files:**
- Modify: `crates/agent-protocol/src/profile.rs`
- Modify: `crates/agent-config/src/resolve.rs`
- Modify: `crates/agent-config/src/lib.rs`

**Interfaces:**
- Produces: `RunBudget.max_steps: Option<u32>`
- Produces: `RunBudget::default().max_steps == None`
- Produces: a field default that returns `Some(8)` only when JSON omits `max_steps`
- Consumes: `BudgetSpec.max_steps: Option<u32>` unchanged

- [ ] **Step 1: Add failing protocol tests**

In `crates/agent-protocol/src/profile.rs`, add tests that decode `{}` as `Some(8)`, decode `{ "max_steps": null }` as `None`, and serialize `RunBudget::default()` with `"max_steps": null`.

- [ ] **Step 2: Run the failing protocol tests**

Run: `cargo test -p agent-protocol profile:: -- --nocapture`

Expected: FAIL because `max_steps` is still `u32`.

- [ ] **Step 3: Implement the optional field and legacy default**

Change `RunBudget.max_steps` to `Option<u32>` with a field-level default function returning `Some(8)`. Do not skip serialization; write `None` as JSON `null`. Change `RunBudget::default()` to set `None`.

- [ ] **Step 4: Add failing resolver tests**

In `crates/agent-config/src/lib.rs`, assert that an empty configuration gives `None`, explicit `max_steps = 4` gives `Some(4)`, and `max_total_tokens = 1000` without `max_steps` gives `None`. Keep the zero-step rejection test.

- [ ] **Step 5: Run the failing resolver tests**

Run: `cargo test -p agent-config max_steps -- --nocapture`

Expected: FAIL until the implicit defaults are removed.

- [ ] **Step 6: Remove the implicit defaults**

In `crates/agent-config/src/resolve.rs`, delete `DEFAULT_BACKSTOP_STEPS`, `named_steps`, and the `primary_limit && !named_steps` assignment. Set `budget.max_steps = Some(steps)` only in `apply_budget`. Update comments that still claim 8 is the configured default.

- [ ] **Step 7: Run focused tests**

Run: `cargo test -p agent-protocol -p agent-config -- --nocapture`

Expected: PASS.

- [ ] **Step 8: Commit only isolated hunks**

```powershell
git diff -- crates/agent-protocol/src/profile.rs crates/agent-config/src/resolve.rs crates/agent-config/src/lib.rs
git add crates/agent-protocol/src/profile.rs crates/agent-config/src/resolve.rs crates/agent-config/src/lib.rs
git diff --cached --check
git commit -m "refactor: make run step ceilings optional"
```

If those files already contain unrelated changes, stage only this task's hunks or leave the commit uncreated and report it.

---

### Task 2: Make Scheduler and parking tolerate no step ceiling

**Files:**
- Modify: `crates/agent-kernel/src/scheduler.rs`
- Modify: `crates/agent-kernel/src/parking.rs`
- Create: `crates/agent-kernel/src/scheduler/tests/step_budget_tests.rs`
- Modify: `apps/agent/src/stack.rs`
- Modify: `apps/agent/src/subagent_serve.rs`

**Interfaces:**
- Consumes: `RunBudget.max_steps: Option<u32>`
- Produces: `Scheduler::new(store, loop, max_steps: Option<u32>)`
- Produces: `parking::percent_of(steps: u32, max_steps: Option<u32>, tokens: u64, max_tokens: Option<u64>) -> u32`

- [ ] **Step 1: Add failing scheduler tests**

Create `crates/agent-kernel/src/scheduler/tests/step_budget_tests.rs` and register it with `mod step_budget_tests;` beside `subject_tests`.

Add a counting `AgentLoop` that returns `stop: None` for 320 steps and `FinalAnswer` on step 321. Assert `FinalAnswer`, 321 calls, and no `budget/parked`. Add a `Some(3)` test that yields `MaxSteps`, exactly 3 step starts, no fourth step, and one parked record reason `max_steps`.

- [ ] **Step 2: Run the failing scheduler tests**

Run: `cargo test -p agent-kernel step_budget_tests -- --nocapture`

Expected: FAIL to compile because Scheduler still takes `u32`.

- [ ] **Step 3: Update Scheduler**

Change the field and constructor to `Option<u32>`. Check the ceiling only for `Some(limit)`. Keep all other endings and their order unchanged. Pass the option to parking.

- [ ] **Step 4: Add failing parking tests**

In `crates/agent-kernel/src/parking.rs`, assert `percent_of(12, None, 0, None) == 0`, `percent_of(12, None, 50, Some(100)) == 50`, and `percent_of(2, Some(4), 0, None) == 50`.

- [ ] **Step 5: Run the failing parking tests**

Run: `cargo test -p agent-kernel parking:: -- --nocapture`

Expected: FAIL until the signature accepts `Option<u32>`.

- [ ] **Step 6: Implement optional parking percentages**

Use a step contribution of 0 for `None`; preserve existing saturating math for `Some(limit)` and unchanged token math.

- [ ] **Step 7: Update all Scheduler construction sites**

Update `apps/agent/src/stack.rs`, `apps/agent/src/subagent_serve.rs`, and test helpers. Tests that intentionally exercise a cap pass `Some(current_value)`; unbounded tests pass `None`. Update assertions such as `first.max_steps == 8`.

- [ ] **Step 8: Verify compile**

Run: `cargo test -p agent-kernel -p agent --no-run`

Expected: PASS.

- [ ] **Step 9: Commit only isolated hunks**

```powershell
git diff -- crates/agent-kernel/src/scheduler.rs crates/agent-kernel/src/parking.rs crates/agent-kernel/src/scheduler/tests/step_budget_tests.rs apps/agent/src/stack.rs apps/agent/src/subagent_serve.rs
git add crates/agent-kernel/src/scheduler.rs crates/agent-kernel/src/parking.rs crates/agent-kernel/src/scheduler/tests/step_budget_tests.rs apps/agent/src/stack.rs apps/agent/src/subagent_serve.rs
git diff --cached --check
git commit -m "feat: allow unbounded scheduler runs"
```

Preserve pre-existing changes in `scheduler.rs`, `stack.rs`, and `subagent_serve.rs`.

---

### Task 3: Propagate optional step semantics into prompts and reason snapshots

**Files:**
- Modify: `crates/agent-ports/src/agent_loop.rs`
- Modify: `crates/agent-loop/src/context_layers.rs`
- Modify: `crates/agent-loop/src/default/work_state.rs`
- Modify: `crates/agent-loop/src/default.rs`
- Modify: `crates/agent-loop/src/default/tests/limits.rs`
- Modify: `crates/agent-loop/src/default/tests/stall.rs`
- Modify: `crates/agent-loop/src/default/tests/summary_route.rs`
- Modify: `crates/agent-loop/src/default/tests/completion.rs`
- Modify: `crates/agent-loop/tests/context_overflow_recovery.rs`
- Modify: `crates/agent-loop/tests/halted_response.rs`
- Modify: `crates/agent-protocol/src/reason.rs`

**Interfaces:**
- Consumes: `RunBudget.max_steps: Option<u32>`
- Produces: `TurnStateFacts.max_steps: Option<u32>`
- Produces: `Situation.max_steps: Option<u32>` serialized as `null` when unbounded

- [ ] **Step 1: Add failing prompt tests**

In `crates/agent-loop/src/context_layers.rs`, test that `max_steps: None` renders “no fixed model-step ceiling” and never “at most”. Keep a bounded counterpart asserting `Some(4)` still renders “at most 4 model steps”.

- [ ] **Step 2: Add a failing work-state assertion**

Add a focused test asserting `None` renders `step 7`, while `Some(12)` renders `step 7 of 12`.

- [ ] **Step 3: Add failing reason serialization tests**

In `crates/agent-protocol/src/reason.rs`, serialize `Situation { max_steps: None, .. }` and assert `"max_steps": null`. Decode an old numeric value and assert `Some(n)`.

- [ ] **Step 4: Run the failing tests**

Run: `cargo test -p agent-loop an_unbounded_task_layer -- --nocapture`

Run: `cargo test -p agent-protocol reason:: -- --nocapture`

Expected: FAIL to compile until the fields are optional.

- [ ] **Step 5: Implement optional facts and rendering**

Change both facts types to `Option<u32>`. In `context_layers.rs`, handle `Some(1)`, `Some(n)`, and `None`. In `work_state.rs`, use `step X of N` only for `Some(N)` and `step X` otherwise. In `default.rs`, map the optional fact directly instead of `unwrap_or(0)`.

- [ ] **Step 6: Update constructors and tests**

Replace test literals with `Some(n)` where a cap is intended and add `None` only for unbounded tests.

- [ ] **Step 7: Run focused tests**

Run: `cargo test -p agent-ports -p agent-loop -p agent-protocol -- --nocapture`

Expected: PASS.

- [ ] **Step 8: Commit only isolated hunks**

```powershell
git diff -- crates/agent-ports/src/agent_loop.rs crates/agent-loop/src/context_layers.rs crates/agent-loop/src/default/work_state.rs crates/agent-loop/src/default.rs crates/agent-protocol/src/reason.rs
git add crates/agent-ports/src/agent_loop.rs crates/agent-loop/src/context_layers.rs crates/agent-loop/src/default/work_state.rs crates/agent-loop/src/default.rs crates/agent-protocol/src/reason.rs
git diff --cached --check
git commit -m "feat: render unbounded step semantics"
```

Preserve unrelated edits already present in these files.

---

### Task 4: Return partial subagent outcomes at explicit step caps

**Files:**
- Modify: `apps/agent/src/inprocess.rs`
- Modify: `crates/agent-ports/src/subagent.rs`
- Modify: `crates/agent-protocol/src/events.rs`

**Interfaces:**
- Consumes: `StopReason::MaxSteps`
- Produces: `SubagentOutcome::Partial { summary }` for step-capped children
- Keeps: `SubagentOutcome::Failed` for tool-call, wall-clock, token, and output-token limits

- [ ] **Step 1: Add failing subagent tests**

In `apps/agent/src/inprocess.rs`, test both `final_content: Some("half done")` and `None`. Assert `Partial`, child id present, “step” and “incomplete” present, and never `Completed`.

- [ ] **Step 2: Run the failing tests**

Run: `cargo test -p agent inprocess:: -- --nocapture`

Expected: FAIL because `MaxSteps` still maps to `Failed`.

- [ ] **Step 3: Implement the partial mapping**

Add `partial_at_steps(report, child)`. For `StopReason::MaxSteps`, return `Partial` with a summary naming the explicit ceiling, incomplete result, child session id, presence or absence of final text, and the option to continue or re-delegate. Leave other limit branches unchanged.

- [ ] **Step 4: Update protocol documentation**

Change the `SubagentOutcome::Partial` doc to allow parent-side synthesis only for an explicit step-ceiling pause with a verifiable child session. Update the matching `subagent/completed` comment in `crates/agent-protocol/src/events.rs`.

- [ ] **Step 5: Run focused tests**

Run: `cargo test -p agent inprocess:: -- --nocapture`

Run: `cargo test -p agent-ports subagent:: -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit only isolated hunks**

```powershell
git diff -- apps/agent/src/inprocess.rs crates/agent-ports/src/subagent.rs crates/agent-protocol/src/events.rs
git add apps/agent/src/inprocess.rs crates/agent-ports/src/subagent.rs crates/agent-protocol/src/events.rs
git diff --cached --check
git commit -m "feat: report step-capped subagents as partial"
```

Preserve unrelated edits already present in `inprocess.rs`.

---

### Task 5: Update Web text, contract judgment, and migration documentation

**Files:**
- Modify: `web/src/format.ts`
- Modify: `crates/agent-web/tests/http.rs`
- Modify: `CONTRACT.md`
- Modify: `PROJECT.md`
- Modify: `README.md`

**Interfaces:**
- Produces: UI text “达到步数上限” for an explicit `max_steps` ending
- Produces: one new `CONTRACT.md` §7 judgment row for the unbounded default
- Produces: operator migration guidance for removing the old `max_steps` line

- [ ] **Step 1: Update Web wording**

Change the `max_steps` label from “步数用尽” to “达到步数上限”. Do not add a zero/default display path; unbounded runs never emit that ending.

- [ ] **Step 2: Add a Web regression test**

In `crates/agent-web/tests/http.rs`, add a transcript/read-model fixture with more than 300 step events and no `budget_parked`. Assert the projected ending is not `max_steps` and no parking row is exposed.

- [ ] **Step 3: Run the failing Web regression**

Run: `cargo test -p agent-web no_synthetic_step_parking -- --nocapture`

Expected: FAIL until the fixture and projection assertion are added.

- [ ] **Step 4: Record the contract judgment**

Append a row to `CONTRACT.md` §7 after the last existing row. Decide: omitted `max_steps` is unbounded; JSON `null` is unbounded for new snapshots; missing field in old snapshots means historical 8; zero is invalid; stall/cancel/token/time/tool caps remain; explicit caps still park as `max_steps`. Update old text claiming 8 is still the default.

- [ ] **Step 5: Update operator docs**

In `PROJECT.md`, replace statements presenting 8 steps as the default. In `README.md`, document that omitting `max_steps` means no fixed ceiling and that an existing `max_steps` line must be removed. State that `max_tool_calls` remains independent.

- [ ] **Step 6: Run Web tests**

Run: `pnpm --dir web check`

Run: `cargo test -p agent-web -- --nocapture`

Expected: PASS.

- [ ] **Step 7: Commit only isolated hunks**

```powershell
git diff -- web/src/format.ts crates/agent-web/tests/http.rs CONTRACT.md PROJECT.md README.md
git add web/src/format.ts crates/agent-web/tests/http.rs CONTRACT.md PROJECT.md README.md
git diff --cached --check
git commit -m "docs: record unbounded step semantics"
```

Preserve unrelated edits already present in `CONTRACT.md`.

---

### Task 6: Whole-workspace verification and reverse test

**Files:**
- Verify only; change tests only if a missing assertion is discovered.

**Interfaces:**
- Consumes all prior tasks.
- Produces a verified branch and an explicit report of preserved dirty files.

- [ ] **Step 1: Format**

Run: `cargo fmt --all -- --check`

Expected: exit 0.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -j 1 -- -D warnings`

Expected: exit 0.

- [ ] **Step 3: Full tests**

Run: `cargo test --workspace`

Expected: 0 failed; passed count at least the 2026-11-02 baseline plus new tests.

- [ ] **Step 4: Web build**

Run: `pnpm --dir web build`

Expected: exit 0.

- [ ] **Step 5: Repository gate**

Run: `powershell -ExecutionPolicy Bypass -File scripts\test.ps1`

Expected: all three gates pass. If execution policy blocks it, report the exact blocker and rely on Steps 1-3.

- [ ] **Step 6: Reverse-test the boundary**

Temporarily change the 320-step test to pass `Some(8)`, confirm it fails with `MaxSteps`, restore `None`, and rerun. Record both observations.

- [ ] **Step 7: Review the final diff**

Run:

```powershell
git status --short
git diff --stat
git diff --check
```

Confirm no unrelated pre-existing edits were staged or reverted.

- [ ] **Step 8: Final commit only if task-owned hunks are isolated**

If unrelated changes remain inseparable, do not force a commit. Report the preserved files and verification evidence.

## Plan Self-Review

- Spec coverage: Tasks 1-3 cover optional budgets, scheduler behavior, context, persistence, and safety limits. Task 4 covers subagent partials. Task 5 covers Web text, contract, and migration. Task 6 covers the full gate and reverse test.
- Step scan: Every task has a failing test, implementation, passing test, and commit or explicit dirty-tree handling.
- Type consistency: `Option<u32>` is used for `RunBudget.max_steps`, `Scheduler::new`, `TurnStateFacts.max_steps`, and `Situation.max_steps`; parking receives `Option<u32>`.
- Review Focus coverage: Legacy decoding is tested in Task 1; 8/20/300 boundaries and other limits in Task 2; no-text partials in Task 4; serialization drift in Tasks 1 and 3.
- Proportion: The plan names interfaces and assertions rather than reproducing implementation bodies.