//! The report descriptor, and the only correct way to read `PresentStatus`.
//!
//! `HidP_GetUsages` decodes a report against the device's own parsed descriptor.
//! The alternative — working out bit positions by hand — produces flags that
//! look plausible and are wrong, and one of those flags decides whether the
//! machine shuts down.

use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidP_GetButtonCaps, HidP_GetUsages, HidP_GetValueCaps, HidP_MaxUsageListLength, HidP_Feature,
    HidP_Input, HIDP_BUTTON_CAPS, HIDP_REPORT_TYPE, HIDP_VALUE_CAPS, PHIDP_PREPARSED_DATA,
};

use crate::decode::{PresentStatus, PAGE_APC, PAGE_BATTERY, PAGE_POWER};

use super::raw::HIDP_STATUS_SUCCESS;

/// One entry from a caps walk, flattened to what `--probe` wants to show.
#[derive(Debug, Clone)]
pub struct Cap {
    pub report_id: u8,
    pub usage_page: u16,
    pub usage: u16,
    pub bit_size: u16,
    pub logical_min: i32,
    pub logical_max: i32,
    pub is_button: bool,
}

impl Cap {
    pub fn name(&self) -> &'static str {
        usage_name(self.usage_page, self.usage)
    }
}

/// Which usages of `page` are *set* in `report`.
///
/// Returns an empty list rather than an error when the report simply has no
/// usages from that page — that is the ordinary case, not a failure, since this
/// gets asked once per page against one report.
pub fn usages_set(
    preparsed: PHIDP_PREPARSED_DATA,
    report_type: HIDP_REPORT_TYPE,
    page: u16,
    report: &[u8],
) -> Vec<(u16, u16)> {
    unsafe {
        let max = HidP_MaxUsageListLength(report_type, page, preparsed);
        if max == 0 {
            return Vec::new();
        }
        let mut list = vec![0u16; max as usize];
        let mut len = max;
        // The API takes a mutable pointer but does not modify the report.
        let mut scratch = report.to_vec();
        let status = HidP_GetUsages(
            report_type,
            page,
            0,
            list.as_mut_ptr(),
            &mut len,
            preparsed,
            scratch.as_mut_ptr(),
            scratch.len() as u32,
        );
        if status != HIDP_STATUS_SUCCESS {
            return Vec::new();
        }
        list.truncate(len as usize);
        list.into_iter().map(|u| (page, u)).collect()
    }
}

/// Decode report 22 into named flags.
///
/// Asks each page the device actually uses, because `HidP_GetUsages` is
/// per-page and the flags are spread across the Power and Battery pages.
pub fn present_status(
    preparsed: PHIDP_PREPARSED_DATA,
    report_type: HIDP_REPORT_TYPE,
    report: &[u8],
) -> PresentStatus {
    let mut all = Vec::new();
    for page in [PAGE_POWER, PAGE_BATTERY, PAGE_APC] {
        all.extend(usages_set(preparsed, report_type, page, report));
    }
    PresentStatus::from_usages(&all)
}

/// Every value cap for a report type.
pub fn value_caps(preparsed: PHIDP_PREPARSED_DATA, report_type: HIDP_REPORT_TYPE) -> Vec<Cap> {
    unsafe {
        let mut len: u16 = 0;
        // First call sizes the buffer; it fails with BUFFER_TOO_SMALL by design.
        HidP_GetValueCaps(report_type, std::ptr::null_mut(), &mut len, preparsed);
        if len == 0 {
            return Vec::new();
        }
        let mut buf: Vec<HIDP_VALUE_CAPS> = vec![std::mem::zeroed(); len as usize];
        if HidP_GetValueCaps(report_type, buf.as_mut_ptr(), &mut len, preparsed)
            != HIDP_STATUS_SUCCESS
        {
            return Vec::new();
        }
        buf.truncate(len as usize);
        buf.iter()
            .map(|c| Cap {
                report_id: c.ReportID,
                usage_page: c.UsagePage,
                // NotRange is the right arm for every entry on this device;
                // IsRange would mean a span of usages rather than one.
                usage: c.Anonymous.NotRange.Usage,
                bit_size: c.BitSize,
                logical_min: c.LogicalMin,
                logical_max: c.LogicalMax,
                is_button: false,
            })
            .collect()
    }
}

