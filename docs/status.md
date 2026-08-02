# Status

Where the work stands, what is proven, and what is next. Written to survive a
context boundary — [implementation-plan.md](implementation-plan.md) carries the
reasoning and the hardware map, so this is deliberately short and points there
rather than repeating it.

Last updated: 2026-08-02, 34 commits in, pushed to origin/main.

## Built

Phases 1–8 of the plan. Three binaries over one lib, one dependency
(`windows-sys`), 151 tests, clippy clean.

**It has replaced PowerChute.** A full armed shutdown ran on real hardware on
2026-08-01: 60-second notice, forced shutdown, UPS cut output about two minutes
later, and the machine came back by itself when mains returned. PowerChute is
still installed and is set to "Do not shut down in the event of a power outage".

| | |
|---|---|
| `jdups.exe` | `--once` `--watch` `--probe` `--list` `--log` `--sample` |
| `jdups-tray.exe` | notification icon, menu, notifications; `--balloon` to fire a test one |
| `jdups-agent.exe` | decides, warns, and shuts the machine down. `--check` `--print-config` |
| `install.ps1` | machine-wide, or `-PerUser` with no elevation; `-Agent` adds the agent |
| `uninstall.ps1` | only elevates if a machine-wide install is present |

`policy.rs` decides, `config.rs` guards the thresholds, `agent/journal.rs`
decides what gets written, `agent/watch.rs` drives the device, and
`agent/shutdown.rs` is the transaction. **`armed = false` is the default and
what a missing config means**, so no accident of packaging produces an agent
that acts.

The interlock is stated, not enforced: PowerChute and jdups write the same UPS
countdown register and the last writer wins. Whether PowerChute is *armed* lives
inside its own configuration and is not visible from outside, so the agent says
so at startup rather than pretending to check.

## Proven against the hardware

Not asserted, measured. The plug-pull capture is `docs/plug-pull.txt`.

- The full report map, from a value **and button** caps walk. Report 22 is
  `PresentStatus`, eleven flags, invisible to a value-only walk.
- `IOCTL_HID_GET_FEATURE` is `METHOD_OUT_DIRECT`. `--probe` re-derives this every
  run because the wrong encoding does not error — it returns a well-formed
  report that decodes as 0 % charge.
- Voltage mapping: report 49 is mains (0 V on battery), report 9 is the battery
  (sagged 27.26 → 24.67 V). The log columns are labelled correctly.
- Report 19 pushes on change only; 12 and 6 are periodic.
- `FF86:52` reads **8** for a plug-pull, across two events.
- The sampler's event path wrote a real `online` row with `transfer=8`.
- **Writes take about 30 ms to become visible to a read.** `HidD_SetFeature`
  returns success immediately, but a read issued straight afterwards returns the
  **old** value. Measured by round-tripping `AudibleAlarmControl`: 1 → 2 → 1,
  each change ~30 ms to settle, confirmed on both of its mirror reports.

  This is a trap with the shutdown transaction's name on it. "Verify every
  write" obeyed literally, with an immediate readback, reports every successful
  write as a failure — so a transaction would cancel every correct arming it
  ever performed. Worse the other way: cancel a countdown with -1, read the
  stale positive value, conclude the cancel failed, while the UPS is still
  counting down to cutting power. `set_feature` polls until the device agrees.
- The charge estimate is a *model*, not a measurement: it drops ~20 points
  within seconds of a transfer and recovers over hours, while battery voltage
  recovers in seconds. This shapes `policy.rs`'s settle window.

## Not verified

Be honest about these rather than assuming they work.

- **`uninstall.ps1` has never been executed.** `install.ps1` now has, machine-wide
  with `-Agent`: three tasks, the ACL applied with inheritance off, both SYSTEM
  processes up and both logs writing. The removal path is still unproven.

  One thing that install verification has to account for: an unelevated
  `Get-ScheduledTask` **silently omits** `jdups-sampler` and `jdups-agent`. A
  task registered with a SYSTEM principal gets a task-file ACL that excludes
  ordinary users, so the query returns only `jdups-tray` and looks like a
  half-finished install. `Test-Path` on the task file answering *access denied*
  rather than *false* is what tells them apart.
- ~~The notification icon.~~ **Fixed and confirmed by eye.** Two icons, not
  one: `Shell_NotifyIcon` controls the large body image, while the header icon
  comes from the AppUserModelID registration. Registering the ID fills the
  header; the body image is now deliberately absent.
