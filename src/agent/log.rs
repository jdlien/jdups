//! The agent's own log file.
//!
//! Deliberately not the sampler's CSV. That file has one writer by design, a
//! fixed schema, and a row every five minutes; this one is prose, written at
//! irregular times, by a different process running as a different user. Merging
//! them would mean interleaved writers on a file whose value depends on being
//! reconstructable months later.
//!
//! Plain text rather than CSV because nothing here is a series to be plotted.
//! It is an account of what the agent decided, meant to be read by a person
//! after something happened.

use std::io::Write;
use std::path::{Path, PathBuf};

use jdups::logfile::LocalTime;

use crate::journal::Level;

/// `jdups-agent-YYYY-MM.log`, alongside the sampler's CSV.
///
/// Monthly for the same reason the CSV is: a file that grows without bound is
/// one nobody opens. The name cannot collide with `jdups-YYYY-MM.csv`, so the
/// tray's "Open log" still finds only the CSV.
pub fn month_file(at: &LocalTime) -> String {
    format!("jdups-agent-{:04}-{:02}.log", at.year, at.month)
}

/// Append one line. Opened and flushed per write, so the file can be read,
/// moved or deleted without stopping the agent.
pub fn append(dir: &Path, at: &LocalTime, level: Level, msg: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(month_file(at));
    let fresh = !path.exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    if fresh {
        writeln!(f, "# jdups agent log v1")?;
    }
    writeln!(f, "{}  {}  {msg}", at.iso8601(), level.tag())?;
    f.flush()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_cannot_be_mistaken_for_the_samplers_csv() {
        let at = jdups::logfile::now_local();
        let name = month_file(&at);
        assert!(name.starts_with("jdups-agent-"));
        assert!(name.ends_with(".log"));
        assert!(
            !jdups::logfile::is_log_name(&name),
            "the tray's Open log would hand the shell {name}"
        );
    }
}
