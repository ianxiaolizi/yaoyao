# Verification Loop and Convergence Implementation Plan (Phase 2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make verification a named, repeatable part of a task instead of an ad-hoc command: operator-configured recipes run real checks (a browser renders a page, a browser decodes a video), the loop warns a model that keeps editing one file without a passing check, and the metrics report records edits, verifications, and the last change digest.

**Architecture:** `agent verify` already starts a program directly and records `verification/recorded` evidence (`crates/agent-kernel/src/verifier.rs`). This plan adds a recipe layer in front of that path: recipes live in the **user** configuration only, `agent verify --recipe <name> --target <artifact>` expands one into a `CheckSpec`, and the existing verifier records the result. Two Playwright-based check programs (`scripts/verify/web.mjs`, `scripts/verify/video.mjs`) use the Edge binary the GUI harness already drives. The loop gains a convergence notice beside the stall notice; it is a reminder, not a new stop reason. `agent metrics` gains a derived task block.

**Spec:** `docs/superpowers/specs/2026-11-09-agent-delivery-hardening-design.md` §5 Phase 2

## Global Constraints

- Recipes are read from the **user** layer (`AGENT_USER_CONFIG`) only. The workspace layer is model-writable; a check a workspace could define is a check the work could grade itself with.
- The verifier still starts a program directly (`EnvCommandVerifier`), never through a shell; no policy or approval change.
- Do not change the completion gate, evidence freshness, the isolation floor, or the default step ceiling.
- The convergence notice is a reminder: it must never end a turn or add a stop reason.
- Metrics stay a pure fold over durable events; no clock is read and no elapsed time is measured.
- User-facing text stays Chinese; prompt-layer text stays English.
- Additive: `agent verify [flags] -- <program> [args]` keeps its current behaviour, exit codes, and JSON shape; new JSON fields, if any, are additive.
- Tests use `tempfile`; no repository artifacts, no shared fixed temporary paths.

## Review Focus

- A recipe defined in `.agent/config.toml` is not seen by `agent verify`.
- Recipe arguments are passed to the program verbatim; no shell re-parses them.
- An unknown recipe names the recipes that do exist; an unknown placeholder names the one supported placeholder.
- A failing check command must not clear the convergence notice; a passing one must.
- The notice appears once per edit cycle (same `message_uid`, replaced not repeated) and disappears after a pass.
- Metrics count only successful `fs.write` / `fs.edit` results, and a malformed `workspace/changes.recorded` payload must not panic the fold.
- The task digest is the recorded `after_sha256` of the last change, not a hash of the model's arguments.

---

### Task 1: Add verifier recipes to the user configuration

**Files:**
- Create: `crates/agent-config/src/verify.rs`
- Modify: `crates/agent-config/src/layers.rs` (`UserConfig`)
- Modify: `crates/agent-config/src/lib.rs` (module + re-export)
- Test: `crates/agent-config/src/verify.rs`, `crates/agent-config/src/layers.rs`

**Interfaces:**
- Produces: `VerifySpec { recipes: BTreeMap<String, RecipeSpec> }`
- Produces: `RecipeSpec { program, args, cwd, timeout_ms, expect_exit, expect_stdout, expect_tests, apparatus }`
- Produces: `RecipeSpec::to_check_spec(&self, recipe: &str, target: Option<&str>) -> Result<CheckSpec, String>`
- Consumes: `agent_protocol::verification::{CheckSpec, Expectation, DEFAULT_CHECK_TIMEOUT_MS}` and `agent_protocol::apparatus::ApparatusSpec`.

- [x] **Step 1: Write the recipe types**

`VerifySpec` and `RecipeSpec` are `#[serde(deny_unknown_fields)]`; `args` and `apparatus` default to empty. `UserConfig` gains `pub verify: Option<VerifySpec>`. No other layer gains the key.

- [x] **Step 2: Validate and expand**

`to_check_spec` refuses an empty program, a target with an unknown `{placeholder}`, and a recipe name containing whitespace; it substitutes `{target}` in every argument. When a target is given and no argument names it, the target is appended as the last argument. Timeout defaults to `DEFAULT_CHECK_TIMEOUT_MS`, expectation defaults match `Expectation::default()`.

- [x] **Step 3: Tests**

