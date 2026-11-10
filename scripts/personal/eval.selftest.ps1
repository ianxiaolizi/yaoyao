$ErrorActionPreference = 'Stop'
$temp = Join-Path $env:TEMP ("yaoyao-eval-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $temp | Out-Null
$savedPath = $env:PATH
try {
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
    if ($text -notmatch 'AGENT_SANDBOX=app-container') { throw 'eval did not select the AppContainer backend' }

    $fakeCargo = Join-Path $temp 'cargo.cmd'
    Set-Content -LiteralPath $fakeCargo -Encoding ASCII -Value @(
        '@echo off',
        'echo fake cargo progress 1>&2',
        '> "%KERNEL_EVAL_REPORT%" echo {"key":"%AGENT_MODEL_API_KEY%","status":"ok"}',
        'exit /b 0'
    )
    $report = Join-Path $temp 'report.json'
    $env:PATH = "$temp;$savedPath"
    $run = & "$PSScriptRoot\eval.ps1" -Runs 3 -Report $report 2>&1
    if ($LASTEXITCODE -ne 0) { throw "eval fake run failed: $($run -join "`n")" }
    if (-not (Test-Path -LiteralPath $report)) { throw 'eval did not write the report' }
    $reportText = Get-Content -Raw -LiteralPath $report
    if ($reportText -match 'test-key') { throw 'eval leaked the API key into the report' }
    if (($run -join "`n") -match 'test-key') { throw 'eval leaked the API key to stdout' }
    Write-Host 'PASS'
}
finally {
    $env:PATH = $savedPath
    $resolved = [IO.Path]::GetFullPath($temp)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolved)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}