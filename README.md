# jdups

A small Windows tray readout for an APC Back-UPS RS 1500MS2: charge, runtime,
load, voltages, at a click.

Not started. Two documents, in order:

- **[docs/jdups-plan.md](docs/jdups-plan.md)** — the investigation record. The
  hardware, the verified HID report map, and what the dead ends rule out.
- **[docs/implementation-plan.md](docs/implementation-plan.md)** — what to build,
  in what order, and how to know it works.

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

A readout and a log it keeps itself, first. A graceful-shutdown agent second, as
a **separate binary**, gated behind the readout being trusted — a readout that is
wrong shows a stale number, and a shutdown agent that is wrong eats a filesystem.

Until that agent exists and has been proven end to end, **PowerChute stays
installed and armed**: it is the one job it genuinely does, Windows has no
built-in UPS service, and losing it unnoticed would only become apparent during
an outage.
