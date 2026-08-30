//! CalDAV (RFC 4791) + CardDAV (RFC 6352) server face for cosmix-maild.
//!
//! A translation + routing layer over maild's existing JMAP calendar /
//! contact store — it owns **no** storage. Calendars/events live in
//! `db::calendar` (JSCalendar, RFC 8984), addressbooks/contacts in
//! `db::contact` (JSContact, RFC 9553); auth reuses `auth::basic`; sync
//! state reuses `db::changelog`. Plan + rationale:
//! `_plan/2026-06-09-maild-dav-server.md`.
//!
//! ## Milestones
//! * **M1 (this commit)** — discovery: OPTIONS, `.well-known` redirects,
//!   Basic-auth challenge, PROPFIND on principal / home-sets / collection
//!   listings. Read of individual objects, REPORT, writes, and sync come
//!   in M2–M4.
//!
//! DAV's non-standard verbs (PROPFIND/REPORT/MKCALENDAR/…) reach handlers
//! because every route is registered with `any(...)` and the dispatcher
//! branches on `Request::method` — axum's `MethodFilter` would otherwise
//! drop them.

mod auth;
mod get;
mod href;
mod propfind;
mod report;
mod write;
mod xml;

/// Max REPORT/PROPFIND request body we'll buffer (multiget href lists are
/// small; this bounds a hostile body).
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// DoS backstop: cap on objects returned from a single collection
/// listing / query REPORT. Far above any real personal calendar/
/// addressbook; hitting it is logged (never silently truncated).
pub(super) const MAX_COLLECTION_OBJECTS: i64 = 100_000;

/// DoS backstop: cap on hrefs processed from one multiget REPORT.
pub(super) const MAX_MULTIGET_HREFS: usize = 10_000;

/// Parse a stored timestamp into a UTC instant, accepting both RFC 3339
/// (written by `update_event`/`update_contact`) and SQLite's
/// `datetime('now')` naive form `YYYY-MM-DD HH:MM:SS` (the column
/// default on insert). Used for DTSTART/DTEND/DTSTAMP — recovering the
/// naive form keeps emission deterministic instead of falling back.
pub(super) fn parse_stored_dt(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|n| n.and_utc())
}

/// Log loudly when a bounded read returns exactly the cap — the operator
/// must see truncation, never have it pass as "complete".
pub(super) fn warn_if_capped(returned: usize, what: &str) {
    if returned as i64 >= MAX_COLLECTION_OBJECTS {
        tracing::warn!(
            target: "maild::dav",
            cap = MAX_COLLECTION_OBJECTS,
            "{what} hit the object cap — response truncated; collection larger than the v1 limit",
        );
    }
}

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use crate::jmap::AppState;

pub use href::Resource;

/// Methods this build implements on a DAV resource. Advertised via
/// `OPTIONS`/`Allow` and used to synthesise a correct `405` — axum's
/// `any()` router does not emit an `Allow` header itself.
const ALLOW_METHODS: &str = "OPTIONS, PROPFIND, REPORT, GET, PUT, DELETE";

/// DAV compliance classes advertised in the `DAV:` response header.
const DAV_COMPLIANCE: &str = "1, 3, calendar-access, addressbook";

/// Build the DAV router. Merged into maild's main axum router in
/// `runtime.rs` *before* `with_state`.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/.well-known/caldav", any(well_known))
        .route("/.well-known/carddav", any(well_known))
        .route("/dav", any(dispatch))
        .route("/dav/", any(dispatch))
        .route("/dav/{*rest}", any(dispatch))
}

