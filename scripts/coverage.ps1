#Requires -Version 5.1
<#
.SYNOPSIS
    Runs the workspace line-coverage gate.

.DESCRIPTION
    Measures line coverage with cargo-llvm-cov and exits non-zero when it falls
    below the threshold recorded in scripts/coverage-baseline.json, or below
    -Threshold when that is given.

    The build runs with two jobs: instrumented linking of the whole workspace
    at full parallelism exhausts Windows' commit limit on this class of machine
    (os error 1455), and the measurement is the same either way.

    The summary report is printed and also left in target/coverage-gate.txt;
    the HTML browsing copy is written to target/coverage-html.

    When AGENT_APPCONTAINER_TEST=1, the AppContainer live suite is measured in
    a second, merged run. Those tests start real confined processes, so almost
    all of the backend's FFI is only reachable from them; the ordinary suite
    alone cannot reach the threshold. A third run adds the end-to-end file
    tools case for the same reason: the worker environment only exists in a run
    that starts the worker inside the container. The machine must have run
    `agent sandbox setup` first. Without the variable the gate measures the
    ordinary suite only.

.PARAMETER Threshold
    Minimum line coverage percentage. Defaults to threshold_lines in
    scripts/coverage-baseline.json.

.PARAMETER Jobs
    Cargo build parallelism. Defaults to 2.

.EXAMPLE
    pwsh scripts/coverage.ps1

.EXAMPLE
    pwsh scripts/coverage.ps1 -Threshold 99.9
#>
[CmdletBinding()]
param(
    [double]$Threshold,
    [int]$Jobs = 2
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$baselinePath = Join-Path $PSScriptRoot 'coverage-baseline.json'

if (-not $PSBoundParameters.ContainsKey('Threshold')) {
    if (-not (Test-Path -LiteralPath $baselinePath)) {
        throw "no baseline at $baselinePath; pass -Threshold or record one first"
    }
    $baseline = Get-Content -Raw -LiteralPath $baselinePath | ConvertFrom-Json
    $Threshold = [double]$baseline.threshold_lines
}

Write-Host "coverage gate: line coverage must be >= $Threshold% (cargo jobs: $Jobs)"

Push-Location $root
try {
    # `--no-report` here and below: the report is generated once, after every
    # test binary that can contribute has run, so the live suite's coverage is
    # merged into the same measurement rather than replacing it.
    & cargo llvm-cov --workspace --no-report -j $Jobs
    $gate = $LASTEXITCODE

    if ($gate -eq 0 -and $env:AGENT_APPCONTAINER_TEST -eq '1') {
        Write-Host 'coverage: adding the AppContainer live suite (merged run)'
        & cargo llvm-cov --no-report -p agent-sandbox-win --test appcontainer_live `
            -j $Jobs -- --ignored
        $gate = $LASTEXITCODE
    }

    if ($gate -eq 0 -and $env:AGENT_APPCONTAINER_TEST -eq '1') {
        # The file tools under the container, end to end through the binary:
        # the worker environment and its client are only reachable from a run
        # that really starts the worker, so the ordinary suite cannot measure
        # them. The child processes inherit the profile path, which is how
        # their lines reach this report.
        Write-Host 'coverage: adding the file tools inside the container (merged run)'
        & cargo llvm-cov --no-report -p agent --test cli_end_to_end `
            -j $Jobs -- --ignored the_file_tools_answer_under_app_container
        $gate = $LASTEXITCODE
    }

    if ($gate -eq 0) {
        $summary = & cargo llvm-cov report
        $gate = $LASTEXITCODE
        $summary | Tee-Object -FilePath 'target/coverage-gate.txt'
        if ($gate -eq 0) {
            $total = $summary | Where-Object { $_ -match '^TOTAL\s' } | Select-Object -First 1
            if (-not $total) {
                throw 'the coverage report has no TOTAL line to read'
            }
            # `cargo llvm-cov report` prints one table row per file:
            #   name | regions | missed regions | region cover | functions |
            #   missed functions | executed | lines | missed lines | line cover
            # (branch columns, when present, follow the line block). The gate's
            # name, its threshold, and the baseline's `measured.lines` are all
            # line coverage, so read column 9, not the region column at 3.
            $measured = [double](($total -split '\s+')[9].TrimEnd('%'))
            if ($measured -lt $Threshold) {
                Write-Host "coverage gate failed: line coverage $measured% is below $Threshold%"
                $gate = 1
            } else {
                Write-Host "coverage gate: line coverage $measured% (threshold $Threshold%)"
            }
        }
    }

    & cargo llvm-cov report --html --output-dir 'target/coverage-html'
    if ($LASTEXITCODE -ne 0) {
        Write-Warning 'the HTML report could not be generated'
    }
} finally {
    Pop-Location
}

if ($gate -ne 0) {
    exit $gate
}

Write-Host 'coverage gate passed'
