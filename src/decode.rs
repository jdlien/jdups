//! Pure decoding of the UPS's HID reports. **No I/O lives here.**
//!
//! Every number jdups shows is a function from a five-byte report, so all of the
//! interesting logic tests exhaustively with no UPS attached. The golden vectors
//! in the tests below are real payloads captured from the live unit — see
//! docs/implementation-plan.md, "What the device actually exposes".

/// Report IDs, named. Verified against a full `HidP_GetValueCaps` +
/// `HidP_GetButtonCaps` walk of the device rather than guessed.
pub mod report {
    /// RemainingCapacity (`85:66`) + RunTimeToEmpty (`85:68`) in one read.
    /// Both fields arrive in the same five bytes, so they cannot disagree.
    pub const CHARGE_RUNTIME: u8 = 12;
    /// Charging (`85:44`) / Discharging (`85:45`).
    pub const CHARGE_STATE: u8 = 6;
    /// ACPresent (`85:D0`). Pushed on the input stream, on change.
    pub const AC_PRESENT: u8 = 19;
    /// ShutdownImminent (`84:69`) + BelowRemainingCapacityLimit (`85:42`).
    pub const SHUTDOWN_IMMINENT: u8 = 20;
    /// PresentStatus — eleven flags in one read. The most useful report here.
    pub const PRESENT_STATUS: u8 = 22;
    /// Test (`84:58`).
    pub const TEST: u8 = 33;
    /// Battery Voltage (`84:30`), hundredths of a volt.
    pub const BATTERY_VOLTS: u8 = 9;
    /// Input Voltage (`84:30`), whole volts.
    pub const INPUT_VOLTS: u8 = 49;
    /// PercentLoad (`84:35`).
    pub const LOAD: u8 = 80;
    /// ConfigActivePower (`84:44`) — 900 W on this unit.
    pub const RATED_POWER: u8 = 82;
    /// ManufacturerDate (`85:85`). Also on 32 and 123.
    pub const MANUFACTURE_DATE: u8 = 7;
    /// LowVoltageTransfer (`84:53`).
    pub const LOW_TRANSFER: u8 = 50;
    /// HighVoltageTransfer (`84:54`).
    pub const HIGH_TRANSFER: u8 = 51;
    /// APC vendor `FF86:52`, 0..13. Last transfer reason.
    pub const LAST_TRANSFER: u8 = 54;
    /// DelayBeforeShutdown (`84:57`), i16, -1 = none scheduled. Also on 66.
    ///
    /// **The standard register, and PowerChute does not use it.** Observed
    /// sitting at -1 through an entire vendor-driven shutdown; see
    /// `APC_SHUTDOWN_COUNTDOWN` for the one that actually works on this unit.
    pub const DELAY_BEFORE_SHUTDOWN: u8 = 21;

    /// AudibleAlarmControl (`84:5A`), 1..3. Also mirrored on 120.
    ///
    /// Writable, verified on the hardware: 1 -> 2 -> 1 round-tripped with
    /// readback on both mirrors. This is what the tray's alarm toggle drives.
    pub const ALARM: u8 = 24;

    /// APC vendor `FF86:7C`, 0..1. Set while a countdown is armed.
    ///
    /// Read 1 for the whole of the observed shutdown and 0 either side of it.
    /// Whether it is written or merely reflects the countdown is not yet known:
    /// the first sample already had it set, so the order was never seen.
    pub const APC_SHUTDOWN_ARMED: u8 = 64;

    /// APC vendor `FF86:7D`, i16, -1 = none scheduled. **This is the one.**
    ///
    /// Seconds until the UPS cuts its own output. Measured 2026-08-01 by
    /// watching a real PowerChute shutdown: it set this to 120 and the value
    /// counted down in real time (119, 116, 114, ... 98) while report 21 stayed
    /// at -1 throughout. The UPS cut output when it reached zero.
    ///
    /// The plan had this backwards. It was labelled the *restart* delay on the
    /// reasoning that its `-1..32767` range matched `DelayBeforeShutdown` — which
    /// was the right observation and the wrong conclusion. It matches because it
    /// **is** a shutdown delay, just APC's own.
    pub const APC_SHUTDOWN_COUNTDOWN: u8 = 65;
}

