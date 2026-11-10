# Personal Agent Launch Readiness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make 爻爻 usable every day by one trusted Windows user without turning it into a public multi-tenant service.

**Architecture:** Keep the existing event-sourced kernel, local Web GUI, capability policy, approval ledger, and AppContainer backend. Add a personal bootstrap/profile layer around them, make AppContainer the standard command backend, add an explicit broad-internet opt-in for trusted personal use, and add reproducible eval/package/backup scripts. No account, billing, tenant, public bind, or remote sandbox work is included.

**Tech Stack:** Rust 1.99, PowerShell 5.1+, Windows AppContainer, existing `agent-config` / `agent-kernel` / `agent-web`, pnpm + Vite for the embedded UI.

**Spec:** `G:\yaoyao-public\PROJECT.md`, `G:\yaoyao-public\CONTRACT.md`, `G:\yaoyao-public\SECURITY.md`, `G:\yaoyao-public\README.md`

## Global Constraints

- Target one trusted Windows 10/11 user and one local workspace at a time.
- `agent web` stays bound to `127.0.0.1` with the existing one-time bearer token; no public listener, TLS, login, tenant, quota, or billing work.
- `shell.run` must run only under `app-container`; `local` continues to refuse high-risk actions.
- `AGENT_SANDBOX_NETWORK=internet` is an explicit, broad `internetClient` opt-in and is never the default. The old `allow` spelling keeps failing closed.
- Agent state must remain outside the model-writable workspace.
- Existing event logs, configs, CLI behavior, and approval semantics stay backward compatible unless a task explicitly changes them.
- Never place API keys in logs, event payloads, model context, committed files, or test output.
- Every task must leave a working command or testable artifact; no task may end with only a prose claim.

## Review Focus

- Fresh machine without AppContainer setup: setup must name the exact repair and must not silently fall back to `local`.
- User selects broad internet: doctor and launcher must say “broad, not site-allowlisted” rather than implying an origin allowlist.
- Missing or invalid model credentials: setup/eval must fail before a task session is created and must not print the key.
- Existing data directory or old config: start/setup must refuse unsafe in-workspace state and preserve readable old sessions.
- A tool child or untrusted web page tries to approve work: existing approval-authority tests must continue to pass; new personal scripts must not add a bypass.

## File Structure

- Create `G:\yaoyao-public\scripts\personal\setup.ps1`: first-run checks and personal config creation.
- Create `G:\yaoyao-public\scripts\personal\setup.selftest.ps1`: dry-run assertions for the setup script.
- Create `G:\yaoyao-public\examples\personal\config.toml`: personal safe/coding/coding-online/research profiles.
- Create `G:\yaoyao-public\crates\agent-config\tests\personal_config.rs`: template parsing and resolution tests.
- Modify `G:\yaoyao-public\scripts\gui\start.local.ps1.example`: remove the invalid in-workspace data-dir example.
- Modify `G:\yaoyao-public\scripts\gui\start.ps1`: pass the selected profile to `agent web`, display the selected network policy, and preserve explicit environment values.
- Modify `G:\yaoyao-public\apps\agent\src\sandbox.rs`: add `internet` as an explicit broad network mode.
- Modify `G:\yaoyao-public\apps\agent\src\stack.rs`: refuse `internet` outside AppContainer.
- Modify `G:\yaoyao-public\apps\agent\src\checks.rs`: report broad internet honestly in doctor.
- Create `G:\yaoyao-public\scripts\personal\eval.ps1`: run the existing ignored kernel eval against a real endpoint.
- Create `G:\yaoyao-public\scripts\personal\eval.selftest.ps1`: dry-run assertions for eval configuration.
- Create `G:\yaoyao-public\scripts\personal\package.ps1`: build a portable personal release folder.
- Create `G:\yaoyao-public\scripts\personal\package.selftest.ps1`: dry-run and packaged-help smoke checks.
- Create `G:\yaoyao-public\scripts\personal\backup.ps1`: snapshot the personal data directory.
- Create `G:\yaoyao-public\scripts\personal\diagnose.ps1`: create a redacted support bundle.
- Modify `G:\yaoyao-public\README.md`, `G:\yaoyao-public\PROJECT.md`, `G:\yaoyao-public\SECURITY.md`, and `G:\yaoyao-public\CONTRACT.md`: document personal mode and the new network semantics.
- Modify `G:\yaoyao-public\.github\workflows\ci.yml`: run the four personal PowerShell self-tests.

