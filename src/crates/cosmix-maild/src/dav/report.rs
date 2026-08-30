//! REPORT (RFC 4791 §7 / RFC 6352 §8) — multiget + query.
//!
//! v1 handles `calendar-multiget` / `addressbook-multiget` (return the
//! named hrefs), `calendar-query` with a `time-range` filter (RFC 4791
//! §9.9 overlap on the indexed UTC instants; recurring events are
//! conservatively included whenever DTSTART precedes the range end —
//! a correct superset, since instances never occur before DTSTART and
//! server-side recurrence expansion is a later refinement), and
//! `addressbook-query` (whole collection — property filters are treated
//! as "match all", a correct superset). `sync-collection` REPORT is M4.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use uuid::Uuid;

use axum::response::Response;

use crate::db;
use crate::db::calendar::CalendarEvent;
use crate::db::contact::Contact;
use crate::jmap::AppState;

use super::href::{self, Resource};
use super::xml;

const CAL_DATA: &str = "C:calendar-data";
const ADDR_DATA: &str = "CR:address-data";

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    CalendarMultiget,
    CalendarQuery,
    AddressbookMultiget,
    AddressbookQuery,
    /// RFC 6578 sync-collection (valid on either collection type).
    SyncCollection,
    Unknown,
}

struct Parsed {
    kind: Kind,
    hrefs: Vec<String>,
    /// The `<D:sync-token>` element text (sync-collection only); `None`
    /// for an initial sync.
    sync_token: Option<String>,
    /// A `<C:time-range start end>` from a calendar-query filter (RFC
    /// 4791 §9.9; values are UTC basic `YYYYMMDDTHHMMSSZ`). Either side
    /// may be absent (one-sided range). Only the first time-range in the
    /// body is honoured — clients send a single VEVENT comp-filter.
    time_range: Option<TimeRange>,
    /// `true` if the XML parser hit an error before EOF. A body whose
    /// root we recognised but whose remainder is malformed must NOT run
    /// a query — otherwise a truncated `<calendar-query>…` would trigger
    /// a full collection read.
    malformed: bool,
}

/// A CalDAV time-range as `(start, end)`; either side may be `None`
/// (one-sided range).
type TimeRange = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Parse an RFC 4791 UTC basic date-time (`YYYYMMDDTHHMMSSZ`).
fn parse_caldav_utc(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|n| n.and_utc())
}

/// Read the `start` / `end` attributes off a `time-range` element.
fn time_range_attrs(e: &quick_xml::events::BytesStart<'_>) -> TimeRange {
    let mut start = None;
    let mut end = None;
    for attr in e.attributes().flatten() {
        let Ok(val) = attr.unescape_value() else {
            continue;
        };
        match attr.key.as_ref() {
            b"start" => start = parse_caldav_utc(&val),
            b"end" => end = parse_caldav_utc(&val),
            _ => {}
        }
    }
    (start, end)
}

fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.to_string(),
    }
}

fn kind_of(local: &str) -> Kind {
    match local {
        "calendar-multiget" => Kind::CalendarMultiget,
        "calendar-query" => Kind::CalendarQuery,
        "addressbook-multiget" => Kind::AddressbookMultiget,
        "addressbook-query" => Kind::AddressbookQuery,
        "sync-collection" => Kind::SyncCollection,
        _ => Kind::Unknown,
    }
}

