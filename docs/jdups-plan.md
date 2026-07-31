# Plan: `jdups` — a tray readout for an APC Back-UPS

Status: proposed, not started. Written after investigating the live hardware;
every number below was read off the device, not inferred from documentation.

## The headline

**PowerChute is not needed for any of the live data.** The UPS is a standard USB
HID Power Device. Every value the PowerChute web UI displays is one HID feature
report read, the device opens *shared* while the PowerChute service is running,
and the whole read takes milliseconds. A ~250 KB Rust binary can do the readout
half of a bundled JRE + Jetty + 90 jars.

Verified on the actual unit, against the web UI, while `APCPBEAgent` was running.

## Hardware

- **UPS:** APC Back-UPS RS 1500MS2 — the web UI and the device's own
  `ConfigActivePower` (900 W) both say 1500MS2, so treat "RS1200MS2" as a slip.
- **Connection:** the RJ50-to-USB cable. Despite looking like a serial relic, it
  presents as plain USB HID.
- **USB ID:** `051D:0002` (American Power Conversion). The unit's serial appears
  in the USB devnode path (`USB\VID_051D&PID_0002\<serial>`), which is worth
  knowing if you ever need to tell two units apart — and worth *not* pasting
  into a public repo.
- **HID:** Usage Page `0x84` (Power Device), Usage `0x04` (UPS).
  Input report 5 bytes, feature report 5 bytes, no output reports.
- Windows attaches it as a Battery-class device (`HID\VID_051D&PID_0002`).

## What was tested, and what it rules in and out

| Route | Result |
|---|---|
| `Win32_Battery` WMI | **Empty.** That class only covers portable batteries. |
| `GetSystemPowerStatus` | **`BatteryFlag = 128`** — Windows does not treat the UPS as a system battery. |
| `GUID_DEVICE_BATTERY` interface + battery IOCTLs | **No interface enumerated**, despite the Battery-class devnode. |
| **Direct HID feature reports** | **Works.** Opens shared, reads everything, no elevation. |

So the battery-class routes are all dead ends here and the plan should not spend
time on them. Direct HID is the answer, which also makes this the same shape as
`jdrgb`: `hidapi`, one-shot reads, nothing resident but the tray itself.

Both `CreateFile` with access `0` and with `GENERIC_READ` succeed with
`FILE_SHARE_READ | FILE_SHARE_WRITE` **while PowerChute holds the device**. That
is the single most important fact in this document: jdups and PowerChute can
coexist, so nothing has to be uninstalled to try this.

## The data map

All read as 5-byte feature reports: `buf[0]` is the report ID, `buf[1..5]` the
payload. Values below are real readings taken during the investigation.

| Field | Usage | Report | Decode | Read |
|---|---|---|---|---|
| Battery charge | `0x85:0x66` RemainingCapacity | 12, byte 1 | `u8` % | **100 %** |
| Runtime remaining | `0x85:0x68` RunTimeToEmpty | 12, bytes 2–3 | `u16` seconds | **2274 s = 38 min** |
| UPS load | `0x84:0x35` PercentLoad | 80, byte 1 | `u8` % | **20 %** |
| On mains | `0x85:0xD0` ACPresent | 19, byte 1 | bool | **1** |
| Charging / discharging | `0x85:0x44` / `0x85:0x45` | 6, bytes 1–2 | bool | **0 / 0** |
| Battery voltage | `0x84:0x30` Voltage | 9 or 38 | `u16` × 0.01 V | **2726 → 27.26 V** |
| Input voltage | `0x84:0x30` Voltage | 49 | `u16` V | **118 V** |
| Rated output | `0x84:0x44` ConfigActivePower | 82 | `u16` W | **900 W** |
| Battery date | `0x85:0x85` ManufacturerDate | 7, 32, 123 | packed, see below | **2021-11-23** |
| Low transfer point | `0x84:0x53` | 50 | `u16` V | **88 V** |
| High transfer point | `0x84:0x54` | 51 | `u16` V | **144 V** |
| Last transfer reason | `0xFF86:0x52` (APC vendor) | 54 | `u8`, 0–13 | **0** (none) |

Cross-check against the web UI at the same moment: 100 % charge, 27.2 VDC,
118.0 VAC, 18–20 % load, 38–43 min. Everything agrees.

Report 12 is the one that matters most — it carries **both** charge and runtime
in a single 5-byte read, which is the entire "what do I actually need to know"
payload.

