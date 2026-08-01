//! The log: what gets written, and how a five-minute window becomes one row.
//!
//! Everything here except `now_local` is pure, so the interval arithmetic —
//! which is where the subtle mistakes live — tests without a clock or a device.
//!
//! **The sampler owns this file exclusively.** The tray displays and never
//! writes. One writer means no dedup, no interleaving, and a log that is
//! continuous whether or not anyone is logged in.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::decode::PresentStatus;

/// Default seconds per row. Matches PowerChute's own cadence so the two series
/// are comparable if you ever want to overlay them.
pub const DEFAULT_INTERVAL_S: u64 = 300;

pub const HEADER: &str =
    "timestamp,charge,runtime_s,load_pct,watts,input_v,battery_v,ac,n,event";

/// Why a row was written.
///
/// A row is a summary of a window, not an instant, so it has to say which kind
/// of window it summarises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The interval simply elapsed.
    Interval,
    /// Mains lost. The interval was cut short to write this.
    OnBattery,
    /// Mains returned.
    OnLine,
    /// The device stopped answering. No values, only the fact and the time.
    DeviceLost,
    /// The device came back.
    DeviceFound,
    /// Sampling stopped cleanly.
    Stopped,
}

impl Event {
    pub fn tag(self) -> &'static str {
        match self {
            Event::Interval => "",
            Event::OnBattery => "onbattery",
            Event::OnLine => "online",
            Event::DeviceLost => "device-lost",
            Event::DeviceFound => "device-found",
            Event::Stopped => "stopped",
        }
    }

    /// Transitions close the current interval early.
    ///
    /// Otherwise a window straddling an outage medians mains and battery
    /// samples together and reports a state that never existed.
    pub fn closes_interval(self) -> bool {
        !matches!(self, Event::Interval)
    }
}

/// Local wall time, to the second, with its UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// Minutes east of UTC. Negative west.
    pub offset_min: i32,
}

impl LocalTime {
    /// ISO-8601 with offset. Unambiguous across a DST change, which a naive
    /// local timestamp is not — and a UPS log that loses an hour every autumn
    /// is a log you cannot reason about.
    pub fn iso8601(&self) -> String {
        let (sign, off) = if self.offset_min < 0 {
            ('-', -self.offset_min)
        } else {
            ('+', self.offset_min)
        };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            sign,
            off / 60,
            off % 60
        )
    }

    /// `jdups-YYYY-MM.csv`. Monthly, so a file never grows unmanageable.
    pub fn month_file(&self) -> String {
        format!("jdups-{:04}-{:02}.csv", self.year, self.month)
    }
}

/// One interval's worth of observations.
///
/// Stream fields and sweep fields are accumulated separately because they come
/// from different places at different rates: charge and runtime arrive on the
/// input stream many times a minute, while load and the voltages are
/// feature-only and are sampled on a slower cadence. An earlier draft of the
/// plan said "median every field", which is not possible — reports 6, 12, 19,
/// 20 and 22 do not carry load or voltage at all.
#[derive(Debug, Default, Clone)]
pub struct Accumulator {
    charge: Vec<u8>,
    runtime: Vec<u16>,
    load: Vec<u8>,
    input_v: Vec<u16>,
    battery_v: Vec<u16>, // hundredths, to stay integral for the median
    rated_w: Option<u16>,
    status: Option<PresentStatus>,
    /// Stream samples seen. This is the `n` column.
    pub samples: u32,
}

impl Accumulator {
    pub fn new() -> Accumulator {
        Accumulator::default()
    }

    pub fn push_stream(&mut self, charge: Option<u8>, runtime: Option<u16>) {
        if let Some(c) = charge {
            self.charge.push(c);
        }
        if let Some(r) = runtime {
            self.runtime.push(r);
        }
        self.samples += 1;
    }

    pub fn push_sweep(
        &mut self,
        load: Option<u8>,
        input_v: Option<u16>,
        battery_v: Option<f32>,
        rated_w: Option<u16>,
    ) {
        if let Some(l) = load {
            self.load.push(l);
        }
        if let Some(v) = input_v {
            self.input_v.push(v);
        }
        if let Some(v) = battery_v {
            self.battery_v.push((v * 100.0).round() as u16);
        }
        if rated_w.is_some() {
            self.rated_w = rated_w;
        }
    }

    pub fn set_status(&mut self, s: PresentStatus) {
        self.status = Some(s);
    }

    pub fn is_empty(&self) -> bool {
        self.samples == 0 && self.load.is_empty()
    }

