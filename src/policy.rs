//! The shutdown decision, as a pure function.
//!
//! **Nothing here acts.** No device writes, no `InitiateSystemShutdownExW`, no
//! clock, no I/O. It takes observations and returns what *should* happen, which
//! means the whole state space can be walked in tests long before anything is
//! wired up to obey it.
//!
//! That is deliberate but it is not sufficient, and the plan is explicit about
//! why: most of the ways a shutdown agent ruins your day are *outside* this
//! function — a partial UPS write, a readback that never happened, a crash
//! between arming the UPS and the OS going down, PowerChute racing it. Those
//! need a fault-injected state machine, not a table test. This is the part that
//! can be made correct early, so it is.
//!
//! Three findings from the hardware shape everything below:
//!
//! 1. **The charge estimate is a model, not a measurement.** It drops ~20 points
//!    within seconds of losing mains and takes hours of recharge to return, while
//!    battery voltage recovers immediately. Acting on it during a transfer means
//!    acting on a number that is about to correct itself.
//! 2. **Runtime is quantised and jitters ±3.5 %** at a dead-steady load, so a
//!    single sample crossing a threshold means nothing.
//! 3. **`ShutdownImminent` is device-authoritative.** The UPS says when it is
//!    about to cut output, which beats anything computed from thresholds.

/// The grace period the transaction asks Windows for, mirrored here so
/// `validate` can reserve it. Kept deliberately small and separate: this module
/// does no I/O and must not depend on the agent, but a threshold that ignores
/// this margin is a threshold that lies.
pub const OS_GRACE_ALLOWANCE_S: u64 = 10;

/// How long after an unattended resume an outage may still be blamed on it.
///
/// The realistic gap is seconds: the resume broadcast lands, the loop polls
/// within two, and the latch follows the first believed reading. The window is
/// generous to cover a slow resume, and bounded so an unattended maintenance
/// wake at 3 a.m. cannot arm a prompt shutdown for an outage hours later.
pub const WAKE_ATTRIBUTION_S: u64 = 60;

/// What the agent should do about the world as it currently stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Mains is fine, or nothing is known and nothing is owed.
    Nothing,
    /// On battery, not yet at the point of shutting down. Worth telling someone.
    Warn,
    /// Shut the machine down now, and the reason it decided that.
    Shutdown(Why),
}

impl Action {
    pub fn is_shutdown(self) -> bool {
        matches!(self, Action::Shutdown(_))
    }
}

/// Which of the four independent routes to a shutdown was taken.
///
/// Carried out of the decision rather than reconstructed by the caller. A dry
/// run whose log says *that* it would have shut down but not *why* cannot be
/// used to tune the thresholds, and a caller that re-derives the reason from the
/// same observation is free to disagree with the decision it is describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The device raised `ShutdownImminent`. It is about to cut its own output.
    DeviceImminent,
    /// On battery longer than the configured maximum, whatever the numbers say.
    Backstop,
    /// Predicted runtime at or below the threshold, held past the debounce.
    Runtime,
    /// Charge at or below the floor, held past the debounce.
    Charge,
    /// The machine woke by itself onto battery and `shutdown_on_wake` says an
    /// idle machine should not spend the battery waiting for the thresholds.
    Wake,
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Why::DeviceImminent => "the UPS says shutdown is imminent",
            Why::Backstop => "on battery past the maximum",
            Why::Runtime => "predicted runtime below the threshold",
            Why::Charge => "charge below the floor",
            Why::Wake => "woke unattended onto battery",
        }
    }
}

/// Which clock produced a shutdown estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtaRoute {
    /// Runtime draining toward the threshold, plus the debounce still owed.
    Runtime,
    /// The on-battery time limit.
    Backstop,
}

/// What the service learned about a resume since the last observation.
///
/// Only a service is ever told. A console agent and a scheduled task always
/// pass `None`, which leaves the wake route permanently unarmed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WakeEvent {
    /// No resume since the last observation.
    #[default]
    None,
    /// The machine resumed and nothing indicates a person did it.
    Alone,
    /// The machine resumed and a person was involved, or has since shown up.
    Attended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Shut down at or below this much predicted runtime.
    pub runtime_threshold_s: u16,
    /// ...or at or below this charge, whichever comes first.
    pub charge_floor_pct: u8,
    /// A qualifying condition must hold this long before it counts.
    pub debounce_s: u64,
    /// Ignore thresholds for this long after mains is lost.
    ///
    /// The transfer sag makes both charge and runtime least trustworthy in
    /// exactly the window the agent cares about most.
    pub settle_s: u64,
    /// Shut down after this long on battery no matter what the numbers say.
    /// The backstop for a device that has stopped telling the truth.
    pub max_on_battery_s: u64,
    /// A reading older than this is not evidence of anything.
    pub stale_after_s: u64,
    /// How long Windows is given to shut down, in seconds.
    ///
    /// This is what the UPS countdown is sized from, and it is not a preference:
    /// it is how long this machine actually takes to shut down, including the
    /// hibernation file Fast Startup may write. Cutting power mid-write is how a
    /// corrupt resume happens. PowerChute's own default for this unit is 120.
    pub os_shutdown_s: u32,
    /// Shut down promptly if the machine woke by itself onto battery.
    ///
    /// Only a service is told about a resume, so this does nothing under a
    /// scheduled task. When it applies, the reasoning is: the machine was
    /// asleep, something woke it without a person involved, and it is running
    /// on battery -- so the UPS woke it, nobody is here, and holding an idle
    /// machine up for another twenty-five minutes spends the battery for
    /// nothing.
    ///
    /// **Off by default.** The wake path is hard to exercise deliberately, and
    /// a default that shuts a machine down sooner than the thresholds say
    /// should be a choice somebody made rather than one they inherited.
    pub shutdown_on_wake: bool,
    /// Warn for this long before actually shutting down.
    ///
    /// The decision is made, announced, and only then acted on. PowerChute shows
    /// a dialog at this moment; it was observed **not** to block — the shutdown
    /// proceeded with the dialog still sitting there unclicked — so it is
    /// informational, and the choice between it and a notification is a question
    /// of how people actually notice things, not of correctness.
    ///
    /// That question is genuinely open. A dialog in the middle of the screen is
    /// hard to miss; a notification in the corner of a 57-inch display is easy
    /// to. What settles it for now is that a notification does not put anything
    /// in the way of someone already hurrying to save their work.
    ///
    /// The warning comes out of the runtime budget, so it is not free: every
    /// second spent warning is a second not spent shutting down.
    pub warn_before_s: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            // Sized from a *measured* worst-case shutdown time on the machine
            // it protects, not from this placeholder. See the plan.
            runtime_threshold_s: 300,
            charge_floor_pct: 25,
            debounce_s: 20,
            settle_s: 30,
            max_on_battery_s: 30 * 60,
            stale_after_s: 30,
            warn_before_s: 60,
            os_shutdown_s: 120,
            shutdown_on_wake: false,
        }
    }
}

