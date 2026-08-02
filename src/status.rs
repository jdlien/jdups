//! What the agent publishes and the tray reads.
//!
//! These two processes cannot talk to each other by any of the usual means. The
//! agent runs as SYSTEM in **session 0**, which has no window station and no
//! desktop: it cannot show a notification, cannot post a window message into
//! your session, and must not be given the ability to. The tray runs as you,
//! unprivileged, and is the only half that can put anything on screen.
//!
//! So the channel is a file, and the direction of trust runs one way. It lives
//! in `%ProgramData%\jdups`, which the installer ACLs to SYSTEM/Administrators
//! write and Users read — so the tray can read it and nothing running as you can
//! forge it. A SYSTEM process that took instructions from a user-writable file
//! would be the same privilege bug the log directory exists to avoid.
//!
//! Deliberately the same plain-text shape as the config: greppable, readable in
//! Notepad, and diagnosable when the interesting thing has already happened.

use std::path::{Path, PathBuf};

/// What the agent is currently doing about the power.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Mains, or nothing known. Nothing owed.
    Idle,
    /// On battery, above the thresholds.
    OnBattery,
    /// The decision has been made and the grace period is running.
    Pending,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::OnBattery => "onbattery",
            Phase::Pending => "pending",
        }
    }
    fn parse(s: &str) -> Option<Phase> {
        match s {
            "idle" => Some(Phase::Idle),
            "onbattery" => Some(Phase::OnBattery),
            "pending" => Some(Phase::Pending),
            _ => None,
        }
    }
}

/// Something the tray should tell the user about, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    None,
    /// A shutdown is coming. This is the one that has to reach a human in time
    /// for them to save their work.
    Pending,
    /// ...and it is not coming after all.
    Cancelled,
    /// It is happening now.
    Shutdown,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::None => "none",
            Event::Pending => "pending",
            Event::Cancelled => "cancelled",
            Event::Shutdown => "shutdown",
        }
    }
    fn parse(s: &str) -> Option<Event> {
        match s {
            "none" => Some(Event::None),
            "pending" => Some(Event::Pending),
            "cancelled" => Some(Event::Cancelled),
            "shutdown" => Some(Event::Shutdown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Local time this was written, for a human reading the file directly.
    pub updated: String,
    /// False while the agent is only deciding and logging.
    pub armed: bool,
    pub phase: Phase,
    /// Seconds until the shutdown, while `phase` is `Pending`.
    pub seconds_left: Option<u64>,
    /// Why, in the words `policy::Why` uses.
    pub reason: Option<String>,
    /// Bumped whenever `event` is set to something worth announcing.
    ///
    /// The tray remembers the last one it acted on. Without a sequence number
    /// it would have to infer "this is new" from the contents, and a shutdown
    /// warning that fires twice — or, far worse, not at all because it looked
    /// like the previous one — is exactly what must not happen here.
    pub seq: u64,
    pub event: Event,
}

impl Default for Status {
    fn default() -> Status {
        Status {
            updated: String::new(),
            armed: false,
            phase: Phase::Idle,
            seconds_left: None,
            reason: None,
            seq: 0,
            event: Event::None,
        }
    }
}

impl Status {
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        s.push_str("# jdups agent status. Written by the agent, read by the tray.\n");
        s.push_str("version = 1\n");
        s.push_str(&format!("updated = {}\n", self.updated));
        s.push_str(&format!("armed = {}\n", self.armed));
        s.push_str(&format!("phase = {}\n", self.phase.as_str()));
        s.push_str(&format!("seq = {}\n", self.seq));
        s.push_str(&format!("event = {}\n", self.event.as_str()));
        if let Some(n) = self.seconds_left {
            s.push_str(&format!("seconds_left = {n}\n"));
        }
        if let Some(r) = &self.reason {
            s.push_str(&format!("reason = {r}\n"));
        }
        // The terminator, and it is load-bearing. See `parse`.
        s.push_str("end\n");
        s
    }

    /// Parse a **complete** record, or nothing.
    ///
    /// The `end` line is what makes it complete, and it is not ceremony. A file
    /// truncated mid-write parses perfectly well as something plausible and
    /// wrong: `seconds_left = 60` cut after one digit reads as **6**, which is a
    /// shutdown warning that arrives ten times too late to be any use. There is
    /// a test that walks every truncation point and it found exactly that.
    ///
    /// `write` renames a fully written temporary into place, so a torn read
    /// should be impossible anyway. This is the belt to that pair of braces,
    /// because the cost of being wrong is a machine going down unannounced.
    ///
    /// Within a complete record, unknown fields are ignored rather than fatal.
    /// The tray and the agent are separate binaries and can be different
    /// versions mid-upgrade; a tray that refused to read a newer file would
    /// disable the warning without saying so.
    pub fn parse(text: &str) -> Option<Status> {
        let mut st = Status::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            // **`end` ends it.** Parsing used to continue past the terminator, so
            // a complete record followed by a partial one -- an interrupted
            // rename, two writes into the same file -- parsed as a hybrid, with
            // the trailing fragment overriding fields of the record that had
            // already been declared complete.
            if line == "end" {
                return Some(st);
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            // **A known field this version cannot parse fails the record.**
            // Defaulting it looked safe and was not: a corrupted `event` became
            // `None` while the sequence still advanced, so the tray recorded
            // that sequence as seen and then ignored the corrected publication
            // behind it -- dropping a shutdown warning in silence.
            match k {
                "updated" => st.updated = v.to_string(),
                "armed" => {
                    st.armed = match v {
                        "true" => true,
                        "false" => false,
                        _ => return None,
                    }
                }
                "phase" => st.phase = Phase::parse(v)?,
                "seq" => st.seq = v.parse().ok()?,
                "event" => st.event = Event::parse(v)?,
                "seconds_left" => st.seconds_left = Some(v.parse().ok()?),
                "reason" => st.reason = Some(v.to_string()),
                // Unknown keys stay ignored: the two binaries can be different
                // versions mid-upgrade, and a tray that refused a newer file
                // would disable the warning without saying so.
                _ => {}
            }
        }
        // No terminator: not a record.
        None
    }
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("agent-status.txt")
}

/// Write it where the tray will find it.
///
/// Written to a temporary file and renamed, so a reader never sees a half
/// written record. The tray polls this on a timer and a torn read would show a
/// countdown of zero, or a phase without its reason.
pub fn write(dir: &Path, st: &Status) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let final_path = path_in(dir);
    let tmp = dir.join("agent-status.tmp");
    std::fs::write(&tmp, st.to_text())?;
    std::fs::rename(&tmp, &final_path)
}

