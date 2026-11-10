param(
    [int]$Runs = 3,
    [string]$Tasks = '',
    [string]$Report = '',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

function Stop-Eval([string]$message, [int]$code) {
    Write-Output $message
    exit $code
}

if ($Runs -lt 1) { Stop-Eval 'eval: -Runs must be a positive integer' 2 }
$provider = if ($env:AGENT_MODEL_PROVIDER) { $env:AGENT_MODEL_PROVIDER.Trim() } else { '' }
if ($provider -eq 'echo') { Stop-Eval 'eval: echo cannot evaluate a real model' 2 }
if ($provider -ne 'openai') { Stop-Eval 'eval: AGENT_MODEL_PROVIDER must be openai' 2 }
foreach ($name in 'AGENT_MODEL_BASE_URL', 'AGENT_MODEL_NAME', 'AGENT_MODEL_API_KEY') {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        Stop-Eval "eval: missing $name (the value is never printed)" 2
    }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Stop-Eval 'eval: cargo is required' 2 }
$sandbox = if ($env:AGENT_SANDBOX) { $env:AGENT_SANDBOX.Trim() } else { 'app-container' }
if ($sandbox -ne 'app-container') {
    Stop-Eval 'eval: requires AGENT_SANDBOX=app-container so shell checks can run under the personal backend' 2
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if ($Report) {
    $reportPath = [IO.Path]::GetFullPath($Report)
} else {
    if (-not $env:LOCALAPPDATA) { Stop-Eval 'eval: LOCALAPPDATA is not set' 2 }
    $reportPath = Join-Path $env:LOCALAPPDATA "yaoyao-agent\eval\kernel-eval-$stamp.json"
}

Write-Output 'eval: kernel evaluation'
Write-Output "KERNEL_EVAL_RUNS=$Runs"
if ($Tasks) { Write-Output "KERNEL_EVAL_TASKS=$Tasks" }
Write-Output 'KERNEL_EVAL_TARGET=cargo test -p agent --test kernel_eval -- --ignored --nocapture'
Write-Output "KERNEL_EVAL_REPORT=$reportPath"
Write-Output "AGENT_SANDBOX=$sandbox"
if ($DryRun) {
    Write-Output 'dry-run: no evaluation run and no report written'
    exit 0
}

$tempReport = Join-Path ([IO.Path]::GetTempPath()) ("yaoyao-kernel-eval-" + [guid]::NewGuid() + '.json')
$savedRuns = $env:KERNEL_EVAL_RUNS
$savedTasks = $env:KERNEL_EVAL_TASKS
$savedReport = $env:KERNEL_EVAL_REPORT
$savedSandbox = $env:AGENT_SANDBOX
try {
    $env:AGENT_SANDBOX = $sandbox
    $env:KERNEL_EVAL_RUNS = "$Runs"
    if ($Tasks) { $env:KERNEL_EVAL_TASKS = $Tasks } else { Remove-Item Env:KERNEL_EVAL_TASKS -ErrorAction SilentlyContinue }
    $env:KERNEL_EVAL_REPORT = $tempReport
    # PowerShell 5.1 turns native stderr into a terminating error when
    # ErrorActionPreference is Stop. Cargo reports progress on stderr, so keep
    # the command's own exit status authoritative while capturing both streams.
    $savedEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $cargoOutput = & cargo test -p agent --test kernel_eval -- --ignored --nocapture 2>&1
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedEap
    }
} finally {
    $env:KERNEL_EVAL_RUNS = $savedRuns
    $env:KERNEL_EVAL_TASKS = $savedTasks
    $env:KERNEL_EVAL_REPORT = $savedReport
    $env:AGENT_SANDBOX = $savedSandbox
}

if (-not (Test-Path -LiteralPath $tempReport -PathType Leaf)) {
    Write-Output "FAIL: kernel_eval did not produce a report; report=$reportPath"
    exit 1
}

try {
    $reportText = [IO.File]::ReadAllText($tempReport)
    $key = [Environment]::GetEnvironmentVariable('AGENT_MODEL_API_KEY')
    if ($key) { $reportText = $reportText.Replace($key, '[REDACTED]') }
    $parent = Split-Path -Parent $reportPath
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    [IO.File]::WriteAllText($reportPath, $reportText, (New-Object System.Text.UTF8Encoding($false)))
} catch {
    Stop-Eval "FAIL: cannot write kernel_eval report: $($_.Exception.Message)" 1
} finally {
    if (Test-Path -LiteralPath $tempReport) { Remove-Item -LiteralPath $tempReport -Force }
}
if ($code -ne 0) {
    Write-Output "FAIL: kernel_eval has failing task(s); report=$reportPath"
    exit 1
}
Write-Output "PASS: kernel_eval report=$reportPath"