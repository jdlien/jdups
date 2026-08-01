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
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Why::DeviceImminent => "the UPS says shutdown is imminent",
            Why::Backstop => "on battery past the maximum",
            Why::Runtime => "predicted runtime below the threshold",
            Why::Charge => "charge below the floor",
        }
    }
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
    /// Warn for this long before actually shutting down.
    ///
    /// The decision is made, announced, and only then acted on. PowerChute puts
    /// up a modal dialog with an OK button at this moment, which is worse than
    /// it looks: a shutdown that can be stalled by a dialog nobody is present to
    /// dismiss means the battery decides when the machine goes down. A
    /// notification informs without being able to block, which is the whole
    /// difference.
    ///
    /// It comes out of the runtime budget, so it is not free — every second
    /// spent warning is a second not spent shutting down.
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
        // Not bounded below: zero is a legitimate choice for a machine nobody
        // sits at, where the warning has no one to reach and the seconds are
        // better spent shutting down.
        if self.warn_before_s > 600 {
            return Err("warn_before_s above ten minutes spends the battery on a notification");
        }
        if self.warn_before_s >= self.runtime_threshold_s as u64 {
            return Err("warn_before_s must leave time to shut down after the warning");
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

    /// Fold in one observation and say what to do.
    pub fn observe(&mut self, o: &Observation, cfg: &Config) -> Action {
        if o.fresh {
            self.last_fresh = Some(o.now_s);
        }

        // --- the latch --------------------------------------------------
        // A confirmed outage stays confirmed until mains is *freshly* confirmed
        // back, for long enough to not be a flap. Silence never clears it.
        if o.fresh {
            if o.on_battery {
                self.mains_since = None;
                if self.outage_since.is_none() {
                    self.outage_since = Some(o.now_s);
                    self.qualifying_since = None;
                }
            } else {
                let since = *self.mains_since.get_or_insert(o.now_s);
                if o.now_s.saturating_sub(since) >= cfg.debounce_s {
                    self.outage_since = None;
                    self.qualifying_since = None;
                }
            }
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

        // --- the backstop -----------------------------------------------
        // Runs before the staleness check on purpose. If the device went quiet
        // *during* an outage, the deadline is the only thing left, and it must
        // still fire.
        if on_battery_for >= cfg.max_on_battery_s {
            return Action::Shutdown(Why::Backstop);
        }

        // --- staleness, which is asymmetric -----------------------------
        // Before an outage, unknown is not an emergency. *During* one, losing
        // the device is not permission to relax — that is precisely the case
        // where doing nothing runs the battery flat. Hold, warn, and let the
        // backstop above decide.
        let stale = self
            .last_fresh
            .is_none_or(|t| o.now_s.saturating_sub(t) > cfg.stale_after_s);
        if stale {
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
}