impl Config {
    /// Refuse anything that would make the agent dangerous.
    ///
    /// A SYSTEM process reading thresholds from a file is shutdown-as-a-service
    /// if it will accept any value it finds. Validation belongs next to the
    /// meaning of the fields, not in whatever parses them.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.runtime_threshold_s < 30 {
            return Err("runtime_threshold_s below 30 leaves no time to shut down");
        }
        if self.runtime_threshold_s > 3600 {
            return Err("runtime_threshold_s above an hour would shut down on mains loss alone");
        }
        if self.charge_floor_pct > 90 {
            return Err("charge_floor_pct above 90 would fire on almost any discharge");
        }
        if self.debounce_s < 5 {
            return Err("debounce_s below 5 cannot survive the device's own jitter");
        }
        if self.settle_s < 10 {
            return Err("settle_s below 10 acts inside the transfer sag");
        }
        if self.max_on_battery_s < 60 {
            return Err("max_on_battery_s below a minute is not a backstop");
        }
        // Unbounded above, it is not a backstop either: with charge and runtime
        // both unreadable the agent would sit in Warn until the battery died.
        if self.max_on_battery_s > 24 * 3600 {
            return Err("max_on_battery_s above a day is not a backstop at all");
        }
        // Unbounded above, a reading never goes stale, so one old low sample
        // keeps counting toward a shutdown long after the device stopped
        // answering. `u64::MAX` used to be accepted.
        if self.stale_after_s < 5 || self.stale_after_s > 3600 {
            return Err("stale_after_s must be between 5 seconds and an hour");
        }
        // Not bounded below: zero is a legitimate choice for a machine nobody
        // sits at, where the warning has no one to reach and the seconds are
        // better spent shutting down.
        if self.os_shutdown_s < 30 {
            return Err("os_shutdown_s below 30 will cut power mid-shutdown");
        }
        if self.os_shutdown_s > 1800 {
            return Err("os_shutdown_s above half an hour outlasts any battery");
        }
        if self.warn_before_s > 600 {
            return Err("warn_before_s above ten minutes spends the battery on a notification");
        }
        // **The whole sequence has to fit in the threshold, not just the
        // warning.** Firing at `runtime_threshold_s` only helps if what follows
        // finishes before the battery does, and what follows is: the debounce
        // that confirmed it, the warning, the OS grace, and the shutdown itself.
        // `warn_before_s < runtime_threshold_s` alone accepted a 299 s warning
        // against a 300 s threshold with a 120 s shutdown after it, which begins
        // going down with no predicted runtime left.
        let needed = self
            .debounce_s
            .saturating_add(self.warn_before_s)
            .saturating_add(OS_GRACE_ALLOWANCE_S)
            .saturating_add(self.os_shutdown_s as u64);
        if needed >= self.runtime_threshold_s as u64 {
            return Err(
                "debounce + warn_before_s + os_shutdown_s must fit inside runtime_threshold_s,                  or the battery runs out mid-shutdown",
            );
        }
        Ok(())
    }
}

/// One look at the world, at a monotonic instant.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    /// Monotonic seconds. Never wall time: a clock adjustment must not be able
    /// to make a deadline appear to have passed.
    pub now_s: u64,
    /// Did we actually hear from the device this time?
    pub fresh: bool,
    pub on_battery: bool,
    pub shutdown_imminent: bool,
    pub charge: Option<u8>,
    pub runtime_s: Option<u16>,
    /// A resume noticed since the last observation, from the service control
    /// handler. An edge, not a level: `Alone`/`Attended` appear on exactly one
    /// observation each and `None` everywhere else.
    pub wake: WakeEvent,
    /// The operating system's own view of mains, through its battery driver,
    /// when it has one bound: an independent read path to the same hardware.
    /// `None` when Windows sees no system battery or cannot say, which keeps a
    /// bare desktop's permanent "AC online" from ever counting as evidence.
    pub os_ac_present: Option<bool>,
}

/// What the agent remembers between observations.
#[derive(Debug, Default, Clone, Copy)]
pub struct State {
    /// When the current outage was first confirmed. The **latch**.
    outage_since: Option<u64>,
    /// When the shutdown condition first held continuously.
    qualifying_since: Option<u64>,
    /// When mains was first seen back, for the return hysteresis.
    mains_since: Option<u64>,
    /// Last time the device actually spoke.
    last_fresh: Option<u64>,
    /// When the machine last resumed with nobody involved, for the wake route.
    wake_at: Option<u64>,
    /// Whether the OS's battery driver reported the current outage too. What
    /// turns a later "AC present" from a cached leftover into an edge worth
    /// believing; see the backstop.
    os_saw_offline: bool,
    /// Whether that wake has been tied to the current outage. Sticky until the
    /// outage clears or a person shows up, so the grace period cannot outlast
    /// the attribution window and watch the decision evaporate.
    wake_armed: bool,
}

impl State {
    pub fn new() -> State {
        State::default()
    }

