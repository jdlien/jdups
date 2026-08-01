//! jdups-tray — the notification-area icon and its menu.
//!
//! Hand-rolled Win32, no toolkit. The structure is lifted from jdrgb's tray,
//! which paid for every sharp edge in here once already — the retry loop on
//! NIM_ADD, NIM_SETVERSION after *every* add, TaskbarCreated recovery, the
//! never-shown top-level window that is deliberately not HWND_MESSAGE.
//!
//! A separate binary from the CLI because a PE has one subsystem, and a
//! windows-subsystem process cannot hand a shell its stdout or its exit code.
//! See Cargo.toml.

#![windows_subsystem = "windows"]

mod device;
mod draw;

use std::time::Duration;

use jdups::model::{Power, Snapshot};

use windows_sys::Win32::Foundation::{
    GetLastError, GlobalFree, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::Shell::{
    SetCurrentProcessExplicitAppUserModelID, ShellExecuteW, Shell_NotifyIconGetRect,
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
    NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW,
    NOTIFYICONIDENTIFIER, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
/// The device thread has something new. Payload-free; the UI reads the snapshot.
const WM_SNAPSHOT: u32 = WM_APP + 2;

/// `WM_USER`-based notification-icon events windows-sys doesn't surface.
const NIN_SELECT: u32 = 0x0400;
const NIN_KEYSELECT: u32 = 0x0401;

const ID_STATUS: u32 = 1;
const ID_LOAD: u32 = 2;
const ID_INPUT: u32 = 3;
const ID_BATTERY: u32 = 4;
const ID_INSTALLED: u32 = 5;
const ID_OPEN_LOG: u32 = 6;
const ID_EXIT: u32 = 7;

const CF_UNICODETEXT: u32 = 13;

struct App {
    hwnd: HWND,
    nid: NOTIFYICONDATAW,
    icon: HICON,
    /// Cached on `(state, dpi)`, not state alone. jdrgb's icon cache keys on
    /// state only, so dragging the taskbar to a different-DPI monitor keeps the
    /// old bitmap; the same bug would land here by inheritance.
    icon_key: Option<(draw::Gauge, u32)>,
    taskbar_created: u32,
    monitor: Option<device::Monitor>,
    last_power: Option<Power>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy into a fixed wide buffer, truncating on a char boundary and always
/// leaving room for the terminator. `szInfo` is 256 units and `szInfoTitle` 64.
fn set_field(dst: &mut [u16], s: &str) {
    let cap = dst.len().saturating_sub(1);
    let mut n = 0;
    dst.fill(0);
    for ch in s.chars() {
        let w = ch.len_utf16();
        if n + w > cap {
            break;
        }
        ch.encode_utf16(&mut dst[n..n + w]);
        n += w;
    }
}

/// What Windows attributes notifications to.
///
/// `NOTIFYICONDATA` has no app-name field, so a `NIF_INFO` balloon — which
/// Windows 10+ renders as a toast — is headed with whatever identity the
/// process has. With none, that is the raw executable name, "jdups-tray.exe".
///
/// **This cuts a documented corner.** Microsoft specifies an AppUserModelID as
/// `Company.Product.SubProduct.Version` and says it must not contain spaces;
/// the conforming way to get a friendly heading is to install a Start Menu
/// shortcut carrying the ID as a `System.AppUserModel.ID` property, after which
/// Windows shows the *shortcut's* name. That needs a shortcut, an install step,
/// and `IShellLink` + `IPropertyStore` — a lot of machinery for a line of text,
/// and it would do nothing for a build run straight out of `target\release`.
///
/// An unregistered ID is simply displayed verbatim, which was verified here:
/// `JDLien.jdups.UPSStatus` rendered as itself, and this renders as "UPS Status".
/// The only surfaces it reaches are the toast heading and the entry under
/// Settings > Notifications, and it is right for both.
///
/// If a future Windows starts enforcing the format, the symptom is a heading
/// that reverts to the exe name, and the fix is the shortcut route above.
const APP_ID: &str = "UPS Status";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut serial = None;
    let mut test_balloon = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--serial" => serial = args.next(),
            // Fire a notification at startup. Notifications otherwise only
            // appear on a real power transition, which is a slow way to iterate
            // on how one looks.
            "--balloon" => test_balloon = true,
            _ => {}
        }
    }
    std::process::exit(run(serial, test_balloon));
}

