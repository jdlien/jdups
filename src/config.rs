//! The agent's configuration file.
//!
//! Separate from `policy` on purpose. `policy` says what the numbers *mean*;
//! this says how a file on disk is allowed to become those numbers, and that is
//! a privilege boundary rather than a parsing convenience. The agent runs as
//! SYSTEM and the only thing standing between "a config file" and "shutdown as a
//! service" is this module refusing bad input.
//!
//! Three rules, each of which exists because the lenient version is dangerous:
//!
//! 1. **A malformed file is a hard error, never a fallback to defaults.** An
//!    agent that silently ignores the thresholds you wrote and uses its own is
//!    an agent whose behaviour you cannot predict from the file in front of you.
//! 2. **An unknown key is an error.** `runtime_threshhold_s` misspelt would
//!    otherwise be accepted in silence while the real threshold stayed at its
//!    default, and you would not find out until an outage.
//! 3. **A missing file means dry run.** Arming requires a file that explicitly
//!    says so, so no accident of packaging or deployment can produce an armed
//!    agent.

use std::path::{Path, PathBuf};

use crate::policy;

/// Everything the agent reads from disk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// May this agent actually shut the machine down?
    ///
    /// Defaults to false and has to be turned on deliberately. Dry run is not a
    /// testing mode you opt into; it is what the agent does unless told
    /// otherwise.
    pub armed: bool,
    pub policy: policy::Config,
}

// Written out rather than derived, which clippy would prefer. `armed` is the
// one default in this codebase where the value has to be visible in the source
// next to the word "default", not inferred from the type.
#[allow(clippy::derivable_impls)]
impl Default for Settings {
    fn default() -> Settings {
        Settings { armed: false, policy: policy::Config::default() }
    }
}

impl Settings {
    /// The effective settings, one per line, for the agent to write into its log
    /// at startup.
    ///
    /// A dry-run log that records decisions without recording the thresholds
    /// that produced them cannot be interpreted six weeks later.
    pub fn describe(&self) -> Vec<String> {
        let p = &self.policy;
        vec![
            format!("armed = {}", self.armed),
            format!("runtime_threshold_s = {}", p.runtime_threshold_s),
            format!("charge_floor_pct = {}", p.charge_floor_pct),
            format!("debounce_s = {}", p.debounce_s),
            format!("settle_s = {}", p.settle_s),
            format!("max_on_battery_s = {}", p.max_on_battery_s),
            format!("stale_after_s = {}", p.stale_after_s),
            format!("warn_before_s = {}", p.warn_before_s),
        ]
    }
}

/// Everywhere a config might live, in the order they win.
///
/// `%ProgramData%\jdups` first, which is where a machine-wide install puts it.
/// That is the conventional home for machine-wide mutable configuration on
/// Windows — `%ProgramFiles%` is for files an installer writes once and nothing
/// edits afterwards, and this is a file meant to be edited.
///
/// **The privilege boundary survives the move**, which is the only reason it is
/// allowed. `%ProgramData%` inherits an ACL that lets ordinary users create
/// files, and a SYSTEM process taking thresholds from a user-writable path is
/// shutdown-as-a-service. The installer does not inherit it: it strips
/// inheritance from `%ProgramData%\jdups` and sets SYSTEM and Administrators
/// full, Users read. That directory is therefore exactly as safe as Program
/// Files was, and **the installer is what makes it so** — this is the second
/// thing depending on that ACL, after the log.
///
/// `%LOCALAPPDATA%` second, for a `-PerUser` install. No boundary is crossed
/// there: the agent runs as the same account that can write it. Note that a
/// SYSTEM agent resolves this to the *system* profile, not to anyone's real one.
///
/// Beside the binary last, so a tree run out of `target\release` can carry its
/// own config without touching the installed one.
pub fn search_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = crate::logfile::candidate_dirs()
        .into_iter()
        .map(|d| d.join("jdups.conf"))
        .collect();
    if let Some(beside) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("jdups.conf"))) {
        v.push(beside);
    }
    v
}

/// The config that will actually be read.
///
/// The first that exists, or the first candidate if none do — so `--check` can
/// name the file it looked for rather than saying nothing.
pub fn default_path() -> Option<PathBuf> {
    let paths = search_paths();
    paths
        .iter()
        .find(|p| p.exists())
        .cloned()
        .or_else(|| paths.into_iter().next())
}