    pub fn on_battery(&self) -> bool {
        self.outage_since.is_some()
    }

    /// How long the current outage has been latched, or `None` on mains.
    ///
    /// For the log, not for the decision: "would have shut down 4 minutes into
    /// the outage" is the sentence that makes a dry run worth reading.
    pub fn on_battery_for(&self, now_s: u64) -> Option<u64> {
        self.outage_since.map(|t| now_s.saturating_sub(t))
    }

    /// Roughly how long until this state first says Shutdown, and by which
    /// clock. `None` off battery.
    ///
    /// **For display, never for the decision.** Runtime ticks down at roughly
    /// real time, so runtime minus threshold is itself a time estimate; add
    /// the debounce not yet served, floor it at what remains of the settle
    /// window, and let the backstop's hard deadline beat it when it is sooner.
    /// The charge floor is deliberately absent: percent decay cannot be
    /// extrapolated honestly, so the estimate can only ever be early through
    /// the routes it does model, and the caller should say "about".
    pub fn shutdown_eta(&self, o: &Observation, cfg: &Config) -> Option<(u64, EtaRoute)> {
        let outage_since = self.outage_since?;
        let on_battery_for = o.now_s.saturating_sub(outage_since);

        let backstop = cfg.max_on_battery_s.saturating_sub(on_battery_for);

        let runtime = o.runtime_s.map(|r| {
            let to_cross = u64::from(r.saturating_sub(cfg.runtime_threshold_s));
            let debounce_owed = match self.qualifying_since {
                Some(qs) if to_cross == 0 => {
                    cfg.debounce_s.saturating_sub(o.now_s.saturating_sub(qs))
                }
                _ => cfg.debounce_s,
            };
            let settle_floor = cfg.settle_s.saturating_sub(on_battery_for);
            (to_cross + debounce_owed).max(settle_floor)
        });

        Some(match runtime {
            Some(r) if r <= backstop => (r, EtaRoute::Runtime),
            _ => (backstop, EtaRoute::Backstop),
        })
    }

