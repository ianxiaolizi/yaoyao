[CmdletBinding()]
param(
    [switch]$DryRun,
    [string]$OutputRoot = ''
)

$ErrorActionPreference = 'Stop'
if (-not $OutputRoot) { $OutputRoot = Join-Path $PSScriptRoot 'dist' }
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$outputRootPath = [IO.Path]::GetFullPath($OutputRoot)
$out = Join-Path $outputRootPath 'yaoyao-personal'
$web = Join-Path $repo 'web'
$dist = Join-Path $web 'dist'
$releaseExe = Join-Path $repo 'target\release\agent.exe'
$config = Join-Path $repo 'examples\personal\config.toml'

function Invoke-Native([string]$File, [string[]]$Arguments) {
    $savedEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $File @Arguments 2>&1 | ForEach-Object { Write-Host $_ }
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedEap
    }
}

function Show-Manifest {
    Write-Output 'package manifest:'
    foreach ($name in 'agent.exe', '启动爻爻.cmd', 'config.example.toml', 'README.txt', 'web\dist') {
        Write-Output "  $name"
    }
    Write-Output "output=$out"
}

if ($DryRun) {
    Write-Output 'package: dry-run'
    Show-Manifest
    exit 0
}

$index = Join-Path $dist 'index.html'
$stale = -not (Test-Path -LiteralPath $index -PathType Leaf)
if (-not $stale) {
    $builtAt = (Get-Item -LiteralPath $index).LastWriteTime
    $newestSource = Get-ChildItem (Join-Path $web 'src') -Recurse -File |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($newestSource -and $newestSource.LastWriteTime -gt $builtAt) { $stale = $true }
    foreach ($name in 'index.html', 'package.json', 'pnpm-lock.yaml', 'vite.config.ts', 'tsconfig.json') {
        $candidate = Join-Path $web $name
        if ((Test-Path -LiteralPath $candidate) -and (Get-Item -LiteralPath $candidate).LastWriteTime -gt $builtAt) {
            $stale = $true
        }
    }
}

if ($stale) {
    $pnpm = Get-Command pnpm -ErrorAction SilentlyContinue
    if (-not $pnpm) { throw 'package: web/dist is stale and pnpm is not installed' }
    Push-Location $web
    try {
        if (-not (Test-Path -LiteralPath (Join-Path $web 'node_modules'))) {
            if ((Invoke-Native $pnpm.Source @('install')) -ne 0) { throw 'package: pnpm install failed' }
        }
        if ((Invoke-Native $pnpm.Source @('build')) -ne 0) { throw 'package: pnpm build failed' }
    } finally {
        Pop-Location
    }
}

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) { throw 'package: cargo is not installed' }
if ((Invoke-Native $cargo.Source @('build', '--release', '-p', 'agent')) -ne 0) {
    throw 'package: cargo build --release -p agent failed'
}
if (-not (Test-Path -LiteralPath $releaseExe -PathType Leaf)) { throw "package: missing $releaseExe" }

New-Item -ItemType Directory -Force -Path $out | Out-Null
Copy-Item -LiteralPath $releaseExe -Destination (Join-Path $out 'agent.exe') -Force
Copy-Item -LiteralPath $config -Destination (Join-Path $out 'config.example.toml') -Force

$webOut = Join-Path $out 'web\dist'
if (Test-Path -LiteralPath $webOut) {
    $outPrefix = [IO.Path]::GetFullPath($out).TrimEnd('\') + '\'
    $webResolved = [IO.Path]::GetFullPath($webOut)
    if (-not $webResolved.StartsWith($outPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "package: refusing to replace a path outside $out"
    }
    Remove-Item -LiteralPath $webOut -Recurse -Force
}
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $webOut) | Out-Null
Copy-Item -LiteralPath $dist -Destination $webOut -Recurse -Force

$launcher = @('@echo off', 'setlocal', 'cd /d "%~dp0"', 'agent.exe web --port 7441') -join "`r`n"
[IO.File]::WriteAllText((Join-Path $out '启动爻爻.cmd'), $launcher + "`r`n", (New-Object System.Text.UTF8Encoding($false)))

$readmeLines = @(
    '爻爻 personal package',
    '====================',
    '',
    'Requirements',
    '- Windows 10/11',
    '- An OpenAI-compatible model endpoint',
    '- AppContainer setup for shell execution',
    '',
    'Set the model and personal configuration in the environment before launching:',
    '  AGENT_MODEL_PROVIDER=openai',
    '  AGENT_MODEL_BASE_URL=...',
    '  AGENT_MODEL_NAME=...',
    '  AGENT_MODEL_API_KEY=...',
    '  AGENT_USER_CONFIG=%LOCALAPPDATA%\yaoyao-agent\personal-config.toml',
    '  AGENT_PROFILE=coding',
    '  AGENT_SANDBOX=app-container',
    '  AGENT_SANDBOX_NETWORK=deny',
    '',
    'On a new machine, open an elevated PowerShell in this folder and run:',
    '  .\agent.exe sandbox setup',
    '',
    'Then double-click 启动爻爻.cmd. The service binds only to 127.0.0.1 and opens a',
    'one-time-token URL. config.example.toml is a copy of the personal profile template;',
    'place it outside the model-writable workspace before use.',
    '',
    'No API key, event log, .agent-data directory, developer start.local.ps1, cargo, or',
    'pnpm is included in this package.'
)
[IO.File]::WriteAllText((Join-Path $out 'README.txt'), ($readmeLines -join "`r`n") + "`r`n", (New-Object System.Text.UTF8Encoding($false)))

Show-Manifest