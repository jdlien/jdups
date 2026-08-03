//! What a reading *is*, and how it reads to a human.
//!
//! Pure. The console readout, the tooltip, the menu and the clipboard all
//! render from here, so they cannot drift apart.

use crate::decode::{self, PresentStatus};

/// One complete sweep of the device.
///
/// Every field is optional because a partial reading is still worth showing —
/// one unreadable report should leave a gap, not blank the menu.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Reading {
    pub charge: Option<u8>,
    pub runtime_s: Option<u16>,
    pub load_pct: Option<u8>,
    pub rated_watts: Option<u16>,
    pub input_volts: Option<u16>,
    pub battery_volts: Option<f32>,
    pub battery_installed: Option<(i32, u32, u32)>,
    pub last_transfer: Option<u8>,
    pub shutdown_delay: Option<i16>,
    /// `AudibleAlarmControl`: 1 disabled, 2 enabled, 3 muted. `None` until read.
    pub alarm: Option<u8>,
    pub status: PresentStatus,
    /// Whether `status` was actually read, as opposed to defaulted. A defaulted
    /// `PresentStatus` has `ac_present: false`, which reads as "on battery" —
    /// so the difference between "on battery" and "we don't know" has to be
    /// carried explicitly rather than inferred from the flags.
    pub have_status: bool,
}

impl Reading {
    /// Load in watts, against the device's own rated output.
    pub fn watts(&self) -> Option<u32> {
        Some(decode::watts(self.load_pct?, self.rated_watts?))
    }

    pub fn runtime_min(&self) -> Option<u32> {
        self.runtime_s.map(|s| (s as u32 + 30) / 60)
    }

    /// True only when we have actually read the flags and they say so.
    pub fn on_battery(&self) -> bool {
        self.have_status && self.status.on_battery()
    }

    /// The one-line summary that heads the menu and fills the tooltip.
    pub fn status_line(&self) -> String {
        if !self.have_status {
            return "Status unknown".into();
        }
        // "Online", not PowerChute's "On line". One word is the canonical
        // spelling, and the log's event tag was already `online` -- so the two
        // halves of this program disagreed with each other.
        let where_ = if self.status.on_battery() {
            "On battery"
        } else {
            "Online"
        };
        match (self.charge, self.runtime_min()) {
            (Some(c), Some(m)) => format!("{where_}, {c}%, {m} min"),
            (Some(c), None) => format!("{where_}, {c}%"),
            _ => where_.to_string(),
        }
    }

    /// The digits to paint into the icon: **always minutes of runtime.**
    ///
    /// An earlier version showed charge on mains and minutes on battery. That
    /// was ambiguous and it was also redundant, which is the part that settles
    /// it. Ambiguous because the two ranges overlap — "31" is a believable
    /// percentage *and* a believable number of minutes, and a colour change does
    /// not announce that the units just moved. Redundant because **the fill bar
    /// already is the charge percentage**; spending the digits on it too said one
    /// thing twice and the other thing never.
    ///
    /// So the icon carries two quantities on two channels: fill for charge,
    /// digits for time. Minutes is also the number that survives the load
    /// question — 20 % is half an hour at light load and ninety seconds at heavy
    /// — and on mains it reads as "how long could I survive right now", which is
    /// exactly what a UPS is for.
    ///
    /// Nothing at all when the UPS is resting full on mains: a permanent
    /// number is chartjunk on a 16 px canvas. The threshold of 99 turned out
    /// to be load-bearing, not slack: this unit's charge **tops out at 99 %
    /// and never reports 100** (observed across full recharge cycles,
    /// 2026-08-02), so a threshold of 100 would have meant digits that never
    /// rest at all.
    ///
    /// Never returns three digits — minutes cap at 99 — which is what makes the
    /// two-digit layout sufficient. Above that the answer is "you are fine", and
    /// the exact figure is in the tooltip.
    pub fn icon_digits(&self) -> Option<u8> {
        if !self.have_status {
            return None;
        }
        // Full and on mains: nothing worth saying.
        if !self.status.on_battery() && self.charge.is_some_and(|c| c >= FULL_ENOUGH) {
            return None;
        }
        // **Never invent a zero.** This used to be `unwrap_or(0)`, which paints
        // "0" whenever runtime is merely unknown — and "0" on a battery icon
        // reads as *no runtime left*, which is the most frightening thing this
        // program can say and was, when it happened, wrong by twenty-five
        // minutes. An unknown number gets no digits; the bar still shows charge
        // and the colour still shows where the power is coming from.
        Some(self.runtime_min()?.min(99) as u8)
    }