Watts = `PercentLoad × ConfigActivePower / 100` → 20 % × 900 W = 180 W. Note
PowerChute's own energy log disagrees: it records 189 W at 18 %, implying a
1050 W reference (1500 VA × 0.7) rather than the 900 W the device reports for
itself. Prefer the device's figure and don't try to match PowerChute.

### The battery date is real

You suspected a date written to a file. It isn't. `0x85:0x85` is the HID
**ManufacturerDate** usage, read straight off the UPS, and it is *more* precise
than the web UI shows — the device says **2021-11-23** where PowerChute renders
only `11 / 2021`. Decode is the standard HID packing:

```
year  = 1980 + (raw >> 9)
month = (raw >> 5) & 0x0F
day   = raw & 0x1F
```

`21367 → 2021-11-23`, consistent with buying the unit in early 2022. Three
separate report IDs (7, 32, 123) all return it.

It is a writable usage, which is how PowerChute's "Battery Installation Date"
field works — so it is genuinely device state, but state a human can set. Treat
it as "what someone last told the UPS", not a factory stamp. **Writing it is a
non-goal** (see below).

## Logging — the honest answer

You asked whether logging needs PowerChute. Three separate things, three answers:

1. **PowerChute's `agent/EventLog`** — a **Java serialized object stream**
   (`java.io.ObjectOutputStream`, full of `com.apcc.m11.arch.event.Events`). Not
   text, not JSON. Parsing it outside a JVM means reimplementing Java
   serialization against APC's private classes. **Don't.** This is the one thing
   that genuinely needs their software.

2. **PowerChute's `agent/energylog/YYYY-MM.log`** — plain text, and pleasant:
   ```
   # $interval=300
   #2010timestamp;realLoad(watts);relativeLoad(percentage);calculatedLoad(watts)
   523200534;null;18.000;189.000
   ```
   Semicolon-separated, one sample per 5 minutes, timestamps in **seconds since
   2010-01-01**. Trivially parseable — but it only exists because PowerChute is
   running and writing it.

