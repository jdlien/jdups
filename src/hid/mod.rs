//! The device seam.
//!
//! Everything above this module talks to a `Ups`, so the tray, the sampler and
//! (later) the agent can all be driven by a scripted fake. The agent is the
//! reason this matters: its decision path has to be testable without a battery.

pub mod caps;
pub mod raw;

use std::io;

use windows_sys::Win32::Devices::HumanInterfaceDevice::{HidP_Feature, HidP_Input};

use crate::decode::{self, report, PresentStatus};
use crate::model::Reading;

/// The default deadline for a single feature report.
///
/// The whole exchange takes single-digit milliseconds when the device is well;
/// this only has to be short enough that a wedged one is noticed.
pub const FEATURE_TIMEOUT_MS: u32 = 1_000;

/// What the rest of the program needs from a UPS.
pub trait Ups {
    /// One feature report, bounded.
    fn feature(&self, report_id: u8) -> io::Result<Vec<u8>>;
    /// One input report, or `None` on timeout.
    fn input(&self, timeout_ms: u32) -> io::Result<Option<Vec<u8>>>;
    /// Decode a report's `PresentStatus` flags against the parsed descriptor.
    fn status_of(&self, report: &[u8], from_input: bool) -> PresentStatus;

    /// Everything the menu shows, in one sweep.
    ///
    /// Each field is independent: one unreadable report leaves a `None` rather
    /// than failing the whole readout, because a partial reading is still worth
    /// showing and the alternative is a blank menu whenever anything hiccups.
    fn read_all(&self) -> Reading {
        let f = |id: u8| self.feature(id).ok();

        let charge_runtime = f(report::CHARGE_RUNTIME);
        let load = f(report::LOAD);
        let rated = f(report::RATED_POWER);
        let status_report = f(report::PRESENT_STATUS);

        Reading {
            charge: charge_runtime.as_deref().and_then(decode::charge),
            runtime_s: charge_runtime.as_deref().and_then(decode::runtime_s),
            load_pct: load.as_deref().and_then(decode::load_pct),
            rated_watts: rated.as_deref().and_then(decode::rated_watts),
            input_volts: f(report::INPUT_VOLTS)
                .as_deref()
                .and_then(decode::input_volts),
            battery_volts: f(report::BATTERY_VOLTS)
                .as_deref()
                .and_then(decode::battery_volts),
            battery_installed: f(report::MANUFACTURE_DATE)
                .as_deref()
                .and_then(decode::manufacture_date),
            last_transfer: f(report::LAST_TRANSFER)
                .as_deref()
                .and_then(decode::last_transfer),
            shutdown_delay: f(report::DELAY_BEFORE_SHUTDOWN)
                .as_deref()
                .and_then(decode::delay_before_shutdown),
            status: status_report
                .as_deref()
                .map(|r| self.status_of(r, false))
                .unwrap_or_default(),
            have_status: status_report.is_some(),
        }
    }
}

impl Ups for raw::Device {
    fn feature(&self, report_id: u8) -> io::Result<Vec<u8>> {
        self.get_feature(report_id, FEATURE_TIMEOUT_MS)
    }

    fn input(&self, timeout_ms: u32) -> io::Result<Option<Vec<u8>>> {
        self.read_input(timeout_ms)
    }

    fn status_of(&self, report: &[u8], from_input: bool) -> PresentStatus {
        let ty = if from_input { HidP_Input } else { HidP_Feature };
        caps::present_status(self.preparsed(), ty, report)
    }
}

/// Open the UPS, or say why not.
pub fn open(serial: Option<&str>) -> io::Result<raw::Device> {
    let info = raw::find_ups(serial)?;
    raw::Device::open(&info)
}

// ---------------------------------------------------------------------------
// A scripted fake, for tests that need a device but no hardware
// ---------------------------------------------------------------------------

/// Answers from a fixed table. Missing reports produce an error, which is how
/// the partial-reading path gets exercised.
pub struct FakeUps {
    pub features: Vec<(u8, Vec<u8>)>,
    pub inputs: std::cell::RefCell<Vec<Vec<u8>>>,
}

impl FakeUps {
    pub fn new(features: Vec<(u8, Vec<u8>)>) -> FakeUps {
        FakeUps {
            features,
            inputs: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl Ups for FakeUps {
    fn feature(&self, report_id: u8) -> io::Result<Vec<u8>> {
        self.features
            .iter()
            .find(|(id, _)| *id == report_id)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such report"))
    }

    fn input(&self, _timeout_ms: u32) -> io::Result<Option<Vec<u8>>> {
        Ok(self.inputs.borrow_mut().pop())
    }

    /// The fake has no preparsed data, so it takes the flags it was handed.
    /// Tests that care about flag decoding exercise `PresentStatus::from_usages`
    /// directly — that is the pure part, and the part worth testing.
    fn status_of(&self, _report: &[u8], _from_input: bool) -> PresentStatus {
        PresentStatus::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_unit() -> FakeUps {
        FakeUps::new(vec![
            (report::CHARGE_RUNTIME, vec![0x0C, 0x64, 0x23, 0x0A, 0x00]),
            (report::LOAD, vec![0x50, 0x14, 0x00, 0x00, 0x00]),
            (report::RATED_POWER, vec![0x52, 0x84, 0x03, 0x00, 0x00]),
            (report::INPUT_VOLTS, vec![0x31, 0x75, 0x00, 0x00, 0x00]),
            (report::BATTERY_VOLTS, vec![0x09, 0xA6, 0x0A, 0x00, 0x00]),
            (report::MANUFACTURE_DATE, vec![0x07, 0x77, 0x53, 0x00, 0x00]),
        ])
    }

    #[test]
    fn a_full_sweep_reproduces_the_captured_unit() {
        let r = live_unit().read_all();
        assert_eq!(r.charge, Some(100));
        assert_eq!(r.runtime_s, Some(2595));
        assert_eq!(r.load_pct, Some(20));
        assert_eq!(r.watts(), Some(180));
        assert_eq!(r.input_volts, Some(117));
        assert_eq!(r.battery_volts, Some(27.26));
        assert_eq!(r.battery_installed, Some((2021, 11, 23)));
    }

    /// One unreadable report must not blank the whole menu.
    #[test]
    fn a_missing_report_leaves_a_hole_not_an_error() {
        let ups = FakeUps::new(vec![(report::LOAD, vec![0x50, 0x14, 0x00, 0x00, 0x00])]);
        let r = ups.read_all();
        assert_eq!(r.load_pct, Some(20));
        assert_eq!(r.charge, None);
        assert_eq!(r.watts(), None, "no rated power means no watts, not zero");
    }
}
