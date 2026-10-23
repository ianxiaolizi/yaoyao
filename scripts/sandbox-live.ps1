#Requires -Version 5.1
<#
.SYNOPSIS
    Runs the AppContainer live suites, in release by default.

.DESCRIPTION
    The live suites start real confined processes and are `#[ignore]`d, so the
    ordinary `cargo test` run never reaches them. They cover the launcher
    (spawn, stdin/stdout, environment, kernel refusals), the file tools driven
    through the container's worker, and the git tools under the container.

    Release is the default because a whole class of launcher defect exists only
    there. `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` keeps the pointer it is handed
    rather than copying what it points at; the handle and job arrays were once
    declared in a block that ended before `CreateProcessW`, so an optimized
    build reused those stack slots and every spawn failed with
    `ERROR_INVALID_HANDLE` while the debug build passed. Only a release run of
    these suites sees that. The debug profile is one switch away for the
    quicker loop: -DebugBuild.

    The machine must have run `agent sandbox setup` from an elevated prompt.
    This script checks that with `sandbox status` and stops with the setup's
    own instruction when it is missing, rather than letting each test discover
    it in its own way.

.PARAMETER DebugBuild
    Run the suites in the debug profile instead of release. Named this way
    because `-Debug` is a common parameter of every advanced script, and
    declaring it again is rejected before the script body runs.

.PARAMETER Jobs
    Cargo build parallelism. Defaults to 2: instrumented and release linking of
    the whole workspace at full width exhausts Windows' commit limit on this
    class of machine.

.EXAMPLE
    pwsh scripts/sandbox-live.ps1

.EXAMPLE
    pwsh scripts/sandbox-live.ps1 -DebugBuild -Jobs 1
#>
[CmdletBinding()]
param(
    [switch]$DebugBuild,
    [int]$Jobs = 2
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$profileName = if ($DebugBuild) { 'debug' } else { 'release' }

# `[string[]]`, not the value of an `if` expression: PowerShell collapses a
# one-element array returned from `if` into a string, and splatting a string
# hands its characters to cargo as separate arguments -- cargo then reports
# `unexpected argument '-' found`.
[string[]]$profileArgs = @()
if (-not $DebugBuild) {
    $profileArgs = @('--release')
}

Push-Location $root
try {
    Write-Host "sandbox live: $profileName profile (cargo jobs: $Jobs)"

    # `sandbox status` exits non-zero when the null-device grant is missing, and
    # prints what to run. Its exit code is the precondition this script shares
    # with the tests.
    #
    # `Continue` around the native calls: PowerShell 5.1 turns a native
    # program's stderr into an error record, and under `Stop` that would end
    # the script on a line that merely wrote a warning. Exit codes are checked
    # by hand instead.
    $ErrorActionPreference = 'Continue'
    & cargo run -q -p agent -- sandbox status
    $status = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($status -ne 0) {
        throw 'the AppContainer setup is not in place; see the message above'
    }

    # The child processes are the ones that need the variable: the tests read it
    # to decide whether to skip.
    $previous = $env:AGENT_APPCONTAINER_TEST
    $env:AGENT_APPCONTAINER_TEST = '1'
    $gate = 0
    try {
        $ErrorActionPreference = 'Continue'
        Write-Host 'sandbox live: launcher and backend'
        & cargo test @profileArgs -j $Jobs -p agent-sandbox-win --test appcontainer_live -- --ignored
        $gate = $LASTEXITCODE

        if ($gate -eq 0) {
            Write-Host 'sandbox live: file and git tools through the binary'
            & cargo test @profileArgs -j $Jobs -p agent --test cli_end_to_end -- --ignored
            $gate = $LASTEXITCODE
        }
        $ErrorActionPreference = 'Stop'
    } finally {
        $env:AGENT_APPCONTAINER_TEST = $previous
    }
} finally {
    Pop-Location
}

if ($gate -ne 0) {
    exit $gate
}

Write-Host "sandbox live: passed in $profileName"
