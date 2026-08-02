//! `jdups --sample`: the headless logger.
//!
//! Runs continuously under a scheduled task and is the **sole writer** of the
//! log. The tray reads the device too, but only to draw; nothing else appends
//! here, so there is no interleaving to reason about.
//!
//! A tray app only logs while someone is logged in. This does not, which is the
//! whole point: "runtime at a known load, tracked over months" is the series
//! that answers whether the battery is dying, and it is worthless with holes
//! wherever the machine sat at a lock screen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jdups::decode::{self, report, PresentStatus};
use jdups::hid::{self, Ups};
use jdups::logfile::{self, Accumulator, Event};

/// How long the parked read waits before looping.
const READ_TIMEOUT_MS: u32 = 500;
/// Cadence for the feature-only fields.
const SWEEP_EVERY: Duration = Duration::from_secs(15);
const RETRY_MIN: Duration = Duration::from_secs(2);
/// How often to try to recover a failed input stream, backing off because it can
/// fail permanently: the inbox HID battery driver owns the stream once Windows
/// binds it, and ours never comes back.
const INPUT_RETRY_MIN: Duration = Duration::from_secs(60);
const INPUT_RETRY_MAX: Duration = Duration::from_secs(3600);
const RETRY_MAX: Duration = Duration::from_secs(60);

pub struct Options {
    pub interval: Duration,
    pub dir: std::path::PathBuf,
    pub serial: Option<String>,
    /// Print each row as it is written. For watching the thing work.
    pub verbose: bool,
}

