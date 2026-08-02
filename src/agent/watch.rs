//! The agent's loop: read the UPS, decide, write it down.
//!
//! Deliberately thinner than the sampler's. The agent needs four facts —
//! on battery, shutdown imminent, charge, runtime — and reading the voltages it
//! does not use would be four more feature reports per tick and four more ways
//! to fail. The sampler already logs those, and it is the one keeping history.
//!
//! Two sources, because neither alone is sufficient:
//!
//! - **The input stream**, parked on a bounded read. `ShutdownImminent` arrives
//!   here the moment the device raises it, and that is the one signal where
//!   latency is measured against the UPS cutting its own output.
//! - **A feature poll on a fixed cadence**, which is what makes "fresh" mean
//!   something. Freshness driven by pushed reports alone would be a property of
//!   how talkative the device happens to be, and the staleness rule in
//!   `policy` is load-bearing during an outage.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jdups::config::Settings;
use jdups::decode::{self, report, PresentStatus};
use jdups::hid::{self, Ups};
use jdups::logfile;
use jdups::policy::{Observation, State};
use jdups::status::{self, Event, Phase, Status};

use crate::journal::{Journal, Level, Tick};
use crate::shutdown;
use crate::log;

/// How long the parked read waits before looping.
const READ_TIMEOUT_MS: u32 = 500;
/// How often to poll the feature reports, which is also what defines freshness.
const POLL_EVERY: Duration = Duration::from_secs(2);
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How often to try to get a failed input stream back, and how far that backs
/// off when it keeps failing.
///
/// It can fail *permanently*: once Windows binds its inbox HID battery driver
/// to the UPS -- which is what happens when PowerChute is uninstalled -- that
/// driver owns the input reports and ours never come back. Retrying every
/// minute forever then writes two log lines a minute about a condition that is
/// never going to change, which buries the lines that matter.
const INPUT_RETRY_MIN: Duration = Duration::from_secs(60);
const INPUT_RETRY_MAX: Duration = Duration::from_secs(3600);
/// How long to wait before trying a failed shutdown transaction again. Long
/// enough not to hammer `InitiateShutdownW`, short enough to matter on battery.
const RETRY_SHUTDOWN_S: u64 = 30;

pub struct Options {
    pub settings: Settings,
    pub dir: std::path::PathBuf,
    pub serial: Option<String>,
    /// Echo every line to stdout as well. On by default when run from a console.
    pub echo: bool,
    /// Suspend/resume notifications, when running as a service. `None` as a
    /// console process or a scheduled task, neither of which can be told.
    pub wake: Option<Arc<crate::service::Wake>>,
}

