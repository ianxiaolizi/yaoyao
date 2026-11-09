[CmdletBinding()]
param(
    [string]$DataDir = '',
    [string]$OutputRoot = '',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$exe = Join-Path $repo 'target\debug\agent.exe'
if (-not $DataDir) {
    $DataDir = if ($env:AGENT_DATA_DIR) { $env:AGENT_DATA_DIR } elseif ($env:LOCALAPPDATA) {
        Join-Path $env:LOCALAPPDATA 'yaoyao-agent'
    } else {
        ''
    }
}
if (-not $DataDir) { Write-Output 'diagnose: AGENT_DATA_DIR is unset and LOCALAPPDATA is unavailable'; exit 2 }
$dataRoot = [IO.Path]::GetFullPath($DataDir)
if (-not $OutputRoot) {
    if (-not $env:LOCALAPPDATA) { Write-Output 'diagnose: LOCALAPPDATA is unavailable'; exit 2 }
    $OutputRoot = Join-Path $env:LOCALAPPDATA 'yaoyao-agent\diagnostics'
}
$outputRootPath = [IO.Path]::GetFullPath($OutputRoot)
$stamp = Get-Date -Format 'yyyyMMdd-HHmmssfff'
$bundle = Join-Path $outputRootPath "yaoyao-diagnostics-$stamp.zip"
$latest = Get-ChildItem -LiteralPath $dataRoot -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'events.jsonl') } |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1

if ($DryRun) {
    Write-Output 'diagnose: dry-run'
    Write-Output "exe=$exe"
    Write-Output "data=$dataRoot"
    Write-Output "bundle=$bundle"
    Write-Output 'command=agent doctor'
    Write-Output 'command=agent metrics --json --out metrics.txt'
    if ($latest) { Write-Output "command=agent export --session $($latest.Name) --out export --redacted --json" }
    else { Write-Output 'export=skipped: no session log found' }
    exit 0
}
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { Write-Output "diagnose: missing $exe"; exit 2 }

function Invoke-ToFile([string[]]$Arguments, [string]$Path) {
    $savedEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $exe @Arguments 2>&1 | Out-File -LiteralPath $Path -Encoding UTF8
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedEap
    }
}

$staging = Join-Path ([IO.Path]::GetTempPath()) ("yaoyao-diagnostics-stage-" + [guid]::NewGuid())
$savedData = $env:AGENT_DATA_DIR
try {
    New-Item -ItemType Directory -Force -Path $staging,$outputRootPath | Out-Null
    $env:AGENT_DATA_DIR = $dataRoot
    $doctorExit = Invoke-ToFile @('doctor') (Join-Path $staging 'doctor.txt')
    $metricsPath = Join-Path $staging 'metrics.txt'
    $metricsExit = Invoke-ToFile @('metrics', '--json', '--out', $metricsPath) (Join-Path $staging 'metrics-command.txt')
    if (-not (Test-Path -LiteralPath $metricsPath)) {
        [IO.File]::WriteAllText($metricsPath, "metrics did not produce a document`r`n", (New-Object System.Text.UTF8Encoding($false)))
    }

    $exportExit = -1
    if ($latest) {
        $exportDir = Join-Path $staging 'export'
        $exportExit = Invoke-ToFile @('export', '--session', $latest.Name, '--out', $exportDir, '--redacted', '--json') (Join-Path $staging 'export.txt')
    } else {
        [IO.File]::WriteAllText((Join-Path $staging 'export.txt'), "no session log was available`r`n", (New-Object System.Text.UTF8Encoding($false)))
    }

    $secretNames = Get-ChildItem Env: | Where-Object {
        $_.Name -match 'KEY|TOKEN|SECRET' -and -not [string]::IsNullOrEmpty([string]$_.Value)
    } | Select-Object -ExpandProperty Name
    $secretValues = @($secretNames | ForEach-Object { [Environment]::GetEnvironmentVariable($_) } | Where-Object { $_ })
    Get-ChildItem -LiteralPath $staging -Recurse -File | ForEach-Object {
        $text = [IO.File]::ReadAllText($_.FullName)
        foreach ($value in $secretValues) { $text = $text.Replace($value, '[REDACTED]') }
        [IO.File]::WriteAllText($_.FullName, $text, (New-Object System.Text.UTF8Encoding($false)))
    }

    $files = Get-ChildItem -LiteralPath $staging -Recurse -File
    $entries = foreach ($file in $files) {
        [pscustomobject]@{
            path = $file.FullName.Substring($staging.Length).TrimStart('\').Replace('\', '/')
            size = $file.Length
            sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    $manifest = [pscustomobject]@{
        created_at = (Get-Date).ToString('o')
        latest_session = if ($latest) { $latest.Name } else { $null }
        doctor_exit = $doctorExit
        metrics_exit = $metricsExit
        export_exit = $exportExit
        file_count = @($entries).Count
        files = @($entries)
    }
    [IO.File]::WriteAllText((Join-Path $staging 'manifest.json'), ($manifest | ConvertTo-Json -Depth 6), (New-Object System.Text.UTF8Encoding($false)))
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $bundle -CompressionLevel Optimal -Force
} catch {
    Write-Output "diagnose: failed: $($_.Exception.Message)"
    exit 1
} finally {
    $env:AGENT_DATA_DIR = $savedData
    $stagingResolved = [IO.Path]::GetFullPath($staging)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($stagingResolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $stagingResolved)) {
        Remove-Item -LiteralPath $stagingResolved -Recurse -Force
    }
}
Write-Output "bundle=$bundle"