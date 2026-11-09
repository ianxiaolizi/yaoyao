# Capability Preamble and Stop Rules Implementation Plan (Phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Make a run that keeps asking for a capability the backend cannot provide stop early and hand the decision to the user, and state the run's real capabilities before the first step instead of discovering them through repeated refusals.

**Architecture:** The kernel already refuses `process.spawn` on `local` through the isolation floor and records the refusal layer (`sandbox_unavailable`) in the `tool/failed` payload. The loop counts those refusals inside one turn and ends the turn with a new `capability_blocked` reason once the same gap is asked for twice. The turn ends as a completed-but-incomplete record, not a yielded one, because a yielded turn with no pending approval has no continuation path in this build (`Resume::classify` returns `Unresumable`, and the Web has no resume-with-text entry). The status axes stay truthful: unfinished is not delivered, and the Web label prefers “未完成 · 已阻塞” over “已交付 · 未验证”. The capability layer states files/commands/browser-render/export before the first request.

**Spec:** `docs/superpowers/specs/2026-11-09-agent-delivery-hardening-design.md` §5 Phase 1

## Global Constraints

- Never grant `process.spawn` or weaken the risk-l3 refusal; `local` stays a non-OS-sandbox backend.
- Do not change the completion gate, evidence freshness, or the default step ceiling.
- Do not reintroduce a default fixed `max_steps`.
- The stop rule is per-turn and mechanical: it counts recorded `sandbox_unavailable` refusals, not model prose.
- Keep the existing Chinese UI vocabulary in user-facing text; prompt-layer text stays English.
- `agent status`'s existing JSON/text behavior is preserved; new fields are additive.
- A `capability_blocked` turn must not appear as delivered: `incomplete: true` keeps it off the delivery axis.

## Review Focus

- One sandbox-unavailable refusal alone must not stop a turn that then finds a working path.
- A policy `rule`/`managed_policy` denial must not count as a capability gap.
- A stopped turn must answer every tool call it proposed (remaining calls become `tool/not_started`), or the next request's history is invalid.
- The stop text names the refused tool, the missing capability, and the three choices; it is kernel-authored, not model-generated.
- The Web must not show “已交付” for a turn whose `delivery_status` is `unanswered`.
- A status endpoint failure must remain non-fatal to the transcript (Phase 0 behavior).

## Planned Deviation

The design says “转入 NeedsUserInput”. This plan ends the turn `capability_blocked` (completed + `incomplete: true`) instead, because in this build a yielded turn with no held call is unresumable and would wedge both CLI and Web. The user's next message is the three-way choice; the kernel records it as an ordinary new turn. Record this in the execution notes and the design follow-up.

---

### Task 1: State the run's capabilities before the first step

**Files:**
- Modify: `crates/agent-loop/src/context_layers.rs`
- Test: existing `capability_text` tests in `crates/agent-loop/src/default.rs`

**Interfaces:**
- Consumes: `TurnCapabilityFacts.sandbox`, `TurnCapabilityFacts.network_allowed`.
- Produces: the capability layer's text lines for files, commands, and browser/render/export.

- [x] **Step 1: Add the capability lines**

In `capability_text`, after the sandbox line, add a line per sandbox mode:

```text
- files: read-only (no writes) | read and write inside the workspace
- commands: not available (this backend starts no process) | available under the sandbox confinement
- browser/render/export: not available (needs a process) | available only when the sandbox has the program and a command tool is offered
```

Derive each word from `facts.sandbox`; `ReadOnly` writes none, `LocalProcess` starts no process, `WorkspaceSandbox`/`Container` do.

- [x] **Step 2: Add the substitution rule**

Append one sentence:

```text
If the task needs a capability this run does not have, say so and ask the user which way to go; do not call a static file, an HTML page, or a mock the real result.
```

- [x] **Step 3: Test the wording**

Extend the existing `capability_text` tests: local names commands as unavailable and browser/render/export as unavailable; app-container names commands as available; read-only names writes as unavailable.

- [x] **Step 4: Build**

```powershell
cargo test -p agent-loop capability
```

Expected: exit 0.

---

### Task 2: Stop a turn that asks twice for an unavailable capability

**Files:**
- Modify: `crates/agent-ports/src/agent_loop.rs`
- Modify: `crates/agent-loop/src/default.rs`
- Modify: `crates/agent-kernel/src/scheduler/endings.rs`
- Modify: `crates/agent-cli/src/lib.rs`
- Test: `crates/agent-loop/src/default.rs` (unit), `crates/agent-kernel/src/scheduler.rs`

