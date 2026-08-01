<#
.SYNOPSIS
    Install jdups: the tray readout and the headless sampler.

.DESCRIPTION
    Copies both binaries, creates the log directory, and registers two
    scheduled tasks:

      jdups-tray     at logon, as you, limited rights. The notification icon.
      jdups-sampler  the logger, which is why history survives a logoff.

    With -Agent, a third task runs the shutdown agent in dry run. See that
    parameter: it cannot shut anything down, and is not a replacement for
    PowerChute.

    Nothing jdups does at runtime needs administrator rights -- it only reads
    HID feature reports. Elevation is for the *install shape*, not the program:
    Program Files, a shared log directory that a SYSTEM writer can be trusted
    with, and a task that runs before anyone signs in.

    Use -PerUser to skip all of that. See that parameter for what it costs.

    This installs the *readout only*. There is no shutdown agent yet, so
    PowerChute must stay installed and armed -- losing unattended shutdown
    without noticing is the one failure this project is trying not to cause.

.PARAMETER PerUser
    Install entirely inside your profile, with no elevation and no UAC prompt.

    Nothing about the readout needs administrator rights. What needs them is
    only the machine-wide half: writing to Program Files, locking down a shared
    log directory, and registering a task that runs as SYSTEM.

    The one real cost is the sampler. Under -PerUser it runs at *logon* rather
    than at startup, so the log gains a gap whenever nobody is signed in. For
    "how bad is my power, is the battery dying" that is usually fine -- the
    machine is on when you care. It is not the same guarantee, and the runtime
    decay series is the one that suffers, so it is called out rather than
    buried.

.PARAMETER Agent
    Also register the shutdown agent, in dry run.

    The agent watches the UPS, decides whether this machine should shut down,
    and writes what it *would* have done to jdups-agent-YYYY-MM.log. It cannot
    act: this build refuses to start with armed = true, because the shutdown
    transaction is not written yet.

    The point of running it is that thresholds picked on a bench are guesses.
    Weeks of decisions against your own power are not. Leave it running, read
    the log after the next outage, and tune jdups.conf before anything is ever
    allowed to act on it.

    PowerChute stays armed. This changes nothing about that.

.PARAMETER SamplerOnly
    Register only the sampler. Useful on a machine nobody logs into.

.PARAMETER TrayOnly
    Register only the tray. No log will be written. Needs no elevation at all,
    with or without -PerUser.

.EXAMPLE
    .\install.ps1 -PerUser
.EXAMPLE
    .\install.ps1
.EXAMPLE
    .\install.ps1 -Interval 60
#>
[CmdletBinding()]
param(
    [string]$InstallDir = "",
    [string]$LogDir     = "",
    [int]$Interval      = 300,
    [string]$Serial     = "",
    [switch]$PerUser,
    [switch]$Agent,
    [switch]$SamplerOnly,
    [switch]$TrayOnly,
    # Carried across the UAC boundary; see the elevation block.
    [string]$TrayUser   = ""
)

$ErrorActionPreference = "Stop"
$TrayTask    = "jdups-tray"
$SamplerTask = "jdups-sampler"
$AgentTask   = "jdups-agent"

if ($SamplerOnly -and $TrayOnly) {
    throw "-SamplerOnly and -TrayOnly are mutually exclusive."
}
if ($Agent -and $TrayOnly) {
    throw "-Agent and -TrayOnly are mutually exclusive."
}
if ($Interval -lt 10 -or $Interval -gt 3600) {
    throw "-Interval must be between 10 and 3600 seconds."
}

# -TrayOnly touches nothing outside the profile, so it never needs elevation
# whatever else was asked for.
if ($TrayOnly) { $PerUser = $true }

if (-not $InstallDir) {
    $InstallDir = if ($PerUser) { "$env:LOCALAPPDATA\Programs\jdups" } else { "$env:ProgramFiles\jdups" }
}
if (-not $LogDir) {
    $LogDir = if ($PerUser) { "$env:LOCALAPPDATA\jdups" } else { "$env:ProgramData\jdups" }
}

