//! DAV multistatus XML — hand-rolled writer (RFC 4918 §14).
//!
//! M1 emits discovery responses only (principal, home-sets, collection
//! listings). The writer builds escaped strings directly; quick-xml is
//! introduced in M2 for parsing REPORT request bodies. All hrefs are
//! **relative** (RFC 4918 allows it; Radicale does the same), so the
//! server stays correct behind a reverse proxy regardless of scheme/host.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

use crate::db::calendar::Calendar;
use crate::db::contact::AddressBook;

/// XML-escape text content (`& < > " '`).
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

const OK: &str = "<D:status>HTTP/1.1 200 OK</D:status>";

/// Wrap response fragments in a `<multistatus>` document with all DAV /
/// CalDAV / CardDAV / calendarserver / Apple namespaces declared.
pub fn wrap_multistatus(responses: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <D:multistatus xmlns:D=\"DAV:\" \
         xmlns:C=\"urn:ietf:params:xml:ns:caldav\" \
         xmlns:CR=\"urn:ietf:params:xml:ns:carddav\" \
         xmlns:CS=\"http://calendarserver.org/ns/\" \
         xmlns:A=\"http://apple.com/ns/ical/\">\
         {responses}</D:multistatus>"
    )
}

/// Build the `207 Multi-Status` HTTP response.
pub fn multistatus_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(body))
        .expect("static multistatus response is always valid")
}

/// A sync-collection `207` — the member responses followed by the new
/// `<D:sync-token>` (RFC 6578 §3.6), all inside `<D:multistatus>`.
pub fn sync_multistatus_response(responses: &str, sync_token: &str) -> Response {
    let inner = format!(
        "{responses}<D:sync-token>{}</D:sync-token>",
        esc(sync_token)
    );
    multistatus_response(wrap_multistatus(&inner))
}

// ── Discovery response fragments ────────────────────────────────────────

/// `/dav/` — current-user-principal probe.
pub fn root(account: i32) -> String {
    let p = format!("/dav/principals/{account}/");
    format!(
        "<D:response><D:href>/dav/</D:href><D:propstat><D:prop>\
         <D:current-user-principal><D:href>{p}</D:href></D:current-user-principal>\
         <D:principal-URL><D:href>{p}</D:href></D:principal-URL>\
         <D:resourcetype><D:collection/></D:resourcetype>\
         </D:prop>{OK}</D:propstat></D:response>"
    )
}

/// `/dav/principals/{account}/` — the principal: home-set hrefs.
pub fn principal(account: i32) -> String {
    let p = format!("/dav/principals/{account}/");
    let cal = format!("/dav/calendars/{account}/");
    let book = format!("/dav/addressbooks/{account}/");
    format!(
        "<D:response><D:href>{p}</D:href><D:propstat><D:prop>\
         <D:resourcetype><D:principal/><D:collection/></D:resourcetype>\
         <D:displayname>Account {account}</D:displayname>\
         <D:current-user-principal><D:href>{p}</D:href></D:current-user-principal>\
         <D:principal-URL><D:href>{p}</D:href></D:principal-URL>\
         <C:calendar-home-set><D:href>{cal}</D:href></C:calendar-home-set>\
         <CR:addressbook-home-set><D:href>{book}</D:href></CR:addressbook-home-set>\
         </D:prop>{OK}</D:propstat></D:response>"
    )
}

/// The calendar-home collection itself (`/dav/calendars/{account}/`).
pub fn calendar_home_self(account: i32) -> String {
    let p = format!("/dav/principals/{account}/");
    format!(
        "<D:response><D:href>/dav/calendars/{account}/</D:href><D:propstat><D:prop>\
         <D:resourcetype><D:collection/></D:resourcetype>\
         <D:displayname>Calendars</D:displayname>\
         <D:current-user-principal><D:href>{p}</D:href></D:current-user-principal>\
         </D:prop>{OK}</D:propstat></D:response>"
    )
}

/// One calendar collection. `ctag` is the calendarserver getctag hint
/// (Apple/Thunderbird poll it for "did anything change"); sync-collection
/// REPORT support is deferred to M4, so no `<D:sync-token>` is advertised
/// here yet (advertising it would invite a REPORT we don't serve).
pub fn calendar_collection(account: i32, cal: &Calendar, ctag: &str, sync_token: &str) -> String {
    let color = cal
        .color
        .as_deref()
        .map(|c| format!("<A:calendar-color>{}</A:calendar-color>", esc(c)))
        .unwrap_or_default();
    format!(
        "<D:response><D:href>/dav/calendars/{account}/{id}/</D:href><D:propstat><D:prop>\
         <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>\
         <D:displayname>{name}</D:displayname>\
         <C:supported-calendar-component-set><C:comp name=\"VEVENT\"/></C:supported-calendar-component-set>\
         <CS:getctag>{ctag}</CS:getctag>\
         <D:sync-token>{sync_token}</D:sync-token>\
         <D:supported-report-set>\
         <D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report>\
         <D:supported-report><D:report><C:calendar-multiget/></D:report></D:supported-report>\
         <D:supported-report><D:report><C:calendar-query/></D:report></D:supported-report>\
         </D:supported-report-set>\
         {color}\
         </D:prop>{OK}</D:propstat></D:response>",
        id = esc(&cal.id),
        name = esc(&cal.name),
        ctag = esc(ctag),
        sync_token = esc(sync_token),
    )
}

