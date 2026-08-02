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
server and 90 jars serving a web page on `https://localhost:6547`, to show about
six numbers. The numbers are worth having. The rest is not.

jdups is three binaries totalling under a megabyte with one dependency, and it
reads the same numbers straight off the device. It does the shutdown too, which
was the one job the vendor software genuinely earned its keep for.

### Measured, both installed and running on the same idle machine

Against PowerChute Serial Shutdown 1.5.0.301:

| | PowerChute | jdups | |
|---|---|---|---|
| Installed | 170.1 MB | **0.65 MB** | **262x** |
| Files | 3,869 | **4** | **967x** |
| Private memory | 458.4 MB | **4.4 MB** | **104x** |
| Threads | 110 | **8** | **14x** |
| CPU, idle | 1.30 % of a core | **0.09 %** | **15x** |

Some of the shape behind those numbers:

- **77.7 MB of the install is web assets** — 3,311 HTML, JS, CSS and image files.
  That is 120x the entire jdups installation, to render six numbers in a browser.
- It ships **`ecj`, the Eclipse Java compiler**, so it can compile JSPs at
  runtime on your machine.
- **Its notification-area icon alone uses 71.5 MB** and 8 threads -- 16x the
  memory of everything jdups installs -- and its menu is a set of links that
  open parts of the web app. It shows no readings and does not follow the
  system light/dark theme.

  Ours is the state: a battery whose fill is charge, whose colour is where the
  power is coming from, and whose digits are the minutes remaining. The menu
  carries the full readout and copies any row to the clipboard, toggles the
  UPS's audible alarm, opens the log, and counts a pending shutdown down in
  red. In 2.4 MB, and it respects dark mode.
- 1.3 % of a core, continuously, is not free either — it is roughly what jdups
  costs *fifteen times over*, to poll the same device over the same USB cable.

**Fair caveats.** Both were idle-monitoring; neither figure covers behaviour
during an actual outage. PowerChute's number may be a floor, since opening its
web UI would likely push it higher. And it is not a like-for-like comparison in
its favour: it ships a web interface, SNMP, and email notification that jdups
deliberately does not. The 77.7 MB buys something — just nothing this project
wanted.

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
- **`install.ps1`** — registers everything. `-Agent` adds the shutdown agent,
  `-Service` runs it as a Windows service rather than a scheduled task, and
  `-PerUser` installs inside your profile with no elevation at all.

Also in the tray: a **notification** when the power goes or returns, a **red
countdown in the icon** while a shutdown is pending, an **audible alarm toggle**
for the UPS itself, and **Open log**.

## Quick start

```powershell
cargo build --release
.\target\release\jdups.exe --once      # does it see the UPS?
.\install.ps1 -Agent -Service          # tray, logger, and the agent, in dry run
```

It installs **inert**. The agent decides and logs and does nothing until you say
otherwise, which is the right way round: run it for a while, read the log, and
pick thresholds from your own power rather than from a default.

To arm it, in `C:\ProgramData\jdups\jdups.conf`:

```
armed = true
```

...then restart the service. `jdups-agent.exe --check` prints what it resolved
to and whether it is armed.

## What it writes

Everything lands in `%ProgramData%\jdups` (or `%LOCALAPPDATA%\jdups` under
`-PerUser`):

| file | what |
|---|---|
| `jdups-YYYY-MM.csv` | the sampler's history: medians per interval, a row per event |
| `jdups-agent-YYYY-MM.log` | the agent's account of what it decided and why |
| `jdups.conf` | thresholds. Commented, all defaults, edit and restart |
| `agent-status.txt` | how the SYSTEM agent tells the tray to show a warning |

The CSV is the series that answers "is the battery dying" — runtime at a known
load, tracked over months. The prose log is the one you open after something
happened.

## The console side

```
jdups --once                 the readout
jdups --watch [SECS]         stream decoded input reports
jdups --probe                dump every value and button cap, and re-derive the IOCTL
jdups --list                 every HID collection present, and which one we would pick
jdups --read 21,64,65        read feature reports by number
jdups --set 24 2             write one. Refuses to arm the UPS countdown
jdups --log                  print the path of the newest log

jdups-agent --check          validate the config and print what it resolves to
jdups-agent --print-config   a commented default config file
```

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

Deliberately not here: email notifications, a web UI, and anything that needs a
service running on a port. The tray and a text file are the interface.

## Docs

- **[docs/status.md](docs/status.md)** — where the work stands, what is proven,
  what is unverified, and what is next. Start here.
- **[docs/implementation-plan.md](docs/implementation-plan.md)** — what to build,
  in what order, how to know it works, and what each phase turned out to have
  wrong.
- **[docs/jdups-plan.md](docs/jdups-plan.md)** — the original investigation: the
  hardware, the HID map, and what the dead ends rule out.