**Interfaces:**
- Produces: `StopReason::CapabilityBlocked` (`as_str` = `capability_blocked`).
- Produces: `LoopTurn::note_capability_gap(tool, capability) -> bool` and `capability_gap_text(...)`.
- Consumes: `ToolGatewayError::PolicyDenied { policy, .. }` whose `policy.refusals` contains `RefusalLayer::SandboxUnavailable`.

- [x] **Step 1: Add the stop reason**

In `StopReason`, add:

```rust
/// The turn kept asking for a capability this backend does not provide
/// (for example `process.spawn` on `local`). It stops so a person can
/// choose: switch backends, accept a degraded result, or cancel.
CapabilityBlocked,
```

and map it in `as_str` to `"capability_blocked"`.

- [x] **Step 2: Count the gap in the loop**

Add to `LoopTurn`:

```rust
/// Consecutive-or-not `sandbox_unavailable` refusals this turn has seen.
capability_denials: u32,
/// The last refusal that named a capability gap: (tool, capability).
last_capability_gap: Option<(String, String)>,
```

Default both. In the `Err(e)` branch after the `policy` payload is written, before building the observation:

```rust
if let ToolGatewayError::PolicyDenied { policy, .. } = &e
    && let Some((capability, _)) = policy
        .refusals
        .iter()
        .find(|(_, layer)| **layer == RefusalLayer::SandboxUnavailable)
{
    turn.capability_denials = turn.capability_denials.saturating_add(1);
    turn.last_capability_gap = Some((call.tool_name.clone(), capability.as_str().to_string()));
}
```

Do not count `RefusalLayer::Rule` or `ManagedPolicy`.

- [x] **Step 3: End the turn at the second gap**

After the refusal event, observation, and tool message are pushed (the same sequence the refusal path already writes), if `turn.capability_denials >= 2`:

```rust
let text = capability_gap_text(
    turn.ctx.capability.as_ref().map(|facts| facts.sandbox),
    turn.last_capability_gap.as_ref(),
);
refuse_undispatched(
    ctx,
    &step_id,
    turn,
    &mut events,
    &tool_calls[index + 1..],
    &StopReason::CapabilityBlocked,
    None,
);
events.push(draft(ctx, &step_id, EventType::STEP_COMPLETED, serde_json::json!({}), Actor::Agent));
return Ok(StepOutcome {
    step_id,
    events,
    stop: Some(StopReason::CapabilityBlocked),
    final_content: Some(text),
    spent: turn.spent,
});
```

Add a `StopReason::CapabilityBlocked` arm to `refuse_undispatched` saying the turn stopped on a capability gap and nothing ran.

- [x] **Step 4: Write the three-way text**

`capability_gap_text` (kernel-authored, user-facing Chinese):

```text
能力缺口：`<tool>` 需要 `<capability>`，当前 `<sandbox>` 后端不能提供，已停止，不再用静态文件假装完成。
请选择：
1. 用 app-container 后端重启后重试；
2. 接受降级交付（只交付静态文件，并明确它不是原目标）；
3. 取消这个任务。
回复 1、2 或 3。
```

- [x] **Step 5: Settle it truthfully**

In `Ending::settle`, add `StopReason::CapabilityBlocked` beside `SecurityBlocked` for the terminal event types (`turn/completed` + `run/completed`), and give it a payload arm:

```rust
serde_json::json!({
    "stop_reason": "capability_blocked",
    "incomplete": true,
    "final_content": content,
})
```

- [x] **Step 6: Teach the CLI**

Add `CapabilityBlocked` to `halted`, and in `stopped_text` print a specific notice (not a budget advice): the text above, followed by “not delivered: the backend cannot provide the capability this task needs; choose app-container, a degraded result, or cancel”.

- [x] **Step 7: Tests**

- Two `sandbox_unavailable` denials in one turn → `StopReason::CapabilityBlocked`, remaining calls `tool/not_started`, final text names `app-container` and the three options.
- One denial then a successful file call → the turn continues.
- A `RefusalLayer::Rule` denial twice → no `CapabilityBlocked`.
- Kernel: the turn settles `turn/completed` + `run/completed` with `incomplete: true`; status is `delivered=Unanswered`, `completion=BlockedOrParked`.

- [x] **Step 8: Run**

```powershell
cargo test -p agent-loop capability
cargo test -p agent-kernel capability
cargo test -p agent-cli halted
```

Expected: pass.

---

### Task 3: Make the Web read an unfinished ending as not delivered

