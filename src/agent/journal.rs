//! What the agent writes down, and when.
//!
//! Split out from the device loop because it is the part that can be wrong in a
//! quiet way. The loop either reads the UPS or it does not; this decides whether
//! a six-week dry run produces a readable account of what the agent would have
//! done, or four hundred thousand identical lines.
//!
//! Two failure modes to steer between, and both are real:
//!
//! - **Log every tick** and the file is unreadable, so nobody reads it, so the
//!   dry run proves nothing.
//! - **Log only transitions** and an outage that lasts twenty minutes leaves two
//!   lines, neither of which says how the battery actually behaved on the way
//!   down. The discharge curve *is* the evidence.
//!
//! So: every change of decision, plus a heartbeat while on battery.

use jdups::policy::{Action, Observation};

/// A heartbeat this often while on battery, so the discharge is visible.
/// Nothing is written at this cadence on mains.
const BEAT_S: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    /// Something was done, or in dry run would have been.
    Act,
}

impl Level {
    pub fn tag(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Act => "ACT ",
        }
    }
}

/// One look at the world, already decided on.
pub struct Tick<'a> {
    pub now_s: u64,
    pub action: Action,
    pub obs: &'a Observation,
    /// How long the outage has been latched, per `policy::State`.
    pub on_battery_for: Option<u64>,
    /// False once the agent has been told it may actually act.
    pub dry_run: bool,
    /// The device has genuinely stopped answering, per the configured window.
    ///
    /// Not `!obs.fresh`. That flag means "no read landed on *this* pass", and
    /// the loop runs several times per poll, so three passes in four are not
    /// fresh even when the device is perfectly healthy. Using it here labelled
    /// almost every heartbeat NOT FRESH during a real outage, which is worse
    /// than useless: it trains the reader to ignore the word on the one
    /// occasion it matters.
    pub stale: bool,
}

#[derive(Debug, Default)]
pub struct Journal {
    last: Option<Action>,
    /// Whether the shutdown decision has already been announced for this
    /// outage. Without this, a dry run that decides to shut down goes on
    /// deciding it every two seconds until mains returns.
    announced: bool,
    last_beat_s: u64,
}

impl Journal {
    pub fn new() -> Journal {
        Journal::default()
    }

    /// Fold in one decided tick, and say what to write, if anything.
    pub fn note(&mut self, t: &Tick) -> Option<(Level, String)> {
        let changed = self.last != Some(t.action);
        let due = t.now_s.saturating_sub(self.last_beat_s) >= BEAT_S;
        let prev = self.last;
        self.last = Some(t.action);

        match t.action {
            Action::Shutdown(why) => {
                let elapsed = t.on_battery_for.unwrap_or(0);
                if !self.announced {
                    self.announced = true;
                    self.last_beat_s = t.now_s;
                    let verb = if t.dry_run { "would shut down now" } else { "shutting down now" };
                    return Some((
                        Level::Act,
                        format!(
                            "{verb}: {} ({}, {} on battery)",
                            why.as_str(),
                            numbers(t.obs, t.stale),
                            duration(elapsed),
                        ),
                    ));
                }
                // Already said. Keep beating, because how much longer the
                // battery actually lasted past the trigger is the number that
                // tells you whether the threshold is set anywhere near right.
                if due {
                    self.last_beat_s = t.now_s;
                    return Some((
                        Level::Warn,
                        format!(
                            "still past the trigger: {} ({} on battery)",
                            numbers(t.obs, t.stale),
                            duration(elapsed),
                        ),
                    ));
                }
                None
            }

            Action::Warn => {
                let elapsed = t.on_battery_for.unwrap_or(0);
                if changed && !prev.is_some_and(|p| p.is_shutdown()) {
                    self.last_beat_s = t.now_s;
                    return Some((
                        Level::Warn,
                        format!("on battery: {}", numbers(t.obs, t.stale)),
                    ));
                }
                if due {
                    self.last_beat_s = t.now_s;
                    return Some((
                        Level::Warn,
                        format!("on battery {}: {}", duration(elapsed), numbers(t.obs, t.stale)),
                    ));
                }
                None
            }

            Action::Nothing => {
                self.announced = false;
                // Only interesting as a return. Mains staying up is not news,
                // and this is the one state the machine spends its life in.
                if changed && prev.is_some_and(|p| p != Action::Nothing) {
                    return Some((Level::Info, format!("back on mains: {}", numbers(t.obs, t.stale))));
                }
                None
            }
        }
    }
}