- A user configuration with `[verify.recipes.web]` parses.
- The same table in a workspace configuration is refused by `deny_unknown_fields`.
- `{target}` is substituted in place; an unknown placeholder is refused and the message names `{target}`.
- A target with no placeholder is appended; no target leaves the args unchanged.
- An empty program and a recipe name with whitespace are refused.

---

### Task 2: `agent verify --recipe <name> --target <artifact>`

**Files:**
- Modify: `apps/agent/src/config.rs` (read recipes from the user layer)
- Modify: `apps/agent/src/commands/verify.rs` (parse and resolve a recipe)
- Test: `apps/agent/src/commands/verify.rs`, `apps/agent/tests/verify_recipes.rs` (new)

**Interfaces:**
- Produces: `config::verify_recipes(workspace_root: &str) -> Result<BTreeMap<String, RecipeSpec>, LoadError>`
- Produces: CLI flags `--recipe <name>`, `--target <value>`; both are command-local.
- Consumes: `RecipeSpec::to_check_spec`.

- [x] **Step 1: Read the user layer**

`config::verify_recipes` reads only `AGENT_USER_CONFIG` through the same parser (`UserConfig::from_toml_str`) and returns the recipe table. An unset variable is an empty table, not an error.

- [x] **Step 2: Parse the source**

`verify::parse` returns a source: either the existing program-after-`--` form or `Recipe { name, target }`. Refuse `--recipe` together with a program after `--`, and refuse `--target` without `--recipe`. The existing override flags (`--cwd`, `--timeout-ms`, `--expect-*`, `--apparatus`) still apply on top of a recipe.

- [x] **Step 3: Resolve the recipe**

Look the name up; an unknown name prints the known names in sorted order. Build the spec through `RecipeSpec::to_check_spec`, then apply the command-line overrides. A recipe with no `{target}` and no `--target` is legal; the artifact is simply not part of the check.

- [x] **Step 4: End-to-end test**

`apps/agent/tests/verify_recipes.rs` writes a user configuration in a temp directory, sets `AGENT_USER_CONFIG`, and runs the real binary:
- a recipe that exits 0 records `verification/recorded` and exits 0;
- a recipe that exits 3 records the failure and exits 1;
- an unknown recipe exits 2 and its stderr names the configured recipe;
- a recipe placed in the workspace configuration is reported as an unknown field and the command does not run.

- [x] **Step 5: Build**

```powershell
cargo test -p agent-config verify
cargo test -p agent --test verify_recipes
```

Expected: pass.

---

### Task 3: Web artifact check program

**Files:**
- Create: `scripts/verify/package.json`
- Create: `scripts/verify/package-lock.json`
- Create: `scripts/verify/web.mjs`
- Create: `scripts/verify/selftest.mjs`

**Interfaces:**
- Consumes: `playwright-core` + an Edge/Chromium executable (`EDGE_PATH`, default `C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`).
- Produces: exit 0 + `CHECK_OK ...` on stdout when the page loads, renders visible content, and reports no console error; exit 1 + `CHECK_FAILED ...` on stderr otherwise. `--screenshot <path>` writes a PNG.

- [x] **Step 1: Write `web.mjs`**

Arguments: `--target <path-or-url>` (required), `--screenshot <path>`, `--timeout-ms <n>` (default 30,000). Convert a readable path to a `file://` URL. Collect `console` errors and `pageerror`s; for http(s) targets also collect 4xx/5xx responses. Fail when the body has no visible text and no `canvas`/`svg`/`video`/`img`. Screenshot the page (full page) when asked.

- [x] **Step 2: Write `selftest.mjs`**

In `os.tmpdir()`: (a) a page that renders text passes; (b) a page whose script throws fails; (c) a page with an empty body fails. The self-test spawns `web.mjs` as a child process and checks the exit codes, so it exercises the same shape `agent verify` uses.

- [x] **Step 3: Run**

```powershell
npm ci --prefix scripts/verify
node scripts/verify/selftest.mjs
```

Expected: exit 0, three checks reported.

---

### Task 4: Video artifact check program

**Files:**
- Create: `scripts/verify/video.mjs`
- Modify: `scripts/verify/selftest.mjs`

**Interfaces:**
- Produces: exit 0 + `CHECK_OK ... duration_ms=<n> width=<n> height=<n>` when the browser decodes metadata and a frame; exit 1 otherwise. Optional `--min-duration-ms`, `--max-duration-ms`, `--width`, `--height`, `--screenshot`.