/// Parse a REPORT body for its report kind (the root element) and any
/// `<href>` members (multiget). Tolerant: malformed XML yields whatever
/// was read so far rather than erroring.
fn parse(body: &[u8]) -> Parsed {
    let mut reader = Reader::from_reader(body);
    let mut buf = Vec::new();
    let mut kind = Kind::Unknown;
    let mut first = true;
    let mut in_href = false;
    let mut in_token = false;
    let mut cur = String::new();
    let mut tok = String::new();
    let mut hrefs = Vec::new();
    let mut sync_token = None;
    let mut time_range = None;
    let mut malformed = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let ln = local_name(e.name().as_ref());
                if first {
                    kind = kind_of(&ln);
                    first = false;
                }
                match ln.as_str() {
                    "href" => {
                        in_href = true;
                        cur.clear();
                    }
                    "sync-token" => {
                        in_token = true;
                        tok.clear();
                    }
                    "time-range" if time_range.is_none() => {
                        time_range = Some(time_range_attrs(&e));
                    }
                    _ => {}
                }
            }
            // `<C:time-range start=… end=…/>` is self-closing in practice.
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == "time-range" && time_range.is_none() {
                    time_range = Some(time_range_attrs(&e));
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    if in_href {
                        cur.push_str(&s);
                    } else if in_token {
                        tok.push_str(&s);
                    }
                }
            }
            Ok(Event::End(e)) => match local_name(e.name().as_ref()).as_str() {
                "href" => {
                    in_href = false;
                    let h = cur.trim().to_string();
                    if !h.is_empty() {
                        hrefs.push(h);
                    }
                }
                "sync-token" => {
                    in_token = false;
                    sync_token = Some(tok.trim().to_string());
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => {
                // A parser error mid-document: do not trust the partial
                // parse for dispatch (the caller rejects with 400).
                malformed = true;
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    Parsed {
        kind,
        hrefs,
        sync_token,
        time_range,
        malformed,
    }
}

/// Time-range-relevant facts about an event, from its JSCalendar `data`
/// and (for DAV-written objects) the verbatim iCal's **first VEVENT
/// only** — a `VTIMEZONE`'s DST `RRULE`s must not classify the event as
/// recurring.
struct VeventFacts {
    recurring: bool,
    /// `DTSTART;VALUE=DATE` (all-day): date-only, local reckoning, and a
    /// missing `DTEND` means a one-day duration (RFC 5545 §3.6.1).
    all_day: bool,
    /// The VEVENT carries a `DURATION` property — its real end extends
    /// past DTSTART by an amount the v1 index doesn't model.
    has_duration: bool,
    /// `true` only when every date endpoint present is a UTC instant
    /// (`…Z`, no `TZID`). TZID/floating values are indexed local-as-UTC
    /// (`to_utc` v1), so their instants can be off by a UTC offset in
    /// either direction — a UTC DTSTART with a TZID DTEND is still
    /// uncertain.
    utc_certain: bool,
}

/// Does an unfolded uppercase iCal line carry this property? Property
/// names terminate at `:` or `;` — a bare prefix match would also hit
/// X-props sharing the prefix.
fn is_prop(line: &str, name: &str) -> bool {
    line.strip_prefix(name)
        .is_some_and(|rest| rest.starts_with(':') || rest.starts_with(';'))
}

fn vevent_facts(data: &serde_json::Value) -> VeventFacts {
    let mut f = VeventFacts {
        recurring: data.get("recurrenceRules").is_some_and(|v| !v.is_null()),
        all_day: data
            .get("showWithoutTime")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        has_duration: false,
        utc_certain: true,
    };
    let Some(raw) = data
        .get(cosmix_lib_davproto::ical::RAW_ICAL_KEY)
        .and_then(|v| v.as_str())
    else {
        return f;
    };
    // Unfold (RFC 5545 §3.1) so folded property lines classify correctly.
    let unfolded = raw
        .replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "");
    let utc_value = |line: &str| -> bool {
        !line.contains("TZID=") && line.rsplit(':').next().unwrap_or("").trim().ends_with('Z')
    };
    let mut in_vevent = false;
    for line in unfolded.lines() {
        let up = line.trim_end_matches('\r').to_ascii_uppercase();
        if up == "BEGIN:VEVENT" {
            in_vevent = true;
            continue;
        }
        if up == "END:VEVENT" {
            break; // first VEVENT only
        }
        if !in_vevent {
            continue;
        }
        if is_prop(&up, "RRULE") || is_prop(&up, "RDATE") {
            f.recurring = true;
        }
        if is_prop(&up, "DURATION") {
            f.has_duration = true;
        }
        if is_prop(&up, "DTSTART") {
            f.all_day = up.contains("VALUE=DATE") && !up.contains("VALUE=DATE-TIME");
            if f.all_day || !utc_value(&up) {
                f.utc_certain = false;
            }
        }
        if is_prop(&up, "DTEND") && !utc_value(&up) {
            f.utc_certain = false;
        }
    }
    f
}

/// Largest real UTC offset is UTC+14 (Kiribati); an instant indexed
/// local-as-UTC is at most this far from its true UTC value.
const TZ_SLACK: chrono::Duration = chrono::Duration::hours(14);

/// RFC 4791 §9.9 time-range overlap on the indexed UTC instants.
///
/// - No range in the query → everything matches.
/// - Unparseable/missing DTSTART → conservative include (superset).
/// - Recurring events match whenever DTSTART precedes the range end:
///   instances never occur before DTSTART and may extend arbitrarily
///   forward; expansion is a later refinement.
/// - A missing/degenerate DTEND defaults to one day for all-day events
///   (RFC 5545 §3.6.1) and one second otherwise; a `DTSTART`+`DURATION`
///   event (no DTEND indexed in v1) is included whenever DTSTART
///   precedes the range end, since its true extent isn't modelled.
/// - TZID/floating/all-day values are indexed local-as-UTC (v1), so the
///   range edges get ±14h slack for them — a bounded superset instead of
///   a false negative; UTC-certain events compare exactly.
///
/// Known refinements deliberately deferred (scale/fidelity, not
/// correctness for the current payloads): the filter runs after the
/// `MAX_COLLECTION_OBJECTS` fetch (a later-range query on a >cap
/// calendar could miss members — `warn_if_capped` already flags the
/// truncation), and the captured time-range is not validated against
/// its comp-filter context (a VTODO-scoped range would filter VEVENTs;
/// the store holds only VEVENTs in v1).
fn event_in_range(ev: &CalendarEvent, range: &Option<TimeRange>) -> bool {
    let Some((range_start, range_end)) = range else {
        return true;
    };
    let Some(start) = ev.start_dt.as_deref().and_then(super::parse_stored_dt) else {
        return true;
    };
    let facts = vevent_facts(&ev.data);
    let slack = if facts.utc_certain {
        chrono::Duration::zero()
    } else {
        TZ_SLACK
    };
    // Saturating add: a range edge or stored instant near the datetime
    // ceiling must clamp, never panic (range edges are client input).
    let plus = |dt: DateTime<Utc>, d: chrono::Duration| {
        dt.checked_add_signed(d).unwrap_or(DateTime::<Utc>::MAX_UTC)
    };
    let starts_before_range_end = range_end.is_none_or(|re| start < plus(re, slack));
    if facts.recurring {
        return starts_before_range_end;
    }
    let stored_end = ev
        .end_dt
        .as_deref()
        .and_then(super::parse_stored_dt)
        .filter(|e| *e > start);
    // No indexed end but a real DURATION: the true extent is unknown to
    // the index — conservative include (bounded only by the range end).
    if stored_end.is_none() && facts.has_duration {
        return starts_before_range_end;
    }
    let default_dur = if facts.all_day {
        chrono::Duration::days(1)
    } else {
        chrono::Duration::seconds(1)
    };
    let end = stored_end.unwrap_or_else(|| plus(start, default_dur));
    starts_before_range_end && range_start.is_none_or(|rs| plus(end, slack) > rs)
}

fn event_ics_etag(ev: &CalendarEvent) -> (String, String) {
    let parse_dt = super::parse_stored_dt;
    let ics = cosmix_lib_davproto::ical::event_to_ics(
        &ev.uid,
        ev.title.as_deref(),
        ev.start_dt.as_deref().and_then(parse_dt),
        ev.end_dt.as_deref().and_then(parse_dt),
        ev.updated_at.as_deref().and_then(parse_dt),
        &ev.data,
    );
    let etag = cosmix_lib_davproto::etag::for_event(
        &ev.uid,
        ev.title.as_deref(),
        ev.start_dt.as_deref(),
        ev.end_dt.as_deref(),
        ev.updated_at.as_deref(),
        &ev.data,
    );
    (ics, etag)
}

fn contact_vcf_etag(c: &Contact) -> (String, String) {
    let vcf = cosmix_lib_davproto::vcard::contact_to_vcf(
        &c.uid,
        c.full_name.as_deref(),
        c.email.as_deref(),
        c.company.as_deref(),
        &c.data,
    );
    let etag = cosmix_lib_davproto::etag::for_contact(
        &c.uid,
        c.full_name.as_deref(),
        c.email.as_deref(),
        c.company.as_deref(),
        &c.data,
    );
    (vcf, etag)
}

/// Max member changes returned in one sync-collection response. If a
/// client is further behind, it receives a continuation sync-token and
/// re-syncs (RFC 6578 §3.6) until caught up.
const SYNC_BATCH: i64 = 2000;

/// Handle a REPORT against a collection resource.
pub async fn handle(
    state: &Arc<AppState>,
    account: i32,
    resource: &Resource,
    body: &[u8],
) -> Response {
    let parsed = parse(body);
    if parsed.malformed {
        return super::bad_request();
    }
    match (resource, &parsed.kind) {
        (Resource::Calendar { calendar_id, .. }, Kind::SyncCollection) => {
            calendar_sync(state, account, calendar_id, &parsed).await
        }
        (Resource::Calendar { calendar_id, .. }, _) => {
            calendar_report(state, account, calendar_id, parsed).await
        }
        (Resource::Addressbook { book_id, .. }, Kind::SyncCollection) => {
            addressbook_sync(state, account, book_id, &parsed).await
        }
        (Resource::Addressbook { book_id, .. }, _) => {
            addressbook_report(state, account, book_id, parsed).await
        }
        // REPORT is only meaningful on a collection in v1.
        _ => super::not_found(),
    }
}

/// Parse the destroyed/created/updated id lists (strings) into UUIDs,
/// dropping any unparseable id.
fn to_uuids(ids: &[String]) -> Vec<Uuid> {
    ids.iter().filter_map(|s| s.parse::<Uuid>().ok()).collect()
}

async fn calendar_sync(
    state: &Arc<AppState>,
    account: i32,
    calendar_id: &str,
    parsed: &Parsed,
) -> Response {
    let Ok(cal_uuid) = calendar_id.parse::<Uuid>() else {
        return super::not_found();
    };
    // The collection must exist for this account — don't return a 207 for
    // a phantom/foreign collection id.
    match db::calendar::get_by_ids(&state.db.conn, account, &[cal_uuid]).await {
        Ok(v) if !v.is_empty() => {}
        Ok(_) => return super::not_found(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: collection lookup failed");
            return super::server_error();
        }
    }
    // Resolve the client's sync-token → changelog state.
    let Some(since) = super::parse_sync_token(parsed.sync_token.as_deref()) else {
        return super::invalid_sync_token();
    };
    // A non-initial token must be one we actually issued (RFC 6578 §3.7).
    match crate::db::changelog::is_valid_state(&state.db.conn, account, "CalendarEvent", since)
        .await
    {
        Ok(true) => {}
        Ok(false) => return super::invalid_sync_token(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: state validation failed");
            return super::server_error();
        }
    }

    let changes = match crate::db::changelog::changes_since(
        &state.db.conn,
        account,
        "CalendarEvent",
        since,
        SYNC_BATCH,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: changes_since failed");
            return super::server_error();
        }
    };

    // Created/updated → fetch the live rows (account-wide), keep those in
    // THIS calendar, emit getetag members; collect their hrefs so a
    // delete-then-recreate of the same uid doesn't also emit a 404.
    let mut live_ids = to_uuids(&changes.created);
    live_ids.extend(to_uuids(&changes.updated));
    let live = match db::calendar::get_events_by_ids(&state.db.conn, account, &live_ids).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: fetch changed failed");
            return super::server_error();
        }
    };

    let mut out = String::new();
    let mut live_hrefs = std::collections::HashSet::new();
    for ev in &live {
        if ev.calendar_id != calendar_id {
            continue;
        }
        let href = format!("/dav/calendars/{account}/{calendar_id}/{}.ics", ev.uid);
        let (_, etag) = event_ics_etag(ev);
        out.push_str(&xml::object_listing(
            &href,
            &etag,
            "text/calendar; charset=utf-8",
        ));
        live_hrefs.insert(href);
    }

    // Destroyed → tombstone lookup for the href; 404 member unless the
    // href is now live again (delete+recreate of the same uid).
    let destroyed_ids = to_uuids(&changes.destroyed);
    match crate::db::tombstone::lookup(&state.db.conn, account, "CalendarEvent", &destroyed_ids)
        .await
    {
        Ok(tombs) => {
            for t in tombs {
                if t.collection_id != calendar_id {
                    continue;
                }
                let href = format!("/dav/calendars/{account}/{calendar_id}/{}.ics", t.uid);
                if !live_hrefs.contains(&href) {
                    out.push_str(&xml::object_not_found(&href));
                }
            }
        }
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: tombstone lookup failed");
            return super::server_error();
        }
    }

    let token = super::sync_token_string(&changes.new_state);
    xml::sync_multistatus_response(&out, &token)
}

