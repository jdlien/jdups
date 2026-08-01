# jdups

A small Windows tray readout for an APC Back-UPS RS 1500MS2: charge, runtime,
load, voltages, at a click.

```
Online, 100%, 43 min
Load               20%  (180 W)
Input              117 V
Battery            27.26 V
Battery installed  2021-11-23
```

## Why

The vendor software (PowerChute Serial Shutdown) is a bundled JRE, a Jetty
server and ~90 jars serving a web page on `https://localhost:6547`, to show
about six numbers. The numbers are worth having. The rest is not.

jdups is two binaries totalling ~400 KB with one dependency, and it reads the
same numbers straight off the device.

## What works

- **`jdups-tray.exe`** — a notification icon that *is* the state. A car battery
  whose fill is charge and whose colour is where the power is coming from, with
  the minutes remaining painted into it when there is something to say. The menu
  shows the full readout; clicking any row copies it.
- **`jdups.exe`** — the console side. `--once` for a readout, `--watch` to
  stream decoded reports live, `--probe` to dump the report descriptor,
  `--list` to see what is attached.
- **`jdups.exe --sample`** — a headless logger writing monthly CSV, medians per
  interval, transitions closing the window early.
- **`install.ps1`** — registers both as scheduled tasks. `-PerUser` installs
  inside your profile with no elevation at all.

Not yet: the graceful-shutdown agent. Its decision logic exists and is tested
(`src/policy.rs`), but nothing is wired up to act on it.

## Build

```powershell
cargo build --release
.\target\release\jdups.exe --once
```

Rust, `windows-sys`, no other dependencies. No admin needed to run.

## The short version of how

The UPS is a standard USB HID Power Device (`051D:0002`), and **every value that
web UI shows is a single 5-byte HID feature report**. The device opens shared,
so this reads it while PowerChute is still running — nothing has to be
uninstalled to try it.

The most useful report turned out to be one the original investigation missed
entirely: **report 22, `PresentStatus`**, eleven status flags in one read. It is
invisible to a `HidP_GetValueCaps` walk because those are *button* caps.

## Scope

A readout and a log it keeps itself, first. A graceful-shutdown agent second, as
a **separate binary**, gated behind the readout being trusted — a readout that is
wrong shows a stale number, and a shutdown agent that is wrong eats a filesystem.

Until that agent exists and has been proven end to end, **PowerChute stays
installed and armed**: it is the one job it genuinely does, Windows has no
built-in UPS service, and losing it unnoticed would only become apparent during
an outage.

## Docs

- **[docs/status.md](docs/status.md)** — where the work stands, what is proven,
  what is unverified, and what is next. Start here.
- **[docs/implementation-plan.md](docs/implementation-plan.md)** — what to build,
  in what order, how to know it works, and what each phase turned out to have
  wrong.
- **[docs/jdups-plan.md](docs/jdups-plan.md)** — the original investigation: the
  hardware, the HID map, and what the dead ends rule out.
