//! JSCalendar (RFC 8984) → iCalendar VEVENT (RFC 5545) emit.
//!
//! v1 maps the core fields needed for client interop: UID, SUMMARY
//! (`title`), DTSTART/DTEND (the indexed UTC instants — no VTIMEZONE in
//! v1), DESCRIPTION, LOCATION, LAST-MODIFIED. Recurrence / alarms /
//! participants are a later refinement; the JSCalendar payload is the
//! authority and is preserved verbatim in storage regardless.

use chrono::{DateTime, Utc};
use icalendar::{
    Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, Event, EventLike,
};

/// Key under which a DAV PUT stashes the client's **verbatim** iCalendar
/// in the stored JSCalendar `data`. GET/REPORT return it unchanged for a
/// lossless round-trip (alarms, RRULE, X-props the v1 mapper doesn't
/// model survive); JSCalendar objects created via JMAP have no such key
/// and are emitted from their fields.
pub const RAW_ICAL_KEY: &str = "cosmix:rawICalendar";

/// Build a single-VEVENT `VCALENDAR` string from an event's indexed
/// fields plus its JSCalendar `data` (for DESCRIPTION / LOCATION). If the
/// `data` carries a verbatim iCal under [`RAW_ICAL_KEY`] (a DAV-written
/// object), that is returned unchanged.
pub fn event_to_ics(
    uid: &str,
    title: Option<&str>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    updated: Option<DateTime<Utc>>,
    data: &serde_json::Value,
) -> String {
    if let Some(raw) = data.get(RAW_ICAL_KEY).and_then(|v| v.as_str()) {
        return raw.to_string();
    }
    let mut ev = Event::new();
    ev.uid(uid);
    // DTSTAMP is REQUIRED on a VEVENT (RFC 5545 §3.6.1); `icalendar`
    // does not set it for us, and clients (Apple) reject events without
    // it. It MUST be a pure function of the stored inputs — using
    // `Utc::now()` would emit a different DTSTAMP on every read while the
    // (input-derived) ETag stayed constant, breaking the ETag⇔body
    // invariant. Resolve deterministically: last-modified → start → a
    // fixed epoch sentinel.
    let dtstamp = updated
        .or(start)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is valid"));
    ev.timestamp(dtstamp);
    if let Some(t) = title {
        ev.summary(t);
    }
    if let Some(s) = start {
        ev.starts(s);
    }
    if let Some(e) = end {
        ev.ends(e);
    }
    if let Some(u) = updated {
        ev.last_modified(u);
    }
    if let Some(d) = data.get("description").and_then(|v| v.as_str()) {
        ev.description(d);
    }
    if let Some(loc) = first_location(data) {
        ev.location(&loc);
    }
    let event = ev.done();

    let mut cal = Calendar::new();
    cal.push(event);
    cal.name("Cosmix");
    cal.done().to_string()
}

/// The fields a DAV PUT extracts from an incoming iCalendar VEVENT: the
/// indexed columns for querying, plus a JSCalendar `data` object that
/// carries the **verbatim** iCal under [`RAW_ICAL_KEY`] for a lossless
/// round-trip.
pub struct ParsedEvent {
    pub uid: Option<String>,
    pub title: Option<String>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub data: serde_json::Value,
}

/// Convert an icalendar date-or-time to a UTC instant. A floating or
/// tz-qualified local time is treated as UTC for the *indexed* column
/// (v1 — no VTIMEZONE resolution); the verbatim body preserves the true
/// value for the client.
fn to_utc(dt: &DatePerhapsTime) -> Option<DateTime<Utc>> {
    match dt {
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(d)) => Some(*d),
        DatePerhapsTime::DateTime(CalendarDateTime::Floating(n)) => Some(n.and_utc()),
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone { date_time, .. }) => {
            Some(date_time.and_utc())
        }
        DatePerhapsTime::Date(d) => d.and_hms_opt(0, 0, 0).map(|n| n.and_utc()),
    }
}

