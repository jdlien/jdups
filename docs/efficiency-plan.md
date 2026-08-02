# Efficiency: baseline, plan, and the easy wins

Written 2026-08-02, before changing anything. These are three always-resident
background processes, so the currency is not throughput: it is **wakeups per
second, device IOCTLs per second, and disk churn** on an idle machine. The
sampler's 3.2 %-of-a-core spin (fixed, see review-notes.md) is the cautionary
tale in both directions: real waste hides in loops nobody watches, and it was
found by *measuring*, not by reading code. So: numbers first, then changes,
then the same numbers again.

## Baseline, 2026-08-02

Idle machine, on mains, input streams alive, PowerChute still installed,
post-review build freshly deployed. 60 s window, unelevated observer.

| process | CPU | working set | private | handles | threads | IO reads/min | IO writes/min | IO other/min |
|---|---|---|---|---|---|---|---|---|
| `jdups` (sampler) | ~0 ms/min | 2.7 MB | 0.9 MB | 67 | 1 | 156 | 0 | 131 |
| `jdups-agent` (service) | ~0 ms/min | 6.3 MB | 1.1 MB | 87 | 3 | 156 | 11 | 704 |
| `jdups-tray` | 62.5 ms/min (0.10 % core) | 7.3 MB | 2.4 MB | 203 | 2 | 291 | 0 | 1455 |

Context switches (wakeups), all jdups threads combined: **~29/s**, of which one
thread sits at a flat **10.0/s** — the service watcher thread's 100 ms stop
poll, identifiable from the cadence alone.

Reading the numbers: CPU and memory are already excellent (the sampler burned
1937 ms/min before its fix; it now burns less than one timer tick). What is
*not* excellent is the background chatter: the agent performs ~12 "other" I/O
operations a second and the tray ~24, on a machine where nothing is happening,
and the process trio wakes the CPU ~29 times a second to conclude that nothing
has changed. None of this shows up as CPU percent; all of it shows up as idle
power state residency and as noise in anything that traces the system.

## How to measure (the before/after contract)

Same boot, same idle conditions (mains, no user activity, PCSS state
unchanged), three 60 s runs per side, compare medians. The two snippets below
are the instruments; they live here so the "after" measurement is the same
experiment as the "before".

```powershell
# CPU / memory / IO deltas over 60 s
$names = @('jdups','jdups-tray','jdups-agent')
# snapshot TotalProcessorTime + Win32_Process IO counters, Start-Sleep 60,
# snapshot again, print deltas.  (Full script in the git history of this file's
# baseline commit; keep it verbatim.)

# Wakeups
Get-Counter '\Thread(jdups*)\Context Switches/sec' -SampleInterval 1 -MaxSamples 8
```

Attribution, when a number needs explaining rather than trusting: Process
Monitor filtered to the three image names answers "which file/device op is
that", and `wpr -start power` / WPA answers "which thread wakes and why".
Two open attribution questions already: what makes up the tray's 24 other-ops/s
(the sweep accounts for ~2/s), and its 62 ms/min of CPU (the other two burn
approximately zero doing structurally similar work).

**A change counts as an improvement when:** the targeted counter drops on the
same boot with everything else flat, all 185 tests stay green, and no behavior
the docs promise gets slower — the shutdown warning path keeps its 1 s
delivery, `ShutdownImminent` keeps its sub-second stream latency while the
stream lives, and the on-battery countdown watch stays at every-pass (that one
is deliberate and outage-only; do not optimise the outage path to save idle
power).

## The easy wins, ranked

1. **The service watcher thread polls the stop flag at 10 Hz, forever**
   (`service.rs`: `while !STOP { sleep(100ms) }`). It exists to bridge the SCM
   control handler to the loop's `Arc<AtomicBool>`. Replace the poll with a
   Win32 event the handler sets and the watcher waits on (`WaitForSingleObject`
   INFINITE): 10 wakeups/s → 0, stop latency *improves*, ~a third of the trio's
   total wakeups gone. Lowest risk, biggest single number.
2. **The agent reads the three countdown registers every poll, on mains**
   (`watch.rs`: `if polled || o.on_battery`) — 3 IOCTLs per 2 s, 90/min, to
   watch registers that change only when something arms the UPS. On battery,
   every pass is justified and stays. On mains, every 30 s still catches an
   external arming for the log at 1/15th the chatter.
3. **The tray sweep re-reads constants every 5 s** (`tray/device.rs::sweep`):
   `RATED_POWER` is a property of the unit (the sampler's accumulator already
   treats it that way), `LAST_TRANSFER` only changes on a transfer, and `ALARM`
   only changes when someone toggles it. Read rated power once per connect,
   last-transfer only when the status flags moved, alarm every few sweeps and
   immediately after a toggle request. Sweep drops from ~8 IOCTLs to ~4-5. The
   sampler gets the same rated-power treatment (`sample.rs::sweep`).
4. **The tray re-reads and re-parses the agent status file every second**
   (`check_agent` → `status::read`) even though the agent rewrites it every
   ~5 s idle. Stat first, read only when mtime/size moved: ~80 % of those
   reads and parses gone, the 1 s delivery guarantee untouched because the
   stat itself stays at 1 Hz.