3. **The Windows Application event log** — PowerChute writes there under
   provider **`APCPBEAgent`** (IDs 1000 "Monitoring Stopped", 1001 "Monitoring
   Started", 1002 "Communications Established", and the power events). Readable
   by anything, no PowerChute process required to *read* it, and it is where the
   existing history already lives.

**So: you don't need PowerChute to log, but you do need it to keep the log it
already has.** The clean answer for jdups is to keep its own, because it has
direct device access and owes nothing to anyone:

- Sample `input voltage`, `load`, `ACPresent`, `charge`, `runtime` on a slow
  timer and append a line per sample.
- Newline-delimited JSON or the same semicolon CSV shape — either is fine, but
  pick text a human can open, which is the whole point of the "Open log" button.
- Roll monthly like PowerChute does, so a file never gets unmanageable.

The gap is honest and worth stating up front: **a tray app only logs while
you're logged in.** For "how crappy is my power and is this thing dying", that
is almost certainly fine — the machine is on when you care — but it is not the
same guarantee a service gives. If the gap ever matters, the same binary can be
registered as a second scheduled task that samples on an interval without a UI,
which is a small addition rather than a redesign.

For "is this thing dying" specifically, the useful series is **runtime at a
known load** over months, plus battery voltage. Both are one read.

## Architecture

Follow `jdrgb`'s tray closely — it is a known-good shape and you liked the
result. Differences are noted where the two genuinely differ.

```
jdups.exe   (windows subsystem)
  ├── hid.rs      open 051D:0002, read feature reports, decode
  ├── draw.rs     generated icon + menu bitmaps
  ├── log.rs      sampler + append
  └── main.rs     window, tray icon, menu, message loop
```

- **Rust**, `hidapi` + `windows-sys`, no GUI toolkit. Hand-rolled Win32 tray.
  Expect a similar footprint to jdrgb (~250 KB, single-digit ms startup).
- **One binary**, unlike jdrgb. There is no pre-existing CLI to preserve here, so
  the two-subsystem problem doesn't arise. If a console readout turns out to be
  wanted for scripting or debugging, `AttachConsole(ATTACH_PARENT_PROCESS)` lets
  the same windows-subsystem binary print to the calling shell — cheaper than a
  second target.
- **No admin.** Nothing in the read path needs it.

### Reading

Read on demand when the menu opens. The whole set is a handful of 5-byte feature
reports and completes in milliseconds, so there is no reason to cache and every
reason not to — a cached readout goes stale the moment you look away.

**Do not poll for events.** The device has a 5-byte *input* report, which means a
blocking `ReadFile` on a `GENERIC_READ` handle wakes up exactly when the UPS
state changes. A background thread parked on that read costs nothing while idle
and gets you an instant "On Battery" notification, rather than discovering it up
to N seconds late. This is the nicest thing available here and worth doing
properly. Keep a slow timer (30–60 s) as a backstop only.

### The icon

jdrgb generated its icon from the same pixel loop as the swatches, and the same
trick applies with more reason: the icon should *be* the state. A battery outline
with a fill proportional to charge, recoloured on `ACPresent = 0`, tells you
everything at a glance without opening anything.

You said you'd make an icon. Both can be true — a hand-drawn base for identity,
with the charge fill and the on-battery colour generated over it. If you'd rather
keep it purely static, the tooltip carries the numbers instead. Worth deciding
before the drawing code is written, since "generated gauge" and "static asset
with overlay" want slightly different code.

One lesson from jdrgb's swatches, since it applies directly: any outline has to
be carved *out of* the icon's own area, so a ringed shape reads as smaller than
an unringed one. If the icon has both a border and a fill level, keep the border
constant so the fill is the only thing that moves.

### Menu

Read-only rows on top, actions below. Mirrors jdups's actual job, which is
looking rather than doing.

```
  ▮ On line — 100%, 38 min       <- status; icon colour matches
  ─────────────
  Load          20%  (180 W)
  Input        118 V
  Battery     27.3 V
  Battery installed  2021-11-23
  ─────────────
  Open log
  Open PowerChute                <- https://localhost:6547, only if installed
  ─────────────
  Exit
```

Two things learned the hard way on jdrgb, which apply unchanged:

- A **disabled** menu item renders dim and is easy to misread as broken. There is
  no "bright but inert" state in a standard Win32 menu without `MFT_OWNERDRAW`,
  and owner-draw forfeits the Windows 11 rounded menu styling. Since every row
  here is genuinely read-only, either accept the dim look for the data rows, or
  give them a real click (e.g. copy the value) so they can be enabled honestly.
- Use `MIIM_BITMAP` for any icons in the menu, never owner-draw, for the same
  styling reason.

### Log viewer

"Open log" should just hand the file to the shell — `ShellExecuteW` with the
`open` verb respects whatever the user has associated with `.log`/`.json`, which
on this machine will be Sublime or similar. Don't hardcode an editor, don't
bundle a viewer.

## Windows *does* have UPS shutdown. APC disabled it.

Worth knowing before deciding what to build, because it very nearly makes the
agent unnecessary.

`C:\Windows\INF\oem50.inf` — provider "APC by Schneider Electric", **DriverVer
11/03/2009** — is `Class=Battery` and claims `HID\VID_051D&PID_0000` through
`0012`. Its install section registers **no service**: `DEVPKEY_Device_Service` on
the devnode is empty. It is a null driver whose only function is to occupy the
Battery devnode so the inbox HID battery driver cannot bind.

That single fact explains all three dead ends recorded earlier — no
`Win32_Battery` row, `GetSystemPowerStatus` reporting `BatteryFlag = 128`, and no
`GUID_DEVICE_BATTERY` interface. Windows would treat this UPS as a system battery
out of the box. APC turned that off so PowerChute could own the device.

**Option A — rebind the inbox driver.** The UPS becomes a Windows battery, and
Power Options grows *Critical battery action → Shut down*. No code, reversible
with a driver rollback.

It is still the wrong tool, for one decisive reason: **Windows can only threshold
on percentage.** For a UPS that is a poor variable — 20 % at 5 % load is half an
hour, 20 % at 80 % load is ninety seconds. `RunTimeToEmpty` already folds load in
and the device computes it. Windows cannot act on it. Also expect a taskbar
battery icon and laptop-oriented battery-saver behaviour on a desktop.

Worth trying once regardless, as a free experiment and a fallback.

**Option B — write the agent.** Nothing here is proprietary. Everything needed is
standard HID Power Device usage already read off the unit: `RemainingCapacity`,
`RunTimeToEmpty`, `ACPresent`, and `DelayBeforeShutdown` (`0x84:0x57`, reports 21
and 66). The shutdown itself is `InitiateSystemShutdownExW`.

The difficulty is operational, not protocol:

1. **It cannot be the tray app.** It must run with nobody logged in — a SYSTEM
   service or SYSTEM scheduled task. The tray and the agent are two binaries
   sharing a HID module, exactly as `jdrgb` shares its palette.
2. **`SE_SHUTDOWN_NAME` must be explicitly enabled** in the process token before
   the shutdown call. SYSTEM holds the privilege but not enabled by default, and
   the failure is silent.
3. **Debounce.** One glitched read must never take the machine down. Require
   several consecutive samples past the threshold *and* `ACPresent = 0`.
4. **The restart handshake.** Write `DelayBeforeShutdown` so the UPS cuts its own
   output once the OS is down; otherwise returning mains leaves the machine off.
   Also depends on the BIOS "restore on AC power loss" setting.
5. **PowerChute must be disarmed first.** Two armed agents will both act.

### Testing it without dreading it

The decision is a pure function of `(charge, runtime, on_battery, history)` →
`Action`. Unit-test it exhaustively with no hardware; that is where the real risk
lives, and it is entirely testable offline.

Then, in order:

- **Dry run.** A mode that logs what it *would* do and never calls shutdown. Run
  it for a couple of weeks against real power.
- **Absurd thresholds.** Set "shut down below 95 % or 30 minutes" so pulling the
  plug fires it in about ten seconds. There is no need to drain the battery to
  test the trigger — this is the trick that makes live testing cheap.
- **Then** the realistic thresholds, once with nothing important running.

## Non-goals

Firm ones, with reasons:

- **Graceful shutdown, *as part of the tray app*.** Promoted to a wanted feature
  — see the section above — but it stays a separate binary. A readout that is
  wrong shows a stale number; a shutdown agent that is wrong eats a filesystem.
  Different stakes deserve different code, different testing, and a different
  process lifetime (SYSTEM, no logged-in user).

  Until that agent exists and is trusted, **do not uninstall PowerChute** — you
  would silently lose unattended shutdown and find out during an outage.
- **Writing the battery date**, or any other writable usage. Reads are safe and
  reversible; writes to a UPS's configuration are neither, and PowerChute already
  has a field for it.
- **Triggering self-tests** (`0x84:0x58` is right there, and reads `6`). Same
  reasoning — a self-test drops to battery deliberately.
- **Parsing PowerChute's `EventLog`.** See above.
- **Energy cost / CO2 reporting.** That's the part of PowerChute nobody asked
  for.

## Open questions for whoever builds this

1. **Which voltage report is which.** Reports 9 and 38 both returned `2726`
   (27.26 V, battery) and report 49 returned `118` (mains). Confirm the mapping
   holds when the unit is actually on battery, since that is when they diverge —
   pull the plug once and re-read.
2. **`0xFF86:0x52` last transfer reason** reads `0` with a clean power history.
   Its range is 0–13, and NUT's `apc-hid.c` is the reference for what the codes
   mean. Worth decoding for the log, but you need an actual transfer to verify.
3. **Whether report 12's two fields are always coherent** — it packs charge and
   runtime into one read, which is convenient but assumes the firmware updates
   both atomically. Probably fine; worth not assuming in the log.
4. **Icon direction** — generated gauge vs. static asset (see above).

## Appendix: reproducing the investigation

Everything above was obtained with `CreateFileW` on
`\\?\hid#vid_051d&pid_0002#...` with access `0` and
`FILE_SHARE_READ | FILE_SHARE_WRITE`, then `HidD_GetFeature` per report ID. The
usage map came from `HidD_GetPreparsedData` + `HidP_GetValueCaps(HidP_Feature)`,
which returned **63 feature value caps** — the table above is the useful subset.

In Rust, `hidapi`'s `get_feature_report` covers the reads directly; the usage map
only matters if you want to rediscover report IDs on different hardware, in which
case it is worth keeping the `HidP_GetValueCaps` walk behind a `--probe` flag,
the way `jdrgb probe` earns its place.

A one-liner sanity check before writing any Rust, to confirm the device is
present and talking:

```powershell
# expects 100 %, and runtime in seconds, from report 12
$b = New-Object byte[] 5; $b[0] = 12
# ...CreateFileW + HidD_GetFeature, then:
# $b[1] = charge %, [BitConverter]::ToUInt16($b,2) = runtime seconds
```