    /// Multi-line, for the clipboard. Every menu data row copies this.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let na = |v: Option<String>| v.unwrap_or_else(|| "n/a".into());

        s.push_str(&format!("{}\n", self.status_line()));
        s.push_str(&format!(
            "Load               {}\n",
            na(self.load_pct.map(|l| match self.watts() {
                Some(w) => format!("{l}%  ({w} W)"),
                None => format!("{l}%"),
            }))
        ));
        s.push_str(&format!(
            "Input              {}\n",
            na(self.input_volts.map(|v| format!("{v} V")))
        ));
        s.push_str(&format!(
            "Battery            {}\n",
            na(self.battery_volts.map(|v| format!("{v:.2} V")))
        ));
        s.push_str(&format!(
            "Runtime            {}\n",
            na(self
                .runtime_s
                .map(|r| format!("{r} s ({} min)", (r as u32 + 30) / 60)))
        ));
        s.push_str(&format!(
            "Battery installed  {}\n",
            na(self
                .battery_installed
                .map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}")))
        ));
        s
    }
}

/// At or above this, the UPS is "full" for icon purposes.
pub const FULL_ENOUGH: u8 = 99;

/// Below this much runtime on battery, the readout goes red.
///
/// Display only — nothing acts on it. The agent's own threshold is a separate,
/// configured value, and deliberately not this one: a colour being wrong is a
/// cosmetic bug, and a shutdown threshold being wrong is not.
pub const CRITICAL_RUNTIME_S: u16 = 300;

/// Where the power is coming from, as far as the display is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Mains,
    Battery,
    Critical,
    /// Nothing read yet, or the device stopped answering.
    Unknown,
}

impl Reading {
    pub fn power(&self) -> Power {
        if !self.have_status {
            return Power::Unknown;
        }
        if !self.status.on_battery() {
            return Power::Mains;
        }
        match self.runtime_s {
            Some(s) if s <= CRITICAL_RUNTIME_S => Power::Critical,
            _ if self.status.shutdown_imminent => Power::Critical,
            _ => Power::Battery,
        }
    }
}

/// Smooths the two fields that arrive on the input stream.
///
/// Two separate reasons, and it is worth being clear that this fixes one of
/// them and only takes the edge off the other:
///
/// 1. **Quantisation noise.** At a dead-steady load the device cycles between
///    three runtime values ~90 s apart — a ±3.5 % spread with nothing changing.
///    A median removes that outright.
/// 2. **The transfer sag.** On losing mains, the reported charge and runtime
///    fall steeply for the first several seconds: 100 % to 80 % in ten. That is
///    *not* energy leaving the battery. APC estimates charge from terminal
///    voltage, and voltage sags the moment a load is applied, harder as a
///    battery ages. It recovers to a truer figure once the reading settles.
///
/// A median cannot and should not hide (2) — it is a real reading — but it does
/// stop the icon flickering through every intermediate value on the way down.
///
/// **The agent in Phase 8 needs more than this.** A shutdown decision taken
/// during the sag would fire on a number that is about to correct itself
/// upward, so it must additionally ignore the first several seconds after a
/// transfer. See the plan.
#[derive(Debug, Default, Clone)]
pub struct Smoother {
    charge: Vec<u8>,
    runtime: Vec<u16>,
    shown_charge: Option<u8>,
    shown_runtime: Option<u16>,
    /// Candidate value and how many consecutive samples it has held for.
    pending_charge: Option<(u8, u32)>,
    pending_runtime: Option<(u16, u32)>,
}