/// `AudibleAlarmControl` values, which are a 1-based enum rather than a flag.
pub const ALARM_DISABLED: u8 = 1;
pub const ALARM_ENABLED: u8 = 2;
pub const ALARM_MUTED: u8 = 3;

/// HID usage pages this device speaks.
pub const PAGE_POWER: u16 = 0x84;
pub const PAGE_BATTERY: u16 = 0x85;
pub const PAGE_APC: u16 = 0xFF86;

/// A report whose ID or length is not what we asked for is an error, never
/// something to decode. A short read that got silently zero-extended would
/// produce a plausible reading, which is the worst possible failure here.
fn payload(buf: &[u8], id: u8, need: usize) -> Option<&[u8]> {
    if buf.first().copied() != Some(id) || buf.len() < 1 + need {
        return None;
    }
    Some(&buf[1..])
}

fn u8_at(buf: &[u8], id: u8, i: usize) -> Option<u8> {
    payload(buf, id, i + 1).map(|p| p[i])
}

fn u16_at(buf: &[u8], id: u8, i: usize) -> Option<u16> {
    payload(buf, id, i + 2).map(|p| u16::from_le_bytes([p[i], p[i + 1]]))
}

fn i16_at(buf: &[u8], id: u8, i: usize) -> Option<i16> {
    u16_at(buf, id, i).map(|v| v as i16)
}

/// Battery charge, percent.
pub fn charge(buf: &[u8]) -> Option<u8> {
    u8_at(buf, report::CHARGE_RUNTIME, 0).filter(|&c| c <= 100)
}

/// Runtime remaining, seconds.
///
/// Note this figure jitters by ~±3.5 % at a dead-steady load (three quantised
/// values ~90 s apart). Anything tracking battery decay must aggregate.
pub fn runtime_s(buf: &[u8]) -> Option<u16> {
    u16_at(buf, report::CHARGE_RUNTIME, 1)
}

/// UPS load, percent of rated output.
pub fn load_pct(buf: &[u8]) -> Option<u8> {
    u8_at(buf, report::LOAD, 0).filter(|&l| l <= 100)
}

/// Rated output, watts. 900 W on this unit.
pub fn rated_watts(buf: &[u8]) -> Option<u16> {
    u16_at(buf, report::RATED_POWER, 0)
}

/// Mains input, whole volts.
pub fn input_volts(buf: &[u8]) -> Option<u16> {
    u16_at(buf, report::INPUT_VOLTS, 0)
}

/// Battery volts. The device reports hundredths.
pub fn battery_volts(buf: &[u8]) -> Option<f32> {
    u16_at(buf, report::BATTERY_VOLTS, 0).map(|v| v as f32 / 100.0)
}

/// `ACPresent`, from its own report.
pub fn ac_present(buf: &[u8]) -> Option<bool> {
    u8_at(buf, report::AC_PRESENT, 0).map(|v| v != 0)
}

/// Charging / discharging, from report 6.
pub fn charge_state(buf: &[u8]) -> Option<(bool, bool)> {
    let p = payload(buf, report::CHARGE_STATE, 2)?;
    Some((p[0] != 0, p[1] != 0))
}

/// `DelayBeforeShutdown`, seconds. **-1 means no shutdown is scheduled** — the
/// device's resting state, and the value to restore on cancel.
pub fn delay_before_shutdown(buf: &[u8]) -> Option<i16> {
    i16_at(buf, report::DELAY_BEFORE_SHUTDOWN, 0)
}