async fn addressbook_sync(
    state: &Arc<AppState>,
    account: i32,
    book_id: &str,
    parsed: &Parsed,
) -> Response {
    let Ok(book_uuid) = book_id.parse::<Uuid>() else {
        return super::not_found();
    };
    match db::contact::get_books_by_ids(&state.db.conn, account, &[book_uuid]).await {
        Ok(v) if !v.is_empty() => {}
        Ok(_) => return super::not_found(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: collection lookup failed");
            return super::server_error();
        }
    }
    let Some(since) = super::parse_sync_token(parsed.sync_token.as_deref()) else {
        return super::invalid_sync_token();
    };
    match crate::db::changelog::is_valid_state(&state.db.conn, account, "Contact", since).await {
        Ok(true) => {}
        Ok(false) => return super::invalid_sync_token(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: state validation failed");
            return super::server_error();
        }
    }

    let changes = match crate::db::changelog::changes_since(
        &state.db.conn,
        account,
        "Contact",
        since,
        SYNC_BATCH,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: changes_since failed");
            return super::server_error();
        }
    };

    let mut live_ids = to_uuids(&changes.created);
    live_ids.extend(to_uuids(&changes.updated));
    let live = match db::contact::get_contacts_by_ids(&state.db.conn, account, &live_ids).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: fetch changed failed");
            return super::server_error();
        }
    };

    let mut out = String::new();
    let mut live_hrefs = std::collections::HashSet::new();
    for c in &live {
        if c.addressbook_id != book_id {
            continue;
        }
        let href = format!("/dav/addressbooks/{account}/{book_id}/{}.vcf", c.uid);
        let (_, etag) = contact_vcf_etag(c);
        out.push_str(&xml::object_listing(
            &href,
            &etag,
            "text/vcard; charset=utf-8",
        ));
        live_hrefs.insert(href);
    }

    let destroyed_ids = to_uuids(&changes.destroyed);
    match crate::db::tombstone::lookup(&state.db.conn, account, "Contact", &destroyed_ids).await {
        Ok(tombs) => {
            for t in tombs {
                if t.collection_id != book_id {
                    continue;
                }
                let href = format!("/dav/addressbooks/{account}/{book_id}/{}.vcf", t.uid);
                if !live_hrefs.contains(&href) {
                    out.push_str(&xml::object_not_found(&href));
                }
            }
        }
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "sync: tombstone lookup failed");
            return super::server_error();
        }
    }

    let token = super::sync_token_string(&changes.new_state);
    xml::sync_multistatus_response(&out, &token)
}

