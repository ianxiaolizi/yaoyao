# Truthful Delivery Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Web UI report the kernel's existing execution, delivery, verification, and completion facts, and warn before a task relies on a runtime that cannot start commands.

**Architecture:** The kernel already derives the status axes and exposes them at `GET /api/sessions/{id}/status`; this plan wires that response into the Web state model, renders it as the authoritative latest-session status, and adds a local-backend capability warning. It does not change the completion gate or add a new fixed step ceiling.

**Tech Stack:** TypeScript, embedded Web bundle, Rust/axum read endpoint already present, Playwright smoke test, pnpm/Vite.

**Spec:** `docs/superpowers/specs/2026-11-09-agent-delivery-hardening-design.md`

## Global Constraints

- Never derive completion from `turn/ending=answered`; only render the kernel's status axes.
- Do not change `crates/agent-kernel/src/completion.rs`, evidence freshness, or completion-gate semantics.
- Do not reintroduce a default fixed `max_steps` ceiling.
- Keep `local` as a non-OS-sandbox backend; do not grant `process.spawn` or weaken risk-l3 refusal.
- Use the existing Chinese UI vocabulary and the existing API protocol/version rules.
- The Web remains readable when the status endpoint fails: keep the transcript usable and show a non-fatal notice.
- Preserve the existing `agent status` text and JSON behavior.

## Review Focus

- A turn with `ending=answered` but no completion evidence must never show “完成”.
- A historical turn must not inherit the latest session's verification state.
- A status request that fails must not erase the transcript or stop the SSE reader.
- `local` is the only sandbox warning in this phase; `read-only`, `app-container`, and unsupported `docker` must not be mislabeled.
- The status refresh after `turn_end` must tolerate the terminal event batch not being visible through a second request yet.

---

### Task 1: Add the status transport and state

**Files:**
- Modify: `web/src/types.ts`
- Modify: `web/src/api.ts`
- Modify: `web/src/state.ts`
- Modify: `web/src/main.ts`
- Test: `web` build plus `scripts/gui/smoke.mjs` in Task 4

**Interfaces:**
- Consumes: the existing `GET /api/sessions/{id}/status` response.
- Produces:
  - `export interface SessionStatus`
  - `export async function loadStatus(id: string): Promise<SessionStatus>`
  - `State.status: SessionStatus | null`
  - action `{ type: 'status/loaded'; status: SessionStatus }`
  - action `{ type: 'status/cleared' }`

- [x] **Step 1: Add the wire type**

In `web/src/types.ts`, add:

```ts
export interface SessionStatus {
  v: number;
  session: string;
  run_status: 'in_progress' | 'completed' | 'interrupted' | 'failed' | null;
  delivery_status: 'delivered' | 'unanswered' | null;
  verification_status: 'verified' | 'unverified' | 'stale' | 'not_applicable' | 'blocked' | null;
  completion_status: 'verified_complete' | 'blocked_or_parked' | null;
}
```

- [x] **Step 2: Add the API client**

In `web/src/api.ts`, add:

```ts
export async function loadStatus(id: string): Promise<SessionStatus> {
  return api<SessionStatus>(`/api/sessions/${encodeURIComponent(id)}/status`);
}
```

Import `SessionStatus` from `./types`.

- [x] **Step 3: Add state and reset rules**

In `web/src/state.ts`:

- import `SessionStatus`;
- add `status: SessionStatus | null` to `State`;
- add the two actions above;
- initialize `status` to `null`;
- clear it on `session/select` and `session/new`;
- update it on `status/loaded`;
- clear it on `status/cleared`.

- [x] **Step 4: Load and refresh the status**

In `web/src/main.ts`:

- import `loadStatus`;
- add `refreshStatus(id)` that dispatches `status/loaded` only while `store.get().active === id`;
- in `openSession`, load `loadSession(id)`, `loadTranscript(id)`, and `loadStatus(id)` together; a status failure must not fail the transcript load;
- on `item.kind === 'turn_end'`, call `void refreshStatus(id)`;
- after a successful `resume`, call `void refreshStatus(id)` after the next terminal event or immediately with the same non-fatal behavior used in `openSession`.

- [x] **Step 5: Verify the type layer**

Run:

```powershell
pnpm --dir web build
```

Expected: exit 0; no TypeScript errors about `SessionStatus` or the new state actions.

- [x] **Step 6: Commit**

```powershell
git add web/src/types.ts web/src/api.ts web/src/state.ts web/src/main.ts
git commit -m "feat(web): load kernel session status"
```

---

### Task 2: Render truthful status and warn on unverified delivery

**Files:**
- Modify: `web/src/format.ts`
- Modify: `web/src/view.ts`
- Modify: `web/src/styles/shell.css`
- Test: `scripts/gui/smoke.mjs` in Task 4

