//! Running as a Windows service.
//!
//! A scheduled task already runs at startup with no user logged in, so that is
//! **not** what this buys. Three things are:
//!
//! 1. **`SERVICE_CONTROL_POWEREVENT`.** The only way to be told the machine
//!    suspended or resumed. Nothing runs during S3 — the CPU is off — so an
//!    agent cannot watch a UPS while the machine sleeps, and no amount of
//!    engineering changes that. What it *can* do is find out afterwards, and
//!    react: if this machine woke by itself and is on battery, then the UPS is
//!    almost certainly what woke it, nobody is sitting here, and running a
//!    190 W load for twenty-five more minutes to protect an idle machine is
//!    backwards. See `Wake`.
//! 2. **`SERVICE_CONTROL_PRESHUTDOWN`.** A clean stand-down during an ordinary
//!    reboot, before the shutdown gets violent. A scheduled task is simply
//!    killed — observed directly: the agent's final read never fired during a
//!    PowerChute-driven shutdown, because a task has no console to receive
//!    `CTRL_SHUTDOWN_EVENT` on.
//! 3. Restart policy, dependencies, and a status the SCM actually knows about.
//!
//! Hand-rolled on `windows-sys` rather than taking `windows-service`. It is
//! about a hundred lines of dispatcher and status reporting, and this project's
//! whole claim is one dependency against a bundled JRE.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use jdups::stop::Stop;

use windows_sys::Win32::Foundation::ERROR_SERVICE_SPECIFIC_ERROR;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_POWEREVENT,
    SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_POWEREVENT, SERVICE_CONTROL_PRESHUTDOWN, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};

/// The service name, which must match what the installer registers.
pub const NAME: &str = "jdups-agent";

/// `PBT_*` power-broadcast codes. Not surfaced by `windows-sys` under
/// `Win32_System_Services`, and they are stable documented constants.
const PBT_APMSUSPEND: u32 = 0x0004;
/// The system resumed **without** user intervention: a device or a timer woke
/// it. This is the one that means "nobody asked for this".
const PBT_APMRESUMEAUTOMATIC: u32 = 0x0012;
/// The system resumed and a user is present. Windows sends this *in addition*
/// to the automatic one when someone actually woke the machine.
const PBT_APMRESUMESUSPEND: u32 = 0x0007;

/// What the loop needs to know about suspend and resume.
///
/// Shared with the device loop rather than acted on here: the control handler
/// runs on an SCM thread with a deadline and must return promptly, so it records
/// and gets out of the way.
#[derive(Debug, Default)]
pub struct Wake {
    /// The machine resumed and nothing indicates a person did it.
    ///
    /// Windows sends `PBT_APMRESUMEAUTOMATIC` for every resume and adds
    /// `PBT_APMRESUMESUSPEND` when a user was involved, so "automatic and not
    /// user" is the distinction that matters: it separates *the UPS woke this
    /// machine because the power failed* from *somebody pressed a key*.
    pub resumed_alone: AtomicBool,
    /// Set by a user-driven resume, which clears the above.
    pub user_present: AtomicBool,
    /// Monotonic-ish counter, so the loop can tell a fresh event from one it has
    /// already handled without clearing state the handler may be writing.
    pub seq: AtomicU32,
}

