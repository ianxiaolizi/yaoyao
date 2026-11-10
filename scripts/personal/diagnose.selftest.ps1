$ErrorActionPreference = 'Stop'
$temp = Join-Path $env:TEMP ("yaoyao-diagnose-test-" + [guid]::NewGuid())
$sessionRoot = Join-Path $temp 'workspace-hash'
$sessionDir = Join-Path $sessionRoot 'web-diagnose-test'
$output = Join-Path $temp 'diagnostics'
New-Item -ItemType Directory -Force -Path $sessionDir,$output | Out-Null
try {
    Set-Content -LiteralPath (Join-Path $sessionDir 'events.jsonl') -Value '{}' -Encoding UTF8
    $out = & "$PSScriptRoot\diagnose.ps1" -DataDir $temp -OutputRoot $output -DryRun 2>&1
    if ($LASTEXITCODE -ne 0) { throw "diagnose dry-run failed: $($out -join "`n")" }
    $text = $out -join "`n"
    if ($text -notmatch [regex]::Escape("session=web-diagnose-test")) {
        throw 'diagnose did not find a session nested under a workspace data directory'
    }
    if ($text -notmatch [regex]::Escape('command=agent export --session web-diagnose-test')) {
        throw 'diagnose did not plan an export for the latest session'
    }
    if ($text -notmatch [regex]::Escape("session_data=$sessionRoot")) {
        throw 'diagnose did not select the workspace data directory for agent commands'
    }
    Write-Host 'PASS'
}
finally {
    $resolved = [IO.Path]::GetFullPath($temp)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolved)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}