/// The addressbook-home collection itself (`/dav/addressbooks/{account}/`).
pub fn addressbook_home_self(account: i32) -> String {
    let p = format!("/dav/principals/{account}/");
    format!(
        "<D:response><D:href>/dav/addressbooks/{account}/</D:href><D:propstat><D:prop>\
         <D:resourcetype><D:collection/></D:resourcetype>\
         <D:displayname>Address Books</D:displayname>\
         <D:current-user-principal><D:href>{p}</D:href></D:current-user-principal>\
         </D:prop>{OK}</D:propstat></D:response>"
    )
}

/// One addressbook collection.
pub fn addressbook_collection(
    account: i32,
    book: &AddressBook,
    ctag: &str,
    sync_token: &str,
) -> String {
    format!(
        "<D:response><D:href>/dav/addressbooks/{account}/{id}/</D:href><D:propstat><D:prop>\
         <D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype>\
         <D:displayname>{name}</D:displayname>\
         <CS:getctag>{ctag}</CS:getctag>\
         <D:sync-token>{sync_token}</D:sync-token>\
         <D:supported-report-set>\
         <D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report>\
         <D:supported-report><D:report><CR:addressbook-multiget/></D:report></D:supported-report>\
         <D:supported-report><D:report><CR:addressbook-query/></D:report></D:supported-report>\
         </D:supported-report-set>\
         </D:prop>{OK}</D:propstat></D:response>",
        id = esc(&book.id),
        name = esc(&book.name),
        ctag = esc(ctag),
        sync_token = esc(sync_token),
    )
}

// ── Object (resource) response fragments ────────────────────────────────

/// A child object in a collection PROPFIND depth-1 listing: metadata
/// only (no body), so a client learns the href + ETag to fetch/diff.
pub fn object_listing(href: &str, etag: &str, content_type: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
         <D:resourcetype/>\
         <D:getetag>{etag}</D:getetag>\
         <D:getcontenttype>{ctype}</D:getcontenttype>\
         </D:prop>{OK}</D:propstat></D:response>",
        href = esc(href),
        etag = esc(etag),
        ctype = esc(content_type),
    )
}

/// An object response carrying its serialized body in `data_elem`
/// (`C:calendar-data` or `CR:address-data`) — used by multiget/query
/// REPORTs. `data_elem` is a controlled constant (not user input).
pub fn object_with_data(href: &str, etag: &str, data_elem: &str, data: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
         <D:getetag>{etag}</D:getetag>\
         <{data_elem}>{data}</{data_elem}>\
         </D:prop>{OK}</D:propstat></D:response>",
        href = esc(href),
        etag = esc(etag),
        data = esc(data),
    )
}

/// A `404` member response (a multiget href the server doesn't have).
pub fn object_not_found(href: &str) -> String {
    format!(
        "<D:response><D:href>{}</D:href>\
         <D:status>HTTP/1.1 404 Not Found</D:status></D:response>",
        esc(href)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_text() {
        assert_eq!(esc("a&b<c>\"'"), "a&amp;b&lt;c&gt;&quot;&apos;");
    }

    #[test]
    fn principal_lists_home_sets() {
        let xml = wrap_multistatus(&principal(3));
        assert!(xml.contains("<C:calendar-home-set><D:href>/dav/calendars/3/</D:href>"));
        assert!(xml.contains("<CR:addressbook-home-set><D:href>/dav/addressbooks/3/</D:href>"));
        assert!(xml.contains("xmlns:CR=\"urn:ietf:params:xml:ns:carddav\""));
    }

    #[test]
    fn calendar_collection_escapes_name_and_carries_ctag() {
        let cal = Calendar {
            id: "cal-uuid".into(),
            account_id: 3,
            name: "Work & Play".into(),
            color: Some("#ff0000".into()),
            description: None,
            is_visible: true,
            default_alerts: None,
            timezone: None,
            sort_order: 0,
        };
        let frag = calendar_collection(3, &cal, "42", "https://cosmix.invalid/ns/sync/42");
        assert!(frag.contains("<D:displayname>Work &amp; Play</D:displayname>"));
        assert!(frag.contains("<CS:getctag>42</CS:getctag>"));
        assert!(frag.contains("<C:calendar/>"));
        assert!(frag.contains("<A:calendar-color>#ff0000</A:calendar-color>"));
        assert!(frag.contains("/dav/calendars/3/cal-uuid/"));
        assert!(frag.contains("<D:sync-collection/>")); // supported-report-set
        assert!(frag.contains("<D:sync-token>https://cosmix.invalid/ns/sync/42</D:sync-token>"));
    }
}
