//! GET on a calendar/contact object resource — emit iCalendar / vCard.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use uuid::Uuid;

use crate::db;
use crate::jmap::AppState;

use super::href::Resource;
use super::parse_stored_dt as parse_dt;

/// Dispatch GET for an object resource (collections/discovery resources
/// have no GET → 405 is handled by the caller). Returns 404 for a missing
/// object, 200 + ETag + body otherwise.
pub async fn handle(state: &Arc<AppState>, account: i32, resource: &Resource) -> Response {
    match resource {
        Resource::CalendarObject {
            calendar_id, uid, ..
        } => calendar_object(state, account, calendar_id, uid).await,
        Resource::AddressbookObject { book_id, uid, .. } => {
            addressbook_object(state, account, book_id, uid).await
        }
        // GET on a collection/principal/home is not meaningful here.
        _ => super::method_not_allowed_get(),
    }
}

async fn calendar_object(
    state: &Arc<AppState>,
    account: i32,
    calendar_id: &str,
    uid: &str,
) -> Response {
    let Ok(cal_uuid) = calendar_id.parse::<Uuid>() else {
        return super::not_found();
    };
    match db::calendar::get_event_by_uid(&state.db.conn, account, cal_uuid, uid).await {
        Ok(Some(ev)) => {
            let start = ev.start_dt.as_deref().and_then(parse_dt);
            let end = ev.end_dt.as_deref().and_then(parse_dt);
            let updated = ev.updated_at.as_deref().and_then(parse_dt);
            let ics = cosmix_lib_davproto::ical::event_to_ics(
                &ev.uid,
                ev.title.as_deref(),
                start,
                end,
                updated,
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
            body_response(ics.into_bytes(), "text/calendar; charset=utf-8", &etag)
        }
        Ok(None) => super::not_found(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "GET calendar object failed");
            super::server_error()
        }
    }
}

async fn addressbook_object(
    state: &Arc<AppState>,
    account: i32,
    book_id: &str,
    uid: &str,
) -> Response {
    let Ok(book_uuid) = book_id.parse::<Uuid>() else {
        return super::not_found();
    };
    match db::contact::get_contact_by_uid(&state.db.conn, account, book_uuid, uid).await {
        Ok(Some(c)) => {
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
            body_response(vcf.into_bytes(), "text/vcard; charset=utf-8", &etag)
        }
        Ok(None) => super::not_found(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "GET addressbook object failed");
            super::server_error()
        }
    }
}

/// `200 OK` with the resource body, content type, and strong ETag.
fn body_response(body: Vec<u8>, content_type: &str, etag: &str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ETAG, etag)
        .body(Body::from(body))
        .unwrap_or_else(|_| super::server_error())
}