/// `.well-known/{caldav,carddav}` → `301` to the DAV root; clients run
/// discovery from there.
async fn well_known() -> Response {
    (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/dav/")]).into_response()
}

/// Single entry point for every `/dav/…` request: authenticate, parse the
/// path, bind the account, then branch on method.
async fn dispatch(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();

    // OPTIONS is an unauthenticated capability probe (RFC 4918 §9.1):
    // answer with the DAV compliance + Allow headers *before* the auth
    // gate so a client can discover DAV support before logging in. (It
    // exposes only server capabilities, no account data.)
    if parts.method == axum::http::Method::OPTIONS {
        return options_response();
    }

    // Auth first, so an unauthenticated probe gets the Basic challenge
    // (realm-bearing 401) rather than a 404 that hides the realm.
    let Some(account_id) = auth::authenticate(&state, &parts.headers).await else {
        return auth::challenge();
    };

    let Some(resource) = href::parse(parts.uri.path()) else {
        return not_found();
    };

    // Account binding: the path's numeric account segment must equal the
    // authenticated account id (integer equality, fail-closed). `Root`
    // has no account segment and is always allowed for an authed user.
    if let Some(path_acct) = resource.account_id()
        && path_acct != account_id
    {
        return StatusCode::FORBIDDEN.into_response();
    }

    match parts.method.as_str() {
        "PROPFIND" => {
            let depth = depth_header(&parts.headers);
            propfind::handle(&state, account_id, &resource, depth).await
        }
        "GET" => get::handle(&state, account_id, &resource).await,
        "REPORT" => {
            let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
                Ok(b) => b,
                Err(_) => return payload_too_large(),
            };
            report::handle(&state, account_id, &resource, &bytes).await
        }
        "PUT" => {
            let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
                Ok(b) => b,
                Err(_) => return payload_too_large(),
            };
            write::put(&state, account_id, &resource, &parts.headers, &bytes).await
        }
        "DELETE" => write::delete(&state, account_id, &resource, &parts.headers).await,
        _ => method_not_allowed(),
    }
}

/// Parse the `Depth` header. Missing/`1`/`infinity` → list children;
/// `0` → the resource itself. (We collapse `infinity` since the DAV tree
/// is at most home → collection → object.)
fn depth_header(headers: &HeaderMap) -> propfind::Depth {
    match headers.get("depth").and_then(|v| v.to_str().ok()) {
        Some("0") => propfind::Depth::Zero,
        _ => propfind::Depth::One,
    }
}

/// `OPTIONS` — advertise DAV compliance + the implemented method set.
fn options_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("DAV", DAV_COMPLIANCE)
        .header(header::ALLOW, ALLOW_METHODS)
        .header(header::CONTENT_LENGTH, "0")
        .body(Body::empty())
        .expect("static OPTIONS response is always valid")
}

/// `405` carrying the `Allow` header (`any()` won't synthesise it).
fn method_not_allowed() -> Response {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(header::ALLOW, ALLOW_METHODS)
        .body(Body::empty())
        .expect("static 405 response is always valid")
}

/// `405` for a GET against a non-object resource (collections/principals
/// have no document body to return).
fn method_not_allowed_get() -> Response {
    method_not_allowed()
}

/// `413` when a REPORT/PROPFIND body exceeds [`MAX_BODY_BYTES`].
fn payload_too_large() -> Response {
    StatusCode::PAYLOAD_TOO_LARGE.into_response()
}

/// `400` — a malformed or unsupported REPORT root element.
pub(super) fn bad_request() -> Response {
    StatusCode::BAD_REQUEST.into_response()
}

/// Opaque prefix for DAV sync-tokens (RFC 6578). The token is this prefix
/// plus the changelog state integer; clients store it verbatim and echo
/// it back. A URI form maximises client compatibility.
pub(super) const SYNC_TOKEN_PREFIX: &str = "https://cosmix.invalid/ns/sync/";

/// Render a changelog state as a DAV sync-token.
pub(super) fn sync_token_string(state: &str) -> String {
    format!("{SYNC_TOKEN_PREFIX}{state}")
}

/// Parse a client-supplied sync-token back to a changelog state integer.
/// `None`/empty → `Some(0)` (initial sync). A non-empty token that doesn't
/// resolve to an integer → `None` (the caller answers 403 valid-sync-token,
/// forcing a full resync).
pub(super) fn parse_sync_token(token: Option<&str>) -> Option<i64> {
    match token {
        None => Some(0),
        Some(t) => {
            let t = t.trim();
            if t.is_empty() {
                return Some(0);
            }
            let num = t.strip_prefix(SYNC_TOKEN_PREFIX).unwrap_or(t);
            num.parse::<i64>().ok()
        }
    }
}

/// `403 Forbidden` with a `valid-sync-token` precondition body — the
/// client's sync-token is stale/unrecognised and it must do a full resync
/// (RFC 6578 §3.7).
pub(super) fn invalid_sync_token() -> Response {
    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                <D:error xmlns:D=\"DAV:\"><D:valid-sync-token/></D:error>";
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::FORBIDDEN.into_response())
}

/// `404` — unknown/unsupported DAV path or absent collection.
fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// `500` — an internal error (DB failure); detail is logged, never leaked.
fn server_error() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
