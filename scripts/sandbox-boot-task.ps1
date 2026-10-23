#Requires -Version 5.1
<#
.SYNOPSIS
    Installs, reports, or removes the logon task that re-applies the
    AppContainer host preparation.

.DESCRIPTION
    The Windows kernel resets \Device\Null's security descriptor at every
    boot, so the grant `agent sandbox setup` installs is gone after a restart.
    This script registers a task that runs setup again whenever the selected
    user logs on, with highest privileges and without a UAC prompt.

    The task runs a stable copy of the binary under
    %ProgramData%\yaoyao\bin, not target\debug: a `cargo clean` would
    otherwise leave the machine unready after the next boot. Re-run -Install
    after updating the binary.

    The grants setup makes are additive and idempotent, so a task that fires
    more than once changes nothing. -Install also runs the task once, so a
    machine that is already logged on does not have to wait for the next
    logon.

    Install and Remove need an elevated prompt; Status does not.

.PARAMETER Install
    Copy the binary, restrict its ACLs, register the task, and run it once.

.PARAMETER Status
    Report whether the task is registered, its last result, and whether the
    null-device grant is currently in place.

.PARAMETER Remove
    Unregister the task and delete the stable binary directory.

.PARAMETER Source
    The agent.exe to install. Defaults to target\debug\agent.exe under the
    repository root.

.PARAMETER User
    The account whose logon runs the task. Defaults to the current user.

.PARAMETER Readonly
    Directories setup grants read+execute. No extra directories are granted
    unless this parameter is supplied; every path must exist when the task runs.

.EXAMPLE
    powershell -File scripts\sandbox-boot-task.ps1 -Install

.EXAMPLE
    powershell -File scripts\sandbox-boot-task.ps1 -Status
#>
param(
    [switch]$Install,
    [switch]$Remove,
    [switch]$Status,
    [string]$Source,
    [string]$User = [Security.Principal.WindowsIdentity]::GetCurrent().Name,
    [string[]]$Readonly = @()
)

$ErrorActionPreference = 'Stop'

$taskName = 'yaoyao-sandbox-setup'
$installDir = Join-Path $env:ProgramData 'yaoyao\bin'
$installed = Join-Path $installDir 'agent.exe'

function Test-Elevated {
    $principal = New-Object Security.Principal.WindowsPrincipal(
        [Security.Principal.WindowsIdentity]::GetCurrent())
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Show-Status {
    $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($null -eq $task) {
        Write-Host "task: not registered"
        $registered = $false
    } else {
        $info = Get-ScheduledTaskInfo -TaskName $taskName
        Write-Host ("task: registered; state={0}; last_run={1}; last_result=0x{2:X8}" -f `
            $task.State, $info.LastRunTime, $info.LastTaskResult)
        $registered = $true
    }

    if (Test-Path -LiteralPath $installed) {
        Write-Host "binary: $installed"
        & $installed sandbox status | Out-Host
        $ready = ($LASTEXITCODE -eq 0)
    } else {
        Write-Host "binary: missing ($installed)"
        $ready = $false
    }

    if ($registered -and $ready) { return 0 }
    return 2
}

function Invoke-Install {
    if (-not (Test-Elevated)) {
        throw 'run -Install from an elevated prompt'
    }
    if (-not $Source) {
        $Source = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\debug\agent.exe'
    }
    if (-not (Test-Path -LiteralPath $Source)) {
        throw "no binary at $Source; build it first"
    }
    foreach ($path in $Readonly) {
        if (-not (Test-Path -LiteralPath $path)) {
            throw "the read-only path $path does not exist"
        }
    }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item -LiteralPath $Source -Destination $installed -Force
    # The task runs with highest privileges; a standard user must not be able
    # to replace the binary it executes. SIDs rather than names so the grant
    # does not depend on the display language.
    & icacls.exe $installDir /inheritance:r `
        /grant:r '*S-1-5-18:(OI)(CI)F' `
        '*S-1-5-32-544:(OI)(CI)F' `
        '*S-1-5-32-545:(OI)(CI)RX' | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "icacls failed on $installDir"
    }

    $arguments = 'sandbox setup'
    foreach ($path in $Readonly) {
        $arguments += ' --readonly "' + $path + '"'
    }
    $action = New-ScheduledTaskAction -Execute $installed -Argument $arguments
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $User
    $principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable `
        -ExecutionTimeLimit (New-TimeSpan -Minutes 5) `
        -MultipleInstances IgnoreNew
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings -Force | Out-Null

    Write-Host "installed: task '$taskName' runs at logon of $User"
    Write-Host "binary:   $installed"
    Write-Host "command:  $installed $arguments"

    Start-ScheduledTask -TaskName $taskName
    $deadline = (Get-Date).AddSeconds(30)
    do {
        Start-Sleep -Milliseconds 500
        $info = Get-ScheduledTaskInfo -TaskName $taskName
        # 0x41301 is SCHED_S_TASK_RUNNING.
        $running = ($info.LastTaskResult -eq 267009)
    } while ($running -and ((Get-Date) -lt $deadline))
    if ($info.LastTaskResult -ne 0) {
        throw ("the task ran and failed with 0x{0:X8}; run -Status for details" -f `
            $info.LastTaskResult)
    }
}

function Invoke-Remove {
    if (-not (Test-Elevated)) {
        throw 'run -Remove from an elevated prompt'
    }
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $installDir) {
        Remove-Item -LiteralPath $installDir -Recurse -Force
    }
    $parent = Split-Path -Parent $installDir
    if ((Test-Path -LiteralPath $parent) -and -not (Get-ChildItem -LiteralPath $parent -Force)) {
        Remove-Item -LiteralPath $parent -Force
    }
    Write-Host "removed: task '$taskName' and $installDir"
}

$selected = 0
if ($Install) { $selected++ }
if ($Remove) { $selected++ }
if ($Status) { $selected++ }
if ($selected -ne 1) {
    Write-Host ('usage: sandbox-boot-task.ps1 -Install | -Status | -Remove ' +
        '[-Source <agent.exe>] [-User <account>] [-Readonly <path>...]')
    exit 2
}

if ($Install) {
    Invoke-Install
    exit 0
}
if ($Remove) {
    Invoke-Remove
    exit 0
}
exit (Show-Status)