pub fn run(opts: Options, stop: Arc<AtomicBool>) -> i32 {
    let mut device: Option<hid::raw::Device> = None;
    let mut acc = Accumulator::new();
    let mut backoff = RETRY_MIN;

    // Intervals are driven off a monotonic clock. Wall time is used only for
    // the timestamp column and the monthly filename, so a clock adjustment
    // cannot produce a negative-length window or a burst of rows.
    // Sweep often enough that every window gets several. At the default
    // 300 s interval this is just SWEEP_EVERY, but a short interval -- which is
    // how anyone will first try this -- would otherwise emit rows with the
    // feature-only columns blank, because no sweep happened to fall inside them.
    let sweep_every = SWEEP_EVERY.min(opts.interval / 4).max(Duration::from_secs(1));

    let mut window_start = Instant::now();
    // `None` means "due now". Never `Instant::now() - sweep_every`: `Instant`
    // on Windows counts from boot, and subtracting more than the machine has
    // been up panics -- the boot-started sampler runs in exactly that window.
    let mut last_sweep: Option<Instant> = None;

    let mut last_status: Option<PresentStatus> = None;
    let mut last_test: Option<u8> = None;
    let mut device_ok = false;
    // The input stream can fail while the device stays perfectly healthy.
    let mut input_ok = true;
    let mut last_input_retry = Instant::now();
    let mut input_backoff = INPUT_RETRY_MIN;
    // Whether the log has a start marker yet, so a reader can tell a gap apart
    // from the beginning of the file.
    let mut started = false;

    let write = |acc: &mut Accumulator, event: Event, verbose: bool| {
        let at = logfile::now_local();
        let row = acc.row(&at, event);
        match logfile::append(&opts.dir, &at, &row) {
            Ok(_) => {
                if verbose {
                    println!("{row}");
                }
                acc.reset();
            }
            // Keep the window. The samples are still evidence and the next
            // append may succeed -- a full disk clears, an ACL gets fixed --
            // and a SYSTEM task's stderr goes nowhere, so throwing the window
            // away here silently lost the series the sampler exists to keep.
            // The vectors grow while writes fail, but at stream rates that is
            // kilobytes per hour.
            Err(e) => eprintln!("jdups: could not write the log: {e}"),
        }
    };

    while !stop.load(Ordering::SeqCst) {
        // The status seen this pass, from the stream or the sweep, so
        // transitions are detected wherever it came from.
        let mut seen_status: Option<PresentStatus> = None;
        let mut swept = false;

        // --- connect ------------------------------------------------------
        let dev = match &device {
            Some(d) => d,
            None => match hid::open(opts.serial.as_deref()) {
                Ok(d) => {
                    backoff = RETRY_MIN;
                    device = Some(d);
                    // The condition here was inverted: `device_ok` is *cleared*
                    // when the device goes away, so testing it on reconnect
                    // meant the "came back" row could never be written and a
                    // gap in the log had a beginning but no end. Found by
                    // comparing the event list against PowerChute's.
                    if !started {
                        started = true;
                        write(&mut acc, Event::Started, opts.verbose);
                        window_start = Instant::now();
                    } else if !device_ok {
                        write(&mut acc, Event::DeviceFound, opts.verbose);
                        window_start = Instant::now();
                    }
                    device_ok = true;
                    last_status = None;
                    last_test = initial_test(device.as_ref().unwrap());
                    device.as_ref().unwrap()
                }
                Err(e) => {
                    if device_ok {
                        eprintln!("jdups: lost the UPS: {e}");
                        write(&mut acc, Event::DeviceLost, opts.verbose);
                        window_start = Instant::now();
                        device_ok = false;
                    }
                    sleep_interruptibly(backoff, &stop);
                    backoff = (backoff * 2).min(RETRY_MAX);
                    continue;
                }
            },
        };

        // --- the input stream ---------------------------------------------
        if !input_ok && last_input_retry.elapsed() >= input_backoff {
            last_input_retry = Instant::now();
            input_backoff = (input_backoff * 2).min(INPUT_RETRY_MAX);
            if let Ok(d) = hid::open(opts.serial.as_deref()) {
                device = Some(d);
                input_ok = true;
                continue;
            }
        }
        match if input_ok {
            dev.input(READ_TIMEOUT_MS)
        } else {
            // Pace off the read timeout rather than spinning with nothing to
            // wait on.
            sleep_interruptibly(Duration::from_millis(READ_TIMEOUT_MS as u64), &stop);
            Ok(None)
        } {
            Ok(Some(buf)) => match buf.first().copied() {
                Some(report::CHARGE_RUNTIME) => {
                    acc.push_stream(decode::charge(&buf), decode::runtime_s(&buf));
                }
                Some(report::PRESENT_STATUS) | Some(report::SHUTDOWN_IMMINENT) => {
                    let decoded = dev.status_of(&buf, true);
                    // Report 20 is a partial view -- ShutdownImminent and
                    // BelowRemainingCapacityLimit, nothing else -- so decoding
                    // it as a full status reads every other flag as cleared and
                    // fabricates "on battery": a report-20 push clearing its
                    // flags wrote a phantom onbattery row. Merge the two flags
                    // it carries; with no baseline yet, wait for report 22.
                    if buf.first().copied() == Some(report::SHUTDOWN_IMMINENT) {
                        if let Some(mut prev) = last_status {
                            prev.apply_shutdown_report(&decoded);
                            seen_status = Some(prev);
                        }
                    } else {
                        seen_status = Some(decoded);
                    }
                }
                // Self-tests. Report 33 is pushed on change, so a monthly
                // self-test result lands here without being polled for -- and
                // "did the last self-test pass" is exactly the question the log
                // exists to answer over months.
                Some(report::TEST) => {
                    if let Some(v) = decode::test(&buf) {
                        if last_test.is_some_and(|p| p != v) {
                            sweep(dev, &mut acc);
                            acc.set_detail(format!("result={}", decode::test_result(v)));
                            write(&mut acc, Event::SelfTest, opts.verbose);
                            window_start = Instant::now();
                        }
                        last_test = Some(v);
                    }
                }
                _ => {}
            },
            Ok(None) => {}
            Err(e) => {
                // **A failed input stream is not a lost device**, and this is the
                // third place that had to learn it. Dropping the handle reopened
                // it -- which succeeds, since opening is fine -- and failed on
                // the next read immediately, so this spun as fast as the CPU
                // allowed. Measured at 3.2 % of a core, continuously, while the
                // agent and the tray sat at 0.05 % each.
                //
                // The stream dies for good once Windows binds its own battery
                // driver to the UPS. Everything this logs is available from
                // feature reads, so the fallback costs a little latency on a
                // transition and nothing else.
                if input_ok {
                    input_ok = false;
                    eprintln!("jdups: input stream failed ({e}); polling instead");
                }
                last_input_retry = Instant::now();
            }
        }

        // --- the feature-only fields ---------------------------------------
        if last_sweep.is_none_or(|t| t.elapsed() >= sweep_every) {
            if let Some(s) = sweep(dev, &mut acc) {
                seen_status = Some(s);
            }
            swept = true;
            last_sweep = Some(Instant::now());

            // **Polled, not just watched.** Report 33 is pushed on change, and
            // relying on that alone meant self-tests stopped being logged
            // entirely the moment the input stream went away -- which happens
            // for good once Windows binds its own battery driver to the UPS.
            // A self-test also transfers to battery briefly, so without this
            // row the log shows an onbattery/online pair indistinguishable from
            // a brownout, which is exactly the question the log gets opened to
            // answer.
            if let Some(v) = dev.feature(report::TEST).ok().as_deref().and_then(decode::test) {
                if last_test.is_some_and(|p| p != v) {
                    // No second sweep: the one above already filled this
                    // window's feature columns, and sweeping twice
                    // double-weighted its samples in the event row's medians.
                    acc.set_detail(format!("result={}", decode::test_result(v)));
                    write(&mut acc, Event::SelfTest, opts.verbose);
                    window_start = Instant::now();
                }
                last_test = Some(v);
            }
        }

        // --- transitions, whichever source produced the status ---------------
        // Detected here rather than on the stream alone. The stream dies for
        // good once Windows binds its own battery driver, and a sampler that
        // only saw transitions on the stream logged no onbattery or online
        // rows at all from then on -- the exact events the log exists to hold.
        // A transition closes the window immediately, so an outage is never
        // averaged into invisibility by a five-minute median spanning both
        // sides of it.
        if let Some(s) = seen_status {
            acc.set_status(s);
            if let Some(prev) = last_status {
                let power_moved = prev.on_battery() != s.on_battery();
                // Any other notable flag moving is worth a row too: an
                // overload, comms dropping, mains out of regulation.
                let flags_moved = prev.notable() != s.notable();
                if power_moved || flags_moved {
                    // Fill the event row's feature columns, unless this pass's
                    // sweep already did.
                    if !swept {
                        sweep(dev, &mut acc);
                    }
                    let e = if power_moved {
                        // The *reason* is the interesting half of a transfer
                        // and it is feature-only, so it is only ever available
                        // if we go and read it.
                        if let Some(r) = dev
                            .feature(report::LAST_TRANSFER)
                            .ok()
                            .as_deref()
                            .and_then(decode::last_transfer)
                        {
                            acc.set_detail(format!("transfer={r}"));
                        }
                        if s.on_battery() { Event::OnBattery } else { Event::OnLine }
                    } else {
                        Event::Status
                    };
                    write(&mut acc, e, opts.verbose);
                    window_start = Instant::now();
                }
            }
            last_status = Some(s);
        }

        // --- close the window ----------------------------------------------
        if window_start.elapsed() >= opts.interval {
            write(&mut acc, Event::Interval, opts.verbose);
            window_start = Instant::now();
        }
    }

    // A final row, so a clean stop is visible in the data rather than looking
    // like the machine fell over. Unconditional once started: an empty last
    // window is exactly when you most want to know the stop was orderly.
    if started {
        write(&mut acc, Event::Stopped, opts.verbose);
    }
    0
}

/// Read the feature-only fields into the window, and return the status if the
/// device answered it. The status is read every sweep, not only when the
/// stream is quiet: once the stream dies for good this is the only source of
/// transitions, and the caller diffs whatever this returns.
fn sweep(dev: &hid::raw::Device, acc: &mut Accumulator) -> Option<PresentStatus> {
    let f = |id: u8| dev.feature(id).ok();
    acc.push_sweep(
        f(report::LOAD).as_deref().and_then(decode::load_pct),
        f(report::INPUT_VOLTS).as_deref().and_then(decode::input_volts),
        f(report::BATTERY_VOLTS).as_deref().and_then(decode::battery_volts),
        f(report::RATED_POWER).as_deref().and_then(decode::rated_watts),
    );
    let s = f(report::PRESENT_STATUS).map(|b| dev.status_of(&b, false));
    if let Some(s) = s {
        acc.set_status(s);
    }
    s
}

/// The self-test state at startup, so the first value seen on the stream is a
/// baseline rather than a spurious "the test result changed" row.
fn initial_test(dev: &hid::raw::Device) -> Option<u8> {
    dev.feature(report::TEST).ok().as_deref().and_then(decode::test)
}

fn sleep_interruptibly(d: Duration, stop: &AtomicBool) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
}
