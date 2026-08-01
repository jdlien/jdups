//! Direct Win32 HID. No `hidapi`.
//!
//! Three reasons, all of which cost something to learn — see
//! docs/implementation-plan.md, "Why not hidapi":
//!
//! 1. No bounded feature I/O. A wedged device would freeze whichever thread
//!    called it, including the one on its way to a shutdown decision.
//! 2. It hides the preparsed data, and `PresentStatus` cannot be decoded
//!    correctly without `HidP_GetUsages`.
//! 3. It silently retries with access `0` when read-write fails. That handle
//!    still serves feature reports but cannot `ReadFile`, and nothing tells you
//!    — the whole event-driven design would disable itself with no error.
//!
//! So this module owns enumeration, opening, and both I/O paths, and it is
//! explicit about which access mode it actually got.

use std::ffi::c_void;
use std::io;

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetPreparsedData, HidD_GetSerialNumberString, HidD_SetNumInputBuffers, HidP_GetCaps,
    HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_PENDING, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};

/// The UPS. APC = American Power Conversion.
pub const VID_APC: u16 = 0x051D;
pub const PID_BACKUPS: u16 = 0x0002;
/// HID Power Device / UPS. The collection we want, not merely the right VID.
pub const USAGE_PAGE_POWER: u16 = 0x84;
pub const USAGE_UPS: u16 = 0x04;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

// CTL_CODE(FILE_DEVICE_KEYBOARD=0x0b, function, method, FILE_ANY_ACCESS=0).
// hidclass.h builds the HID IOCTLs this way; the method bits are the part that
// is easy to get wrong, so `probe_feature_ioctl` checks the choice against
// HidD_GetFeature rather than trusting it.
const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
    (device << 16) | (access << 14) | (function << 2) | method
}
const FILE_DEVICE_KEYBOARD: u32 = 0x0b;

/// `IOCTL_HID_GET_FEATURE` — `HID_OUT_CTL_CODE(100)`, i.e. **METHOD_OUT_DIRECT**.
///
/// The method bits are the easy thing to get wrong here, and getting them wrong
/// fails as a flat `ERROR_INVALID_FUNCTION` that says nothing about which of the
/// four encodings the driver wanted. METHOD_BUFFERED was tried first and
/// rejected; `--probe` re-derives this against the live device on every run, so
/// a wrong constant cannot survive a single smoke test.
pub const IOCTL_HID_GET_FEATURE: u32 = ctl_code(FILE_DEVICE_KEYBOARD, 100, 2, 0);

/// The four possible method encodings of function 100, for `--probe` to try.
pub const FEATURE_IOCTL_CANDIDATES: [(&str, u32); 4] = [
    ("METHOD_BUFFERED", ctl_code(FILE_DEVICE_KEYBOARD, 100, 0, 0)),
    ("METHOD_IN_DIRECT", ctl_code(FILE_DEVICE_KEYBOARD, 100, 1, 0)),
    ("METHOD_OUT_DIRECT", ctl_code(FILE_DEVICE_KEYBOARD, 100, 2, 0)),
    ("METHOD_NEITHER", ctl_code(FILE_DEVICE_KEYBOARD, 100, 3, 0)),
];

/// How the handle was actually opened.
///
/// Recorded rather than assumed. An access-`0` handle serves feature reports but
/// cannot `ReadFile`, so a monitor thread on one would park forever with no
/// error — exactly the silent failure that made hidapi unusable here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    Read,
    None,
}

impl Access {
    pub fn can_read_input(self) -> bool {
        matches!(self, Access::ReadWrite | Access::Read)
    }

    fn flags(self) -> u32 {
        match self {
            Access::ReadWrite => GENERIC_READ | GENERIC_WRITE,
            Access::Read => GENERIC_READ,
            Access::None => 0,
        }
    }
}

/// One HID collection found by enumeration.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    path: Vec<u16>,
    pub path_display: String,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub serial: Option<String>,
    pub input_len: u16,
    pub feature_len: u16,
}

impl DeviceInfo {
    /// The unit's serial, as the device reports it.
    ///
    /// Shown plainly. This used to be masked, which was over-cautious: a UPS
    /// serial is not a credential, and a diagnostic tool that will not tell you
    /// which unit it picked is worse than useless when there are two. The real
    /// concern — not committing the serial to a public repo — is about what goes
    /// in `docs/`, and hiding it at runtime never addressed that anyway.
    ///
    /// Trimmed because the device space-pads it.
    pub fn serial_display(&self) -> &str {
        match self.serial.as_deref().map(str::trim) {
            None | Some("") => "(none)",
            Some(s) => s,
        }
    }
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Every HID collection currently present.
///
/// This is the work `hidapi` was doing for us. It is also where the selection
/// rule lives: a UPS is not "the first thing with the right VID".
pub fn enumerate() -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    unsafe {
        let mut guid: GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);