/// Last transfer reason, 0..13. 0 with a clean power history.
pub fn last_transfer(buf: &[u8]) -> Option<u8> {
    u8_at(buf, report::LAST_TRANSFER, 0)
}

/// Self-test state, from `84:58`. Arrives as an input report on change.
pub fn test(buf: &[u8]) -> Option<u8> {
    u8_at(buf, report::TEST, 0)
}

/// Name a `Test` value.
///
/// The mapping is APC's convention as recorded by NUT's `apc-hid.c`, and the
/// one point confirmed here is the resting value: this unit reads **6** with no
/// test having been run, which matches "no test initiated". The rest is taken on
/// NUT's authority rather than measured, so an unrecognised code renders as
/// itself rather than being forced into a name.
pub fn test_result(v: u8) -> String {
    match v {
        1 => "passed".into(),
        2 => "warning".into(),
        3 => "error".into(),
        4 => "aborted".into(),
        5 => "in-progress".into(),
        6 => "none".into(),
        other => format!("code-{other}"),
    }
}

/// Battery installation date, from the HID `ManufacturerDate` packing.
///
/// This is real device state read off the UPS, not a date in a file — and more
/// precise than PowerChute renders it. It is a *writable* usage, so read it as
/// "what someone last told the UPS", not a factory stamp.
pub fn manufacture_date(buf: &[u8]) -> Option<(i32, u32, u32)> {
    unpack_date(u16_at(buf, report::MANUFACTURE_DATE, 0)?)
}

/// The standard HID date packing: `year = 1980 + (raw >> 9)`, month, day.
pub fn unpack_date(raw: u16) -> Option<(i32, u32, u32)> {
    let year = 1980 + (raw >> 9) as i32;
    let month = ((raw >> 5) & 0x0F) as u32;
    let day = (raw & 0x1F) as u32;
    if month == 0 || month > 12 || day == 0 || day > 31 {
        return None;
    }
    Some((year, month, day))
}

/// Watts from percent load against the device's own rated output.
///
/// Deliberately uses the 900 W the device reports for itself. PowerChute's
/// energy log implies 1050 W (1500 VA x 0.7) and disagrees by ~17 %; prefer the
/// device's figure and don't try to match theirs.
pub fn watts(load_pct: u8, rated_watts: u16) -> u32 {
    (load_pct as u32 * rated_watts as u32) / 100
}

// ---------------------------------------------------------------------------
// PresentStatus
// ---------------------------------------------------------------------------

/// The eleven flags packed into report 22.
///
/// These are 1-bit *button* caps, which is why a value-caps walk misses them
/// entirely and why the original investigation never found this report.
///
/// Built from a decoded usage list rather than from bit positions: the bit order
/// is a property of the report descriptor, not of the usage numbers, and a wrong
/// guess yields flags that look plausible and are wrong. `HidP_GetUsages` is the
/// only correct way to get here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PresentStatus {
    pub ac_present: bool,
    pub battery_present: bool,
    pub charging: bool,
    pub discharging: bool,
    pub shutdown_imminent: bool,
    pub below_remaining_capacity_limit: bool,
    pub remaining_time_limit_expired: bool,
    pub overload: bool,
    pub communication_lost: bool,
    pub voltage_not_regulated: bool,
}

impl PresentStatus {
    /// Fold a list of set `(usage_page, usage)` pairs into named flags.
    pub fn from_usages(usages: &[(u16, u16)]) -> PresentStatus {
        let mut s = PresentStatus::default();
        for &(page, usage) in usages {
            match (page, usage) {
                (PAGE_POWER, 0x65) => s.overload = true,
                (PAGE_POWER, 0x69) => s.shutdown_imminent = true,
                (PAGE_POWER, 0x73) => s.communication_lost = true,
                (PAGE_BATTERY, 0x42) => s.below_remaining_capacity_limit = true,
                // The descriptor declares this usage twice, on 0x43 and 0x4B.
                (PAGE_BATTERY, 0x43) | (PAGE_BATTERY, 0x4B) => {
                    s.remaining_time_limit_expired = true
                }
                (PAGE_BATTERY, 0x44) => s.charging = true,
                (PAGE_BATTERY, 0x45) => s.discharging = true,
                (PAGE_BATTERY, 0xD0) => s.ac_present = true,
                (PAGE_BATTERY, 0xD1) => s.battery_present = true,
                (PAGE_BATTERY, 0xDB) => s.voltage_not_regulated = true,
                _ => {}
            }
        }
        s
    }