/// Read and validate, or explain why not.
///
/// A missing file is not an error: it means defaults, which means dry run.
/// Anything else that goes wrong is an error, because the alternative is an
/// agent whose thresholds do not match the file you are looking at.
pub fn load(path: &Path) -> Result<Settings, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    parse(&text).map_err(|errs| {
        let mut s = format!("{}:\n", path.display());
        for e in errs {
            s.push_str("  ");
            s.push_str(&e);
            s.push('\n');
        }
        s.trim_end().to_string()
    })
}

/// Parse the file's text, reporting **every** problem rather than the first.
///
/// Someone editing thresholds is usually editing several, and a parser that
/// stops at the first mistake turns one round trip into four.
pub fn parse(text: &str) -> Result<Settings, Vec<String>> {
    let mut s = Settings::default();
    let mut errs = Vec::new();
    let mut seen: Vec<&str> = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            errs.push(format!("line {line_no}: expected `key = value`, got `{line}`"));
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if seen.contains(&key) {
            errs.push(format!("line {line_no}: `{key}` is set more than once"));
            continue;
        }
        seen.push(key);

        // Each arm parses into its field. `u64`/`u16`/`u8` are doing real work
        // here: an out-of-range value fails as a parse error with the line
        // number attached, before `validate` ever sees it.
        let r: Result<(), String> = match key {
            "armed" => flag(value).map(|v| s.armed = v),
            "runtime_threshold_s" => num(value).map(|v| s.policy.runtime_threshold_s = v),
            "charge_floor_pct" => num(value).map(|v| s.policy.charge_floor_pct = v),
            "debounce_s" => num(value).map(|v| s.policy.debounce_s = v),
            "settle_s" => num(value).map(|v| s.policy.settle_s = v),
            "max_on_battery_s" => num(value).map(|v| s.policy.max_on_battery_s = v),
            "stale_after_s" => num(value).map(|v| s.policy.stale_after_s = v),
            "warn_before_s" => num(value).map(|v| s.policy.warn_before_s = v),
            other => Err(format!("unknown setting `{other}`")),
        };
        if let Err(e) = r {
            errs.push(format!("line {line_no}: {e}"));
        }
    }

    // Range validation last, so it runs against the whole file rather than
    // complaining about a field a later line was about to fix.
    if let Err(e) = s.policy.validate() {
        errs.push(e.to_string());
    }

    if errs.is_empty() {
        Ok(s)
    } else {
        Err(errs)
    }
}

fn flag(v: &str) -> Result<bool, String> {
    match v {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        other => Err(format!("expected true or false, got `{other}`")),
    }
}

fn num<T: std::str::FromStr>(v: &str) -> Result<T, String> {
    v.parse::<T>().map_err(|_| {
        format!("`{v}` is not a whole number in range for this setting")
    })
}

/// The file the installer writes, and the documentation of last resort.
///
/// Every value is the default and every line is commented out, so the shipped
/// file is inert: uncommenting is the act of deciding something.
pub const TEMPLATE: &str = r#"# jdups agent configuration.
#
# The agent reads this at startup only. Restart it after editing.
# Every setting below is shown at its default value.

# May the agent actually shut this machine down? With this false -- which is
# also what a missing file means -- it decides, logs what it would have done,
# and does nothing. Do not turn this on until the shutdown has been proven end
# to end on a machine you are willing to lose.
# armed = false

# Shut down at or below this much predicted runtime, in seconds. This is the
# number that matters: it folds in the current load, which a percentage does
# not. Size it from a *measured* worst-case shutdown of this machine, with the
# hibernation file write included, and then leave margin.
# runtime_threshold_s = 300

# ...or at or below this charge, whichever comes first. A backstop for a
# runtime estimate that has stopped being believable, not the primary trigger.
# charge_floor_pct = 25

# How long a qualifying condition must hold before it counts. The runtime
# estimate jitters either side of any threshold at a steady load, so a single
# sample crossing the line means nothing.
# debounce_s = 20

# Ignore the thresholds for this long after mains is lost. This device's charge
# estimate drops about twenty points within seconds of a transfer and takes
# hours to come back, so the numbers are least trustworthy in exactly the window
# the agent cares about most.
# settle_s = 30

