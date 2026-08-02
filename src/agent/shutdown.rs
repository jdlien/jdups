//! The shutdown transaction.
//!
//! Two things have to happen and they are not atomic: Windows has to go down,
//! and the UPS has to be told to cut its output afterwards. Every interesting
//! failure in this file is a failure *between* those two.
//!
//! **Order, and the argument for it.** The OS shutdown is requested first and
//! the UPS is armed second. Consider each half failing on its own:
//!
//! - *Armed, then the shutdown fails.* The UPS cuts power to a **running**
//!   machine. That is a corrupt filesystem.
//! - *Shutdown accepted, then the arming fails.* The machine goes down cleanly
//!   and the UPS keeps supplying a powered-off PC until the battery runs out.
//!   Wasteful, recoverable, harmless.
//!
//! The second is strictly better, so the irreversible half goes last.
//!
//! **But not with a zero timeout.** `InitiateSystemShutdownExW` returning
//! success with `dwTimeout = 0` means Windows is already going down, and this
//! process can be killed before it reaches the UPS write. So it asks for a short
//! grace period instead: that makes the shutdown *confirmed accepted* and *not
//! yet destructive* at the same moment, which is the window the arming needs.
//! If arming fails inside it, `AbortSystemShutdownW` calls the whole thing off
//! and nothing has happened.
//!
//! **Every write is read back through `set_feature`**, which polls until the
//! device agrees. The device takes ~30 ms to commit and an immediate read
//! returns the *old* value, so a naive verify would report every correct arming
//! as a failure and cancel it. That was measured, not guessed. See
//! `hid::raw::set_feature`.

use std::path::Path;

use jdups::decode::report;
use jdups::hid::raw::Device;
use jdups::hid::Ups;
use jdups::logfile;

/// How long Windows is asked to wait before it begins.
///
/// The window in which the shutdown is accepted but has not started, so the UPS
/// can be armed with something left to undo if that fails. Long enough to write
/// a HID report and read it back several times over; short enough that nobody
/// waits on it.
const OS_GRACE_S: u32 = 10;

/// What happened, in enough detail to log.
#[derive(Debug)]
pub enum Outcome {
    /// Windows is going down and the UPS will cut output.
    Committed { ups_cutoff_s: i16 },
    /// Nothing happened, and nothing is left armed.
    Aborted(String),
}

/// Do it.
///
/// Returns `Aborted` with a reason rather than an error type, because every
/// caller does the same thing with a failure: log it loudly and stay up. There
/// is no recovery this function does not already attempt itself.
pub fn execute(dev: &Device, dir: &Path, os_seconds: u32, say: &dyn Fn(&str)) -> Outcome {
    // --- 1. the privilege ------------------------------------------------
    // SYSTEM holds SE_SHUTDOWN_NAME but does **not** have it enabled, and the
    // failure is silent: AdjustTokenPrivileges returns success even when it
    // changed nothing at all.
    if let Err(e) = enable_shutdown_privilege() {
        return Outcome::Aborted(format!("could not enable the shutdown privilege: {e}"));
    }
    say("shutdown privilege enabled");

    // --- 2. the intent record --------------------------------------------
    // Written before anything happens, so a crash between the two halves leaves
    // evidence. The next start reads this and reconciles; see `reconcile`.
    let cutoff = (OS_GRACE_S + os_seconds).min(i16::MAX as u32) as i16;
    if let Err(e) = write_intent(dir, cutoff) {
        // Not fatal. An unrecorded shutdown is worse to diagnose, not worse to
        // survive, and refusing to protect the machine because a log file could
        // not be written would be the wrong trade.
        say(&format!("warning: could not record intent: {e}"));
    }

    // --- 3. ask Windows, and confirm it agreed ---------------------------
    if let Err(e) = begin_os_shutdown(OS_GRACE_S) {
        let _ = clear_intent(dir);
        return Outcome::Aborted(format!("Windows refused the shutdown: {e}"));
    }
    say(&format!("Windows accepted the shutdown, starting in {OS_GRACE_S} s"));

    // --- 4. arm the UPS, and prove it took -------------------------------
    // Report 65, not 21. The standard DelayBeforeShutdown is untouched by the
    // vendor on this unit and was never observed to do anything; 65 is what
    // PowerChute writes and what the UPS counts down. See decode::report.
    match dev.set_feature_i16(report::APC_SHUTDOWN_COUNTDOWN, cutoff) {
        Ok(Some(v)) if v == cutoff => {
            say(&format!("UPS armed: it cuts output in {cutoff} s"));
            Outcome::Committed { ups_cutoff_s: cutoff }
        }
        other => {
            // The whole reason for the grace period. Nothing is committed yet.
            let what = match other {
                Ok(Some(v)) => format!("it holds {v}, not {cutoff}"),
                Ok(None) => "it returned nothing readable".to_string(),
                Err(e) => format!("{e}"),
            };
            say(&format!("arming the UPS failed ({what}); calling the shutdown off"));
            unwind(dev, dir, say);
            Outcome::Aborted(format!("could not arm the UPS: {what}"))
        }
    }
}

