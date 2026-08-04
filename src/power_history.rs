//! Power-source history, read from the log Windows already keeps.
//!
//! The sampler records what the UPS *measured*; nothing else has those numbers.
//! What it cannot record is the short stuff: with the input stream permanently
//! dead, a transition is only noticed when a sweep happens to see it, and five
//! plug-pulls inside ninety seconds produced three CSV rows. Polling the device
//! faster is the obvious fix and the wrong one -- that pressure is what wedged
//! the unit on 2026-08-03.
//!
//! Windows already has the answer. Its battery driver is *pushed* every
//! power-source change and journals one `Kernel-Power` event 105 for each, with
//! a timestamp: the same ninety seconds that gave the sampler three rows gave
//! the System log nine events. So this asks Windows instead of asking the UPS,
//! which costs one log query and no USB traffic at all.
//!
//! **What this is not.** Event 105 is evidence, not a contract: Microsoft
//! documents it only as "Power source change", it depends on Windows
//! recognising the UPS through its battery stack, it is blind while the machine
//! is off or asleep, and the System log rolls over. A quiet week here is not
//! proof of clean power, and the report says so rather than implying otherwise.

/// One power-source change, as Windows recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// ISO-8601 UTC, straight out of the event's `SystemTime`.
    pub at: String,
    /// True when the machine went **to** mains, false when it went to battery.
    pub on_mains: bool,
}

/// An interval between two edges, or one left open at either end of the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub from: String,
    pub on_mains: bool,
    /// `None` when the span runs off the end of the window rather than closing.
    pub seconds: Option<i64>,
}

/// What a human wants to know: how often, how long, how bad.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Transitions **to battery**. The number people mean by "how many outages".
    pub transfers: usize,
    /// Of those, the ones that ended within `BRIEF_S`.
    pub brief: usize,
    pub total_on_battery_s: i64,
    pub longest_on_battery_s: i64,
    pub spans: Vec<Span>,
}

/// Under this, an outage is a blip: long enough to transfer, short enough that
/// the sampler's 30-second sweep would likely never have seen it.
pub const BRIEF_S: i64 = 30;

/// Fold a chronological edge list into spans and totals.
///
/// Pure, so the parsing and the arithmetic are testable apart from the event
/// log. Three rules the review insisted on, each of which is the difference
/// between a table and a plausible lie: consecutive duplicate states are folded
/// rather than counted as transfers, the trailing span is left open rather than
/// given an invented duration, and a leading span is reported as the state the
/// window opened in rather than as an event.
pub fn summarize(edges: &[Edge]) -> Summary {
    let mut s = Summary::default();
    let mut folded: Vec<&Edge> = Vec::new();
    for e in edges {
        // A repeat of the state we are already in is not a transition. Windows
        // can log the same source twice across a race.
        if folded.last().is_some_and(|p| p.on_mains == e.on_mains) {
            continue;
        }
        folded.push(e);
    }

    for (i, e) in folded.iter().enumerate() {
        let seconds = folded.get(i + 1).and_then(|next| gap_seconds(&e.at, &next.at));
        if !e.on_mains {
            s.transfers += 1;
            if let Some(secs) = seconds {
                s.total_on_battery_s += secs;
                s.longest_on_battery_s = s.longest_on_battery_s.max(secs);
                if secs <= BRIEF_S {
                    s.brief += 1;
                }
            }
        }
        s.spans.push(Span { from: e.at.clone(), on_mains: e.on_mains, seconds });
    }
    s
}

/// Seconds between two event timestamps.
///
/// The event log writes `2026-08-03T21:01:39.1234567Z`, always UTC and always
/// this shape, so the fields are read positionally rather than by pulling in a
/// date library for one subtraction. `None` if either fails to parse, which
/// leaves the span open instead of inventing a number.
fn gap_seconds(from: &str, to: &str) -> Option<i64> {
    Some(epoch_seconds(to)? - epoch_seconds(from)?)
}

/// The event log's UTC stamp as local wall time, to the second.
///
/// The CSV is local time with its offset, and a report that mixed the two would
/// have someone comparing 03:01 against 21:01 and concluding the tool was
/// broken. `offset_min` comes from the OS's *current* offset, so a span that
/// crosses a DST change is off by an hour on the far side of it -- acceptable
/// for a few days of history, and the reason the offset is printed rather than
/// implied.
pub fn to_local(utc: &str, offset_min: i32) -> Option<String> {
    let secs = epoch_seconds(utc)? + i64::from(offset_min) * 60;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let (sign, off) = if offset_min < 0 { ('-', -offset_min) } else { ('+', offset_min) };
    Some(format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}{sign}{:02}:{:02}",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60,
        off / 60,
        off % 60
    ))
}