/// Parse an incoming iCalendar string, extracting the first VEVENT's
/// indexed fields and building a stored JSCalendar `data` that stashes
/// the verbatim input under [`RAW_ICAL_KEY`]. Errors if the body has no
/// VEVENT.
pub fn parse_ics(ics: &str) -> anyhow::Result<ParsedEvent> {
    let cal: Calendar = ics
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid iCalendar: {e}"))?;
    let event = cal
        .components
        .iter()
        .find_map(|c| match c {
            CalendarComponent::Event(e) => Some(e),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("no VEVENT in iCalendar body"))?;

    let uid = event.get_uid().map(|s| s.to_string());
    let title = event.get_summary().map(|s| s.to_string());
    let start = event.get_start().as_ref().and_then(to_utc);
    let end = event.get_end().as_ref().and_then(to_utc);

    let mut data = serde_json::json!({ "@type": "Event" });
    if let Some(u) = &uid {
        data["uid"] = serde_json::Value::String(u.clone());
    }
    if let Some(t) = &title {
        data["title"] = serde_json::Value::String(t.clone());
    }
    if let Some(d) = event.get_description() {
        data["description"] = serde_json::Value::String(d.to_string());
    }
    // Stash the verbatim body for a lossless GET round-trip.
    data[RAW_ICAL_KEY] = serde_json::Value::String(ics.to_string());

    Ok(ParsedEvent {
        uid,
        title,
        start,
        end,
        data,
    })
}

/// Patch `SUMMARY` / `DTSTART` / `DTEND` of an existing iCalendar VEVENT in
/// place, **preserving every other line** (RRULE, VALARM, X-props). Used
/// when a structured (JMAP) edit touches an event carrying a verbatim iCal
/// ([`RAW_ICAL_KEY`]). DTSTART/DTEND are rewritten as basic UTC
/// (`YYYYMMDDTHHMMSSZ`), dropping any TZID/VALUE params — adequate for
/// timed events (all-day/recurring exceptions are a later refinement).
pub fn patch_ics(
    raw: &str,
    summary: Option<&str>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> String {
    let fmt = |d: DateTime<Utc>| d.format("%Y%m%dT%H%M%SZ").to_string();
    let mut out: Vec<String> = Vec::new();
    let (mut set_sum, mut set_start, mut set_end) = (false, false, false);
    // Unfold first (RFC 5545 §3.1): a CRLF + space/tab is a continuation —
    // matching physical lines without unfolding would orphan continuations.
    let unfolded = raw
        .replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "");
    // Only patch SUMMARY/DTSTART/DTEND *inside* the VEVENT — a VTIMEZONE
    // (common before the VEVENT) carries its own DTSTART that must not be
    // touched. `set_*` flags also ensure we patch only the first VEVENT.
    let mut in_vevent = false;
    for line in unfolded.lines() {
        let l = line.trim_end_matches('\r');
        if l.eq_ignore_ascii_case("BEGIN:VEVENT") {
            in_vevent = true;
            out.push(l.to_string());
            continue;
        }
        if l.eq_ignore_ascii_case("END:VEVENT") {
            // Insert any requested property the VEVENT lacked, before it
            // closes (so a DAV GET reflects the edit even when absent).
            if in_vevent {
                if let Some(s) = summary
                    && !set_sum
                {
                    out.push(format!("SUMMARY:{}", esc_text(s)));
                    set_sum = true;
                }
                if let Some(d) = start
                    && !set_start
                {
                    out.push(format!("DTSTART:{}", fmt(d)));
                    set_start = true;
                }
                if let Some(d) = end
                    && !set_end
                {
                    out.push(format!("DTEND:{}", fmt(d)));
                    set_end = true;
                }
            }
            in_vevent = false;
            out.push(l.to_string());
            continue;
        }
        let prop = l
            .split([';', ':'])
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        match prop.as_str() {
            "SUMMARY" if in_vevent && summary.is_some() && !set_sum => {
                out.push(format!("SUMMARY:{}", esc_text(summary.unwrap())));
                set_sum = true;
            }
            "DTSTART" if in_vevent && start.is_some() && !set_start => {
                out.push(format!("DTSTART:{}", fmt(start.unwrap())));
                set_start = true;
            }
            "DTEND" if in_vevent && end.is_some() && !set_end => {
                out.push(format!("DTEND:{}", fmt(end.unwrap())));
                set_end = true;
            }
            _ => out.push(l.to_string()),
        }
    }
    let mut s = out.join("\r\n");
    s.push_str("\r\n");
    s
}

/// Escape iCalendar TEXT values (RFC 5545 §3.3.11): backslash, semicolon,
/// comma, and newline.
fn esc_text(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            ';' => o.push_str("\\;"),
            ',' => o.push_str("\\,"),
            '\n' => o.push_str("\\n"),
            '\r' => {}
            _ => o.push(c),
        }
    }
    o
}