fn run(serial: Option<String>, test_balloon: bool) -> i32 {
    unsafe {
        // Before any notification: this is what a toast is attributed to.
        SetCurrentProcessExplicitAppUserModelID(wide(APP_ID).as_ptr());

        // Before any window or DPI-dependent call, or the first measurement is
        // taken in the wrong units and everything downstream inherits it.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        if already_running() {
            return 0;
        }
        init_dark_mode();

        let hwnd = create_window();
        if hwnd.is_null() {
            return 1;
        }

        let mut app = Box::new(App {
            hwnd,
            nid: core::mem::zeroed(),
            icon: core::ptr::null_mut(),
            icon_key: None,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            monitor: None,
            last_power: None,
        });
        app.monitor = Some(device::Monitor::start(hwnd, WM_SNAPSHOT, serial));

        let app = Box::into_raw(app);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);

        add_icon(app);
        refresh(app);
        if test_balloon {
            balloon(app, "UPS Status", "Test notification.");
        }

        let mut msg: MSG = core::mem::zeroed();
        while GetMessageW(&mut msg, core::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Stop and join the device thread *before* tearing down the window, so
        // nothing can post to a destroyed HWND.
        if let Some(m) = (*app).monitor.as_mut() {
            m.stop();
        }
        Shell_NotifyIconW(NIM_DELETE, &(*app).nid);
        if !(*app).icon.is_null() {
            DestroyIcon((*app).icon);
        }
        drop(Box::from_raw(app));
        0
    }
}

/// A second launch would stack a duplicate icon. The handle is deliberately
/// leaked: it must live as long as we do.
unsafe fn already_running() -> bool {
    let name = wide("Local\\jdups-tray-singleton");
    let h = unsafe { CreateMutexW(core::ptr::null(), 1, name.as_ptr()) };
    h.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS
}

unsafe fn create_window() -> HWND {
    let class = wide("jdups_tray");
    let hinst = unsafe { GetModuleHandleW(core::ptr::null()) };
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: core::ptr::null_mut(),
        hCursor: core::ptr::null_mut(),
        hbrBackground: core::ptr::null_mut(),
        lpszMenuName: core::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    unsafe { RegisterClassW(&wc) };
    // A real top-level window that is simply never shown — *not* HWND_MESSAGE.
    // Message-only windows don't receive broadcasts, and TaskbarCreated is one.
    unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            hinst,
            core::ptr::null(),
        )
    }
}

/// Windows 11 dark menus, via the undocumented uxtheme ordinals.
///
/// Every lookup is guarded: these are ordinals, not exports, and a future build
/// is free to renumber them. Losing dark menus is cosmetic, so the failure mode
/// is a light menu rather than a crash.
unsafe fn init_dark_mode() {
    let name = wide("uxtheme.dll");
    let lib = unsafe {
        LoadLibraryExW(name.as_ptr(), core::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32)
    };
    if lib.is_null() {
        return;
    }
    if let Some(p) = unsafe { GetProcAddress(lib, 135 as _) } {
        let set: extern "system" fn(i32) -> i32 = unsafe { core::mem::transmute(p) };
        set(1); // AllowDark: follow the system setting
    }
    if let Some(p) = unsafe { GetProcAddress(lib, 136 as _) } {
        let flush: extern "system" fn() = unsafe { core::mem::transmute(p) };
        flush();
    }
}