        let set = SetupDiGetClassDevsW(
            &guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        // HDEVINFO is an isize, not a HANDLE, so the invalid value has to be
        // spelled out rather than compared against INVALID_HANDLE_VALUE.
        if set == -1isize {
            return out;
        }

        let mut index = 0u32;
        loop {
            let mut ifdata: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            ifdata.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut ifdata) == 0 {
                break; // ERROR_NO_MORE_ITEMS
            }
            index += 1;

            // Called twice: once for the size, once for the data. The struct is
            // a variable-length trailing string, so this is a byte buffer with a
            // header written into it, not a value type.
            let mut needed = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &ifdata,
                std::ptr::null_mut(),
                0,
                &mut needed,
                std::ptr::null_mut(),
            );
            if needed == 0 || GetLastError() != ERROR_INSUFFICIENT_BUFFER {
                continue;
            }

            let mut buf = vec![0u8; needed as usize];
            // cbSize is the size of the FIXED header (8 on x64), never the size
            // of the buffer. Getting this wrong fails as ERROR_INVALID_USER_BUFFER
            // and reads like a buffer-size problem, which sends you the wrong way.
            let header_size = if cfg!(target_pointer_width = "64") { 8u32 } else { 6u32 };
            std::ptr::write(buf.as_mut_ptr() as *mut u32, header_size);

            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &ifdata,
                buf.as_mut_ptr() as *mut _,
                needed,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
            {
                continue;
            }

            // DevicePath sits at offset 4 in the struct, even though cbSize is 8.
            let path_bytes = &buf[4..];
            let path_u16: Vec<u16> = path_bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            let mut path = path_u16.clone();
            path.push(0);

            if let Some(info) = inspect(&path) {
                out.push(info);
            }
        }

        SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// Open a candidate read-only to ask what it is. Access `0` so this never
/// disturbs anything, including PowerChute.
unsafe fn inspect(path: &[u16]) -> Option<DeviceInfo> {
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut attrs: HIDD_ATTRIBUTES = unsafe { std::mem::zeroed() };
    attrs.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    let got_attrs = unsafe { HidD_GetAttributes(h, &mut attrs) };

    let mut pp: PHIDP_PREPARSED_DATA = 0;
    let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
    let mut got_caps = false;
    if unsafe { HidD_GetPreparsedData(h, &mut pp) } {
        got_caps = unsafe { HidP_GetCaps(pp, &mut caps) } == HIDP_STATUS_SUCCESS;
        unsafe { HidD_FreePreparsedData(pp) };
    }

    let mut sbuf = [0u16; 128];
    let serial = if unsafe {
        HidD_GetSerialNumberString(h, sbuf.as_mut_ptr() as *mut c_void, std::mem::size_of_val(&sbuf) as u32)
    } {
        // Trimmed: this device pads its serial with trailing spaces, which
        // otherwise survive masking and give away the real length.
        let s = wide_to_string(&sbuf).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    } else {
        None
    };

    unsafe { CloseHandle(h) };

    if !got_attrs || !got_caps {
        return None;
    }

    Some(DeviceInfo {
        path_display: wide_to_string(path),
        path: path.to_vec(),
        vid: attrs.VendorID,
        pid: attrs.ProductID,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        serial,
        input_len: caps.InputReportByteLength,
        feature_len: caps.FeatureReportByteLength,
    })
}

/// `HIDP_STATUS_SUCCESS`. Not zero — a plain `== 0` check silently skips every
/// caps walk and looks like the device has no capabilities.
pub const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000u32 as i32;

/// Pick the UPS from everything present.
///
/// Filters on the *collection*, not just the VID/PID, and fails closed when more
/// than one thing matches. "First match wins" is unsafe the moment a second UPS
/// or a second top-level collection shows up, and it matters most for the
/// Phase 8 writes.
pub fn find_ups(serial: Option<&str>) -> io::Result<DeviceInfo> {
    let all = enumerate();
    let mut matches: Vec<DeviceInfo> = all
        .into_iter()
        .filter(|d| {
            d.vid == VID_APC
                && d.pid == PID_BACKUPS
                && d.usage_page == USAGE_PAGE_POWER
                && d.usage == USAGE_UPS
        })
        .collect();

    if let Some(want) = serial {
        matches.retain(|d| d.serial.as_deref() == Some(want));
    }

    match matches.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no APC UPS (051D:0002, usage page 0x84, usage 0x04) found",
        )),
        1 => Ok(matches.remove(0)),
        n => Err(io::Error::other(
            format!("{n} matching UPS collections; pass a serial to disambiguate"),
        )),
    }
}