impl Wake {
    fn note(&self, event: u32) {
        match event {
            PBT_APMRESUMEAUTOMATIC => {
                self.resumed_alone.store(true, Ordering::SeqCst);
                self.seq.fetch_add(1, Ordering::SeqCst);
            }
            PBT_APMRESUMESUSPEND => {
                // A person woke it. Whatever the automatic broadcast said, this
                // machine is not unattended.
                self.user_present.store(true, Ordering::SeqCst);
                self.resumed_alone.store(false, Ordering::SeqCst);
                self.seq.fetch_add(1, Ordering::SeqCst);
            }
            PBT_APMSUSPEND => {
                self.user_present.store(false, Ordering::SeqCst);
                self.resumed_alone.store(false, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

// The control handler is an `extern "system"` callback with no useful place to
// hang state, so these are process-global. The agent is a singleton by
// construction -- see the mutex in main -- so there is exactly one of each.
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);
static WAKE: std::sync::OnceLock<Arc<Wake>> = std::sync::OnceLock::new();
static STOP: std::sync::OnceLock<Arc<Stop>> = std::sync::OnceLock::new();

/// Hand this process to the SCM. Blocks until the service stops.
///
/// Fails if the process was not started as a service, which is how `--service`
/// distinguishes a real service start from someone running it by hand.
pub fn dispatch() -> Result<(), std::io::Error> {
    let mut name: Vec<u16> = NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// What `service_main` calls to do the actual work.
///
/// Set before `dispatch`, because the SCM calls back on its own thread and
/// there is no way to pass anything in.
type Body = fn(Arc<Stop>, Arc<Wake>) -> i32;
static BODY: std::sync::OnceLock<Body> = std::sync::OnceLock::new();

pub fn set_body(f: Body) {
    let _ = BODY.set(f);
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let mut name: Vec<u16> = NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(name.as_mut_ptr(), Some(control_handler), std::ptr::null_mut())
    };
    if handle.is_null() {
        return;
    }
    STATUS_HANDLE.store(handle as usize, Ordering::SeqCst);

    report(SERVICE_START_PENDING, 0);
    // Both shared with the control handler through process-globals, and set
    // *before* RUNNING is reported so no control can arrive first. This used
    // to be bridged by a dedicated thread polling an AtomicBool every 100 ms
    // for the life of the service -- ten scheduler wakeups a second, forever,
    // and it was the single largest wakeup source the efficiency baseline
    // found. The handler now signals the loop's own Stop directly.
    let wake = Arc::new(Wake::default());
    let _ = WAKE.set(Arc::clone(&wake));
    let stop = Arc::new(Stop::new());
    let _ = STOP.set(Arc::clone(&stop));

    report(SERVICE_RUNNING, 0);

    let code = match BODY.get() {
        Some(f) => f(stop, wake),
        None => ERROR_SERVICE_SPECIFIC_ERROR as i32,
    };

    report_exit(code);
}

unsafe extern "system" fn control_handler(
    control: u32,
    event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match control {
        // Preshutdown is the point of being a service: it arrives *before* the
        // shutdown turns violent, with a timeout long enough to finish. Treated
        // exactly like a stop.
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_PRESHUTDOWN | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, 20_000);
            if let Some(s) = STOP.get() {
                s.stop();
            }
        }
        SERVICE_CONTROL_POWEREVENT => {
            if let Some(w) = WAKE.get() {
                w.note(event_type);
            }
        }
        SERVICE_CONTROL_INTERROGATE => report(SERVICE_RUNNING, 0),
        _ => {}
    }
    0
}

fn status(state: u32, wait_hint: u32) -> SERVICE_STATUS {
    SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        // Accepting preshutdown *replaces* plain shutdown, which is the point:
        // it is the one that comes early enough to be useful.
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_PRESHUTDOWN | SERVICE_ACCEPT_POWEREVENT
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: if state == SERVICE_RUNNING || state == SERVICE_STOPPED {
            0
        } else {
            CHECKPOINT.fetch_add(1, Ordering::SeqCst) + 1
        },
        dwWaitHint: wait_hint,
    }
}

fn report(state: u32, wait_hint: u32) {
    let h = STATUS_HANDLE.load(Ordering::SeqCst) as SERVICE_STATUS_HANDLE;
    if h.is_null() {
        return;
    }
    let s = status(state, wait_hint);
    unsafe { SetServiceStatus(h, &s) };
}

/// Report STOPPED, carrying a non-zero exit through so the SCM's restart policy
/// can see the difference between a clean stop and a failure.
fn report_exit(code: i32) {
    let h = STATUS_HANDLE.load(Ordering::SeqCst) as SERVICE_STATUS_HANDLE;
    if h.is_null() {
        return;
    }
    let mut s = status(SERVICE_STOPPED, 0);
    if code != 0 {
        s.dwWin32ExitCode = ERROR_SERVICE_SPECIFIC_ERROR;
        s.dwServiceSpecificExitCode = code as u32;
    }
    unsafe { SetServiceStatus(h, &s) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distinction the whole wake feature rests on. Windows sends the
    /// automatic broadcast for *every* resume and adds the user one when a
    /// person was involved, so order matters and the user signal must win.
    #[test]
    fn a_user_resume_clears_the_unattended_flag() {
        let w = Wake::default();
        w.note(PBT_APMRESUMEAUTOMATIC);
        assert!(w.resumed_alone.load(Ordering::SeqCst));

        w.note(PBT_APMRESUMESUSPEND);
        assert!(!w.resumed_alone.load(Ordering::SeqCst), "a person woke it and it still read as unattended");
        assert!(w.user_present.load(Ordering::SeqCst));
    }

    #[test]
    fn an_unattended_resume_stays_unattended() {
        let w = Wake::default();
        w.note(PBT_APMRESUMEAUTOMATIC);
        assert!(w.resumed_alone.load(Ordering::SeqCst));
        assert!(!w.user_present.load(Ordering::SeqCst));
    }

    /// Going back to sleep clears everything, so the next resume is judged on
    /// its own evidence rather than on the last one's.
    #[test]
    fn suspending_resets_the_verdict() {
        let w = Wake::default();
        w.note(PBT_APMRESUMEAUTOMATIC);
        w.note(PBT_APMSUSPEND);
        assert!(!w.resumed_alone.load(Ordering::SeqCst));
        assert!(!w.user_present.load(Ordering::SeqCst));
    }

    /// The loop keys "is this new" on the counter, so every resume has to move
    /// it -- including a user-driven one, which is a real change of verdict.
    #[test]
    fn every_resume_advances_the_sequence() {
        let w = Wake::default();
        w.note(PBT_APMRESUMEAUTOMATIC);
        assert_eq!(w.seq.load(Ordering::SeqCst), 1);
        w.note(PBT_APMRESUMESUSPEND);
        assert_eq!(w.seq.load(Ordering::SeqCst), 2);
        // Suspending is not an event the loop needs to chase.
        w.note(PBT_APMSUSPEND);
        assert_eq!(w.seq.load(Ordering::SeqCst), 2);
    }
}
