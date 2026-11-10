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
    # Compare real paths rather than raw text: the selection is a path, and a
    # host that reports it with different casing or separators still selected
    # the right directory. When it selected a different one, report what it
    # printed instead of only that something was wrong.
    $selected = @($out | Where-Object { "$_" -like 'session_data=*' })
    $actual = if ($selected.Count -eq 1) { ("$($selected[0])" -replace '^session_data=', '').Trim() } else { '' }
    $resolvedActual = if ($actual) { [IO.Path]::GetFullPath($actual).TrimEnd('\') } else { '' }
    $expected = [IO.Path]::GetFullPath($sessionRoot).TrimEnd('\')
    if (-not $resolvedActual -or -not [string]::Equals($resolvedActual, $expected, [StringComparison]::OrdinalIgnoreCase)) {
        throw "diagnose did not select the workspace data directory for agent commands: $text"
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