- [x] **Step 1: Write `video.mjs`**

Load the target in a `<video>` element on a generated page. Wait for `loadedmetadata` (bounded by `--timeout-ms`), require a finite positive duration, seek to a frame, wait for `seeked`, draw it to a canvas, and require the canvas to contain more than one distinct pixel value (a blank decode is not a pass). Apply the optional duration/dimension expectations. Screenshot the video element when asked.

- [x] **Step 2: Extend the self-test**

Generate a short WebM in the browser with `canvas.captureStream()` + `MediaRecorder`, write it to the temp directory, and assert `video.mjs` passes on it. Assert `video.mjs` fails on a text file. Skip the generated-video leg with a visible message when the browser cannot record (no silent pass).

- [x] **Step 3: Run**

```powershell
node scripts/verify/selftest.mjs
```

Expected: exit 0, web and video legs reported.

---

### Task 5: Convergence notice for repeated edits without a passing check

**Files:**
- Create: `crates/agent-loop/src/default/convergence.rs`
- Modify: `crates/agent-loop/src/default.rs` (field, two call sites, one echo)
- Modify: `crates/agent-protocol/src/model.rs` (`convergence_notice_for_turn`)
- Test: `crates/agent-loop/src/default/tests/convergence.rs` (new)

**Interfaces:**
- Produces: `convergence::Edits` with `note_edit(path)`, `note_pass()`, and `place_notice(turn, turn_id) -> Option<String>`.
- Produces: `ModelMessage::convergence_notice_for_turn`, uid `msg/convergence/{turn_id}`.
- Consumes: successful `fs.write` / `fs.edit` results and successful check-like `shell.run` results (`checks::check_command` + `checks::is_check_command`).

- [x] **Step 1: Track edits and passes**

`Edits` counts edits per path since the last passing check. Reaching `EDIT_CHURN_THRESHOLD = 3` on one path arms a notice; a passing check clears the counts and the notice. The notice text names the path and the count and tells the model to verify or say what blocks verification. It is English, like the stall notice.

- [x] **Step 2: Place it**

`place_notice` removes any stale notice with the same uid, then inserts the current one at the message tail, the same way `place_stall_notice` does. Call it beside the stall notice before request preparation, and echo `convergence_notice` into the prepared payload (path, count, text) for read-back.

- [x] **Step 3: Feed it**

In the tool-result path: a Completed `fs.write`/`fs.edit` calls `note_edit(path)`; a Completed command whose text is check-like calls `note_pass()`. A failed command does neither.

- [x] **Step 4: Tests**

- Three edits to one path inject one notice with the path and `3`.
- A fourth edit replaces the notice rather than adding a second.
- A passing check removes the notice and resets the count.
- A failing check leaves the notice in place.
- Two edits never inject a notice.
- The notice is in the prepared payload and the turn still ends normally.

---

### Task 6: Task block in the metrics report

**Files:**
- Modify: `crates/agent-projection/src/metrics.rs`
- Modify: `apps/agent/src/metrics.rs`
- Test: `crates/agent-projection/src/metrics.rs`

**Interfaces:**
- Produces: `TaskMetrics { edits, verifications, criterion_checks, last_modified_path?, last_modified_sha256? }` on `SessionMetrics` as `task`.
- Produces: a `task:` totals line and a per-session suffix in the text report.

- [x] **Step 1: Fold the counters**

`TOOL_COMPLETED` with `tool_name` `fs.write`/`fs.edit` increments `edits`. `VERIFICATION_RECORDED` increments `verifications`; `COMPLETION_CHECKED` increments `criterion_checks`. `WORKSPACE_CHANGES_RECORDED` parses the change manifest and keeps the last change that has an `after_sha256` (path + digest). A payload that does not parse is ignored, not fatal.

- [x] **Step 2: Render**

The text report adds `task: <n> edit(s), <n> verification(s), <n> criterion check(s), last change <path> <digest12>` and a per-session suffix when the session has edits or verifications. JSON carries the same fields under `task`; absent digests are omitted.

- [x] **Step 3: Tests**

- Successful writes/edits count; failed ones do not.
- A verification record and a completion check each count once.
- The last change digest is the last non-empty `after_sha256` in event order.
- A malformed changes payload leaves the counters intact and does not panic.