    /// The flags worth recording, in a stable order.
    ///
    /// Everything except `charging` and `battery_present`. Those two are
    /// excluded for opposite reasons: `charging` toggles constantly during
    /// normal float-charging and would make every log row look like an event,
    /// and `battery_present` is true forever on a UPS with its battery in.
    ///
    /// `ac_present` is here even though it also gets its own column, because
    /// this is the set that decides whether *something happened*, and mains is
    /// the most important member of it.
    pub fn notable(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (name, set) in [
            ("ac", self.ac_present),
            ("discharging", self.discharging),
            ("shutdown-imminent", self.shutdown_imminent),
            ("below-capacity-limit", self.below_remaining_capacity_limit),
            ("time-limit-expired", self.remaining_time_limit_expired),
            ("overload", self.overload),
            ("comm-lost", self.communication_lost),
            ("voltage-not-regulated", self.voltage_not_regulated),
        ] {
            if set {
                v.push(name);
            }
        }
        v
    }

    /// True when the UPS is running the load off its battery.
    ///
    /// `ACPresent` is the primary signal; `discharging` corroborates it. Either
    /// alone is enough, because requiring both would fail *open* — the dangerous
    /// direction — if one flag lagged the other across a transition.
    pub fn on_battery(&self) -> bool {
        !self.ac_present || self.discharging
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors, captured from the live unit.
    const R12: [u8; 5] = [0x0C, 0x64, 0x23, 0x0A, 0x00];
    const R06: [u8; 5] = [0x06, 0x00, 0x00, 0x08, 0x00];
    const R07: [u8; 5] = [0x07, 0x77, 0x53, 0x00, 0x00];
    const R09: [u8; 5] = [0x09, 0xA6, 0x0A, 0x00, 0x00];
    const R19: [u8; 5] = [0x13, 0x01, 0x00, 0x00, 0x00];
    const R21: [u8; 5] = [0x15, 0xFF, 0xFF, 0x00, 0x00];
    const R49: [u8; 5] = [0x31, 0x75, 0x00, 0x00, 0x00];
    const R80: [u8; 5] = [0x50, 0x14, 0x00, 0x00, 0x00];
    const R82: [u8; 5] = [0x52, 0x84, 0x03, 0x00, 0x00];

    #[test]
    fn report_12_carries_charge_and_runtime() {
        assert_eq!(charge(&R12), Some(100));
        assert_eq!(runtime_s(&R12), Some(2595));
    }

    #[test]
    fn the_simple_scalars_decode() {
        assert_eq!(load_pct(&R80), Some(20));
        assert_eq!(rated_watts(&R82), Some(900));
        assert_eq!(input_volts(&R49), Some(117));
        assert_eq!(battery_volts(&R09), Some(27.26));
        assert_eq!(ac_present(&R19), Some(true));
        assert_eq!(charge_state(&R06), Some((false, false)));
    }

    /// -1, not 65535. Reading this as unsigned would make "no shutdown
    /// scheduled" look like an 18-hour countdown.
    #[test]
    fn a_resting_shutdown_delay_is_minus_one() {
        assert_eq!(delay_before_shutdown(&R21), Some(-1));
    }

    #[test]
    fn the_battery_date_unpacks() {
        assert_eq!(manufacture_date(&R07), Some((2021, 11, 23)));
        assert_eq!(unpack_date(21367), Some((2021, 11, 23)));
    }

    #[test]
    fn a_nonsense_date_is_rejected_rather_than_shown() {
        assert_eq!(unpack_date(0), None); // month 0, day 0
        assert_eq!(unpack_date(0xFFFF), None); // month 15
    }

    /// The whole point of `payload`: a report that is not the one we asked for
    /// must not decode. Report 6's bytes read as a 0 % charge otherwise, which
    /// is exactly what a naive multiplexed reader would report.
    #[test]
    fn a_different_report_never_decodes_as_this_one() {
        assert_eq!(charge(&R06), None);
        assert_eq!(runtime_s(&R06), None);
        assert_eq!(load_pct(&R82), None);
        assert_eq!(input_volts(&R09), None);
    }

    #[test]
    fn a_short_report_is_refused_not_zero_extended() {
        assert_eq!(charge(&[0x0C]), None);
        assert_eq!(runtime_s(&[0x0C, 0x64]), None);
        assert_eq!(runtime_s(&[0x0C, 0x64, 0x23]), None);
        assert_eq!(charge(&[]), None);
    }

    #[test]
    fn an_out_of_range_percentage_is_refused() {
        assert_eq!(charge(&[0x0C, 200, 0, 0, 0]), None);
        assert_eq!(load_pct(&[0x50, 101, 0, 0, 0]), None);
    }

    #[test]
    fn watts_uses_the_devices_own_rating() {
        assert_eq!(watts(20, 900), 180);
        assert_eq!(watts(0, 900), 0);
        assert_eq!(watts(100, 900), 900);
    }

    #[test]
    fn present_status_maps_usages_to_names() {
        let s = PresentStatus::from_usages(&[
            (PAGE_BATTERY, 0xD0),
            (PAGE_BATTERY, 0xD1),
        ]);
        assert!(s.ac_present && s.battery_present);
        assert!(!s.discharging && !s.shutdown_imminent);
        assert!(!s.on_battery());
    }

    #[test]
    fn the_duplicated_time_limit_usage_maps_to_one_flag() {
        for u in [0x43u16, 0x4B] {
            let s = PresentStatus::from_usages(&[(PAGE_BATTERY, u)]);
            assert!(s.remaining_time_limit_expired, "usage {u:#x} should map");
        }
    }

    /// Failing *open* here would mean not shutting down during a real outage.
    /// Either signal alone must be enough.
    #[test]
    fn on_battery_trips_on_either_signal() {
        let no_ac = PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD1)]);
        assert!(no_ac.on_battery(), "absent ACPresent must read as on-battery");

        let discharging =
            PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD0), (PAGE_BATTERY, 0x45)]);
        assert!(
            discharging.on_battery(),
            "discharging must count even while ACPresent lags"
        );
    }

    /// The one value confirmed on the live unit: it rests at 6 with no test
    /// having been run. The rest is NUT's mapping, taken on its authority.
    #[test]
    fn a_self_test_result_is_named_or_shown_raw() {
        assert_eq!(test_result(6), "none");
        assert_eq!(test_result(1), "passed");
        assert_eq!(test_result(3), "error");
        // An unrecognised code renders as itself rather than being forced into
        // a name that might be a lie.
        assert_eq!(test_result(9), "code-9");
        assert_eq!(test_result(0), "code-0");
    }

    #[test]
    fn the_test_report_decodes_only_from_its_own_report() {
        assert_eq!(test(&[0x21, 6, 0, 0, 0]), Some(6));
        assert_eq!(test(&[0x0C, 6, 0, 0, 0]), None);
    }

    #[test]
    fn an_unknown_usage_is_ignored_not_misfiled() {
        let s = PresentStatus::from_usages(&[(PAGE_APC, 0x60), (0x99, 0x01)]);
        assert_eq!(s, PresentStatus::default());
    }
}
