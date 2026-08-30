//! CalDAV/CardDAV protocol codecs — shared by the cosmix-maild DAV server
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