# --- Self-elevate, unless we were asked not to -------------------------------
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin -and -not $PerUser) {
    Write-Host "This installs machine-wide, which needs administrator rights:"
    Write-Host "  - Program Files, a locked-down log directory, and a SYSTEM task"
    Write-Host "    so the log keeps running when nobody is signed in."
    Write-Host ""
    Write-Host "  For a no-admin install inside your profile:  .\install.ps1 -PerUser"
    Write-Host "  (the tray is identical; the log gains gaps while nobody is logged in)"
    Write-Host ""
    Write-Host "Elevating (accept the UAC prompt)..."
    # Every switch has to be reconstructed here. One that is not named is
    # silently dropped when the script relaunches, and the install then quietly
    # does something other than what was asked. This bit jdrgb once.
    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($PSBoundParameters.ContainsKey("InstallDir")) { $argList += @("-InstallDir", "`"$InstallDir`"") }
    if ($PSBoundParameters.ContainsKey("LogDir"))     { $argList += @("-LogDir", "`"$LogDir`"") }
    if ($PSBoundParameters.ContainsKey("Interval"))   { $argList += @("-Interval", $Interval) }
    if ($Serial)      { $argList += @("-Serial", $Serial) }
    if ($Agent)       { $argList += "-Agent" }
    if ($SamplerOnly) { $argList += "-SamplerOnly" }
    if ($TrayOnly)    { $argList += "-TrayOnly" }

    # The *invoking* user's SID, captured before elevating. Afterwards the
    # process is whoever answered UAC, which on a machine where the desktop user
    # is not an administrator is the wrong account -- and the tray would be
    # registered for someone who never sees it.
    $sid = ([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value
    $argList += @("-TrayUser", $sid)

    Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList
    return
}

# --- Installing from here ----------------------------------------------------
$failed = $false
function Fail([string]$msg) { Write-Host "  FAILED: $msg" -ForegroundColor Red; $script:failed = $true }

Write-Host ("jdups install ({0})" -f $(if ($PerUser) { "per-user, no elevation" } else { "machine-wide" }))
Write-Host "  binaries -> $InstallDir"
Write-Host "  log      -> $LogDir"

# --- Locate the binaries -----------------------------------------------------
$root = Split-Path -Parent $PSCommandPath
$src = @{}
$wanted = @("jdups.exe", "jdups-tray.exe")
if ($Agent) { $wanted += "jdups-agent.exe" }
foreach ($exe in $wanted) {
    $candidates = @(
        (Join-Path $root $exe),
        (Join-Path $root "target\release\$exe")
    )
    $found = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $found) {
        throw "Could not find $exe. Run ``cargo build --release`` first."
    }
    $src[$exe] = $found
}

# --- Copy --------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
foreach ($exe in $src.Keys) {
    try {
        # A running tray holds its own image open.
        Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($exe)) -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -eq (Join-Path $InstallDir $exe) } |
            Stop-Process -Force
        Copy-Item $src[$exe] (Join-Path $InstallDir $exe) -Force
        Write-Host "  copied $exe"
    } catch { Fail "copying ${exe}: $_" }
}

# --- The log directory is a privileged write target --------------------------
# The sampler runs as SYSTEM. A SYSTEM process appending to a path a normal
# user can influence is an elevation-of-privilege bug, so the directory is
# created here, by an elevated process, with inheritance off and an explicit
# ACL -- and a reparse point found in the way is refused rather than followed.
if ($PerUser) {
    # Inside the profile there is no privilege boundary to defend: the writer
    # and the reader are the same account, and the directory already inherits
    # an ACL that excludes everyone else.
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    Write-Host "  created $LogDir"
} else {
if (Test-Path $LogDir) {
    $item = Get-Item $LogDir -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "$LogDir is a reparse point. Refusing to let a SYSTEM writer follow it."
    }
} else {
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
}

try {
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)   # inheritance off, inherited rules dropped
    $rights = [System.Security.AccessControl.FileSystemRights]
    $inherit = [System.Security.AccessControl.InheritanceFlags]"ContainerInherit,ObjectInherit"
    $prop = [System.Security.AccessControl.PropagationFlags]::None
    $allow = [System.Security.AccessControl.AccessControlType]::Allow

    foreach ($who in @("NT AUTHORITY\SYSTEM", "BUILTIN\Administrators")) {
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            $who, $rights::FullControl, $inherit, $prop, $allow)))
    }
    # Users read the log; they must not be able to alter what SYSTEM appends to.
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        "BUILTIN\Users", $rights::ReadAndExecute, $inherit, $prop, $allow)))
    $acl.SetOwner((New-Object System.Security.Principal.NTAccount("BUILTIN\Administrators")))
    Set-Acl -Path $LogDir -AclObject $acl
    Write-Host "  locked down $LogDir (SYSTEM/Admins write, Users read)"
} catch { Fail "setting the ACL on ${LogDir}: $_" }
}

# --- Scheduled tasks ---------------------------------------------------------
# Task Scheduler defaults DisallowStartIfOnBatteries and StopIfGoingOnBatteries
# to TRUE. Harmless today, because Windows does not see this UPS as a battery --
# and immediately fatal if anyone ever rebinds the inbox HID battery driver,
# which is a documented option in the plan. A UPS monitor that refuses to run on
# battery is worth one explicit line to prevent.
function New-Settings {
    New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -ExecutionTimeLimit ([TimeSpan]::Zero) `
        -RestartCount 3 `
        -RestartInterval (New-TimeSpan -Minutes 1) `
        -MultipleInstances IgnoreNew `
        -StartWhenAvailable
}

