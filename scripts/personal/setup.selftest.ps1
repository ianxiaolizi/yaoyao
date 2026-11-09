$ErrorActionPreference = 'Stop'
$temp = Join-Path $env:TEMP ("yaoyao-setup-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $temp | Out-Null
try {
    $out = & "$PSScriptRoot\setup.ps1" -DryRun -Mode coding -Workspace $temp 2>&1
    if ($LASTEXITCODE -ne 0) { throw "setup dry-run failed: $($out -join "`n")" }
    $text = $out -join "`n"
    if ($text -notmatch 'AGENT_SANDBOX=app-container') { throw 'setup did not select AppContainer' }
    if ($text -match [regex]::Escape((Join-Path $temp '.agent'))) { throw 'setup put state inside the workspace' }
    if ($text -notmatch 'AGENT_USER_CONFIG=') { throw 'setup did not name the personal config' }
    Write-Host 'PASS'
}
finally {
    $resolved = [IO.Path]::GetFullPath($temp)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolved)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}