---

### Task 1: Personal config profiles

**Files:**
- Create: `G:\yaoyao-public\examples\personal\config.toml`
- Create: `G:\yaoyao-public\crates\agent-config\tests\personal_config.rs`
- Modify: `G:\yaoyao-public\README.md`

**Interfaces:**
- Consumes: `agent_config::UserConfig::from_toml_str`, `agent_config::Layers`, `agent_config::resolve`.
- Produces: one user-layer file with `safe`, `coding`, `coding-online`, and `research` profiles; `coding` asks before shell, `coding-online` pairs the read-only `web.request` grant with the host's explicit broad `internetClient` mode, and `research` asks before network.

- [ ] **Step 1: Write the failing template test**

Create `crates/agent-config/tests/personal_config.rs`:

```rust
use agent_config::{Layers, UserConfig, resolve};

const PERSONAL: &str = include_str!("../../../examples/personal/config.toml");

fn resolved(profile: &str) -> agent_config::ResolvedProfile {
    let user = UserConfig::from_toml_str(PERSONAL).expect("personal config parses");
    resolve(Layers { user, profile_override: Some(profile.to_string()), ..Layers::default() })
        .expect("personal profile resolves")
}

#[test]
fn personal_profiles_resolve_with_narrow_defaults() {
    let safe = resolved("safe");
    assert!(!safe.capability_set.contains(&agent_protocol::permissions::Capability::new("process.spawn")));

    let coding = resolved("coding");
    assert!(coding.capability_set.contains(&agent_protocol::permissions::Capability::new("process.spawn")));
    assert_eq!(coding.policy.overrides.iter().find(|d| d.capability.as_str() == "process.spawn").unwrap().decision, agent_protocol::permissions::PolicyDecision::Ask);

    let research = resolved("research");
    assert!(research.capability_set.contains(&agent_protocol::permissions::Capability::new("network.request")));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p agent-config --test personal_config`

Expected: FAIL because `examples/personal/config.toml` does not exist.

- [ ] **Step 3: Add the personal template**

Create `examples/personal/config.toml` with a capability ceiling that contains `fs.read`, `fs.write`, `git.status`, `memory.read`, `memory.write`, `process.spawn`, and `network.request`; then define:

- `safe`: no `process.spawn`, no `network.request`
- `coding`: `fs.*`, `git.status`, `memory.*`, `process.spawn`; `shell = "ask"`, `network = "deny"`
- `coding-online`: same as `coding`, but `network = "allow"` for the read-only `web.request` tool; the host may separately select `AGENT_SANDBOX_NETWORK=internet` for broad child-process egress
- `research`: `fs.read`, `memory.read`, `network.request`; `network = "ask"`, no process

