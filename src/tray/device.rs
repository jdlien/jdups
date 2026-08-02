//! The device thread.
//!
//! One thread owns the handle. It parks on the input stream, sweeps the
//! feature-only fields on a slow cadence, and publishes an immutable
//! `Snapshot`. The UI reads that snapshot and **never touches HID** — a wedged
//! device must not be able to hang the menu, which is the whole reason jdrgb
//! grew a worker thread in the first place.
//!
//! The notification posted to the window is payload-free on purpose. Passing a
//! heap pointer through `LPARAM` means owning it across a queue that may be
//! discarded during teardown; letting the UI read shared state instead has no
//! ownership question to get wrong.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jdups::decode::{self, report, PresentStatus};
use jdups::hid::{self, Ups};
use jdups::model::{Reading, Smoother, Snapshot};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

/// How long the parked read waits before looping. Short enough that a stop
/// request is noticed promptly; long enough to be free while idle.
const READ_TIMEOUT_MS: u32 = 500;
/// Cadence for the fields that are not on the input stream: load, voltages,
/// rated power, install date.
const SWEEP_EVERY: Duration = Duration::from_secs(5);
/// Backoff after the device goes away. Capped so a replugged UPS is picked up
/// promptly rather than after an ever-growing wait.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(15);

pub struct Monitor {
    snapshot: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    /// A requested `AudibleAlarmControl` value, or 0 for "nothing asked".
    ///
    /// The UI must not touch HID -- see the module note -- so the alarm toggle
    /// is a request left here for the thread that owns the handle. A menu that
    /// could block on a wedged device would be exactly the bug the thread
    /// exists to prevent, and this is the only writable thing the UI offers.
    alarm_request: Arc<AtomicU8>,
    handle: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(hwnd: HWND, msg: u32, serial: Option<String>) -> Monitor {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));

        // HWND is not Send, but the numeric handle is and the window outlives
        // the thread: teardown stops and joins before destroying the window.
        let target = hwnd as usize;
        let snap = Arc::clone(&snapshot);
        let flag = Arc::clone(&stop);
        let alarm_request = Arc::new(AtomicU8::new(0));
        let alarm = Arc::clone(&alarm_request);

        let handle = std::thread::spawn(move || {
            run(target, msg, serial, snap, flag, alarm);
        });

        Monitor {
            snapshot,
            stop,
            alarm_request,
            handle: Some(handle),
        }
    }

    /// Ask the device thread to set the audible alarm. Returns immediately.
    ///
    /// The result shows up in the next snapshot as `reading.alarm`, read back
    /// from the device rather than assumed -- so the menu reflects what the UPS
    /// actually holds, not what it was told.
    pub fn set_alarm(&self, value: u8) {
        self.alarm_request.store(value, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    /// Stop and **join**. A flag alone only lets the thread notice eventually;
    /// without the join a late `PostMessageW` can target a destroyed window.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    target: usize,
    msg: u32,
    serial: Option<String>,
    snapshot: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    alarm_request: Arc<AtomicU8>,
) {
    let mut device: Option<hid::raw::Device> = None;
    let mut backoff = RETRY_MIN;
    let mut last_sweep: Option<Instant> = None;
    let mut last_stream: Option<Instant> = None;
    let mut reading = Reading::default();
    let mut last_status: Option<PresentStatus> = None;
    let mut smoother = Smoother::new();

    while !stop.load(Ordering::SeqCst) {
        // --- reconnect ----------------------------------------------------
        let dev = match &device {
            Some(d) => d,
            None => match hid::open(serial.as_deref()) {
                Ok(d) => {
                    backoff = RETRY_MIN;
                    reading = Reading::default();
                    smoother.clear();
                    last_stream = None;
                    last_sweep = None;
                    last_status = None;
                    device = Some(d);
                    device.as_ref().unwrap()
                }
                Err(e) => {
                    publish(&snapshot, target, msg, &reading, false, Some(e.to_string()), None, None);
                    // Sleep in slices so a stop request is not stuck behind the
                    // full backoff.
                    let until = Instant::now() + backoff;
                    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    backoff = (backoff * 2).min(RETRY_MAX);
                    continue;
                }
            },
        };

        // --- the input stream ---------------------------------------------
        match dev.input(READ_TIMEOUT_MS) {
            Ok(Some(buf)) => {
                last_stream = Some(Instant::now());
                apply_input(dev, &buf, &mut reading, &mut smoother);
            }
            Ok(None) => {} // timeout; loop and re-check the stop flag
            Err(e) => {
                // Lost the device. Clear per-field state so nothing survives
                // the gap looking current.
                device = None;
                reading = Reading::default();
                smoother.clear();
                publish(&snapshot, target, msg, &reading, false, Some(e.to_string()), None, None);
                continue;
            }
        }

        // --- an alarm change the UI asked for ------------------------------
        // Serviced here, on the thread that owns the handle, so a wedged device
        // stalls this loop and not the menu. `set_feature` waits for the write
        // to become visible before returning -- the device takes ~30 ms to
        // commit and an immediate read gives the old value -- so what lands in
        // `reading.alarm` is what the UPS actually holds.
        let wanted = alarm_request.swap(0, Ordering::SeqCst);
        if wanted != 0 {
            match dev.set_feature(report::ALARM, &[wanted]) {
                Ok(back) => reading.alarm = back.get(1).copied(),
                // Left as it was, so the menu keeps showing the truth rather
                // than the request. There is nowhere useful to report this: the
                // tick that matters is the one in the menu.
                Err(_) => reading.alarm = dev
                    .feature(report::ALARM)
                    .ok()
                    .and_then(|b| b.get(1).copied()),
            }
        }

        // --- the feature sweep --------------------------------------------
        // On a cadence, and immediately when the status flags change, so an
        // outage is reflected at once rather than up to a sweep later.
        let status_changed = last_status != Some(reading.status);
        let due = last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY);
        if due || status_changed {
            sweep(dev, &mut reading);
            last_sweep = Some(Instant::now());
            last_status = Some(reading.status);
        }

        publish(
            &snapshot,
            target,
            msg,
            &reading,
            true,
            None,
            last_stream.map(|t| t.elapsed().as_secs()),
            last_sweep.map(|t| t.elapsed().as_secs()),
        );
    }
}