**Files:**
- Modify: `crates/agent-web/src/sessions.rs`
- Modify: `web/src/types.ts`
- Modify: `web/src/format.ts`
- Modify: `web/src/view.ts` (only if the note needs the new field)

**Interfaces:**
- Produces: `SessionStatus.stop_reason?: string | null` (additive; `v` stays 1).
- Produces: a label rule “delivery unanswered → 未完成 · 已阻塞”.
- Produces: a note for `stop_reason === 'capability_blocked'`.

- [x] **Step 1: Add the stop reason to the status response**

In `sessions::status`, read the latest `turn/*` terminal's `stop_reason` from the same event slice and add:

```rust
"stop_reason": latest_stop_reason,
```

- [x] **Step 2: Add the wire field**

In `SessionStatus`:

```ts
stop_reason?: string | null;
```

- [x] **Step 3: Fix the label order**

`sessionStatusLabel`:

```ts
if (status.completion_status === 'verified_complete') return { label: '已验证完成', className: 'done' };
if (status.delivery_status === 'unanswered') return { label: '未完成 · 已阻塞', className: 'fail' };
if (status.verification_status === 'unverified') ...
```

- [x] **Step 4: Add the capability note**

In `sessionStatusNote`, before the generic blocked note:

```ts
if (status.stop_reason === 'capability_blocked') {
  return '这次任务需要当前后端没有的执行能力（例如启动命令或渲染）。请切换 app-container、接受降级交付，或取消。';
}
```

- [x] **Step 5: Build**

```powershell
pnpm --dir web build
```

Expected: exit 0.

---

### Task 4: Regression fixtures

**Files:**
- Modify: `scripts/gui/smoke.mjs`
- Test: `crates/agent-loop/src/default/tests/*` (new capability fixture)

- [x] **Step 1: Add the loop fixture**

Seed two `tool/failed` events with `policy.refusals = { "process.spawn": "sandbox_unavailable" }` and assert the turn ends `capability_blocked` with the three options.

- [x] **Step 2: Add the GUI fixture**

Seed a `capability-blocked` session: run/started, a `tool/failed` with the sandbox refusal, `turn/completed` with `stop_reason: capability_blocked`, `incomplete: true`, `final_content` (the three options), run/completed. Assert:

```js
await expect(page.locator('#statusPill')).toContainText('未完成 · 已阻塞');
await expect(page.locator('#statusNote')).toContainText('app-container');
await expect(page.locator('#statusPill')).not.toContainText('已交付');
```

- [x] **Step 3: Run the smoke test**

```powershell
cargo build -p agent
node scripts/gui/smoke.mjs
```

Expected: exit 0.

- [x] **Step 4: Full suite**

```powershell
cargo test --workspace
```

Expected: 0 failed; record exact pass/ignore counts in the execution notes.

---

### Task 5: Sync the documents

- [x] Tick this plan's checkboxes with the commands actually run.
- [x] Append an `Execution Notes` section: commits, test counts, the `NeedsUserInput` → `capability_blocked` deviation, and anything deferred.
- [x] Add one line to the design spec's Phase 1 section pointing at this plan and the deviation.
- [x] Commit the plan, the implementation, and the tests.

## Execution Notes

### Commits

- `b69d3fc` — implementation and tests: `crates/agent-ports`, `crates/agent-loop`, `crates/agent-kernel`, `crates/agent-cli`, `crates/agent-web`, `apps/agent`, `web/`, `scripts/gui/`, on the work branch `feat/gui-m10`.
- The docs commit that carries this plan's checkboxes and the design-spec pointer follows it on the same branch.

### Where the code landed