    /// Fold in one observation and say what to do.
    pub fn observe(&mut self, o: &Observation, cfg: &Config) -> Action {
        if o.fresh {
            self.last_fresh = Some(o.now_s);
        }

        // --- the wake route's bookkeeping --------------------------------
        // Windows sends the automatic broadcast for every resume and adds the
        // user one when a person was involved, possibly delayed until they
        // touch the machine. So Attended must be able to retract an Alone that
        // already armed.
        match o.wake {
            WakeEvent::Alone => self.wake_at = Some(o.now_s),
            WakeEvent::Attended => {
                self.wake_at = None;
                self.wake_armed = false;
            }
            WakeEvent::None => {}
        }

        // --- the latch --------------------------------------------------
        // A confirmed outage stays confirmed until mains is confirmed back,
        // for long enough to not be a flap. Device silence alone never clears
        // it.
        //
        // The device is the witness whenever it is talking. When it has gone
        // silent, the operating system's battery driver stands in -- but only
        // with the 0 -> 1 edge inside this outage, so a value Windows might
        // merely be repeating back cannot end an outage it never saw begin.
        // **Ending the outage, rather than suspending the backstop, is the
        // point**: two reviews independently observed that an indefinite
        // reprieve is its own hazard, since a wedge outliving the outage would
        // leave a latch nothing could clear and a deadline deferred into the
        // next real one. Ending it lands the agent in a state it already
        // understands -- blind on mains, where silence is not an emergency and
        // a fresh battery reading starts a fresh outage.
        let os_says_mains_returned = o.os_ac_present == Some(true) && self.os_saw_offline;
        if o.fresh && o.on_battery {
            self.mains_since = None;
            if self.outage_since.is_none() {
                self.outage_since = Some(o.now_s);
                self.qualifying_since = None;
            }
        } else if (o.fresh && !o.on_battery) || (!o.fresh && os_says_mains_returned) {
            let since = *self.mains_since.get_or_insert(o.now_s);
            if o.now_s.saturating_sub(since) >= cfg.debounce_s {
                self.outage_since = None;
                self.qualifying_since = None;
                // The episode the wake belonged to is over. The next outage
                // is an ordinary outage, whoever woke the machine.
                self.wake_at = None;
                self.wake_armed = false;
                // And the OS edge belonged to this outage; the next one
                // must earn its own.
                self.os_saw_offline = false;
            }
        }

        // The OS corroborating the outage is remembered per outage: it is the
        // first half of the 0 -> 1 edge the backstop's reprieve requires.
        if self.outage_since.is_some() && o.os_ac_present == Some(false) {
            self.os_saw_offline = true;
        }

        // The device saying so outranks anything computed. Not debounced, not
        // gated on the settle window: it is the UPS telling us it is about to
        // cut output, and there is nothing to second-guess it with.
        if o.fresh && o.shutdown_imminent {
            return Action::Shutdown(Why::DeviceImminent);
        }

        let Some(outage_since) = self.outage_since else {
            // Not on battery. Note this covers "we have never heard from the
            // device": before an outage, unknown means do nothing. There is
            // nothing to protect against yet.
            return Action::Nothing;
        };

        let on_battery_for = o.now_s.saturating_sub(outage_since);

        let stale = self
            .last_fresh
            .is_none_or(|t| o.now_s.saturating_sub(t) > cfg.stale_after_s);

        // --- the mains-return hysteresis is not a firing window ----------
        // The latch outlives the outage by up to a debounce, and the thresholds
        // and the backstop used to keep evaluating inside that tail -- against
        // observations that said, believably, that power was back. A qualifying
        // streak begun during the outage could then complete its debounce and
        // shut the machine down seconds after mains returned. So: while the
        // last believed status is mains and the device is still answering, no
        // *new* shutdown fires; either the return sustains and the latch
        // clears, or the outage resumes and everything below rearms. Gated on
        // `!stale` because a mains report the device went silent on is not
        // evidence either, and the backstop must outrank it.
        if !stale && !o.on_battery {
            self.qualifying_since = None;
            return Action::Warn;
        }

        // --- the wake route -----------------------------------------------
        // Associated once, inside the window, then sticky for the rest of the
        // outage: the grace period can outlast the attribution window and the
        // decision must not un-decide itself mid-countdown. No settle, no
        // debounce -- the machine woke by itself onto a confirmed outage, and
        // the whole point is not spending the battery holding up an idle box.
        if !self.wake_armed
            && self
                .wake_at
                .is_some_and(|w| o.now_s.saturating_sub(w) <= WAKE_ATTRIBUTION_S)
        {
            self.wake_armed = true;
        }
        if self.wake_armed && cfg.shutdown_on_wake {
            return Action::Shutdown(Why::Wake);
        }

        // --- the backstop -----------------------------------------------
        // Runs before the staleness check on purpose. If the device went quiet
        // *during* an outage, the deadline is the only thing left, and it must
        // still fire.
        //
        // Unless the operating system, reading the same hardware through its
        // own battery driver, says mains **came back**: AC present now, after
        // reporting the outage earlier -- the 0 -> 1 edge is what makes the
        // reading evidence. The backstop exists for a device that stopped
        // telling the truth, and another stack having watched the power fail
        // and return beats a deadline computed from our own blindness; that
        // exact sequence nearly shut a machine down on healthy mains on
        // 2026-08-03, when the UPS wedged at mains-return.
        //
        // The edge requirement is the second review's correction to the first
        // draft, which trusted any positive reading: the OS path is
        // independent plumbing but **not an independent sensor** -- its driver
        // reads the same wedged device and its value carries no freshness. A
        // cached "AC present" that never once said offline during this outage
        // is a leftover, and deferring on it forever turns a wedge that
        // persists into a real outage into a battery-exhaustion power cut.
        // Absent, negative, or edge-less OS evidence changes nothing.
        //
        // This only has to hold the line for the debounce: a sustained OS
        // mains-return clears the latch above, and then there is no deadline
        // left to defer.
        if on_battery_for >= cfg.max_on_battery_s && !os_says_mains_returned {
            return Action::Shutdown(Why::Backstop);
        }

        // --- staleness, which is asymmetric -----------------------------
        // Before an outage, unknown is not an emergency. *During* one, losing
        // the device is not permission to relax — that is precisely the case
        // where doing nothing runs the battery flat. Hold, warn, and let the
        // backstop above decide.
        if stale {
            // **The debounce restarts.** `qualifying_since` used to survive
            // this, so one low sample, a long silence, and one more low sample
            // satisfied "held continuously" -- which is the exact jitter
            // protection the debounce exists to provide, defeated by the gap.
            self.qualifying_since = None;
            return Action::Warn;
        }

        // --- the settle window ------------------------------------------
        // Both numbers are at their least trustworthy here: the charge model
        // collapses on transfer and corrects itself over the following minutes.
        if on_battery_for < cfg.settle_s {
            return Action::Warn;
        }

        // --- the thresholds ---------------------------------------------
        // OR, not AND. Requiring both to agree fails *open* — the dangerous
        // direction — whenever one of them is unreadable.
        //
        // Runtime is named first when both qualify, because it is the primary
        // trigger: it folds the current load in and the device computes it,
        // which is the whole argument for writing an agent at all. The log
        // prints both numbers regardless, so nothing is hidden by the choice.
        let why = if o.runtime_s.is_some_and(|r| r <= cfg.runtime_threshold_s) {
            Some(Why::Runtime)
        } else if o.charge.is_some_and(|c| c <= cfg.charge_floor_pct) {
            Some(Why::Charge)
        } else {
            None
        };

        let Some(why) = why else {
            self.qualifying_since = None;
            return Action::Warn;
        };

        let since = *self.qualifying_since.get_or_insert(o.now_s);
        if o.now_s.saturating_sub(since) >= cfg.debounce_s {
            Action::Shutdown(why)
        } else {
            Action::Warn
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    fn mains(now_s: u64) -> Observation {
        Observation {
            now_s,
            fresh: true,
            on_battery: false,
            shutdown_imminent: false,
            charge: Some(100),
            runtime_s: Some(2600),
            wake: WakeEvent::None,
            os_ac_present: None,
        }
    }

    fn battery(now_s: u64, charge: u8, runtime_s: u16) -> Observation {
        Observation {
            now_s,
            fresh: true,
            on_battery: true,
            shutdown_imminent: false,
            charge: Some(charge),
            runtime_s: Some(runtime_s),
            wake: WakeEvent::None,
            os_ac_present: None,
        }
    }

    /// Drive a sequence and return the last action.
    fn run(s: &mut State, obs: impl IntoIterator<Item = Observation>) -> Action {
        let c = cfg();
        let mut last = Action::Nothing;
        for o in obs {
            last = s.observe(&o, &c);
        }
        last
    }

    #[test]
    fn the_default_config_is_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn dangerous_configs_are_refused() {
        let bad = [
            Config { runtime_threshold_s: 5, ..cfg() },
            Config { runtime_threshold_s: 7200, ..cfg() },
            Config { charge_floor_pct: 99, ..cfg() },
            Config { debounce_s: 1, ..cfg() },
            Config { settle_s: 0, ..cfg() },
            Config { max_on_battery_s: 10, ..cfg() },
        ];
        for c in bad {
            assert!(c.validate().is_err(), "accepted {c:?}");
        }
    }

    #[test]
    fn mains_is_never_a_reason_to_do_anything() {
        let mut s = State::new();
        assert_eq!(run(&mut s, (0..100).map(mains)), Action::Nothing);
        assert!(!s.on_battery());
    }

    /// Before an outage, silence is not an emergency. There is nothing to
    /// protect against yet, and shutting down because a USB cable is loose
    /// would be its own disaster.
    #[test]
    fn silence_before_an_outage_does_nothing() {
        let mut s = State::new();
        let quiet = |t| Observation { fresh: false, ..mains(t) };
        assert_eq!(run(&mut s, (0..1000).map(quiet)), Action::Nothing);
    }

    /// The device saying so beats anything we compute, immediately.
    #[test]
    fn shutdown_imminent_fires_at_once() {
        let mut s = State::new();
        let o = Observation {
            shutdown_imminent: true,
            ..battery(1, 100, 2600)
        };
        assert_eq!(s.observe(&o, &cfg()), Action::Shutdown(Why::DeviceImminent));
    }

    /// The transfer sag: charge collapses ~20 points within seconds of losing
    /// mains and corrects itself over the following minutes. Acting inside that
    /// window means acting on a number that is about to be wrong.
    #[test]
    fn the_settle_window_ignores_the_transfer_sag() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        // Charge craters well past the floor, immediately.
        for t in 1..c.settle_s {
            let a = s.observe(&battery(t, 10, 200), &c);
            assert_eq!(a, Action::Warn, "acted at t={t}, inside the settle window");
        }
    }

    #[test]
    fn a_sustained_low_runtime_eventually_shuts_down() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);