**Interfaces:**
- Consumes: `State.status`, `State.running`, `State.waiting`.
- Produces:
  - `export function sessionStatusLabel(status: SessionStatus | null): { label: string; className: string }`
  - `export function sessionStatusNote(status: SessionStatus | null): string | null`
  - the DOM node `#statusNote`

- [x] **Step 1: Add label and note helpers**

In `web/src/format.ts`, implement:

```ts
export function sessionStatusLabel(status: SessionStatus | null): { label: string; className: string } {
  if (!status) return { label: '已回答', className: 'done' };
  if (status.completion_status === 'verified_complete') return { label: '已验证完成', className: 'done' };
  if (status.verification_status === 'unverified') return { label: '已交付 · 未验证', className: 'unverified' };
  if (status.verification_status === 'stale') return { label: '验证已过期', className: 'fail' };
  if (status.verification_status === 'not_applicable') return { label: '已回答', className: 'done' };
  if (status.completion_status === 'blocked_or_parked') return { label: '未完成 · 已阻塞', className: 'fail' };
  return { label: '已回答', className: 'done' };
}

export function sessionStatusNote(status: SessionStatus | null): string | null {
  if (!status) return null;
  if (status.verification_status === 'unverified') {
    return '结果已交付，但没有通过完成检查。请先检查产出；未验证的交付不等于完成。';
  }
  if (status.verification_status === 'stale') {
    return '上一次验证已不再覆盖当前改动。继续修改后需要重新验证。';
  }
  if (status.completion_status === 'blocked_or_parked') {
    return '这次运行没有形成可验证的完成结论。请查看证据面板或继续处理。';
  }
  return null;
}
```

- [x] **Step 2: Change the neutral ending wording**

In `web/src/format.ts::turnStatus`, change:

```ts
case 'answered':
  return { label: '完成', className: 'done' };
```

to:

```ts
case 'answered':
  return { label: '已回答', className: 'done' };
```

This keeps historical turns truthful; the latest session status supplies the stronger completion label.

- [x] **Step 3: Add the status note DOM**

In `web/src/view.ts`, after the `<header class="y-top">...</header>` block, add:

```html
<div class="y-status-note" id="statusNote" hidden></div>
```

- [x] **Step 4: Render the latest-session status**

In the main `render(state, previous)` function:

- query `#statusNote`;
- compute `sessionStatusLabel(state.status)` for the top pill when the session is settled;
- keep `运行中` and `等待确认` precedence;
- set the note text from `sessionStatusNote(state.status)`;
- hide the note when there is no note;
- apply `warn`, `fail`, or `done` class from the status;
- do not use `state.status` when no current turn exists.

- [x] **Step 5: Style the note**

In `web/src/styles/shell.css`, add a compact `.y-status-note` row with existing token colors. It must be readable in both themes and use the same pill/tone classes as the rest of the shell. Do not add a modal.

- [x] **Step 6: Build**

Run:

```powershell
pnpm --dir web build
```

Expected: exit 0.

- [x] **Step 7: Commit**

```powershell
git add web/src/format.ts web/src/view.ts web/src/styles/shell.css
git commit -m "feat(web): show verified delivery status"
```

---

### Task 3: Warn when the local backend cannot start processes

**Files:**
- Modify: `web/src/view.ts`
- Modify: `web/src/styles/shell.css`
- Test: `scripts/gui/smoke.mjs` in Task 4

**Interfaces:**
- Consumes: `State.meta.sandbox`.
- Produces: the DOM node `#envNote` and the text rule described below.

- [x] **Step 1: Add the notice DOM**

Inside the composer block in `web/src/view.ts`, before `<textarea id="input"...>`, add:

```html
<div class="y-env-note" id="envNote" hidden></div>
```

- [x] **Step 2: Render the local-backend warning**

In `render(state, previous)`:

```ts
const envNote = $('#envNote');
const local = state.meta?.sandbox === 'local';
envNote.hidden = !local;
envNote.textContent = local
  ? '当前是 local 后端：爻爻可以编辑文件，但不会启动命令或渲染进程。需要构建、测试、导出视频时，请用 app-container 启动。'
  : '';
```

Do not block sending a turn. The warning is informational in this phase.

- [x] **Step 3: Style the notice**

In `web/src/styles/shell.css`, style `.y-env-note` as a compact, warning-toned line inside the composer. It must wrap on narrow screens and must not push the send/stop controls out of reach.

- [x] **Step 4: Build**

Run:

```powershell
pnpm --dir web build
```

Expected: exit 0.

- [x] **Step 5: Commit**

```powershell
git add web/src/view.ts web/src/styles/shell.css
git commit -m "feat(web): warn about local execution limits"
```

