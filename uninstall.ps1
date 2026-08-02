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
    if (-not $task) {
        # An unelevated Get-ScheduledTask silently *omits* SYSTEM tasks -- the
        # task file is ACL'd against ordinary users -- so "no task" from it
        # proves nothing. Ask the file: access denied means the task exists and
        # is exactly the kind that needs elevation to remove; not-found means
        # it is genuinely absent. Without this, deleting the install dirs by
        # hand and re-running left both SYSTEM tasks behind while printing
        # a green Done.
        try {
            $null = [IO.File]::GetAttributes("$env:windir\System32\Tasks\$t")
            $needsAdmin = $true
        } catch [System.UnauthorizedAccessException] {
            $needsAdmin = $true
        } catch { }
    }
}
# A service always needs elevation to remove.
if (Get-Service -Name $AgentTask -ErrorAction SilentlyContinue) { $needsAdmin = $true }

if ($needsAdmin -and -not $admin -and -not $NoElevate) {
    Write-Host "A machine-wide install is present; elevating (accept the UAC prompt)..."
    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($Logs) { $argList += "-Logs" }
    try {
        Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList
    } catch {
        Write-Host ""
        Write-Host "Elevation was declined; nothing was removed." -ForegroundColor Red
        Write-Host "Press Enter to close."; [void][Console]::ReadLine()
        exit 1
    }
    return
}

$failed = $false
function Fail([string]$msg) { Write-Host "  FAILED: $msg" -ForegroundColor Red; $script:failed = $true }

Write-Host "jdups uninstall"

# --- The agent service, if it was installed that way -------------------------
# Before the tasks, because stopping it releases the singleton and the device.
try {
    $svc = Get-Service -Name $AgentTask -ErrorAction SilentlyContinue
    if ($svc) {
        if ($svc.Status -ne 'Stopped') { Stop-Service -Name $AgentTask -Force -ErrorAction SilentlyContinue }
        # sc.exe rather than Remove-Service, which is PowerShell 6+ only.
        & sc.exe delete $AgentTask | Out-Null
        Write-Host "  removed the $AgentTask service"
    }
} catch { Fail "removing the ${AgentTask} service: $_" }

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
# The tray's only window is hidden, so .NET's CloseMainWindow() finds nothing.
# Post WM_CLOSE to its window class instead: that is the clean teardown that
# removes the notification icon; a kill leaves a ghost until Explorer notices.
if (-not ("Jdups.Win32" -as [type])) {
    Add-Type -Namespace Jdups -Name Win32 -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)]
public static extern IntPtr FindWindowW(string lpClassName, string lpWindowName);
[DllImport("user32.dll")]
public static extern bool PostMessageW(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);
'@
}
$h = [Jdups.Win32]::FindWindowW("jdups_tray", $null)
if ($h -ne [IntPtr]::Zero) {
    [void][Jdups.Win32]::PostMessageW($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) # WM_CLOSE
    Start-Sleep -Milliseconds 500
}
foreach ($name in @("jdups", "jdups-tray", "jdups-agent")) {
    Get-Process -Name $name -ErrorAction SilentlyContinue |
        Where-Object { $paths -contains $_.Path } |
        ForEach-Object {
            try {
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

# --- The tray's toast identity ------------------------------------------------
# Registered at first run so notifications carry a name and header icon; without
# this it outlives the uninstall as an orphan pointing at a deleted exe.
try {
    Remove-Item "HKCU:\Software\Classes\AppUserModelId\UPS Status" -Recurse -Force -ErrorAction SilentlyContinue
} catch { }

# --- Logs, only if asked -----------------------------------------------------
foreach ($d in ($MachineDirs[1], $UserDirs[1])) {
    if (-not (Test-Path $d)) { continue }
    if ($Logs) {
        try {
            Remove-Item $d -Recurse -Force
            Write-Host "  removed $d"
        } catch { Fail "removing ${d}: $_" }
    } else {
        $note = if (Test-Path (Join-Path $d "jdups.conf")) { ", jdups.conf" } else { "" }
        Write-Host "  kept $d (logs$note and settings; pass -Logs to delete it)"
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
