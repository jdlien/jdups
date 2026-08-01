//! jdups-agent — decides whether this machine should shut down, and says so.
//!
//! **It cannot shut anything down yet.** `armed = true` is refused at startup,
//! on purpose: the decision, the config boundary and the log are one change, and
//! the shutdown transaction is another, with its own testing ladder. Shipping
//! them together would mean the first time the transaction runs is also the
//! first time the whole thing has ever run.
//!
//! What it is for right now is the cheapest evidence available: point it at the
//! real UPS, leave it for weeks, and read what it *would* have done. Thresholds
//! chosen on the bench are guesses. Thresholds chosen from a month of this
//! machine's own power are not.
//!
//! A console binary, like `jdups.exe` and for the same reason: the exit code and
//! stderr are the whole interface while this is being debugged. It becomes a
//! service in the next phase, which is what buys preshutdown notification and
//! knowing when the machine suspends. Until then it is something you run.

mod journal;
mod log;
mod watch;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const USAGE: &str = "\
jdups-agent - decides whether this machine should shut down

USAGE:
    jdups-agent [--dir PATH] [--serial SERIAL] [--config PATH] [-q]
    jdups-agent --check
    jdups-agent --print-config

    --config PATH    Settings file (default: jdups.conf beside this binary)
    --dir PATH       Where to write the agent log (default %ProgramData%\\jdups)
    --serial SERIAL  Select a specific unit when more than one is attached
    -q               Do not echo to stdout; write only to the log

    --check          Validate the config, print what it resolves to, and exit
    --print-config   Print a commented default config file to stdout

It runs in dry run: it decides, it logs, and it does not act. Arming is
refused until the shutdown transaction exists. See docs/implementation-plan.md,
Phase 8.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir: Option<std::path::PathBuf> = None;
    let mut config: Option<std::path::PathBuf> = None;
    let mut serial: Option<String> = None;
    let mut echo = true;
    let mut mode = "run";

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                dir = args.get(i + 1).map(std::path::PathBuf::from);
                i += 1;
            }
            "--config" => {
                config = args.get(i + 1).map(std::path::PathBuf::from);
                i += 1;
            }
            "--serial" => {
                serial = args.get(i + 1).cloned();
                i += 1;
            }
            "-q" | "--quiet" => echo = false,
            "--check" => mode = "check",
            "--print-config" => mode = "print-config",
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("jdups-agent: unrecognised argument {other:?}\n");
                print!("{USAGE}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if mode == "print-config" {
        print!("{}", jdups::config::TEMPLATE);
        return;
    }

    // --- the config, which is a privilege boundary -------------------------
    let path = config
        .or_else(jdups::config::default_path)
        .unwrap_or_else(|| std::path::PathBuf::from("jdups.conf"));
    let settings = match jdups::config::load(&path) {
        Ok(s) => s,
        Err(e) => {
            // No fallback to defaults. An agent whose thresholds do not match
            // the file in front of you is an agent you cannot reason about.
            eprintln!("jdups-agent: bad configuration, refusing to start\n{e}");
            std::process::exit(2);
        }
    };

    if mode == "check" {
        println!("config   {}", path.display());
        if !path.exists() {
            println!("         (no such file, so these are the built-in defaults)");
        }
        for line in settings.describe() {
            println!("  {line}");
        }
        println!(
            "\nmode     {}",
            if settings.armed {
                "ARMED, which this build refuses; see --help"
            } else {
                "dry run: decides and logs, does not act"
            }
        );
        return;
    }

    // --- arming is refused, loudly ----------------------------------------
    // Failing closed and saying why, rather than quietly running dry when the
    // file says armed. Somebody who wrote `armed = true` believes their machine
    // is protected, and letting them keep believing that is the worst outcome
    // available here.
    if settings.armed {
        eprintln!("jdups-agent: {} says armed = true, and this build cannot act on it.", path.display());
        eprintln!("             The shutdown transaction is not implemented yet, so an armed");
        eprintln!("             agent would be a false sense of protection. Set armed = false");
        eprintln!("             to run in dry run. Keep PowerChute armed either way.");
        std::process::exit(2);
    }

    // --- singleton ---------------------------------------------------------
    // Two agents deciding independently is the interlock failure the plan warns
    // about: with the transaction in place they would both write the UPS
    // countdown, and the last writer would win.
    if !claim_singleton() {
        eprintln!("jdups-agent: an agent is already running; not starting a second one");
        std::process::exit(1);
    }

    let opts = watch::Options {
        settings,
        dir: dir.unwrap_or_else(jdups::logfile::default_dir),
        serial,
        echo,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let _ = ctrl_c_handler(move || flag.store(true, Ordering::SeqCst));

    std::process::exit(watch::run(opts, stop));
}

/// `Global\`, not `Local\`: as a service this runs in a different session from
/// anything a logged-in user starts, and a session-local name would not collide
/// with itself.
fn claim_singleton() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = r"Global\jdups-agent-singleton"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    // The handle is deliberately leaked: it must live as long as the process,
    // and the process owning it until exit is the whole mechanism.
    !h.is_null() && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS
}

fn ctrl_c_handler<F: Fn() + Send + 'static>(f: F) -> Result<(), ()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    static mut HOOK: Option<Box<dyn Fn() + Send>> = None;
    unsafe extern "system" fn handler(_: u32) -> windows_sys::core::BOOL {
        #[allow(static_mut_refs)]
        if let Some(f) = unsafe { HOOK.as_ref() } {
            f();
        }
        1
    }
    unsafe {
        HOOK = Some(Box::new(f));
        if SetConsoleCtrlHandler(Some(handler), 1) == 0 {
            return Err(());
        }
    }
    Ok(())
}
