//! PUT + DELETE on object resources — ETag-guarded writes (RFC 4791 §5.3
//! / RFC 6352 §6.3).
//!
//! Preconditions (`If-None-Match: *` create-only; `If-Match: <etag>`
//! conditional) are parsed here but **evaluated inside the DB lock** by
//! `db::conditional_*` — so the read-check-write is atomic and two
//! concurrent conditional writers can't both pass. A successful write
//! records the changelog and pushes a JMAP `StateChange` so EventSource
//! clients observe DAV-originated changes too.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::db::calendar::EventMetadata;
use crate::db::contact::ContactMetadata;
use crate::db::{self, DavPrecondition, DeleteOutcome, IfClause, UpsertOutcome};
use crate::jmap::{AppState, StateChange};

use super::href::Resource;

// ── precondition parsing (evaluation happens in the DB layer) ────────────

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Parse one `If-Match`/`If-None-Match` header value into an [`IfClause`].
fn parse_if_clause(v: &str) -> IfClause {
    if v.trim() == "*" {
        IfClause::Star
    } else {
        IfClause::Tags(
            v.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
        )
    }
}

/// Build a [`DavPrecondition`] from the request headers.
fn parse_precondition(headers: &HeaderMap) -> DavPrecondition {
    DavPrecondition {
        if_match: header_str(headers, "if-match").map(parse_if_clause),
        if_none_match: header_str(headers, "if-none-match").map(parse_if_clause),
    }
}

// ── PUT ─────────────────────────────────────────────────────────────────

pub async fn put(
    state: &Arc<AppState>,
    account: i32,
    resource: &Resource,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    match resource {
        Resource::CalendarObject {
            calendar_id, uid, ..
        } => put_event(state, account, calendar_id, uid, headers, body).await,
        Resource::AddressbookObject { book_id, uid, .. } => {
            put_contact(state, account, book_id, uid, headers, body).await
        }
        _ => super::method_not_allowed(),
    }
}

async fn put_event(
    state: &Arc<AppState>,
    account: i32,
    calendar_id: &str,
    uid: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let Ok(cal_uuid) = calendar_id.parse::<Uuid>() else {
        return super::not_found();
    };
    let Ok(body_str) = std::str::from_utf8(body) else {
        return unprocessable();
    };
    let parsed = match cosmix_lib_davproto::ical::parse_ics(body_str) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(target: "maild::dav", error = %e, "PUT: invalid iCalendar");
            return unprocessable();
        }
    };
    let precond = parse_precondition(headers);
    let meta = EventMetadata {
        title: parsed.title.as_deref(),
        start_dt: parsed.start,
        end_dt: parsed.end,
    };
    let outcome = db::calendar::conditional_upsert_event_by_uid(
        &state.db.conn,
        account,
        cal_uuid,
        uid,
        &parsed.data,
        meta,
        &precond,
    )
    .await;
    match outcome {
        Ok(UpsertOutcome::Created(_)) => {
            notify_state(state, account, "CalendarEvent").await;
            let etag = event_etag(state, account, cal_uuid, uid).await;
            write_ok(true, &etag)
        }
        Ok(UpsertOutcome::Updated(_)) => {
            notify_state(state, account, "CalendarEvent").await;
            let etag = event_etag(state, account, cal_uuid, uid).await;
            write_ok(false, &etag)
        }
        Ok(UpsertOutcome::PreconditionFailed) => precondition_failed(),
        Ok(UpsertOutcome::CollectionMissing) => super::not_found(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "PUT: upsert failed");
            super::server_error()
        }
    }
}

async fn put_contact(
    state: &Arc<AppState>,
    account: i32,
    book_id: &str,
    uid: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let Ok(book_uuid) = book_id.parse::<Uuid>() else {
        return super::not_found();
    };
    let Ok(body_str) = std::str::from_utf8(body) else {
        return unprocessable();
    };
    let parsed = match cosmix_lib_davproto::vcard::parse_vcf(body_str) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(target: "maild::dav", error = %e, "PUT: invalid vCard");
            return unprocessable();
        }
    };
    let precond = parse_precondition(headers);
    let meta = ContactMetadata {
        full_name: parsed.full_name.as_deref(),
        email: parsed.email.as_deref(),
        company: parsed.company.as_deref(),
    };
    let outcome = db::contact::conditional_upsert_contact_by_uid(
        &state.db.conn,
        account,
        book_uuid,
        uid,
        &parsed.data,
        meta,
        &precond,
    )
    .await;
    match outcome {
        Ok(UpsertOutcome::Created(_)) => {
            notify_state(state, account, "Contact").await;
            let etag = contact_etag(state, account, book_uuid, uid).await;
            write_ok(true, &etag)
        }
        Ok(UpsertOutcome::Updated(_)) => {
            notify_state(state, account, "Contact").await;
            let etag = contact_etag(state, account, book_uuid, uid).await;
            write_ok(false, &etag)
        }
        Ok(UpsertOutcome::PreconditionFailed) => precondition_failed(),
        Ok(UpsertOutcome::CollectionMissing) => super::not_found(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "PUT: upsert failed");
            super::server_error()
        }
    }
}

// ── DELETE ──────────────────────────────────────────────────────────────