5. **`sleep_interruptibly` slices every sleep into 100 ms wakeups** (three
   copies: agent, sampler, tray backoff). Mostly dormant today because the
   input streams are alive — but the moment PowerChute is removed and Windows
   binds its battery driver, the streams die *permanently* and these become
   the loop pacers: 10 wakeups/s × two processes, forever. Fix before that
   config becomes the normal one: wait on a stop event with a timeout instead
   of slicing (or at minimum 250 ms slices). The tray's device thread should
   share whatever shape wins.
6. **Input read timeout 500 ms → 1000 ms** (all three loops). Halves the
   parked-read wakeup rate. The timeout only paces the loop when the stream is
   *quiet* — pushed reports still arrive instantly — so the costs are: stop
   latency up to 1 s (the service asks for 20 s), and one extra second of
   latency on the poll fallback. Measure-then-decide; take only if the ctx
   number says it matters after #1 and #5.

## Measure first, then decide

- **The agent rewrites `agent-status.txt` every 5 s idle** (11 writes+renames
  a minute, ~2 KB/min) and the tray's freshness window depends on that
  cadence (`FRESH_FOR_S = 20`). Stretching to 10 s halves the churn and still
  fits the window with margin — but this is the safety channel, and 11 small
  writes a minute is a rounding error on any SSD. Only touch it if ProcMon
  shows the rename path is a meaningful slice of the agent's 704 other/min.
- **The tray's 62 ms/min CPU and 1455 other/min** want attribution before any
  further tuning: if it is GDI or uxtheme or the notify-icon plumbing, items
  3-4 will not move it and something else is going on. ProcMon first.
- **On-battery profile**: measure during the next planned plug-pull (the same
  two snippets, while on battery and during a pending countdown). No target;
  just capture it so the outage path has a baseline too.

## Not worth touching (anti-goals, so nobody "fixes" them later)

- **Memory.** 2.7-7.3 MB working sets against a bundled-JRE incumbent; there
  is nothing here worth a line of code.
- **Binary size / build flags.** Already `opt-level = "z"`, fat LTO, one
  codegen unit, `panic = "abort"`, stripped. Done.
- **The CSV open-write-close per row** (`logfile.rs`): deliberate and
  documented — one row per five minutes, and a closed file is one you can
  read, move, or delete without stopping the service.
- **The on-battery every-pass countdown watch**: outage-only by design, and
  the moment it exists to catch is a few hundred milliseconds wide.
- **Architecture.** Three processes with a file between them is the product's
  privilege model, not overhead to be consolidated.

## Order of work

Baseline is captured (above). Land wins #1-#5 as separate commits, re-measure
after #1 alone (it should be visible in the ctx counter by itself), then after
the batch. Decide #6 and the measure-first items on the evidence. Re-run the
60 s profile once more after PowerChute is eventually removed, because the
dead-stream configuration changes which code paths pace the loops — that run
is the one #5 exists for.

## Results, same day

Wins #1-#5 landed (#1 and #5 became one change: the `Stop` condvar replaced
both the watcher thread and every sliced sleep). Same methodology, same boot
per pair, deployed via a full reinstall before the "after" runs. The tray was
measured twice — once as a dev copy, once installed — and agreed with itself
(15.6/15.6 ms, 557/548 other-ops).

| metric, idle on mains | before | after |
|---|---|---|
| combined wakeups, all jdups threads | 29.3/s | **13.9/s** |
| tray CPU | 62.5 ms/min (0.104 % core) | **15.6 ms/min (0.026 %)** |
| tray IO other-ops | 1,455/min | **548/min** |
| tray IO reads | 291/min | **180/min** |
| tray private memory | 2.4 MB | **1.7 MB** |
| agent IO other-ops | 704/min | **601/min** (the countdown cadence, as predicted) |
| agent threads | 3 | **2** (the watcher is gone) |
| sampler / agent CPU | ~0 | ~0 |

The flat 10/s thread is gone from the wakeup profile, and the wakeup total
fell by more than that thread alone — the tray's per-second parse was
evidently paying scheduler costs too. The tray attribution question mostly
answered itself: the 4x CPU drop landing together with the reader change says
the 1 Hz read-and-parse was the bulk of it, with the sweep diet carrying the
IOCTL reduction. ~548 other-ops/min remain (parked-read cancel path, stats,
the slimmed sweep); nothing about that number demands the ProcMon session the
plan reserved for it.

Deferred, unchanged from the plan: #6 (input timeout 500 ms → 1 s) — at 14
wakeups/s combined the remaining pacer is the parked reads, and halving them
is still on the table if anyone wants the trio under 8/s; the idle status
rewrite (measured at 12 writes/min, unchanged); and the post-PowerChute re-run,
which is the one that will show what #5 bought.

One functional catch fell out of the sweep diet rather than any counter: the
tray only read `PresentStatus` before the stream delivered one, so once the
input stream dies for good -- the post-PowerChute permanent state -- the icon's
power state would have frozen at the last streamed value while the ages stayed
fresh. The sweep reads the status every pass now. Chatter numbers should not
cost outage visibility, and this one was headed the other way.