- ~~The agent has never seen an outage.~~ **Proven end to end, 2026-08-01**, on
  real hardware in dry run, with absurd thresholds so it cost 4 minutes of
  battery:

  ```
  19:13:34  warn  on battery: 97%, 34 min left (2013 s)
  19:13:49  ACT   decision reached: would shut down: predicted runtime below the threshold
  19:13:49  ACT   shutting down in 70 s. Save your work.
  19:14:59  ACT   the grace period is up: this is where it would shut down
  19:17:33  info  back on mains: 81%, 34 min left (2031 s)
  19:17:33  ACT   shutdown cancelled: the machine is no longer past the trigger
  ```

  Settle and debounce timed exactly, the grace period expired to the second, the
  tray toasted and counted down in red, and the mains-return debounce held for
  five seconds before clearing. Nothing was written to the UPS.

  Two defects only a real run could find, both now fixed: the warning fired
  three times inside one second (`now_s` is whole seconds, the loop is faster
  than that), and the menu still read "On battery" while the icon counted down.
- **`jdups-agent.exe` is a scheduled task, not a service.** So it cannot take
  `SERVICE_CONTROL_PRESHUTDOWN` or power notifications, which means: sleep and
  hibernate are unhandled, and there is no clean last gasp on an ordinary
  reboot. Observed directly — the "final read" on the way out never fired during
  the PowerChute shutdown, because a task has no console to receive
  `CTRL_SHUTDOWN_EVENT` on. This is the largest remaining gap.
- **Report 64 (`FF86:7C`)**: whether it is written or merely reflects an armed
  countdown is still unknown. The first sample already had it set, so the order
  was never observed. The transaction does not write it and works anyway.

## Next

**Needs the machine, not the developer:**