/// Read it from wherever it is, or `None`.
///
/// Looks in both candidate directories for the same reason the tray's log
/// lookup does: it has no way to know which install shape is in use.
pub fn read() -> Option<Status> {
    for dir in crate::logfile::candidate_dirs() {
        if let Ok(text) = std::fs::read_to_string(path_in(&dir)) {
            if let Some(st) = Status::parse(&text) {
                return Some(st);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Status {
        Status {
            updated: "2026-08-01T12:49:31-06:00".into(),
            armed: true,
            phase: Phase::Pending,
            seconds_left: Some(60),
            reason: Some("predicted runtime below the threshold".into()),
            seq: 7,
            event: Event::Pending,
        }
    }

    #[test]
    fn it_round_trips() {
        assert_eq!(Status::parse(&sample().to_text()), Some(sample()));
    }

    #[test]
    fn the_default_round_trips_too() {
        let d = Status::default();
        assert_eq!(Status::parse(&d.to_text()), Some(d));
    }

    /// The two binaries can be different versions mid-upgrade. A tray that
    /// refused to parse a newer file would disable the shutdown warning without
    /// saying anything, which is the worst way for this to fail.
    #[test]
    fn an_unknown_field_is_ignored_not_fatal() {
        let text = sample().to_text().replace("end\n", "something_new = 42\nend\n");
        assert_eq!(Status::parse(&text), Some(sample()));
    }

    /// Garbage is not a status. Failing toward "no idea" rather than toward a
    /// plausible-looking idle record keeps a corrupt file from reading as a
    /// healthy one.
    #[test]
    fn nonsense_is_not_a_status() {
        for text in ["", "\0\0\0", "phase = wat\nevent = wat\nseq = wat"] {
            assert_eq!(Status::parse(text), None, "{text:?}");
        }
    }

    /// The one that matters, and it found a real bug: without a terminator,
    /// `seconds_left = 60` truncated after one digit parses as 6 -- a shutdown
    /// warning that arrives ten times too late to be any use. Every truncation
    /// point must be rejected outright.
    #[test]
    fn a_truncated_record_is_never_a_different_record() {
        let full = sample().to_text();
        for n in 0..=full.len() {
            // Either it is not a status at all, or it is exactly the one that
            // was written. What must never happen is a third thing that looks
            // plausible: `seconds_left = 60` cut after one digit reading as 6.
            if let Some(st) = Status::parse(&full[..n]) {
                assert_eq!(st, sample(), "a partial file at byte {n} parsed as something else");
            }
        }
        assert!(Status::parse(&full).is_some(), "the whole thing must still parse");
        // And the sentinel is doing the work: without it, nothing parses.
        assert_eq!(Status::parse(&full.replace("end\n", "")), None);
    }

    #[test]
    fn writing_is_atomic_enough_to_read_back() {
        let dir = std::env::temp_dir().join("jdups-status-test");
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, &sample()).unwrap();
        let text = std::fs::read_to_string(path_in(&dir)).unwrap();
        assert_eq!(Status::parse(&text), Some(sample()));
        // The temporary must not be left lying about next to the real one.
        assert!(!dir.join("agent-status.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A known field this version cannot parse must fail the whole record.
    /// Defaulting it advanced the sequence while losing the event, so the tray
    /// marked that sequence seen and then ignored the corrected publication --
    /// dropping a shutdown warning in silence.
    #[test]
    fn a_corrupt_known_field_rejects_the_record() {
        for bad in ["phase = wat", "event = wat", "seq = wat", "seconds_left = wat"] {
            let text = sample().to_text().replace("end
", &format!("{bad}
end
"));
            assert_eq!(Status::parse(&text), None, "accepted {bad:?}");
        }
    }

    /// `end` ends it. Two records in one file -- an interrupted rename, a double
    /// write -- must not merge, with the fragment overriding the complete one.
    #[test]
    fn parsing_stops_at_the_terminator() {
        let text = format!("{}seq = 999
phase = idle
", sample().to_text());
        assert_eq!(Status::parse(&text), Some(sample()), "read past the terminator");
    }
}
