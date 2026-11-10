$ErrorActionPreference = 'Stop'
$temp = Join-Path $env:TEMP ("yaoyao-backup-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $temp | Out-Null
try {
    Set-Content -LiteralPath (Join-Path $temp 'events.jsonl') -Value '{}' -Encoding UTF8
    $dest = Join-Path $temp 'backups'
    $out = & "$PSScriptRoot\backup.ps1" -DataDir $temp -Destination $dest -DryRun 2>&1
    if ($LASTEXITCODE -ne 0) { throw "backup dry-run failed: $($out -join "`n")" }
    if (($out -join "`n") -notmatch 'events.jsonl') { throw 'backup did not list the event log' }

    $out = & "$PSScriptRoot\backup.ps1" -DataDir $temp -Destination $dest 2>&1
    if ($LASTEXITCODE -ne 0) { throw "backup failed: $($out -join "`n")" }
    $zip = Get-ChildItem -LiteralPath $dest -Filter '*.zip' | Select-Object -First 1
    if (-not $zip) { throw 'backup did not create a zip' }
    if (-not (Test-Path -LiteralPath (Join-Path $temp 'events.jsonl'))) { throw 'backup deleted the source' }

    $extract = Join-Path $temp 'extract'
    Expand-Archive -LiteralPath $zip.FullName -DestinationPath $extract -Force
    $manifest = Get-Content -Raw -LiteralPath (Join-Path $extract 'manifest.json') | ConvertFrom-Json
    if ($manifest.file_count -lt 1) { throw 'manifest has no files' }
    if ($manifest.files[0].sha256 -notmatch '^[0-9a-f]{64}$') { throw 'manifest has no SHA-256' }
    if (-not (Test-Path -LiteralPath (Join-Path $extract 'events.jsonl'))) { throw 'archive omitted events.jsonl' }
    Write-Host 'PASS'
}
finally {
    $resolved = [IO.Path]::GetFullPath($temp)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolved)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}