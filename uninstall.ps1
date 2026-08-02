<#
.SYNOPSIS
    Remove jdups: both tasks and both binaries.

.DESCRIPTION
    Unregisters the tray and sampler tasks, stops anything still running, and
    deletes the installed binaries.

    The log is left alone. It is data you asked the thing to collect, possibly
    over months, and an uninstaller that silently deletes months of history is
    an uninstaller you cannot trust. Pass -Logs to remove it too, or just
    delete the directory yourself.

    Handles both install shapes: machine-wide and -PerUser. Elevation is only
    requested if something machine-wide is actually present.

.PARAMETER Logs
    Also delete the log directory.

.EXAMPLE
    .\uninstall.ps1
.EXAMPLE
    .\uninstall.ps1 -Logs
#>
[CmdletBinding()]
param(
    [switch]$Logs,
    [switch]$NoElevate
)

$ErrorActionPreference = "Stop"
$TrayTask    = "jdups-tray"
$SamplerTask = "jdups-sampler"
$AgentTask   = "jdups-agent"

$MachineDirs = @("$env:ProgramFiles\jdups", "$env:ProgramData\jdups")
$UserDirs    = @("$env:LOCALAPPDATA\Programs\jdups", "$env:LOCALAPPDATA\jdups")

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# Only ask for elevation if there is something we cannot otherwise touch. A
# per-user install should uninstall without a UAC prompt, the same way it went
# in.
$needsAdmin = $false
foreach ($d in $MachineDirs) { if (Test-Path $d) { $needsAdmin = $true } }
foreach ($t in @($SamplerTask, $AgentTask)) {
    $task = Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
    if ($task -and $task.Principal.UserId -match 'SYSTEM') { $needsAdmin = $true }
}

if ($needsAdmin -and -not $admin -and -not $NoElevate) {
    Write-Host "A machine-wide install is present; elevating (accept the UAC prompt)..."
    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($Logs) { $argList += "-Logs" }
    Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList
    return
}

$failed = $false
function Fail([string]$msg) { Write-Host "  FAILED: $msg" -ForegroundColor Red; $script:failed = $true }

Write-Host "jdups uninstall"

# --- Tasks -------------------------------------------------------------------
foreach ($t in @($TrayTask, $SamplerTask, $AgentTask)) {
    try {
        if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $t -Confirm:$false
            Write-Host "  unregistered $t"
        }
    } catch { Fail "unregistering ${t}: $_" }
}

# --- Running processes -------------------------------------------------------
# Stopped by full image path, never by bare name. `Stop-Process -Name jdups`
# would also kill a development build running from target\release, which is
# an unpleasant surprise in the middle of working on this.
$paths = @()
foreach ($d in ($MachineDirs + $UserDirs)) {
    $paths += (Join-Path $d "jdups.exe")
    $paths += (Join-Path $d "jdups-tray.exe")
    $paths += (Join-Path $d "jdups-agent.exe")
}
foreach ($name in @("jdups", "jdups-tray", "jdups-agent")) {
    Get-Process -Name $name -ErrorAction SilentlyContinue |
        Where-Object { $paths -contains $_.Path } |
        ForEach-Object {
            try {
                # No -Force: the tray gets a clean WM_CLOSE teardown, which is
                # what removes its notification icon. Killing it outright leaves
                # a ghost icon behind until Explorer restarts.
                $_.CloseMainWindow() | Out-Null
                Start-Sleep -Milliseconds 300
                if (-not $_.HasExited) { $_ | Stop-Process -Force }
                Write-Host "  stopped $($_.Path)"
            } catch { Fail "stopping $($_.Path): $_" }
        }
}
Start-Sleep -Milliseconds 300

# --- Binaries ----------------------------------------------------------------
foreach ($d in ($MachineDirs[0], $UserDirs[0])) {
    if (-not (Test-Path $d)) { continue }
    try {
        # jdups.conf is no longer here -- it lives with the logs, and is kept or
        # removed with them. Still listed so an upgrade from a build that put it
        # beside the binary does not strand a copy that nothing reads.
        foreach ($exe in @("jdups.exe", "jdups-tray.exe", "jdups-agent.exe", "jdups.conf")) {
            $p = Join-Path $d $exe
            if (Test-Path $p) { Remove-Item $p -Force }
        }
        # Only if we left it empty; never recursively, in case something else
        # lives there.
        if (-not (Get-ChildItem $d -Force)) {
            Remove-Item $d -Force
            Write-Host "  removed $d"
        } else {
            Write-Host "  removed binaries from $d (directory not empty, kept)"
        }
    } catch { Fail "removing binaries from ${d}: $_" }
}

# --- Logs, only if asked -----------------------------------------------------
foreach ($d in ($MachineDirs[1], $UserDirs[1])) {
    if (-not (Test-Path $d)) { continue }
    if ($Logs) {
        try {
            Remove-Item $d -Recurse -Force
            Write-Host "  removed $d"
        } catch { Fail "removing ${d}: $_" }
    } else {
        $note = if (Test-Path (Join-Path $d "jdups.conf")) { " and jdups.conf" } else { "" }
        Write-Host "  kept $d (logs$note; pass -Logs to delete it)"
    }
}

Write-Host ""
if ($failed) {
    Write-Host "Finished with errors." -ForegroundColor Red
    Write-Host "Press Enter to close."; [void][Console]::ReadLine()
    exit 1
}
Write-Host "Done." -ForegroundColor Green
Write-Host "Press Enter to close."; [void][Console]::ReadLine()