async fn calendar_report(
    state: &Arc<AppState>,
    account: i32,
    calendar_id: &str,
    parsed: Parsed,
) -> Response {
    // A parser error anywhere in the body → 400 (don't trust a partial
    // parse, even if the root element was recognised).
    if parsed.malformed {
        return super::bad_request();
    }
    // Reject a REPORT root that isn't a calendar report (a CardDAV report
    // mis-sent here, or an unknown root) rather than silently running an
    // expensive query-all.
    match parsed.kind {
        Kind::CalendarMultiget | Kind::CalendarQuery => {}
        _ => return super::bad_request(),
    }

    let Ok(cal_uuid) = calendar_id.parse::<Uuid>() else {
        return super::not_found();
    };

    let mut out = String::new();

    if parsed.kind == Kind::CalendarMultiget {
        // Return exactly the requested hrefs (echo verbatim for client
        // correlation); 404 for any the server doesn't have. An empty
        // href list yields an empty (valid) multistatus — NOT query-all.
        if parsed.hrefs.len() > super::MAX_MULTIGET_HREFS {
            tracing::warn!(
                target: "maild::dav",
                count = parsed.hrefs.len(),
                cap = super::MAX_MULTIGET_HREFS,
                "calendar multiget href count over cap — extra hrefs ignored",
            );
        }
        for h in parsed.hrefs.iter().take(super::MAX_MULTIGET_HREFS) {
            match href::parse(h) {
                // Account- and collection-scoped: a foreign or
                // cross-collection href resolves to a 404 member, never
                // a cross-tenant read.
                Some(Resource::CalendarObject {
                    account: a,
                    calendar_id: c,
                    uid,
                }) if a == account && c == calendar_id => {
                    match db::calendar::get_event_by_uid(&state.db.conn, account, cal_uuid, &uid)
                        .await
                    {
                        Ok(Some(ev)) => {
                            let (ics, etag) = event_ics_etag(&ev);
                            out.push_str(&xml::object_with_data(h, &etag, CAL_DATA, &ics));
                        }
                        _ => out.push_str(&xml::object_not_found(h)),
                    }
                }
                _ => out.push_str(&xml::object_not_found(h)),
            }
        }
    } else {
        // calendar-query: return the (bounded) collection filtered by the
        // query's time-range when present (RFC 4791 §9.9 — see
        // `event_in_range` for the recurrence/TZ caveats). Non-time
        // filters are still treated as match-all: a correct superset.
        match db::calendar::list_events_in_calendar(
            &state.db.conn,
            account,
            cal_uuid,
            super::MAX_COLLECTION_OBJECTS,
        )
        .await
        {
            Ok(events) => {
                super::warn_if_capped(events.len(), "calendar-query REPORT");
                for ev in events
                    .iter()
                    .filter(|ev| event_in_range(ev, &parsed.time_range))
                {
                    let href = format!("/dav/calendars/{account}/{calendar_id}/{}.ics", ev.uid);
                    let (ics, etag) = event_ics_etag(ev);
                    out.push_str(&xml::object_with_data(&href, &etag, CAL_DATA, &ics));
                }
            }
            Err(e) => {
                tracing::warn!(target: "maild::dav", error = %e, "calendar REPORT failed");
                return super::server_error();
            }
        }
    }

    xml::multistatus_response(xml::wrap_multistatus(&out))
}

