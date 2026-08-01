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
    let mut window_start = Instant::now();
    let mut last_sweep = Instant::now() - SWEEP_EVERY;

    let mut last_status: Option<PresentStatus> = None;
    let mut device_ok = false;

    let write = |acc: &mut Accumulator, event: Event, verbose: bool| {
        let at = logfile::now_local();
        let row = acc.row(&at, event);
        match logfile::append(&opts.dir, &at, &row) {
            Ok(_) => {
                if verbose {
                    println!("{row}");
                }
            }
            Err(e) => eprintln!("jdups: could not write the log: {e}"),
        }
        acc.reset();
    };

    while !stop.load(Ordering::SeqCst) {
        // --- connect ------------------------------------------------------
        let dev = match &device {
            Some(d) => d,
            None => match hid::open(opts.serial.as_deref()) {
                Ok(d) => {
                    backoff = RETRY_MIN;
                    device = Some(d);
                    if device_ok {
                        // Only interesting if we had previously said it was lost.
                        write(&mut acc, Event::DeviceFound, opts.verbose);
                        window_start = Instant::now();
                    }
                    device_ok = true;
                    last_status = None;
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
        match dev.input(READ_TIMEOUT_MS) {
            Ok(Some(buf)) => match buf.first().copied() {
                Some(report::CHARGE_RUNTIME) => {
                    acc.push_stream(decode::charge(&buf), decode::runtime_s(&buf));
                }
                Some(report::PRESENT_STATUS) | Some(report::SHUTDOWN_IMMINENT) => {
                    let s = dev.status_of(&buf, true);
                    acc.set_status(s);
                    // A transition closes the window immediately, so an outage
                    // is never averaged into invisibility by a five-minute
                    // median that spans both sides of it.
                    if let Some(prev) = last_status {
                        if prev.on_battery() != s.on_battery() {
                            let e = if s.on_battery() {
                                Event::OnBattery
                            } else {
                                Event::OnLine
                            };
                            sweep(dev, &mut acc);
                            write(&mut acc, e, opts.verbose);
                            window_start = Instant::now();
                        }
                    }
                    last_status = Some(s);
                }
                _ => {}
            },
            Ok(None) => {}
            Err(e) => {
                eprintln!("jdups: read failed: {e}");
                device = None;
                continue;
            }
        }

        // --- the feature-only fields ---------------------------------------
        if last_sweep.elapsed() >= SWEEP_EVERY {
            sweep(dev, &mut acc);
            last_sweep = Instant::now();
        }

        // --- close the window ----------------------------------------------
        if window_start.elapsed() >= opts.interval {
            write(&mut acc, Event::Interval, opts.verbose);
            window_start = Instant::now();
        }
    }

    // A final row, so a clean stop is visible in the data rather than looking
    // like the machine fell over.
    if device_ok && !acc.is_empty() {
        write(&mut acc, Event::Stopped, opts.verbose);
    }
    0
}

fn sweep(dev: &hid::raw::Device, acc: &mut Accumulator) {
    let f = |id: u8| dev.feature(id).ok();
    acc.push_sweep(
        f(report::LOAD).as_deref().and_then(decode::load_pct),
        f(report::INPUT_VOLTS).as_deref().and_then(decode::input_volts),
        f(report::BATTERY_VOLTS).as_deref().and_then(decode::battery_volts),
        f(report::RATED_POWER).as_deref().and_then(decode::rated_watts),
    );
    // If the stream has not yet produced a status, take one directly rather
    // than logging a blank `ac` column for the first window.
    if let Some(b) = f(report::PRESENT_STATUS) {
        acc.set_status(dev.status_of(&b, false));
    }
}

fn sleep_interruptibly(d: Duration, stop: &AtomicBool) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
}
