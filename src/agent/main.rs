//! jdups-agent — decides whether this machine should shut down, and says so.
//!
//! **Dry run unless told otherwise.** `armed = false` is the default and what a
//! missing config means, so no accident of packaging can produce an agent that
//! acts. In that mode it decides, logs what it would have done, and touches
//! nothing -- which is the cheapest evidence available for choosing thresholds,
//! since ones picked on a bench are guesses and ones picked from this machine's
//! own power are not.
//!
//! Armed, it runs the transaction in `shutdown.rs`. **PowerChute must be
//! disarmed first**: both write the same UPS countdown register and the last
//! writer wins.
//!
//! A console binary, like `jdups.exe` and for the same reason: the exit code and
//! stderr are the whole interface while this is being debugged. It becomes a
//! service in the next phase, which is what buys preshutdown notification and
//! knowing when the machine suspends. Until then it is something you run.

mod journal;
mod log;
mod service;
mod shutdown;
mod watch;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const USAGE: &str = "\
jdups-agent - decides whether this machine should shut down

USAGE:
    jdups-agent [--dir PATH] [--serial SERIAL] [--config PATH] [-q]
    jdups-agent --check
    jdups-agent --print-config

    --config PATH    Settings file (default: %ProgramData%\\jdups\\jdups.conf)
    --dir PATH       Where to write the agent log (default %ProgramData%\\jdups)
    --serial SERIAL  Select a specific unit when more than one is attached
    -q               Do not echo to stdout; write only to the log

    --service        Run under the Service Control Manager. Only meaningful when
                     the SCM starts it; see install.ps1 -Service
    --check          Validate the config, print what it resolves to, and exit
    --print-config   Print a commented default config file to stdout

It runs in dry run unless the config says `armed = true`. Armed, it can shut
this machine down, and PowerChute must be disarmed first: both write the same
UPS countdown register. See docs/implementation-plan.md, Phase 8.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir: Option<std::path::PathBuf> = None;
    let mut config: Option<std::path::PathBuf> = None;
    let mut serial: Option<String> = None;
    let mut echo = true;
    let mut mode = "run";

    // A flag missing its value is fatal. `--dir` alone silently logging to the
    // default directory is an agent doing something other than what it was
    // told, in a binary where that difference can be a shutdown.
    let value_of = |args: &[String], i: usize, what: &str| -> String {
        args.get(i + 1).cloned().unwrap_or_else(|| {
            eprintln!("jdups-agent: {} needs a value", what);
            std::process::exit(2);
        })
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                dir = Some(std::path::PathBuf::from(value_of(&args, i, "--dir")));
                i += 1;
            }
            "--config" => {
                config = Some(std::path::PathBuf::from(value_of(&args, i, "--config")));
                i += 1;
            }
            "--serial" => {
                serial = Some(value_of(&args, i, "--serial"));
                i += 1;
            }
            "-q" | "--quiet" => echo = false,
            "--service" => mode = "service",
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
            println!("         looked in, in order:");
            for p in jdups::config::search_paths() {
                println!("           {}", p.display());
            }
        }
        for line in settings.describe() {
            println!("  {line}");
        }
        println!(
            "\nmode     {}",
            if settings.armed {
                "ARMED: it will shut this machine down. Disarm PowerChute first."
            } else {
                "dry run: decides and logs, does not act"
            }
        );
        return;
    }

    // --- arming: allowed now, and said out loud ---------------------------
    // This used to refuse, because the transaction did not exist and an agent
    // that quietly ran dry while its config said armed would be the worst
    // outcome available: somebody believing their machine is protected when it
    // is not. It exists now, so this warns instead.
    //
    // The interlock cannot be checked from here, so it is stated. PowerChute
    // and jdups both write the same UPS countdown register, and the last writer
    // wins -- two armed agents can extend a countdown Windows is already
    // shutting down against, or cancel one the other is relying on. Whether
    // PowerChute is *armed* is a setting inside its own configuration, not
    // something visible from outside, so no amount of service-poking would
    // answer it honestly.
    if settings.armed {
        eprintln!("jdups-agent: ARMED. This agent can shut this machine down.");
        eprintln!("             PowerChute must be set to \"Do not shut down in the event of");
        eprintln!("             a power outage\" first. Both write the same UPS countdown and");
        eprintln!("             the last writer wins.");
    }

    // --- singleton ---------------------------------------------------------
    // Two agents deciding independently is the interlock failure the plan warns
    // about: with the transaction in place they would both write the UPS
    // countdown, and the last writer would win.
    if !claim_singleton() {
        eprintln!("jdups-agent: an agent is already running; not starting a second one");
        std::process::exit(1);
    }

    let dir = dir.unwrap_or_else(jdups::logfile::default_dir);

    // --- as a service ------------------------------------------------------
    // The SCM calls back on its own thread with nothing passed in, so the
    // settings are stashed first and the callback picks them up.
    if mode == "service" {
        OPTS.set((settings, dir, serial)).ok();
        service::set_body(|stop, wake| {
            let (settings, dir, serial) = OPTS.get().expect("options set before dispatch").clone();
            watch::run(
                watch::Options { settings, dir, serial, echo: false, wake: Some(wake) },
                stop,
            )
        });
        if let Err(e) = service::dispatch() {
            eprintln!("jdups-agent: not started by the service manager ({e}).");
            eprintln!("             Run it without --service to use it from a console,");
            eprintln!(r"             or install it with: .\install.ps1 -Agent -Service");
            std::process::exit(2);
        }
        return;
    }

    let opts = watch::Options {
        settings,
        dir,
        serial,
        echo,
        // A console process and a scheduled task are both untold about suspend
        // and resume. Only the SCM delivers that.
        wake: None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let _ = ctrl_c_handler(move || flag.store(true, Ordering::SeqCst));

    std::process::exit(watch::run(opts, stop));
}

type Stashed = (jdups::config::Settings, std::path::PathBuf, Option<String>);
static OPTS: std::sync::OnceLock<Stashed> = std::sync::OnceLock::new();

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