impl Smoother {
    /// Nine samples is roughly fifteen to thirty seconds of stream.
    ///
    /// Five was not enough. The device cycles through its quantised values, so a
    /// short window's *median* still steps as the cycle drifts through it.
    pub const WINDOW: usize = 9;

    /// How many consecutive samples a new median must hold before it is adopted.
    ///
    /// **Dwell, not a magnitude band.** The first version held the shown value
    /// until the median moved 120 s away from it, which stopped the twitching
    /// but had an ugly consequence: the value could then only ever move in jumps
    /// of two minutes or more, so roughly every other minute was unreachable.
    /// It stepped 33 -> 35 while PowerChute sat on 34, and 34 was not a value it
    /// could physically display.
    ///
    /// Waiting for *persistence* instead separates the two things properly.
    /// Jitter never persists — the median flips back before the count is met —
    /// while a genuine drift holds its new value and gets adopted, one minute at
    /// a time, passing through every number on the way.
    pub const DWELL: u32 = 6;

    /// A change this large is real by inspection and skips the dwell.
    ///
    /// Losing mains must not wait six samples to show up.
    pub const RUNTIME_JUMP_S: u16 = 300;
    pub const CHARGE_JUMP_PCT: u8 = 5;

    pub fn new() -> Smoother {
        Smoother::default()
    }

    pub fn push_charge(&mut self, v: u8) {
        push_capped(&mut self.charge, v, Self::WINDOW);
        let Some(m) = median(&self.charge) else { return };
        let jump = self.shown_charge.is_some_and(|s| s.abs_diff(m) >= Self::CHARGE_JUMP_PCT);
        if settle(&mut self.pending_charge, m, self.shown_charge, jump) {
            self.shown_charge = Some(m);
        }
    }

    pub fn push_runtime(&mut self, v: u16) {
        push_capped(&mut self.runtime, v, Self::WINDOW);
        let Some(m) = median(&self.runtime) else { return };
        let jump = self
            .shown_runtime
            .is_some_and(|s| s.abs_diff(m) >= Self::RUNTIME_JUMP_S);
        if settle(&mut self.pending_runtime, m, self.shown_runtime, jump) {
            self.shown_runtime = Some(m);
        }
    }

    pub fn charge(&self) -> Option<u8> {
        self.shown_charge
    }

    pub fn runtime(&self) -> Option<u16> {
        self.shown_runtime
    }

    /// After a disconnect, nothing from before the gap should survive it.
    pub fn clear(&mut self) {
        self.charge.clear();
        self.runtime.clear();
        self.shown_charge = None;
        self.shown_runtime = None;
        self.pending_charge = None;
        self.pending_runtime = None;
    }
}

/// Should `candidate` become the shown value?
///
/// True on the first reading, on a jump large enough to be real by inspection,
/// or once the candidate has held for `DWELL` consecutive samples. A candidate
/// that changes resets the count, which is what makes jitter unable to get
/// through while a steady drift always does.
fn settle<T: Copy + PartialEq>(
    pending: &mut Option<(T, u32)>,
    candidate: T,
    shown: Option<T>,
    jump: bool,
) -> bool {
    if shown.is_none() || jump {
        *pending = None;
        return true;
    }
    if shown == Some(candidate) {
        *pending = None;
        return false;
    }
    let n = match *pending {
        Some((v, n)) if v == candidate => n + 1,
        _ => 1,
    };
    if n >= Smoother::DWELL {
        *pending = None;
        true
    } else {
        *pending = Some((candidate, n));
        false
    }
}

fn push_capped<T>(buf: &mut Vec<T>, v: T, cap: usize) {
    buf.push(v);
    if buf.len() > cap {
        buf.remove(0);
    }
}

fn median<T: Copy + Ord>(v: &[T]) -> Option<T> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    Some(s[s.len() / 2])
}