/// Extract a display location from JSCalendar `locations` — a map of
/// id → `{ name, ... }`. Returns the first location's `name`.
fn first_location(data: &serde_json::Value) -> Option<String> {
    data.get("locations")
        .and_then(|v| v.as_object())
        .and_then(|m| m.values().next())
        .and_then(|loc| loc.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn emits_valid_vevent() {
        let start = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 9, 11, 0, 0).unwrap();
        let data = serde_json::json!({
            "description": "Sprint sync",
            "locations": { "1": { "name": "Room A" } }
        });
        let ics = event_to_ics(
            "evt-1",
            Some("Standup"),
            Some(start),
            Some(end),
            None,
            &data,
        );
        assert!(ics.contains("BEGIN:VCALENDAR"));
        assert!(ics.contains("BEGIN:VEVENT"));
        assert!(ics.contains("UID:evt-1"));
        assert!(ics.contains("SUMMARY:Standup"));
        assert!(ics.contains("DESCRIPTION:Sprint sync"));
        assert!(ics.contains("LOCATION:Room A"));
        assert!(ics.contains("DTSTART:20260609T100000Z"));
        assert!(ics.contains("DTSTAMP:")); // RFC 5545 required
        assert!(ics.contains("END:VCALENDAR"));
    }

    #[test]
    fn parse_extracts_fields_and_stashes_raw() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc-123\r\n\
                   SUMMARY:Lunch\r\nDTSTART:20260609T120000Z\r\nDTEND:20260609T130000Z\r\n\
                   DESCRIPTION:Catch up\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let p = parse_ics(ics).unwrap();
        assert_eq!(p.uid.as_deref(), Some("abc-123"));
        assert_eq!(p.title.as_deref(), Some("Lunch"));
        assert_eq!(p.data["description"], "Catch up");
        // Verbatim body stashed → GET round-trips losslessly.
        assert_eq!(p.data[RAW_ICAL_KEY], ics);
    }

    #[test]
    fn raw_stash_round_trips_through_emit() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:S\r\n\
                   DTSTART:20260609T120000Z\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let p = parse_ics(ics).unwrap();
        // event_to_ics must return the verbatim body (incl. the RRULE the
        // v1 mapper doesn't model), not a re-emitted projection.
        let out = event_to_ics("x", Some("S"), None, None, None, &p.data);
        assert_eq!(out, ics);
        assert!(out.contains("RRULE:FREQ=WEEKLY"));
    }

    #[test]
    fn parse_rejects_non_event() {
        assert!(parse_ics("not an ical").is_err());
    }

    #[test]
    fn patch_ics_updates_and_preserves() {
        let start = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        // SUMMARY is folded across two physical lines; RRULE must survive.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:x\r\n\
                   SUMMARY:Old\r\n  Title\r\nDTSTART:20260615T100000Z\r\nRRULE:FREQ=WEEKLY\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = patch_ics(ics, Some("New, Title"), Some(start), None);
        assert!(out.contains("SUMMARY:New\\, Title\r\n")); // updated + escaped comma
        assert!(!out.contains("Old")); // old folded summary gone, no orphan
        assert!(out.contains("DTSTART:20260701T090000Z\r\n")); // start changed
        assert!(out.contains("RRULE:FREQ=WEEKLY\r\n")); // preserved
        assert!(out.contains("UID:x\r\n"));
    }

    #[test]
    fn patch_ics_only_touches_vevent_not_vtimezone() {
        let start = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
                   BEGIN:VTIMEZONE\r\nTZID:X\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\n\
                   END:STANDARD\r\nEND:VTIMEZONE\r\n\
                   BEGIN:VEVENT\r\nUID:z\r\nDTSTART:20260615T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = patch_ics(ics, None, Some(start), None);
        assert!(out.contains("DTSTART:19701025T030000\r\n")); // VTIMEZONE untouched
        assert!(out.contains("DTSTART:20260701T090000Z\r\n")); // VEVENT updated
    }

    #[test]
    fn patch_ics_adds_missing_before_end() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:y\r\n\
                   DTSTART:20260615T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let out = patch_ics(ics, Some("Added"), None, None);
        assert!(out.contains("SUMMARY:Added\r\n")); // inserted (was absent)
    }
}