---

### Task 7: Regression fixtures

**Files:**
- Modify: `scripts/verify/selftest.mjs`
- Test: `apps/agent/tests/verify_recipes.rs`, `crates/agent-loop/src/default/tests/convergence.rs`, `crates/agent-projection/src/metrics.rs`

- [x] **Step 1: Add the negative cases**

Every new behaviour gets a test that fails if the behaviour is removed: a workspace recipe is not seen, an unknown recipe is refused, a third edit warns, a pass clears it, a failed edit is not counted.

- [x] **Step 2: Full suite**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -j 1 -- -D warnings
cargo test --workspace
```

Expected: 0 failed; record exact pass/ignore counts in the execution notes.

---

### Task 8: Sync the documents

- [x] Tick this plan's checkboxes with the commands actually run.
- [x] Append an `Execution Notes` section: commits, test counts, deviations, deferred work.
- [x] Add one line to the design spec's Phase 2 section pointing at this plan.
- [x] Add a `CONTRACT.md` §7 row for the two new invariants: recipes are user-layer only, and the convergence notice is not a stop.
- [x] Commit the plan, the implementation, and the tests.

## Phase 2 Exit Criteria

- `agent verify --recipe <name> --target <artifact>` records structured evidence, using only recipes from the user layer.
- A web artifact passes only when it renders visible content with no console error; a screenshot can be written.
- A video artifact passes only when the browser decodes a frame; duration and dimensions are reported.
- Three edits to one file without a passing check produce one convergence notice; a passing check clears it; no turn is stopped by it.
- `agent metrics` reports per-session edits, verifications, criterion checks, and the last change digest.
- Full suite and the verifier self-test pass; no completion-gate, policy, or sandbox change.

## Deferred To Later Plans

- A model-callable `verify.run` tool (the model can run a check itself where the backend allows commands).
- Automatic post-turn verification by a configured recipe inside the scheduler.
- Per-task wall-clock/token/tool-call budgets and the Phase 3 failure-cost report.
- Codec/container inspection beyond what the browser decodes (for example ffprobe-specific assertions).

## Execution Notes

**Commit:** one commit on `feat/gui-m10` carrying the plan, the implementation, the tests, and the document updates (hash in `git log` for this file).

**Commands run (2026-11-09):**

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -j 1 -- -D warnings` | clean |
| `cargo test --workspace` | 124 test binaries: **2421 passed, 0 failed, 17 ignored** |
| `node scripts/verify/selftest.mjs` | 13 assertions, `selftest ok (web + video)` |
| `cargo test -p agent-config verify` | 9 passed |
| `cargo test -p agent --test verify_recipes` | 5 passed |
| `cargo test -p agent-loop convergence` | 9 passed |
| `cargo test -p agent-projection metrics` | 16 passed |
| `cargo test -p agent --lib metrics` | 8 passed |

**Deviations**

1. `config::verify_recipes()` takes no `workspace_root`; the plan sketched `verify_recipes(workspace_root: &str)`. The recipe read never consults the workspace, and a parameter would imply a layer choice this command must not have. The workspace's `[verify]` is refused at parse time, which `apps/agent/tests/verify_recipes.rs` exercises through `run`.
2. The video check passes on **a visible sampled pixel**, not on "more than one distinct pixel value" as Task 4 Step 1 says. A title card or an opening fade is legitimately uniform, and the selftest's recorded fixture is solid-colour frames; requiring two colours would have failed a real video and taught the operator to weaken the check. Duration, positive dimensions, and an explicit non-blank frame are still required.
3. The convergence ledger is folded through `convergence::note_result` rather than inline in `default.rs`: the loop file is at its 7737-line ratchet (now 7725 lines).
4. `web.mjs` / `video.mjs` report a failed check by throwing to the top-level handler (exit code 1) instead of calling `process.exit(1)` inside the `try`, so the `finally` block still closes the browser. Exit codes are unchanged: 1 for a check that ran and failed, 2 for arguments that did not run. Without this, a model looping on a failing check left a browser process behind per attempt.
5. The metrics text line omits the `last change <path> <digest12>` clause until a change manifest records a digest; the plan's sample line shows it unconditionally. JSON omits the two fields when absent, as the plan states.

**Deferred:** unchanged from the list above.

