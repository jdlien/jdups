//! jdups — the console readout.
//!
//! A console binary, deliberately: this is the surface every later phase gets
//! debugged through, and it has to hand the shell real stdout and a real exit
//! code. The tray is `jdups-tray.exe`, built from the same lib.

mod sample;

use std::io::Write;
use std::time::{Duration, Instant};

use jdups::decode::{self, report, PresentStatus};
use jdups::hid::{self, raw, Ups};

const USAGE: &str = "\
jdups - a readout for an APC Back-UPS

USAGE:
    jdups [--once] [--watch [SECONDS]] [--probe] [--list] [--serial SERIAL]
    jdups --sample [--interval SECONDS] [--dir PATH] [-v]

    --once           Print the current readout and exit (default)
    --watch [SECS]   Stream decoded input reports (default: until Ctrl-C)
    --probe          Dump the report descriptor: every value and button cap
    --list           List every HID collection present, and which one we'd pick
    --serial SERIAL  Select a specific unit when more than one is attached

    --sample         Log to CSV continuously. This is the headless logger the
                     scheduled task runs; it is the only writer of the log.
    --interval SECS  Seconds per row (default 300)
    --dir PATH       Where to write (default %ProgramData%\\jdups)
    -v               Echo each row as it is written

The notification-area icon is a separate binary: jdups-tray.exe
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode = "once";
    let mut serial: Option<String> = None;
    let mut watch_secs: Option<u64> = None;
    let mut interval: Option<u64> = None;
    let mut dir: Option<std::path::PathBuf> = None;
    let mut verbose = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--once" => mode = "once",
            "--sample" => mode = "sample",
            "-v" | "--verbose" => verbose = true,
            "--interval" => {
                interval = args.get(i + 1).and_then(|v| v.parse::<u64>().ok());
                i += 1;
            }
            "--dir" => {
                dir = args.get(i + 1).map(std::path::PathBuf::from);
                i += 1;
            }
            "--probe" => mode = "probe",
            "--list" => mode = "list",
            "--watch" => {
                mode = "watch";
                if let Some(next) = args.get(i + 1) {
                    if let Ok(n) = next.parse::<u64>() {
                        watch_secs = Some(n);
                        i += 1;
                    }
                }
            }
            "--serial" => {
                serial = args.get(i + 1).cloned();
                i += 1;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("jdups: unrecognised argument {other:?}\n");
                print!("{USAGE}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let code = match mode {
        "list" => cmd_list(),
        "sample" => cmd_sample(serial, interval, dir, verbose),
        _ => run(mode, serial.as_deref(), watch_secs),
    };
    std::process::exit(code);
}

/// The headless logger.
///
/// A singleton, because two of these appending to one file would interleave
/// rows and there would be no way to tell afterwards. `Global\` rather than
/// `Local\`: the scheduled task runs as SYSTEM in a different session from
/// the tray, and a session-local name would not collide with itself.
fn cmd_sample(
    serial: Option<String>,
    interval: Option<u64>,
    dir: Option<std::path::PathBuf>,
    verbose: bool,
) -> i32 {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = r"Global\jdups-sampler-singleton"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if h.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        eprintln!("jdups: a sampler is already running; not starting a second one");
        return 1;
    }

    let opts = sample::Options {
        interval: Duration::from_secs(interval.unwrap_or(jdups::logfile::DEFAULT_INTERVAL_S)),
        dir: dir.unwrap_or_else(jdups::logfile::default_dir),
        serial,
        verbose,
    };
    println!(
        "sampling every {}s into {}",
        opts.interval.as_secs(),
        opts.dir.display()
    );

    // Ctrl-C writes a final row rather than dropping the window on the floor.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&stop);
    let _ = ctrl_c_handler(move || flag.store(true, std::sync::atomic::Ordering::SeqCst));

    sample::run(opts, stop)
}

/// Minimal Ctrl-C hook. The scheduled task stops us with a terminate, but a
/// human running this by hand should get a clean last row.
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

fn cmd_list() -> i32 {
    let all = raw::enumerate();
    println!("{} HID collection(s) present\n", all.len());
    for d in &all {
        let ours = d.vid == raw::VID_APC
            && d.pid == raw::PID_BACKUPS
            && d.usage_page == raw::USAGE_PAGE_POWER
            && d.usage == raw::USAGE_UPS;
        println!(
            "{} {:04X}:{:04X}  usage {:#04X}:{:#04X}  in={} feat={}  serial={}",
            if ours { "->" } else { "  " },
            d.vid,
            d.pid,
            d.usage_page,
            d.usage,
            d.input_len,
            d.feature_len,
            d.serial_display(),
        );
    }
    match raw::find_ups(None) {
        Ok(d) => {
            println!("\nselected: {:04X}:{:04X} serial {}", d.vid, d.pid, d.serial_display());
            0
        }
        Err(e) => {
            println!("\nno UPS selected: {e}");
            1
        }
    }
}

