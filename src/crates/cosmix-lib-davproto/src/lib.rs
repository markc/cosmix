//! CalDAV/CardDAV protocol codecs — shared by the maild DAV server
//! and the future DAV client (`_plan/2026-06-09-maild-dav-server.md` §10.2).
//!
//! v1 scope:
//! * **emit** JSCalendar (RFC 8984) → iCalendar VEVENT (RFC 5545) and
//!   JSContact (RFC 9553) → vCard 4.0 (RFC 6350) — the read path (M2).
//! * **strong ETags** — a content hash so a resource's ETag is stable
//!   across reads and changes iff its content changes.
//!
//! The reverse direction (iCal/vCard → JSCalendar/JSContact, the write
//! path) lands with maild DAV M3.

pub mod etag;
pub mod ical;
pub mod vcard;

/// Reject retired raw representation keys, even when their value is null or
/// the replacement key is also present. Call before reading or changing data.
pub fn validate_raw_keys(data: &serde_json::Value) -> anyhow::Result<()> {
    for (retired, current) in [
        (ical::RETIRED_ICAL_KEY, ical::RAW_ICAL_KEY),
        (vcard::RETIRED_VCARD_KEY, vcard::RAW_VCARD_KEY),
    ] {
        if data.get(retired).is_some() {
            anyhow::bail!(
                "retired wire key {retired:?} present: migrate the stored object to {current:?}"
            );
        }
    }
    Ok(())
}