/// Every button cap for a report type.
///
/// Worth its own pass: these are the 1-bit usages, and a walk that only asks for
/// *value* caps misses `PresentStatus` entirely — which is exactly how report 22
/// stayed unidentified through the original investigation.
pub fn button_caps(preparsed: PHIDP_PREPARSED_DATA, report_type: HIDP_REPORT_TYPE) -> Vec<Cap> {
    unsafe {
        let mut len: u16 = 0;
        HidP_GetButtonCaps(report_type, std::ptr::null_mut(), &mut len, preparsed);
        if len == 0 {
            return Vec::new();
        }
        let mut buf: Vec<HIDP_BUTTON_CAPS> = vec![std::mem::zeroed(); len as usize];
        if HidP_GetButtonCaps(report_type, buf.as_mut_ptr(), &mut len, preparsed)
            != HIDP_STATUS_SUCCESS
        {
            return Vec::new();
        }
        buf.truncate(len as usize);
        buf.iter()
            .map(|c| Cap {
                report_id: c.ReportID,
                usage_page: c.UsagePage,
                usage: c.Anonymous.NotRange.Usage,
                bit_size: 1,
                logical_min: 0,
                logical_max: 1,
                is_button: true,
            })
            .collect()
    }
}

/// All four walks, sorted by report ID — what `--probe` prints.
pub fn walk(preparsed: PHIDP_PREPARSED_DATA) -> Vec<(&'static str, Vec<Cap>)> {
    let mut out = Vec::new();
    for (label, ty) in [("FEATURE", HidP_Feature), ("INPUT", HidP_Input)] {
        let mut caps = value_caps(preparsed, ty);
        caps.extend(button_caps(preparsed, ty));
        caps.sort_by_key(|c| (c.report_id, c.usage_page, c.usage));
        out.push((label, caps));
    }
    out
}

/// HID Power Device usage names, for human-readable probe output.
pub fn usage_name(page: u16, usage: u16) -> &'static str {
    match (page, usage) {
        (PAGE_POWER, 0x02) => "PresentStatus",
        (PAGE_POWER, 0x1A) => "PowerSummary",
        (PAGE_POWER, 0x30) => "Voltage",
        (PAGE_POWER, 0x31) => "Current",
        (PAGE_POWER, 0x32) => "Frequency",
        (PAGE_POWER, 0x35) => "PercentLoad",
        (PAGE_POWER, 0x40) => "ConfigVoltage",
        (PAGE_POWER, 0x42) => "ConfigFrequency",
        (PAGE_POWER, 0x43) => "ConfigApparentPower",
        (PAGE_POWER, 0x44) => "ConfigActivePower",
        (PAGE_POWER, 0x53) => "LowVoltageTransfer",
        (PAGE_POWER, 0x54) => "HighVoltageTransfer",
        (PAGE_POWER, 0x55) => "DelayBeforeReboot",
        (PAGE_POWER, 0x56) => "DelayBeforeStartup",
        (PAGE_POWER, 0x57) => "DelayBeforeShutdown",
        (PAGE_POWER, 0x58) => "Test",
        (PAGE_POWER, 0x5A) => "AudibleAlarmControl",
        (PAGE_POWER, 0x61) => "Good",
        (PAGE_POWER, 0x65) => "Overload",
        (PAGE_POWER, 0x69) => "ShutdownImminent",
        (PAGE_POWER, 0x73) => "CommunicationLost",
        (PAGE_POWER, 0xFD) => "iManufacturer",
        (PAGE_POWER, 0xFE) => "iProduct",
        (PAGE_POWER, 0xFF) => "iSerialNumber",
        (PAGE_BATTERY, 0x29) => "RemainingCapacityLimit",
        (PAGE_BATTERY, 0x2C) => "CapacityMode",
        (PAGE_BATTERY, 0x42) => "BelowRemainingCapacityLimit",
        (PAGE_BATTERY, 0x43) | (PAGE_BATTERY, 0x4B) => "RemainingTimeLimitExpired",
        (PAGE_BATTERY, 0x44) => "Charging",
        (PAGE_BATTERY, 0x45) => "Discharging",
        (PAGE_BATTERY, 0x46) => "FullyCharged",
        (PAGE_BATTERY, 0x47) => "FullyDischarged",
        (PAGE_BATTERY, 0x65) => "DesignCapacity",
        (PAGE_BATTERY, 0x66) => "RemainingCapacity",
        (PAGE_BATTERY, 0x67) => "FullChargeCapacity",
        (PAGE_BATTERY, 0x68) => "RunTimeToEmpty",
        (PAGE_BATTERY, 0x83) => "iDeviceChemistry",
        (PAGE_BATTERY, 0x85) => "ManufacturerDate",
        (PAGE_BATTERY, 0x89) => "iDeviceName",
        (PAGE_BATTERY, 0x8B) => "Rechargeable",
        (PAGE_BATTERY, 0x8D) => "CapacityGranularity1",
        (PAGE_BATTERY, 0x8E) => "CapacityGranularity2",
        (PAGE_BATTERY, 0x8F) => "iOEMInformation",
        (PAGE_BATTERY, 0xD0) => "ACPresent",
        (PAGE_BATTERY, 0xD1) => "BatteryPresent",
        (PAGE_BATTERY, 0xDB) => "VoltageNotRegulated",
        (PAGE_APC, 0x52) => "APC:LastTransferReason",
        (PAGE_APC, _) => "APC vendor",
        _ => "",
    }
}
