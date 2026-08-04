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
mod hooks;
mod png;

use std::time::Duration;

use jdups::decode;
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
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::Shell::{
    SetCurrentProcessExplicitAppUserModelID, ShellExecuteW, Shell_NotifyIconGetRect,
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
    NIF_SHOWTIP, NIF_TIP, NIIF_NONE, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
/// The device thread has something new. Payload-free; the UI reads the snapshot.
const WM_SNAPSHOT: u32 = WM_APP + 2;
/// `--balloon`, forwarded to whichever instance already owns the icon.
const WM_TEST_BALLOON: u32 = WM_APP + 3;
/// Drives the countdown while a shutdown is pending, and only then.
const TIMER_COUNTDOWN: usize = 1;
/// Polls the agent's status file once a second, always.
///
/// The file cannot be event-driven from here: the agent arming a shutdown
/// changes nothing the *device* reports, and `WM_SNAPSHOT` only arrives when
/// the device snapshot changes -- which, on battery with a steady load and the
/// smoother doing its job, can be most of a minute. A warning that waits for
/// the next snapshot is a warning that can arrive after the shutdown.
const TIMER_AGENT: usize = 2;

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
const ID_ALARM: u32 = 8;
const ID_OPEN_CONFIG: u32 = 9;

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
    /// The last agent status sequence the user was told about. See
    /// `check_agent`: the agent runs as SYSTEM and cannot show a notification,
    /// so this process is the only half that can deliver its warning.
    agent_seq: Option<u64>,
    /// `GetTickCount64` at which the shutdown lands, while one is pending.
    ///
    /// Held as a deadline rather than as a remaining count so the icon can tick
    /// off a local timer. The agent publishes whole seconds, several times a
    /// second, and a countdown that reads straight from the file would jump
    /// around -- losing the one thing that makes the digits legible as seconds,
    /// that they move one at a time.
    ///
    /// **Latched, not re-derived.** Recomputing this on every publish treats a
    /// value the agent produced most of a second ago as if it were current, so
    /// the deadline slides forward and back and the display skips and repeats.
    /// It is re-synced only on a disagreement too large to be the quantisation
    /// of a whole-second field. See `check_agent`.
    pending_until: Option<u64>,
    /// The last countdown value actually painted, so the timer can fire often
    /// without repainting the icon four times a second.
    last_countdown_shown: Option<u64>,
    /// Whether the notification icon is actually in the tray. `NIM_MODIFY`
    /// cannot re-create a deleted icon, so once an add fails the only recovery
    /// is another `NIM_ADD`, retried from the agent timer.
    icon_added: bool,
    /// Whether the user has been told "On battery" and not yet given the
    /// all-clear. What lets a recovery that happened behind a device dropout
    /// still announce "Power restored". See `notice`.
    saw_outage: bool,
    /// Stat-before-parse reader for the agent's status file, polled at 1 Hz.
    agent_reader: jdups::status::Reader,
    /// The agent's shutdown estimate while on battery, ready to render: the
    /// seconds and the prose the agent composed for what ends them.
    agent_eta: Option<(u64, String)>,
    /// The power-event hook commands and their change detector. The tray is
    /// the only executor; see docs/power-hooks.md for why the agent must not be.
    hooks: hooks::Hooks,
    hook_cmds: hooks::Commands,
    /// Whether the agent that published the pending shutdown can actually act.
    /// The menu and the notification have to say "would" rather than "will"
    /// while it cannot, or a dry run reads as an emergency.
    agent_armed: bool,
    /// Whether the user has been told the UPS stopped answering. See `notice`:
    /// without it, the `Unknown` -> `Mains` settle that happens at every single
    /// startup is indistinguishable from a real reconnection.
    reported_missing: bool,
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

/// Give the AppUserModelID a name and a picture, so the toast header has
/// something to draw in the gap beside the app name.
///
/// The heading worked without this because an unregistered ID is displayed
/// verbatim. The **icon** does not: the shell looks the ID up, finds nothing,
/// and leaves the space empty. That is the gap.
///
/// `HKCU\Software\Classes\AppUserModelId\<id>` is the documented registration
/// for an unpackaged desktop app, and it is per-user, so this needs no
/// elevation and no installer step — a build run straight out of
/// `target\release` registers itself the same way an installed one does.
///
/// `IconUri` is a **path to a file**, which is why there is a PNG writer in this
/// crate: an `HICON` cannot be handed over. The file is rewritten on every start
/// rather than only when missing, so changing how the battery is drawn does not
/// leave a stale picture behind forever.
///
/// Best-effort throughout. A failure here costs a picture, and taking the tray
/// down over it would be a bad trade.
unsafe fn register_app_id() {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let Some(icon) = write_toast_icon() else { return };

    let path = wide(&format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
    let mut key: HKEY = core::ptr::null_mut();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            core::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            core::ptr::null(),
            &mut key,
            core::ptr::null_mut(),
        )
    };
    if rc != 0 || key.is_null() {
        return;
    }

    let set_sz = |name: &str, value: &str| {
        let n = wide(name);
        let v = wide(value);
        unsafe {
            RegSetValueExW(
                key,
                n.as_ptr(),
                0,
                REG_SZ,
                v.as_ptr() as *const u8,
                (v.len() * 2) as u32,
            )
        };
    };
    set_sz("DisplayName", APP_ID);
    set_sz("IconUri", &icon);

    // Without this the entry is invisible under Settings > Notifications, and
    // an app whose notifications cannot be turned off from the usual place is
    // an app that has overstayed its welcome.
    let n = wide("ShowInSettings");
    let one: u32 = 1;
    unsafe {
        RegSetValueExW(
            key,
            n.as_ptr(),
            0,
            REG_DWORD,
            &one as *const u32 as *const u8,
            4,
        );
        RegCloseKey(key);
    }
}