fn run(mode: &str, serial: Option<&str>, watch_secs: Option<u64>) -> i32 {
    let dev = match hid::open(serial) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("jdups: {e}");
            eprintln!("       (try `jdups --list` to see what is attached)");
            return 1;
        }
    };

    // Recorded, not assumed. An access-0 handle serves feature reports but
    // cannot read the input stream, and saying so beats parking forever.
    if !dev.access.can_read_input() {
        eprintln!(
            "jdups: warning: opened with access {:?}; input reports unavailable",
            dev.access
        );
    }

    match mode {
        "probe" => cmd_probe(&dev),
        "watch" => cmd_watch(&dev, watch_secs),
        _ => cmd_once(&dev),
    }
}

fn cmd_once(dev: &raw::Device) -> i32 {
    let r = dev.read_all();
    print!("{}", r.summary());

    if let Some(t) = r.last_transfer {
        println!("Last transfer      {t}");
    }
    if let Some(d) = r.shutdown_delay {
        println!(
            "Shutdown delay     {}",
            if d < 0 { "none scheduled".to_string() } else { format!("{d} s") }
        );
    }

    if r.have_status {
        println!("\nPresentStatus      {}", flags(&r.status));
    }
    println!(
        "\ndevice             {:04X}:{:04X} serial {} (access {:?})",
        dev.info.vid,
        dev.info.pid,
        dev.info.serial_display(),
        dev.access
    );

    if r.charge.is_none() {
        eprintln!("\njdups: could not read charge. Is the UPS still attached?");
        return 1;
    }
    0
}

fn flags(s: &PresentStatus) -> String {
    let mut on = Vec::new();
    for (name, set) in [
        ("ACPresent", s.ac_present),
        ("BatteryPresent", s.battery_present),
        ("Charging", s.charging),
        ("Discharging", s.discharging),
        ("ShutdownImminent", s.shutdown_imminent),
        ("BelowCapacityLimit", s.below_remaining_capacity_limit),
        ("TimeLimitExpired", s.remaining_time_limit_expired),
        ("Overload", s.overload),
        ("CommunicationLost", s.communication_lost),
        ("VoltageNotRegulated", s.voltage_not_regulated),
    ] {
        if set {
            on.push(name);
        }
    }
    if on.is_empty() {
        "(none set)".to_string()
    } else {
        on.join(", ")
    }
}

/// Stream the input reports, decoded, one line each.
///
/// This is how the plug-pull experiment in Phase 1.5 gets read: run it into a
/// file, pull the plug for thirty seconds, and the transitions are all here.
fn cmd_watch(dev: &raw::Device, secs: Option<u64>) -> i32 {
    let deadline = secs.map(|s| Instant::now() + Duration::from_secs(s));
    match deadline {
        Some(_) => println!("watching for {}s (Ctrl-C to stop)\n", secs.unwrap()),
        None => println!("watching (Ctrl-C to stop)\n"),
    }
    let start = Instant::now();
    let mut count = 0u64;
    let mut last_sweep = Instant::now() - Duration::from_secs(10);

    loop {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }

        // Battery and input voltage are feature-only — they never appear on the
        // input stream — so a capture built from input reports alone cannot
        // answer the question the plug-pull exists to answer: which voltage
        // report is which once the unit is actually on battery.
        if last_sweep.elapsed() >= Duration::from_secs(2) {
            last_sweep = Instant::now();
            let f = |id: u8| dev.feature(id).ok();
            let bv = f(report::BATTERY_VOLTS)
                .as_deref()
                .and_then(decode::battery_volts);
            let iv = f(report::INPUT_VOLTS)
                .as_deref()
                .and_then(decode::input_volts);
            let ld = f(report::LOAD).as_deref().and_then(decode::load_pct);
            let xfer = f(report::LAST_TRANSFER)
                .as_deref()
                .and_then(decode::last_transfer);
            println!(
                "{:7.2}s  [sweep]              battery {}  input {}  load {}  last transfer {}",
                start.elapsed().as_secs_f32(),
                bv.map(|v| format!("{v:.2} V")).unwrap_or_else(|| "?".into()),
                iv.map(|v| format!("{v} V")).unwrap_or_else(|| "?".into()),
                ld.map(|v| format!("{v}%")).unwrap_or_else(|| "?".into()),
                xfer.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            );
            let _ = std::io::stdout().flush();
        }

        match dev.input(1_000) {
            Ok(None) => continue, // timeout; just loop
            Err(e) => {
                eprintln!("jdups: read failed: {e}");
                return 1;
            }
            Ok(Some(buf)) => {
                count += 1;
                let t = start.elapsed().as_secs_f32();
                let hex: Vec<String> = buf.iter().map(|b| format!("{b:02X}")).collect();
                let id = buf.first().copied().unwrap_or(0);
                let decoded = describe_input(dev, &buf);
                println!("{t:7.2}s  [{}]  {}", hex.join(" "), decoded);
                let _ = std::io::stdout().flush();
                let _ = id;
            }
        }
    }
    println!("\n{count} reports in {:.1}s", start.elapsed().as_secs_f32());
    0
}

