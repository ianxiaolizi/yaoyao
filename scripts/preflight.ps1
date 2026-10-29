#Requires -Version 5.1
<#
.SYNOPSIS
    Reports whether this machine can run the repository's gate, and names the
    environment cause when it cannot.

.DESCRIPTION
    The gate in scripts\test.ps1 depends on four things the product does not
    control: a Rust toolchain with rustfmt and clippy, a writable TEMP for the
    test processes, an openable NUL device, and executables that are not
    confined below Medium integrity.

    This script checks each of them and prints OK / WARN / FAIL with the
    remediation. Warnings describe a state the gate repairs for itself (the
    test binaries' integrity label); failures stop the gate.

    Exit code is 0 when everything the gate needs is present, 1 otherwise.
    The repository root's own integrity label is reported, never changed here:
    a project script that silently rewrites ACLs hides the machine fact the
    next failure will need.

.EXAMPLE
    powershell -File scripts\preflight.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$failures = 0

function Write-Check([string]$name, [string]$state, [string]$detail) {
    $color = switch ($state) {
        'OK' { 'Green' }
        'WARN' { 'Yellow' }
        default { 'Red' }
    }
    Write-Host ('{0,-5} {1,-28} {2}' -f $state, $name, $detail) -ForegroundColor $color
    if ($state -eq 'FAIL') { $script:failures++ }
}

Write-Host "preflight: $root"
Write-Host ''

# 1. Toolchain -------------------------------------------------------------
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
$rustc = Get-Command rustc -ErrorAction SilentlyContinue
if (-not $cargo -or -not $rustc) {
    Write-Check 'toolchain' 'FAIL' 'cargo or rustc is not on PATH'
} else {
    $rustcVersion = (& rustc --version) -split ' ' | Select-Object -Index 1
    $cargoVersion = (& cargo --version) -split ' ' | Select-Object -Index 1
    Write-Check 'toolchain' 'OK' "rustc $rustcVersion, cargo $cargoVersion"

    # The workspace pins rust-version; a floating toolchain is a different gate.
    $manifest = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
    $pinned = [regex]::Match($manifest, 'rust-version\s*=\s*"([^"]+)"').Groups[1].Value
    if ($pinned) {
        $have = [version]$rustcVersion
        $want = [version]($pinned.Split('.')[0] + '.' + $pinned.Split('.')[1] + '.0')
        if ($have -lt $want) {
            Write-Check 'rust-version' 'FAIL' "rustc $rustcVersion is older than the workspace pin $pinned"
        } else {
            Write-Check 'rust-version' 'OK' "workspace pins $pinned"
        }
    } else {
        Write-Check 'rust-version' 'WARN' 'Cargo.toml has no rust-version; the gate cannot be reproducible'
    }
}

foreach ($tool in @('fmt', 'clippy')) {
    $component = Get-Command "cargo-$tool" -ErrorAction SilentlyContinue
    if ($component) {
        Write-Check "cargo-$tool" 'OK' ((& "cargo-$tool" --version) -join '')
    } else {
        Write-Check "cargo-$tool" 'FAIL' "run: rustup component add $tool"
    }
}

# 2. TEMP ------------------------------------------------------------------
# The gate points TEMP at this directory, outside the checkout (a test uses a
# temp workspace as "a directory that is not a git repository"). It must be
# writable and labelled Low, because the test processes may run at Low.
$gateTemp = Join-Path $env:TEMP 'yaoyao-test-tmp'
try {
    New-Item -ItemType Directory -Force -Path $gateTemp | Out-Null
    $probe = Join-Path $gateTemp ("preflight-{0}.tmp" -f $PID)
    Set-Content -LiteralPath $probe -Value 'ok' -ErrorAction Stop
    Remove-Item -LiteralPath $probe -ErrorAction Stop
    $labels = & icacls.exe $gateTemp 2>$null
    if (($labels -join "`n") -match 'Low Mandatory Level') {
        Write-Check 'TEMP (gate)' 'OK' "$gateTemp (Low, writable)"
    } else {
        Write-Check 'TEMP (gate)' 'WARN' "$gateTemp is not labelled Low; Low test processes cannot write to it (scripts\test.ps1 labels it)"
    }
} catch {
    Write-Check 'TEMP (gate)' 'FAIL' "$gateTemp is not writable: $($_.Exception.Message)"
}
try {
    $probe = Join-Path $env:TEMP ("yaoyao-preflight-{0}.tmp" -f $PID)
    Set-Content -LiteralPath $probe -Value 'ok' -ErrorAction Stop
    Remove-Item -LiteralPath $probe -ErrorAction Stop
    Write-Check 'TEMP (system)' 'OK' $env:TEMP
} catch {
    Write-Check 'TEMP (system)' 'WARN' "$env:TEMP is not writable from this process; scripts\test.ps1 points TEMP at %TEMP%\yaoyao-test-tmp"
}

# 3. NUL -------------------------------------------------------------------
# System.IO.File refuses device names ("not a regular file"), so open the
# device the way the tests do: CreateFileW with GENERIC_WRITE.
$nulSignature = @'
using System;
using System.Runtime.InteropServices;
public static class YaoyaoNulProbe {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr CreateFileW(string name, uint access, uint share,
        IntPtr attributes, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll")]
    public static extern bool CloseHandle(IntPtr handle);
}
'@
if (-not ('YaoyaoNulProbe' -as [type])) {
    Add-Type -TypeDefinition $nulSignature
}
$invalidHandle = [IntPtr]::new(-1)
$handle = [YaoyaoNulProbe]::CreateFileW('NUL', 0x40000000, 0x3, [IntPtr]::Zero, 3, 0x80, [IntPtr]::Zero)
if ($handle -eq $invalidHandle) {
    $code = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
    Write-Check 'NUL device' 'FAIL' "CreateFileW(NUL) failed with Win32 error $code; this process is below Medium integrity"
} else {
    [void][YaoyaoNulProbe]::CloseHandle($handle)
    Write-Check 'NUL device' 'OK' 'opens for writing'
}

# 4. Integrity labels -------------------------------------------------------
function Get-IntegrityLabel([string]$path) {
    $labels = & icacls.exe $path 2>$null
    if ($LASTEXITCODE -ne 0) { return $null }
    foreach ($level in @('Low', 'Medium', 'High', 'System')) {
        if (($labels -join "`n") -match "$level Mandatory Level") { return $level }
    }
    return $null
}

$rootLabel = Get-IntegrityLabel $root
if ($rootLabel -eq 'Low') {
    Write-Check 'repo integrity' 'WARN' 'root is Low; binaries built under it inherit Low, and plain cargo test cannot open NUL (scripts\test.ps1 raises them)'
} elseif ($rootLabel) {
    Write-Check 'repo integrity' 'OK' $rootLabel
} else {
    Write-Check 'repo integrity' 'WARN' 'label could not be read'
}

$deps = Join-Path $root 'target\debug\deps'
$sample = $null
if (Test-Path -LiteralPath $deps) {
    $sample = Get-ChildItem -LiteralPath $deps -Filter '*.exe' -File | Select-Object -First 1
}
if (-not $sample) {
    Write-Check 'test binaries' 'WARN' 'none built yet; run scripts\test.ps1 (it builds, then raises them)'
} else {
    $label = Get-IntegrityLabel $sample.FullName
    if ($label -eq 'Low') {
        Write-Check 'test binaries' 'WARN' "sample $(Split-Path -Leaf $sample.FullName) is Low; scripts\test.ps1 raises them before running"
    } else {
        Write-Check 'test binaries' 'OK' "sample $(Split-Path -Leaf $sample.FullName) is $label"
    }
}

Write-Host ''
if ($failures -gt 0) {
    Write-Host "preflight: $failures check(s) failed" -ForegroundColor Red
    exit 1
}
Write-Host 'preflight: the gate can run on this machine' -ForegroundColor Green
exit 0
