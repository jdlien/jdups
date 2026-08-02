<#
.SYNOPSIS
    Install jdups: the tray readout and the headless sampler.

.DESCRIPTION
    Copies both binaries, creates the log directory, and registers two
    scheduled tasks:

      jdups-tray     at logon, as you, limited rights. The notification icon.
      jdups-sampler  the logger, which is why history survives a logoff.

    With -Agent, the shutdown agent is installed too -- as a scheduled task, or
    as a Windows service with -Service. It starts in dry run unless jdups.conf
    says armed = true.

    Nothing jdups does at runtime needs administrator rights -- it only reads
    HID feature reports. Elevation is for the *install shape*, not the program:
    Program Files, a shared log directory that a SYSTEM writer can be trusted
    with, and a task that runs before anyone signs in.

    Use -PerUser to skip all of that. See that parameter for what it costs.

    Keep PowerChute installed and disarmed ("Do not shut down in the event of a
    power outage") before arming jdups: both write the same UPS countdown
    register and the last writer wins.

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

    The agent watches the UPS, warns before it acts, and shuts the machine down
    cleanly. It starts in **dry run** -- armed = false is the default and what a
    missing config means -- so it decides and logs and touches nothing until
    jdups.conf says otherwise.

    Run it that way first. Thresholds picked on a bench are guesses; ones picked
    from a week of your own power are not.

.PARAMETER Service
    Install the agent as a Windows service rather than a scheduled task.
    Requires -Agent, and cannot be combined with -PerUser.

    This does *not* buy running with no user logged in -- the SYSTEM task
    already did that. What it buys:

      - Power notifications, the only way to be told the machine suspended or
        resumed. Nothing runs during S3, so an agent cannot watch a UPS while
        the machine sleeps; what it can do is find out on waking and act. See
        shutdown_on_wake in jdups.conf.
      - SERVICE_CONTROL_PRESHUTDOWN, which arrives early enough in a reboot to
        stand down cleanly. A scheduled task is simply killed.
      - A restart-on-failure policy the SCM enforces.

    Installing this removes the scheduled-task form, so the two cannot fight
    over the agent's singleton.

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
    [switch]$Service,
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
if ($Service -and -not $Agent) {
    throw "-Service applies to the agent; pass -Agent as well."
}
if ($Service -and $PerUser) {
    # A service runs as SYSTEM by definition, so there is no per-user shape of
    # it, and pretending otherwise would quietly install something else.
    throw "-Service cannot be combined with -PerUser."
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
    if ($Service)     { $argList += "-Service" }
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
# Everything currently running is stopped *first*, before anything is copied or
# any task is restarted. Doing it per-binary meant the old tray was still alive
# and watching while its neighbours were killed out from under it -- and a tray
# that briefly loses the device and finds it again looks exactly like a power
# event to the code that watches for one. An upgrade should not be able to
# generate notifications about itself.
#
# Stopped by full image path, never by bare name: `Stop-Process -Name jdups`
# would also kill a development build running out of target\release, which is an
# unpleasant surprise in the middle of working on this.
$running = @()
foreach ($exe in $src.Keys) {
    $running += Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($exe)) -ErrorAction SilentlyContinue |
                Where-Object { $_.Path -eq (Join-Path $InstallDir $exe) }
}
foreach ($p in $running) {
    try {
        # The tray gets a WM_CLOSE first, which is what removes its notification
        # icon. Killing it outright leaves a ghost behind until Explorer
        # restarts, which uninstall.ps1 already knew and this did not.
        $p.CloseMainWindow() | Out-Null
        Start-Sleep -Milliseconds 300
        if (-not $p.HasExited) { $p | Stop-Process -Force }
        Write-Host "  stopped $(Split-Path -Leaf $p.Path)"
    } catch { Fail "stopping $($p.Path): $_" }
}
if ($running) { Start-Sleep -Milliseconds 400 }

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
foreach ($exe in $src.Keys) {
    try {
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
        # The settings file goes in the *log* directory, not beside the binary.
        # Program Files is for files an installer writes once and nothing edits
        # afterwards; this one is meant to be edited, and %ProgramData% is where
        # machine-wide mutable configuration belongs.
        #
        # It is only as safe as the ACL applied above, and that is not a
        # throwaway remark: %ProgramData% inherits permissions that let ordinary
        # users create files, and a SYSTEM agent reading thresholds from a
        # user-writable path is shutdown-as-a-service. The block above strips
        # inheritance and grants Users read only, which is what makes this
        # directory a legitimate home for it.
        #
        # Written only if absent -- an upgrade must never silently replace
        # thresholds somebody chose.
        $conf = Join-Path $LogDir "jdups.conf"
        if (Test-Path $conf) {
            Write-Host "  kept existing jdups.conf"
        } else {
            & (Join-Path $InstallDir "jdups-agent.exe") --print-config |
                Set-Content -Path $conf -Encoding UTF8
            Write-Host "  wrote $conf (all defaults, all commented out)"
        }

        $exe = Join-Path $InstallDir "jdups-agent.exe"
        $args = "-q --dir `"$LogDir`""
        if ($Serial) { $args += " --serial $Serial" }

        if ($Service) {
            # A service, not a task. What that buys is power notifications --
            # the only way to be told the machine suspended or resumed -- and
            # SERVICE_CONTROL_PRESHUTDOWN, which arrives early enough to stand
            # down cleanly. It does *not* buy running with no user logged in;
            # the SYSTEM task already did that.
            $svc = Get-Service -Name $AgentTask -ErrorAction SilentlyContinue
            if ($svc) {
                if ($svc.Status -ne 'Stopped') { Stop-Service -Name $AgentTask -Force }
                # sc.exe rather than Remove-Service, which needs PS 6+ and is
                # not present on Windows PowerShell 5.1.
                & sc.exe delete $AgentTask | Out-Null
                Start-Sleep -Milliseconds 500
            }
            # The scheduled-task form must not linger, or two agents fight over
            # the singleton and one of them silently loses.
            if (Get-ScheduledTask -TaskName $AgentTask -ErrorAction SilentlyContinue) {
                Stop-ScheduledTask -TaskName $AgentTask -ErrorAction SilentlyContinue
                Unregister-ScheduledTask -TaskName $AgentTask -Confirm:$false
                Write-Host "  removed the $AgentTask scheduled task (superseded by the service)"
            }

            $bin = "`"$exe`" --service $args"
            & sc.exe create $AgentTask binPath= "$bin" start= auto DisplayName= "jdups UPS agent" | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "sc.exe create returned $LASTEXITCODE" }
            & sc.exe description $AgentTask "Watches the UPS and shuts this machine down on a sustained power failure." | Out-Null
            # Restart twice on failure, then leave it alone rather than looping.
            & sc.exe failure $AgentTask reset= 86400 actions= restart/60000/restart/60000/"" | Out-Null
            # Ask for a preshutdown window long enough to finish: the default is
            # generous, but the shutdown transaction budgets its own time.
            & sc.exe failureflag $AgentTask 1 | Out-Null
            Start-Service -Name $AgentTask
            Write-Host "  registered $AgentTask (Windows service, SYSTEM, automatic)"
            $AgentIsService = $true
        } else {
        $action = New-ScheduledTaskAction -Execute $exe -Argument $args
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
        Write-Host "  registered $AgentTask (scheduled task, $desc)"
        }
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

# --- Check the work, rather than reporting the attempt ---------------------
# Everything above prints on the strength of a cmdlet not throwing. That is not
# the same as it having worked, and the difference is invisible afterwards:
# an unelevated `Get-ScheduledTask` cannot see a SYSTEM-principal task at all --
# its task file is ACL'd against ordinary users -- so checking by hand later
# shows one task of three and looks like a half-finished install.
#
# This block runs while still elevated, which is the only moment all three are
# visible at once. It is also the only report here that is evidence.
Write-Host ""
Write-Host "Verifying:"
$expected = @()
if (-not $TrayOnly)    { $expected += $SamplerTask }
if ($Agent -and -not $Service) { $expected += $AgentTask }
if (-not $SamplerOnly) { $expected += $TrayTask }

if ($Service) {
    $svc = Get-Service -Name $AgentTask -ErrorAction SilentlyContinue
    if ($svc) {
        Write-Host ("  {0,-14} {1,-9} as {2}" -f $AgentTask, $svc.Status, "SYSTEM (service)")
        if ($svc.Status -ne 'Running') { Fail "$AgentTask did not start" }
    } else {
        Fail "$AgentTask service did not register"
    }
}

foreach ($t in $expected) {
    $task = Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
    if (-not $task) {
        Fail "$t did not register"
        continue
    }
    $info = $task | Get-ScheduledTaskInfo
    # 0x41301 is "currently running", which is success, not an error code.
    $result = if ($info.LastTaskResult -eq 0x41301) { "running" }
              else { "last result 0x{0:X}" -f $info.LastTaskResult }
    Write-Host ("  {0,-14} {1,-9} as {2}  ({3})" -f $t, $task.State, $task.Principal.UserId, $result)
}

# The logs are the real proof for the two headless tasks: a process that started
# and immediately died would still leave the task looking fine.
if (-not $TrayOnly) {
    Start-Sleep -Seconds 2
    foreach ($pattern in @("jdups-*.csv", "jdups-agent-*.log")) {
        $f = Get-ChildItem (Join-Path $LogDir $pattern) -ErrorAction SilentlyContinue |
             Sort-Object LastWriteTime | Select-Object -Last 1
        if ($f) {
            Write-Host "  wrote          $($f.Name) ($($f.Length) bytes)"
        }
    }
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
    Write-Host "  Thresholds:  $LogDir\jdups.conf"
    Write-Host "  Its log:     $LogDir\jdups-agent-YYYY-MM.log"
    Write-Host "  Check them:  jdups-agent.exe --check"
}
Write-Host ""
Write-Host "  PowerChute must stay installed and armed. jdups reads; it does not"
Write-Host "  shut anything down, and nothing else will if you remove PowerChute."
Write-Host ""
Write-Host "Press Enter to close."; [void][Console]::ReadLine()