/// What the device thread publishes and the UI renders from.
///
/// Immutable, versioned, and carrying its own observation times: "some report
/// arrived recently" is not liveness, because the periodic reports keep coming
/// while a specific field goes stale.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub reading: Reading,
    pub device_ok: bool,
    pub error: Option<String>,
    /// Age of the most recent input report, in seconds.
    pub stream_age_s: Option<u64>,
    /// Age of the most recent full feature sweep, in seconds.
    pub sweep_age_s: Option<u64>,
}

impl Snapshot {
    /// Anything older than this is not worth showing as current.
    pub const STALE_AFTER_S: u64 = 30;

    /// Has the device stopped answering?
    ///
    /// **Either source counts.** This used to require a recent *input report*,
    /// which conflated "the stream is quiet" with "the UPS is gone" — and those
    /// come apart in a way that is not theoretical: across an S3 suspend and
    /// resume this device stops pushing input reports altogether while feature
    /// reads keep working perfectly, and reopening the handle does not bring the
    /// stream back. The tray then showed "UPS not responding (stale)" over a
    /// device that was answering every question put to it.
    ///
    /// A device serving feature reads is responding. The stream is how the
    /// urgent things arrive quickly, not what proves the UPS is there.
    pub fn is_stale(&self) -> bool {
        if !self.device_ok {
            return true;
        }
        let freshest = match (self.stream_age_s, self.sweep_age_s) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        freshest.is_none_or(|a| a > Self::STALE_AFTER_S)
    }

    /// What the icon should show. Stale or lost reads as `Unknown` rather than
    /// as the last good value, which would quietly go on looking healthy.
    pub fn power(&self) -> Power {
        if self.is_stale() {
            Power::Unknown
        } else {
            self.reading.power()
        }
    }