/// Dispatch on `buf[0]`. The stream is multiplexed, and decoding report 6 as
/// though it were report 12 yields a confident, wrong 0 % charge.
fn describe_input(dev: &raw::Device, buf: &[u8]) -> String {
    match buf.first().copied() {
        Some(report::CHARGE_RUNTIME) => {
            let c = decode::charge(buf);
            let r = decode::runtime_s(buf);
            match (c, r) {
                (Some(c), Some(r)) => format!("charge/runtime  {c}%  {r}s ({} min)", (r as u32 + 30) / 60),
                _ => "charge/runtime  (undecodable)".into(),
            }
        }
        Some(report::CHARGE_STATE) => match decode::charge_state(buf) {
            Some((ch, dis)) => format!("charge state    charging={ch} discharging={dis}"),
            None => "charge state    (undecodable)".into(),
        },
        Some(report::AC_PRESENT) => match decode::ac_present(buf) {
            Some(v) => format!("ACPresent       {v}"),
            None => "ACPresent       (undecodable)".into(),
        },
        Some(report::PRESENT_STATUS) => {
            format!("PresentStatus   {}", flags(&dev.status_of(buf, true)))
        }
        Some(report::SHUTDOWN_IMMINENT) => {
            format!("shutdown/limit  {}", flags(&dev.status_of(buf, true)))
        }
        Some(report::TEST) => "test".into(),
        Some(id) => format!("report {id}       (unmapped)"),
        None => "(empty)".into(),
    }
}

/// Dump the descriptor.
///
/// Walks value **and** button caps: a value-only walk misses `PresentStatus`
/// entirely, which is how report 22 stayed unidentified through the original
/// investigation.
fn cmd_probe(dev: &raw::Device) -> i32 {
    println!(
        "device   {:04X}:{:04X}  usage {:#04X}:{:#04X}  input={}B feature={}B  access={:?}",
        dev.info.vid,
        dev.info.pid,
        dev.info.usage_page,
        dev.info.usage,
        dev.info.input_len,
        dev.info.feature_len,
        dev.access,
    );
    println!("serial   {}\n", dev.info.serial_display());

    for (label, caps) in jdups::hid::caps::walk(dev.preparsed()) {
        println!("=== {label}: {} caps ===", caps.len());
        println!("rpt  page:usage  kind   bits  logical range        name");
        for c in &caps {
            println!(
                "{:3}  {:04X}:{:02X}    {:6} {:4}  {:>8} .. {:<8}  {}",
                c.report_id,
                c.usage_page,
                c.usage,
                if c.is_button { "button" } else { "value" },
                c.bit_size,
                c.logical_min,
                c.logical_max,
                c.name(),
            );
        }
        println!();
    }

    // The bounded IOCTL path is the one the tray and the agent will use, so it
    // is worth proving it returns the same bytes as the blocking API rather
    // than assuming the CTL_CODE method bits are right.
    println!("=== which CTL_CODE encoding does this driver accept? ===");
    let truth = match dev.get_feature_blocking(report::CHARGE_RUNTIME) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("jdups: even HidD_GetFeature failed ({e}); giving up");
            return 1;
        }
    };
    for (name, ioctl) in raw::FEATURE_IOCTL_CANDIDATES {
        let verdict = match dev.get_feature_via(ioctl, report::CHARGE_RUNTIME, hid::FEATURE_TIMEOUT_MS) {
            Ok(b) if b == truth => "OK, matches HidD_GetFeature".to_string(),
            Ok(b) => format!("returned {b:02X?}, expected {truth:02X?}"),
            Err(e) => format!("{e}"),
        };
        println!(
            "  {name:18} {ioctl:#010X}  {}{verdict}",
            if ioctl == raw::IOCTL_HID_GET_FEATURE { "<- in use  " } else { "           " }
        );
    }

    println!("\n=== bounded feature I/O cross-check ===");
    println!("IOCTL_HID_GET_FEATURE = {:#010X}", raw::IOCTL_HID_GET_FEATURE);
    let mut agree = 0;
    let mut differ = 0;
    for id in [
        report::CHARGE_RUNTIME,
        report::LOAD,
        report::RATED_POWER,
        report::INPUT_VOLTS,
        report::BATTERY_VOLTS,
        report::MANUFACTURE_DATE,
        report::PRESENT_STATUS,
        report::AC_PRESENT,
    ] {
        let fast = dev.get_feature(id, hid::FEATURE_TIMEOUT_MS);
        let slow = dev.get_feature_blocking(id);
        match (&fast, &slow) {
            (Ok(a), Ok(b)) if a == b => agree += 1,
            (Ok(a), Ok(b)) => {
                differ += 1;
                println!("  report {id:3}: MISMATCH ioctl={a:02X?} hidd={b:02X?}");
            }
            (Err(e), Ok(b)) => {
                differ += 1;
                println!("  report {id:3}: ioctl failed ({e}); hidd={b:02X?}");
            }
            (_, Err(e)) => println!("  report {id:3}: blocking read failed ({e})"),
        }
    }
    println!("  {agree} agree, {differ} differ");
    if differ > 0 {
        eprintln!("\njdups: bounded feature I/O disagrees with HidD_GetFeature; do not");
        eprintln!("       rely on it until the CTL_CODE is settled.");
        return 1;
    }
    0
}
