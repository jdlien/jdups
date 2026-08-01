# Status

Where the work stands, what is proven, and what is next. Written to survive a
context boundary — [implementation-plan.md](implementation-plan.md) carries the
reasoning and the hardware map, so this is deliberately short and points there
rather than repeating it.

Last updated: 2026-08-01, 14 commits in.

## Built

Phases 1–7 of the plan, and the first rung of Phase 8. Three binaries over one
lib, one dependency (`windows-sys`), 127 tests, clippy clean.

| | |
|---|---|
| `jdups.exe` | `--once` `--watch` `--probe` `--list` `--log` `--sample` |
| `jdups-tray.exe` | notification icon, menu, notifications; `--balloon` to fire a test one |
| `jdups-agent.exe` | **dry run**: decides and logs, cannot act. `--check` `--print-config` |
| `install.ps1` | machine-wide, or `-PerUser` with no elevation; `-Agent` for the dry run |
| `uninstall.ps1` | only elevates if a machine-wide install is present |

The agent is deliberately half a feature. `policy.rs` decides, `config.rs`
guards the thresholds, `agent/journal.rs` decides what gets written and
`agent/watch.rs` drives the device. **`armed = true` is refused at startup** —
the shutdown transaction is a separate change with its own testing ladder, and
shipping both at once would mean the first run of the transaction is also its
first test.

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
- The charge estimate is a *model*, not a measurement: it drops ~20 points
  within seconds of a transfer and recovers over hours, while battery voltage
  recovers in seconds. This shapes `policy.rs`'s settle window.

## Not verified

Be honest about these rather than assuming they work.

- **`install.ps1` and `uninstall.ps1` have never been executed.** They parse
  clean and are dry-checked. That is the largest untested surface in the repo.
- **The notification icon is measured, not seen.** `Shell_NotifyIconW` now
  returns success with `NIIF_USER | NIIF_LARGE_ICON`, and the shell records a
  toast each time, where before it returned 0 and recorded nothing. That the
  gauge is *visible* on the toast is still somebody else's observation to make.
- **The agent has never seen an outage.** It has been run against the live UPS
  on mains, found the device, and correctly written nothing. Every path from
  `Warn` onward is covered by tests and by nothing else. The plug-pull below is
  what settles it.
- Everything in Phase 8 past the decision: the service, the transaction, the
  restart handshake.

## Next

**Needs the machine, not the developer:**

1. Run `install.ps1 -PerUser` (or without, for the SYSTEM sampler and gapless
   history) and confirm both tasks register and start.
2. Look at a toast and say whether the gauge is on it: `jdups-tray.exe --balloon`
   (which now forwards to the running instance rather than exiting in silence).
3. The tray icon sits in the Windows 11 hidden-icon overflow after any change to
   `APP_ID`, because Explorer keys visibility on app identity. Drag it out once.
4. **Make the agent decide, on purpose, in fifteen seconds.** Put absurd
   thresholds in a config and pull the plug. Nothing can act on the decision, so
   this costs a few seconds of battery and proves the whole path from device to
   log entry:

   ```
   runtime_threshold_s = 3600     # always qualifies
   settle_s = 10                  # the minimum validate() allows
   debounce_s = 5                 # likewise
   ```

   ```powershell
   .\target\release\jdups-agent.exe --config .\test.conf --dir .
   ```

   Expect an `on battery:` line immediately, then an `ACT` line reading
   `would shut down now: predicted runtime below the threshold` about fifteen
   seconds later. If the reason or the timing is wrong, that is worth knowing
   before anything is ever allowed to act on it.

**Phase 8, and only in this order.** See the plan's Phase 8 for the full
argument; the short version, with the first rung now done:

0. ~~The decision, wired to the device, in dry run.~~ **Built.** Run it for
   weeks before touching anything below, and tune `jdups.conf` from what it
   says. A threshold chosen from a month of this machine's own power is worth
   more than every other item on this list.
1. `jdups-agent.exe` as a **Windows service**, not a scheduled task — a task
   cannot receive `SERVICE_CONTROL_PRESHUTDOWN` or power notifications, and
   sleep/hibernate/Fast Startup are otherwise unhandled. The dry run is a
   scheduled task today, which is fine precisely because it cannot act.
2. The shutdown **transaction** with a persisted intent record, ordered so the
   OS commits before the UPS is armed. `SE_SHUTDOWN_NAME` must be explicitly
   enabled and `AdjustTokenPrivileges` checked for `ERROR_NOT_ALL_ASSIGNED`.
3. **The restart handshake is the real unknown.** `DelayBeforeStartup` does not
   exist on this device. Reports 64/65 (`FF86:7C`, `FF86:7D`) have the right
   shape but that is a hypothesis. Confirm against NUT's `apc-hid.c` and prove
   the full shutdown → mains-return → restart cycle **on a sacrificial load**,
   never this machine.
4. Dry-run for weeks. Absurd thresholds to test the trigger cheaply. Only then
   realistic ones, and only then disarm PowerChute.

**Until the agent is proven, PowerChute stays installed and armed.**

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