    pub fn status_line(&self) -> String {
        if !self.device_ok {
            return match &self.error {
                Some(e) => format!("UPS not responding: {e}"),
                None => "UPS not responding".into(),
            };
        }
        if self.is_stale() {
            return "UPS not responding (stale)".into();
        }
        self.reading.status_line()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{PAGE_BATTERY, PAGE_POWER};

    fn on_mains(charge: u8) -> Reading {
        Reading {
            charge: Some(charge),
            runtime_s: Some(2595),
            load_pct: Some(20),
            rated_watts: Some(900),
            status: PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD0), (PAGE_BATTERY, 0xD1)]),
            have_status: true,
            ..Default::default()
        }
    }

    fn on_battery(runtime_s: u16) -> Reading {
        Reading {
            charge: Some(80),
            runtime_s: Some(runtime_s),
            load_pct: Some(20),
            rated_watts: Some(900),
            status: PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD1), (PAGE_BATTERY, 0x45)]),
            have_status: true,
            ..Default::default()
        }
    }

    #[test]
    fn watts_needs_both_halves() {
        assert_eq!(on_mains(100).watts(), Some(180));
        let mut r = on_mains(100);
        r.rated_watts = None;
        assert_eq!(r.watts(), None);
    }

    #[test]
    fn runtime_rounds_to_the_nearest_minute() {
        assert_eq!(on_mains(100).runtime_min(), Some(43)); // 2595 s
        let mut r = on_mains(100);
        r.runtime_s = Some(89);
        assert_eq!(r.runtime_min(), Some(1));
        r.runtime_s = Some(91);
        assert_eq!(r.runtime_min(), Some(2));
    }

    /// The point of `have_status`: a defaulted PresentStatus has ac_present
    /// false, and reporting "On battery" because we failed to read anything
    /// would be a lie in the alarming direction.
    #[test]
    fn an_unread_status_is_unknown_not_on_battery() {
        let r = Reading {
            charge: Some(100),
            ..Default::default()
        };
        assert!(!r.on_battery());
        assert_eq!(r.status_line(), "Status unknown");
        assert_eq!(r.icon_digits(), None);
    }

    /// The 99 case is the one that exists: this unit's charge tops out at 99
    /// and never reports 100 (measured, full recharge cycles, 2026-08-02), so
    /// hiding only at 100 would be hiding never.
    #[test]
    fn a_full_unit_on_mains_shows_no_digits() {
        assert_eq!(on_mains(100).icon_digits(), None);
        assert_eq!(on_mains(99).icon_digits(), None);
    }

    /// The whole point of the change: the digits mean the same thing in every
    /// state, so a glance never has to work out which unit it is looking at.
    /// Charge lives on the fill bar, where it is not competing with anything.
    #[test]
    fn the_digits_are_always_minutes_never_percent() {
        // Recharging on mains at 78 %: 22 minutes of protection, not "78".
        let mut recharging = on_mains(78);
        recharging.runtime_s = Some(1337);
        assert_eq!(recharging.icon_digits(), Some(22));

        // On battery, same quantity, no switch.
        assert_eq!(on_battery(2595).icon_digits(), Some(43));
        assert_eq!(on_battery(90).icon_digits(), Some(2));
    }

    /// The ambiguity that prompted this: charge and minutes overlap in range, so
    /// a value that could be read either way must never be able to mean both.
    #[test]
    fn a_number_that_could_be_read_either_way_is_always_minutes() {
        let mut r = on_mains(31); // 31 % charge...
        r.runtime_s = Some(600); // ...but 10 minutes left
        assert_eq!(r.icon_digits(), Some(10), "showed the percentage");
    }

    /// The property the whole two-digit icon layout depends on.
    #[test]
    fn the_icon_never_needs_three_digits() {
        for charge in 0..=100u8 {
            for secs in [0u16, 90, 600, 2595, 6000, u16::MAX] {
                let mut r = on_mains(charge);
                r.runtime_s = Some(secs);
                if let Some(d) = r.icon_digits() {
                    assert!(d < 100, "charge {charge} runtime {secs} produced {d}");
                }
                let mut b = on_battery(secs);
                b.charge = Some(charge);
                if let Some(d) = b.icon_digits() {
                    assert!(d < 100, "battery charge {charge} runtime {secs} gave {d}");
                }
            }
        }
    }

    /// A full battery still shows minutes when the mains has gone — being at
    /// 100 % does not mean there is nothing to say.
    #[test]
    fn a_full_battery_on_mains_failure_still_shows_minutes() {
        let mut r = on_battery(600);
        r.charge = Some(100);
        assert_eq!(r.icon_digits(), Some(10));
    }

    #[test]
    fn the_status_line_names_where_the_power_is_coming_from() {
        assert_eq!(on_mains(100).status_line(), "Online, 100%, 43 min");
        assert_eq!(on_battery(600).status_line(), "On battery, 80%, 10 min");
    }

    /// The quantisation cycle this exists for: three adjacent runtime values at
    /// a dead-steady load, which a median flattens to the middle one.
    #[test]
    fn the_smoother_flattens_the_quantisation_cycle() {
        let mut s = Smoother::new();
        for v in [2508u16, 2595, 2688, 2595, 2508] {
            s.push_runtime(v);
        }
        assert_eq!(s.runtime(), Some(2508), "first sample sets it, cycle cannot move it");
    }

    /// The reported symptom: 36, 37, 36, 35, 36 minutes every few seconds. The
    /// median alone does not stop this — its own output steps as the cycle
    /// drifts through the window — so the shown value has to be sticky.
    #[test]
    fn the_shown_runtime_does_not_twitch_between_neighbours() {
        let mut s = Smoother::new();
        // Minutes: 36, 37, 36, 35, 36, ... in seconds, jittering either side.
        let cycle = [2160u16, 2220, 2160, 2100, 2160, 2220, 2100, 2160, 2220];
        for v in cycle {
            s.push_runtime(v);
        }
        let settled = s.runtime().unwrap();
        for _ in 0..40 {
            for v in cycle {
                s.push_runtime(v);
                assert_eq!(s.runtime(), Some(settled), "the shown value moved");
            }
        }
        // ...and it still lands on a sane minute.
        assert!((35..=37).contains(&((settled as u32 + 30) / 60)));
    }

    /// The defect a magnitude band caused: with a 120 s hysteresis window the
    /// shown value could only ever move in two-minute jumps, so it stepped
    /// 35 -> 33 and simply could not display 34 while PowerChute sat on it.
    /// Dwell has no such dead values.
    #[test]
    fn a_slow_drift_passes_through_every_minute() {
        let mut s = Smoother::new();
        for _ in 0..Smoother::WINDOW {
            s.push_runtime(2100); // 35 min
        }
        let mut seen = std::collections::BTreeSet::new();
        // Drift down to 33 min, slowly, the way a real discharge does.
        for step in 0..40 {
            let v = 2100 - step * 3;
            for _ in 0..Smoother::DWELL {
                s.push_runtime(v);
            }
            seen.insert((s.runtime().unwrap() as u32 + 30) / 60);
        }
        assert!(seen.contains(&34), "34 was never displayed; saw {seen:?}");
        assert!(seen.contains(&35) && seen.contains(&33), "saw {seen:?}");
    }

    /// A transfer must not wait out the dwell.
    #[test]
    fn a_large_jump_is_adopted_at_once() {
        let mut s = Smoother::new();
        for _ in 0..Smoother::WINDOW {
            s.push_runtime(2600);
        }
        assert_eq!(s.runtime(), Some(2600));
        // Mains lost: the estimate collapses. One sample past the median is
        // enough to move it.
        for _ in 0..Smoother::WINDOW {
            s.push_runtime(900);
        }
        assert_eq!(s.runtime(), Some(900));
    }

    /// Sticky must not mean stuck. A real decline has to get through.
    #[test]
    fn a_genuine_decline_still_moves_the_shown_value() {
        let mut s = Smoother::new();
        for v in [2400u16; 9] {
            s.push_runtime(v);
        }
        let before = s.runtime().unwrap();
        for v in (600..2400).rev().step_by(60) {
            s.push_runtime(v as u16);
        }
        let after = s.runtime().unwrap();
        assert!(after + 600 < before, "held at {before} while it fell to {after}");
    }

    #[test]
    fn charge_is_sticky_too_but_follows_a_real_drain() {
        let mut s = Smoother::new();
        for v in [80u8, 81, 80, 79, 80, 81, 80, 79, 80] {
            s.push_charge(v);
        }
        let settled = s.charge().unwrap();
        for v in [81u8, 79, 80, 81, 79] {
            s.push_charge(v);
            assert_eq!(s.charge(), Some(settled));
        }
        for v in (20..80).rev() {
            s.push_charge(v as u8);
        }
        assert!(s.charge().unwrap() < 40);
    }

    #[test]
    fn the_smoother_reports_from_the_first_sample() {
        let mut s = Smoother::new();
        assert_eq!(s.runtime(), None, "nothing seen yet is not zero");
        s.push_runtime(1800);
        assert_eq!(s.runtime(), Some(1800));
    }

    /// It must not lag a genuine trend into uselessness. The transfer sag is a
    /// real reading, and a window that swallowed it would be worse than none.
    #[test]
    fn the_smoother_still_follows_a_real_decline() {
        let mut s = Smoother::new();
        for v in [2600u16; Smoother::WINDOW] {
            s.push_runtime(v);
        }
        assert_eq!(s.runtime(), Some(2600));
        // A steady fall, longer than the window so the median fully turns over.
        for v in (1000..2600).rev().step_by(50) {
            s.push_runtime(v as u16);
        }
        assert!(
            s.runtime().unwrap() < 1400,
            "held at {:?} through a 1600 s decline",
            s.runtime()
        );
    }

    /// A single wild sample must not move the displayed value at all.
    #[test]
    fn one_outlier_is_ignored_outright() {
        let mut s = Smoother::new();
        for v in [1800u16, 1800, 1800, 1800] {
            s.push_runtime(v);
        }
        s.push_runtime(0);
        assert_eq!(s.runtime(), Some(1800), "a zero blipped through");
    }

    #[test]
    fn the_window_never_grows_without_bound() {
        let mut s = Smoother::new();
        for i in 0..1000u16 {
            s.push_runtime(i);
        }
        // Asserted against WINDOW rather than a copied-out list, so retuning the
        // window does not leave a test failing for no reason.
        assert_eq!(s.runtime.len(), Smoother::WINDOW);
        assert_eq!(s.runtime.last(), Some(&999));
    }

    #[test]
    fn a_disconnect_clears_everything_before_the_gap() {
        let mut s = Smoother::new();
        s.push_charge(100);
        s.push_runtime(2600);
        s.clear();
        assert_eq!(s.charge(), None);
        assert_eq!(s.runtime(), None);
    }

    #[test]
    fn shutdown_imminent_counts_as_on_battery_for_display() {
        let r = Reading {
            status: PresentStatus::from_usages(&[(PAGE_POWER, 0x69), (PAGE_BATTERY, 0xD1)]),
            have_status: true,
            ..Default::default()
        };
        assert!(r.on_battery(), "no ACPresent flag means on battery");
    }

    #[test]
    fn a_partial_reading_says_na_rather_than_inventing_numbers() {
        let r = Reading {
            charge: Some(50),
            have_status: true,
            status: PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD0)]),
            ..Default::default()
        };
        let s = r.summary();
        assert!(s.contains("n/a"), "{s}");
        assert!(!s.contains("0 V"), "missing must not render as zero: {s}");
    }

    /// The S3 case, and the reason `is_stale` looks at both ages. Across a
    /// suspend and resume this device stops pushing input reports entirely
    /// while feature reads keep working, and reopening does not restore the
    /// stream. A device answering feature reads is responding.
    #[test]
    fn a_dead_input_stream_is_not_a_dead_device() {
        let s = Snapshot {
            device_ok: true,
            stream_age_s: Some(9_999),
            sweep_age_s: Some(2),
            ..Snapshot::default()
        };
        assert!(!s.is_stale(), "a device serving feature reads read as gone");
    }

    /// ...and the converse: sweeps stalled but the stream alive is also fine.
    #[test]
    fn a_stalled_sweep_alone_is_not_stale_either() {
        let s = Snapshot {
            device_ok: true,
            stream_age_s: Some(1),
            sweep_age_s: Some(9_999),
            ..Snapshot::default()
        };
        assert!(!s.is_stale());
    }

    /// Both quiet is genuinely gone.
    #[test]
    fn both_sources_quiet_is_stale() {
        let s = Snapshot {
            device_ok: true,
            stream_age_s: Some(9_999),
            sweep_age_s: Some(9_999),
            ..Snapshot::default()
        };
        assert!(s.is_stale());
        assert_eq!(s.power(), Power::Unknown);
    }

    /// Never heard anything at all is stale, not healthy.
    #[test]
    fn having_heard_nothing_is_stale() {
        let s = Snapshot { device_ok: true, ..Snapshot::default() };
        assert!(s.is_stale());
    }

    /// The one that actually happened: on battery, status known, runtime not.
    /// It painted "0", which reads as no runtime left, on a machine with
    /// twenty-five minutes. Unknown must render as nothing at all.
    #[test]
    fn an_unknown_runtime_never_paints_a_zero() {
        let r = Reading {
            have_status: true,
            status: PresentStatus { ac_present: false, discharging: true, ..Default::default() },
            charge: Some(77),
            runtime_s: None,
            ..Reading::default()
        };
        assert_eq!(r.icon_digits(), None, "invented a runtime of zero");
    }

    /// ...but a genuine zero is still a zero. The device is entitled to say the
    /// battery is out, and suppressing that would be the opposite mistake.
    #[test]
    fn a_real_zero_runtime_is_still_shown() {
        let r = Reading {
            have_status: true,
            status: PresentStatus { ac_present: false, discharging: true, ..Default::default() },
            charge: Some(1),
            runtime_s: Some(0),
            ..Reading::default()
        };
        assert_eq!(r.icon_digits(), Some(0));
    }
}