        // The clock that matters starts at the *first battery observation*,
        // not at the last mains one: settle and debounce both run from when the
        // outage was latched.
        let outage_at = 1;
        let fires_at = outage_at + c.settle_s + c.debounce_s;
        for t in outage_at..fires_at {
            assert_eq!(s.observe(&battery(t, 80, 120), &c), Action::Warn, "at t={t}");
        }
        assert_eq!(s.observe(&battery(fires_at, 80, 120), &c), Action::Shutdown(Why::Runtime));
    }

    /// Either threshold alone is enough. Requiring both to agree fails open
    /// whenever one of them is unreadable, which is the dangerous direction.
    ///
    /// Also pins which reason each route reports, so the dry-run log cannot
    /// start attributing a charge trigger to runtime without a test noticing.
    #[test]
    fn the_thresholds_are_or_not_and() {
        let c = cfg();
        for (charge, runtime, why) in [
            (80u8, 120u16, Why::Runtime),
            (10, 2600, Why::Charge),
        ] {
            let mut s = State::new();
            s.observe(&mains(0), &c);
            let mut last = Action::Nothing;
            for t in 1..=1 + c.settle_s + c.debounce_s {
                last = s.observe(&battery(t, charge, runtime), &c);
            }
            assert_eq!(last, Action::Shutdown(why), "charge {charge} runtime {runtime}");
        }
    }

    /// The jitter finding: runtime oscillates around the threshold at a steady
    /// load. A condition that keeps lapsing has not held, and must not
    /// accumulate credit toward shutting the machine down.
    #[test]
    fn a_condition_that_keeps_lapsing_never_qualifies() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        let t0 = c.settle_s + 1;
        for i in 0..200u64 {
            let t = t0 + i;
            // Alternates either side of the threshold, forever.
            let runtime = if i % 2 == 0 { 280 } else { 320 };
            let a = s.observe(&battery(t, 80, runtime), &c);
            assert_eq!(a, Action::Warn, "shut down on a flapping reading at t={t}");
        }
    }

    /// During an outage, losing the device is not permission to relax. This is
    /// the case the first draft got backwards: it cleared to Nothing and would
    /// have run the battery flat after a USB drop.
    #[test]
    fn going_quiet_during_an_outage_does_not_clear_the_latch() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        assert!(s.on_battery());

        // The device stops answering.
        for t in 2..200u64 {
            let a = s.observe(&Observation { fresh: false, ..battery(t, 90, 2000) }, &c);
            assert_ne!(a, Action::Nothing, "relaxed at t={t} while on battery");
            assert!(s.on_battery(), "latch cleared by silence at t={t}");
        }
    }

    /// ...and the deadline still fires when nothing else can.
    #[test]
    fn the_backstop_fires_even_with_no_readings_at_all() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        let a = s.observe(
            &Observation { fresh: false, ..battery(1 + c.max_on_battery_s, 90, 2000) },
            &c,
        );
        assert_eq!(a, Action::Shutdown(Why::Backstop));
    }

    /// Mains coming back has to be believed for a while. A flapping supply that
    /// cleared the latch on every blip would reset the debounce forever and the
    /// agent would never act.
    #[test]
    fn a_brief_flicker_of_mains_does_not_clear_the_latch() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);

        // Mains reappears for less than the hysteresis, then goes again.
        for t in 2..c.debounce_s {
            s.observe(&mains(t), &c);
            assert!(s.on_battery(), "latch cleared by a flicker at t={t}");
        }
    }

    #[test]
    fn a_sustained_mains_return_does_clear_it() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        for t in 2..=2 + c.debounce_s {
            s.observe(&mains(t), &c);
        }
        assert!(!s.on_battery());
        assert_eq!(s.observe(&mains(100), &c), Action::Nothing);
    }

    /// A second outage after a recovery must start its own settle window rather
    /// than inheriting credit from the first.
    #[test]
    fn a_second_outage_starts_from_scratch() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        // First outage, long enough to be close to acting.
        for t in 1..c.settle_s + c.debounce_s {
            s.observe(&battery(t, 80, 120), &c);
        }
        // Mains returns for good.
        let mut t = c.settle_s + c.debounce_s;
        for _ in 0..=c.debounce_s {
            s.observe(&mains(t), &c);
            t += 1;
        }
        assert!(!s.on_battery());

        // Second outage: the very next qualifying reading must not shut down.
        assert_eq!(s.observe(&battery(t, 10, 60), &c), Action::Warn);
    }

    /// Missing numbers are not a qualifying condition. A device that reports
    /// nothing must ride the backstop, not the thresholds.
    #[test]
    fn absent_readings_never_qualify_on_their_own() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        let t0 = c.settle_s + 1;
        for t in t0..t0 + c.debounce_s * 3 {
            let o = Observation {
                charge: None,
                runtime_s: None,
                ..battery(t, 0, 0)
            };
            assert_eq!(s.observe(&o, &c), Action::Warn, "acted on nothing at t={t}");
        }
    }

    /// No sequence of observations may produce Shutdown while mains is present
    /// and the device is healthy. This is the property that matters most, so it
    /// is checked over the whole plausible space rather than at a few points.
    #[test]
    fn healthy_mains_never_shuts_down_whatever_the_numbers_say() {
        let c = cfg();
        for charge in [0u8, 1, 25, 50, 99, 100] {
            for runtime in [0u16, 1, 60, 300, 2600] {
                let mut s = State::new();
                for t in 0..300u64 {
                    let o = Observation {
                        charge: Some(charge),
                        runtime_s: Some(runtime),
                        ..mains(t)
                    };
                    assert!(
                        !s.observe(&o, &c).is_shutdown(),
                        "shut down on mains at charge {charge} runtime {runtime}"
                    );
                }
            }
        }
    }

    /// Time going backwards must not be able to manufacture an elapsed
    /// deadline. Monotonic clocks do not, but `saturating_sub` is what makes
    /// that a property of the code rather than of the caller.
    #[test]
    fn a_clock_that_goes_backwards_cannot_trigger_anything() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(1000), &c);
        s.observe(&battery(1001, 90, 2000), &c);
        // Every subsequent observation claims an earlier instant.
        for t in (0..1000).rev() {
            let a = s.observe(&battery(t, 10, 60), &c);
            assert!(!a.is_shutdown(), "shut down on a backwards clock at t={t}");
        }
    }

    /// Found by review. `warn_before_s < runtime_threshold_s` was the only
    /// check, so a 299 s warning against a 300 s threshold with a 120 s
    /// shutdown behind it passed -- and began going down with no predicted
    /// runtime left at all.
    #[test]
    fn the_whole_sequence_has_to_fit_in_the_threshold() {
        let bad = Config { runtime_threshold_s: 300, warn_before_s: 299, os_shutdown_s: 120, ..cfg() };
        assert!(bad.validate().is_err(), "accepted a warning that eats the whole budget");

        // The default must still fit, or the shipped configuration is invalid.
        let d = Config::default();
        let needed = d.debounce_s + d.warn_before_s + OS_GRACE_ALLOWANCE_S + d.os_shutdown_s as u64;
        assert!(needed < d.runtime_threshold_s as u64, "default needs {needed} s of {}", d.runtime_threshold_s);
        assert!(d.validate().is_ok());
    }

    #[test]
    fn an_unbounded_backstop_is_not_a_backstop() {
        assert!(Config { max_on_battery_s: u64::MAX, ..cfg() }.validate().is_err());
    }

    /// The estimate the tray shows under its status line. Runtime ticks down
    /// at roughly real time, so runtime minus threshold *is* a time estimate,
    /// plus the debounce still owed; the backstop is a hard deadline that can
    /// beat it. Approximate on purpose and never load-bearing: nothing acts on
    /// it, and the charge floor can still fire first unannounced.
    #[test]
    fn the_eta_names_the_sooner_route() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        let o = battery(1, 90, 2000);
        s.observe(&o, &c);
        // Runtime 2000 s against a 300 s threshold: 1700 s to cross plus the
        // debounce, well inside the 1800 s backstop.
        let (eta, route) = s.shutdown_eta(&o, &c).expect("no eta during an outage");
        assert_eq!(route, EtaRoute::Runtime);
        assert_eq!(eta, 1700 + c.debounce_s);

        // A runtime that outlasts the backstop hands the estimate to it.
        let o = battery(2, 95, 3000);
        s.observe(&o, &c);
        let (eta, route) = s.shutdown_eta(&o, &c).unwrap();
        assert_eq!(route, EtaRoute::Backstop);
        assert_eq!(eta, c.max_on_battery_s - 1);
    }

    #[test]
    fn the_eta_owes_only_the_debounce_still_unserved() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        for t in 1..=1 + c.settle_s {
            s.observe(&battery(t, 90, 2000), &c);
        }
        // Past the settle window and already below the threshold: the clock
        // that remains is the debounce, and it is part-served.
        let t0 = 2 + c.settle_s;
        s.observe(&battery(t0, 80, 200), &c);
        let half = t0 + c.debounce_s / 2;
        let o = battery(half, 80, 200);
        s.observe(&o, &c);
        let (eta, route) = s.shutdown_eta(&o, &c).unwrap();
        assert_eq!(route, EtaRoute::Runtime);
        assert_eq!(eta, c.debounce_s - c.debounce_s / 2, "served debounce not credited");
    }

    #[test]
    fn there_is_no_eta_on_mains() {
        let c = cfg();
        let mut s = State::new();
        let o = mains(0);
        s.observe(&o, &c);
        assert_eq!(s.shutdown_eta(&o, &c), None);
    }

    /// With no runtime reading the backstop is the only clock left, which is
    /// also what the policy itself does.
    #[test]
    fn a_missing_runtime_rides_the_backstop() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        let o = Observation { runtime_s: None, ..battery(1, 90, 0) };
        s.observe(&o, &c);
        let (eta, route) = s.shutdown_eta(&o, &c).unwrap();
        assert_eq!(route, EtaRoute::Backstop);
        // Latched at this very observation, so the whole limit remains.
        assert_eq!(eta, c.max_on_battery_s);
    }

    /// The settle window floors the estimate: nothing fires inside it, however
    /// low the numbers already are, so the estimate must not promise sooner.
    #[test]
    fn the_settle_window_floors_the_eta() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        // Crashed straight through the threshold at the moment of transfer.
        let o = battery(1, 10, 60);
        s.observe(&o, &c);
        let (eta, _) = s.shutdown_eta(&o, &c).unwrap();
        assert!(
            eta >= c.settle_s,
            "promised {eta} s inside a {} s settle window",
            c.settle_s
        );
    }

    /// Found by review. `shutdown_on_wake` used to live outside the policy as a
    /// bare commitment flag, and the very next pass observed a non-shutdown
    /// action and cancelled it -- the option could never actually fire. Routed
    /// through the policy it uses the same announce, grace and cancel machinery
    /// as every other reason.
    #[test]
    fn waking_alone_onto_battery_shuts_down_promptly_when_opted_in() {
        let c = Config { shutdown_on_wake: true, ..cfg() };
        let mut s = State::new();
        s.observe(&mains(0), &c);
        // The resume is noticed before the first post-resume reading lands...
        s.observe(&Observation { wake: WakeEvent::Alone, ..mains(10) }, &c);
        // ...and the first believed reading says battery. No settle, no
        // debounce: the whole point is not spending the battery on an idle box.
        assert_eq!(s.observe(&battery(12, 90, 2000), &c), Action::Shutdown(Why::Wake));
        // Sticky past the attribution window: the grace period may outlast it
        // and the decision must not un-decide itself mid-countdown.
        for t in 13..13 + WAKE_ATTRIBUTION_S * 2 {
            assert!(s.observe(&battery(t, 90, 2000), &c).is_shutdown(), "un-decided at t={t}");
        }
    }

    #[test]
    fn the_wake_route_is_off_by_default() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&Observation { wake: WakeEvent::Alone, ..mains(10) }, &c);
        assert_eq!(s.observe(&battery(12, 90, 2000), &c), Action::Warn);
    }

    /// Windows adds the user-present broadcast when a person was involved,
    /// sometimes only once they touch the machine. Whatever the automatic
    /// broadcast already caused, a person present means the premise is gone.
    #[test]
    fn a_person_showing_up_cancels_the_wake_shutdown() {
        let c = Config { shutdown_on_wake: true, ..cfg() };
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&Observation { wake: WakeEvent::Alone, ..mains(10) }, &c);
        assert!(s.observe(&battery(12, 90, 2000), &c).is_shutdown());
        let a = s.observe(&Observation { wake: WakeEvent::Attended, ..battery(13, 90, 2000) }, &c);
        assert!(!a.is_shutdown(), "still shutting down with a person present");
    }

    /// An unattended wake on healthy mains -- Windows Update at 3 a.m. -- must
    /// not arm a prompt shutdown for an outage that starts hours later.
    #[test]
    fn an_unattended_wake_onto_mains_expires() {
        let c = Config { shutdown_on_wake: true, ..cfg() };
        let mut s = State::new();
        s.observe(&Observation { wake: WakeEvent::Alone, ..mains(0) }, &c);
        for t in 1..=WAKE_ATTRIBUTION_S + 1 {
            s.observe(&mains(t), &c);
        }
        let a = s.observe(&battery(WAKE_ATTRIBUTION_S + 2, 90, 2000), &c);
        assert_eq!(a, Action::Warn, "an expired wake still armed the shortcut");
    }

    /// Mains returning cancels a wake shutdown exactly like any other, and the
    /// wake does not survive the recovery to shortcut the *next* outage.
    #[test]
    fn mains_returning_retires_the_wake_route() {
        let c = Config { shutdown_on_wake: true, ..cfg() };
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&Observation { wake: WakeEvent::Alone, ..battery(1, 90, 2000) }, &c);
        assert!(s.observe(&battery(2, 90, 2000), &c).is_shutdown());
        for t in 3..3 + c.debounce_s + 1 {
            let a = s.observe(&mains(t), &c);
            assert!(!a.is_shutdown(), "wake shutdown survived fresh mains at t={t}");
        }
        assert!(!s.on_battery());
        // A second outage still inside the attribution window runs the normal
        // route: the episode the wake belonged to ended when mains held.
        let t = 3 + c.debounce_s + 2;
        assert_eq!(s.observe(&battery(t, 90, 2000), &c), Action::Warn);
    }

    /// Found by review. During the mains-return hysteresis the latch is still
    /// set, and the thresholds used to keep evaluating against fresh *mains*
    /// observations -- so a qualifying streak begun during the outage could
    /// complete its debounce and fire a shutdown seconds after power came back.
    /// With `warn_before_s = 0`, which validate allows, that is a machine
    /// shutting down on restored mains.
    #[test]
    fn a_threshold_cannot_fire_while_fresh_readings_say_mains() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        // A healthy outage, held long enough to leave the settle window.
        for t in 1..=1 + c.settle_s {
            s.observe(&battery(t, 90, 2000), &c);
        }
        // Deep into the outage, the charge crosses the floor...
        let t0 = 2 + c.settle_s;
        for t in t0..t0 + c.debounce_s / 2 {
            assert_eq!(s.observe(&battery(t, 10, 2000), &c), Action::Warn, "at t={t}");
        }
        // ...and then mains returns, with the charge still low, because the
        // charge model recovers over hours. Nothing may fire on the way out.
        for t in t0 + c.debounce_s / 2..t0 + c.debounce_s * 3 {
            let o = Observation { charge: Some(10), ..mains(t) };
            let a = s.observe(&o, &c);
            assert!(!a.is_shutdown(), "shut down at t={t} with mains freshly confirmed");
        }
        assert!(!s.on_battery(), "the latch never cleared");
    }

    /// The same hole, through the backstop. A machine that slept through the
    /// end of an outage resumes with the latch set and the deadline long past;
    /// the first observation it acts on must not be a shutdown when that same
    /// observation freshly says mains is back.
    #[test]
    fn the_backstop_defers_to_a_believed_mains_return() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        // Way past the backstop, and the world says mains.
        let t0 = 1 + c.max_on_battery_s * 2;
        for t in t0..=t0 + c.debounce_s {
            let a = s.observe(&mains(t), &c);
            assert!(!a.is_shutdown(), "backstop fired at t={t} against fresh mains");
        }
        assert!(!s.on_battery());
    }

    /// Found by a live near-miss, 2026-08-03. The UPS wedged its USB interface
    /// at mains-return, the latch was held, and an armed backstop counted
    /// toward shutting down a machine on healthy mains -- through device
    /// silence, exactly as designed. Windows' battery driver is an independent
    /// read path to the same hardware, and its "AC present" defers the
    /// backstop -- but only after it reported the outage too. The edge is what
    /// makes the reading evidence rather than a cached leftover.
    #[test]
    fn the_backstop_defers_to_the_os_saying_mains_came_back() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        // The OS sees the outage too...
        s.observe(
            &Observation { os_ac_present: Some(false), ..battery(2, 90, 2000) },
            &c,
        );
        // ...the device goes silent, the deadline passes, and the OS reports
        // mains back: the 0 -> 1 edge happened inside this outage, so it
        // counts.
        let quiet_with_ac = |t| Observation {
            fresh: false,
            os_ac_present: Some(true),
            ..battery(t, 90, 2000)
        };
        let t = 1 + c.max_on_battery_s + 100;
        assert!(
            !s.observe(&quiet_with_ac(t), &c).is_shutdown(),
            "shut down on healthy mains through a wedged device"
        );
        // The OS stops being able to say: the backstop is back in charge.
        let quiet_unknown = Observation { fresh: false, os_ac_present: None, ..battery(t + 1, 90, 2000) };
        assert_eq!(s.observe(&quiet_unknown, &c), Action::Shutdown(Why::Backstop));
    }

    /// Two reviews, independently: an indefinite reprieve is its own hazard.
    /// A sustained OS mains-return during device silence must *end the
    /// outage*, not suspend the backstop forever -- otherwise a wedge that
    /// outlives the outage leaves a latch nothing can clear, and the next real
    /// outage finds the backstop still deferred by a value Windows may simply
    /// be repeating back. Ending it puts the agent in a state it already
    /// understands: blind on mains, where silence is not an emergency.
    #[test]
    fn a_sustained_os_mains_return_ends_the_outage_rather_than_deferring_forever() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        // The OS sees the outage too, then the device wedges and the OS
        // reports mains back for longer than the debounce.
        s.observe(&Observation { os_ac_present: Some(false), ..battery(2, 90, 2000) }, &c);
        let silent_ac = |t| Observation {
            fresh: false,
            os_ac_present: Some(true),
            ..battery(t, 90, 2000)
        };
        for t in 3..=3 + c.debounce_s {
            assert!(!s.observe(&silent_ac(t), &c).is_shutdown(), "acted at t={t}");
        }
        assert!(!s.on_battery(), "the latch survived a sustained OS mains return");

        // ...so the deadline it was counting toward is simply gone, and no
        // later silence resurrects it.
        for t in 3 + c.debounce_s..3 + c.debounce_s + c.max_on_battery_s * 2 {
            let o = Observation { fresh: false, os_ac_present: None, ..battery(t, 90, 2000) };
            assert!(!s.observe(&o, &c).is_shutdown(), "a cleared outage fired at t={t}");
        }
    }

    /// The second review's counter-case: the OS reading is an independent
    /// path, not an independent sensor, and it carries no freshness. A cached
    /// "AC present" that never once said offline during this outage is a
    /// leftover, not evidence, and deferring on it forever is how a wedge that
    /// persists into a real outage becomes a battery-exhaustion power cut.
    #[test]
    fn a_cached_mains_reading_with_no_edge_cannot_defer_the_backstop() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        // The outage latches from the device, but the OS said AC the whole
        // time -- its driver never saw the outage at all.
        s.observe(
            &Observation { os_ac_present: Some(true), ..battery(1, 90, 2000) },
            &c,
        );
        let quiet_stale_ac = |t| Observation {
            fresh: false,
            os_ac_present: Some(true),
            ..battery(t, 90, 2000)
        };
        let a = s.observe(&quiet_stale_ac(1 + c.max_on_battery_s), &c);
        assert_eq!(a, Action::Shutdown(Why::Backstop), "a stale cached AC=1 deferred the backstop");
    }

    /// The OS saying "on battery" changes nothing anywhere: it is corroboration
    /// of the outage, not a reprieve, and the thresholds already run on the
    /// device's own fresher numbers.
    #[test]
    fn the_os_saying_battery_is_not_a_reprieve() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        let quiet = |t| Observation { fresh: false, os_ac_present: Some(false), ..battery(t, 90, 2000) };
        let a = s.observe(&quiet(1 + c.max_on_battery_s), &c);
        assert_eq!(a, Action::Shutdown(Why::Backstop));
    }

    /// ...but a mains report the device has since gone silent on does not keep
    /// deferring it forever. Once the reading is stale the backstop is the only
    /// protection left, and it must come back.
    #[test]
    fn a_stale_mains_report_does_not_disarm_the_backstop() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        s.observe(&battery(1, 90, 2000), &c);
        // One fresh flicker of mains, then silence with the status held there.
        s.observe(&mains(2), &c);
        let quiet = |t| Observation { fresh: false, ..mains(t) };
        let a = s.observe(&quiet(1 + c.max_on_battery_s + c.stale_after_s + 1), &c);
        assert_eq!(a, Action::Shutdown(Why::Backstop), "the backstop never came back");
    }

    /// Found by review. A qualifying sample, a long stretch with no trustworthy
    /// reading, then one more qualifying sample used to satisfy "held
    /// continuously" -- defeating the jitter protection with a gap rather than
    /// with data.
    #[test]
    fn a_stale_gap_restarts_the_debounce() {
        let c = cfg();
        let mut s = State::new();
        s.observe(&mains(0), &c);
        let t0 = c.settle_s + 1;
        // One qualifying reading.
        assert_eq!(s.observe(&battery(t0, 80, 120), &c), Action::Warn);
        // Then nothing believable for well past the staleness window.
        for t in t0 + 1..t0 + 1 + c.stale_after_s * 3 {
            s.observe(&Observation { fresh: false, ..battery(t, 80, 120) }, &c);
        }
        // A single fresh qualifying reading must not now count as sustained.
        let t = t0 + 1 + c.stale_after_s * 3;
        assert_eq!(
            s.observe(&battery(t, 80, 120), &c),
            Action::Warn,
            "a gap counted as the condition holding"
        );
    }
}
