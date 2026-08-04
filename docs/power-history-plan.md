# Power history: ask Windows what it already wrote down

Planned 2026-08-03. Prompted by a real question this program could not answer --
*how bad is my mains, actually?* -- and by the discovery that most of the answer
was already on disk, unread.

## The problem, measured

Five plug-pulls inside ninety seconds produced **three** event rows in the
sampler's CSV, and **one** transition in the agent's journal. Neither is a bug:

- The sampler learns the power state from its **15-second sweep** (the input
  stream is permanently dead since Windows bound its battery driver), so any
  pull-and-replug inside one sweep interval is invisible to it.
- The agent deliberately merges rapid cycles: mains has to hold for
  `debounce_s` (20 s) before the latch clears, so a 16-second replug is a
  flicker, not a recovery. That is the anti-flap rule working, and it must not
  change.

Meanwhile Windows recorded **nine** `Kernel-Power` event 105s ("Power source
change") across the same ninety seconds, timestamped to the second, some of them
two to six seconds apart. It was pushed every one of those by its own battery
driver, for free, and journals them whether or not anyone asks.

So the shortest real events are invisible to us and visible to Windows, and the
naive fix -- poll the UPS faster -- is exactly the read pressure that the 2026-08-03
wedge argues against. There is a better trade available.

## The split that makes this work

**Measurement stays ours.** Windows has no idea about load percent, input
voltage, battery voltage, runtime or charge. The CSV's reason to exist --
runtime at a known load, tracked over months -- is unobtainable from any event
log. The sweep stays.

**Transition detection stops being ours.** That is the only thing the 15-second
sweep cadence is buying, and Windows does it better and free.

| | now | after |
|---|---|---|
| Transition latency | up to 15 s; shorter events invisible | 1-2 s |
| Device reads to achieve it | a full sweep every 15 s | none |
| Sweep's remaining job | transitions *and* medians | medians only |

## What the review changed

Reviewed by Codex before any code was written, which was worth it: the original
plan's centrepiece was wrong in the project's own most dangerous way.

**Dropped: the one-second `GetSystemPowerStatus` trigger that swept HID.** The
plan had an OS power-source edge cause an immediate sweep plus a transfer-reason
read -- **several control transfers concentrated at the exact mains-return
instant that wedged this UPS on 2026-08-03**, and during a flapping supply that
is *more* device traffic than today, not less. The plan justified it as "zero
USB cost", which is only true of the syscall, not of the sweep it triggers. It
would have rebuilt the provocation we spent an evening removing, inside the
process that keeps the safety history. Struck entirely.

**Corrected: `BatteryFlag` does not identify the UPS.** Windows' composite
battery driver aggregates every battery, so on a machine with an internal
battery `BatteryFlag != 128/255` stays true even with no UPS bound, and
`ACLineStatus` is system-wide -- a dock or charger can move it. This matters
beyond the sampler: **the agent's already-shipped backstop fallback in
`policy.rs` could have a real UPS outage cleared by an unrelated aggregate
0 -> 1 edge during device silence.** jdups is a desktop-plus-UPS program and
that is now written down as a precondition rather than assumed, in the code and
in status.md.

**Corrected: the counter candidates.** Report 28 (`FF86:16`) is
`APCBattReplaceDate` in NUT's mapping -- a date, not a tally, which fits the
`1C 01 25 09 00` we read. The standard usage worth looking for instead is
`84:38 BadCount` (entries into a bad condition, such as AC out of tolerance);
`85:6B CycleCount` is battery charge cycles and not this. Probe once before and
well *after* a transfer, never repeatedly around mains-return.

**Corrected: I was wrong about the push API.**
`PowerSettingRegisterNotification` takes `DEVICE_NOTIFY_CALLBACK`, so a console
process needs neither a window nor a service handle, and
`GUID_ACDC_POWER_SOURCE` even distinguishes a UPS (`PoHot`) from onboard DC.
Not adopted anyway -- the event log already carries the same edges with
timestamps, so live capture would duplicate it for more code -- but the reason
in the first draft was simply false.

**Settled: reading the System log needs no privilege.** Its default SDDL grants
read to interactive users, so `EvtQuery` direct from the CLI is the shape, and
the SYSTEM-writes-a-summary-file fallback is unnecessary unless Group Policy
has hardened the channel.

## What gets built

### 1. Let the sweep be a historian, not a transient recorder

`SWEEP_EVERY` 15 s -> 30 s, halving the sampler's steady device traffic. The
effective cadence is `min(SWEEP_EVERY, interval / 4)`, so a five-minute window
still gets about ten samples and the medians are unaffected; `n` honestly falls
from ~20 to ~10.

The CSV's event rows become explicitly **best-effort**: a transition is recorded
when a sweep happens to see it, and short cycles will be missed. That is no
longer a gap, because the fine-grained ledger is now `--power-history` below,
and it is free. Accepted cost: self-test and other notable-flag changes are
noticed up to 30 s later, which is nothing against a monthly self-test.

### 2. `jdups --power-history [--days N]`

The feature, because data existing in Event Viewer is not the same as being able
to use it. One command that merges three sources and prints one table:

- **Kernel-Power 105** events from the System log: when the power source
  changed, to the second.
- The **sampler's CSV** event rows: what the UPS reported at those moments, with
  the transfer reason code.
- The **UPS's own counters**, if the probe below finds one.

Sketch:

```
jdups --power-history --days 7

  transfers   14   (3 shorter than 5 s)
  on battery  6 min 20 s total, longest 3 min 44 s

  2026-08-03 21:01:39  ->  battery   45 s   transfer=8 (mains lost)
  2026-08-03 21:02:26  ->  mains     16 s
  2026-08-03 21:02:42  ->  battery   ...
```

Read-only, no daemon involvement, nothing new running. `EvtQuery`/`EvtNext` over
the System channel, XPath-filtered to
`Provider[@Name='Microsoft-Windows-Kernel-Power']` and `EventID=105`, rendered
to XML.

Details the review insisted on, all of which are the difference between a table
and a plausible-looking lie:

- **Parse the `AcOnline` field**, not the localized message text and not an
  assumption that rows alternate. The event carries the state explicitly.
- **Fold consecutive duplicate states** rather than counting them as transfers.
- **Treat the first and last edges as incomplete intervals** and label them,
  instead of inventing a duration.
- Event 105 is **evidence, not a contract**: undocumented as a compatibility
  promise, dependent on Windows recognising the UPS, blind while the machine is
  off or asleep, and erasable by System-log rollover. The output says so, so a
  quiet week is never mistaken for clean power.

### 3. Probe for a transfer counter (measurement, not a feature)

If the UPS maintains a cumulative transfer count, a **slow poll detects fast
events**: the sweep reads it, sees it jumped by three, and records that three
transfers happened without having witnessed any of them. That is strictly better
than every other instrument here, and free.

Candidates from `--probe`, minus report 28 which NUT maps as
`APCBattReplaceDate`: 40 (`FF86:18`, i32), 96 (`FF86:23`, u16), 98 (`FF86:25`,
i32), 117 (`FF86:29`, u16). Baseline taken on battery 2026-08-03 21:12: all
static except 116 (`FF86:2A`), whose middle byte drifts *downward* -- decaying,
not tallying. Also worth grepping the caps walk for `84:38 BadCount`, the
standard usage for "entries into a bad condition such as AC out of tolerance",
which is the thing we actually want if this unit exposes it.

Method: read the set once, well before a transfer; transfer; read again well
*after* mains has returned and settled. **Not repeatedly around the transition**
-- a `GetFeature` is read-only in intent but still a real control transfer into
firmware that has already demonstrated what it thinks of those at that moment.
Confirm across several transfers before believing anything, and pin it in a test
saying it was measured. If nothing increments, record that too: "this unit does
not count transfers" is a useful fact, and it makes the event log the only
fine-grained source there is.

If a counter is found, its delta has nowhere good to go on a transition row that
by definition was not witnessed -- so it belongs on the interval row or in the
history output, as `transfer_delta=N`, not squeezed into an event row that never
happened.

## What does not change

- **The agent's debounce.** It exists so a flapping supply cannot reset a
  shutdown deadline; making it more sensitive to please a historian would be
  trading safety for reporting.
- **The OS signal never drives a safety decision** beyond the narrow,
  edge-gated use already in `policy.rs`. Here it is a historian's prompt, and
  the sampler is not allowed to shut anything down.
- **The CSV schema.** New columns would break the months of history this exists
  to accumulate. If a transfer counter is found, it belongs in the `detail`
  column of an event row, which is already free text.

## Order of work

1. Settle the event-log permission question. It decides where `--power-history`
   lives.
2. The `GetSystemPowerStatus` trigger and the `SWEEP_EVERY` relaxation, with a
   test that a simulated OS change produces exactly one event row and that a
   change with no OS battery driver produces none.
3. Probe for the counter around the next few real transfers.
4. `--power-history`, once (1) has decided its shape.