# Shut down after this long on battery no matter what the numbers say. The
# backstop for a device that has stopped telling the truth.
# max_on_battery_s = 1800

# A reading older than this is not evidence of anything.
# stale_after_s = 30

# Warn for this long before actually shutting down, so anyone at the machine
# has a chance to save their work. The tray shows a notification; nothing has
# to be clicked and nothing can delay the shutdown. Set it to 0 on a machine
# nobody sits at: the seconds come out of the runtime budget either way, so a
# warning with no one to reach is runtime spent for nothing.
# warn_before_s = 60
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(parse("").unwrap(), Settings::default());
        assert_eq!(parse("\n\n   \n").unwrap(), Settings::default());
    }

    /// The shipped file must parse, and must produce exactly the defaults it
    /// claims to document. This is the test that catches the template drifting
    /// away from the code after a threshold changes.
    #[test]
    fn the_template_is_inert_and_honest() {
        let s = parse(TEMPLATE).expect("the shipped template does not parse");
        assert_eq!(s, Settings::default());

        // ...and every documented default matches the real one. Uncomment each
        // line and check the file still describes the same agent.
        let live: String = TEMPLATE
            .lines()
            .filter_map(|l| l.trim().strip_prefix("# "))
            .filter(|l| l.contains('='))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(parse(&live).unwrap(), Settings::default(), "template documents stale defaults");
    }

    #[test]
    fn comments_and_whitespace_are_ignored() {
        let s = parse("  # a comment\n\n settle_s = 45 # trailing\n").unwrap();
        assert_eq!(s.policy.settle_s, 45);
    }

    #[test]
    fn every_setting_round_trips() {
        let text = parse("")
            .unwrap()
            .describe()
            .join("\n");
        assert_eq!(parse(&text).unwrap(), Settings::default(), "describe() is not parseable");
    }

    /// The typo case. Silently keeping the default while the file appears to say
    /// otherwise is the failure that would only surface during an outage.
    #[test]
    fn an_unknown_key_is_refused() {
        let e = parse("runtime_threshhold_s = 600").unwrap_err();
        assert!(e[0].contains("unknown setting"), "{e:?}");
    }

    #[test]
    fn a_repeated_key_is_refused() {
        let e = parse("settle_s = 30\nsettle_s = 60").unwrap_err();
        assert!(e[0].contains("more than once"), "{e:?}");
    }

    /// A value too large for its field must fail as a parse error rather than
    /// wrapping into something small and plausible.
    #[test]
    fn an_out_of_range_number_does_not_wrap() {
        let e = parse("charge_floor_pct = 300").unwrap_err();
        assert!(e[0].contains("not a whole number"), "{e:?}");
    }

    /// A file cannot talk its way past `policy::Config::validate`.
    #[test]
    fn a_dangerous_value_is_refused_even_though_it_parses() {
        let e = parse("runtime_threshold_s = 5").unwrap_err();
        assert!(e[0].contains("no time to shut down"), "{e:?}");
    }

    #[test]
    fn all_the_errors_are_reported_at_once() {
        let e = parse("nonsense\nbogus_key = 1\nsettle_s = x").unwrap_err();
        assert_eq!(e.len(), 3, "{e:?}");
        assert!(e[0].starts_with("line 1"), "{e:?}");
        assert!(e[1].starts_with("line 2"), "{e:?}");
        assert!(e[2].starts_with("line 3"), "{e:?}");
    }

    #[test]
    fn arming_takes_several_spellings_but_only_deliberate_ones() {
        for yes in ["true", "yes", "1"] {
            assert!(parse(&format!("armed = {yes}")).unwrap().armed);
        }
        for no in ["false", "no", "0"] {
            assert!(!parse(&format!("armed = {no}")).unwrap().armed);
        }
        assert!(parse("armed = maybe").is_err());
    }

    /// Nothing about a default install, a missing file, or an empty one may
    /// produce an armed agent.
    #[test]
    fn nothing_accidental_can_arm_the_agent() {
        assert!(!Settings::default().armed);
        assert!(!parse("").unwrap().armed);
        assert!(!parse(TEMPLATE).unwrap().armed);
        let missing = std::path::Path::new(r"C:\jdups-no-such-file-exists.conf");
        assert!(!load(missing).unwrap().armed);
    }
}