// ---------------------------------------------------------------------------
// The open device
// ---------------------------------------------------------------------------

pub struct Device {
    handle: HANDLE,
    event: HANDLE,
    preparsed: PHIDP_PREPARSED_DATA,
    pub access: Access,
    pub info: DeviceInfo,
}

// The handle is owned exclusively by this Device and Win32 handles are
// process-wide, so moving one to the monitor thread is sound.
unsafe impl Send for Device {}

impl Device {
    /// Open, preferring read-write and **recording** what was actually granted.
    ///
    /// All four access modes succeed on this unit with PowerChute running, but
    /// nothing would tell us if that changed, so the answer is kept rather than
    /// assumed.
    pub fn open(info: &DeviceInfo) -> io::Result<Device> {
        unsafe {
            let mut chosen = Access::None;
            let mut handle = INVALID_HANDLE_VALUE;
            for access in [Access::ReadWrite, Access::Read, Access::None] {
                let h = CreateFileW(
                    info.path.as_ptr(),
                    access.flags(),
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                );
                if h != INVALID_HANDLE_VALUE {
                    handle = h;
                    chosen = access;
                    break;
                }
            }
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }

            let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            if event.is_null() {
                let e = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(e);
            }

            let mut preparsed: PHIDP_PREPARSED_DATA = 0;
            if !HidD_GetPreparsedData(handle, &mut preparsed) {
                let e = io::Error::last_os_error();
                CloseHandle(event);
                CloseHandle(handle);
                return Err(e);
            }

            // The class driver queues input reports per file object, so this is
            // our own buffer and does not affect PowerChute's.
            HidD_SetNumInputBuffers(handle, 64);

            Ok(Device {
                handle,
                event,
                preparsed,
                access: chosen,
                info: info.clone(),
            })
        }
    }

    pub fn preparsed(&self) -> PHIDP_PREPARSED_DATA {
        self.preparsed
    }

    /// Wait for an overlapped operation already issued on `self.handle`.
    ///
    /// On timeout the request is cancelled and drained, so the OVERLAPPED and
    /// its buffer are safe to drop — abandoning a pending request would let the
    /// kernel write into freed memory later.
    unsafe fn wait(&self, ov: &mut OVERLAPPED, timeout_ms: u32) -> io::Result<u32> {
        let err = unsafe { GetLastError() };
        if err != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let mut n = 0u32;
        match unsafe { WaitForSingleObject(self.event, timeout_ms) } {
            WAIT_OBJECT_0 => {
                if unsafe { GetOverlappedResult(self.handle, ov, &mut n, 0) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(n)
            }
            WAIT_TIMEOUT => {
                unsafe { CancelIoEx(self.handle, ov) };
                // TRUE: block until the cancellation actually completes.
                unsafe { GetOverlappedResult(self.handle, ov, &mut n, 1) };
                Err(io::Error::new(io::ErrorKind::TimedOut, "HID request timed out"))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Read one input report, or `Ok(None)` on timeout.
    ///
    /// The stream is multiplexed — reports 12, 6, 19, 20, 22 and 33 all arrive
    /// on this one handle — so callers must dispatch on `buf[0]`.
    pub fn read_input(&self, timeout_ms: u32) -> io::Result<Option<Vec<u8>>> {
        if !self.access.can_read_input() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "handle opened without GENERIC_READ; input reports unavailable",
            ));
        }
        let len = self.info.input_len.max(1) as usize;
        let mut buf = vec![0u8; len];
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.event;
            ResetEvent(self.event);

            let ok = ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                len as u32,
                std::ptr::null_mut(),
                &mut ov,
            );
            let n = if ok != 0 {
                let mut got = 0u32;
                GetOverlappedResult(self.handle, &ov, &mut got, 1);
                got
            } else {
                match self.wait(&mut ov, timeout_ms) {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => return Ok(None),
                    Err(e) => return Err(e),
                }
            };
            buf.truncate(n as usize);
        }
        Ok(Some(buf))
    }

    /// A feature report, with a deadline.
    ///
    /// This is the whole reason for the direct Win32 layer: `HidD_GetFeature` is
    /// synchronous with no way out, so a wedged device would hang the caller.
    pub fn get_feature(&self, report_id: u8, timeout_ms: u32) -> io::Result<Vec<u8>> {
        self.get_feature_via(IOCTL_HID_GET_FEATURE, report_id, timeout_ms)
    }

    /// As `get_feature`, but with the IOCTL spelled out — so `--probe` can
    /// establish which encoding this driver actually accepts instead of the
    /// constant being folklore.
    pub fn get_feature_via(
        &self,
        ioctl: u32,
        report_id: u8,
        timeout_ms: u32,
    ) -> io::Result<Vec<u8>> {
        let len = self.info.feature_len.max(2) as usize;
        let mut buf = vec![0u8; len];
        buf[0] = report_id;
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.event;
            ResetEvent(self.event);

            let ok = DeviceIoControl(
                self.handle,
                ioctl,
                buf.as_mut_ptr() as *mut c_void,
                len as u32,
                buf.as_mut_ptr() as *mut c_void,
                len as u32,
                std::ptr::null_mut(),
                &mut ov,
            );
            let n = if ok != 0 {
                let mut got = 0u32;
                GetOverlappedResult(self.handle, &ov, &mut got, 1);
                got
            } else {
                self.wait(&mut ov, timeout_ms)?
            };
            // Deliberately *not* truncated to `n`. The driver reports only the
            // bytes it actually wrote, so a report ending in zeros comes back
            // short — report 12 as 4 bytes rather than 5. The buffer was
            // zero-initialised at full report length, which is exactly what
            // HidD_GetFeature hands back, and the decoders length-check against
            // that. Truncating here silently breaks every field whose top byte
            // happens to be zero.
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "feature report returned no data",
                ));
            }
        }
        if buf.first().copied() != Some(report_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("asked for report {report_id}, got {:?}", buf.first()),
            ));
        }
        Ok(buf)
    }

    /// The blocking path, kept only as a cross-check for `get_feature`.
    ///
    /// If the IOCTL constant were wrong, `get_feature` would fail or return
    /// something else, and comparing the two is how that gets caught with
    /// evidence rather than guessed at.
    pub fn get_feature_blocking(&self, report_id: u8) -> io::Result<Vec<u8>> {
        let len = self.info.feature_len.max(2) as usize;
        let mut buf = vec![0u8; len];
        buf[0] = report_id;
        let ok = unsafe { HidD_GetFeature(self.handle, buf.as_mut_ptr() as *mut c_void, len as u32) };
        if !ok {
            return Err(io::Error::last_os_error());
        }
        Ok(buf)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            if self.preparsed != 0 {
                HidD_FreePreparsedData(self.preparsed);
            }
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
            if self.handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic SetupAPI footgun, asserted so a refactor cannot quietly
    /// change it to the buffer size.
    #[test]
    fn the_interface_detail_header_is_eight_bytes_on_x64() {
        if cfg!(target_pointer_width = "64") {
            assert_eq!(
                std::mem::size_of::<
                    windows_sys::Win32::Devices::DeviceAndDriverInstallation::SP_DEVICE_INTERFACE_DETAIL_DATA_W,
                >(),
                8
            );
        }
    }

    /// 0x00110000, not 0. Checking `== 0` makes every caps walk silently return
    /// nothing, which looks exactly like a device with no capabilities.
    #[test]
    fn hidp_success_is_not_zero() {
        assert_ne!(HIDP_STATUS_SUCCESS, 0);
        assert_eq!(HIDP_STATUS_SUCCESS as u32, 0x0011_0000);
    }

    #[test]
    fn only_a_read_capable_handle_can_take_input_reports() {
        assert!(Access::ReadWrite.can_read_input());
        assert!(Access::Read.can_read_input());
        assert!(!Access::None.can_read_input());
    }

    /// The device space-pads its serial, and the padding is noise in every
    /// place it gets shown.
    #[test]
    fn a_serial_is_shown_whole_but_trimmed() {
        let d = |s: Option<&str>| DeviceInfo {
            path: vec![0],
            path_display: String::new(),
            vid: 0,
            pid: 0,
            usage_page: 0,
            usage: 0,
            serial: s.map(String::from),
            input_len: 0,
            feature_len: 0,
        };
        assert_eq!(d(Some("0B2148N01995  ")).serial_display(), "0B2148N01995");
        assert_eq!(d(None).serial_display(), "(none)");
        assert_eq!(d(Some("   ")).serial_display(), "(none)");
    }
}
