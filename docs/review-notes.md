# Note for whoever is reviewing

One finding from measuring the running processes, and one thing of yours I
noticed but did not touch.

## `jdups --sample` was burning 3.2 % of a core, continuously

Found while measuring footprint, not by reading code. CPU over exactly 60 s of
wall clock, on an idle machine:

```
jdups.exe        1937.5 ms  =  3.2292 % of one core
jdups-agent.exe    31.2 ms  =  0.0521 %
jdups-tray.exe     46.9 ms  =  0.0781 %
```

The sampler should be the *least* busy of the three: it polls every 15 s and
writes a row every 5 minutes. Two orders of magnitude out.

**Cause — the same bug, in its third location.** `sample.rs` treated a failed
`dev.input()` as a lost device:

```rust
Err(e) => {
    eprintln!("jdups: read failed: {e}");
    device = None;     // reopen...
    continue;          // ...which succeeds, then fails on the next read
}
```

The backoff only ever covered a failed **open**. A failed **read** had none,
because the reconnect it triggered always worked. So: reopen, read, fail,
repeat, as fast as the CPU allows.

It only became reachable when the input stream died for good — which happens
the moment Windows binds its inbox HID battery driver to the UPS, i.e. when
PowerChute is uninstalled. Before that the stream always worked and the error
arm was effectively dead code.

The same defect was fixed in `agent/watch.rs` and `tray/device.rs` earlier;
`sample.rs` was missed. **It is worth grepping for a fourth copy** — the shape
to look for is `device = None` inside an `Err` arm of an `input()` call.

**Fix:** a failed stream sets `input_ok = false` and the loop keeps the handle
and carries on polling features. Retry backs off 60 s → 1 h, because the
condition is usually permanent. Everything the sampler logs is available from
feature reads, so the cost is latency on a transition, not capability.

Uncommitted at the time of writing if you are looking for it; committed as
"Stop the sampler spinning on a dead input stream".

## The lesson, since this is now four for four

**The input stream and the device are different things.** Every place that
assumed otherwise has been wrong:

1. `tray/device.rs` — published an error over a healthy device
2. `agent/watch.rs` — 85 log lines/second from a SYSTEM process
3. `model.rs::is_stale` — reported "not responding" over a device answering
   every feature read
4. `sample.rs` — 3.2 % of a core, silently

If you are reviewing anything that reads HID here, that is the assumption most
likely to still be lurking.

## Something of yours, untouched

`policy.rs` has `State::wake_at`, declared and never read — clippy flags it as
dead. Your `WakeEvent` tests all pass without it, so it looks like either a
vestige or a piece not yet wired up. Left alone deliberately since the wake
refactor is yours and in progress; flagging it only because the repo has been
kept at zero clippy warnings and this is the one thing breaking that.

Also: the `Instant` boot-window panic and the fresh-mains guard you committed
are both good catches. The second one is the kind of thing the property test
`healthy_mains_never_shuts_down_whatever_the_numbers_say` was supposed to cover
and evidently did not.
