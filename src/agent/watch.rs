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

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jdups::config::Settings;
use jdups::decode::{self, report, PresentStatus};
use jdups::hid::{self, Ups};
use jdups::logfile;
use jdups::policy::{Observation, State, WakeEvent};
use jdups::status::{self, Event, Phase, Status};
use jdups::stop::Stop;

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
/// Consecutive failed status polls before the handle is declared dead and the
/// device reopened. A healthy device answers every poll, so this is ten
/// seconds of nothing at the 2 s cadence -- long enough to skip a transient,
/// well inside the 30 s staleness default, so a quick reopen keeps the
/// readings from ever counting as stale.
const POLL_FAILURES_TO_REOPEN: u32 = 5;
/// The pause between reopen attempts when reopening does not help, doubling
/// to the cap. The unit can wedge so that opens succeed and every request
/// fails -- seen for real at mains-return on 2026-08-03, fixed only by a
/// physical replug -- and without this the loop burned a full enumeration and
/// six log lines a minute, indefinitely, at a device that was not coming
/// back. Capped low enough that recovery after a replug is never far away.
const REOPEN_BACKOFF_MIN: Duration = Duration::from_secs(10);
const REOPEN_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// How often to look at the countdown registers while on mains. On battery
/// the look runs at the poll cadence instead.
///
/// It used to run **every pass** on battery, to catch PowerChute arming the
/// UPS late in a shutdown -- a moment a few hundred milliseconds wide. That
/// hypothesis was settled on 2026-08-01 and PowerChute is gone; the only
/// writer left is this agent's own transaction, which logs itself. What
/// remains worth seeing -- the UPS decrementing a countdown during a real
/// shutdown -- survives a 2 s look, and the change sheds two-thirds of the
/// agent's on-battery read pressure. That matters since 2026-08-03, when the
/// unit wedged its USB interface at mains-return with three of our readers
/// plus Windows' battery driver all querying it through a transfer event.
const COUNTDOWN_WATCH_MAINS_EVERY: Duration = Duration::from_secs(30);

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