pub async fn delete(
    state: &Arc<AppState>,
    account: i32,
    resource: &Resource,
    headers: &HeaderMap,
) -> Response {
    let precond = parse_precondition(headers);
    match resource {
        Resource::CalendarObject {
            calendar_id, uid, ..
        } => {
            let Ok(cal_uuid) = calendar_id.parse::<Uuid>() else {
                return super::not_found();
            };
            let outcome = db::calendar::conditional_delete_event_by_uid(
                &state.db.conn,
                account,
                cal_uuid,
                uid,
                &precond,
            )
            .await;
            map_delete(state, account, "CalendarEvent", outcome).await
        }
        Resource::AddressbookObject { book_id, uid, .. } => {
            let Ok(book_uuid) = book_id.parse::<Uuid>() else {
                return super::not_found();
            };
            let outcome = db::contact::conditional_delete_contact_by_uid(
                &state.db.conn,
                account,
                book_uuid,
                uid,
                &precond,
            )
            .await;
            map_delete(state, account, "Contact", outcome).await
        }
        _ => super::method_not_allowed(),
    }
}

async fn map_delete(
    state: &Arc<AppState>,
    account: i32,
    object_type: &str,
    outcome: anyhow::Result<DeleteOutcome>,
) -> Response {
    match outcome {
        Ok(DeleteOutcome::Deleted(_)) => {
            notify_state(state, account, object_type).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(DeleteOutcome::NotFound) => super::not_found(),
        Ok(DeleteOutcome::PreconditionFailed) => precondition_failed(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "DELETE failed");
            super::server_error()
        }
    }
}

// ── shared ──────────────────────────────────────────────────────────────

/// ETag of the now-stored event (recomputed from the row so it exactly
/// matches a subsequent GET). Empty string if the row vanished.
async fn event_etag(state: &Arc<AppState>, account: i32, cal: Uuid, uid: &str) -> String {
    match db::calendar::get_event_by_uid(&state.db.conn, account, cal, uid).await {
        Ok(Some(ev)) => cosmix_lib_davproto::etag::for_event(
            &ev.uid,
            ev.title.as_deref(),
            ev.start_dt.as_deref(),
            ev.end_dt.as_deref(),
            ev.updated_at.as_deref(),
            &ev.data,
        ),
        _ => String::new(),
    }
}

async fn contact_etag(state: &Arc<AppState>, account: i32, book: Uuid, uid: &str) -> String {
    match db::contact::get_contact_by_uid(&state.db.conn, account, book, uid).await {
        Ok(Some(c)) => cosmix_lib_davproto::etag::for_contact(
            &c.uid,
            c.full_name.as_deref(),
            c.email.as_deref(),
            c.company.as_deref(),
            &c.data,
        ),
        _ => String::new(),
    }
}

/// Push a JMAP `StateChange` so EventSource clients observe a DAV-
/// originated change. The changelog row was already written **inside the
/// DB transaction** by the `conditional_*` helper, so this only reads the
/// new state and notifies — best-effort (a notify failure must not fail
/// the committed write).
async fn notify_state(state: &Arc<AppState>, account: i32, object_type: &str) {
    if let Ok(new_state) = db::changelog::current_state(&state.db.conn, account, object_type).await
    {
        // `send` errs only when there are no subscribers — ignore.
        let _ = state.state_tx.send(StateChange {
            account_id: account,
            object_type: object_type.to_string(),
            new_state,
        });
    }
}

/// `201 Created` (new) or `204 No Content` (replaced), carrying the new
/// strong ETag.
fn write_ok(created: bool, etag: &str) -> Response {
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    };
    let mut builder = Response::builder().status(status);
    if !etag.is_empty()
        && let Ok(v) = header::HeaderValue::from_str(etag)
    {
        builder = builder.header(header::ETAG, v);
    }
    builder
        .body(Body::empty())
        .unwrap_or_else(|_| super::server_error())
}

/// `412 Precondition Failed`.
fn precondition_failed() -> Response {
    StatusCode::PRECONDITION_FAILED.into_response()
}

/// `422 Unprocessable Entity` — the body wasn't valid UTF-8 / iCal / vCard.
fn unprocessable() -> Response {
    StatusCode::UNPROCESSABLE_ENTITY.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_if_clause_star_and_tags() {
        assert!(matches!(parse_if_clause("*"), IfClause::Star));
        match parse_if_clause("\"a\", W/\"b\"") {
            IfClause::Tags(t) => assert_eq!(t, vec!["\"a\"".to_string(), "W/\"b\"".to_string()]),
            _ => panic!("expected tags"),
        }
    }

    #[test]
    fn if_none_match_star_blocks_overwrite() {
        let p = DavPrecondition {
            if_none_match: Some(IfClause::Star),
            ..Default::default()
        };
        assert!(!p.allows(Some("\"e\""), true)); // exists → blocked
        assert!(p.allows(None, false)); // absent → ok
    }

    #[test]
    fn if_match_uses_strong_comparison() {
        let p = DavPrecondition {
            if_match: Some(IfClause::Tags(vec!["\"good\"".into()])),
            ..Default::default()
        };
        assert!(p.allows(Some("\"good\""), true));
        assert!(!p.allows(Some("\"stale\""), true));
        assert!(!p.allows(None, false)); // absent → fails

        // A weak request tag must NOT strong-match (RFC 7232 §3.1).
        let weak = DavPrecondition {
            if_match: Some(IfClause::Tags(vec!["W/\"good\"".into()])),
            ..Default::default()
        };
        assert!(!weak.allows(Some("\"good\""), true));
    }

    #[test]
    fn no_preconditions_allows() {
        let p = DavPrecondition::default();
        assert!(p.allows(Some("\"e\""), true));
        assert!(p.allows(None, false));
    }
}