1. ~~Run the installer.~~ Done, machine-wide with `-Agent`. Note that a tray
   running from `target\release` holds a `Local\` singleton mutex, so the
   installed copy exits 0 in silence and the install looks like it did nothing.
   Stop the dev one before installing, and before iterating afterwards.
2. ~~Confirm the toast icon.~~ Done. `--balloon` now forwards to the running
   instance rather than exiting in silence, so it works without stopping the tray.
3. The tray icon sits in the Windows 11 hidden-icon overflow after any change to
   `APP_ID`, because Explorer keys visibility on app identity. Drag it out once.
4. ~~Make the agent decide, on purpose.~~ Done, and worth repeating after any
   change to the decision path. Append to `%ProgramData%\jdups\jdups.conf` from
   an elevated shell, restart the task, pull the plug for ninety seconds:

   ```
   runtime_threshold_s = 3600     # always qualifies
   settle_s = 10                  # the minimum validate() allows
   debounce_s = 5                 # likewise
   warn_before_s = 60
   ```

   **Take them out again afterwards.** `runtime_threshold_s = 3600` means any
   loss of mains qualifies instantly, which is inert today and would not be.

**What is left:**

1. **Deploy tonight's work.** `.\install.ps1 -Agent` — the running agent
   predates the review fixes. Nothing is broken; it is simply older.
2. **Try the service**, when you can watch it: `.\install.ps1 -Agent -Service`.
   Built and tested as far as it can be without elevation, and deliberately not
   switched over unattended.
3. **`shutdown_on_wake`**, off by default. Needs the service, and needs a real
   sleep-and-plug-pull to prove.
4. **Prove `uninstall.ps1`**, still the one script never executed.
5. Retire PowerChute entirely, once a few real outages have gone by.

## What four code reviews found

Run by Codex over four areas on 2026-08-01/02. Roughly thirty findings; the
ones acted on are below, and several were declined as unreachable or as a trade
already made deliberately.

**The transaction, the loop, the policy.** `unwind` aborted Windows *before*
cancelling the UPS, so an arming that committed but failed to read back left a
**running** machine with a live countdown — the exact filesystem-corrupting case
the ordering argument exists to prevent. The disconnected path called `tick` and
dropped the `Action`, so an outage that took the USB link with it could reach the
backstop with nothing listening. One `fresh` bit covered both the status and the
numbers, so a charge read marked a stale status current and an outage might never
latch. A failed transaction latched the retry guard shut forever.

**The tray.** The feature sweep stamped its timestamp whether or not anything was
read, so an unplugged-but-open UPS kept a fresh sweep age indefinitely — which
defeats the staleness rule outright. Crossing into or out of stale did not notify
the UI at all.

**HID.** `preparsed()` returned a raw handle freed on `Device` drop, from safe
code. And the report-ID check in `decode::payload` is **vacuous on feature
reads**: `IOCTL_HID_GET_FEATURE` leaves the caller-supplied first byte alone, so
it compares our own write to itself. It earns its keep on input reports only.

**Config, status, decode.** `decode::payload` length-checked against the *field*
rather than the report, so `charge(&[0x0c, 0])` returned `Some(0)` from two bytes
— a truncated read decoding as a plausible flat battery, which is the number the
agent acts on. `status::parse` defaulted any field it could not read, so a
corrupted `event` advanced the sequence while losing the event, and the tray then
ignored the correction behind it.

**Settled, kept for the record:**

- ~~The restart handshake.~~ **Settled 2026-08-01, and there isn't one.** A real
   PowerChute shutdown was watched register by register. **Report 65 (`FF86:7D`)
   is the shutdown countdown** — set to 120, decremented by the UPS in real time,
   output cut at zero. **Report 64 (`FF86:7C`) is the armed flag.** **Report 21,
   the standard `DelayBeforeShutdown`, was never touched.** Write 65, not 21.

   Mains returned, the UPS restored output unaided, and both registers reset
   themselves. Nothing to configure. The PC did not power on, which is a BIOS
   "restore on AC power loss" setting, not a UPS one.

   Still open, and small: whether 64 is written or merely reflects an armed
   countdown. The first sample already had it set, so the order was never seen.
- ~~The shutdown transaction.~~ **Built and proven armed**, `agent/shutdown.rs`.
  Privilege enabled and checked for `ERROR_NOT_ALL_ASSIGNED`; intent record
  persisted and reconciled on the next start; `InitiateShutdownW` with a 10 s
  grace so the shutdown is *accepted but not yet destructive* while the UPS is
  armed, and `AbortSystemShutdownW` if the arming fails. `SHUTDOWN_INSTALL_UPDATES`
  is deliberately **not** passed: a power-cut shutdown that begins installing a
  feature update would outlast any countdown sized from ordinary ones.

**Until the agent is proven, PowerChute stays installed and armed.**

## The device's current settings

Read 2026-08-01 with `jdups --read`, while PowerChute was installed and armed.
Everything is at idle or factory default, so PowerChute maintains **no** standing
configuration in these registers — which is itself the finding: reading them
while nothing is happening cannot reveal what it does at shutdown.

| Report | Reads | |
|---|---|---|
| 21, 66 | `-1` | `DelayBeforeShutdown`. No countdown scheduled |
| 64 | `0` | `FF86:7C`, boolean |
| 65 | `-1` | `FF86:7D`. Idles exactly like report 21 |
| 33 | `6` | `Test` = "none". No self-test result stored |
| 24, 120 | `1` | `AudibleAlarmControl` = disabled |
| 17 | `10` | `RemainingCapacityLimit`, the UPS's own low-battery point |
| 50, 51 | `88`, `144` | Transfer voltages |

`--read` is read-only and there is deliberately no `--write`. There should not
be one until the restart cycle has been demonstrated on a sacrificial load: a
wrong write here arms a countdown on a live machine.

## Open question: will anyone see the warning?

The agent announces a shutdown `warn_before_s` seconds ahead and the tray shows
a notification. PowerChute instead shows a dialog, which was observed **not** to
block — the shutdown proceeded with it still sitting there unclicked — so both
are informational and this is purely a question of what people notice.

It is not settled. A dialog in the middle of the screen is hard to miss. A
notification in the corner of a 57-inch ultrawide is easy to. The case for the
notification is that it puts nothing in the way of someone already hurrying.

Cheaper ways to raise it, if the notification turns out to get missed, roughly in
order of effort:

- **Put the countdown in the tray icon.** The digit rendering already exists and
  is what the icon does when on battery; a pending shutdown could show the
  seconds instead of the minutes, in the critical colour.
- **Repeat the notification** at, say, 30 s and 10 s rather than once.
- **A borderless always-on-top window** on the active monitor. Genuinely hard to
  miss, still not blocking, and the most work by a wide margin.

## How long this machine takes to shut down

`os_shutdown_s` sizes the UPS countdown, so it is not a preference. Measured
from this machine's own System event log, pairing 1074 (shutdown initiated) with
13 (OS down), 41 samples:

| | N | min | median | max |
|---|---|---|---|---|
| **forced / system** | 35 | 4.9 s | 25.3 s | **76.0 s** |
| Start Menu, waits for apps | 6 | 18.2 s | 23.6 s | **866.2 s** |

**The 866 s outlier is a shutdown waiting on an application**, not an update —
the Windows Update activity near it was an unrelated Defender signature. That is
the single strongest argument for forcing: same machine, same software, and the
difference between comfortably inside the UPS countdown and fourteen minutes
past it.

The agent forces, so 76 s is the number that matters. `os_shutdown_s = 120`
leaves 44 s of margin and the UPS cuts at 130. PowerChute's default happens to
be 120 too, but this one is chosen from measurement rather than inherited.

Worth re-running after any big change to what is normally open:

```powershell
Get-WinEvent -FilterHashtable @{LogName='System'; Id=1074,13} -MaxEvents 200
```

## Review findings deliberately not fixed

Recorded so they read as decisions rather than oversights. Someone should feel
free to disagree with any of them.

- **A failed intent record does not abort the shutdown.** The transaction logs
  it and continues. Refusing to protect a machine because a small file could not
  be written is the wrong trade; the cost is that a crash in the following
  seconds leaves a countdown reconciliation cannot attribute.
- **The config file's ACL and ancestors are not verified.** A user who can
  pre-create `%ProgramData%\jdups\jdups.conf` with a protected DACL *before*
  installation could feed a SYSTEM process its thresholds. Real, but it needs
  local access ahead of install, and verifying ownership up the whole path is a
  meaningful chunk of code guarding a narrow window. The installer does refuse a
  reparse point on the directory.
- **`agent-status.txt` can be denied by pre-creation.** Same shape: create it as
  a directory before install and every publish fails, suppressing the tray's
  shutdown warning. The log would still record everything.
- **`HidD_SetFeature` and the readback poll are unbounded.** A wedged driver
  could hang the agent thread during the transaction. Bounding it means moving
  writes to overlapped I/O, and the failure it guards against is benign in
  practice: Windows is already going down by then, so the machine shuts down
  cleanly and the UPS simply never gets armed.
- **`GetOverlappedResult(..., TRUE)` after `CancelIoEx` can wait indefinitely**
  if the driver never completes the cancellation. Documented Windows behaviour,
  no clean fix without a second thread.
- **Assorted low-severity logging items:** `jdups-2026-99.csv` passes the name
  check, a file whose header write failed is never repaired, and the per-window
  sample count could overflow after `u32::MAX` samples. None reachable in
  practice at a five-minute cadence.

## Pinned: an alarm toggle in the tray

**Confirmed possible, not speculation.** `AudibleAlarmControl` (`0084:5A`,
reports 24 and 120) takes 1 = disabled, 2 = enabled, 3 = muted, and it was
round-tripped 1 → 2 → 1 on the real unit with readback confirmation. It needs no
elevation: the tray already opens the device `ReadWrite` and the write succeeded
from an ordinary shell.

So a checkable menu item is a small piece of work, and a genuinely nice one --
the vendor makes you load a web app to silence a beeping UPS at 3 a.m.

Two things to get right when it is built:

- **Write 24, then read it back with the settle loop** and reflect what the
  device actually holds, not what was asked for. Both 24 and 120 mirror the same
  value, so either can confirm.
- **The menu item is a write**, and every other item in that menu is a read that
  copies to the clipboard. It should not be possible to change the alarm by
  misclicking a row while reaching for the readout.

## Worth not losing

Two threads that live only in conversation so far.

**Is the battery dying?** Installed 2021-11-23, so 4 y 8 m against a typical SLA
service life of 3–5 years. Under a 234 W load it sagged to 24.67 V; a rested
full 24 V pack sits ~25.8–26 V, so ~1.2 V of sag at ~11.6 A implies roughly
**100 mΩ** internal resistance against 40–60 mΩ for a healthy pair. That is
suggestive, not a diagnosis — the open-circuit voltage was estimated rather than
measured, the pack's Ah rating is unknown, and inverter efficiency was assumed.

Counterweight: during a **real outage it ran ~30 minutes at idle**, matching the
device's own estimate. So `RunTimeToEmpty` is honest even while the charge model
misbehaves — which matters, because that is the variable Phase 8 thresholds on.

The way to settle it is the log: `battery_v` on an `onbattery` row is the loaded
voltage and the preceding interval row has the float voltage, so the sag is
already derivable with no schema change. Compare today's ~1.2 V against the same
figure in six months.

`NeedReplacement` (`85:48`) is **not** in this device's button caps — checked
against the full walk. The UPS cannot be asked directly.

## Tools worth knowing about

All ignored by default; they exist to be looked at.

```
cargo test --release -- --ignored contact_sheet  --nocapture   # icon states x DPI
cargo test --release -- --ignored font_specimen  --nocapture   # every glyph, labelled
cargo test --release -- --ignored glyph_workshop --nocapture   # candidate glyphs side by side
```

The specimen prints which row each icon size actually renders with, which is the
line connecting it to reality. This machine is at 125 % scaling, so the tray
draws a **20 px** canvas and uses `FONT_8X13` — row 9.