- `crates/agent-ports/src/agent_loop.rs`: `StopReason::CapabilityBlocked` (`as_str` = `capability_blocked`).
- `crates/agent-loop/src/default/capability.rs` (new): `CapabilityGaps`, `LoopTurn::note_capability_gap(&ToolGatewayError, tool) -> bool`, `stop(...)`, and `gap_text` (the kernel-authored three-way ask). The planned inline snippet became a method that takes the error, so the loop only names the tool once.
- `crates/agent-loop/src/default/refusals.rs` (new): `refuse_undispatched` moved verbatim out of `default.rs`, plus its `CapabilityBlocked` arm.
- Why the two modules: `crates/agent-testkit/tests/module_size_guard.rs` ratchets `default.rs` at 7737 lines. The change first took it to 7979, and the remedy is to split (files may shrink, never grow). `default.rs` is now 7698. The capability tests moved to `crates/agent-loop/src/default/tests/capability_stop.rs`; test files are out of the ratchet's scope.
- `crates/agent-kernel/src/scheduler/endings.rs`: settles `turn/completed` + `run/completed` with `incomplete: true`, beside `SecurityBlocked`.
- `crates/agent-kernel/src/status.rs`: pinned that the ending is `Completed` / `Unanswered` / `Unverified` / `BlockedOrParked`.
- `crates/agent-cli/src/lib.rs`: `halted`, a specific `stopped_text` notice, and the limit-advice arm.
- `apps/agent/src/inprocess.rs`: a child that stops on a capability gap is reported `Failed` with a message saying its text is a question, not a result.
- `crates/agent-web/src/sessions.rs`, `web/src/types.ts`, `web/src/format.ts`: `stop_reason` on the status payload, and the label/note ordering fix. The rule is “delivery `unanswered` on a session that has a run → 未完成 · 已阻塞”, ahead of the unverified label; a session with no run yet keeps “已回答”. Keying on `run_status !== null` rather than `completion_status === 'blocked_or_parked'` also covers a completed run that did no tool work, where the completion axis makes no claim at all and the old code would have said “已回答” to an empty delivery.
- `scripts/gui/smoke.mjs`: `seedCapabilityBlocked()` fixture, pill/note/transcript assertions, `m10-capability-blocked.png`, session totals 10 → 11.

### Verification (commands actually run)

```text
cargo fmt --all                                              # clean
cargo check --workspace --all-targets                         # clean
cargo clippy --workspace --all-targets -j 1 -- -D warnings    # clean
cargo test -p agent-loop --lib capability                     # 11 passed (149 filtered out)
cargo test -p agent-testkit --test module_size_guard          # 4 passed
cargo test --workspace                                        # 2389 passed, 0 failed, 17 ignored
cargo build -p agent && node scripts/gui/smoke.mjs            # 144 PASS, ok: true, no console errors
```

### Deviations from the design

1. **`NeedsUserInput` → `capability_blocked`.** Recorded above under Planned Deviation and now in the design spec's Phase 1. The record is a completed turn with `incomplete: true`; the person's next message is an ordinary new turn. A yield here would be unresumable (`Resume::classify` → `Unresumable` with no held call), and the Web has no input affordance that would resume it.
2. **Per-turn, not per-run.** The counter lives on `LoopTurn`, so a second turn of the same run starts from zero. That matches what a turn can observe; a run-level count would need a field the loop does not receive today.
3. **"risk-l3" refusals are the `sandbox_unavailable` ones in this build.** A high-risk command (`shell.run`) asks for `process.spawn`; on `local` the isolation floor refuses it as `SandboxUnavailable`, so the design's `/ risk-l3` is covered by the same layer check. `rule` / `managed_policy` denials are call-level decisions the model can replan around and are deliberately not counted; two of them must not end a turn (tested).
4. **No sandbox name in the ask.** The planned text interpolated the sandbox mode; the implemented text names the refused tool and capability (or stays neutral) and always names `app-container` as the alternative. The mode is already in the capability layer, and the stop text stays the same on every backend.
5. **Web label ordering changed beyond the new note.** `sessionStatusLabel` now prefers a session with a run and `unanswered` delivery → “未完成 · 已阻塞” before `unverified`. Previously any parked run with tool work read “已交付 · 未验证”, and a run that delivered nothing without tool work read “已回答”; both were false, and this plan's exit criteria required the fix. The presence of a run is read from the existing `run_status` field, so the wire change stays additive.

### Deferred

As listed under Deferred To Later Plans below — no resumable input yield, no pre-task task-type classification, no browser render/screenshot/console verification, no export verification, no post-edit hooks.

## Phase 1 Exit Criteria

- The capability layer names files, commands, browser/render/export, and network before the first request, on every sandbox mode.
- Two `sandbox_unavailable` refusals in one turn end it `capability_blocked`; one does not.
- The ending is not delivered: status shows `unanswered` + `blocked_or_parked`, and the Web shows “未完成 · 已阻塞” with the capability note.
- Every proposed call has an outcome in the log after the stop.
- No `process.spawn` grant, no risk-l3 weakening, no fixed step ceiling.
- Full suite and GUI smoke pass.

## Deferred To Later Plans

- A first-class “needs user input” yield with a resumable continuation (kernel + Web input affordance).
- Task-type classification of “needs process/render/export” before the first step.
- Browser render, screenshot, console check, and media export verification.
- Post-edit lint/test hooks, edit-churn detection, snapshot-backed diffs, and task budgets.
