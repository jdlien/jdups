# Working in this repo

jdups monitors an APC Back-UPS over USB HID and **shuts this machine down** when
the power fails. Read that sentence again before changing anything under
`src/agent/` or `src/hid/`.

## The one rule that matters

**This program can cut power to a running computer.** Report 65 (`FF86:7D`) arms
a countdown; when it reaches zero the UPS cuts its own output. Getting that
wrong corrupts a filesystem rather than showing a wrong number.

So:

- **Never write a positive value to reports 21, 64 or 65** except through
  `agent/shutdown.rs`, which can undo a half-finished sequence. `jdups --set`
  refuses to arm them and allows `-1` (cancel) precisely because a CLI flag
  writes and exits with no way to recover.
- **`armed = false` is the default and what a missing config means.** Nothing in
  packaging, deployment or a parse failure may produce an armed agent.
- When testing anything that can act, use absurd thresholds and a plug-pull so
  it fires in seconds. Do not wait for a real low battery.

## Measure, do not assume

Nearly every bug in this project's history was a confident assumption. A partial
list, all of which cost real time:

- `IOCTL_HID_GET_FEATURE` is `METHOD_OUT_DIRECT`. The wrong encoding **does not
  error** — it returns a well-formed report decoding as 0 % charge. `--probe`
  re-derives it every run.
- `HIDP_VALUE_CAPS` is 72 bytes, not the 76 that `Marshal.SizeOf` computes.
- Report 65, not the standard `DelayBeforeShutdown` on 21, is what actually
  arms this unit. Report 21 is untouched by the vendor.
- A feature write takes **~30 ms** to become visible to a read. An immediate
  readback returns the *old* value, so "verify every write" done naively reports
  every success as a failure.
- The **input stream and the device are different things.** This one recurred
  four times: the stream dies across S3 resume and across a driver rebind while
  feature reads keep working perfectly. Never treat a failed `input()` as a lost
  device.

When you find something out about the hardware, **pin it in a test** and say in
the comment that it was measured. `docs/status.md` has the current list.

## Style

- Comments carry the **why**, especially where the obvious implementation is
  wrong. If a line looks needlessly careful, the comment should say what went
  wrong without it.
- **No em dashes in user-facing strings** — log lines, notifications, console
  output, PowerShell messages. Use a comma or a full stop. (Comments and docs
  are fine.)
- Tests are named as sentences that state the invariant:
  `a_short_report_is_not_a_report`, `healthy_mains_never_shuts_down`.
- One dependency: `windows-sys`. The whole thesis is three small binaries
  against a bundled JRE and ~90 jars. Do not add a crate without a strong reason;
  the service dispatcher was hand-rolled rather than take `windows-service`.

## Layout

```
src/
  decode.rs      pure decoding of HID reports. No I/O. Golden vectors from the real unit
  model.rs       what a reading is and how it reads to a human
  policy.rs      the shutdown decision, pure. No clock, no I/O, exhaustively tested
  config.rs      the settings file, treated as a privilege boundary
  status.rs      what the SYSTEM agent publishes and the unprivileged tray reads
  logfile.rs     the sampler's CSV
  hid/           enumeration, opening, overlapped I/O, feature read/write
  main.rs        jdups.exe    console readout + --sample
  tray/          jdups-tray.exe   notification icon, menu, GDI drawing, PNG writer
  agent/         jdups-agent.exe  the loop, the journal, the transaction, the service
```

Three binaries because a PE has one subsystem: the tray must be
windows-subsystem, and the console tools must not be or they hand the shell no
stdout and no exit code.

## Privilege boundaries

- The agent runs as **SYSTEM**. Its config lives in `%ProgramData%\jdups`, which
  `install.ps1` ACLs to SYSTEM/Administrators-write, Users-read. That ACL is
  load-bearing for two things: the log and the config. `%ProgramData%` inherits
  permissions that let ordinary users create files, so the installer stripping
  inheritance is what makes it safe.
- The agent is in **session 0** and cannot show anything on screen. It publishes
  `agent-status.txt`; the tray reads it and shows the notification. Trust runs
  one way and must keep doing so.

## Build, test, deploy

```powershell
cargo build --release
cargo test                      # 167 tests, all offline except the ignored ones
cargo clippy --all-targets      # kept at zero warnings

.\install.ps1 -Agent -Service   # machine-wide; -PerUser for no elevation
.\uninstall.ps1
```

A tray running from `target\release` holds a `Local\` singleton, so an installed
copy exits 0 in silence. **Stop the dev one before installing** or the install
looks like it did nothing.

Visual checks, all `#[ignore]`d:

```powershell
cargo test --release -- --ignored contact_sheet  --nocapture
cargo test --release -- --ignored font_specimen  --nocapture
cargo test --release -- --ignored glyph_workshop --nocapture
```

## Read first

`docs/status.md` — what is built, what is *proven against hardware*, what is
unverified, and which review findings were deliberately not fixed.