/// Put everything back. Called only when something has already gone wrong, so
/// it reports each step rather than failing fast.
fn unwind(dev: &Device, dir: &Path, say: &dyn Fn(&str)) {
    match abort_os_shutdown() {
        Ok(()) => say("Windows shutdown aborted"),
        Err(e) => say(&format!("ALARM: could not abort the Windows shutdown: {e}")),
    }
    // Belt and braces: the arming may have half taken. Cancelling something
    // already cancelled costs one write.
    match dev.set_feature_i16(report::APC_SHUTDOWN_COUNTDOWN, -1) {
        Ok(Some(-1)) => say("UPS countdown confirmed cancelled"),
        Ok(v) => say(&format!("ALARM: the UPS countdown reads {v:?}, not cancelled")),
        Err(e) => say(&format!("ALARM: could not cancel the UPS countdown: {e}")),
    }
    let _ = clear_intent(dir);
}

/// On startup: is there a countdown running that nobody wants?
///
/// The dangerous case is narrow and real. If the agent armed the UPS and the
/// machine then did **not** go down -- the shutdown was vetoed, the agent was
/// killed, the OS hung and recovered -- a countdown is running against a live
/// machine and will cut power to it.
///
/// **Never blindly clear one.** During an outage a running countdown is doing
/// exactly its job, and may be the only thing that will restore power later. So
/// this cancels only with mains present *and* an intent record we wrote,
/// meaning: we armed this, and the shutdown it belonged to did not happen.
pub fn reconcile(dev: &Device, dir: &Path, on_mains: bool, say: &dyn Fn(&str)) {
    let Some(recorded) = read_intent(dir) else { return };
    say(&format!("found a shutdown intent from {recorded}"));

    let pending = dev
        .feature(report::APC_SHUTDOWN_COUNTDOWN)
        .ok()
        .filter(|b| b.len() >= 3)
        .map(|b| i16::from_le_bytes([b[1], b[2]]));

    match (pending, on_mains) {
        (Some(n), true) if n > 0 => {
            say(&format!("a UPS countdown of {n} s is running on mains; cancelling it"));
            match dev.set_feature_i16(report::APC_SHUTDOWN_COUNTDOWN, -1) {
                Ok(Some(-1)) => say("cancelled"),
                other => say(&format!("ALARM: the cancel did not take: {other:?}")),
            }
        }
        (Some(n), false) if n > 0 => {
            // Leave it. It is doing its job, and clearing it here could be the
            // difference between the machine coming back and not.
            say(&format!("a UPS countdown of {n} s is running on battery; leaving it alone"));
        }
        _ => say("no countdown pending; the shutdown completed normally"),
    }
    let _ = clear_intent(dir);
}

fn intent_path(dir: &Path) -> std::path::PathBuf {
    dir.join("shutdown-intent.txt")
}

fn write_intent(dir: &Path, cutoff: i16) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        intent_path(dir),
        format!("{}\nups_cutoff_s = {cutoff}\n", logfile::now_local().iso8601()),
    )
}

fn read_intent(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(intent_path(dir)).ok()?;
    Some(text.lines().next().unwrap_or("an unknown time").to_string())
}

