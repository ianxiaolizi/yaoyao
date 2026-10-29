#Requires -Version 5.1
<#
.SYNOPSIS
    Runs the repository's local gate: format, clippy and the whole test suite.

.DESCRIPTION
    Why a script instead of three cargo commands: on a machine where the
    repository root carries a Low integrity label, every executable built under
    it inherits that label. A Low process may not open the NUL device, and some
    integration tests hand NUL to a child as stdin. The resulting
    `PermissionDenied` is an environment fact, not a product defect, and it
    must not be reported as one -- nor hidden by skipping the test.

    The script:
      1. builds every test target (`cargo test --workspace --no-run`);
      2. raises the built test binaries to Medium integrity with icacls, the
         same step scripts/gui/start.ps1 performs for agent.exe;
      3. points TEMP/TMP at %TEMP%\yaoyao-test-tmp, created here and labelled
         Low so the Low test processes may write to it. It is outside the
         repository on purpose: one test uses a temp workspace as "a directory
         that is not a git repository", and a temp root inside the checkout
         would make that test see the checkout's repository instead;
      4. runs `cargo test --workspace`.

    Run scripts\preflight.ps1 first when a failure looks environmental.

.PARAMETER SkipFormat
    Skip `cargo fmt --all -- --check`.

.PARAMETER SkipClippy
    Skip `cargo clippy`.

.PARAMETER SkipTest
    Skip the test suite (useful to rebuild and raise integrity only).

.PARAMETER Jobs
    Cargo parallelism for clippy. Defaults to 1: this class of machine
    occasionally crashes clippy with 0xc0000409 at full parallelism, and a gate
    that fails for the machine reasons teaches readers to ignore it.

.EXAMPLE
    powershell -File scripts\test.ps1

.EXAMPLE
    powershell -File scripts\test.ps1 -SkipClippy
#>
[CmdletBinding()]
param(
    [switch]$SkipFormat,
    [switch]$SkipClippy,
    [switch]$SkipTest,
    [int]$Jobs = 1
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$depsDir = Join-Path $root 'target\debug\deps'
$examplesDir = Join-Path $root 'target\debug\examples'
$tempDir = Join-Path $env:TEMP 'yaoyao-test-tmp'

function Invoke-Cargo {
    param([string[]]$Arguments)
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        Write-Host "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

# A binary that already carries Medium (or higher) is left alone: writing a new
# label on every file costs an icacls spawn per binary. The check reads the
# label text, which Windows prints in English regardless of the UI language.
function Raise-ToMedium {
    param([string]$Path)
    $labels = & icacls.exe $Path 2>$null
    if ($LASTEXITCODE -ne 0) { return $false }
    if (($labels -join "`n") -notmatch 'Low Mandatory Level') { return $false }
    & icacls.exe $Path /setintegritylevel Medium | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "icacls could not raise $Path to Medium integrity"
    }
    return $true
}

Push-Location $root
$savedTemp = $env:TEMP
$savedTmp = $env:TMP
try {
    if (-not $SkipFormat) {
        Write-Host 'gate: cargo fmt --all -- --check'
        Invoke-Cargo @('fmt', '--all', '--', '--check')
    }

    if (-not $SkipClippy) {
        Write-Host "gate: cargo clippy --workspace --all-targets -j $Jobs -- -D warnings"
        Invoke-Cargo @('clippy', '--workspace', '--all-targets', '-j', "$Jobs", '--', '-D', 'warnings')
    }

    if (-not $SkipTest) {
        Write-Host 'gate: cargo test --workspace --no-run'
        Invoke-Cargo @('test', '--workspace', '--no-run')

        $raised = 0
        $targets = @()
        if (Test-Path -LiteralPath $depsDir) {
            $targets += Get-ChildItem -LiteralPath $depsDir -Filter '*.exe' -File
        }
        if (Test-Path -LiteralPath $examplesDir) {
            $targets += Get-ChildItem -LiteralPath $examplesDir -Filter '*.exe' -File
        }
        $agentExe = Join-Path $root 'target\debug\agent.exe'
        if (Test-Path -LiteralPath $agentExe) {
            $targets += Get-Item -LiteralPath $agentExe
        }
        foreach ($file in $targets) {
            if (Raise-ToMedium -Path $file.FullName) { $raised++ }
        }
        Write-Host "gate: $raised test binaries raised to Medium integrity"

        # The test processes may run at Low integrity, and a Low process may
        # only write to a directory labelled Low (or lower). The directory is
        # here rather than under target\ because it must not sit inside a git
        # repository; the label is set once and left alone afterwards.
        New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
        $labels = & icacls.exe $tempDir 2>$null
        if (($labels -join "`n") -notmatch 'Low Mandatory Level') {
            & icacls.exe $tempDir /setintegritylevel Low | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "icacls could not label $tempDir Low; test processes below Medium cannot write to it"
            }
        }
        $env:TEMP = $tempDir
        $env:TMP = $tempDir
        Write-Host "gate: TEMP/TMP = $tempDir"

        Write-Host 'gate: cargo test --workspace'
        Invoke-Cargo @('test', '--workspace')
    }

    Write-Host 'gate: all requested steps passed' -ForegroundColor Green
} finally {
    $env:TEMP = $savedTemp
    $env:TMP = $savedTmp
    Pop-Location
}