/// Draw the plain battery and write it out as a PNG, returning its path.
///
/// 64 px because the shell scales this down to whatever the header wants and a
/// larger source survives that better than a 16 px one does. There are no digits
/// on it, so none of the per-size font tables are involved and the size is free
/// to be chosen for the medium rather than for the type.
fn write_toast_icon() -> Option<String> {
    let dir = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)?.join("jdups");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("toast-icon.png");

    const SIZE: i32 = 64;
    let px = draw::pixels(SIZE, draw::Gauge::plain());
    // 0xAARRGGBB, premultiplied, but every pixel here is fully opaque or fully
    // transparent, so premultiplied and straight are the same bytes.
    let mut rgba = Vec::with_capacity(px.len() * 4);
    for p in px {
        rgba.push((p >> 16) as u8);
        rgba.push((p >> 8) as u8);
        rgba.push(p as u8);
        rgba.push((p >> 24) as u8);
    }

    let bytes = png::encode(SIZE as u32, SIZE as u32, &rgba);
    std::fs::write(&path, bytes).ok()?;
    Some(path.to_string_lossy().into_owned())
}

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
        // Before any notification: this is what a toast is attributed to, and
        // what the shell looks up to find the header's name and icon.
        SetCurrentProcessExplicitAppUserModelID(wide(APP_ID).as_ptr());
        register_app_id();

        // Before any window or DPI-dependent call, or the first measurement is
        // taken in the wrong units and everything downstream inherits it.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        if already_running() {
            // `--balloon` used to land here and exit 0 in silence, which reads
            // exactly like a broken notification: the usual state of this
            // machine is that a tray is already running. Hand the request to
            // whoever owns the icon instead, since only that process can fire
            // a balloon on it.
            if test_balloon {
                let other = FindWindowW(wide("jdups_tray").as_ptr(), core::ptr::null());
                if !other.is_null() {
                    PostMessageW(other, WM_TEST_BALLOON, 0, 0);
                }
            }
            return 0;
        }
        init_dark_mode();

        let hwnd = create_window();
        if hwnd.is_null() {
            return 1;
        }

        // The hook commands come from the same file the agent reads. A config
        // the agent would refuse must not kill the tray; the hooks stay off.
        let hook_cmds = jdups::config::default_path()
            .and_then(|p| jdups::config::load(&p).ok())
            .map(|s| hooks::Commands::from_settings(&s))
            .unwrap_or_default();

        let mut app = Box::new(App {
            hwnd,
            nid: core::mem::zeroed(),
            icon: core::ptr::null_mut(),
            icon_key: None,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            monitor: None,
            last_power: None,
            reported_missing: false,
            agent_seq: None,
            pending_until: None,
            last_countdown_shown: None,
            icon_added: false,
            saw_outage: false,
            agent_reader: jdups::status::Reader::new(),
            agent_eta: None,
            hooks: hooks::Hooks::default(),
            hook_cmds,
            agent_armed: false,
        });
        app.monitor = Some(device::Monitor::start(hwnd, WM_SNAPSHOT, serial));

        let app = Box::into_raw(app);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);

        add_icon(app, true);
        refresh(app);
        // The agent's warning must not wait for the device to say something
        // new; see TIMER_AGENT.
        SetTimer(hwnd, TIMER_AGENT, 1_000, None);
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