---

### Task 4: Add the incident regression fixture

**Files:**
- Modify: `scripts/gui/smoke.mjs`

**Interfaces:**
- Consumes: the new status client, status renderer, and environment warning.
- Produces: a reusable `unverified-delivery` fixture and assertions that fail if the page calls an unverified answer “完成”.

- [x] **Step 1: Add the fixture**

Add a seed function that writes this event shape:

```js
const log = builder('unverified-delivery');
log.add('run/started', { actor: 'scheduler' });
log.ask('t1', '做一个 3 秒的火柴人战斗视频');
log.add('tool/started', {
  turn: 't1',
  step: 'step-t1',
  actor: 'tool',
  payload: { tool_name: 'shell.run', arguments: { command: 'ffmpeg -version' } },
});
log.add('tool/failed', {
  turn: 't1',
  step: 'step-t1',
  actor: 'tool',
  payload: {
    tool_name: 'shell.run',
    status: 'failed',
    error_kind: 'sandbox_unavailable',
    content: 'policy denied tool shell.run: risk l3',
  },
});
log.add('turn/completed', {
  turn: 't1',
  actor: 'scheduler',
  payload: { final_content: '交付已完成。', stop_reason: 'final_answer' },
});
log.add('completion/unverified_result_delivered', {
  turn: 't1',
  actor: 'scheduler',
  payload: { verdict: 'missing', task_revision: 4 },
});
log.add('run/completed', { actor: 'scheduler', payload: { stop_reason: 'final_answer' } });
log.save();
```

- [x] **Step 2: Add the assertions**

After selecting the fixture session, assert all of:

```js
await expect(page.locator('#statusPill')).toContainText('已交付 · 未验证');
await expect(page.locator('#statusNote')).toBeVisible();
await expect(page.locator('#statusNote')).toContainText('未验证的交付不等于完成');
await expect(page.locator('#envNote')).toContainText('app-container');
```

Also assert that the top pill does not contain the bare word `完成` for this fixture.

- [x] **Step 3: Keep the existing answer fixture truthful**

Update the existing smoke assertion that accepts `完成|运行中|等待确认` so an answered turn with no evidence is expected to read `已回答`, not `完成`.

- [x] **Step 4: Run the smoke test**

Build the binary and run:

```powershell
cargo build -p agent
node scripts/gui/smoke.mjs
```

Expected: exit 0; all existing screenshots/tests pass; the new fixture shows `已交付 · 未验证` and the environment warning.

- [x] **Step 5: Run the targeted Rust status tests**

Run:

```powershell
cargo test -p agent-web --test http the_status_report_carries_the_three_axes
cargo test -p agent-web --test http a_log_without_run_events_still_answers_status
cargo test -p agent-cli --test ask_status_cli
```

Expected: all pass; the kernel's existing status semantics are unchanged.

- [x] **Step 6: Commit**

```powershell
git add scripts/gui/smoke.mjs
git commit -m "test(gui): cover unverified delivery status"
```

---

## Phase 0 Exit Criteria

- `web/src` compiles with the new status type and API call.
- The top pill never maps `answered` to bare “完成”.
- An `unverified` session displays “已交付 · 未验证” and the explanatory note.
- A `verified_complete` session displays “已验证完成”.
- The local sandbox notice is visible without blocking the composer.
- GUI smoke and the existing status endpoint/CLI tests pass.
- No completion-gate or risk-policy behavior changes.

## Deferred To Later Plans

The following are intentionally not implemented here; they are specified in the design document for Phase 1–3:

- repeated sandbox-denial escalation to `NeedsUserInput`;
- browser/render/export verification;
- post-edit lint/test hooks;
- edit churn and no-progress detection;
- snapshot-backed diff for non-Git workspaces;
- task-specific wall-clock/token/tool-call budgets.

## Execution Notes (2026-11-09)

- Phase 0 已实现，提交在 `feat/gui-m10` 分支：`e17420f`（状态接入）、`8a4deaa`（GUI 回归）、`71f84a5`（运行中不显示未验证横幅）。
- 验证：`pnpm --dir web build`、`cargo build -p agent` 通过；GUI smoke 139 项全过；`agent-web` 状态接口 3 项、`ask_status_cli` 21 项全过。
- 偏差：
  - fixture 的 `tool/started` / `tool/failed` 必须带 `provider_tool_call_id`，否则投影校验会让 `/api/sessions` 返回 500。
  - 额外新增 `verified-delivery` fixture，覆盖「已验证完成」正向文案（Phase 0 退出标准要求）。
  - `statusNote` 在运行中 / 等待确认时不渲染：进行中的 run 只是「尚未检查」，不是「未验证交付」。
