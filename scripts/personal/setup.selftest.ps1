$ErrorActionPreference = 'Stop'
$temp = Join-Path $env:TEMP ("yaoyao-setup-test-" + [guid]::NewGuid())
$workspace = Join-Path $temp 'workspace'
$localAppData = Join-Path $temp 'localappdata'
$fakeAgent = Join-Path $temp 'agent.cmd'
$launcher = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\gui\start.local.ps1'))
$launcherExisted = Test-Path -LiteralPath $launcher
$savedLocalAppData = $env:LOCALAPPDATA
New-Item -ItemType Directory -Force -Path $workspace,$localAppData | Out-Null
try {
    $out = & "$PSScriptRoot\setup.ps1" -DryRun -Mode coding -Workspace $workspace 2>&1
    if ($LASTEXITCODE -ne 0) { throw "setup dry-run failed: $($out -join "`n")" }
    $text = $out -join "`n"
    if ($text -notmatch 'AGENT_SANDBOX=app-container') { throw 'setup did not select AppContainer' }
    if ($text -match [regex]::Escape((Join-Path $workspace '.agent'))) { throw 'setup put state inside the workspace' }
    if ($text -notmatch 'AGENT_USER_CONFIG=') { throw 'setup did not name the personal config' }

    $fakeText = @(
        '@echo off',
        'if not "%~1"=="--profile" goto wrong',
        'if not "%~2"=="coding" goto wrong',
        'if not "%~3"=="doctor" goto wrong',
        'echo fake doctor passed',
        'exit /b 0',
        ':wrong',
        'echo fake agent expected --profile coding doctor 1>&2',
        'exit /b 2'
    ) -join "`r`n"
    [IO.File]::WriteAllText($fakeAgent, $fakeText + "`r`n", (New-Object System.Text.UTF8Encoding($false)))
    $env:LOCALAPPDATA = $localAppData
    $out = & "$PSScriptRoot\setup.ps1" -SkipBuild -Mode coding -Workspace $workspace -AgentExe $fakeAgent 2>&1
    if ($LASTEXITCODE -ne 0) { throw "setup preflight failed: $($out -join "`n")" }
    Write-Host 'PASS'
}
finally {
    $env:LOCALAPPDATA = $savedLocalAppData
    if (-not $launcherExisted -and (Test-Path -LiteralPath $launcher)) {
        Remove-Item -LiteralPath $launcher -Force
    }
    $resolved = [IO.Path]::GetFullPath($temp)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolved)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}