unsafe fn add_icon(app: *mut App, patient: bool) -> bool {
    let nid = unsafe { &mut (*app).nid };
    nid.cbSize = core::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = unsafe { (*app).hwnd };
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    set_field(&mut nid.szTip, "UPS");

    // Explorer's notification area may not be ready yet at logon.
    //
    // **This sleeps, so it must never run on the UI thread with the full
    // budget.** `TaskbarCreated` arrives in the window procedure, and retrying
    // there for nine seconds freezes the menu, the snapshot handler and the
    // shutdown countdown along with it. `patient` is false on that path; the
    // recovery is retried from the next snapshot instead, which arrives within
    // seconds anyway.
    let tries = if patient { 10 } else { 1 };
    let mut added = false;
    for attempt in 0..tries {
        if unsafe { Shell_NotifyIconW(NIM_ADD, nid) } != 0 {
            added = true;
            break;
        }
        if attempt + 1 == tries {
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    unsafe { (*app).icon_added = added };
    if !added {
        // Deliberately not marking the icon as present. `icon_key` is left
        // alone by the caller so the next state change retries rather than
        // deciding it already drew this, and the agent timer keeps retrying
        // the add itself -- NIM_MODIFY cannot resurrect a missing icon.
        return false;
    }

    // Must follow *every* successful add, including Explorer-restart recovery:
    // it selects the modern callback packing the WM_TRAY handler assumes.
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, nid) };
    true
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

/// The icon during the grace period: the countdown, in seconds, in red.
///
/// The icon shows minutes remaining the rest of the time, and this quietly
/// switches the unit without saying so — which sounds like a trap and is not.
/// At this moment the seconds remaining *are* the runtime the machine is being
/// given, and a number ticking down one at a time reads unmistakably as seconds
/// where the same number sitting still would not. It is the one place the change
/// of unit explains itself.
///
/// Two digits is the whole budget, which is exactly enough: a warning long
/// enough to need three is a warning nobody is reading anyway, and `validate`
/// caps it well below that. Anything larger is clamped to 99 rather than
/// wrapping, so a misconfiguration shows an implausible number instead of a
/// plausible wrong one.
///
/// The charge fill stays, so the icon does not stop being a battery.
fn gauge_pending(s: &Snapshot, seconds_left: u64) -> draw::Gauge {
    draw::Gauge {
        charge: s.reading.charge,
        digits: Some(seconds_left.min(99) as u8),
        tint: draw::Tint::Critical,
    }
}

unsafe fn refresh(app: *mut App) {
    let snap = match unsafe { (*app).monitor.as_ref() } {
        Some(m) => m.snapshot(),
        None => return,
    };

    let gauge = match unsafe { pending_seconds(app) } {
        Some(n) => gauge_pending(&snap, n),
        None => gauge_for(&snap),
    };
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
            let (missing, outage) = unsafe { (&mut (*app).reported_missing, &mut (*app).saw_outage) };
            if let Some(title) = notice(prev, power, missing, outage) {
                unsafe { balloon(app, title, &snap.status_line()) };
            }
        }
    }

    unsafe { run_hooks(app, power) };
}

/// Fold the current state into the hook layer and fire at most one command.
///
/// Called wherever power or pending changes land: the end of every refresh
/// and of every pending transition. The fold is what makes that safe -- a
/// state the lights are already in never re-runs its command.
unsafe fn run_hooks(app: *mut App, power: Power) {
    let pending = unsafe { (*app).pending_until }.is_some();
    let state = hooks::state_of(power, pending);
    if let Some(s) = unsafe { &mut (*app).hooks }.observe(state) {
        if let Some(cmd) = unsafe { &(*app).hook_cmds }.for_state(s) {
            hooks::run(cmd);
        }
    }
}