async fn addressbook_report(
    state: &Arc<AppState>,
    account: i32,
    book_id: &str,
    parsed: Parsed,
) -> Response {
    if parsed.malformed {
        return super::bad_request();
    }
    match parsed.kind {
        Kind::AddressbookMultiget | Kind::AddressbookQuery => {}
        _ => return super::bad_request(),
    }

    let Ok(book_uuid) = book_id.parse::<Uuid>() else {
        return super::not_found();
    };

    let mut out = String::new();

    if parsed.kind == Kind::AddressbookMultiget {
        if parsed.hrefs.len() > super::MAX_MULTIGET_HREFS {
            tracing::warn!(
                target: "maild::dav",
                count = parsed.hrefs.len(),
                cap = super::MAX_MULTIGET_HREFS,
                "addressbook multiget href count over cap — extra hrefs ignored",
            );
        }
        for h in parsed.hrefs.iter().take(super::MAX_MULTIGET_HREFS) {
            match href::parse(h) {
                Some(Resource::AddressbookObject {
                    account: a,
                    book_id: b,
                    uid,
                }) if a == account && b == book_id => {
                    match db::contact::get_contact_by_uid(&state.db.conn, account, book_uuid, &uid)
                        .await
                    {
                        Ok(Some(c)) => {
                            let (vcf, etag) = contact_vcf_etag(&c);
                            out.push_str(&xml::object_with_data(h, &etag, ADDR_DATA, &vcf));
                        }
                        _ => out.push_str(&xml::object_not_found(h)),
                    }
                }
                _ => out.push_str(&xml::object_not_found(h)),
            }
        }
    } else {
        match db::contact::list_contacts_in_book(
            &state.db.conn,
            account,
            book_uuid,
            super::MAX_COLLECTION_OBJECTS,
        )
        .await
        {
            Ok(contacts) => {
                super::warn_if_capped(contacts.len(), "addressbook-query REPORT");
                for c in &contacts {
                    let href = format!("/dav/addressbooks/{account}/{book_id}/{}.vcf", c.uid);
                    let (vcf, etag) = contact_vcf_etag(c);
                    out.push_str(&xml::object_with_data(&href, &etag, ADDR_DATA, &vcf));
                }
            }
            Err(e) => {
                tracing::warn!(target: "maild::dav", error = %e, "addressbook REPORT failed");
                return super::server_error();
            }
        }
    }

    xml::multistatus_response(xml::wrap_multistatus(&out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_calendar_multiget_hrefs() {
        let body = br#"<?xml version="1.0"?>
            <C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
              <D:prop><D:getetag/><C:calendar-data/></D:prop>
              <D:href>/dav/calendars/1/cal/a.ics</D:href>
              <D:href>/dav/calendars/1/cal/b.ics</D:href>
            </C:calendar-multiget>"#;
        let p = parse(body);
        assert_eq!(p.kind, Kind::CalendarMultiget);
        assert_eq!(
            p.hrefs,
            vec![
                "/dav/calendars/1/cal/a.ics".to_string(),
                "/dav/calendars/1/cal/b.ics".to_string()
            ]
        );
    }

    #[test]
    fn parses_calendar_query_time_range() {
        let body = br#"<?xml version="1.0"?>
            <C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
              <D:prop><D:getetag/><C:calendar-data/></D:prop>
              <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
                <C:time-range start="20260601T000000Z" end="20260630T000000Z"/>
              </C:comp-filter></C:comp-filter></C:filter>
            </C:calendar-query>"#;
        let p = parse(body);
        assert_eq!(p.kind, Kind::CalendarQuery);
        let (start, end) = p.time_range.expect("time-range captured");
        assert_eq!(
            start,
            Some(chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap())
        );
        assert_eq!(
            end,
            Some(chrono::Utc.with_ymd_and_hms(2026, 6, 30, 0, 0, 0).unwrap())
        );
    }

    #[test]
    fn query_without_time_range_has_none() {
        let body = br#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
              <D:prop><D:getetag/></D:prop>
              <C:filter><C:comp-filter name="VCALENDAR"/></C:filter>
            </C:calendar-query>"#;
        let p = parse(body);
        assert_eq!(p.kind, Kind::CalendarQuery);
        assert!(p.time_range.is_none());
    }

    fn mk_event(start: Option<&str>, end: Option<&str>, data: serde_json::Value) -> CalendarEvent {
        CalendarEvent {
            id: "e".into(),
            calendar_id: "c".into(),
            account_id: 1,
            uid: "u".into(),
            data,
            title: None,
            start_dt: start.map(|s| s.to_string()),
            end_dt: end.map(|s| s.to_string()),
            updated_at: None,
        }
    }

    fn june_range() -> Option<TimeRange> {
        Some((
            parse_caldav_utc("20260601T000000Z"),
            parse_caldav_utc("20260630T000000Z"),
        ))
    }

    #[test]
    fn time_range_includes_in_range_event() {
        let ev = mk_event(
            Some("2026-06-15 09:00:00"),
            Some("2026-06-15 10:00:00"),
            serde_json::json!({}),
        );
        assert!(event_in_range(&ev, &june_range()));
    }

    #[test]
    fn time_range_excludes_out_of_range_event() {
        let ev = mk_event(
            Some("2026-01-10 09:00:00"),
            Some("2026-01-10 10:00:00"),
            serde_json::json!({}),
        );
        assert!(!event_in_range(&ev, &june_range()));
    }

    #[test]
    fn time_range_includes_straddling_event() {
        let ev = mk_event(
            Some("2026-05-31 23:00:00"),
            Some("2026-06-01 01:00:00"),
            serde_json::json!({}),
        );
        assert!(event_in_range(&ev, &june_range()));
    }

    #[test]
    fn time_range_point_event_at_range_start_included() {
        let ev = mk_event(Some("2026-06-01 00:00:00"), None, serde_json::json!({}));
        assert!(event_in_range(&ev, &june_range()));
    }

    #[test]
    fn time_range_recurring_old_start_conservatively_included() {
        // Weekly event started in January — instances may fall in June.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nDTSTART:20260110T090000Z\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n"
        });
        let ev = mk_event(
            Some("2026-01-10 09:00:00"),
            Some("2026-01-10 10:00:00"),
            data,
        );
        assert!(event_in_range(&ev, &june_range()));
    }

    #[test]
    fn time_range_recurring_after_range_end_excluded() {
        // First instance is after the queried window — no instance can
        // precede DTSTART, so exclusion is safe even for recurrence.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nDTSTART:20261001T090000Z\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n"
        });
        let ev = mk_event(
            Some("2026-10-01 09:00:00"),
            Some("2026-10-01 10:00:00"),
            data,
        );
        assert!(!event_in_range(&ev, &june_range()));
    }

    #[test]
    fn no_time_range_matches_everything() {
        let ev = mk_event(Some("1999-01-01 00:00:00"), None, serde_json::json!({}));
        assert!(event_in_range(&ev, &None));
    }

    #[test]
    fn all_day_without_dtend_included_by_midday_query() {
        // RFC 5545 §3.6.1: a DATE VEVENT without DTEND lasts one day. A
        // June 15 all-day event must match a range starting June 15 noon.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nUID:a\r\nDTSTART;VALUE=DATE:20260615\r\nEND:VEVENT\r\n"
        });
        let ev = mk_event(Some("2026-06-15 00:00:00"), None, data);
        let range = Some((
            parse_caldav_utc("20260615T120000Z"),
            parse_caldav_utc("20260616T000000Z"),
        ));
        assert!(event_in_range(&ev, &range));
    }

    #[test]
    fn tzid_event_at_range_boundary_not_falsely_excluded() {
        // 2026-06-01 00:30 Brisbane = 2026-05-31 14:30 UTC, but v1 indexes
        // the local wall time as UTC. A query for June must still include
        // it (±14h slack for non-UTC-certain instants) — and equally, a
        // Brisbane event indexed at "2026-05-31 18:00" (true UTC June 1
        // 04:00) must not vanish from a June query.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nUID:b\r\nDTSTART;TZID=Australia/Brisbane:20260531T180000\r\nEND:VEVENT\r\n"
        });
        let ev = mk_event(
            Some("2026-05-31 18:00:00"),
            Some("2026-05-31 19:00:00"),
            data,
        );
        assert!(event_in_range(&ev, &june_range()));
    }

    #[test]
    fn utc_certain_event_gets_no_slack() {
        // A UTC-stamped event just before the range must stay excluded —
        // slack applies only to non-UTC-certain instants.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nUID:c\r\nDTSTART:20260531T180000Z\r\nDTEND:20260531T190000Z\r\nEND:VEVENT\r\n"
        });
        let ev = mk_event(
            Some("2026-05-31 18:00:00"),
            Some("2026-05-31 19:00:00"),
            data,
        );
        assert!(!event_in_range(&ev, &june_range()));
    }

    #[test]
    fn one_off_event_inside_recurring_vtimezone_not_recurring() {
        // VTIMEZONE DST rules carry RRULEs; they must not classify the
        // (single) VEVENT as recurring — else an out-of-range one-off
        // with an old DTSTART would leak into every future query.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\n\
                 BEGIN:DAYLIGHT\r\nRRULE:FREQ=YEARLY;BYMONTH=3\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n\
                 BEGIN:VEVENT\r\nUID:d\r\nDTSTART:20260110T090000Z\r\nDTEND:20260110T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        });
        let facts = vevent_facts(&data);
        assert!(!facts.recurring);
        let ev = mk_event(
            Some("2026-01-10 09:00:00"),
            Some("2026-01-10 10:00:00"),
            data,
        );
        assert!(!event_in_range(&ev, &june_range()));
    }

    #[test]
    fn duration_event_without_dtend_conservatively_included() {
        // RFC 5545 permits DTSTART+DURATION instead of DTEND; the v1
        // index has no end for these — a 09:00Z + PT8H event must match
        // a noon query.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nUID:f\r\nDTSTART:20260615T090000Z\r\nDURATION:PT8H\r\nEND:VEVENT\r\n"
        });
        let ev = mk_event(Some("2026-06-15 09:00:00"), None, data);
        let range = Some((
            parse_caldav_utc("20260615T120000Z"),
            parse_caldav_utc("20260616T000000Z"),
        ));
        assert!(event_in_range(&ev, &range));
    }

    #[test]
    fn mixed_zone_dtend_makes_event_uncertain() {
        // UTC DTSTART + TZID DTEND: end certainty matters near the lower
        // range boundary — the event must get slack, not exact compare.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nUID:g\r\nDTSTART:20260531T180000Z\r\n\
                 DTEND;TZID=Australia/Brisbane:20260601T050000\r\nEND:VEVENT\r\n"
        });
        assert!(!vevent_facts(&data).utc_certain);
        let ev = mk_event(
            Some("2026-05-31 18:00:00"),
            Some("2026-06-01 05:00:00"),
            data,
        );
        assert!(event_in_range(&ev, &june_range()));
    }

    #[test]
    fn range_edge_near_datetime_ceiling_does_not_panic() {
        // Slack addition to a client-supplied range edge near the
        // representable maximum must clamp, not overflow.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nUID:h\r\nDTSTART;TZID=X/Y:99991231T220000\r\nEND:VEVENT\r\n"
        });
        let ev = mk_event(Some("9999-12-31 22:00:00"), None, data);
        let range = Some((
            parse_caldav_utc("99990101T000000Z"),
            parse_caldav_utc("99991231T235959Z"),
        ));
        assert!(event_in_range(&ev, &range));
    }

    #[test]
    fn folded_rrule_line_still_detected() {
        // RRULE folded across physical lines must still classify as
        // recurring after unfolding.
        let data = serde_json::json!({
            "cosmix:rawICalendar":
                "BEGIN:VEVENT\r\nUID:e\r\nDTSTART:20260110T090000Z\r\nRRU\r\n LE:FREQ=WEEKLY\r\nEND:VEVENT\r\n"
        });
        assert!(vevent_facts(&data).recurring);
    }

    #[test]
    fn parses_addressbook_query_kind_no_hrefs() {
        let body =
            br#"<CR:addressbook-query xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
              <D:prop><D:getetag/><CR:address-data/></D:prop>
            </CR:addressbook-query>"#;
        let p = parse(body);
        assert_eq!(p.kind, Kind::AddressbookQuery);
        assert!(p.hrefs.is_empty());
    }

    #[test]
    fn malformed_body_is_tolerated() {
        let p = parse(b"<not-xml");
        assert_eq!(p.kind, Kind::Unknown);
        assert!(p.hrefs.is_empty());
    }

    #[test]
    fn parses_sync_collection_token() {
        let body = br#"<D:sync-collection xmlns:D="DAV:">
              <D:sync-token>https://cosmix.invalid/ns/sync/7</D:sync-token>
              <D:sync-level>1</D:sync-level>
              <D:prop><D:getetag/></D:prop>
            </D:sync-collection>"#;
        let p = parse(body);
        assert_eq!(p.kind, Kind::SyncCollection);
        assert_eq!(
            p.sync_token.as_deref(),
            Some("https://cosmix.invalid/ns/sync/7")
        );
        assert!(!p.malformed);
    }

    #[test]
    fn initial_sync_has_no_token() {
        let body = br#"<D:sync-collection xmlns:D="DAV:"><D:sync-token/><D:prop><D:getetag/></D:prop></D:sync-collection>"#;
        let p = parse(body);
        assert_eq!(p.kind, Kind::SyncCollection);
        // Empty sync-token element → treated as initial sync.
        assert_eq!(
            super::super::parse_sync_token(p.sync_token.as_deref()),
            Some(0)
        );
    }

    #[test]
    fn recognized_root_but_broken_xml_flags_malformed() {
        // Root parses as calendar-query, then the body is truncated mid
        // element — must be flagged so the handler 400s instead of
        // running a full collection query.
        let body = br#"<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav"><C:filter><bad"#;
        let p = parse(body);
        assert_eq!(p.kind, Kind::CalendarQuery);
        assert!(p.malformed, "truncated body must set malformed");
    }
}
