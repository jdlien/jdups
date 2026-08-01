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

use crate::journal::{Journal, Level, Tick};
use crate::log;

/// How long the parked read waits before looping.
const READ_TIMEOUT_MS: u32 = 500;
/// How often to poll the feature reports, which is also what defines freshness.
const POLL_EVERY: Duration = Duration::from_secs(2);
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);

pub struct Options {
    pub settings: Settings,
    pub dir: std::path::PathBuf,
    pub serial: Option<String>,
    /// Echo every line to stdout as well. On by default when run from a console.
    pub echo: bool,
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
    let mut last_poll = Instant::now() - POLL_EVERY;

    // The last thing each field was seen to be. Held across ticks so a poll
    // that fails does not erase what is known; `fresh` is what says whether to
    // still believe it.
    let mut status: Option<PresentStatus> = None;
    let mut charge: Option<u8> = None;
    let mut runtime_s: Option<u16> = None;
    let mut last_countdown: Option<(Option<i16>, Option<u8>, Option<i16>)> = None;

    while !stop.load(Ordering::SeqCst) {
        let mut fresh = false;

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
                    let o = observe(&start, false, &status, charge, runtime_s);
                    tick(&mut state, &mut journal, &o, &cfg, dry_run, &say);

                    sleep_interruptibly(backoff.min(POLL_EVERY), &stop);
                    backoff = (backoff * 2).min(RETRY_MAX);
                    continue;
                }
            }
        }
        let dev = device.as_ref().unwrap();

        // --- the input stream ---------------------------------------------
        match dev.input(READ_TIMEOUT_MS) {
            Ok(Some(buf)) => match buf.first().copied() {
                Some(report::CHARGE_RUNTIME) => {
                    if let Some(c) = decode::charge(&buf) {
                        charge = Some(c);
                        fresh = true;
                    }
                    if let Some(r) = decode::runtime_s(&buf) {
                        runtime_s = Some(r);
                        fresh = true;
                    }
                }
                // The urgent one. Taken straight off the stream rather than
                // waiting for the next poll, because the device raising this is
                // it announcing that it is about to cut output.
                Some(report::PRESENT_STATUS) | Some(report::SHUTDOWN_IMMINENT) => {
                    status = Some(dev.status_of(&buf, true));
                    fresh = true;
                }
                _ => {}
            },
            Ok(None) => {}
            Err(e) => {
                say(Level::Warn, &format!("read failed: {e}"));
                device = None;
                continue;
            }
        }

        // --- the poll -------------------------------------------------------
        let polled = last_poll.elapsed() >= POLL_EVERY;
        if polled {
            last_poll = Instant::now();
            if let Ok(b) = dev.feature(report::PRESENT_STATUS) {
                status = Some(dev.status_of(&b, false));
                fresh = true;
            }
            if let Ok(b) = dev.feature(report::CHARGE_RUNTIME) {
                if let Some(c) = decode::charge(&b) {
                    charge = Some(c);
                }
                if let Some(r) = decode::runtime_s(&b) {
                    runtime_s = Some(r);
                }
                fresh = true;
            }
        }

        let o = observe(&start, fresh, &status, charge, runtime_s);
        tick(&mut state, &mut journal, &o, &cfg, dry_run, &say);

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
    say(Level::Info, "stopped");
    0
}

/// The three registers that arm and cancel a UPS-side power cut.
///
/// Read every poll and logged whenever they move. Two reasons, and the first is
/// temporary but valuable:
///
/// 1. **PowerChute is a working implementation of the thing being
///    reverse-engineered**, and it is still installed and armed. When it shuts
///    this machine down it must write report 21, and it may write 64 and 65 —
///    which are the restart-handshake hypothesis and nothing more until
///    something is observed. Watching costs one poll and settles by observation
///    what would otherwise be settled by experiment on a sacrificial load.
/// 2. **The agent will need this permanently.** The plan requires reading back
///    every write and reconciling any pending countdown on restart, and is
///    explicit that a countdown must never be blindly cleared: it could be the
///    only thing that will restore power. That needs a record of what was armed
///    and by whom.
///
/// Report 65 sitting at -1 alongside `DelayBeforeShutdown`'s own idle -1 is the
/// reason to bother. A register whose unset value is -1 is a delay with a
/// cancel, not a counter and not a flag.
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
        u8_at(APC_RESTART_FLAG),
        i16_at(APC_RESTART_DELAY),
    )
}

/// `FF86:7C`, boolean. Companion to the one below; meaning unconfirmed.
const APC_RESTART_FLAG: u8 = 64;
/// `FF86:7D`, `-1..32767`. Shaped exactly like `DelayBeforeShutdown` and idles
/// at the same -1. The restart-delay hypothesis.
const APC_RESTART_DELAY: u8 = 65;

fn describe_countdown(c: &(Option<i16>, Option<u8>, Option<i16>)) -> String {
    let show = |v: Option<i16>| match v {
        Some(-1) => "none".to_string(),
        Some(n) => format!("{n}s"),
        None => "?".into(),
    };
    format!(
        "UPS countdown registers moved: shutdown(21)={} flag(64)={} restart(65)={}",
        show(c.0),
        c.1.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        show(c.2),
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
    say: &dyn Fn(Level, &str),
) {
    let action = state.observe(o, cfg);
    let t = Tick {
        now_s: o.now_s,
        action,
        obs: o,
        on_battery_for: state.on_battery_for(o.now_s),
        dry_run,
    };
    if let Some((level, msg)) = journal.note(&t) {
        say(level, &msg);
    }

    // The one branch this phase does not have. Arming is refused at startup,
    // so `dry_run` is always true here today; the arm is deliberately a
    // separate change with its own testing ladder, and leaving the site named
    // is better than leaving it implied.
    if action.is_shutdown() && !dry_run {
        say(Level::Act, "armed shutdown is not implemented yet; doing nothing");
    }
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
