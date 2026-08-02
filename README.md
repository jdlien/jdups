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

jdups is three binaries totalling under a megabyte with one dependency, and it
reads the same numbers straight off the device. It now does the shutdown too,
which was the one job the vendor software genuinely earned its keep for.

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
- **`jdups-agent.exe`** — the shutdown agent. It watches the UPS, warns before
  it acts, and shuts the machine down cleanly. **Dry run unless told otherwise**:
  `armed = false` is the default and what a missing config means, so it decides
  and logs and touches nothing until you say so. Run it that way first — a
  threshold picked from your own power beats one picked on a bench.
- **`install.ps1`** — registers them as scheduled tasks. `-PerUser` installs
  inside your profile with no elevation at all; `-Agent` adds the agent.

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

## Removing PowerChute: read this first

Uninstalling it changes something the uninstaller does not mention. PowerChute
keeps Windows' **inbox HID battery driver** off the UPS; remove it and Windows
binds that driver, decides the machine has a system battery, and starts showing
a battery icon.

That is mostly harmless and partly not, because **the whole DC half of your
power plan goes live the moment mains fails**. On a desktop those settings have
never applied before and nobody has ever looked at them. The dangerous one:

```powershell
# Windows must not sleep the machine during an outage. A sleeping machine
# cannot run the agent, and the UPS drains until it cuts output and RAM with it.
powercfg /setdcvalueindex SCHEME_CURRENT SUB_SLEEP STANDBYIDLE 0
powercfg /setactive SCHEME_CURRENT
```

Worth checking the rest of the DC column too — `powercfg /q` — though on the
machine this was developed against the only other differences were benign:
display off sooner (which *extends* runtime), disks spinning down after ten
minutes, and PCIe links set to maximum power savings. **Processor state stayed
at 100 %**, so there is no throttling to worry about.

Two more consequences:

- **The UPS stops serving HID input reports**, because the battery driver owns
  them. jdups falls back to polling feature reports and loses nothing but a
  second or two of latency on `ShutdownImminent`.
- **Scheduled tasks default to `DisallowStartIfOnBatteries` and
  `StopIfGoingOnBatteries`.** Once Windows sees a battery, that default would
  stop every jdups task the instant the power failed. `install.ps1` sets both
  off explicitly; anything else you register yourself needs the same.

## Scope

A readout and a log it keeps itself, first. The shutdown agent second, as a
**separate binary** and defaulting to inert — a readout that is wrong shows a
stale number, and a shutdown agent that is wrong eats a filesystem.

Keep PowerChute installed and armed until you have watched jdups decide
correctly through a real outage, then disarm it before arming jdups. Both write
the same UPS countdown register and the last writer wins.

## Docs

- **[docs/status.md](docs/status.md)** — where the work stands, what is proven,
  what is unverified, and what is next. Start here.
- **[docs/implementation-plan.md](docs/implementation-plan.md)** — what to build,
  in what order, how to know it works, and what each phase turned out to have
  wrong.
- **[docs/jdups-plan.md](docs/jdups-plan.md)** — the original investigation: the
  hardware, the HID map, and what the dead ends rule out.