if (-not $TrayOnly) {
    try {
        $args = "--sample --interval $Interval --dir `"$LogDir`""
        if ($Serial) { $args += " --serial $Serial" }
        $action = New-ScheduledTaskAction -Execute (Join-Path $InstallDir "jdups.exe") -Argument $args
        if ($PerUser) {
            # As you, at logon. No elevation, and no stored credentials -- which
            # a "run whether logged on or not" task would require, and which is
            # a worse trade than an honest gap in the data.
            $me = ([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value
            $principal = New-ScheduledTaskPrincipal -UserId $me -LogonType Interactive -RunLevel Limited
            $trigger = New-ScheduledTaskTrigger -AtLogOn -User $me
            $desc = "at logon, as you"
        } else {
            $principal = New-ScheduledTaskPrincipal -UserId "NT AUTHORITY\SYSTEM" `
                -LogonType ServiceAccount -RunLevel Highest
            $trigger = New-ScheduledTaskTrigger -AtStartup
            $desc = "SYSTEM, at startup"
        }
        Register-ScheduledTask -TaskName $SamplerTask -Action $action `
            -Trigger $trigger -Principal $principal -Settings (New-Settings) -Force | Out-Null
        Start-ScheduledTask -TaskName $SamplerTask
        Write-Host "  registered $SamplerTask ($desc, every ${Interval}s)"
    } catch { Fail "registering ${SamplerTask}: $_" }
}

if ($Agent) {
    try {
        # The settings file lives beside the binary, not in the log directory:
        # under a machine-wide install that puts it in Program Files, which is
        # Administrators-write, so the file a SYSTEM agent trusts is not one an
        # ordinary user can edit. Written only if absent -- an upgrade must
        # never silently replace thresholds somebody chose.
        $conf = Join-Path $InstallDir "jdups.conf"
        if (Test-Path $conf) {
            Write-Host "  kept existing jdups.conf"
        } else {
            & (Join-Path $InstallDir "jdups-agent.exe") --print-config |
                Set-Content -Path $conf -Encoding UTF8
            Write-Host "  wrote $conf (all defaults, all commented out)"
        }

        $args = "-q --dir `"$LogDir`""
        if ($Serial) { $args += " --serial $Serial" }
        $action = New-ScheduledTaskAction -Execute (Join-Path $InstallDir "jdups-agent.exe") -Argument $args
        if ($PerUser) {
            $me = ([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value
            $principal = New-ScheduledTaskPrincipal -UserId $me -LogonType Interactive -RunLevel Limited
            $trigger = New-ScheduledTaskTrigger -AtLogOn -User $me
            $desc = "at logon, as you"
        } else {
            $principal = New-ScheduledTaskPrincipal -UserId "NT AUTHORITY\SYSTEM" `
                -LogonType ServiceAccount -RunLevel Highest
            $trigger = New-ScheduledTaskTrigger -AtStartup
            $desc = "SYSTEM, at startup"
        }
        Register-ScheduledTask -TaskName $AgentTask -Action $action `
            -Trigger $trigger -Principal $principal -Settings (New-Settings) -Force | Out-Null
        Start-ScheduledTask -TaskName $AgentTask
        Write-Host "  registered $AgentTask ($desc, DRY RUN)"
    } catch { Fail "registering ${AgentTask}: $_" }
}

if (-not $SamplerOnly) {
    try {
        if (-not $TrayUser) {
            $TrayUser = ([Security.Principal.WindowsIdentity]::GetCurrent()).User.Value
        }
        $action = New-ScheduledTaskAction -Execute (Join-Path $InstallDir "jdups-tray.exe")
        $principal = New-ScheduledTaskPrincipal -UserId $TrayUser -LogonType Interactive -RunLevel Limited
        Register-ScheduledTask -TaskName $TrayTask -Action $action `
            -Trigger (New-ScheduledTaskTrigger -AtLogOn -User $TrayUser) `
            -Principal $principal -Settings (New-Settings) -Force | Out-Null
        # Started now so the icon appears without a logoff.
        Start-ScheduledTask -TaskName $TrayTask
        $who = (New-Object System.Security.Principal.SecurityIdentifier($TrayUser)).Translate(
            [System.Security.Principal.NTAccount]).Value
        Write-Host "  registered $TrayTask (at logon, as $who)"
    } catch { Fail "registering ${TrayTask}: $_" }
}

Write-Host ""
if ($failed) {
    Write-Host "Finished with errors." -ForegroundColor Red
    Write-Host "Press Enter to close."; [void][Console]::ReadLine()
    exit 1
}

Write-Host "Done." -ForegroundColor Green
Write-Host ""
Write-Host "  The tray icon may be hidden: Windows 11 puts new notification icons"
Write-Host "  behind the chevron. Drag it onto the taskbar to pin it."
Write-Host ""
Write-Host "  Log: $LogDir"
if ($PerUser -and -not $TrayOnly) {
    Write-Host ""
    Write-Host "  The sampler runs at logon, so the log has a gap whenever nobody is"
    Write-Host "  signed in. Re-run without -PerUser for continuous history."
}
if ($Agent) {
    Write-Host ""
    Write-Host "  The agent is in DRY RUN. It decides and logs; it cannot act."
    Write-Host "  Thresholds:  $InstallDir\jdups.conf"
    Write-Host "  Its log:     $LogDir\jdups-agent-YYYY-MM.log"
    Write-Host "  Check them:  jdups-agent.exe --check"
}
Write-Host ""
Write-Host "  PowerChute must stay installed and armed. jdups reads; it does not"
Write-Host "  shut anything down, and nothing else will if you remove PowerChute."
Write-Host ""
Write-Host "Press Enter to close."; [void][Console]::ReadLine()
