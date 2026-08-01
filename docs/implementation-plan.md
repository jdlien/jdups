# Implementation plan: `jdups`

Companion to [jdups-plan.md](jdups-plan.md), which is the investigation record —
the hardware, the first HID map, and why the battery-class routes are dead ends.
That document establishes *what is possible*. This one is *what to build, in what
order, and how to know it works*.

Status: ready to start. Everything in "What the device actually exposes" was
measured on the live unit while writing this, with `APCPBEAgent` running. This
document has been through one adversarial review; see
[What the review changed](#what-the-review-changed).

## Decisions taken

| Question | Decision |
|---|---|
| v1 scope | Readout **and** shutdown agent, phased — the readout ships and gets used before the agent is ever armed |
| Icon | Generated gauge, no artwork. Digits rendered **into** the gauge, but only when there is something to say |
| Menu data rows | Enabled; clicking any of them copies the **whole readout** as one multi-line snapshot |
| Logging | A headless sampler, so history survives logoff and reboot |
| Log owner | The sampler, exclusively. The tray displays and never writes |
| Log format | CSV, monthly files, ISO-8601 local timestamps |
| HID access | **Direct Win32, not `hidapi`** — see [why](#why-not-hidapi) |
| Binaries | `jdups.exe` (tray + sampler + console readout) and, from Phase 8, `jdups-agent.exe` as a **Windows service** |

Decided rather than asked, each stated with reasons where it lands: what number
the icon shows ([Phase 3](#phase-3--the-icon)), the log being a median rather
than a spot read ([Phase 6](#phase-6--logging-and-the-sampler)), and dropping
`hidapi` (immediately below).

## What the device actually exposes

The investigation's table was correct but partial. A full
`HidP_GetValueCaps` + `HidP_GetButtonCaps` walk gives the real map, and three
entries in it change the design.

> **Parsing note, because it cost an hour:** `HIDP_VALUE_CAPS` is **72 bytes**,
> not 76. The union's `DataIndexMin`/`DataIndexMax` are `USHORT`, not `ULONG`.
> .NET's `Marshal.SizeOf` computes 76 for the obvious hand-translation and
> silently misaligns every entry after the first, producing plausible-looking
> garbage. Verify the stride against a raw dump before trusting any caps walk.

### The reports that matter

| Report | Usage | Type | Decode | Reading |
|---|---|---|---|---|
| **22** | **`PresentStatus`** — 11 flags | **input + feature** | bitfield, see below | `0C 00 00 00` |
| 12 | `85:66` RemainingCapacity + `85:68` RunTimeToEmpty | input + feature | `u8` %, `u16` s | 100 %, 2595 s |
| 6 | `85:44` Charging, `85:45` Discharging, `FF86:60` | input + feature | bools | 0 / 0 |
| 19 | `85:D0` ACPresent | **input** + feature | bool | 1 |
| 20 | `84:69` ShutdownImminent, `85:42` BelowRemainingCapacityLimit | **input** + feature | bools | 0 |
| 33 | `84:58` Test | input + feature | `u8` 0–6 | 6 |
| 80 | `84:35` PercentLoad | feature | `u8` % | 20 % |
| 82 | `84:44` ConfigActivePower | feature | `u16` W | 900 W |
| 49 | `84:30` Voltage (input) | feature | `u16` V | 117 V |
| 9, 38 | `84:30` Voltage (battery) | feature | `u16` × 0.01 V | 27.26 V |
| 7, 32, 123 | `85:85` ManufacturerDate | feature | packed | 2021-11-23 |
| 50, 51 | `84:53` / `84:54` transfer points | feature | `u16` V | 88 / 144 |
| 54 | `FF86:52` last transfer reason | feature | `u8` 0–13 | 0 |
| 17 | `85:29` RemainingCapacityLimit | feature | `u8` 1–100 | — |
| 21, 66 | `84:57` **DelayBeforeShutdown** | feature | `i16`, −1..32767 | **−1** |
| 64 | `FF86:7C` (APC vendor) | feature | `u8` 0–1 | — |
| 65 | `FF86:7D` (APC vendor) | feature | `i16`, −1..32767 | — |

### 1. Report 22 is `PresentStatus`, and it is the most useful report on the unit

The draft of this plan listed report 22 as "unidentified, constant `12`, noted so
it is not mistaken for corruption". It is nothing of the sort. It carries
**eleven status flags in one five-byte read**, pushed on the input stream:

```
ACPresent            BatteryPresent       Charging          Discharging
ShutdownImminent     BelowRemainingCapacityLimit            Overload
RemainingTimeLimitExpired (x2)            CommunicationLost VoltageNotRegulated
```

It never showed up in a value-caps walk because these are *button* caps — 1-bit
usages — and `HidP_GetValueCaps` does not return them. Anything reading this
device that only walks value caps will conclude the same wrong thing.

**Decode it with `HidP_GetUsages`, never by hand-guessing bit positions.** The
bit order is a property of the report descriptor, not of the usage numbers, and
a wrong guess yields flags that look plausible and are wrong — the worst possible
failure for something the shutdown agent depends on.

### 2. `ACPresent` *is* pushed as an input report

The draft hedged on this and proposed a plug-pull to find out. Not needed: the
input caps list reports **6, 12, 19, 20, 22, 33**, and 19 is `ACPresent`
outright. It did not appear in the 48-second capture for the obvious reason —
nothing changed. Reports 12 and 6 are periodic; 19, 20 and 33 are on-change.

That is the best of both worlds and it removes the design's one real ambiguity:
mains loss arrives as a push, twice over (report 19 and report 22's flag).

### 3. `ShutdownImminent` is a device-authoritative signal

`84:69` arrives on the input stream. The UPS itself says when it is about to cut
output. This is strictly better evidence than any threshold the agent computes,
and Phase 8 treats it as an immediate, non-debounced trigger.

### 4. There is no `DelayBeforeStartup` on this device

This is the one finding that makes the shutdown story *harder*, and it needs
saying plainly because both earlier documents assumed otherwise.

`DelayBeforeShutdown` (`84:57`) exists, on reports 21 and 66, resting at −1.
**`DelayBeforeStartup` (`84:56`) and `DelayBeforeReboot` (`84:55`) do not appear
anywhere in the 63 feature caps.** So `DelayBeforeShutdown` alone cuts the
output and provides no documented way to bring it back — exactly the gap the
review identified, now confirmed against the hardware rather than assumed.

What does exist is two APC vendor usages with the right shape:

- **report 65, `FF86:7D`, `i16`, −1..32767** — identical range and type to
  `DelayBeforeShutdown`, which is what a startup delay would look like.
- **report 64, `FF86:7C`, `u8`, 0..1** — a boolean, plausibly an arm/trigger.

That is a hypothesis, not a finding. NUT's `apc-hid.c` is the reference for the
`FF86` page and Phase 8 must confirm against it and against the hardware,
**with a sacrificial load, never the real machine**. Until the full
shutdown → mains-return → restart cycle has been demonstrated end to end, the
agent does not get armed. If it cannot be demonstrated, the honest fallback is an
agent that shuts the machine down safely and leaves restart to BIOS
"restore on AC power loss" — losing only the case where mains returns *during*
the shutdown window.

### 5. Other corrections to the investigation

- **The input stream is not a change event.** `jdups-plan.md` claimed a blocking
  read "wakes up exactly when the UPS state changes." It does not — the device
  pushes ~0.85 reports/sec while idle, multiplexed across report IDs. **Dispatch
  on `buf[0]`**; code assuming one fixed input layout decodes report 6 as a 0 %
  charge. And **act on transitions, not arrivals**, or the first outage produces
  a notification every three seconds.
- **Runtime jitters ±3.5 % at a dead-steady load.** Three quantised values
  (2508 / 2595 / 2688 s, ~90 s steps) cycling at a constant 100 % and 20 % load:

  ```
  runtime jitter at steady state: min=2508s max=2688s mean=2632s spread=180s (6.8%)
  ```

  This is the most important finding for the log, because "runtime at a known
  load over months" is why the log exists, and that spread swamps battery decay
  for the first year or more. It is also why the agent's debounce is a
  requirement rather than a precaution.
- **Report 12 packs charge and runtime atomically** — same five bytes, so the
  investigation's open question 3 does not apply.
- **All four open modes succeed** with PowerChute running: access `0`,
  `GENERIC_READ`, `GENERIC_WRITE`, and both. Coexistence holds.

### Why not `hidapi`

jdrgb uses it; this project should not. Three independent reasons, each of which
came out of the review or the caps walk:

1. **No bounded feature I/O.** `hidapi`'s Windows feature path is synchronous
   with no deadline. A wedged device freezes whatever thread called it — the tray
   UI, the sampler, or the agent on its way to a safety decision. `read_timeout`
   covers input reports only.
2. **It hides the preparsed data.** Correct `PresentStatus` decoding needs
   `HidD_GetPreparsedData` + `HidP_GetUsages`, which `hidapi` does not expose.
3. **It fails open, silently.** It requests `GENERIC_READ|GENERIC_WRITE` and on
   failure retries with access `0` (`etc/hidapi/windows/hid.c:343`, `:995`). An
   access-`0` handle still serves feature reports but cannot `ReadFile`, and the
   Rust API gives no way to tell which one you got — the entire event-driven
   design would disable itself with no error anywhere. All four modes succeed on
   this unit today, but nothing would tell us if that changed.

Direct Win32 through the `windows-sys` we already need costs perhaps 150 lines —
`CreateFileW` with `FILE_FLAG_OVERLAPPED`, `ReadFile` with an event and a
deadline, `DeviceIoControl(IOCTL_HID_GET_FEATURE / SET_FEATURE)` with
`CancelIoEx` on expiry, and the `HidP_*` decode helpers — and removes a C
dependency from a 250 KB binary. That is a good trade for a project whose entire
premise is not shipping a runtime to read six numbers.

## Layout

```
jdups/
├── Cargo.toml           [lib] + jdups.exe (windows subsystem)
│                        + jdups-agent.exe (console subsystem / service, Phase 8)
├── src/
│   ├── lib.rs
│   ├── hid/
│   │   ├── mod.rs       the Device trait + a scripted fake for tests
│   │   ├── raw.rs       CreateFile, overlapped read, bounded feature I/O
│   │   └── caps.rs      preparsed data, HidP_GetUsages, report discovery
│   ├── decode.rs        pure byte -> value functions. No I/O
│   ├── model.rs         Reading, Status, Snapshot, display formatting
│   ├── logfile.rs       CSV append, monthly roll, interval accumulator
│   ├── policy.rs        shutdown decision, a pure function (Phase 8)
│   ├── tray/
│   │   ├── main.rs      window, tray icon, menu, message loop
│   │   ├── draw.rs      gauge + digit rendering
│   │   └── device.rs    the device thread; publishes Snapshot
│   └── agent/
│       ├── main.rs      service entry, SCM plumbing
│       └── txn.rs       the shutdown transaction state machine
├── install.ps1
└── uninstall.ps1
```

`decode.rs` holding **no I/O at all** is the load-bearing structural choice.
Every value this project shows is a pure function from five bytes, so the
interesting logic tests exhaustively with no UPS attached, against the real
captured payloads in this document.

Follow jdrgb's `Cargo.toml` profile verbatim (`opt-level = "z"`, fat LTO,
`codegen-units = 1`, `panic = "abort"`, `strip`).

## Dependencies

**Rust**, as with jdrgb — but note the reasoning changed once `hidapi` went. The
case now rests on the sibling project being a working reference for the hard
Win32 parts, on `cargo test` covering `decode`/`policy`/`logfile`/`txn` where the
Phase 8 risk actually lives, and on the ~250 KB no-runtime binary that is the
project's whole thesis. What Rust does *not* buy is safety in the tray itself —
that layer is `unsafe` regardless, and C++ would be about equal there.

One dependency for the readout. One more, agent-only, in Phase 8.

```toml
[dependencies]
windows-sys = { version = "0.61", features = [
    # --- Phase 1: device layer -------------------------------------------
    "Win32_Foundation",
    "Win32_Devices_HumanInterfaceDevice",     # HidD_*, HidP_*, HIDP_*_CAPS
    "Win32_Devices_DeviceAndDriverInstallation", # SetupDi* enumeration
    "Win32_Storage_FileSystem",               # CreateFileW, ReadFile
    "Win32_System_IO",                        # OVERLAPPED, DeviceIoControl,
                                              # GetOverlappedResult, CancelIoEx
    "Win32_System_Threading",                 # CreateEventW, Wait*, CreateMutexW
    "Win32_System_Console",                   # AttachConsole
    # --- Phase 2-4: tray --------------------------------------------------
    "Win32_UI_WindowsAndMessaging",
    "Win32_UI_Shell",
    "Win32_UI_HiDpi",
    "Win32_Graphics_Gdi",
    "Win32_Security",                         # SECURITY_ATTRIBUTES, token privs
    "Win32_System_LibraryLoader",             # uxtheme ordinals
    "Win32_System_DataExchange",              # clipboard
    "Win32_System_Memory",                    # GlobalAlloc/Lock for the clipboard
    # --- Phase 8: agent ---------------------------------------------------
    "Win32_System_Shutdown",                  # InitiateSystemShutdownExW
    "Win32_System_Power",                     # power notifications
    "Win32_System_Services",
    "Win32_System_EventLog",
] }
```

Feature cost is zero for anything unreferenced — jdrgb measured a byte-identical
binary after adding six features it did not yet use. `windows-sys` is pure FFI
declarations, so unused ones emit no code and create no imports. List them once
rather than editing this per phase.

**No async runtime.** One thread parked on an overlapped read does not need one.

**Verified, not assumed** — `windows-sys 0.61.2` carries the whole HID surface
(`HidD_GetPreparsedData`, `HidP_GetValueCaps`, `HidP_GetButtonCaps`,
`HidP_GetUsages`, `HidD_GetFeature`/`SetFeature`) and the SetupAPI four we need.
More usefully, its `HIDP_VALUE_CAPS` union declares `DataIndexMin`/`DataIndexMax`
as `u16` — the exact field a hand-translation gets wrong, and did during this
investigation, silently misaligning every caps entry. The bindings are generated
from Microsoft's Win32 metadata, so that class of bug is structurally gone. Given
that decoding `PresentStatus` wrong is a Phase 8 safety issue, that is worth more
here than it looks.

### Phase 8 only: `windows-service`

`windows-service = "0.8"` (Mullvad), in the agent binary alone so it never
touches the tray's size. Hand-rolling `StartServiceCtrlDispatcherW` and the
`SERVICE_STATUS` transition machine is fiddly, and a subtly wrong status deadline
is a silent failure in the one binary allowed to shut the machine down.

Confirmed it exposes what Phase 8 actually needs: `ServiceControl::Preshutdown`,
`ServiceControlAccept::PRESHUTDOWN`, and `set_preshutdown_timeout` via
`SERVICE_CONFIG_PRESHUTDOWN_INFO`. Preshutdown is the reason the agent is a
service at all, so this was worth checking before committing to it.

### The enumeration work `hidapi` was hiding

Dropping `hidapi` moves device discovery into this project, and the earlier
"~150 lines" estimate did not account for it. Budget **60–80 lines more**:

```
HidD_GetHidGuid
  -> SetupDiGetClassDevsW(DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
  -> SetupDiEnumDeviceInterfaces (loop)
  -> SetupDiGetDeviceInterfaceDetailW  (called twice: size, then data)
  -> CreateFileW(path, 0, FILE_SHARE_READ|FILE_SHARE_WRITE)
  -> HidD_GetAttributes            -> VendorID / ProductID
  -> HidD_GetPreparsedData + HidP_GetCaps -> UsagePage / Usage
  -> HidD_GetSerialNumberString
```

This is where the plan's selection rule gets implemented: filter to
`VID 051D`/`PID 0002` **and** `UsagePage 0x84`/`Usage 0x04`, match a configured
serial where one is set, and **fail closed on ambiguity**.

Two sharp edges, both classic:

- `SP_DEVICE_INTERFACE_DETAIL_DATA_W.cbSize` must be
  `size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()` — **8 on x64**, not the size
  of the buffer you allocated. Getting this wrong fails with
  `ERROR_INVALID_USER_BUFFER` and reads like a buffer-size problem.
- The struct is a variable-length trailing string. Allocate a byte buffer, write
  `cbSize` into its head, and read `DevicePath` from the offset — do not treat it
  as a fixed-size value type.

## The snapshot architecture

One rule, and it resolves several review findings at once:

> **No HID call ever happens on the UI thread.**

A single **device thread** owns the handle. It parks on the overlapped read, and
on a bounded cadence also sweeps the feature-only fields (load, voltages, rated
power). It publishes an immutable `Snapshot` behind a `Mutex`, with a version
counter and a **per-field timestamp**.

- The **menu** reads the last `Snapshot`. Never blocks, never stale by more than
  the sweep interval, and a wedged device cannot hang the menu — which the draft's
  "read fresh on every menu open" would have done, for exactly the reason jdrgb
  grew a worker thread in the first place.
- The **icon** updates from the same snapshot on transition.
- **Per-field freshness matters.** "Some report arrived recently" is not
  liveness: report 22 can keep arriving while runtime goes stale. Every field
  carries its own observation time and consumers decide what staleness means to
  them.

Teardown is explicit: set a closing flag, `CancelIoEx` the pending read, **join
the thread**, and only then destroy the window and app state. A detached thread
posting to a destroyed `HWND` is a use-after-free, and a `WM_APP` message
carrying a heap pointer leaks if the queue is discarded. Prefer posting a
payload-free "snapshot changed" notification and letting the UI read shared
state — no ownership to get wrong.

## Phase 1 — device layer and a console readout

Build the boring half first, and make it observable before there is any UI to
debug through.

- `hid/raw.rs`: open, overlapped read with deadline, bounded feature get/set.
- **Select the device properly.** Not "first match on VID/PID": enumerate,
  filter on `UsagePage 0x84` / `Usage 0x04`, and match a configured serial where
  one is set. **Fail closed on ambiguity.** Two UPSes, or a second top-level
  collection on the same IDs, must never silently resolve to a coin flip — this
  matters most for the Phase 8 writes.
- Validate the returned report ID and length on **every** read. A short read or a
  mismatched ID is an error, not something to decode.
- `hid/caps.rs`: the caps walk, and `HidP_GetUsages` for `PresentStatus`.
- `decode.rs`: one function per field.
- `jdups.exe --once` — `AttachConsole(ATTACH_PARENT_PROCESS)`, print the full
  readout, exit. The debugging surface for every later phase.
- `jdups.exe --probe` — dump all 63 feature caps, 9 input value caps and 11
  button caps, with the 72-byte stride warning applied. Earns its place exactly
  as `jdrgb probe` does.
- `jdups.exe --watch` — stream decoded input reports. Wants to exist before you
  need it.

**Exit criteria:** `--once` agrees with the PowerChute web UI on all six numbers;
`--watch` shows the report cycle and decodes `PresentStatus` flags by name.

### What Phase 1 turned up — **built**

Done. 31 unit tests, clippy clean, **179,712-byte** release binary.

1. **`IOCTL_HID_GET_FEATURE` is `METHOD_OUT_DIRECT` (`0x000B0192`)**, not
   `METHOD_BUFFERED` as first written — that returns a flat
   `ERROR_INVALID_FUNCTION` naming none of the four encodings.

   The part worth keeping: **`METHOD_IN_DIRECT` does not fail.** It returns a
   well-formed `[0C, 00, 00, 00, 00]`, which decodes cleanly as **0 % charge**.
   A wrong constant here would not have errored — it would have reported a flat
   battery, forever, convincingly. That is exactly the failure mode this project
   cannot tolerate, so `--probe` re-derives the encoding against the live device
   on every run and refuses to pass if the bounded path disagrees with
   `HidD_GetFeature`.
2. **Do not truncate a feature report to the returned byte count.**
   `DeviceIoControl` reports only the bytes the device actually wrote, so report
   12 comes back as 4 bytes rather than 5; `HidD_GetFeature` zero-fills to the
   full length. Truncating silently breaks every field whose top byte is zero —
   which is most of them.
3. **`HDEVINFO` is an `isize`, not a `HANDLE`**, so it cannot be compared
   against `INVALID_HANDLE_VALUE`.
4. **The serial is space-padded** by the device, so it is trimmed before display.
   It is otherwise shown in full. An earlier pass masked it, which was
   over-cautious — a UPS serial is not a credential, and a diagnostic tool that
   will not say which unit it selected is actively unhelpful the moment there are
   two. The real rule is about not committing the serial to a public repo, which
   governs what goes in `docs/` and was never addressed by hiding it at runtime.
5. The `SP_DEVICE_INTERFACE_DETAIL_DATA_W` `cbSize`-is-8-not-the-buffer-size
   warning was correct and cost nothing, because it was written down first.

## Phase 1.5 — the plug-pull — **done**

Captured in `docs/plug-pull.txt`. Every question it existed to answer is
answered, and the log schema is confirmed correct rather than merely plausible.

**The voltage mapping holds.** Report 49 is mains and report 9 is the battery,
which was only ever verified on mains where neither is doing anything
interesting. On battery, report 49 read **0 V** and report 9 sagged from 27.26 V
to **24.67 V**; on restoration report 49 jumped to 119 V and report 9 climbed
back through 25.70 → 25.88 → 26.05 V as the charger engaged. So the log's
`input_v` and `battery_v` columns are labelled right. This was the one real risk
in having built Phase 6 ahead of this gate.

**Report 19 pushes on change, exactly as the input caps implied** — it appeared
precisely once in a four-minute capture, at the moment mains returned, among 122
report-12s and 56 report-22s. On-change, not periodic, confirmed.

**`FF86:52` reads 8** for a plug-pull, consistently across two separate events.
One code, one cause; the rest of the 0–13 range still wants decoding against
NUT, and only a different kind of fault will produce them.

**The transfer sag lands entirely inside the first seconds.** The first sampled
row after transfer already read 78 %, and over the following three minutes of
genuine discharge the estimate fell only 78 → 76 % while runtime drifted
*upward* relative to elapsed time — 2078 s down to 1973 s across 183 s of real
time, i.e. the estimate correcting itself upward as the reading settled. That is
direct support for the settle window in `policy.rs`: a threshold evaluated in
the first thirty seconds is evaluated on the worst number the device will
produce.

**The event path works on real hardware.** The sampler wrote an `online` row
carrying `transfer=8`, with `flags` moving `discharging` → `ac` across the
transition. That code had never seen a real power event.

## Phase 1.5 — original plan, kept for the record

Ten minutes, and it settles the remaining unknowns before the log schema or the
notification logic is fixed. `--watch` redirected to a file; pull the plug for
about thirty seconds.

1. **Which voltage report is which.** Reports 9 and 38 both read 2726 and 49
   reads 117. They only diverge on battery. Confirm before the log commits to a
   column meaning.
2. **`FF86:52` last transfer reason**, reports 0–13, currently 0. Cross-check
   against NUT's `apc-hid.c`.
3. **The `PresentStatus` bit layout**, confirmed live against `HidP_GetUsages`
   rather than assumed.
4. **Whether reports 19 and 20 actually push on transition**, as the caps imply.

Record the capture in `docs/`. Everything downstream assumes these answers.

### The charge estimate is a model, not a measurement

Found by pulling the plug for about ten seconds during Phase 4 testing, and it
matters more than anything else observed so far.

**What happened:** charge fell 100 % → 80 % within ten seconds, runtime 43 → 31
minutes, and the countdown visibly raced. On restoring mains, charge sat at
**78 % and did not move for two minutes**, while battery voltage read
**27.43 V** — its normal resting figure, fully recovered.

**It is not energy.** At the observed 180 W (20 % of 900 W), ten seconds is
0.5 Wh. Runtime of 43 min at 180 W implies roughly 130 Wh usable, so the actual
draw was about **0.4 %** of the pack against a reported drop of **22 points** —
off by a factor of fifty.

**Nor is it voltage sag**, which was the first explanation and the data killed
it. Sag would recover with the terminal voltage, in seconds. Voltage was back to
27.43 V while the estimate stayed pinned at 78 %.

So APC's `RemainingCapacity` is a **modelled state**: it is decremented on any
discharge event and restored only over a recharge cycle, which for sealed
lead-acid is hours. `RunTimeToEmpty` is derived from it and inherits the lag
wholesale.

Consequences, in order of how much they matter:

1. **The agent must never act on a number sampled during or shortly after a
   transfer.** Both fields are at their least trustworthy in exactly the window
   the agent cares about. Debounce over monotonic time, and additionally ignore
   the first several seconds after `ACPresent` drops.
2. **Repeated short outages can ratchet the estimate down.** Three brief cuts in
   an hour could leave charge reading far below the pack's real state, because
   each one decrements the model and the recharge has not caught up. An agent
   thresholding on charge alone would eventually shut the machine down over
   nothing. This is a second, independent argument for `ShutdownImminent` being
   authoritative and runtime being primary.
3. **The log gets a genuinely good health metric for free:** the *size* of the
   drop per unit of energy actually drawn. A healthy pack should revise its
   estimate less. Tracking that per outage over months is a better wear signal
   than runtime-at-load, and it costs nothing extra to record.
4. Displayed values get a median (`Smoother`), which removes the quantisation
   cycle and stops the icon flickering through intermediates. It deliberately
   does **not** try to hide the drop — that is a real reading.

Also confirmed by the same event: **`FF86:52` last transfer reason moved from 0
to 8**, so the usage is live and worth decoding against NUT's `apc-hid.c` rather
than guessed at. And `PresentStatus` gained its `Charging` bit (`0x0C` → `0x0D`),
which is the flag decode proving itself against a real state change.

## Phase 2 — tray shell

Lift jdrgb's `src/tray/main.rs` structure; every sharp edge in it was paid for
once already. Keep unchanged: `SetProcessDpiAwarenessContext` first in `main`; a
real never-shown top-level window (**not** `HWND_MESSAGE`, which misses the
`TaskbarCreated` broadcast); the `NIM_ADD` retry loop; `NIM_SETVERSION` to
`NOTIFYICON_VERSION_4` after **every** successful add; `TaskbarCreated`
handling; the named-mutex singleton; guarded uxtheme ordinals 135/136 for dark
menus; `set_field`'s char-boundary and surrogate-pair handling;
`Shell_NotifyIconGetRect` for the icon's real DPI.

**One thing to port with a fix, not verbatim.** jdrgb calls `NIM_DELETE` on
every `WM_ENDSESSION` (`src/tray/main.rs:847`). `wParam == FALSE` means the
session is *not* ending — a shutdown someone cancelled — and jdrgb then runs on
with no icon until Explorer restarts. Tear down only when `wParam != 0`.

## Phase 3 — the icon

The one genuinely new piece of drawing work, and the one thing here that cannot
be settled on paper.

### What it shows

A horizontal battery: constant 1 px outline, opaque interior, fill proportional
to charge growing from the left.

**The outline never changes width.** jdrgb's hardest-won lesson: an outline is
carved out of the shape's own area, so anything varying the border makes the
icon appear to change size. The fill is the only thing that moves.

**The interior is opaque, not transparent.** Alpha means "composites correctly",
not "visible" — also a jdrgb lesson. A definite dark neutral behind the fill
makes the gauge readable on any taskbar and gives the fill boundary defined
contrast.

| State | Fill |
|---|---|
| On mains | green |
| On battery | amber |
| On battery, past the warning threshold | red |
| Device lost / `CommunicationLost` | grey, hollow |

That last row matters: an unattended tray that silently stopped reporting is
worse than one that says so.

### The digits

Rendered into the gauge interior, but only when there is something to say:

> **Show digits unless the UPS is on mains and charge ≥ 99 %.**

This unit sits at 100 % on mains essentially always, and a permanent "100" is
chartjunk on a 16 px canvas. It also covers "full, even if it doesn't quite say
so" — a unit resting at 99 % is not information.

**Which number: always minutes.** One quantity, in every state.

A first version switched — charge on mains, minutes on battery — on the theory
that fill colour distinguished the contexts well enough. It does not, and the
switch was wrong for two reasons:

- **Ambiguous.** The ranges overlap. "31" is a believable percentage *and* a
  believable number of minutes, and a colour change does not announce that the
  units just moved. A glance device with a units question in it is defective.
- **Redundant**, which is what actually settles it. **The fill bar already is the
  charge percentage.** Spending the digits on charge too said one thing twice and
  the other thing never.

So the icon carries two quantities on two channels: **fill for charge, digits for
time.** Minutes is also the number that survives the load question — 20 % is half
an hour at light load and ninety seconds at heavy — and on mains it reads as
"how long could I survive right now", which is what a UPS is for.

Nothing at all when the UPS is resting full on mains, which is almost always: a
permanent number is chartjunk on a 16 px canvas.

A property worth stating: **the icon never needs three digits.** Minutes cap at
99, above which the answer is "you are fine" and the exact figure is in the
tooltip. Two digits is the whole design space.

### Making them legible

Two hand-rolled pixel fonts, `FONT_3X5` and `FONT_5X7`, picked **by what fits the
interior** rather than by canvas size.

The first version keyed the choice off the canvas — small font below 20 px — a
rule written before the car battery shape gave the interior more room. It ended
up drawing 3×5 glyphs into a 14×11 space at 16 px, using barely a third of the
available area, and the result read as faint. Fit-driven selection picks the
tallest layout that fits and gets a bold 5×7 at 16 px instead.

Three things that turned out to matter, all found by rendering and looking:

- **Bold is free.** Drawing each glyph pixel one device pixel larger than its
  cell thickens every stroke without a second set of hand-drawn glyphs.
- **The advance has to include the bold growth.** It did not, so the gap closed
  to nothing at 16 px and "22" rendered as one blob. There is now a test that
  adjacent glyphs cannot touch.
- **Zero must be plain, not slashed.** The diagonal is fine at size; bolded at
  16 px it meets both walls and fills the counter, and the glyph becomes a lump.

Horizontal clearance is one pixel total rather than one per side. At 16 px the
bold 5×7 misses a two-pixel budget by exactly one column, and 16 px is both the
most common size and the one that most needs the weight.

**No unit suffix.** An "m" would settle any doubt that the digits are minutes,
but at 16 px the glyphs already occupy 13 of the interior's 14 columns, so it
does not fit — and adding it only above 16 px would make the icon inconsistent
across DPI. The ambiguity it would have solved is gone anyway now that the digits
never switch quantity.

```rust
// FONT_3X5, one u8 per row, low 3 bits, top row first.
const FONT_3X5: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];
```

Contrast is the real problem: a digit sits partly over fill and partly over empty
interior, and any single colour is unreadable against one of them. So digits are
a **knockout** — each digit pixel takes the other region's colour:

```rust
// Guaranteed contrast on both sides of the fill boundary, with no per-colour
// tuning and nothing to get wrong when the fill colour changes.
let px = if digit_mask {
    if inside_fill { INTERIOR_BG } else { fill_colour }
} else if inside_fill { fill_colour } else { INTERIOR_BG };
```

Everything stays premultiplied BGRA; jdrgb's "no channel may exceed alpha" test
ports unchanged.

**Cache on `(state, dpi, size)`, not state alone.** jdrgb's `refresh_icon`
early-returns when the swatch is unchanged, which means dragging the taskbar to a
different-DPI monitor keeps the old bitmap. Same bug would land here by
inheritance.

### How the layout actually gets decided

At 16 px the interior is roughly 11 × 10 after the outline; two 3×5 digits plus a
gap is 7 × 5. It fits arithmetically. **Whether it is legible on a taskbar is not
something this document can determine**, and the honest history is jdrgb's rims:
four passes of reasoning, and the actual problem was only found by looking.

So Phase 3 ships jdrgb's `contact_sheet` test first, adapted:

```
cargo test --bin jdups -- --ignored contact_sheet
```

Every DPI size (16/20/24/28/32/40) across the state matrix — charging, on
battery, each threshold, device lost, digits on and off, 0/9/47/98 — composited
over light and dark taskbar backgrounds, into one BMP. Then you look at it.

If two digits inside the outline are unreadable at 16 px, the fallback is already
identified: **drop the outline when digits are shown**, letting them occupy
nearly the full canvas with a 2 px charge bar along the bottom. More legible,
gives up the gauge only in the rare state. A five-minute look decides it.

Machine-checkable invariants alongside: premultiplication at every size; the
outline occupies identical pixels regardless of state; fill width monotonic in
charge; digits never overlap the outline; `icon_digits` returns `None` exactly on
the mains-and-≥99 % case.

### What Phases 2–4 turned up — **built**

Done. 48 tests, clippy clean. `jdups.exe` 179 KB, `jdups-tray.exe` 183 KB.

1. **Two binaries after all.** This plan said one, on the reasoning that "there
   is no pre-existing CLI to preserve here". By the time the tray was written
   that was false — `--once` was in daily use. Under
   `#![windows_subsystem = "windows"]` the CLI produced **no output and no exit
   code**: a windows-subsystem process has no stdout from the calling shell, and
   the shell does not wait for it, so `$LASTEXITCODE` came back empty.
   `AttachConsole` + `SetStdHandle` fixes the output but not the waiting, and
   `--probe`'s non-zero exit *is* its safety signal. So: `jdups.exe` (console)
   and `jdups-tray.exe` (windows), sharing the lib — jdrgb's shape, for jdrgb's
   reason.
2. **The digit knockout failed.** Each digit pixel taking the other region's
   colour is elegant, passes every arithmetic test, and is unreadable: where the
   fill boundary crosses a glyph it splits it into two half-digits, and "47" at
   47 % charge stops being a number. **Found by generating the contact sheet and
   looking at it** — no amount of reasoning was going to catch it, which is
   exactly why that test exists.

   The fix was not the identified fallback (drop the outline, digits fill the
   canvas). The knockout was only necessary because a near-black interior and a
   mid-bright fill sit at opposite luminances. **Lifting the empty interior to
   mid grey** lets one flat near-black digit clear 4.5:1 on green, amber, red and
   grey alike, so a glyph crossing the boundary stays one glyph. The gauge
   survives intact.
3. **The tint had to move to the outline.** Second thing the contact sheet
   showed: fill visibility is proportional to charge, so a critical UPS at 20 %
   rendered as a mostly-grey box with a thin red sliver — the most urgent state
   drawn as the least visible. Colouring the outline puts the signal at full
   strength at any level. This is *not* jdrgb's rim problem returning: that was
   about outline **width** being carved out of the fill; width here is constant
   across every state and only the hue moves.
4. **Windows 11 hides new tray icons by default.** `Shell_NotifyIconGetRect`
   returns the *chevron's* rect when an icon is in the overflow, which is a
   useful way to detect it. Worth a line in the install notes rather than a bug
   report.

## Phase 4 — menu

```
  ▮ Online, 100%, 43 min
  ─────────────
  Load               20%  (180 W)
  Input             117 V
  Battery         27.26 V
  Battery installed  2021-11-23
  ─────────────
  Open log
  Open PowerChute
  ─────────────
  Exit
```

Rendered from the last `Snapshot` — no HID on the UI thread. If the snapshot is
stale or the device is lost, the status row says so rather than showing numbers
that have quietly stopped being true.

**Every data row is enabled, and clicking any copies the whole readout** as one
multi-line block. This resolves the dilemma the investigation flags: a disabled
Win32 row renders dim and reads as broken, and owner-draw would forfeit the
Windows 11 rounded menu styling. A real action lets rows be enabled honestly, and
one behaviour across all four means nothing to learn and nothing to mis-click.

Clipboard, in the order that actually works:

- `OpenClipboard(hwnd)` with the **real window handle**, not `NULL`.
  `EmptyClipboard` assigns ownership to the opener, and with a `NULL` owner the
  subsequent `SetClipboardData` fails.
- Retry briefly on contention — another process holding the clipboard is
  ordinary, not exceptional.
- `GlobalAlloc(GMEM_MOVEABLE)`, `GlobalLock`, copy NUL-terminated UTF-16,
  `GlobalUnlock`.
- On success the clipboard **owns** the handle — do not free it. On failure you
  still own it and must `GlobalFree`.
- `CloseClipboard` on every path where the open succeeded.

Other rows: `MIIM_BITMAP` for any menu bitmap, never owner-draw, same styling
reason as jdrgb. **Open log** hands the current month's file to `ShellExecuteW`.
**Open PowerChute** shows `https://localhost:6547` only when the service is
present — detected by service presence, not by probing the port, which would cost
a timeout on every menu open.

### Opening the log

"Open log" appears only when a log exists. The tray cannot know which install
shape is in use, so it looks in both `%ProgramData%\jdups` and
`%LOCALAPPDATA%\jdups` and takes the most recently *modified* file — not the
latest month by name, since if both shapes have been used the one still being
appended to is the one you want.

It opens with the **`.txt`** handler, not the `.csv` one. The log is `.csv`
because that is what it is and because charting decay is why it exists, but the
shell's `open` verb for `.csv` is Excel, which is a heavy way to glance at a
file. Resolving the `.txt` association via `AssocQueryStringW` still respects
*your* editor rather than hardcoding one — the plan is explicit about not
bundling or naming a viewer — it just asks the association system a more useful
question. Measured on this machine: `.txt` resolves to Sublime Text, `.csv` to
Excel. Falls back to the plain `open` verb if the lookup fails.

`jdups --log` prints the path the tray would open, or lists where it looked.

## Phase 5 — transitions and notifications

The device thread already holds the stream. This phase is what it does with it.

- Dispatch on `buf[0]`. Reports 12 (charge/runtime), 6 (charging/discharging),
  19 (`ACPresent`), 20 (`ShutdownImminent`), 22 (`PresentStatus`), 33 (test).
- Compare against last-known state; act **only on transitions**.
- On mains loss and restoration, raise a balloon. Balloons can be suppressed by
  Focus Assist, so the icon must independently reflect reality and never depend
  on one having been seen.
- **Reconnection.** The read fails when the device is unplugged or the USB stack
  resets. Retry on a backoff, show the device-lost icon, and **clear per-field
  state on reconnect** so nothing carries a pre-disconnect value forward.
- A 60 s backstop timer for the feature-only fields.

## Phase 6 — logging and the sampler

**The sampler owns the log; the tray never writes it.** One writer, no dedup, no
interleaving, continuous whether or not anyone is logged in.

`jdups.exe --sample` runs continuously under a scheduled task, with its own
singleton (`Global\jdups-sampler-singleton` — `Global\` because it runs as SYSTEM
while the tray's is per-session).

### Which columns are medians, and which are not

The draft said "median every field", which is not possible: **load and the
voltages are not on the input stream at all** — reports 6/12/19/20/22 do not
carry them. Only what the sampler actually observes can be aggregated.

| Column | Source | Aggregation |
|---|---|---|
| `charge`, `runtime_s` | input report 12, many per interval | **median** |
| `load_pct`, `input_v`, `battery_v` | feature sweep on a fixed cadence | **median of sweeps** |
| `watts` | derived from the median load | computed after |
| `ac`, `status` | `PresentStatus` | state at interval close |
| `n` | — | sample count |

Medians are taken **per column over the same interval**, which is honest so long
as the schema says so — a row is a summary of a window, not an instant that
occurred. `n` makes a thin interval visible instead of silently equal-weight.

### What gets an event row

Everything PowerChute's own event log records, plus what it does not.

| jdups `event` | PowerChute equivalent | `detail` |
|---|---|---|
| `started` / `stopped` | Monitoring Started / Stopped | |
| `device-found` / `device-lost` | Communication Established / Lost | |
| `onbattery` / `online` | On Battery / No Longer On Battery | `transfer=<code>` |
| `selftest` | *(not in its event log)* | `result=passed\|warning\|error\|...` |
| `status` | *(not in its event log)* | —, with `flags` |

A first pass logged **only** the mains transition, which threw away nine of the
eleven `PresentStatus` flags. An overload, comms dropping, or mains drifting out
of regulation left no trace at all. Now any change to the *notable* flag set
writes a row, where notable excludes `charging` (it toggles constantly during
float-charging and every row would look like an event) and `battery_present`
(true forever on a UPS with its battery in).

**Self-tests come for free.** Report 33 is pushed on change, so a monthly
self-test result arrives without being polled for — and "did the last self-test
pass" is precisely the question a log kept over months exists to answer. The
result names come from NUT's `apc-hid.c`; the one point confirmed here is the
resting value, 6 = no test initiated. An unrecognised code renders as `code-N`
rather than being forced into a name.

**The transfer *reason* is feature-only.** `FF86:52` never appears on the input
stream, so it has to be read at the moment of a transition or it is lost. It
moved 0 → 8 during a real outage here, so the usage is live; the code table
still wants decoding against NUT.

### Interval discipline

- **A state transition closes the current interval and starts a new one.**
  Otherwise a five-minute window straddling an outage medians mains and battery
  samples together and reports neither.
- Write a marked row immediately on transition.
- Emit explicit **gap rows** when the device was lost or no samples arrived, so a
  hole in the data is visible rather than inferred from timestamps.
- Drive intervals off a **monotonic** clock; wall time is only for the timestamp
  column and the monthly filename.

```
# jdups log v1  interval=300s  medians per interval; transitions close the interval
timestamp,charge,runtime_s,load_pct,watts,input_v,battery_v,ac,n,event
2026-07-31T15:04:22-06:00,100,2595,20,180,117,27.26,1,58,
2026-07-31T15:09:22-06:00,100,2632,20,180,117,27.26,1,57,
2026-07-31T15:11:07-06:00,100,2508,21,189,0,27.10,0,19,onbattery
2026-07-31T15:16:07-06:00,,,,,,,,0,device-lost
```

Monthly files, `jdups-YYYY-MM.csv`, in `%ProgramData%\jdups\`. Watts is
`PercentLoad × ConfigActivePower / 100` on the device's own 900 W, deliberately
not PowerChute's implied 1050 W.

`.csv` rather than `.log` is a real trade-off: the shell hands it to Excel rather
than a text editor, which is better for the charting this log exists to enable
and worse for a quick look. One-line change if it grates.

### The directory is a privileged write target

"SYSTEM writes, Users read" is too vague for a SYSTEM process. A SYSTEM writer
following a path a normal user can influence is an elevation-of-privilege bug.

- The **elevated installer** creates and owns the directory.
- **Reject reparse points** on the directory and on any pre-existing monthly
  file, and refuse to run rather than follow one.
- Explicit ACL, inheritance disabled: SYSTEM and Administrators full, Users
  read/execute. No user-writable component anywhere on the path.
- Open with sharing that permits the tray to read, flush every row, `sync_data`
  transition rows, and flush on orderly stop.

## Phase 7 — install

**Elevation is about the install shape, not the program.** Nothing jdups does at
runtime needs administrator rights — it reads HID feature reports and draws an
icon. What needs elevation is Program Files, a shared log directory a SYSTEM
writer can be trusted with, and a task that runs before anyone signs in.

So `install.ps1` has two modes:

| | machine-wide | `-PerUser` |
|---|---|---|
| Elevation | yes | **none** |
| Binaries | `%ProgramFiles%\jdups` | `%LOCALAPPDATA%\Programs\jdups` |
| Log | `%ProgramData%\jdups`, explicit ACL | `%LOCALAPPDATA%\jdups` |
| Tray | logon task, as you | identical |
| Sampler | **SYSTEM, at startup** | as you, at logon |

The only thing `-PerUser` gives up is continuity: its sampler runs at logon, so
the log gains a gap whenever nobody is signed in. For "how bad is my power" that
is usually fine — the machine is on when you care. The runtime-decay series is
what suffers, so the installer says so rather than burying it.

`-TrayOnly` implies `-PerUser`, because a tray-only install touches nothing
outside the profile and asking for a UAC prompt would be theatre.

Rejected for the per-user sampler: a "run whether logged on or not" task, which
would close the gap without elevation but requires storing the user's password.
An honest hole in the data is the better trade.


Model on jdrgb's `install.ps1`. Install to `%ProgramFiles%\jdups`, admin-only
writable by design.

- **Tray task:** `-AtLogOn -User <sid>`, `RunLevel Limited`, Interactive.
- **Sampler task:** `-AtStartup`, SYSTEM.
- **Set the settings that default to failure.** Task Scheduler defaults
  `DisallowStartIfOnBatteries` **true** and `StopIfGoingOnBatteries` **true**.
  Harmless today because Windows does not see this UPS as a battery — and fatal
  the moment anyone tries the investigation's Option A driver rebind, which makes
  it one. The irony of a UPS monitor that refuses to run on battery is worth one
  explicit line. Also set `-ExecutionTimeLimit 0`, restart count/interval, and
  `MultipleInstances = IgnoreNew`.
- Capture the invoking user's SID **before** self-elevating. After
  `Start-Process -Verb RunAs`, `$env:USERNAME` is whoever answered the UAC prompt.
- Every switch must appear in the self-elevation argument reconstruction or it is
  silently dropped on relaunch. This bit jdrgb once.
- Start both at install time so nothing waits for a logoff.
- `uninstall.ps1`: unregister both, stop the tray **by full image path** (a bare
  `Stop-Process -Name jdups` would kill a `target\release` development copy), and
  leave `%ProgramData%\jdups` alone — the log is data, not an artifact.

## Phase 8 — the shutdown agent

**Gated.** Do not start until the readout has run for weeks and the log looks
right. Do not disarm PowerChute until this is trusted; getting it wrong is
discovered during an outage.

### It has to be a service, not a scheduled task

A scheduled task cannot receive `SERVICE_CONTROL_PRESHUTDOWN` or power
notifications, and both matter here. As a service it can:

- Register for power events and **know when the machine suspends**. A scheduled
  process is simply frozen through S3 while the UPS keeps draining; output loss
  then destroys RAM state with nothing having had a chance to act.
- **Hold off idle sleep while on battery** (`SetThreadExecutionState` with
  `ES_SYSTEM_REQUIRED | ES_CONTINUOUS`), releasing it on mains return.
- Take preshutdown notification so it can stand down cleanly during an ordinary
  reboot rather than racing it.

If the agent is installed, it can also subsume the sampler — it already holds the
stream continuously. Worth doing then, not before.

### The decision is a pure function — **built**

`src/policy.rs`, with 15 tests. Nothing in it acts: no device writes, no
`InitiateSystemShutdownExW`, no clock, no I/O. It folds observations into a
decision, which means the state space can be walked exhaustively long before
anything is wired up to obey it.

What the hardware findings forced into its shape:

- **A settle window** after mains is lost, during which thresholds are ignored.
  The charge model collapses on transfer and corrects itself over the following
  minutes, so acting inside it means acting on a number that is about to be
  wrong.
- **Latched outage state.** Silence never clears it; only a *freshly confirmed*
  mains return, sustained past the debounce, does. A brief flicker cannot reset
  the clock.
- **Asymmetric staleness.** Before an outage, unknown means do nothing. During
  one, losing the device is not permission to relax — that is exactly the case
  where doing nothing runs the battery flat.
- **A hard deadline** that is checked *before* staleness, so a device that goes
  quiet mid-outage still gets shut down rather than riding to exhaustion.
- **`ShutdownImminent` bypasses everything**, including the settle window.
- **Thresholds are OR, not AND.** Requiring both to agree fails open whenever
  one is unreadable.
- **`saturating_sub` throughout**, so a clock going backwards cannot manufacture
  an elapsed deadline. There is a test that drives time in reverse.
- **`Config::validate`** refuses values that would make the agent dangerous,
  because a SYSTEM process that accepts any threshold it finds in a file is
  shutdown-as-a-service.

### The dry run — **built**

`jdups-agent.exe`, a console binary, three modules and no new dependency:

| | |
|---|---|
| `src/config.rs` | the settings file, treated as a privilege boundary |
| `src/agent/journal.rs` | what gets written, and when. Pure, 11 tests |
| `src/agent/watch.rs` | the device loop feeding `policy` |
| `src/agent/log.rs` | `jdups-agent-YYYY-MM.log`, prose not CSV |

It decides and it logs. **`armed = true` is refused at startup with a non-zero
exit**, because the shutdown transaction below does not exist, and an agent that
quietly ran dry while its config said armed would be the worst outcome available
here: somebody believing their machine is protected when it is not.

What that split buys is the cheapest evidence in the project. Thresholds chosen
on a bench are guesses. Point this at the real UPS, leave it for weeks, and the
log says what it *would* have done against this machine's actual power — before
anything is allowed to act on it.

Four decisions worth recording:

- **A malformed config is a hard error, never a fallback to defaults.** An agent
  whose thresholds do not match the file in front of you cannot be reasoned
  about. Unknown keys included: `runtime_threshhold_s` misspelt would otherwise
  be accepted in silence while the real threshold stayed at its default.
- **The reason comes out of `policy`, not out of the caller.** `Action::Shutdown`
  now carries a `Why`. A log that says *that* it would have shut down but not
  *why* cannot be used to tune anything, and a caller re-deriving the reason from
  the same observation is free to disagree with the decision it is describing.
- **A heartbeat while on battery, and silence on mains.** Logging every tick
  makes a file nobody reads; logging only transitions leaves a twenty-minute
  outage as two lines with no discharge curve between them. There is a test that
  a machine sitting on mains produces literally nothing.
- **The agent keeps deciding while the UPS is unreachable.** The disconnected
  path still drives `policy` on its own cadence rather than sitting in a retry
  loop, so the latch holds and the backstop still fires. This is the failure the
  first draft of `policy` had backwards, and it is easy to reintroduce in the
  loop after fixing it in the decision.

A scheduled task rather than a service, for now — `install.ps1 -Agent`. Legitimate
precisely because it cannot act: what a service buys is preshutdown notification
and knowing when the machine suspends, and neither matters until something is
armed.

**Not yet seen an outage.** It has run against the live UPS on mains, found the
device, and correctly written nothing. Every path from `Warn` onward is covered
by tests and by nothing else.

### It is not where most of the risk is

```rust
pub fn decide(state: &State, history: &History, cfg: &Config) -> Action
```

`Action` is `Nothing`, `Warn`, or `Shutdown`. No I/O, no clock, no device;
exhaustively testable. But the draft claimed "all the risk lives here", and that
is false. Most catastrophic failures are *outside* it: a partial UPS write, a
readback that never happened, privilege failure, a crash between arming the UPS
and the OS going down, mains flapping, PowerChute racing. Those need a
fault-injected state-machine test, not a table test.

Decision rules:

- **`ShutdownImminent` (report 20) fires immediately, undebounced.** The device
  is authoritative about its own output; no threshold beats it.
- **Otherwise threshold on runtime, not percentage.** This is the whole argument
  for writing an agent rather than rebinding the inbox driver: `RunTimeToEmpty`
  folds load in and the device computes it, and Windows cannot act on it.
- Trigger is `runtime <= threshold OR charge <= emergency_floor` — either alone
  is sufficient, because requiring both fails open.
- **Debounce over monotonic time and fresh report-12 observations**, not "N
  samples" — the draft never said which report advanced the counter or how long
  that represented. Runtime jitters ±90 s (finding above), so a naive consecutive
  count can postpone indefinitely while the value oscillates across the line.
- **Latch a confirmed outage.** Once `ACPresent = 0` is confirmed, hold it, with
  a conservative monotonic deadline derived from the last good runtime and a
  configured maximum-on-battery. Clear only on sustained, freshly confirmed mains
  return with hysteresis.
- **Staleness is asymmetric.** *Before* an outage, unknown means do nothing.
  *During* one, losing the device must not reset to `Nothing` — that is precisely
  the case where the draft failed open and would have run the battery flat. Fall
  back to the latched deadline.
- Size the threshold from a **measured** worst-case shutdown time on this
  machine, not the draft's unexplained 300 s.

### The shutdown is a transaction, not two calls

Arming the UPS cutoff and asking Windows to shut down are not atomic, and the
draft treated them as if they were. If the UPS timer is armed and the shutdown
then fails, is vetoed, or hangs, the UPS hard-cuts a live filesystem.

Ordered, with a persisted intent record so a crash mid-sequence is recoverable:

1. Enable `SE_SHUTDOWN_NAME` explicitly. SYSTEM holds it but **not enabled**, and
   the failure is silent. Check `AdjustTokenPrivileges` for
   `ERROR_NOT_ALL_ASSIGNED` — it returns success even when it changed nothing.
2. Record and flush intent to disk.
3. Request the OS shutdown **first**, and confirm acceptance. A successful return
   means only that Windows accepted an asynchronous request.
4. Arm the UPS cutoff only after that, with a delay exceeding measured worst-case
   shutdown **plus hibernation-file write** — Fast Startup means an ordinary
   shutdown may write a hiberfile, and cutting power mid-write is how you get a
   corrupt resume.
5. Read back every write. A write that did not take must not be assumed.
6. On any failure: cancel the UPS countdown, verify the cancel, and log loudly.
7. On restart, read the persisted intent and any pending countdown, and reconcile.
   **Never blindly restore −1** — that could cancel a countdown that is currently
   the only thing that will restore power.

**Force policy, stated rather than defaulted:** `bForceAppsClosed = true`.
`false` can wait indefinitely on one modal dialog, and an indefinite wait here
means the battery decides instead. Losing unsaved work in an outage is the
better failure. Pair it with a warning period and a real shutdown reason code.

### Configuration is a privilege boundary

A SYSTEM process reading thresholds from a user-writable file is
shutdown-as-a-service. Config lives beside the binary in
`%ProgramFiles%\jdups`, ACL'd SYSTEM/Administrators-write, and every value is
range-validated on load with conservative clamps. The agent **refuses armed mode**
if validation fails, if its singleton is already held, or if its interlock check
fails — it does not fall back to defaults and carry on.

It publishes a read-only heartbeat the tray displays, so "is the agent alive and
armed" is visible at a glance. Disarming PowerChute without that recreates
exactly the silent-failure risk the whole project is trying to avoid.

### Can PowerChute be replaced outright?

Asked properly, and answered from this device's own report descriptor rather
than from assumption. **Yes, with one genuine unknown and one feature that is
simply not written.**

Every writable report named below was confirmed present in a `--probe` walk.

| PowerChute does | jdups | What parity needs |
|---|---|---|
| Graceful shutdown on low battery | decides, inert | the transaction |
| Cut UPS output after shutdown | — | `DelayBeforeShutdown` `0084:57`, reports 21 and 66, `-1..32767` |
| Restart when mains returns | — | **the unknown, below** |
| Configurable thresholds | `jdups.conf` | done, and range-validated |
| Scheduled self-test | reads results | `Test` `0084:58`, report 33, `0..6` |
| Event log | CSV + prose log | done, and richer than the vendor's |
| Run a script before shutting down | — | small, and worth having |
| Email on event | — | genuinely new work |
| Web UI on `localhost:6547` | tray + CLI | deliberately not. This is the point |
| Sleep / hibernate / Fast Startup | — | the service |
| Audible alarm control | — | `AudibleAlarmControl` `0084:5A`, reports 24 and 120, `1..3` |
| Transfer voltage, sensitivity | reads them | `0084:53` / `0084:54`, reports 50 and 51 |
| Battery replacement warning | shows the install date | `NeedReplacement` `0085:48` is **not in the caps at all**, so PowerChute cannot be reading it either — it is computing from the date, and so can this |

**`DelayBeforeShutdown` is settled, from the vendor's own UI.** PowerChute's
Shutdown Settings page carries a summary that states the contract outright:

> After **0 Seconds** — Operating System Shutdown starts
> After **120 Seconds** — Outlet Group ... powering the PowerChute Agent turns off

...where 120 is the value of its "Time for operating system to shut down" field.
So the sequence is: arm report 21 with the number of seconds the OS is allowed,
*then* begin the shutdown, and the UPS cuts output when the countdown expires.
That is exactly the ordering the transaction below specifies, arrived at
independently, which is a good sign for both.

It also fixes the sizing question. The delay is not a guess to be tuned; it is
"how long this machine takes to shut down, plus the hibernation file write",
and PowerChute's own default for it here is 120 s.

**No restart delay appears anywhere in that UI.** PowerChute exposes shutdown
timing, OS shutdown type, and a command file, and nothing about coming back. An
implementation that has restarted this machine for years without exposing the
setting is unlikely to be writing it, which is real evidence — the negative
kind — that report 65 is not part of the handshake and the UPS restores output
by itself when mains returns.

### The shutdown mechanism, measured — **settled 2026-08-01**

A real PowerChute shutdown was watched register by register, on this unit, with
only the PC on the UPS. It answers the question the plan had been circling, and
it answers it differently than the plan guessed.

```
12:49:31  ACT   shutdown(21)=none  flag(64)=1  restart(65)=119s
12:49:34  ACT   shutdown(21)=none  flag(64)=1  restart(65)=116s
12:49:36  ACT   shutdown(21)=none  flag(64)=1  restart(65)=114s
                              ... counting down ...
12:49:52  ACT   shutdown(21)=none  flag(64)=1  restart(65)=98s
```

1. **Report 65 (`FF86:7D`) is the shutdown countdown.** PowerChute set it to 120
   — its "Time for operating system to shut down" — and the UPS decremented it in
   real time, cutting output at zero. It is not a restart delay. The plan had it
   labelled as one on the reasoning that its `-1..32767` range matched
   `DelayBeforeShutdown`; that was the right observation and the wrong
   conclusion. It matches because it *is* a shutdown delay, APC's own.
2. **Report 64 (`FF86:7C`) is the armed flag**, 0 → 1 for the duration.
3. **Report 21, the standard `DelayBeforeShutdown`, was never touched.** It read
   -1 through the entire sequence. Every plan and every note that said to write
   report 21 was wrong; write 65.
4. **There is no restart handshake.** Mains returned, the UPS restored output by
   itself, and both registers reset to 0 and -1 unaided. Nothing to write and
   nothing to configure — which is why no such setting exists in PowerChute's UI.
5. The machine itself did not power on, which is a BIOS "restore on AC power
   loss" setting and not something any UPS software controls.

**What is still unknown, and it is small:** whether report 64 is written or
merely reflects an armed countdown. The first sample already had it set, so the
order was never observed. Try writing 65 alone and see whether 64 follows.

**The last real unknown in the project is therefore closed**, and the shutdown
path is now a known sequence rather than a hypothesis.

**The restart handshake was the last unknown and there turns out not to be one.**
`DelayBeforeStartup` (`0084:56`) does not exist on this device and neither does
`DelayBeforeReboot` (`0084:55`) — and nothing needs them. The UPS restores its
own output when mains returns, observed directly. That is also why no such
setting appears anywhere in PowerChute's UI.

**What actually keeps PowerChute installed is not a feature.** It is that it has
been running for years and is known to work. Feature parity is the easy half;
the hard half is earning the same confidence, which is what the testing ladder
below is for.

### PowerChute handover

The draft contradicted itself: it kept PowerChute armed through live shutdown
tests while also saying two armed agents must not coexist. Both write
`DelayBeforeShutdown` globally, and the last writer wins.

Correct sequence: dry-run testing happens **with PowerChute armed** (jdups never
writes, so there is no conflict). Then a single controlled handover — verify
jdups health, stop and disable `APCPBEAgent`, arm jdups, roll back immediately if
arming fails. The agent enforces this interlock itself rather than trusting an
installation note.

### The testing ladder

1. ~~**Exhaustive unit tests** on `decide`, no hardware.~~ **Done**, 15 tests.
2. **Fault-injected state-machine tests** across every failure boundary and
   restart point.
3. ~~**Dry run** — logs what it would do, never calls shutdown. The default mode;
   arming is explicit.~~ **Built.** Run for weeks against real power. The trigger
   itself can be proven in fifteen seconds without draining anything, by giving
   it absurd thresholds and pulling the plug: see `docs/status.md`.
4. **The restart cycle, on a sacrificial load.** Confirm `FF86:7C`/`FF86:7D`
   against NUT before writing anything, then prove
   shutdown → mains return → restart end to end. This machine is not the test rig.
5. **Absurd thresholds** — "shut down below 95 % or 30 minutes" so a plug-pull
   fires in about ten seconds. No need to drain the battery to test the trigger;
   this is what makes live testing cheap.
6. **The power-state matrix:** S3, hibernate, Fast Startup on and off, reboot,
   Windows Update reboot, USB reset, and mains returning *during* shutdown.
7. **Realistic thresholds**, once, with nothing important running.
8. **Then** the PowerChute handover.

## Testing strategy

Nearly all of it runs with no UPS attached — the point of the layering.

| Layer | How |
|---|---|
| `decode.rs` | Golden vectors from this document: `0C 64 23 0A 00` → 100 %, 2595 s; `07 77 53 00 00` → 2021-11-23 |
| `model.rs` | Formatting, thresholds, `icon_digits` across the state space, staleness |
| `draw.rs` | Premultiplication, constant outline, monotonic fill, digit bounds, `(state,dpi,size)` cache key — plus the `contact_sheet` eyeball test |
| `logfile.rs` | Interval accumulator, transition-closes-interval, gap rows, monthly roll, CSV round-trip |
| `policy.rs` | Exhaustive table tests, plus latched-outage and staleness cases |
| `agent/txn.rs` | Fault injection at every step, and restart-recovery from a persisted intent |
| `hid/` | A scripted fake `Device`; the real one via `--once` and `--watch` |

Two things tests cannot cover, and both are named above rather than hidden:
whether the icon is readable (the contact sheet), and whether the restart
handshake works (the sacrificial-load rig).

One integration test worth building early: **a multi-process fan-out check.**
Run two jdups readers plus PowerChute for several minutes and compare report IDs,
payloads and counts. Windows HIDClass gives each open file object its own input
queue, and the 48-second capture is consistent with that — a full stream arrived
while PowerChute was actively reading. But that demonstrates coexistence, not the
absence of drops under load, and the sampler's `n` column depends on the
difference.

## Risks

- **The restart handshake may not be achievable** through documented usages;
  `DelayBeforeStartup` does not exist on this device. Fallback stated in
  Phase 8.4.
- **Digit legibility at 16 px** — mitigated by the contact sheet and an
  identified fallback layout.
- **`PresentStatus` bit order** must come from `HidP_GetUsages`, never a guess.
- **Direct Win32 HID is more code than `hidapi`** and the overlapped read/cancel
  path is easy to get subtly wrong. Justified in [Why not hidapi](#why-not-hidapi),
  and the fake `Device` keeps it isolated behind one seam.
- **The uxtheme dark-mode ordinals are undocumented** and may be renumbered.
  Guard every `GetProcAddress`; degrade to a light menu.
- **The agent is the whole tail of the risk**, which is why it is last, defaults
  to dry-run, requires a sacrificial-load rig, and why PowerChute stays armed
  until it isn't needed.

## Non-goals

Unchanged: writing the battery date, triggering self-tests, parsing PowerChute's
Java-serialised `EventLog`, energy-cost/CO2 reporting. Graceful shutdown is no
longer a non-goal — it is Phase 8 — but it stays a **separate binary**, gated
behind the readout being trusted first.

Phase 8 does introduce the project's first device writes (`DelayBeforeShutdown`
and, if confirmed, the APC vendor restart usages). That is a deliberate,
scoped exception: without them the feature cannot work at all. It does not
reopen writing the battery date or triggering self-tests, both of which remain
firmly out.

## What the review changed

This plan was reviewed adversarially after the first draft. What moved, so the
reasoning is not lost:

1. **`hidapi` dropped for direct Win32.** Three independent reasons, only one of
   which was in the draft.
2. **HID I/O left the UI thread.** The draft's "read fresh on every menu open"
   contradicted the whole reason jdrgb has a worker thread. Replaced with the
   snapshot architecture.
3. **The sampler could not do what it said.** Load and voltages are not on the
   input stream, so "median every field" was impossible as written. Now split
   explicitly by source.
4. **Transitions now close the interval.** Otherwise a window straddling an
   outage medians two different worlds together.
5. **Staleness became asymmetric.** The draft's "unknown is not an emergency" is
   right before an outage and dangerous during one — it would have run the
   battery flat after a USB drop.
6. **The shutdown became a transaction** with a persisted intent record, ordered
   so the OS commits before the UPS is armed.
7. **The agent became a service**, for preshutdown and power notifications a
   scheduled task cannot receive; sleep/hibernate/Fast Startup were absent
   entirely.
8. **`DelayBeforeStartup` turned out not to exist**, which the caps walk settled.
   The draft inherited the assumption that it did.
9. **Config became a privilege boundary**, and `%ProgramData%` grew real ACLs and
   reparse-point rejection.
10. **`WM_ENDSESSION` gained its `wParam` check** — a latent bug inherited from
    jdrgb that the draft said to port verbatim.
11. **Clipboard, DPI cache key, device selection, and monitor join** all
    corrected.
12. Smaller: scheduled-task battery defaults, report/length validation on every
    read, gap rows, the fan-out test.

One review point is recorded as partly overstated: it held that the evidence
proved only that shared `CreateFile` succeeds, not that input reports fan out to
each handle. The 48-second capture is stronger than that — a complete stream
arrived while PowerChute was actively reading the same device. What it does *not*
establish is drop behaviour under load, which is why the fan-out test exists.