Use `[profiles.<name>.tools] allow = [...]` and `[profiles.<name>.policy]` exactly as the existing config tests do.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p agent-config --test personal_config`

Expected: PASS.

- [ ] **Step 5: Document profile selection**

Add a README section showing:

```powershell
$env:AGENT_USER_CONFIG = "$env:LOCALAPPDATA\yaoyao-agent\personal-config.toml"
$env:AGENT_SANDBOX = 'app-container'
cargo run -p agent -- --profile coding ask "hello"
```

State in the same section that `coding-online` pairs the profile with `AGENT_SANDBOX_NETWORK=internet`; that is broad child-process egress, not an origin allowlist. Use `coding` with `deny` when package-manager egress is not needed.

- [ ] **Step 6: Commit**

```powershell
git add examples/personal/config.toml crates/agent-config/tests/personal_config.rs README.md
git commit -m "feat: add personal agent profiles"
```

### Task 2: One-command personal setup

**Files:**
- Create: `G:\yaoyao-public\scripts\personal\setup.ps1`
- Create: `G:\yaoyao-public\scripts\personal\setup.selftest.ps1`
- Modify: `G:\yaoyao-public\scripts\gui\start.local.ps1.example:15`
- Modify: `G:\yaoyao-public\scripts\gui\start.ps1:49-54`
- Test: `G:\yaoyao-public\scripts\personal\setup.selftest.ps1`

**Interfaces:**
- Consumes: `agent_config::UserConfig::from_toml_str`, `AGENT_MODEL_PROVIDER`, `AGENT_MODEL_BASE_URL`, `AGENT_MODEL_NAME`, `AGENT_MODEL_API_KEY`, `scripts\sandbox-boot-task.ps1`, and `target\debug\agent.exe`.
- Produces: `setup.ps1 -DryRun -Mode <safe|coding|coding-online|research> -Workspace <path>`; exit `0` when the plan is valid, `2` when prerequisites are missing, and `1` when the setup cannot be written.

- [ ] **Step 1: Write the failing setup test**

Create `scripts/personal/setup.selftest.ps1` with this shape:

```powershell
$ErrorActionPreference = 'Stop'
$temp = Join-Path $env:TEMP ("yaoyao-setup-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $temp | Out-Null
$out = & "$PSScriptRoot\setup.ps1" -DryRun -Mode coding -Workspace $temp 2>&1
if ($LASTEXITCODE -ne 0) { throw "setup dry-run failed: $($out -join "`n")" }
$text = $out -join "`n"
if ($text -notmatch 'AGENT_SANDBOX=app-container') { throw 'setup did not select AppContainer' }
if ($text -match [regex]::Escape((Join-Path $temp '.agent'))) { throw 'setup put state inside the workspace' }
if ($text -notmatch 'AGENT_USER_CONFIG=') { throw 'setup did not name the personal config' }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\setup.selftest.ps1`

Expected: FAIL because `scripts\personal\setup.ps1` does not exist.

- [ ] **Step 3: Implement the setup script**

Create `setup.ps1` with:

```powershell
[CmdletBinding()]
param(
    [switch]$DryRun,
    [switch]$SkipBuild,
    [string]$Workspace = 'G:\yaoyao-work',
    [ValidateSet('safe', 'coding', 'coding-online', 'research')]
    [string]$Mode = 'coding'
)
```

The script must:
- resolve `$repo` from `$PSScriptRoot\..\..`;
- reject a workspace that does not exist;
- create `%LOCALAPPDATA%\yaoyao-agent` and copy `examples\personal\config.toml` there only when absent;
- write `scripts\gui\start.local.ps1` only when absent, with `AGENT_USER_CONFIG` set to the personal config outside the workspace, `AGENT_PROFILE` set to the selected mode, `AGENT_SANDBOX='app-container'` for `coding`, `coding-online`, and `research`, and `AGENT_SANDBOX_NETWORK='internet'` only for `coding-online`;
- never write `AGENT_DATA_DIR` inside `$Workspace`;
- if `AGENT_MODEL_PROVIDER` is `openai`, refuse with exit `2` unless `AGENT_MODEL_BASE_URL`, `AGENT_MODEL_NAME`, and `AGENT_MODEL_API_KEY` are all non-empty;
- in `-DryRun`, print the would-be config path, workspace, mode, sandbox, network, and model provider without writing anything;
- otherwise build unless `-SkipBuild`, run `agent doctor`, and print the exact elevated repair command if AppContainer is not ready.

- [ ] **Step 4: Run the setup test to verify it passes**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\setup.selftest.ps1`

Expected: PASS with output containing `AGENT_SANDBOX=app-container`.

- [ ] **Step 5: Fix the stale example and launcher display**

Change `scripts\gui\start.local.ps1.example` so it no longer suggests `AGENT_DATA_DIR = 'G:\yaoyao-work\.agent'`; leave data unset and document that an explicit value must be absolute and outside the workspace. In `scripts\gui\start.ps1`, read `AGENT_PROFILE`, pass `--profile <name>` before the `web` subcommand, print the selected network value next to the sandbox value, and show a warning when it is `internet`.

- [ ] **Step 6: Commit**

```powershell
git add scripts/personal/setup.ps1 scripts/personal/setup.selftest.ps1 scripts/gui/start.ps1 scripts/gui/start.local.ps1.example
git commit -m "feat: add personal setup bootstrap"
```

### Task 3: Explicit AppContainer internet mode

**Files:**
- Modify: `G:\yaoyao-public\apps\agent\src\sandbox.rs:82-125`
- Modify: `G:\yaoyao-public\apps\agent\src\stack.rs:551-590`
- Modify: `G:\yaoyao-public\apps\agent\src\checks.rs:485-575`
- Modify: `G:\yaoyao-public\scripts\gui\start.ps1:49-54`
- Modify: `G:\yaoyao-public\SECURITY.md`
- Modify: `G:\yaoyao-public\CONTRACT.md`
- Test: `G:\yaoyao-public\apps\agent\tests\cli_end_to_end.rs`

**Interfaces:**
- Consumes: `AppContainerLauncher::new(..., network)` and `Capabilities::internet_client()`.
- Produces: `sandbox::network(Some("internet")) -> Ok(true)`; `sandbox::network(Some("allow"))` remains `Err(SandboxError::NetworkNotBoundable)`; `sandbox::network_label(true)` returns a broad-internet description; non-AppContainer modes with `network_allowed == true` are rejected by `stack::environment_for`.

- [ ] **Step 1: Write the failing network tests**

Add to the `#[cfg(test)]` module in `apps/agent/src/sandbox.rs`:

```rust
#[test]
fn internet_is_explicit_while_allow_stays_refused() {
    assert!(network(Some("internet")).unwrap());
    assert!(network(Some(" deny ")).unwrap() == false);
    assert!(matches!(network(Some("allow")), Err(SandboxError::NetworkNotBoundable)));
    assert!(network_label(true).contains("broad"));
}
```

Add to `apps/agent/tests/cli_end_to_end.rs`:

```rust
#[test]
fn internet_is_refused_without_app_container() {
    let (workspace, data) = dirs();
    let result = ran_with(&data, &workspace, &[
        ("AGENT_SANDBOX", "local"),
        ("AGENT_SANDBOX_NETWORK", "internet"),
    ], &["doctor"]);
    assert_eq!(result.code, 1);
    assert!(result.stderr.contains("app-container"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p agent --test cli_end_to_end internet_is_refused_without_app_container`

Expected: FAIL because `internet` is still an unknown network policy.

- [ ] **Step 3: Implement the explicit broad mode**

In `apps/agent/src/sandbox.rs`, add the `internet` branch to `network`, keep `allow` rejected, and make `network_label(true)` return `internet (broad; AppContainer internetClient; not site-allowlisted)`.

In `apps/agent/src/stack.rs`, add a `StackError` variant such as:

```rust
#[error("AGENT_SANDBOX_NETWORK=internet requires AGENT_SANDBOX=app-container; {mode} cannot bound a command's traffic")]
NetworkRequiresAppContainer { mode: &'static str },
```

Return it from `environment_for` before constructing `LocalExecutionEnvironment` when `network_allowed` is true and `mode != SandboxMode::WorkspaceSandbox`.

In `apps/agent/src/checks.rs`, use the new `network_label` in the `WorkspaceSandbox` doctor message. In `scripts/gui/start.ps1`, print `网络` and warn that `internet` is broad.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run: `cargo test -p agent --test cli_end_to_end internet_is_refused_without_app_container`

Expected: PASS.

Run: `cargo test -p agent --lib sandbox::tests::internet_is_explicit_while_allow_stays_refused`

Expected: PASS.

- [ ] **Step 5: Document the tradeoff**

Add to `SECURITY.md` and `CONTRACT.md`: `AGENT_SANDBOX_NETWORK=internet` enables Windows `internetClient` for child processes; it is broad egress, not an origin allowlist; it is for a trusted personal machine and must remain opt-in. Do not describe it as equivalent to `web.request`’s allowlist.

- [ ] **Step 6: Commit**

```powershell
git add apps/agent/src/sandbox.rs apps/agent/src/stack.rs apps/agent/src/checks.rs apps/agent/tests/cli_end_to_end.rs scripts/gui/start.ps1 SECURITY.md CONTRACT.md
git commit -m "feat: add explicit app-container internet mode"
```

### Task 4: Real-model evaluation gate

**Files:**
- Create: `G:\yaoyao-public\scripts\personal\eval.ps1`
- Create: `G:\yaoyao-public\scripts\personal\eval.selftest.ps1`
- Modify: `G:\yaoyao-public\README.md`
- Modify: `G:\yaoyao-public\PROJECT.md`

**Interfaces:**
- Consumes: `AGENT_MODEL_PROVIDER=openai`, `AGENT_MODEL_BASE_URL`, `AGENT_MODEL_NAME`, `AGENT_MODEL_API_KEY`, and the existing `apps/agent/tests/kernel_eval.rs`.
- Produces: `eval.ps1 -Runs 3 -DryRun`; a report path under `%LOCALAPPDATA%\yaoyao-agent\eval\`; exit `2` when the model environment is incomplete; no key in stdout or the report.

- [ ] **Step 1: Write the failing eval test**

Create `scripts/personal/eval.selftest.ps1` with:

```powershell
$env:AGENT_MODEL_PROVIDER = 'openai'
$env:AGENT_MODEL_BASE_URL = 'https://example.invalid'
$env:AGENT_MODEL_NAME = 'test-model'
$env:AGENT_MODEL_API_KEY = 'test-key'
$out = & "$PSScriptRoot\eval.ps1" -DryRun -Runs 3 2>&1
if ($LASTEXITCODE -ne 0) { throw "eval dry-run failed: $($out -join "`n")" }
$text = $out -join "`n"
if ($text -match 'test-key') { throw 'eval leaked the API key' }
if ($text -notmatch 'KERNEL_EVAL_RUNS=3') { throw 'eval did not set the run count' }
if ($text -notmatch 'kernel_eval') { throw 'eval did not name the test target' }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\eval.selftest.ps1`

Expected: FAIL because `eval.ps1` does not exist.

- [ ] **Step 3: Implement the evaluation script**

Create `eval.ps1` with parameters:

```powershell
param(
    [int]$Runs = 3,
    [string]$Tasks = '',
    [string]$Report = '',
    [switch]$DryRun
)
```

It must require `AGENT_MODEL_PROVIDER=openai` and all three model variables; refuse `echo`; set `KERNEL_EVAL_RUNS`, optionally `KERNEL_EVAL_TASKS`, and `KERNEL_EVAL_REPORT`; run `cargo test -p agent --test kernel_eval -- --ignored --nocapture`; copy the report to the personal eval directory; and print only pass/fail plus the report path.

- [ ] **Step 4: Run the eval self-test to verify it passes**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\eval.selftest.ps1`

Expected: PASS.

- [ ] **Step 5: Run the real evaluation manually**

With the real key set, run:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\personal\eval.ps1 -Runs 3
```

Expected: a report exists and contains per-task pass/fail plus aggregate results. Do not claim the personal build is ready if this command was not run.

- [ ] **Step 6: Commit**

```powershell
git add scripts/personal/eval.ps1 scripts/personal/eval.selftest.ps1 README.md PROJECT.md
git commit -m "test: add personal real-model evaluation gate"
```

### Task 5: Portable personal package

**Files:**
- Create: `G:\yaoyao-public\scripts\personal\package.ps1`
- Create: `G:\yaoyao-public\scripts\personal\package.selftest.ps1`
- Modify: `G:\yaoyao-public\README.md`

**Interfaces:**
- Consumes: `cargo build --release -p agent`, `pnpm --dir web build`, and the embedded `web/dist`.
- Produces: `scripts\personal\dist\yaoyao-personal\agent.exe`, `启动爻爻.cmd`, `config.example.toml`, and `README.txt`; `package.ps1 -DryRun` lists the same file manifest without building.

- [ ] **Step 1: Write the failing package test**

Create `scripts/personal/package.selftest.ps1`:

```powershell
$out = & "$PSScriptRoot\package.ps1" -DryRun 2>&1
if ($LASTEXITCODE -ne 0) { throw "package dry-run failed: $($out -join "`n")" }
$text = $out -join "`n"
foreach ($name in @('agent.exe', '启动爻爻.cmd', 'config.example.toml', 'README.txt')) {
    if ($text -notmatch [regex]::Escape($name)) { throw "package omitted $name" }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\package.selftest.ps1`

Expected: FAIL because `package.ps1` does not exist.

- [ ] **Step 3: Implement the package script**

Create `package.ps1` with `-DryRun` and `-OutputRoot`. It must:
- build the web bundle when `web/src` is newer than `web/dist`;
- build `agent.exe` in release mode;
- copy `agent.exe`, `web/dist` (for reference only; it is embedded), `examples/personal/config.toml` as `config.example.toml`, a generated `启动爻爻.cmd`, and `README.txt`;
- make `启动爻爻.cmd` call `agent.exe web --port 7441` without cargo or pnpm;
- never include the developer's `start.local.ps1`, API key, event logs, or `.agent-data`;
- print a manifest and the output directory.

- [ ] **Step 4: Run the self-test to verify it passes**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\package.selftest.ps1`

Expected: PASS.

- [ ] **Step 5: Smoke-test the packaged binary**

Run the package without `-DryRun`, then:

```powershell
& .\scripts\personal\dist\yaoyao-personal\agent.exe --help
```

Expected: exit `0` and list the existing commands. Do not start a server in this smoke step.

- [ ] **Step 6: Commit**

```powershell
git add scripts/personal/package.ps1 scripts/personal/package.selftest.ps1 README.md
git commit -m "feat: build portable personal package"
```

### Task 6: Backup and support bundle

**Files:**
- Create: `G:\yaoyao-public\scripts\personal\backup.ps1`
- Create: `G:\yaoyao-public\scripts\personal\diagnose.ps1`
- Create: `G:\yaoyao-public\scripts\personal\backup.selftest.ps1`
- Modify: `G:\yaoyao-public\README.md`

**Interfaces:**
- Consumes: `AGENT_DATA_DIR` when set, otherwise `%LOCALAPPDATA%\yaoyao-agent`; `agent doctor`; `agent metrics`; `agent export`.
- Produces: a timestamped zip under `%LOCALAPPDATA%\yaoyao-agent\backups`, and a redacted diagnostics folder with `doctor.txt`, `metrics.txt`, `manifest.json`, and no `AGENT_MODEL_API_KEY` value.

- [ ] **Step 1: Write the failing backup test**

Create `scripts/personal/backup.selftest.ps1`:

```powershell
$temp = Join-Path $env:TEMP ("yaoyao-backup-test-" + [guid]::NewGuid())
$dest = Join-Path $temp 'backups'
New-Item -ItemType Directory -Force -Path $temp | Out-Null
Set-Content -LiteralPath (Join-Path $temp 'events.jsonl') -Value '{}' -Encoding UTF8
$out = & "$PSScriptRoot\backup.ps1" -DataDir $temp -Destination $dest -DryRun 2>&1
if ($LASTEXITCODE -ne 0) { throw "backup dry-run failed: $($out -join "`n")" }
if (($out -join "`n") -notmatch 'events.jsonl') { throw 'backup did not list the event log' }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\backup.selftest.ps1`

Expected: FAIL because `backup.ps1` does not exist.

- [ ] **Step 3: Implement backup and diagnostics**

`backup.ps1` must accept `-DataDir`, `-Destination`, and `-DryRun`; create a timestamped `.zip`; include a manifest with file count and SHA-256 for each file; refuse a destination inside the source; and never delete the source.

`diagnose.ps1` must run `agent doctor`, `agent metrics`, and `agent export` for the latest session when available; write to a temporary folder; scrub any environment-variable value whose name matches `*KEY*`, `*TOKEN*`, or `*SECRET*`; and zip the result.

- [ ] **Step 4: Run the backup test to verify it passes**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\backup.selftest.ps1`

Expected: PASS.

- [ ] **Step 5: Run the real diagnostics manually**

Run: `powershell -ExecutionPolicy Bypass -File scripts\personal\diagnose.ps1 -DryRun` and then without `-DryRun`.

Expected: a support bundle path is printed and the bundle contains no API key value.

- [ ] **Step 6: Commit**

```powershell
git add scripts/personal/backup.ps1 scripts/personal/diagnose.ps1 scripts/personal/backup.selftest.ps1 README.md
git commit -m "feat: add personal backup and diagnostics"
```

### Task 7: Personal acceptance run and documentation

**Files:**
- Modify: `G:\yaoyao-public\README.md`
- Modify: `G:\yaoyao-public\PROJECT.md`
- Modify: `G:\yaoyao-public\SECURITY.md`
- Modify: `G:\yaoyao-public\.github\workflows\ci.yml`

**Interfaces:**
- Consumes: all previous tasks.
- Produces: one documented personal quickstart and a CI job that runs `setup.selftest.ps1`, `eval.selftest.ps1`, `package.selftest.ps1`, and `backup.selftest.ps1`.

- [ ] **Step 1: Write the CI check**

Add a Windows CI step:

```powershell
pwsh -File scripts/personal/setup.selftest.ps1
pwsh -File scripts/personal/eval.selftest.ps1
pwsh -File scripts/personal/package.selftest.ps1
pwsh -File scripts/personal/backup.selftest.ps1
```

- [ ] **Step 2: Run the CI check locally**

Run: `powershell -ExecutionPolicy Bypass -File scripts\test.ps1`

Expected: existing Rust gate passes; then run each self-test and expect exit `0`.

- [ ] **Step 3: Run the personal acceptance script manually**

Use a fresh workspace and real model:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\personal\setup.ps1 -Workspace G:\yaoyao-work -Mode coding
& G:\yaoyao-public\target\debug\agent.exe doctor
powershell -ExecutionPolicy Bypass -File scripts\personal\eval.ps1 -Runs 3
```

Then complete one real task: ask 爻爻 to create a file, run a test, fix a failure, and report the diff. Confirm the approval card appears, the AppContainer mode is visible, and the session resumes after a restart.

- [ ] **Step 4: Update the docs with the exact acceptance boundary**

In `README.md` and `PROJECT.md`, state:
- personal mode is Windows-only;
- `coding` requires AppContainer for shell;
- `internet` is broad and opt-in;
- `coding-online` pairs `web.request` with the explicit broad `internetClient` mode; it is not an origin allowlist;
- the real-model eval must be run before calling the build ready;
- the public/multi-tenant path is out of scope.

- [ ] **Step 5: Run the final verification**

Run:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\test.ps1
node scripts\gui\smoke.mjs
```

Expected: both exit `0`; the GUI smoke has `0 FAIL`; the personal setup, eval dry-run, package dry-run, and backup dry-run self-tests pass.

- [ ] **Step 6: Commit**

```powershell
git add README.md PROJECT.md SECURITY.md .github/workflows/ci.yml
git commit -m "docs: record personal agent acceptance boundary"
```

---

## Deferred Work

These are intentionally not in this personal-use plan:

- Command-level safe allowlists such as `cargo test` or `pnpm build`. The personal MVP accepts per-command approval; a command matcher must reject shell chaining and separators before it can be added.
- Windows Credential Manager or DPAPI storage for the model key. The personal MVP keeps the existing environment-variable flow and keeps the key out of logs.
- Attachment upload, voice input, cross-device sync, auto-update, and installer signing.
- Public web auth, multi-tenant isolation, quotas, billing, abuse prevention, remote sandboxing, and SLA.

## Acceptance Gate

The personal build is ready for daily use only when all of the following are true:

- A fresh machine can run `setup.ps1`, see a clear AppContainer repair path, and complete a first task from the GUI.
- `AGENT_SANDBOX=app-container` is the normal command backend; `local` never runs `shell.run`.
- `AGENT_SANDBOX_NETWORK=deny` is the default; `internet` is explicitly labeled broad and never silently replaces deny.
- The real OpenAI-compatible endpoint eval ran with at least 3 repetitions and produced a saved report.
- A real task created a file, ran a check, survived a restart, and resumed an approval without duplicating a tool call.
- Backup and diagnostics produce usable artifacts without exposing the API key.
- The existing permission red-team suite still passes.