/// The inverse of the day count below, same lineage.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn epoch_seconds(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let n = |a: usize, z: usize| t.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from the civil date, Howard Hinnant's algorithm: exact for any
    // proleptic-Gregorian date and no dependency for something this small.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// Ask the System log for `Kernel-Power` 105s inside the last `days`.
///
/// Rendered to XML and read for two fields: the `SystemTime` attribute and the
/// `AcOnline` data element. Deliberately not the message text -- that is
/// localized -- and deliberately not an assumption that the rows alternate,
/// which they do not.
#[cfg(windows)]
pub fn read(days: u32) -> Result<Vec<Edge>, String> {
    use windows_sys::Win32::System::EventLog::{
        EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection,
    };

    let ms = u64::from(days) * 24 * 3_600 * 1_000;
    // XPath, so the filtering happens in the service rather than here. 105 is
    // the power-source change; TimeCreated bounds the walk.
    let query = format!(
        "*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and (EventID=105) \
         and TimeCreated[timediff(@SystemTime) <= {ms}]]]"
    );
    let channel = wide("System");
    let q = wide(&query);

    let mut edges = Vec::new();
    unsafe {
        let h = EvtQuery(
            0,
            channel.as_ptr(),
            q.as_ptr(),
            EvtQueryChannelPath | EvtQueryReverseDirection,
        );
        if h == 0 {
            return Err(format!(
                "could not query the System event log: {}. \
                 Group Policy can restrict it; otherwise interactive users may read it.",
                std::io::Error::last_os_error()
            ));
        }

        let mut batch = [0isize; 32];
        loop {
            let mut got = 0u32;
            if EvtNext(h, batch.len() as u32, batch.as_mut_ptr(), 2_000, 0, &mut got) == 0 {
                break; // ERROR_NO_MORE_ITEMS, or a timeout; either ends the walk.
            }
            for &ev in batch.iter().take(got as usize) {
                if let Some(xml) = render_xml(ev) {
                    if let Some(e) = parse_event(&xml) {
                        edges.push(e);
                    }
                }
                EvtClose(ev);
            }
            if got == 0 {
                break;
            }
        }
        EvtClose(h);
    }
    // The query walked newest-first so a huge log costs nothing extra; the
    // caller wants chronological.
    edges.reverse();
    Ok(edges)
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Two-call render: size it, then fill it. `bufferused` comes back in **bytes**,
/// which is the trap in this API -- the buffer is `u16`.
#[cfg(windows)]
unsafe fn render_xml(ev: isize) -> Option<String> {
    use windows_sys::Win32::System::EventLog::{EvtRender, EvtRenderEventXml};

    let mut needed = 0u32;
    let mut props = 0u32;
    unsafe {
        EvtRender(0, ev, EvtRenderEventXml, 0, std::ptr::null_mut(), &mut needed, &mut props);
    }
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u16; (needed as usize / 2) + 1];
    let ok = unsafe {
        EvtRender(
            0,
            ev,
            EvtRenderEventXml,
            needed,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut needed,
            &mut props,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Pull the timestamp and the AC state out of one rendered event.
///
/// Kept separate from the Win32 call so the shape of a real event -- captured
/// from this machine -- can be a test vector.
pub fn parse_event(xml: &str) -> Option<Edge> {
    let at = between(xml, "SystemTime='", "'")
        .or_else(|| between(xml, "SystemTime=\"", "\""))?
        .to_string();
    // The field is named in the event's own template, so it is stable and not
    // localized, unlike the rendered message.
    let ac = between(xml, "Name='AcOnline'>", "<")
        .or_else(|| between(xml, "Name=\"AcOnline\">", "<"))?;
    let on_mains = match ac.trim() {
        "true" | "True" | "1" => true,
        "false" | "False" | "0" => false,
        _ => return None,
    };
    Some(Edge { at, on_mains })
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let rest = s.get(i..)?;
    let j = rest.find(end)?;
    rest.get(..j)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(at: &str, on_mains: bool) -> Edge {
        Edge { at: at.into(), on_mains }
    }

    /// The shape of a real event from this machine, trimmed. Rendered XML is
    /// what the parser actually sees, so it is what the test feeds it.
    #[test]
    fn a_real_event_yields_its_time_and_state() {
        let xml = r#"<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>
<System><Provider Name='Microsoft-Windows-Kernel-Power' Guid='{331c3b3a}'/>
<EventID>105</EventID><TimeCreated SystemTime='2026-08-03T21:01:39.1234567Z'/>
</System><EventData><Data Name='AcOnline'>false</Data>
<Data Name='BatteryPresent'>true</Data></EventData></Event>"#;
        assert_eq!(
            parse_event(xml),
            Some(edge("2026-08-03T21:01:39.1234567Z", false))
        );
        // Double-quoted attributes render on some systems; both must work.
        let alt = xml.replace('\'', "\"");
        assert_eq!(parse_event(&alt).map(|e| e.on_mains), Some(false));
        // Anything without both fields is not an edge, rather than a guess.
        assert_eq!(parse_event("<Event/>"), None);
    }

    /// The report sits next to a CSV written in local time, so it has to speak
    /// the same clock. Round-trips against the day-count function, which is
    /// where an off-by-one would hide.
    #[test]
    fn utc_events_render_as_local_wall_time() {
        // 03:01:39Z at -06:00 is the previous evening, which is exactly the
        // confusion this exists to prevent.
        assert_eq!(
            to_local("2026-08-04T03:01:39.5579243Z", -360).as_deref(),
            Some("2026-08-03 21:01:39-06:00")
        );
        // Forward of UTC, and across a month end in the other direction.
        assert_eq!(
            to_local("2026-08-31T23:30:00.0Z", 330).as_deref(),
            Some("2026-09-01 05:00:00+05:30")
        );
        // The shape is checked, the calendar is not: an impossible day
        // normalizes rather than failing. The source is Windows' own event
        // template, so validating February would be guarding against a
        // malformed log rather than against anything that happens.
        assert_eq!(
            to_local("2026-02-29T00:00:00.0Z", 0).as_deref(),
            Some("2026-03-01 00:00:00+00:00")
        );
        assert_eq!(to_local("rubbish", -360), None);
    }

    #[test]
    fn the_day_count_round_trips() {
        for t in [
            "1970-01-01T00:00:00.0Z",
            "2026-08-04T03:01:39.0Z",
            "2028-02-29T12:00:00.0Z",
            "2100-03-01T00:00:00.0Z",
        ] {
            let e = epoch_seconds(t).unwrap();
            let (y, m, d) = civil_from_days(e.div_euclid(86_400));
            assert_eq!(
                format!("{y:04}-{m:02}-{d:02}"),
                t[..10],
                "round trip lost {t}"
            );
        }
    }

    #[test]
    fn timestamps_subtract_across_days_and_months() {
        assert_eq!(gap_seconds("2026-08-03T21:01:39.0Z", "2026-08-03T21:02:26.0Z"), Some(47));
        assert_eq!(gap_seconds("2026-08-31T23:59:59.0Z", "2026-09-01T00:00:09.0Z"), Some(10));
        // A leap day, because February is where date arithmetic goes to die.
        assert_eq!(gap_seconds("2028-02-28T23:59:59.0Z", "2028-02-29T00:00:00.0Z"), Some(1));
        assert_eq!(gap_seconds("nonsense", "2026-08-03T21:02:26.0Z"), None);
    }

    /// The five-pulls-in-ninety-seconds case that started this: brief outages
    /// counted, durations paired, and the trailing span left open rather than
    /// given a duration nobody measured.
    #[test]
    fn the_summary_counts_transfers_and_leaves_the_last_span_open() {
        let s = summarize(&[
            edge("2026-08-03T21:01:39.0Z", false),
            edge("2026-08-03T21:02:26.0Z", true),
            edge("2026-08-03T21:02:42.0Z", false),
            edge("2026-08-03T21:02:52.0Z", true),
        ]);
        assert_eq!(s.transfers, 2);
        assert_eq!(s.brief, 1, "the 10 s outage should read as a blip");
        assert_eq!(s.total_on_battery_s, 47 + 10);
        assert_eq!(s.longest_on_battery_s, 47);
        assert_eq!(s.spans.last().unwrap().seconds, None, "invented a duration");
    }

    /// Windows can log the same source twice. A repeat is not a transfer, and
    /// counting it would inflate every report.
    #[test]
    fn repeated_states_are_folded_not_counted() {
        let s = summarize(&[
            edge("2026-08-03T21:00:00.0Z", false),
            edge("2026-08-03T21:00:05.0Z", false),
            edge("2026-08-03T21:00:20.0Z", true),
        ]);
        assert_eq!(s.transfers, 1);
        assert_eq!(s.total_on_battery_s, 20);
    }

    #[test]
    fn an_empty_log_is_a_summary_of_nothing() {
        let s = summarize(&[]);
        assert_eq!(s, Summary::default());
        assert_eq!(s.transfers, 0);
    }
}