    /// Render this window as one CSV row.
    ///
    /// Every aggregate is a median. Missing fields are written empty rather
    /// than as zero: a gap and a genuine 0 V are very different things, and a
    /// log that cannot tell them apart is worse than one with holes in it.
    pub fn row(&self, at: &LocalTime, event: Event) -> String {
        let f = |v: Option<String>| v.unwrap_or_default();
        let load = median(&self.load);
        let watts = match (load, self.rated_w) {
            (Some(l), Some(r)) => Some(crate::decode::watts(l, r)),
            _ => None,
        };
        let ac = self.status.map(|s| if s.on_battery() { 0 } else { 1 });

        format!(
            "{},{},{},{},{},{},{},{},{},{}",
            at.iso8601(),
            f(median(&self.charge).map(|v| v.to_string())),
            f(median(&self.runtime).map(|v| v.to_string())),
            f(load.map(|v| v.to_string())),
            f(watts.map(|v| v.to_string())),
            f(median(&self.input_v).map(|v| v.to_string())),
            f(median(&self.battery_v).map(|v| format!("{:.2}", v as f32 / 100.0))),
            f(ac.map(|v| v.to_string())),
            self.samples,
            event.tag(),
        )
    }

    /// Start the next window. Called immediately after `row`.
    ///
    /// `rated_w` survives because it is a property of the unit, not of the
    /// window, and re-reading it every interval would be the only reason to
    /// keep sweeping it.
    pub fn reset(&mut self) {
        let rated = self.rated_w;
        let status = self.status;
        *self = Accumulator::new();
        self.rated_w = rated;
        self.status = status;
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

/// Append a row, creating the file with a header if it is new.
///
/// Opened, written and flushed per row rather than held open: the sampler runs
/// for months, a row every five minutes is nothing, and a file that is closed
/// between writes is one you can read, move or delete without stopping the
/// service.
pub fn append(dir: &Path, at: &LocalTime, row: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(at.month_file());
    let fresh = !path.exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    if fresh {
        writeln!(f, "# jdups log v1  interval={DEFAULT_INTERVAL_S}s  medians per interval")?;
        writeln!(f, "{HEADER}")?;
    }
    writeln!(f, "{row}")?;
    f.flush()?;
    Ok(path)
}

/// Where the log lives.
///
/// `%ProgramData%` because the writer is SYSTEM and the reader is you. The
/// installer is what makes this safe — see the plan: an elevated writer
/// following a path a normal user can influence is an elevation-of-privilege
/// bug, so the directory must be created and ACL'd before the sampler runs.
pub fn default_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("jdups")
}

/// Local time now, with the offset the OS says is in effect.
pub fn now_local() -> LocalTime {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};

    unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);

        let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed();
        // 0 = unknown, 1 = standard, 2 = daylight. The bias to apply depends on
        // which is in effect, and getting it wrong shifts every timestamp by an
        // hour for half the year.
        let which = GetTimeZoneInformation(&mut tz);
        let bias = tz.Bias
            + match which {
                2 => tz.DaylightBias,
                1 => tz.StandardBias,
                _ => 0,
            };

        LocalTime {
            year: st.wYear as i32,
            month: st.wMonth as u32,
            day: st.wDay as u32,
            hour: st.wHour as u32,
            minute: st.wMinute as u32,
            second: st.wSecond as u32,
            // Win32 reports minutes *west* of UTC; ISO-8601 wants east.
            offset_min: -bias,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{PAGE_BATTERY, PAGE_POWER};

    fn t(offset_min: i32) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 7,
            day: 31,
            hour: 15,
            minute: 4,
            second: 22,
            offset_min,
        }
    }

    fn mains() -> PresentStatus {
        PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD0), (PAGE_BATTERY, 0xD1)])
    }

    fn on_battery() -> PresentStatus {
        PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD1), (PAGE_BATTERY, 0x45)])
    }

    #[test]
    fn timestamps_carry_their_offset() {
        assert_eq!(t(-360).iso8601(), "2026-07-31T15:04:22-06:00");
        assert_eq!(t(0).iso8601(), "2026-07-31T15:04:22+00:00");
        assert_eq!(t(330).iso8601(), "2026-07-31T15:04:22+05:30");
    }

    #[test]
    fn the_log_rolls_monthly() {
        assert_eq!(t(0).month_file(), "jdups-2026-07.csv");
        let mut d = t(0);
        d.month = 12;
        assert_eq!(d.month_file(), "jdups-2026-12.csv");
    }

    #[test]
    fn a_row_medians_each_column_over_the_window() {
        let mut a = Accumulator::new();
        for (c, r) in [(100u8, 2508u16), (100, 2595), (100, 2688)] {
            a.push_stream(Some(c), Some(r));
        }
        a.push_sweep(Some(20), Some(117), Some(27.26), Some(900));
        a.push_sweep(Some(22), Some(116), Some(27.43), Some(900));
        a.push_sweep(Some(21), Some(117), Some(27.26), Some(900));
        a.set_status(mains());

        let row = a.row(&t(-360), Event::Interval);
        assert_eq!(
            row,
            "2026-07-31T15:04:22-06:00,100,2595,21,189,117,27.26,1,3,"
        );
    }

    /// The whole reason the log medians rather than spot-reads: the device
    /// cycles through three runtime values at a dead-steady load.
    #[test]
    fn the_quantisation_cycle_collapses_to_its_middle_value() {
        let mut a = Accumulator::new();
        for r in [2688u16, 2508, 2595, 2508, 2688] {
            a.push_stream(Some(100), Some(r));
        }
        assert!(a.row(&t(0), Event::Interval).contains(",2595,"));
    }

    /// A missing field must be empty, never zero. `,,` and `,0,` mean entirely
    /// different things and only one of them is true.
    #[test]
    fn absent_fields_are_blank_not_zero() {
        let a = Accumulator::new();
        let row = a.row(&t(0), Event::DeviceLost);
        assert_eq!(row, "2026-07-31T15:04:22+00:00,,,,,,,,0,device-lost");
        assert!(!row.contains(",0,0,"), "a gap rendered as real zeroes: {row}");
    }

    #[test]
    fn watts_needs_both_the_load_and_the_rating() {
        let mut a = Accumulator::new();
        a.push_sweep(Some(20), None, None, None); // load but no rating
        assert_eq!(a.row(&t(0), Event::Interval).split(',').nth(4), Some(""));
        a.push_sweep(Some(20), None, None, Some(900));
        assert_eq!(a.row(&t(0), Event::Interval).split(',').nth(4), Some("180"));
    }

    /// Every transition cuts the window short. A five-minute median that
    /// straddles an outage describes a state the UPS was never in.
    #[test]
    fn only_a_plain_interval_lets_the_window_continue() {
        assert!(!Event::Interval.closes_interval());
        for e in [
            Event::OnBattery,
            Event::OnLine,
            Event::DeviceLost,
            Event::DeviceFound,
            Event::Stopped,
        ] {
            assert!(e.closes_interval(), "{e:?} should close the interval");
        }
    }

    #[test]
    fn the_ac_column_follows_the_status_flags() {
        let mut a = Accumulator::new();
        a.set_status(mains());
        assert_eq!(a.row(&t(0), Event::Interval).split(',').nth(7), Some("1"));
        a.set_status(on_battery());
        assert_eq!(a.row(&t(0), Event::Interval).split(',').nth(7), Some("0"));
    }

    /// `ShutdownImminent` with no ACPresent is on-battery, and the log has to
    /// agree with the icon about that.
    #[test]
    fn shutdown_imminent_logs_as_on_battery() {
        let mut a = Accumulator::new();
        a.set_status(PresentStatus::from_usages(&[
            (PAGE_POWER, 0x69),
            (PAGE_BATTERY, 0xD1),
        ]));
        assert_eq!(a.row(&t(0), Event::Interval).split(',').nth(7), Some("0"));
    }

    #[test]
    fn reset_keeps_the_units_rating_and_drops_the_window() {
        let mut a = Accumulator::new();
        a.push_stream(Some(100), Some(2595));
        a.push_sweep(Some(20), Some(117), Some(27.26), Some(900));
        a.set_status(mains());
        a.reset();

        assert_eq!(a.samples, 0);
        assert!(a.is_empty());
        // Rating survives, so the next window can still compute watts from its
        // very first sweep.
        a.push_sweep(Some(30), None, None, None);
        assert_eq!(a.row(&t(0), Event::Interval).split(',').nth(4), Some("270"));
    }

    #[test]
    fn the_sample_count_is_reported_so_a_thin_window_is_visible() {
        let mut a = Accumulator::new();
        for _ in 0..7 {
            a.push_stream(Some(100), Some(2595));
        }
        assert_eq!(a.row(&t(0), Event::Interval).split(',').nth(8), Some("7"));
    }

    #[test]
    fn the_header_names_every_column_the_rows_emit() {
        let mut a = Accumulator::new();
        a.push_stream(Some(100), Some(2595));
        a.push_sweep(Some(20), Some(117), Some(27.26), Some(900));
        a.set_status(mains());
        assert_eq!(
            HEADER.split(',').count(),
            a.row(&t(0), Event::Interval).split(',').count()
        );
    }

    #[test]
    fn a_written_log_gets_a_header_once_and_then_only_rows() {
        let dir = std::env::temp_dir().join(format!("jdups-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let at = t(-360);
        let mut a = Accumulator::new();
        a.push_stream(Some(100), Some(2595));
        a.set_status(mains());

        let p = append(&dir, &at, &a.row(&at, Event::Interval)).unwrap();
        append(&dir, &at, &a.row(&at, Event::Interval)).unwrap();

        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].starts_with("# jdups log"));
        assert_eq!(lines[1], HEADER);
        assert!(lines[2].starts_with("2026-07-31T15:04:22-06:00"));
        assert_eq!(lines[2], lines[3]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