/// Deliver the agent's shutdown warning, which the agent cannot deliver itself.
///
/// It runs as SYSTEM in **session 0**: no window station, no desktop, no way to
/// put anything on screen, and it must not be given one. It publishes to a file
/// that only SYSTEM and Administrators can write, and this reads it. The trust
/// runs one way, which is the point.
///
/// **The warning is the whole reason the grace period exists.** PowerChute shows
/// a dialog here instead; it does not block, and the shutdown proceeds whether
/// or not anyone clicks it. So this is a choice about how people notice things,
/// and it is not a settled one: a dialog in the middle of the screen is hard to
/// miss, and a notification in the corner of a very wide monitor is easy to. The
/// case for the notification is that it does not put anything in the way of
/// someone already hurrying to save their work.
///
/// Keyed on the published sequence number rather than on the contents, so a
/// warning fires exactly once and is never mistaken for one already given.
unsafe fn check_agent(app: *mut App) {
    use jdups::status::{Event, Phase};

    let Some(st) = (unsafe { &mut (*app).agent_reader }).read() else {
        unsafe { (*app).agent_eta = None };
        unsafe { set_pending(app, None) };
        return;
    };
    unsafe {
        (*app).agent_eta = match (st.eta_s, st.eta_why.clone()) {
            (Some(s), Some(w)) => Some((s, w)),
            _ => None,
        };
    }

    // The countdown first, and independently of the announcement. These are
    // different jobs: the notification fires once, the icon has to keep moving.
    //
    // **The deadline is latched, not re-derived.** The agent publishes whole
    // seconds several times a second, so recomputing `now + n` on every read
    // treats a value computed most of a second ago as if it were fresh, and the
    // countdown lurches forward and back while the local timer also ticks. That
    // is the "jumps over numbers" glitch. Sync once on entry, then only when the
    // published value has drifted far enough to be a real disagreement rather
    // than the quantisation of a whole-second field.
    const DRIFT_TOLERANCE_MS: i64 = 2_000;
    let until = match (st.phase, st.seconds_left) {
        (Phase::Pending, Some(n)) => {
            // Clamped: the writer is trusted (the file is SYSTEM-ACLed) but an
            // absurd value must saturate the icon at 99, not overflow the add.
            let fresh = unsafe { GetTickCount64() } + n.min(86_400) * 1000;
            match unsafe { (*app).pending_until } {
                Some(held) if (held as i64 - fresh as i64).abs() <= DRIFT_TOLERANCE_MS => Some(held),
                _ => Some(fresh),
            }
        }
        _ => None,
    };
    unsafe { (*app).agent_armed = st.armed };
    unsafe { set_pending(app, until) };

    let seen = unsafe { (*app).agent_seq };

    // First sight of the file adopts its sequence in silence. Otherwise starting
    // the tray -- at logon, after an Explorer restart -- would replay whatever
    // announcement happened to be sitting in the file, for an event long over.
    if seen.is_none() {
        unsafe { (*app).agent_seq = Some(st.seq) };
        return;
    }
    if seen == Some(st.seq) {
        return;
    }

    let dry = !st.armed;
    let reason = st.reason.unwrap_or_else(|| "the UPS is running low".into());
    let (title, body) = match st.event {
        Event::Pending => {
            let secs = st.seconds_left.unwrap_or(0);
            if dry {
                ("UPS shutdown (dry run)".to_string(), format!("Would shut down in {secs} s. {reason}."))
            } else {
                ("Shutting down soon".to_string(), format!("This PC shuts down in {secs} s. Save your work now. {reason}."))
            }
        }
        Event::Cancelled => (
            "Shutdown cancelled".to_string(),
            "Power came back. This PC is no longer shutting down.".to_string(),
        ),
        Event::Shutdown => {
            if dry {
                ("UPS shutdown (dry run)".to_string(), "This is the point where it would shut down.".to_string())
            } else {
                ("Shutting down now".to_string(), "The UPS is out of runtime.".to_string())
            }
        }
        Event::None => {
            unsafe { (*app).agent_seq = Some(st.seq) };
            return;
        }
    };
    // Seen means *delivered*. Marking the sequence before the toast went up
    // swallowed the warning for good whenever the icon happened to be absent,
    // an Explorer restart being the usual reason; leaving it unseen makes the
    // next poll redeliver.
    if unsafe { balloon(app, &title, &body) } {
        unsafe { (*app).agent_seq = Some(st.seq) };
    }
}

/// Seconds until the pending shutdown, or `None` when none is pending.
///
/// Saturating: once the deadline passes this reads zero rather than wrapping to
/// something enormous. The machine is going down at that point anyway, but an
/// icon reading 99 in its final second would be a poor last impression.
unsafe fn pending_seconds(app: *mut App) -> Option<u64> {
    let until = unsafe { (*app).pending_until }?;
    let now = unsafe { GetTickCount64() };
    Some(until.saturating_sub(now).div_ceil(1000))
}

/// Start or stop the once-a-second repaint that makes the digits tick.
///
/// Only runs while something is pending. The tray is otherwise driven entirely
/// by the device thread, and a timer left running would repaint the icon every
/// second forever for no reason.
unsafe fn set_pending(app: *mut App, until: Option<u64>) {
    let was = unsafe { (*app).pending_until };
    unsafe { (*app).pending_until = until };
    if was.is_some() == until.is_some() {
        return;
    }
    let hwnd = unsafe { (*app).hwnd };
    if until.is_some() {
        unsafe { (*app).last_countdown_shown = None };
        unsafe { SetTimer(hwnd, TIMER_COUNTDOWN, 250, None) };
    } else {
        unsafe { KillTimer(hwnd, TIMER_COUNTDOWN) };
        // Back to whatever the device says, at once, rather than at the next
        // snapshot: a cancelled shutdown should not leave a red icon sitting
        // there counting nothing.
        unsafe { refresh(app) };
    }
    // The hooks hear about the pending change now rather than at the next
    // repaint. Pending outranks an unknown power view, so a countdown that
    // begins while the tray cannot see the UPS still turns the room red.
    let power = unsafe { (*app).last_power }.unwrap_or(Power::Unknown);
    unsafe { run_hooks(app, power) };
}