unsafe fn add_icon(app: *mut App) {
    let nid = unsafe { &mut (*app).nid };
    nid.cbSize = core::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = unsafe { (*app).hwnd };
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    set_field(&mut nid.szTip, "UPS");

    // Explorer's notification area may not be ready yet at logon.
    for attempt in 0..10 {
        if unsafe { Shell_NotifyIconW(NIM_ADD, nid) } != 0 {
            break;
        }
        if attempt == 9 {
            return;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }

    // Must follow *every* successful add, including Explorer-restart recovery:
    // it selects the modern callback packing the WM_TRAY handler assumes.
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, nid) };
}

fn gauge_for(s: &Snapshot) -> draw::Gauge {
    let power = s.power();
    let tint = match power {
        Power::Mains => draw::Tint::Mains,
        Power::Battery => draw::Tint::Battery,
        Power::Critical => draw::Tint::Critical,
        Power::Unknown => draw::Tint::Unknown,
    };
    let known = !matches!(power, Power::Unknown);
    draw::Gauge {
        charge: if known { s.reading.charge } else { None },
        digits: if known { s.reading.icon_digits() } else { None },
        tint,
    }
}

unsafe fn refresh(app: *mut App) {
    let snap = match unsafe { (*app).monitor.as_ref() } {
        Some(m) => m.snapshot(),
        None => return,
    };

    let gauge = gauge_for(&snap);
    let dpi = unsafe { icon_dpi(app) };

    if unsafe { (*app).icon_key } != Some((gauge, dpi)) {
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }.max(12);
        let icon = unsafe { draw::icon(size, &draw::pixels(size, gauge)) };
        if !icon.is_null() {
            let old = unsafe { (*app).icon };
            unsafe {
                (*app).icon = icon;
                (*app).icon_key = Some((gauge, dpi));
                let nid = &mut (*app).nid;
                nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
                nid.hIcon = icon;
                set_field(&mut nid.szTip, &tooltip(&snap));
                Shell_NotifyIconW(NIM_MODIFY, nid);
                if !old.is_null() {
                    DestroyIcon(old);
                }
            }
        }
    } else {
        // Same icon, but the numbers behind it may have moved.
        unsafe {
            let nid = &mut (*app).nid;
            nid.uFlags = NIF_TIP | NIF_SHOWTIP;
            set_field(&mut nid.szTip, &tooltip(&snap));
            Shell_NotifyIconW(NIM_MODIFY, nid);
        }
    }

    // Notify on a real transition only. The stream delivers a report every
    // second or two, so "something arrived" means nothing on its own.
    let power = snap.power();
    let previous = unsafe { (*app).last_power };
    unsafe { (*app).last_power = Some(power) };
    if let Some(prev) = previous {
        if prev != power {
            let (title, body) = match power {
                Power::Mains => ("Power restored", snap.status_line()),
                Power::Battery | Power::Critical => ("On battery", snap.status_line()),
                Power::Unknown => ("UPS not responding", snap.status_line()),
            };
            unsafe { balloon(app, title, &body) };
        }
    }
}

fn tooltip(s: &Snapshot) -> String {
    let mut t = format!("UPS: {}", s.status_line());
    if let (Some(l), Some(w)) = (s.reading.load_pct, s.reading.watts()) {
        t.push_str(&format!("\nLoad {l}%  ({w} W)"));
    }
    if let Some(v) = s.reading.input_volts {
        t.push_str(&format!("\nInput {v} V"));
    }
    t
}

unsafe fn balloon(app: *mut App, title: &str, text: &str) {
    unsafe {
        let nid = &mut (*app).nid;
        nid.uFlags = NIF_INFO;
        set_field(&mut nid.szInfoTitle, title);
        set_field(&mut nid.szInfo, text);
        Shell_NotifyIconW(NIM_MODIFY, nid);
    }
}

