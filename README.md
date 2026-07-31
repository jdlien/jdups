# jdups

A small Windows tray readout for an APC Back-UPS RS 1500MS2: charge, runtime,
load, voltages, at a click.

Not started. **[docs/jdups-plan.md](docs/jdups-plan.md) is the plan** — read that
first; it carries the verified HID report map and the reasoning.

## Why

The vendor software (PowerChute Serial Shutdown) is a bundled JRE, a Jetty
server and ~90 jars serving a web page on `https://localhost:6547`, to show
about six numbers. The numbers are worth having. The rest is not.

## The short version

The UPS is a standard USB HID Power Device (`051D:0002`), and **every value that
web UI shows is a single 5-byte HID feature report**. The device opens shared, so
this can be read while PowerChute is still running — nothing has to be
uninstalled to try it.

Verified against the live unit:

| | |
|---|---|
| Battery charge | 100 % |
| Runtime remaining | 2274 s (38 min) |
| UPS load | 20 % of 900 W → 180 W |
| Input voltage | 118 V |
| Battery voltage | 27.26 V |
| Battery installed | 2021-11-23 |

That last one answers a standing question: the installation date is **real
device state**, read from the HID `ManufacturerDate` usage, not a date written
to a file — and it is more precise than PowerChute shows it (`11 / 2021`).

## Scope

A readout, and a log it keeps itself. **Not** a shutdown agent — that is the one
job PowerChute genuinely does, Windows has no built-in UPS service, and losing it
unnoticed would only become apparent during an outage. See the plan's non-goals.
