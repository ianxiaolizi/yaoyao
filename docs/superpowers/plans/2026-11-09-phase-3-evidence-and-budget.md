# Evidence and Budget Implementation Plan (Phase 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Close the evidence gap on non-Git workspaces, let an operator choose a run's limits by task type instead of one global budget, and make the cost of a failed verification visible in the metrics report.

**Architecture:** The workspace already records every change a snapshot-backed call made as `workspace/changes.recorded`; the Web diff panel reads it when no `git.diff` observation exists. Run budgets already compose from layers; a new user-layer `[task_budgets.<name>]` table is selected with `--task <name>` and applied after the profile, before the command line. `agent metrics` stays a pure fold over durable events: it learns a task's steps and final status from `step/started` and terminal turn events, and counts edits that follow a failed `verification/recorded` until a passing one covers them.

**Spec:** `docs/superpowers/specs/2026-11-09-agent-delivery-hardening-design.md` §5 Phase 3

## Global Constraints

- The diff endpoint stays a reader over durable facts: no fresh `git diff`, no filesystem comparison at request time.
- A change manifest says what changed (path, kind, sizes, digests), never a line patch. The panel must not imply a patch it does not have.
- Task budgets are user-layer only (`AGENT_USER_CONFIG`); the workspace layer must refuse the key. Budgets are not a capability, but a repository that can name its own task budget can move what its run spends.
- `--task` selects a named budget; it never classifies the task automatically. Pre-task classification stays deferred.
- Metrics remain a pure fold: no clock read, no elapsed time measured, same events → same bytes.
- `post_failure_edits` counts successful `fs.write` / `fs.edit` calls made after a `verification/recorded` whose status is `failed`, until a `passed` record covers the work. A check that could not run (`error`) is not a verdict and leaves the state unchanged.
- No completion-gate, evidence-freshness, policy, sandbox, or default step-ceiling change.
- User-facing text stays Chinese; prompt-layer and code text stay English.
- Additive wire/JSON changes only; existing CLI text lines keep their values, with new clauses appended.

## Review Focus

- A workspace with only snapshot changes reports them; a workspace with neither a `git.diff` observation nor a change manifest stays a 404.
- A malformed change manifest is skipped, not a 500 and not a panic.
- `--task video` with no `[task_budgets.video]` is refused and names the classes that exist.
- A task budget overrides the profile's limits but is still validated by the same rules (zero, percentages, stall floor).
- A workspace `[task_budgets]` table is an unknown field at parse time.
- Three edits after a failed verification count; a later passing verification resets the counter; a failed edit never counts.
- Steps count `step/started`; final status is the last terminal turn's ending and stop reason.
- The report remains byte-identical when folded twice.

---

### Task 1: Fall back to the workspace change ledger in the diff panel

**Files:**
- Modify: `crates/agent-web/src/sessions.rs` (`diff`)
- Modify: `crates/agent-web/src/lib.rs` (handler call and message)
- Modify: `web/src/view.ts` (renderer)
- Test: `crates/agent-web/tests/http.rs`

**Interfaces:**
- Produces: a diff report with `source: "workspace.changes"` and `changes: [{file, change, before_size, after_size, before_sha256?, after_sha256?}]` when no `git.diff` observation exists.
- Consumes: `EventType::WORKSPACE_CHANGES_RECORDED` payload (`ChangeManifest` JSON).

- [x] **Step 1: Read the ledger**

`sessions::diff` first looks for a completed `git.diff` transcript item exactly as today. If none exists, it reads the session log through the store and uses the newest `workspace/changes.recorded` payload. An unparsable payload is skipped. The report carries `source`, `seq`, `snapshot_id?`, `changes_hash?`, `complete`, and `changes`.

- [x] **Step 2: Render it**

`web/src/view.ts` branches on `source`. A `git.diff` report keeps its patch rendering. A `workspace.changes` report lists each path with 新增 / 修改 / 删除 and the byte delta, and says the ledger has no line patch.

- [x] **Step 3: Tests**

- A session with a completed `fs.write` and a `workspace/changes.recorded` event reports the change.
- A session with neither stays `404 diff_unavailable`.
- The existing `git.diff` report keeps its shape.

---

### Task 2: Task-type budgets, selected with `--task`

**Files:**
- Modify: `crates/agent-config/src/layers.rs` (`UserConfig`)
- Modify: `crates/agent-config/src/lib.rs` (`ConfigError`)
- Modify: `crates/agent-config/src/resolve.rs` (`Layers`, `ResolvedProfile`, `budget`, `resolve`)
- Modify: `crates/agent-protocol/src/profile.rs` (`ResolvedProfileSnapshot`)
- Modify: `crates/agent-cli/src/args.rs` (`--task`)
- Modify: `apps/agent/src/config.rs` (`load`)
- Modify: `apps/agent/src/main.rs` (usage)
- Test: the modules above and their tests

**Interfaces:**
- Produces: `UserConfig.task_budgets: BTreeMap<String, BudgetSpec>`
- Produces: `Layers.task_override: Option<String>`
- Produces: `ResolvedProfile.task_class: Option<String>`
- Produces: `ResolvedProfileSnapshot.task_class: Option<String>` (omitted when absent)
- Produces: CLI flag `--task <name>`.
- Consumes: `apply_budget`.

- [x] **Step 1: Declare the table**

`[task_budgets.video]` is a `BudgetSpec`. The key exists only on `UserConfig`; workspace and managed layers refuse it through `deny_unknown_fields`.

- [x] **Step 2: Select and overlay**