/// The DPI of the monitor the tray icon is actually on — not the hidden
/// window's, which never moves and can easily report a different monitor.
unsafe fn icon_dpi(app: *mut App) -> u32 {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: core::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: unsafe { (*app).hwnd },
        uID: 1,
        guidItem: unsafe { core::mem::zeroed() },
    };
    let mut rect: RECT = unsafe { core::mem::zeroed() };
    if unsafe { Shell_NotifyIconGetRect(&id, &mut rect) } == 0 {
        return unsafe {
            dpi_for_point(POINT {
                x: (rect.left + rect.right) / 2,
                y: (rect.top + rect.bottom) / 2,
            })
        };
    }
    let dpi = unsafe { GetDpiForWindow((*app).hwnd) };
    if dpi == 0 {
        96
    } else {
        dpi
    }
}

unsafe fn dpi_for_point(pt: POINT) -> u32 {
    let mon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let (mut dx, mut dy) = (96u32, 96u32);
    if unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } == 0 && dx > 0 {
        dx
    } else {
        96
    }
}

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------

unsafe fn add_item(menu: HMENU, id: u32, text: &str, enabled: bool) {
    let mut label = wide(text);
    let mut mii: MENUITEMINFOW = unsafe { core::mem::zeroed() };
    mii.cbSize = core::mem::size_of::<MENUITEMINFOW>() as u32;
    mii.fMask = MIIM_ID | MIIM_STRING | MIIM_STATE;
    mii.wID = id;
    mii.dwTypeData = label.as_mut_ptr();
    mii.fState = if enabled { MFS_ENABLED } else { MFS_DISABLED };
    unsafe { InsertMenuItemW(menu, GetMenuItemCount(menu) as u32, 1, &mii) };
}

unsafe fn show_menu(app: *mut App, x: i32, y: i32) {
    let snap = match unsafe { (*app).monitor.as_ref() } {
        Some(m) => m.snapshot(),
        None => return,
    };
    let r = &snap.reading;
    let menu = unsafe { CreatePopupMenu() };
    let na = |v: Option<String>| v.unwrap_or_else(|| "n/a".into());

    unsafe {
        add_item(menu, ID_STATUS, &snap.status_line(), true);
        AppendMenuW(menu, MF_SEPARATOR, 0, core::ptr::null());

        // Every data row is enabled and copies the whole readout. A disabled
        // Win32 item renders dim and reads as broken, and owner-draw would
        // forfeit the Windows 11 rounded menu styling — so giving the rows a
        // real action is what lets them be legible honestly.
        add_item(
            menu,
            ID_LOAD,
            &format!(
                "Load\t{}",
                na(r.load_pct.map(|l| match r.watts() {
                    Some(w) => format!("{l}%  ({w} W)"),
                    None => format!("{l}%"),
                }))
            ),
            true,
        );
        add_item(
            menu,
            ID_INPUT,
            &format!("Input\t{}", na(r.input_volts.map(|v| format!("{v} V")))),
            true,
        );
        add_item(
            menu,
            ID_BATTERY,
            &format!("Battery\t{}", na(r.battery_volts.map(|v| format!("{v:.2} V")))),
            true,
        );
        // Disabled, and therefore dim, on purpose — the one place the greyed
        // look is right. This is reference material, not a live reading: it
        // matters on the day you notice the battery is six years old, and never
        // otherwise. The other rows earn full contrast by changing.
        add_item(
            menu,
            ID_INSTALLED,
            &format!(
                "Battery installed\t{}",
                na(r.battery_installed.map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}")))
            ),
            false,
        );

        AppendMenuW(menu, MF_SEPARATOR, 0, core::ptr::null());
        // Only when there is one. An "Open log" that reports there is no log is
        // furniture; the sampler is a separate install, and its absence is not
        // an error the tray should keep announcing.
        if jdups::logfile::newest_log().is_some() {
            add_item(menu, ID_OPEN_LOG, "Open log", true);
            AppendMenuW(menu, MF_SEPARATOR, 0, core::ptr::null());
        }
        add_item(menu, ID_EXIT, "Exit", true);
    }

    // Required for tray menus: without the foreground switch the menu can fail
    // to dismiss, and without the trailing post it misbehaves on the next open.
    let hwnd = unsafe { (*app).hwnd };
    unsafe { SetForegroundWindow(hwnd) };
    let cmd = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
            x,
            y,
            0,
            hwnd,
            core::ptr::null(),
        )
    };
    unsafe { PostMessageW(hwnd, WM_NULL, 0, 0) };
    unsafe { DestroyMenu(menu) };

    if cmd > 0 {
        unsafe { on_command(app, cmd as u32, &snap) };
    }
}