/// Dispatch on `buf[0]`. The stream is multiplexed; decoding report 6 as though
/// it were report 12 would report a confident 0 % charge.
fn apply_input(
    dev: &hid::raw::Device,
    buf: &[u8],
    reading: &mut Reading,
    smoother: &mut Smoother,
) {
    match buf.first().copied() {
        Some(report::CHARGE_RUNTIME) => {
            // Smoothed, not raw: the device cycles through three runtime values
            // ~90 s apart at a dead-steady load, and showing every one of them
            // makes the icon twitch for no reason. See `Smoother`.
            if let Some(c) = decode::charge(buf) {
                smoother.push_charge(c);
                reading.charge = smoother.charge();
            }
            if let Some(r) = decode::runtime_s(buf) {
                smoother.push_runtime(r);
                reading.runtime_s = smoother.runtime();
            }
        }
        Some(report::PRESENT_STATUS) | Some(report::SHUTDOWN_IMMINENT) => {
            reading.status = dev.status_of(buf, true);
            reading.have_status = true;
        }
        Some(report::AC_PRESENT) => {
            // Its own report, and the flags say the same thing; take the sweep's
            // PresentStatus as authoritative and just note that something moved.
            if let Some(ac) = decode::ac_present(buf) {
                if reading.have_status {
                    reading.status.ac_present = ac;
                }
            }
        }
        _ => {}
    }
}

/// The fields that are not on the input stream at all.
fn sweep(dev: &hid::raw::Device, reading: &mut Reading) {
    let f = |id: u8| dev.feature(id).ok();

    if let Some(b) = f(report::LOAD) {
        reading.load_pct = decode::load_pct(&b);
    }
    if let Some(b) = f(report::RATED_POWER) {
        reading.rated_watts = decode::rated_watts(&b);
    }
    if let Some(b) = f(report::INPUT_VOLTS) {
        reading.input_volts = decode::input_volts(&b);
    }
    if let Some(b) = f(report::BATTERY_VOLTS) {
        reading.battery_volts = decode::battery_volts(&b);
    }
    if reading.battery_installed.is_none() {
        if let Some(b) = f(report::MANUFACTURE_DATE) {
            reading.battery_installed = decode::manufacture_date(&b);
        }
    }
    if let Some(b) = f(report::ALARM) {
        reading.alarm = b.get(1).copied();
    }
    if let Some(b) = f(report::LAST_TRANSFER) {
        reading.last_transfer = decode::last_transfer(&b);
    }
    // Belt and braces: if the stream has not yet produced a PresentStatus, read
    // it directly rather than showing "unknown" for the first few seconds.
    if !reading.have_status {
        if let Some(b) = f(report::PRESENT_STATUS) {
            reading.status = dev.status_of(&b, false);
            reading.have_status = true;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish(
    slot: &Arc<Mutex<Snapshot>>,
    target: usize,
    msg: u32,
    reading: &Reading,
    device_ok: bool,
    error: Option<String>,
    stream_age_s: Option<u64>,
    sweep_age_s: Option<u64>,
) {
    let next = Snapshot {
        reading: reading.clone(),
        device_ok,
        error,
        stream_age_s,
        sweep_age_s,
    };

    let changed = {
        let mut guard = slot.lock().unwrap();
        // Only the parts the UI actually renders. Ages tick constantly and
        // would make every read look like a change.
        let differs = guard.reading != next.reading
            || guard.device_ok != next.device_ok
            || guard.error != next.error;
        *guard = next;
        differs
    };

    if changed {
        unsafe { PostMessageW(target as HWND, msg, 0, 0) };
    }
}