/// What to tell the user about a change of power state, if anything.
///
/// Pulled out of `refresh` because it got this wrong in a way that was only
/// visible on a machine that had *not* had a power cut: every launch announced
/// **"Power restored"**. The first snapshot is always `Unknown` — the device
/// thread has not read anything yet — and the second is `Mains`, so the plain
/// `prev != now` test saw a transition and reported the wrong one. Nothing had
/// been restored. The tray had merely stopped being ignorant.
///
/// `Unknown` is not a power state. It means "no idea", and coming back from no
/// idea is only worth a notification if the user was told about it in the first
/// place, which is what `reported_missing` tracks. Without it, a startup settle
/// and a genuine reconnection are indistinguishable from the states alone.
fn notice(
    prev: Power,
    now: Power,
    reported_missing: &mut bool,
    saw_outage: &mut bool,
) -> Option<&'static str> {
    match now {
        Power::Unknown => {
            *reported_missing = true;
            Some("UPS not responding")
        }
        Power::Battery | Power::Critical => {
            *reported_missing = false;
            *saw_outage = true;
            Some("On battery")
        }
        Power::Mains => {
            let was_missing = std::mem::replace(reported_missing, false);
            let had_outage = std::mem::replace(saw_outage, false);
            match prev {
                // An outage the user was warned about ended while the device
                // was unreachable: the all-clear is owed, not a note about the
                // USB cable.
                Power::Unknown if had_outage => Some("Power restored"),
                // Never lost power; only ever lost track of it. Say so, and only
                // if the loss was announced.
                Power::Unknown => was_missing.then_some("UPS responding again"),
                _ => Some("Power restored"),
            }
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

unsafe fn balloon(app: *mut App, title: &str, text: &str) -> bool {
    // No `NIIF_USER`, and that is the point.
    //
    // There are two icons on a toast and they are not interchangeable. The one
    // this call controls is the **body** image, which Windows 11 renders large,
    // beside the text. The one in the **header**, in the gap before the app
    // name, comes from the AppUserModelID registration instead — see
    // `register_app_id`, which is what fills it.
    //
    // A body image is the wrong place for this program's mark. It is enormous
    // next to two lines of text that already say the state in words, and the
    // small header icon is what actually identifies the notification at a
    // glance. So: register the identity, and send the notification bare.
    //
    // The route not taken cost a day, and is worth leaving written down.
    // `NIIF_USER | NIIF_LARGE_ICON` supplies the body image from an `HICON`, and
    // `NIIF_LARGE_ICON` means `SM_CXICON` **at the current DPI**, nothing else.
    // A hardcoded 32 is right only at 100 % scaling; at 125 % the shell wants 40
    // and refuses the call with `ERROR_INCORRECT_SIZE`. It does not degrade to a
    // plain toast — `Shell_NotifyIconW` returns 0 and **nothing appears at
    // all**. An icon that is merely wrong is visible; a notification that never
    // fires is not, and this one stayed broken until it was measured rather
    // than reasoned about.
    unsafe {
        let nid = &mut (*app).nid;
        nid.uFlags = NIF_INFO;
        set_field(&mut nid.szInfoTitle, title);
        set_field(&mut nid.szInfo, text);
        nid.dwInfoFlags = NIIF_NONE;
        nid.hBalloonIcon = core::ptr::null_mut();
        Shell_NotifyIconW(NIM_MODIFY, nid) != 0
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

/// A menu item carrying a tick, for state rather than an action.
unsafe fn add_check(menu: HMENU, id: u32, text: &str, checked: bool, enabled: bool) {
    let mut label = wide(text);
    let mut mii: MENUITEMINFOW = unsafe { core::mem::zeroed() };
    mii.cbSize = core::mem::size_of::<MENUITEMINFOW>() as u32;
    mii.fMask = MIIM_ID | MIIM_STRING | MIIM_STATE;
    mii.wID = id;
    mii.dwTypeData = label.as_mut_ptr();
    mii.fState = if enabled { MFS_ENABLED } else { MFS_DISABLED }
        | if checked { MFS_CHECKED } else { MFS_UNCHECKED };
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
        // A pending shutdown goes first, above the reading. The icon turning red
        // and counting down is what sends someone here, and the menu answering
        // "On battery, 83%, 31 min" while the icon says 15 leaves them to work
        // out the connection themselves.
        if let Some(n) = pending_seconds(app) {
            let armed = (*app).agent_armed;
            let line = if armed {
                format!("Shutting down in {n} s")
            } else {
                format!("Would shut down in {n} s (dry run)")
            };
            add_item(menu, ID_STATUS, &line, true);
        }
        // Two columns like every data row below, so one prose line does not
        // set the whole menu's width.
        let (state, detail) = snap.status_columns();
        if detail.is_empty() {
            add_item(menu, ID_STATUS, &state, true);
        } else {
            add_item(menu, ID_STATUS, &format!("{state}\t{detail}"), true);
        }
        // What is about to happen to the machine, while it still is not:
        // during an outage, the agent's own estimate of when it will act. Gone
        // once the countdown starts -- the pending line above says the rest.
        if pending_seconds(app).is_none() {
            if let Some((secs, why)) = (*app).agent_eta.as_ref() {
                add_item(menu, ID_STATUS, &eta_line((*app).agent_armed, *secs), true);
                // The clock behind the estimate, dim like the install date:
                // reference material, not a live reading.
                add_item(menu, ID_STATUS, why, false);
            }
        }
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
        }
        // Always: the file may not exist yet, and opening an editor at the
        // place it belongs -- elevated if that is what saving there takes --
        // is exactly how it comes to exist.
        add_item(menu, ID_OPEN_CONFIG, "Open config", true);
        AppendMenuW(menu, MF_SEPARATOR, 0, core::ptr::null());
        // Deliberately down here with the actions, not up among the readings.
        // Every row above copies to the clipboard; this one changes the UPS, and
        // reaching for a number should not be able to silence an alarm. Greyed
        // out until the state has actually been read, so it never shows a tick
        // that is really a guess.
        add_check(
            menu,
            ID_ALARM,
            "Audible alarm",
            r.alarm == Some(decode::ALARM_ENABLED),
            r.alarm.is_some(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, core::ptr::null());

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
        ID_ALARM => {
            // Toggle against what the device last reported, not against a local
            // guess: 3 (muted) is a state the UPS can enter on its own, and
            // toggling from it should enable rather than pretend it was on.
            let now = unsafe { (*app).monitor.as_ref() }
                .map(|m| m.snapshot().reading.alarm)
                .unwrap_or(None);
            let next = if now == Some(decode::ALARM_ENABLED) {
                decode::ALARM_DISABLED
            } else {
                decode::ALARM_ENABLED
            };
            if let Some(m) = unsafe { (*app).monitor.as_ref() } {
                m.set_alarm(next);
            }
        }
        ID_OPEN_LOG => {
            if let Some(p) = jdups::logfile::newest_log() {
                unsafe { open_as_text(&p, false) };
            }
        }
        ID_OPEN_CONFIG => {
            if let Some(p) = jdups::config::default_path() {
                let elevated = config_needs_elevation(&p);
                unsafe { open_as_text(&p, elevated) };
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
/// `elevated` launches the same editor through the "runas" verb instead, which
/// is what makes "Open config" useful on a machine-wide install: the conf is
/// deliberately admin-writable only, and an unelevated Notepad can look at it
/// but never save. The association is still queried with "open" -- "runas" is
/// an execution verb, not a file-type one.
unsafe fn open_as_text(path: &std::path::Path, elevated: bool) {
    use windows_sys::Win32::UI::Shell::{AssocQueryStringW, ASSOCF_NONE, ASSOCSTR_EXECUTABLE};

    /// `SE_ERR_ACCESSDENIED`, which is also what a declined UAC prompt returns.
    const ACCESS_DENIED: isize = 5;

    let file = wide(&path.to_string_lossy());
    let assoc_verb = wide("open");
    let verb = wide(if elevated { "runas" } else { "open" });

    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_EXECUTABLE,
            wide(".txt").as_ptr(),
            assoc_verb.as_ptr(),
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
        } as isize;
        // ShellExecute returns <= 32 on failure.
        if r > 32 {
            return;
        }
        // The user said no to the UAC prompt. That is an answer, not a failure
        // to route around; falling through would only prompt them again.
        if elevated && r == ACCESS_DENIED {
            return;
        }
    }

    if elevated {
        // "runas" needs an executable, not a document, so the document-handler
        // fallback below cannot elevate. Notepad is the one editor always
        // present, and the shell resolves the bare name; a hardcoded viewer is
        // against the house rules for the normal path, but the elevated
        // fallback is already the path where the association system failed us.
        let args = wide(&format!("\"{}\"", path.display()));
        unsafe {
            ShellExecuteW(
                core::ptr::null_mut(),
                verb.as_ptr(),
                wide("notepad.exe").as_ptr(),
                args.as_ptr(),
                core::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        return;
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

/// The menu line under the status row during an outage. Two columns, like the
/// rest of the menu; the *why* renders as its own dim row, so it stopped
/// living here.
///
/// Rounded to minutes, and "~" earned three ways: the runtime reading jitters,
/// the charge floor is not modelled and can fire sooner, and the settle window
/// can defer the start. "Would shut down" is the dry-run marker, same as the
/// pending line and the journal.
fn eta_line(armed: bool, eta_s: u64) -> String {
    let when = if eta_s < 90 {
        "in about a minute".to_string()
    } else {
        format!("in ~{} min", (eta_s + 30) / 60)
    };
    if armed {
        format!("Shutdown\t{when}")
    } else {
        format!("Would shut down\t{when}")
    }
}

/// Whether saving the config will take an elevated editor.
///
/// Decided by asking the filesystem, not by guessing the install shape:
/// opening for append is exactly the permission a save needs and changes
/// nothing by itself. A per-user install's conf says yes and gets a plain
/// editor with no UAC prompt; a machine-wide install's says no, because the
/// installer ACLed the directory to SYSTEM/Administrators-write on purpose.
///
/// A file that does not exist yet is judged by whether we could create it,
/// with the probe removed at once. If the remove ever lost a race, an empty
/// conf means the defaults, exactly like a missing one.
fn config_needs_elevation(path: &std::path::Path) -> bool {
    use std::io::ErrorKind;
    match std::fs::OpenOptions::new().append(true).open(path) {
        Ok(_) => false,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(_) => {
                    let _ = std::fs::remove_file(path);
                    false
                }
                Err(_) => true,
            }
        }
        Err(_) => true,
    }
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
            // Not patient: this is the UI thread, and a nine-second retry here
            // stalls everything including the shutdown countdown.
            add_icon(app, false);
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
            unsafe { check_agent(app) };
            0
        }
        WM_TIMER if wp == TIMER_AGENT => {
            unsafe { check_agent(app) };
            // The icon can only be restored by another NIM_ADD; a failed add
            // at logon or after an Explorer crash used to leave the tray
            // running invisibly for the rest of the session.
            if !unsafe { (*app).icon_added } && unsafe { add_icon(app, false) } {
                unsafe { (*app).icon_key = None };
                unsafe { refresh(app) };
            }
            0
        }
        WM_TIMER if wp == TIMER_COUNTDOWN => {
            // Fires four times a second so the digit changes promptly when it
            // should, but repaints only when it actually differs. A one-second
            // timer lands wherever it lands relative to the real boundary, which
            // is the other half of the countdown looking uneven.
            let now = unsafe { pending_seconds(app) };
            if now != unsafe { (*app).last_countdown_shown } {
                unsafe { (*app).last_countdown_shown = now };
                unsafe { refresh(app) };
            }
            0
        }
        WM_TEST_BALLOON => {
            unsafe { balloon(app, "UPS Status", "Test notification.") };
            0
        }
        WM_CLOSE => {
            // Stop and join the device thread *before* the window dies, so a
            // late PostMessageW cannot target a destroyed -- or worse,
            // recycled -- HWND. The join after the message loop stays as the
            // backstop; stop() is idempotent.
            if let Some(m) = unsafe { (*app).monitor.as_mut() } {
                m.stop();
            }
            unsafe { DestroyWindow(hwnd) };
            0
        }
        // DPI or theme moved. The icon is keyed on (gauge, dpi) and the dark
        // mode flush runs once at startup, so without these the change waits
        // for the next device-driven repaint -- unbounded on steady mains.
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            unsafe { init_dark_mode() };
            unsafe { (*app).icon_key = None };
            unsafe { refresh(app) };
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
                unsafe { (*app).icon_added = false };
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

    /// The estimate is a two-column row like the rest of the menu, in minutes
    /// because the seconds jitter, never claiming precision it does not have.
    /// The why lives on its own dim row and is not this function's problem.
    #[test]
    fn the_eta_line_is_two_columns_and_rounds_to_minutes() {
        assert_eq!(eta_line(true, 1720), "Shutdown\tin ~29 min");
        assert_eq!(eta_line(false, 1720), "Would shut down\tin ~29 min");
        // Under 90 seconds the number would be noise; the words carry it.
        assert_eq!(eta_line(true, 45), "Shutdown\tin about a minute");
        // Rounds, never truncates: 150 s is closer to 3 min than 2.
        assert!(eta_line(true, 150).contains("~3 min"));
    }

    /// A writable config never prompts for elevation, and probing a missing
    /// one leaves nothing behind. The denied case needs an ACL to simulate,
    /// so it is exercised by the machine-wide install rather than here.
    #[test]
    fn the_elevation_probe_is_honest_and_leaves_no_residue() {
        let dir = std::env::temp_dir().join("jdups-elev-probe-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let existing = dir.join("jdups.conf");
        std::fs::write(&existing, "# hi\n").unwrap();
        assert!(!config_needs_elevation(&existing), "a writable file demanded UAC");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "# hi\n", "the probe changed the file");

        let missing = dir.join("no-such.conf");
        assert!(!config_needs_elevation(&missing), "a creatable file demanded UAC");
        assert!(!missing.exists(), "the probe left its file behind");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two digits is the whole budget the icon has. A warning longer than 99 s
    /// must clamp rather than wrap: 105 seconds wrapping to "05" would be a
    /// countdown that lies in the direction of panic.
    #[test]
    fn the_countdown_clamps_instead_of_wrapping() {
        let s = snap(Reading::default(), true, Some(0));
        assert_eq!(gauge_pending(&s, 60).digits, Some(60));
        assert_eq!(gauge_pending(&s, 99).digits, Some(99));
        assert_eq!(gauge_pending(&s, 100).digits, Some(99));
        assert_eq!(gauge_pending(&s, 100_000).digits, Some(99));
    }

    /// Zero is a real value here, not a missing one. It is the last thing the
    /// icon shows before the machine goes down.
    #[test]
    fn the_countdown_shows_zero_rather_than_nothing() {
        let s = snap(Reading::default(), true, Some(0));
        assert_eq!(gauge_pending(&s, 0).digits, Some(0));
    }

    /// A pending shutdown reads as critical whatever the battery is doing, and
    /// keeps its fill so it still looks like a battery.
    #[test]
    fn a_pending_shutdown_is_always_critical() {
        let r = Reading { charge: Some(80), ..Reading::default() };
        let s = snap(r, true, Some(0));
        let g = gauge_pending(&s, 30);
        assert_eq!(g.tint, draw::Tint::Critical);
        assert_eq!(g.charge, Some(80));
    }

    /// Drive a sequence of power states and collect what the user is told.
    fn told(states: &[Power]) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut missing = false;
        let mut outage = false;
        let mut last: Option<Power> = None;
        for &now in states {
            if let Some(prev) = last {
                if prev != now {
                    if let Some(t) = notice(prev, now, &mut missing, &mut outage) {
                        out.push(t);
                    }
                }
            }
            last = Some(now);
        }
        out
    }

    /// The bug this exists to prevent. Every launch begins Unknown, because the
    /// device thread has not read anything yet, and settles to Mains a moment
    /// later. That is not a power event and must not be announced as one.
    #[test]
    fn starting_up_on_mains_says_nothing_at_all() {
        assert_eq!(told(&[Power::Unknown, Power::Mains, Power::Mains]), Vec::<&str>::new());
    }

    /// ...and the same is true of an install, a logoff, or an Explorer restart,
    /// each of which starts a fresh tray on a machine with perfectly good power.
    #[test]
    fn repeated_restarts_stay_silent() {
        for _ in 0..5 {
            assert!(told(&[Power::Unknown, Power::Mains]).is_empty());
        }
    }

    /// Starting up *during* an outage is worth saying, though. Silence here
    /// would be the opposite mistake.
    #[test]
    fn starting_up_on_battery_does_say_so() {
        assert_eq!(told(&[Power::Unknown, Power::Battery]), vec!["On battery"]);
    }

    #[test]
    fn a_real_outage_and_recovery_reads_correctly() {
        assert_eq!(
            told(&[Power::Unknown, Power::Mains, Power::Battery, Power::Critical, Power::Mains]),
            vec!["On battery", "On battery", "Power restored"]
        );
    }

    /// Losing the UPS is announced, and so is getting it back — but as a device
    /// event, not as "Power restored", which would claim an outage that never
    /// happened.
    #[test]
    fn losing_the_device_and_getting_it_back_is_not_a_power_event() {
        assert_eq!(
            told(&[Power::Unknown, Power::Mains, Power::Unknown, Power::Mains]),
            vec!["UPS not responding", "UPS responding again"]
        );
    }

    /// A device that drops out mid-outage and comes back still on battery has
    /// not had its power restored either.
    #[test]
    fn a_dropout_during_an_outage_does_not_claim_recovery() {
        let said = told(&[Power::Mains, Power::Battery, Power::Unknown, Power::Battery]);
        assert!(!said.contains(&"Power restored"), "{said:?}");
    }

    /// ...but an outage that *ends* while the device is unreachable did have
    /// its power restored, and whoever was told "On battery" is owed the
    /// all-clear rather than a note about the USB cable.
    #[test]
    fn an_outage_that_ends_during_a_dropout_still_announces_recovery() {
        assert_eq!(
            told(&[Power::Mains, Power::Battery, Power::Unknown, Power::Mains]),
            vec!["On battery", "UPS not responding", "Power restored"]
        );
    }

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