fn clear_intent(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(intent_path(dir)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

// --- the Win32 half ------------------------------------------------------

/// Enable `SE_SHUTDOWN_NAME` on this process, and **check that it worked**.
///
/// `AdjustTokenPrivileges` is documented to return success when it could not
/// assign everything asked for; the only way to know is `GetLastError` reporting
/// `ERROR_NOT_ALL_ASSIGNED`. Skipping that check is how a shutdown agent
/// discovers it never had the privilege at the one moment it needed it.
fn enable_shutdown_privilege() -> Result<(), String> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID,
    };
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let name: Vec<u16> = "SeShutdownPrivilege".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut luid: LUID = std::mem::zeroed();
        if LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) == 0 {
            return Err(format!("LookupPrivilegeValue: {}", std::io::Error::last_os_error()));
        }

        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return Err(format!("OpenProcessToken: {}", std::io::Error::last_os_error()));
        }

        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let ok = AdjustTokenPrivileges(
            token,
            0,
            &tp,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        // Read it before CloseHandle, which would overwrite it.
        let err = GetLastError();
        CloseHandle(token);

        if ok == 0 {
            return Err(format!("AdjustTokenPrivileges: {}", std::io::Error::from_raw_os_error(err as i32)));
        }
        if err == ERROR_NOT_ALL_ASSIGNED {
            return Err("this account does not hold SeShutdownPrivilege".into());
        }
    }
    Ok(())
}

/// Ask Windows to shut down in `grace_s` seconds.
///
/// `bForceAppsClosed = true`, stated rather than defaulted. False can wait
/// indefinitely on one application's unsaved-changes prompt, and an indefinite
/// wait here means the battery decides when the machine goes down instead. The
/// warning period before this point is what makes that a fair trade: someone at
/// the machine has already been told and given time.
fn begin_os_shutdown(grace_s: u32) -> Result<(), std::io::Error> {
    use windows_sys::Win32::System::Shutdown::{
        InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_POWER,
        SHTDN_REASON_MINOR_ENVIRONMENT,
    };

    let mut msg: Vec<u16> = "The UPS is running low on battery."
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        InitiateSystemShutdownExW(
            std::ptr::null(),      // this machine
            msg.as_mut_ptr(),      // shown by Windows during the grace period
            grace_s,
            1,                     // bForceAppsClosed
            0,                     // bRebootAfterShutdown
            SHTDN_REASON_MAJOR_POWER | SHTDN_REASON_MINOR_ENVIRONMENT | SHTDN_REASON_FLAG_PLANNED,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn abort_os_shutdown() -> Result<(), std::io::Error> {
    use windows_sys::Win32::System::Shutdown::AbortSystemShutdownW;
    if unsafe { AbortSystemShutdownW(std::ptr::null()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cutoff must cover the grace period *and* the shutdown, or the UPS
    /// cuts power while Windows is still writing.
    #[test]
    fn the_cutoff_covers_both_halves() {
        for os in [30u32, 120, 600] {
            let cutoff = (OS_GRACE_S + os).min(i16::MAX as u32) as i16;
            assert!(cutoff as u32 > os, "cutoff {cutoff} does not outlast the shutdown");
            assert_eq!(cutoff as u32, OS_GRACE_S + os);
        }
    }

    /// A configuration that would overflow the register must clamp rather than
    /// wrap. Wrapping produces a small positive delay, which is the one outcome
    /// that cuts power to a machine mid-shutdown.
    #[test]
    fn an_absurd_timeout_clamps_instead_of_wrapping() {
        let cutoff = (OS_GRACE_S + u32::MAX / 2).min(i16::MAX as u32) as i16;
        assert_eq!(cutoff, i16::MAX);
        assert!(cutoff > 0);
    }

    #[test]
    fn the_intent_record_round_trips_and_clears() {
        let dir = std::env::temp_dir().join("jdups-intent-test");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read_intent(&dir).is_none());
        write_intent(&dir, 130).unwrap();
        assert!(read_intent(&dir).is_some());
        clear_intent(&dir).unwrap();
        assert!(read_intent(&dir).is_none());
        // Clearing twice is not an error: unwind calls it on paths where it may
        // never have been written.
        clear_intent(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