`--task <name>` sets `Layers.task_override`. `resolve` looks the name up in the user layer and refuses an unknown one with the known names. The selected spec is applied after the workspace and profile budgets, before the derived defaults and validation, so a task class can add a primary limit or tighten one.

- [x] **Step 3: Record the choice**

`ResolvedProfile.task_class` and the sealed snapshot carry the selected name, omitted when no task class was named. The profile/resolved payload therefore says which budget class the run used.

- [x] **Step 4: Tests**

- A user table parses; a workspace table is refused.
- `--task` applies the overlay over the profile's values.
- An unknown task class is refused and names the configured ones.
- The overlay is still validated (zero timeout, bad percent).
- A snapshot omits `task_class` when absent and keeps it when present.

---

### Task 3: Count post-failure edits, steps, and the final status

**Files:**
- Modify: `crates/agent-projection/src/metrics.rs`
- Test: `crates/agent-projection/src/metrics.rs`

**Interfaces:**
- Produces: `TaskMetrics.steps: u64`
- Produces: `TaskMetrics.post_failure_edits: u64`
- Produces: `SessionMetrics.final_status: Option<String>` (`<ending>:<reason>`, last terminal turn wins)

- [x] **Step 1: Fold the counters**

`STEP_STARTED` increments `steps`. A `verification/recorded` with status `failed` arms a failure; `passed` clears it; `error` / unknown leave it. A successful `fs.write` / `fs.edit` while armed increments both `edits` and `post_failure_edits`. A terminal turn event writes `final_status` from its ending and stop reason.

- [x] **Step 2: Tests**

- Failed verification → two edits → passed verification → one edit counts two post-failure edits.
- An `error` verification does not arm the counter.
- Failed edits are not counted.
- The last terminal turn's ending and reason win.
- Empty logs omit `final_status`.

---

### Task 4: Failure-cost report in `agent metrics`

**Files:**
- Modify: `apps/agent/src/metrics.rs`
- Test: `apps/agent/src/metrics.rs`

**Interfaces:**
- Produces: `task:` totals line with steps, tool calls, edits, verifications, criterion checks, post-failure edits, and final status.
- Produces: a per-session suffix with the same cost fields when the session did work.

- [x] **Step 1: Render**

The totals line becomes `task: <n> step(s), <n> tool call(s), <n> edit(s), <n> verification(s), <n> criterion check(s), <n> post-failure edit(s), final <status|open>`. A session with edits, verifications, or post-failure edits gets the same fields as a suffix.

- [x] **Step 2: Tests**

- The line names every field and the final status.
- JSON carries `steps`, `post_failure_edits`, and `final_status`.
- A report folded twice is byte-identical.

---

### Task 5: Sync the documents and run the suite

- [x] Tick this plan with the commands actually run.
- [x] Append `Execution Notes`: commits, test counts, deviations, deferred work.
- [x] Add the design spec's Phase 3 execution record pointing here.
- [x] Add `CONTRACT.md` §7 rows for the new invariants.
- [x] Commit the plan, implementation, tests, and web bundle.

## Phase 3 Exit Criteria

- A non-Git workspace whose calls recorded changes shows them in the 改动 panel, and a panel with neither source stays a 404.
- `agent run --task <name>` applies the named budget from the user layer; a workspace cannot define the table.
- `agent metrics` reports steps, tool calls, edits, verifications, post-failure edits, and the final status per session and in totals.
- Full suite, web build, and the verifier self-test pass; no completion-gate, policy, sandbox, or step-ceiling change.

## Deferred To Later Plans

- Automatic task-type classification before the first step.
- Per-message task-budget selection in the Web (the server resolves one class at startup today).
- Line-level patches from the change ledger (snapshots record digests and sizes, not bytes).
- Grouping metrics by task class.

## Execution Notes

**Commit:** one commit on `feat/gui-m10` carrying the plan, the implementation, the tests, the rebuilt web bundle, and the document updates (hash in `git log` for this file).

**Commands run (2026-11-09):**

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | clean (after `cargo fmt --all`) |
| `cargo clippy --workspace --all-targets -j 1 -- -D warnings` | clean |
| `cargo test --workspace` | 124 test binaries: **2436 passed, 0 failed, 17 ignored** |
| `pnpm --dir web build` | clean, `dist/assets/index-B-W3ulAZ.js` |
| `cargo test -p agent-web --test http -- change ledger` | 3 passed |
| `cargo test -p agent-config` | 108 passed |
| `cargo test -p agent-cli --test args_cli` | 13 passed |
| `cargo test -p agent-protocol --lib profile` | 9 passed |
| `cargo test -p agent-projection metrics` | 22 passed |
| `cargo test -p agent --lib metrics` | 9 passed |

**Deviations**

1. The report prints `final none` when a log has no terminal event and no open turn; the plan sketched `<status|open>`. "Work in progress" and "a log with no work" are different facts, and collapsing them would make an empty session look like a running one. An open turn without a terminal prints `final open` as planned.
2. `--task` is process-wide, so `agent web --task video` applies the class to every session that server runs. Per-message class selection is deferred (the server resolves one profile at startup); CONTRACT #197 records the limitation.
3. The diff fallback reports an empty `changes` list as a valid answer — the manifest says the comparison found nothing — while the panel shows "最近一次工作区快照比较没有发现改动". Only the absence of both a `git.diff` observation and a change manifest is a `404`.
4. `post_failure_edits` arms on any failed verification and disarms on any passing one; it does not match the edit to the file or check the failure named. Edit events do not always carry a path, and a path-level rule would be a guess about intent rather than a count of events. CONTRACT #198 records the rule.

**Deferred:** unchanged from the list above.
