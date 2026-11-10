[CmdletBinding()]
param(
    [string]$DataDir = '',
    [string]$Destination = '',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

function Stop-Backup([string]$Message, [int]$Code) {
    Write-Output $Message
    exit $Code
}

if (-not $DataDir) {
    $DataDir = if ($env:AGENT_DATA_DIR) { $env:AGENT_DATA_DIR } elseif ($env:LOCALAPPDATA) {
        Join-Path $env:LOCALAPPDATA 'yaoyao-agent'
    } else {
        ''
    }
}
if (-not $DataDir) { Stop-Backup 'backup: AGENT_DATA_DIR is unset and LOCALAPPDATA is unavailable' 2 }
if (-not (Test-Path -LiteralPath $DataDir -PathType Container)) {
    Stop-Backup "backup: data directory does not exist: $DataDir" 2
}

$sourceRoot = [IO.Path]::GetFullPath($DataDir).TrimEnd('\')
$sourcePrefix = $sourceRoot + '\'
if (-not $Destination) { $Destination = Join-Path $sourceRoot 'backups' }
$destinationRoot = [IO.Path]::GetFullPath($Destination)
$destinationPrefix = $destinationRoot.TrimEnd('\') + '\'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmssfff'
$zipPath = Join-Path $destinationRoot "yaoyao-backup-$stamp.zip"

$files = Get-ChildItem -LiteralPath $sourceRoot -Recurse -File | Where-Object {
    -not $_.FullName.StartsWith($destinationPrefix, [StringComparison]::OrdinalIgnoreCase)
}
if ($DryRun) {
    Write-Output 'backup: dry-run'
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($sourceRoot.Length).TrimStart('\')
        Write-Output "  $relative"
    }
    Write-Output "file_count=$($files.Count)"
    Write-Output "backup=$zipPath"
    exit 0
}

$staging = Join-Path ([IO.Path]::GetTempPath()) ("yaoyao-backup-stage-" + [guid]::NewGuid())
try {
    New-Item -ItemType Directory -Force -Path $staging,$destinationRoot | Out-Null
    $entries = New-Object System.Collections.Generic.List[object]
    foreach ($file in $files) {
        $relative = $file.FullName.Substring($sourceRoot.Length).TrimStart('\')
        $target = Join-Path $staging $relative
        $parent = Split-Path -Parent $target
        if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
        Copy-Item -LiteralPath $file.FullName -Destination $target -Force
        $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $entries.Add([pscustomobject]@{
            path = $relative.Replace('\', '/')
            size = $file.Length
            sha256 = $hash
        })
    }
    $manifest = [pscustomobject]@{
        created_at = (Get-Date).ToString('o')
        source = $sourceRoot
        file_count = $entries.Count
        files = $entries
        excluded_destination = $destinationRoot
    }
    [IO.File]::WriteAllText((Join-Path $staging 'manifest.json'), ($manifest | ConvertTo-Json -Depth 6), (New-Object System.Text.UTF8Encoding($false)))
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zipPath -CompressionLevel Optimal -Force
} catch {
    Stop-Backup "backup: failed: $($_.Exception.Message)" 1
} finally {
    $stagingResolved = [IO.Path]::GetFullPath($staging)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($stagingResolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $stagingResolved)) {
        Remove-Item -LiteralPath $stagingResolved -Recurse -Force
    }
}
Write-Output "file_count=$($files.Count)"
Write-Output "backup=$zipPath"