pub fn run(opts: Options, stop: Arc<AtomicBool>) -> i32 {
    let cfg = opts.settings.policy;
    let dry_run = !opts.settings.armed;

    let say = |level: Level, msg: &str| {
        let at = logfile::now_local();
        if opts.echo {
            println!("{}  {}  {msg}", at.iso8601(), level.tag());
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        if let Err(e) = log::append(&opts.dir, &at, level, msg) {
            eprintln!("jdups-agent: could not write the log: {e}");
        }
    };

    say(
        Level::Info,
        if dry_run {
            "started in dry run: it decides and logs, and cannot shut anything down"
        } else {
            "started ARMED: it can shut this machine down"
        },
    );
    for line in opts.settings.describe() {
        say(Level::Info, &line);
    }

    // The clock is monotonic and starts here. Wall time appears only in the
    // timestamp column; a clock adjustment must not be able to make a deadline
    // look elapsed. `policy` uses `saturating_sub` for the same reason.
    let start = Instant::now();

    let mut state = State::new();
    let mut journal = Journal::new();
    let mut device: Option<hid::raw::Device> = None;
    let mut backoff = RETRY_MIN;
    let mut device_ok = false;
    let mut reconciled = false;
    let mut last_poll = Instant::now() - POLL_EVERY;

    // The last thing each field was seen to be. Held across ticks so a poll
    // that fails does not erase what is known; `fresh` is what says whether to
    // still believe it.
    let mut status: Option<PresentStatus> = None;
    let mut charge: Option<u8> = None;
    let mut runtime_s: Option<u16> = None;
    let mut last_countdown: Option<(Option<i16>, Option<u8>, Option<i16>)> = None;
    // When the device last actually answered. `Observation::fresh` is per-pass
    // and says nothing about health; this is what the log should report.
    let mut last_answer = Instant::now();
    // The grace period. `Some(t)` once the decision is made, holding the
    // monotonic instant it was made at.
    let mut committed_at: Option<u64> = None;
    let mut published = Status::default();
    let mut last_publish = Instant::now() - Duration::from_secs(60);
    let mut holding_awake = false;
    // Whether the input stream is usable. It can fail independently of the
    // device; see the error arm below.
    let mut input_ok = true;
    let mut last_input_retry = Instant::now();
    let mut input_backoff = INPUT_RETRY_MIN;
    // Monotonic second of the last transaction attempt, so a failure retries
    // rather than latching shut.
    let mut last_attempt: Option<u64> = None;
    let mut last_wake_seq: u32 = 0;
    // Whether the broken stream has already been reported.
    let mut input_reported = false;
    // The last self-test result seen, so only a change is worth a line.
    let mut last_test: Option<u8> = None;

    while !stop.load(Ordering::SeqCst) {
        // **Per field, not one bit for all of them.** A single `fresh` flag let
        // a successful charge read mark a *stale status* current, and a status
        // read mark stale numbers current. The first is the dangerous one: the
        // last status says mains, status reads then start failing during an
        // outage while charge reports keep arriving, and the outage is never
        // latched because every observation still claims to be fresh.
        let mut status_fresh = false;
        let mut numbers_fresh = false;

        // --- connect ------------------------------------------------------
        if device.is_none() {
            match hid::open(opts.serial.as_deref()) {
                Ok(d) => {
                    backoff = RETRY_MIN;
                    if !device_ok {
                        say(Level::Info, &format!("UPS found: serial {}", d.info.serial_display()));
                    }
                    device_ok = true;
                    device = Some(d);

                    // Once, on the first open. If a previous run armed the UPS
                    // and the machine did not go down, a countdown is running
                    // against a live machine right now.
                    if !reconciled {
                        reconciled = true;
                        let d = device.as_ref().unwrap();
                        let on_mains = d
                            .feature(report::PRESENT_STATUS)
                            .ok()
                            .map(|b| !d.status_of(&b, false).on_battery())
                            .unwrap_or(false);
                        shutdown::reconcile(d, &opts.dir, on_mains, &|m| say(Level::Act, m));
                    }
                }
                Err(e) => {
                    if device_ok {
                        // Not merely noteworthy. If this happens during an
                        // outage the agent is now flying on a latched state and
                        // a deadline, which is exactly what the asymmetric
                        // staleness rule in `policy` exists for.
                        say(Level::Warn, &format!("lost the UPS: {e}"));
                        device_ok = false;
                    }
                    // Keep deciding while disconnected. The latch and the
                    // backstop are the whole point: an outage that takes the USB
                    // link with it must not become an agent that sits quiet.
                    //
                    // **And keep acting on it.** This used to call `tick` and
                    // drop the Action on the floor, so the backstop could fire
                    // with nothing listening -- in exactly the scenario it was
                    // written for. There is no device to arm, but the machine
                    // can still be shut down, which is the half that matters.
                    let o = observe(&start, false, &status, charge, runtime_s);
                    let stale = last_answer.elapsed().as_secs() > cfg.stale_after_s;
                    let action = tick(&mut state, &mut journal, &o, &cfg, dry_run, stale, &say);
                    if action.is_shutdown() && !dry_run && committed_at.is_none() {
                        say(Level::Act, "the UPS is unreachable and the deadline has passed; shutting down without it");
                        committed_at = Some(o.now_s);
                    }

                    sleep_interruptibly(backoff.min(POLL_EVERY), &stop);
                    backoff = (backoff * 2).min(RETRY_MAX);
                    continue;
                }
            }
        }
        let dev = device.as_ref().unwrap();

        // --- the input stream ---------------------------------------------
        // Retried on a slow cadence: a stream that came back is worth having,
        // and reopening is the only thing that has ever restored one.
        if !input_ok && last_input_retry.elapsed() >= input_backoff {
            last_input_retry = Instant::now();
            input_backoff = (input_backoff * 2).min(INPUT_RETRY_MAX);
            if let Ok(d) = hid::open(opts.serial.as_deref()) {
                device = Some(d);
                input_ok = true;
                // Silent. Whether it actually came back is decided by the read
                // below, and announcing the attempt was half of the noise.
                continue;
            }
        }
        match if input_ok {
            dev.input(READ_TIMEOUT_MS)
        } else {
            // Nothing to wait on, so pace the loop off the poll instead of
            // spinning through it as fast as the CPU allows.
            sleep_interruptibly(Duration::from_millis(READ_TIMEOUT_MS as u64), &stop);
            Ok(None)
        } {
            Ok(Some(buf)) => {
                // It came back. Worth one line, and worth resetting the backoff.
                if input_reported {
                    input_reported = false;
                    input_backoff = INPUT_RETRY_MIN;
                    say(Level::Info, "the input stream is back");
                }
                match buf.first().copied() {
                Some(report::CHARGE_RUNTIME) => {
                    if let Some(c) = decode::charge(&buf) {
                        charge = Some(c);
                        numbers_fresh = true;
                    }
                    if let Some(r) = decode::runtime_s(&buf) {
                        runtime_s = Some(r);
                        numbers_fresh = true;
                    }
                }
                // The urgent one. Taken straight off the stream rather than
                // waiting for the next poll, because the device raising this is
                // it announcing that it is about to cut output.
                Some(report::PRESENT_STATUS) | Some(report::SHUTDOWN_IMMINENT) => {
                    status = Some(dev.status_of(&buf, true));
                    status_fresh = true;
                }
                _ => {}
                }
            }
            Ok(None) => {}
            Err(e) => {
                // **Do not throw the device away.** The input stream and the
                // device are different things, and they come apart: this UPS
                // stops serving input reports across an S3 resume, and again
                // after a re-enumeration, while feature reads keep working
                // perfectly. Dropping the handle here reopened it -- which
                // succeeds, because opening is fine -- and then failed on the
                // very next read, forever. Measured at ~85 log lines a second,
                // indefinitely, from a SYSTEM process writing to disk.
                //
                // So: stop reading the stream, keep the handle, and carry on
                // polling features. Everything the decision needs comes from
                // the poll; the stream only makes ShutdownImminent arrive
                // sooner. Degrading to a 2 s latency beats spinning.
                if input_ok {
                    input_ok = false;
                    // Once per *transition*, not once per attempt. A stream the
                    // battery driver has taken over never comes back, and
                    // saying so every minute for the life of the machine buries
                    // the lines that matter.
                    if !input_reported {
                        input_reported = true;
                        say(
                            Level::Warn,
                            &format!(
                                "input stream failed ({e}); polling every {} s instead.                                  Expected if Windows has bound its own battery driver to the UPS.",
                                POLL_EVERY.as_secs()
                            ),
                        );
                    }
                }
                last_input_retry = Instant::now();
            }
        }

        // --- the poll -------------------------------------------------------
        let polled = last_poll.elapsed() >= POLL_EVERY;
        if polled {
            last_poll = Instant::now();
            if let Ok(b) = dev.feature(report::PRESENT_STATUS) {
                status = Some(dev.status_of(&b, false));
                status_fresh = true;
            }
            // The self-test, which matters more than it looks. A test transfers
            // to battery for a few seconds, so without this the log shows an
            // "on battery" and a "back on mains" that read exactly like a
            // brownout -- and telling those apart is the main reason anyone
            // opens this file. Polled rather than watched, because the input
            // stream is gone for good once Windows binds its own battery driver.
            if let Some(v) = dev.feature(report::TEST).ok().as_deref().and_then(decode::test) {
                match last_test {
                    Some(prev) if prev != v => say(
                        Level::Info,
                        &format!("UPS self-test: {}", decode::test_result(v)),
                    ),
                    _ => {}
                }
                last_test = Some(v);
            }
            if let Ok(b) = dev.feature(report::CHARGE_RUNTIME) {
                if let Some(c) = decode::charge(&b) {
                    charge = Some(c);
                }
                if let Some(r) = decode::runtime_s(&b) {
                    runtime_s = Some(r);
                }
                numbers_fresh = true;
            }
        }

        // The status is what `fresh` means to `policy`: it gates the latch, and
        // the latch is what an outage hangs on. Numbers going stale on their own
        // is survivable; a stale status pretending to be current is not.
        let o = observe(&start, status_fresh, &status, charge, runtime_s);
        let _ = numbers_fresh;
        if o.fresh {
            last_answer = Instant::now();
        }
        let stale = last_answer.elapsed().as_secs() > cfg.stale_after_s;
        let action = tick(&mut state, &mut journal, &o, &cfg, dry_run, stale, &say);

        // --- did we just wake into an outage? --------------------------------
        // Only a service is told this. If the machine resumed with no sign of a
        // person and it is on battery, the UPS is almost certainly what woke it:
        // nobody is here, and spending twenty-five more minutes of battery
        // holding up an idle machine is backwards. Opt-in, because the wake path
        // is hard to exercise and the conservative default is to do nothing
        // surprising.
        if let Some(w) = opts.wake.as_ref() {
            let seq = w.seq.load(Ordering::SeqCst);
            if seq != last_wake_seq {
                last_wake_seq = seq;
                let alone = w.resumed_alone.load(Ordering::SeqCst);
                say(
                    Level::Info,
                    if alone {
                        "resumed from sleep with no user present"
                    } else {
                        "resumed from sleep"
                    },
                );
                if alone && cfg.shutdown_on_wake && state.on_battery() && committed_at.is_none() {
                    say(
                        Level::Act,
                        "woke onto battery with nobody here; shutting down rather than                          spending the battery on an idle machine",
                    );
                    committed_at = Some(o.now_s);
                }
            }
        }

        // --- do not let it sleep through an outage --------------------------
        // Driven off the latched outage state rather than the raw reading, so a
        // device that goes quiet mid-outage does not let the machine doze off.
        let on_battery_now = state.on_battery();
        if on_battery_now != holding_awake {
            // **Why did it transfer?** A self-test drops to battery for a few
            // seconds and reads identically to a brownout otherwise, and telling
            // those apart is the main reason anyone opens this log. Two signals,
            // both read at the moment it happens:
            //
            //   report 33 = in-progress   a test is running, so this is a test
            //   report 54                 APC's own transfer reason code
            //
            // The code is printed raw. Only one value has been confirmed against
            // this hardware -- 8, for a pulled plug, seen twice -- and inventing
            // names for the rest from a table nobody here has verified would be
            // worse than a number somebody can look up.
            if on_battery_now {
                let testing = dev
                    .feature(report::TEST)
                    .ok()
                    .as_deref()
                    .and_then(decode::test)
                    .is_some_and(|v| decode::test_result(v) == "in-progress");
                let code = dev
                    .feature(report::LAST_TRANSFER)
                    .ok()
                    .as_deref()
                    .and_then(decode::last_transfer);
                say(
                    Level::Info,
                    &match (testing, code) {
                        (true, _) => "transferred to battery: a self-test is running".to_string(),
                        (false, Some(8)) => "transferred to battery: reason 8 (mains lost)".into(),
                        (false, Some(c)) => format!("transferred to battery: reason code {c}"),
                        (false, None) => "transferred to battery: reason unreadable".into(),
                    },
                );
            }
            holding_awake = on_battery_now;
            hold_awake(on_battery_now);
            say(
                Level::Info,
                if on_battery_now {
                    "holding the machine awake while on battery"
                } else {
                    "released the sleep hold"
                },
            );
        }

        // --- the grace period ------------------------------------------------
        // The decision is announced before it is acted on, so whoever is at the
        // machine gets a chance to save their work. It lives here rather than in
        // `policy` because it is about carrying a decision out politely, not
        // about what the numbers mean -- and because cancelling is simply the
        // policy no longer saying Shutdown, which is observed for free.
        let mut event = Event::None;
        match action {
            jdups::policy::Action::Shutdown(why) => {
                // Fire on *becoming* committed, not on the elapsed count being
                // zero. `now_s` is whole seconds and this loop runs several
                // times within one, so testing the count announced the shutdown
                // once per pass -- three identical warnings, and three toasts,
                // in the same second. Caught by a real plug-pull, not by review.
                let first = committed_at.is_none();
                let at = *committed_at.get_or_insert(o.now_s);
                let waited = o.now_s.saturating_sub(at);
                if first {
                    event = Event::Pending;
                    say(
                        Level::Act,
                        &format!(
                            "shutting down in {} s: {}. Save your work.",
                            cfg.warn_before_s,
                            why.as_str()
                        ),
                    );
                }
                // Guarded on an attempt counter, not on the published event.
                // Publishing Shutdown *before* calling execute meant an abort
                // left the guard permanently closed: the policy stayed in
                // shutdown, and every later pass skipped the retry, so one
                // transient failure under the backstop left the machine running
                // to battery exhaustion.
                let due = waited >= cfg.warn_before_s
                    && last_attempt.is_none_or(|t: u64| o.now_s.saturating_sub(t) >= RETRY_SHUTDOWN_S);
                if due {
                    last_attempt = Some(o.now_s);
                    event = Event::Shutdown;
                    if dry_run {
                        say(Level::Act, "the grace period is up: this is where it would shut down");
                    } else {
                        // Publish *before* acting. Windows starts tearing this
                        // process down moments later, and a tray that never
                        // heard the shutdown began would sit showing a
                        // countdown that had already run out.
                        publish(
                            &opts.dir, &mut published, &mut last_publish, true,
                            Phase::Pending, Some(0), action, Event::Shutdown,
                        );
                        match shutdown::execute(dev, &opts.dir, cfg.os_shutdown_s, &|m| {
                            say(Level::Act, m)
                        }) {
                            shutdown::Outcome::Committed { ups_cutoff_s } => say(
                                Level::Act,
                                &format!("committed: Windows is going down, UPS cuts output in {ups_cutoff_s} s"),
                            ),
                            shutdown::Outcome::Aborted(why) => {
                                // Nothing is armed and the machine is still up.
                                // Do not retry on the next pass: whatever
                                // stopped it will stop it again, and a loop
                                // hammering InitiateSystemShutdown is worse than
                                // being honest that it failed.
                                say(Level::Act, &format!("SHUTDOWN FAILED, machine stays up: {why}"));
                            }
                        }
                    }
                }
            }
            _ => {
                if committed_at.take().is_some() {
                    last_attempt = None;
                    event = Event::Cancelled;
                    say(Level::Act, "shutdown cancelled: the machine is no longer past the trigger");
                }
            }
        }

        publish(
            &opts.dir,
            &mut published,
            &mut last_publish,
            !dry_run,
            phase_of(action, &o),
            committed_at.map(|at| cfg.warn_before_s.saturating_sub(o.now_s.saturating_sub(at))),
            action,
            event,
        );

        // --- watch the countdown registers ---------------------------------
        // On battery this runs every pass rather than every poll. The moment
        // worth catching is a few hundred milliseconds wide: whoever arms the
        // UPS does it late in a shutdown sequence, and this process is being
        // torn down by that same sequence. A 2 s cadence could miss the only
        // event the watch exists for.
        if polled || o.on_battery {
            let now = countdown(dev);
            if last_countdown.is_some_and(|prev| prev != now) {
                say(Level::Act, &describe_countdown(&now));
            }
            last_countdown = Some(now);
        }
    }

    // A last look on the way out, because *this* is the likeliest moment to
    // have missed one. Windows delivers CTRL_SHUTDOWN_EVENT to console
    // processes as it goes down, which is what breaks the loop above -- so if
    // something armed the UPS as part of that same shutdown, this read is the
    // closest we get to it. Unconditional: "nothing was armed" is worth
    // recording too, since it is what rules the hypothesis out.
    if let Some(d) = device.as_ref() {
        say(Level::Info, &format!("final read, {}", describe_countdown(&countdown(d))));
    }
    if holding_awake {
        hold_awake(false);
    }
    say(Level::Info, "stopped");
    0
}

/// The three registers that arm and cancel a UPS-side power cut.
///
/// This watch was built to settle a hypothesis and it did, on 2026-08-01, by
/// observing a real PowerChute shutdown: **report 65 is the countdown**, set to
/// 120 and decrementing in real time until the UPS cut its own output, while the
/// standard `DelayBeforeShutdown` on report 21 was never touched at all. See
/// `report::APC_SHUTDOWN_COUNTDOWN`.
///
/// It stays because the agent needs it permanently. The plan requires reading
/// back every write and reconciling any pending countdown on restart, and is
/// explicit that one must never be blindly cleared: it could be the only thing
/// that will restore power. That needs a record of what was armed and when.
///
/// Report 21 is still read. It is not used by the vendor on this unit, but a
/// value appearing there would mean something else is arming this UPS, and that
/// is worth knowing.
fn countdown(dev: &hid::raw::Device) -> (Option<i16>, Option<u8>, Option<i16>) {
    let i16_at = |id: u8| -> Option<i16> {
        let b = dev.feature(id).ok()?;
        (b.len() >= 3).then(|| i16::from_le_bytes([b[1], b[2]]))
    };
    let u8_at = |id: u8| -> Option<u8> {
        let b = dev.feature(id).ok()?;
        b.get(1).copied()
    };
    (
        i16_at(report::DELAY_BEFORE_SHUTDOWN),
        u8_at(report::APC_SHUTDOWN_ARMED),
        i16_at(report::APC_SHUTDOWN_COUNTDOWN),
    )
}

/// Keep the machine awake while it is running on battery.
///
/// **This does not need a service, and it is not monitoring during sleep.**
/// Nothing monitors during sleep: in S3 the CPU is off and no code of any kind
/// runs. What can be done is refusing to enter it, which is a per-thread call
/// any process can make.
///
/// The scenario it closes: the machine idles into sleep, mains fails, and
/// nothing is left running to notice. The UPS drains over hours, cuts output,
/// and everything in RAM goes with it. Disks are quiesced in S3 so the
/// filesystem survives, but unsaved work does not, and the machine cold-boots
/// rather than resuming.
///
/// It cannot help if the machine is **already** asleep when the power fails.
/// There is no software answer to that one; it needs the UPS armed as a USB
/// wake source, or sleep disabled outright.
///
/// `ES_CONTINUOUS` makes the state stick until it is cleared rather than
/// counting as a single nudge, so this is called on transition rather than on
/// every pass.
fn hold_awake(on: bool) {
    use windows_sys::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
    };
    unsafe {
        if on {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        } else {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

fn phase_of(action: jdups::policy::Action, o: &Observation) -> Phase {
    if action.is_shutdown() {
        Phase::Pending
    } else if o.on_battery {
        Phase::OnBattery
    } else {
        Phase::Idle
    }
}

/// Publish for the tray.
///
/// Rewritten on any change and otherwise once every few seconds, so a stale file
/// is recognisable as stale by its timestamp. The tray reads this; nothing here
/// reads anything the tray writes, because the agent is SYSTEM and taking input
/// from an unprivileged process is exactly the bug this project avoids
/// elsewhere.
#[allow(clippy::too_many_arguments)]
fn publish(
    dir: &std::path::Path,
    published: &mut Status,
    last: &mut Instant,
    armed: bool,
    phase: Phase,
    seconds_left: Option<u64>,
    action: jdups::policy::Action,
    event: Event,
) {
    let reason = match action {
        jdups::policy::Action::Shutdown(why) => Some(why.as_str().to_string()),
        _ => None,
    };
    // While a shutdown is pending, every pass. The tray ticks its own clock
    // between publishes and re-syncs to each one, so a coarse cadence here shows
    // up as the countdown jumping backwards a second or two whenever a stale
    // value arrives. During the grace period this writes a small file a few
    // times a second for under a minute, which is a fair price for digits that
    // only ever go down.
    let changed = published.phase != phase
        || published.reason != reason
        || event != Event::None
        || phase == Phase::Pending;
    if !changed && last.elapsed() < Duration::from_secs(5) {
        return;
    }

    published.phase = phase;
    published.reason = reason;
    published.seconds_left = seconds_left;
    published.armed = armed;
    published.updated = logfile::now_local().iso8601();
    // Only a real event advances the sequence. The tray keys "is this new" on
    // it, and bumping it on a routine refresh would re-announce a shutdown that
    // had already been announced -- or, worse, train the tray to ignore it.
    if event != Event::None {
        published.seq += 1;
        published.event = event;
    }
    *last = Instant::now();

    if let Err(e) = status::write(dir, published) {
        eprintln!("jdups-agent: could not publish status: {e}");
    }
}

fn describe_countdown(c: &(Option<i16>, Option<u8>, Option<i16>)) -> String {
    let show = |v: Option<i16>| match v {
        Some(-1) => "none".to_string(),
        Some(n) => format!("{n}s"),
        None => "?".into(),
    };
    format!(
        "UPS countdown: apc(65)={} armed(64)={} standard(21)={}",
        show(c.2),
        c.1.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        show(c.0),
    )
}

/// Assemble one observation from what is currently known.
fn observe(
    start: &Instant,
    fresh: bool,
    status: &Option<PresentStatus>,
    charge: Option<u8>,
    runtime_s: Option<u16>,
) -> Observation {
    Observation {
        now_s: start.elapsed().as_secs(),
        // A status that was never read is not a fresh anything, whatever the
        // reads did. Without this, a device that answers report 12 but not
        // report 22 would look healthy while `on_battery` stayed false.
        fresh: fresh && status.is_some(),
        on_battery: status.is_some_and(|s| s.on_battery()),
        shutdown_imminent: status.is_some_and(|s| s.shutdown_imminent),
        charge,
        runtime_s,
    }
}

/// Decide, write, and — once there is something to do — do it.
fn tick(
    state: &mut State,
    journal: &mut Journal,
    o: &Observation,
    cfg: &jdups::policy::Config,
    dry_run: bool,
    stale: bool,
    say: &dyn Fn(Level, &str),
) -> jdups::policy::Action {
    let action = state.observe(o, cfg);
    let t = Tick {
        now_s: o.now_s,
        action,
        obs: o,
        on_battery_for: state.on_battery_for(o.now_s),
        dry_run,
        stale,
    };
    if let Some((level, msg)) = journal.note(&t) {
        say(level, &msg);
    }
    action
}

fn sleep_interruptibly(d: Duration, stop: &AtomicBool) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jdups::policy::Action;

    fn status(ac: bool, imminent: bool) -> PresentStatus {
        PresentStatus {
            ac_present: ac,
            discharging: !ac,
            shutdown_imminent: imminent,
            ..Default::default()
        }
    }

    /// Reads succeeding on the numbers while the status report never answers
    /// must not read as a healthy device. `on_battery` would be false, and an
    /// outage would be invisible.
    #[test]
    fn numbers_without_a_status_are_not_fresh() {
        let start = Instant::now();
        let o = observe(&start, true, &None, Some(80), Some(1800));
        assert!(!o.fresh);
        assert!(!o.on_battery);
    }

    #[test]
    fn a_status_read_carries_both_flags_through() {
        let start = Instant::now();
        let o = observe(&start, true, &Some(status(false, true)), Some(80), Some(1800));
        assert!(o.fresh);
        assert!(o.on_battery);
        assert!(o.shutdown_imminent);
    }

    /// A tick with no successful read keeps the last known numbers but says so.
    #[test]
    fn a_failed_tick_keeps_the_numbers_and_drops_freshness() {
        let start = Instant::now();
        let o = observe(&start, false, &Some(status(false, false)), Some(80), Some(1800));
        assert!(!o.fresh);
        assert_eq!(o.charge, Some(80));
        assert!(o.on_battery, "the latch has to see the last known state");
    }

    /// The disconnected path has to keep driving the policy. This is the
    /// failure the plan calls out: an agent that goes quiet when the USB link
    /// drops mid-outage is one that runs the battery flat.
    #[test]
    fn a_disconnected_agent_still_reaches_its_backstop() {
        let cfg = jdups::policy::Config::default();
        let mut state = State::new();
        let start = Instant::now();

        // Seen on battery once, then nothing ever again.
        let seen = Some(status(false, false));
        let mut o = observe(&start, true, &seen, Some(90), Some(2000));
        o.now_s = 0;
        assert_eq!(state.observe(&o, &cfg), Action::Warn);

        let mut last = Action::Nothing;
        for t in 1..=cfg.max_on_battery_s {
            let mut o = observe(&start, false, &seen, Some(90), Some(2000));
            o.now_s = t;
            last = state.observe(&o, &cfg);
        }
        assert!(last.is_shutdown(), "never reached the backstop: {last:?}");
    }
}