unsafe fn on_command(app: *mut App, id: u32, snap: &Snapshot) {
    match id {
        ID_EXIT => {
            unsafe { PostMessageW((*app).hwnd, WM_CLOSE, 0, 0) };
        }
        ID_OPEN_LOG => {
            if let Some(p) = jdups::logfile::newest_log() {
                unsafe { open_as_text(&p) };
            }
        }
        // ID_INSTALLED is here although its row is disabled and cannot send a
        // command: if that row ever earns its way back to enabled, copying is
        // what it should do.
        ID_STATUS | ID_LOAD | ID_INPUT | ID_BATTERY | ID_INSTALLED => {
            unsafe { copy_to_clipboard((*app).hwnd, &snap.reading.summary()) };
        }
        _ => {}
    }
}

/// Open a file with whatever handles **`.txt`**, not whatever handles `.csv`.
///
/// The distinction matters. The log is `.csv` because that is what it is and
/// because charting battery decay is the reason it exists — but the shell's
/// `open` verb for `.csv` is Excel, which is a heavy way to glance at a file.
/// "Open log" should do what a log implies.
///
/// So the `.txt` handler is resolved and handed the path. That is still *your*
/// editor rather than a hardcoded one — the plan is explicit about not
/// bundling or naming a viewer — it is simply asking the association system a
/// more useful question. Falls back to the plain `open` verb if the lookup
/// fails, which at worst restores the Excel behaviour.
unsafe fn open_as_text(path: &std::path::Path) {
    use windows_sys::Win32::UI::Shell::{AssocQueryStringW, ASSOCF_NONE, ASSOCSTR_EXECUTABLE};

    let file = wide(&path.to_string_lossy());
    let verb = wide("open");

    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_EXECUTABLE,
            wide(".txt").as_ptr(),
            verb.as_ptr(),
            buf.as_mut_ptr(),
            &mut len,
        )
    } == 0;

    if ok {
        // Quoted: the log lives under a path with spaces in it more often than
        // not, and an unquoted argument would be split at the first one.
        let args = wide(&format!("\"{}\"", path.display()));
        let exe = buf.as_ptr();
        let r = unsafe {
            ShellExecuteW(
                core::ptr::null_mut(),
                verb.as_ptr(),
                exe,
                args.as_ptr(),
                core::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecute returns <= 32 on failure.
        if r as isize > 32 {
            return;
        }
    }

    unsafe {
        ShellExecuteW(
            core::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
}

/// The clipboard, in the order that actually works.
///
/// `OpenClipboard(NULL)` followed by `EmptyClipboard` makes the subsequent
/// `SetClipboardData` fail: EmptyClipboard assigns ownership to the opener, and
/// a NULL owner cannot own it. On success the clipboard takes the handle and it
/// must **not** be freed; on failure it is still ours.
unsafe fn copy_to_clipboard(hwnd: HWND, text: &str) -> bool {
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = std::mem::size_of_val(&utf16[..]);

    // Contention is ordinary, not exceptional — another process may hold it.
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(hwnd) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if !opened {
        return false;
    }

    let ok = unsafe {
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h.is_null() {
            false
        } else {
            let p = GlobalLock(h);
            if p.is_null() {
                GlobalFree(h);
                false
            } else {
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), p as *mut u16, utf16.len());
                GlobalUnlock(h);
                if SetClipboardData(CF_UNICODETEXT, h).is_null() {
                    GlobalFree(h); // still ours; the clipboard refused it
                    false
                } else {
                    true // the clipboard owns it now — do not free
                }
            }
        }
    };
    unsafe { CloseClipboard() };
    ok
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let app = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if app.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }

    // Explorer restarted and threw away every icon; put ours back.
    if msg == unsafe { (*app).taskbar_created } {
        unsafe {
            add_icon(app);
            (*app).icon_key = None; // force the icon to be re-attached
            refresh(app);
        }
        return 0;
    }

    match msg {
        WM_TRAY => {
            // NOTIFYICON_VERSION_4 packing: event in the low word of lParam,
            // anchor in wParam. Coordinates are signed — a monitor to the left
            // gives negative x.
            let event = (lp as u32) & 0xFFFF;
            let x = (wp & 0xFFFF) as u16 as i16 as i32;
            let y = ((wp >> 16) & 0xFFFF) as u16 as i16 as i32;
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT) {
                unsafe { show_menu(app, x, y) };
            }
            0
        }
        WM_SNAPSHOT => {
            unsafe { refresh(app) };
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_ENDSESSION => {
            // Only when the session is actually ending. jdrgb deletes the icon
            // on every WM_ENDSESSION, so a *cancelled* shutdown (wParam FALSE)
            // leaves it running with no icon until Explorer restarts.
            if wp != 0 {
                unsafe { Shell_NotifyIconW(NIM_DELETE, &(*app).nid) };
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jdups::decode::{PresentStatus, PAGE_BATTERY};
    use jdups::model::Reading;

    fn snap(reading: Reading, ok: bool, age: Option<u64>) -> Snapshot {
        Snapshot {
            reading,
            device_ok: ok,
            error: None,
            stream_age_s: age,
            sweep_age_s: age,
        }
    }

    fn mains(charge: u8) -> Reading {
        Reading {
            charge: Some(charge),
            runtime_s: Some(2595),
            status: PresentStatus::from_usages(&[(PAGE_BATTERY, 0xD0), (PAGE_BATTERY, 0xD1)]),
            have_status: true,
            ..Default::default()
        }
    }

    /// A device that stopped answering must not keep showing its last good
    /// charge — that is the failure that looks healthy.
    #[test]
    fn a_lost_device_draws_unknown_not_the_last_reading() {
        let g = gauge_for(&snap(mains(100), false, Some(1)));
        assert_eq!(g.tint, draw::Tint::Unknown);
        assert_eq!(g.charge, None);
        assert_eq!(g.digits, None);
    }

    #[test]
    fn a_stale_stream_is_also_unknown() {
        let g = gauge_for(&snap(mains(100), true, Some(Snapshot::STALE_AFTER_S + 1)));
        assert_eq!(g.tint, draw::Tint::Unknown);
        assert_eq!(g.charge, None);
    }

    #[test]
    fn a_healthy_reading_draws_its_charge() {
        let g = gauge_for(&snap(mains(100), true, Some(1)));
        assert_eq!(g.tint, draw::Tint::Mains);
        assert_eq!(g.charge, Some(100));
        assert_eq!(g.digits, None, "a full unit on mains says nothing");
    }

    #[test]
    fn set_field_truncates_on_a_char_boundary() {
        let mut buf = [0u16; 8];
        set_field(&mut buf, "abcdefghijklmnop");
        assert_eq!(buf[7], 0);
        assert_eq!(String::from_utf16(&buf[..7]).unwrap(), "abcdefg");
    }

    #[test]
    fn set_field_never_splits_a_surrogate_pair() {
        let mut buf = [0u16; 4];
        set_field(&mut buf, "ab\u{1F600}");
        assert_eq!(&buf[..3], &['a' as u16, 'b' as u16, 0]);
    }
}