pub fn run(opts: Options, stop: Arc<Stop>) -> i32 {
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
    let mut poll_failures: u32 = 0;
    // The wedged-device machinery: when the handle keeps dying, reopens are
    // paced and the log hears about transitions, not attempts.
    let mut reopen_after: Option<Instant> = None;
    let mut reopen_backoff = REOPEN_BACKOFF_MIN;
    let mut wedge_reported = false;
    // `None` means "due now". Never `Instant::now() - POLL_EVERY`: `Instant` on
    // Windows counts from boot, and subtracting more than the machine has been
    // up panics -- which is exactly the state a boot-started agent runs in.
    let mut last_poll: Option<Instant> = None;

    // The last thing each field was seen to be. Held across ticks so a poll
    // that fails does not erase what is known; `fresh` is what says whether to
    // still believe it.
    let mut status: Option<PresentStatus> = None;
    let mut charge: Option<u8> = None;
    let mut runtime_s: Option<u16> = None;
    // When the numbers last actually arrived. They age separately from the
    // status: report 12 failing while report 22 keeps answering left a
    // days-old runtime qualifying the thresholds as though it were current.
    let mut numbers_at: Option<Instant> = None;
    let mut last_countdown: Option<(Option<i16>, Option<u8>, Option<i16>)> = None;
    let mut last_countdown_look: Option<Instant> = None;
    // When the device last actually answered. `Observation::fresh` is per-pass
    // and says nothing about health; this is what the log should report.
    let mut last_answer = Instant::now();
    // The grace period. `Some(t)` once the decision is made, holding the
    // monotonic instant it was made at.
    let mut committed_at: Option<u64> = None;
    let mut published = Status::default();
    let mut last_publish: Option<Instant> = None;
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

    while !stop.is_stopped() {
        // **Per field, not one bit for all of them.** A single `fresh` flag let
        // a successful charge read mark a *stale status* current, and a status
        // read mark stale numbers current. The first is the dangerous one: the
        // last status says mains, status reads then start failing during an
        // outage while charge reports keep arriving, and the outage is never
        // latched because every observation still claims to be fresh.
        let mut status_fresh = false;
        let mut numbers_fresh = false;

        // --- connect ------------------------------------------------------
        if device.is_none() && reopen_after.is_some_and(|t| Instant::now() < t) {
            // Waiting out the reopen backoff. Everything below still runs --
            // the latch, the backstop, the publishes -- just without a device;
            // pace the pass the way the read timeout otherwise would.
            stop.wait_for(Duration::from_millis(READ_TIMEOUT_MS as u64));
        } else if device.is_none() {
            reopen_after = None;
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
                    // No read to park on, so pace the loop here -- and then
                    // fall through. Everything below still runs while
                    // disconnected: the latch and the backstop are the whole
                    // point, and an outage that takes the USB link with it must
                    // not become an agent that sits quiet. This path used to
                    // decide on its own, announce "shutting down without it",
                    // and then execute nothing at all.
                    stop.wait_for(backoff.min(POLL_EVERY));
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
        }

        // --- the input stream ---------------------------------------------
        // Retried on a slow cadence: a stream that came back is worth having,
        // and reopening is the only thing that has ever restored one.
        if device.is_some() && !input_ok && last_input_retry.elapsed() >= input_backoff {
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
        if let Some(dev) = device.as_ref() {
        match if input_ok {
            dev.input(READ_TIMEOUT_MS)
        } else {
            // Nothing to wait on, so pace the loop off the poll instead of
            // spinning through it as fast as the CPU allows.
            stop.wait_for(Duration::from_millis(READ_TIMEOUT_MS as u64));
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
                Some(report::PRESENT_STATUS) => {
                    status = Some(dev.status_of(&buf, true));
                    status_fresh = true;
                }
                // Report 20 is a partial view -- ShutdownImminent and
                // BelowRemainingCapacityLimit, nothing else -- so decoding it
                // as a full status reads every other flag as cleared and
                // fabricates "on battery", which latches a phantom outage.
                // Merge the two flags it carries; with no baseline yet the 2 s
                // poll supplies one almost immediately, and report 22 carries
                // ShutdownImminent too.
                Some(report::SHUTDOWN_IMMINENT) => {
                    if let Some(s) = status.as_mut() {
                        s.apply_shutdown_report(&dev.status_of(&buf, true));
                        status_fresh = true;
                    }
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
        let polled = last_poll.is_none_or(|t| t.elapsed() >= POLL_EVERY);
        if polled {
            last_poll = Some(Instant::now());
            match dev.feature(report::PRESENT_STATUS) {
                Ok(b) => {
                    poll_failures = 0;
                    if wedge_reported {
                        wedge_reported = false;
                        reopen_backoff = REOPEN_BACKOFF_MIN;
                        say(Level::Info, "the UPS is answering reads again");
                    }
                    status = Some(dev.status_of(&b, false));
                    status_fresh = true;
                }
                // Counted, because a handle can die while the device stays
                // attached: this UPS re-enumerates across some resumes, the
                // old handle then fails every read forever, and reopening is
                // the only recovery. Without the count, `device` was never
                // dropped after the first open and the reconnect branch above
                // was dead code.
                Err(_) => poll_failures += 1,
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
        }

        // --- a dead handle is a lost device ----------------------------------
        // A healthy device answers every status poll, so a run of failures
        // means the handle, not the mains. Reopen through the connect branch,
        // which warns if the device is genuinely gone -- but on a backoff,
        // and said once: a wedged unit is not coming back because we asked
        // again sooner.
        if poll_failures >= POLL_FAILURES_TO_REOPEN {
            poll_failures = 0;
            device = None;
            if !wedge_reported {
                wedge_reported = true;
                say(
                    Level::Warn,
                    "the UPS stopped answering reads; reopening on a backoff. If this persists, replug its USB cable.",
                );
            }
            reopen_after = Some(Instant::now() + reopen_backoff);
            reopen_backoff = (reopen_backoff * 2).min(REOPEN_BACKOFF_MAX);
        }

        // --- resumes, from the service control handler ------------------------
        // Only a service is told. Read *before* the observation is assembled so
        // the policy sees the wake on the same pass; what to do about it lives
        // in `policy::WakeEvent`. Wiring it here as a bare commitment used to be
        // cancelled by the very next pass, because the policy's action never
        // said Shutdown -- the option could not fire at all.
        let mut wake_event = WakeEvent::None;
        if let Some(w) = opts.wake.as_ref() {
            let seq = w.seq.load(Ordering::SeqCst);
            if seq != last_wake_seq {
                last_wake_seq = seq;
                let alone = w.resumed_alone.load(Ordering::SeqCst);
                wake_event = if alone { WakeEvent::Alone } else { WakeEvent::Attended };
                say(
                    Level::Info,
                    if alone {
                        "resumed from sleep with no user present"
                    } else {
                        "resumed from sleep"
                    },
                );
            }
        }

        // The status is what `fresh` means to `policy`: it gates the latch, and
        // the latch is what an outage hangs on. Numbers going stale on their own
        // is survivable -- they are withheld below and the backstop still
        // stands -- but a stale status pretending to be current is not.
        if numbers_fresh {
            numbers_at = Some(Instant::now());
        }
        let numbers_age_s = numbers_at.map(|t| t.elapsed().as_secs());
        let o = observe(
            &start,
            status_fresh,
            &status,
            aged(charge, numbers_age_s, cfg.stale_after_s),
            aged(runtime_s, numbers_age_s, cfg.stale_after_s),
            wake_event,
            os_ac_present(),
        );
        if o.fresh {
            last_answer = Instant::now();
        }
        let stale = last_answer.elapsed().as_secs() > cfg.stale_after_s;
        let action = tick(&mut state, &mut journal, &o, &cfg, dry_run, stale, &say);

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
                let testing = device
                    .as_ref()
                    .and_then(|d| d.feature(report::TEST).ok())
                    .as_deref()
                    .and_then(decode::test)
                    .is_some_and(|v| decode::test_result(v) == "in-progress");
                let code = device
                    .as_ref()
                    .and_then(|d| d.feature(report::LAST_TRANSFER).ok())
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
                            Phase::Pending, Some(0), None, action, Event::Shutdown,
                        );
                        // Announced. The pass-end publish must not carry the
                        // event again: each Event::Shutdown bumps the sequence,
                        // and the tray toasts "shutting down now" once per bump.
                        event = Event::None;
                        let outcome = match device.as_ref() {
                            Some(dev) => shutdown::execute(dev, &opts.dir, cfg.os_shutdown_s, &|m| {
                                say(Level::Act, m)
                            }),
                            // No UPS to arm, but the machine can still go down
                            // cleanly, which is the half that matters. The UPS
                            // keeps supplying a powered-off PC until mains
                            // returns or the battery runs out; wasteful,
                            // recoverable, harmless.
                            None => shutdown::execute_os_only(&|m| say(Level::Act, m)),
                        };
                        match outcome {
                            shutdown::Outcome::Committed { ups_cutoff_s: -1 } => say(
                                Level::Act,
                                "committed: Windows is going down; the UPS was unreachable and stays up",
                            ),
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

        // The estimate the tray shows under its status line, composed here
        // because the agent knows the operative thresholds and the tray
        // deliberately does not. Display only; nothing acts on it, and it is
        // absent outside the on-battery phase -- pending has its own countdown.
        let eta = match phase_of(action, &o) {
            Phase::OnBattery => state.shutdown_eta(&o, &cfg).map(|(secs, route)| {
                // Capitalised: the tray renders this as its own menu row.
                // The backstop's phrase carries no number on purpose -- the
                // estimate next to it *is* that number, and restating it was
                // what made the menu wide.
                let why = match route {
                    // Exact, not rounded: this is a margin, and "At 5 min"
                    // for a 280 s threshold overstated it by twenty seconds.
                    jdups::policy::EtaRoute::Runtime => format!(
                        "At {} of runtime remaining",
                        crate::journal::duration_exact(u64::from(cfg.runtime_threshold_s))
                    ),
                    jdups::policy::EtaRoute::Backstop => "At the on-battery time limit".to_string(),
                };
                (secs, why)
            }),
            _ => None,
        };

        publish(
            &opts.dir,
            &mut published,
            &mut last_publish,
            !dry_run,
            phase_of(action, &o),
            committed_at.map(|at| cfg.warn_before_s.saturating_sub(o.now_s.saturating_sub(at))),
            eta,
            action,
            event,
        );

        // --- watch the countdown registers ---------------------------------
        // Poll cadence on battery, slow on mains. See the constant for why the
        // old every-pass battery watch retired with PowerChute.
        let every = if o.on_battery { POLL_EVERY } else { COUNTDOWN_WATCH_MAINS_EVERY };
        let look = last_countdown_look.is_none_or(|t| t.elapsed() >= every);
        if look {
            if let Some(dev) = device.as_ref() {
                last_countdown_look = Some(Instant::now());
                let now = countdown(dev);
                if last_countdown.is_some_and(|prev| prev != now) {
                    say(Level::Act, &describe_countdown(&now));
                }
                last_countdown = Some(now);
            }
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
    last: &mut Option<Instant>,
    armed: bool,
    phase: Phase,
    seconds_left: Option<u64>,
    eta: Option<(u64, String)>,
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
    if !changed && last.is_some_and(|l| l.elapsed() < Duration::from_secs(5)) {
        return;
    }

    published.phase = phase;
    published.reason = reason;
    published.seconds_left = seconds_left;
    (published.eta_s, published.eta_why) = match eta {
        Some((secs, why)) => (Some(secs), Some(why)),
        None => (None, None),
    };
    published.armed = armed;
    published.updated = logfile::now_local().iso8601();
    // Only a real event advances the sequence. The tray keys "is this new" on
    // it, and bumping it on a routine refresh would re-announce a shutdown that
    // had already been announced -- or, worse, train the tray to ignore it.
    if event != Event::None {
        published.seq += 1;
        published.event = event;
    }
    *last = Some(Instant::now());

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

/// A reading older than the staleness window is not evidence. `None` age means
/// it was never read at all. The policy already treats absent readings as
/// non-qualifying, so withholding here is what connects "the charge reports
/// stopped arriving" to "the thresholds stop acting on the last one".
fn aged<T>(v: Option<T>, age_s: Option<u64>, max_s: u64) -> Option<T> {
    match age_s {
        Some(a) if a <= max_s => v,
        _ => None,
    }
}

/// Assemble one observation from what is currently known.
fn observe(
    start: &Instant,
    fresh: bool,
    status: &Option<PresentStatus>,
    charge: Option<u8>,
    runtime_s: Option<u16>,
    wake: WakeEvent,
    os_ac: Option<bool>,
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
        wake,
        // Passed in rather than read here, so tests do not inherit the power
        // state of whatever machine happens to run them.
        os_ac_present: os_ac,
    }
}

/// The operating system's own view of mains, through its inbox battery driver
/// -- an independent read path to the same hardware, one syscall, no device
/// I/O. `None` when Windows sees no system battery (the PowerChute era, or
/// the driver unbound), which keeps a bare desktop's permanent "AC online"
/// from ever counting as evidence, and `None` when either field says unknown.
fn os_ac_present() -> Option<bool> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    const BATTERY_FLAG_NO_BATTERY: u8 = 128;
    const BATTERY_FLAG_UNKNOWN: u8 = 255;
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return None;
    }
    if s.BatteryFlag == BATTERY_FLAG_NO_BATTERY || s.BatteryFlag == BATTERY_FLAG_UNKNOWN {
        return None;
    }
    match s.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => None,
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
        let o = observe(&start, true, &None, Some(80), Some(1800), WakeEvent::None, None);
        assert!(!o.fresh);
        assert!(!o.on_battery);
    }

    #[test]
    fn a_status_read_carries_both_flags_through() {
        let start = Instant::now();
        let o = observe(&start, true, &Some(status(false, true)), Some(80), Some(1800), WakeEvent::None, None);
        assert!(o.fresh);
        assert!(o.on_battery);
        assert!(o.shutdown_imminent);
    }

    /// A tick with no successful read keeps the last known numbers but says so.
    #[test]
    fn a_failed_tick_keeps_the_numbers_and_drops_freshness() {
        let start = Instant::now();
        let o = observe(&start, false, &Some(status(false, false)), Some(80), Some(1800), WakeEvent::None, None);
        assert!(!o.fresh);
        assert_eq!(o.charge, Some(80));
        assert!(o.on_battery, "the latch has to see the last known state");
    }

    /// Charge and runtime age separately from the status. A charge report that
    /// stopped arriving hours ago must not keep qualifying the thresholds as
    /// though it were current.
    #[test]
    fn a_number_past_the_staleness_window_is_withheld() {
        assert_eq!(aged(Some(42u8), Some(5), 30), Some(42));
        assert_eq!(aged(Some(42u8), Some(30), 30), Some(42));
        assert_eq!(aged(Some(42u8), Some(31), 30), None);
        assert_eq!(aged(Some(42u8), None, 30), None, "never read is not evidence");
        assert_eq!(aged(None::<u8>, Some(5), 30), None);
    }

    /// One armed shutdown is one sequence bump. The inner publish before
    /// execute and the pass-end publish used to both carry Event::Shutdown, so
    /// the tray -- which keys "is this new" on the sequence -- toasted the same
    /// shutdown twice.
    #[test]
    fn an_event_advances_the_sequence_once_and_a_refresh_not_at_all() {
        let dir = std::env::temp_dir().join("jdups-publish-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut published = Status::default();
        let mut last: Option<Instant> = None;
        let action = Action::Shutdown(jdups::policy::Why::Runtime);

        // The inner publish, right before execute...
        publish(&dir, &mut published, &mut last, true, Phase::Pending, Some(0), None, action, Event::Shutdown);
        assert_eq!(published.seq, 1);
        assert_eq!(published.event, Event::Shutdown);
        // ...and the pass-end publish, which carries no event by then.
        publish(&dir, &mut published, &mut last, true, Phase::Pending, Some(0), None, action, Event::None);
        assert_eq!(published.seq, 1, "the same shutdown was announced twice");
        assert_eq!(published.event, Event::Shutdown, "the event must survive a refresh");
        let _ = std::fs::remove_dir_all(&dir);
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
        let mut o = observe(&start, true, &seen, Some(90), Some(2000), WakeEvent::None, None);
        o.now_s = 0;
        assert_eq!(state.observe(&o, &cfg), Action::Warn);

        let mut last = Action::Nothing;
        for t in 1..=cfg.max_on_battery_s {
            let mut o = observe(&start, false, &seen, Some(90), Some(2000), WakeEvent::None, None);
            o.now_s = t;
            last = state.observe(&o, &cfg);
        }
        assert!(last.is_shutdown(), "never reached the backstop: {last:?}");
    }
}