/// Charge and runtime, with the missing cases spelled out rather than skipped.
///
/// A line that silently omits runtime looks like a line about a healthy device.
fn numbers(o: &Observation, stale: bool) -> String {
    let mut s = String::new();
    match o.charge {
        Some(c) => s.push_str(&format!("{c}%")),
        None => s.push_str("charge unknown"),
    }
    match o.runtime_s {
        Some(r) => s.push_str(&format!(", {} left ({r} s)", duration(r as u64))),
        None => s.push_str(", runtime unknown"),
    }
    if stale {
        s.push_str(", DEVICE NOT ANSWERING");
    }
    s
}

/// Seconds as something a human reads without arithmetic.
fn duration(s: u64) -> String {
    if s < 90 {
        return format!("{s} s");
    }
    let m = (s + 30) / 60;
    if m < 90 {
        return format!("{m} min");
    }
    format!("{} h {} min", m / 60, m % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jdups::policy::Why;

    fn obs(now_s: u64, on_battery: bool, charge: u8, runtime_s: u16) -> Observation {
        Observation {
            now_s,
            fresh: true,
            on_battery,
            shutdown_imminent: false,
            charge: Some(charge),
            runtime_s: Some(runtime_s),
        }
    }

    /// Feed a run and collect only what was actually written.
    fn drive(j: &mut Journal, ticks: impl IntoIterator<Item = (u64, Action, bool)>) -> Vec<(Level, String)> {
        let mut out = Vec::new();
        for (now_s, action, on_batt) in ticks {
            let o = obs(now_s, on_batt, 80, 1800);
            let t = Tick {
                now_s,
                action,
                obs: &o,
                on_battery_for: on_batt.then_some(now_s),
                dry_run: true,
                stale: false,
            };
            if let Some(line) = j.note(&t) {
                out.push(line);
            }
        }
        out
    }

    /// The whole point of the module. A machine sitting on mains for a month
    /// must produce nothing at all.
    #[test]
    fn mains_writes_nothing_ever() {
        let mut j = Journal::new();
        let lines = drive(&mut j, (0..100_000).map(|t| (t, Action::Nothing, false)));
        assert!(lines.is_empty(), "wrote {} lines on mains", lines.len());
    }

    #[test]
    fn going_on_battery_says_so_once_then_beats() {
        let mut j = Journal::new();
        // Five minutes on battery, a tick every second.
        let lines = drive(&mut j, (0..300).map(|t| (t, Action::Warn, true)));
        // One on entry, then one a minute.
        assert_eq!(lines.len(), 1 + 4, "{lines:#?}");
        assert!(lines[0].1.starts_with("on battery:"), "{:?}", lines[0]);
        assert!(lines.iter().all(|(l, _)| *l == Level::Warn));
    }

    /// The line that matters. It must be written exactly once per outage, and it
    /// must carry the reason.
    #[test]
    fn the_decision_is_announced_once_not_every_tick() {
        let mut j = Journal::new();
        let mut ticks: Vec<_> = (0..30).map(|t| (t, Action::Warn, true)).collect();
        ticks.extend((30..60).map(|t| (t, Action::Shutdown(Why::Runtime), true)));
        let lines = drive(&mut j, ticks);

        let acts: Vec<_> = lines.iter().filter(|(l, _)| *l == Level::Act).collect();
        assert_eq!(acts.len(), 1, "{lines:#?}");
        assert!(acts[0].1.contains("would shut down now"), "{:?}", acts[0]);
        assert!(acts[0].1.contains("runtime below the threshold"), "{:?}", acts[0]);
    }

    /// ...but the log must not go silent afterwards, because how long the
    /// battery lasted *past* the trigger is how the threshold gets tuned.
    #[test]
    fn it_keeps_beating_after_the_decision() {
        let mut j = Journal::new();
        let lines = drive(
            &mut j,
            (0..300).map(|t| (t, Action::Shutdown(Why::Runtime), true)),
        );
        assert_eq!(lines[0].0, Level::Act);
        assert!(lines.len() >= 4, "went quiet after deciding: {lines:#?}");
        assert!(lines[1..].iter().all(|(_, s)| s.starts_with("still past the trigger")));
    }

    #[test]
    fn mains_returning_is_worth_one_line() {
        let mut j = Journal::new();
        let mut ticks: Vec<_> = (0..10).map(|t| (t, Action::Warn, true)).collect();
        ticks.extend((10..100).map(|t| (t, Action::Nothing, false)));
        let lines = drive(&mut j, ticks);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert!(lines[1].1.starts_with("back on mains"), "{:?}", lines[1]);
    }

    /// A second outage has to announce again. The latch is per-outage, and one
    /// that never cleared would silently swallow the second event.
    #[test]
    fn a_second_outage_announces_again() {
        let mut j = Journal::new();
        let mut ticks: Vec<_> = (0..5).map(|t| (t, Action::Shutdown(Why::Runtime), true)).collect();
        ticks.extend((5..40).map(|t| (t, Action::Nothing, false)));
        ticks.extend((40..45).map(|t| (t, Action::Shutdown(Why::Charge), true)));
        let lines = drive(&mut j, ticks);
        let acts: Vec<_> = lines.iter().filter(|(l, _)| *l == Level::Act).collect();
        assert_eq!(acts.len(), 2, "{lines:#?}");
        assert!(acts[1].1.contains("charge below the floor"), "{:?}", acts[1]);
    }

    /// Dropping back from Shutdown to Warn must not re-announce the outage as
    /// though it had just started. It cannot happen from the policy today, but
    /// this is the module that would render it as a lie if it did.
    #[test]
    fn falling_back_to_warn_does_not_look_like_a_new_outage() {
        let mut j = Journal::new();
        let mut ticks: Vec<_> = (0..5).map(|t| (t, Action::Shutdown(Why::Runtime), true)).collect();
        ticks.extend((5..10).map(|t| (t, Action::Warn, true)));
        let lines = drive(&mut j, ticks);
        assert!(
            !lines.iter().any(|(_, s)| s.starts_with("on battery:")),
            "claimed a fresh outage: {lines:#?}"
        );
    }

    /// A device that has genuinely stopped answering has to be labelled. The
    /// numbers still print, because a stale number is evidence; it is just not
    /// the same evidence as a current one.
    #[test]
    fn a_device_that_stopped_answering_is_marked() {
        let o = obs(10, true, 50, 600);
        let mut j = Journal::new();
        let line = j
            .note(&Tick { now_s: 10, action: Action::Warn, obs: &o, on_battery_for: Some(10), dry_run: true, stale: true })
            .unwrap();
        assert!(line.1.contains("DEVICE NOT ANSWERING"), "{line:?}");
        assert!(line.1.contains("50%"), "{line:?}");
    }

    /// ...and a healthy one is not, however many individual passes happened to
    /// carry no read. This is the regression: `Observation::fresh` is per-pass,
    /// the loop runs several times per poll, and using it here put a warning on
    /// nearly every heartbeat of a real outage.
    #[test]
    fn a_pass_without_a_read_is_not_a_missing_device() {
        let o = Observation { fresh: false, ..obs(10, true, 50, 600) };
        let mut j = Journal::new();
        let line = j
            .note(&Tick { now_s: 10, action: Action::Warn, obs: &o, on_battery_for: Some(10), dry_run: true, stale: false })
            .unwrap();
        assert!(!line.1.contains("NOT ANSWERING"), "{line:?}");
    }

    #[test]
    fn missing_numbers_say_so_rather_than_vanishing() {
        let o = Observation { charge: None, runtime_s: None, ..obs(10, true, 0, 0) };
        let mut j = Journal::new();
        let line = j
            .note(&Tick { now_s: 10, action: Action::Warn, obs: &o, on_battery_for: Some(10), dry_run: true, stale: false })
            .unwrap();
        assert!(line.1.contains("charge unknown"), "{line:?}");
        assert!(line.1.contains("runtime unknown"), "{line:?}");
    }

    /// An armed agent says what it did, not what it would do. Getting this
    /// backwards in the log would be its own small disaster during an incident.
    #[test]
    fn armed_and_dry_run_read_differently() {
        let o = obs(10, true, 20, 100);
        let mut dry = Journal::new();
        let mut armed = Journal::new();
        let t = |dry_run| Tick {
            now_s: 10,
            action: Action::Shutdown(Why::Runtime),
            obs: &o,
            on_battery_for: Some(10),
            dry_run,
            stale: false,
        };
        assert!(dry.note(&t(true)).unwrap().1.starts_with("would shut down"));
        assert!(armed.note(&t(false)).unwrap().1.starts_with("shutting down"));
    }

    #[test]
    fn durations_read_like_english() {
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(600), "10 min");
        assert_eq!(duration(1800), "30 min");
        assert_eq!(duration(7200), "2 h 0 min");
        assert_eq!(duration(9000), "2 h 30 min");
    }
}
