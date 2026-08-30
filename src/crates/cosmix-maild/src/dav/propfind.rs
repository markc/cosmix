//! PROPFIND (RFC 4918 §9.1) — discovery + collection listing.
//!
//! M1 returns a fixed, server-chosen property set for each resource kind
//! (it does not yet parse the request body to honour an explicit
//! `<prop>` list — a widely-tolerated simplification; clients use what
//! they recognise). Honouring the requested prop list + emitting `404`
//! propstats for unknown props is a later refinement.

use std::sync::Arc;

use anyhow::Result;
use axum::response::Response;
use uuid::Uuid;

use crate::db;
use crate::jmap::AppState;

use super::href::Resource;
use super::xml;

/// PROPFIND `Depth` (we collapse `infinity` → `One`; the tree is shallow).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Zero,
    One,
}

/// Handle a PROPFIND against an (already account-bound) resource.
pub async fn handle(
    state: &Arc<AppState>,
    account: i32,
    resource: &Resource,
    depth: Depth,
) -> Response {
    let built: Result<Option<String>> = match resource {
        Resource::Root => Ok(Some(xml::root(account))),
        Resource::Principal { .. } => Ok(Some(xml::principal(account))),
        Resource::CalendarHome { .. } => calendar_home(state, account, depth).await.map(Some),
        Resource::Calendar { calendar_id, .. } => {
            single_calendar(state, account, calendar_id, depth).await
        }
        Resource::AddressbookHome { .. } => addressbook_home(state, account, depth).await.map(Some),
        Resource::Addressbook { book_id, .. } => {
            single_addressbook(state, account, book_id, depth).await
        }
        // Object-level PROPFIND needs a resource read → M2.
        Resource::CalendarObject { .. } | Resource::AddressbookObject { .. } => {
            return super::not_found();
        }
    };

    match built {
        Ok(Some(inner)) => xml::multistatus_response(xml::wrap_multistatus(&inner)),
        Ok(None) => super::not_found(),
        Err(e) => {
            tracing::warn!(target: "maild::dav", error = %e, "PROPFIND failed");
            super::server_error()
        }
    }
}

/// CTag for a collection's object type — the calendarserver getctag hint.
/// Account-wide granularity in v1 (`_plan` §7): it tracks all calendars /
/// all addressbooks of the account, so it over-invalidates but never
/// under-reports. Reuses the existing JMAP changelog.
async fn ctag(state: &Arc<AppState>, account: i32, object_type: &str) -> String {
    db::changelog::current_state(&state.db.conn, account, object_type)
        .await
        .unwrap_or_else(|_| "0".to_string())
}

async fn calendar_home(state: &Arc<AppState>, account: i32, depth: Depth) -> Result<String> {
    let mut out = xml::calendar_home_self(account);
    if depth == Depth::One {
        let cals = db::calendar::get_all(&state.db.conn, account).await?;
        let ct = ctag(state, account, "CalendarEvent").await;
        let st = super::sync_token_string(&ct);
        for c in &cals {
            out.push_str(&xml::calendar_collection(account, c, &ct, &st));
        }
    }
    Ok(out)
}

async fn single_calendar(
    state: &Arc<AppState>,
    account: i32,
    calendar_id: &str,
    depth: Depth,
) -> Result<Option<String>> {
    let Ok(uuid) = calendar_id.parse::<Uuid>() else {
        return Ok(None);
    };
    let cals = db::calendar::get_by_ids(&state.db.conn, account, &[uuid]).await?;
    let Some(cal) = cals.first() else {
        return Ok(None);
    };
    let ct = ctag(state, account, "CalendarEvent").await;
    let st = super::sync_token_string(&ct);
    let mut out = xml::calendar_collection(account, cal, &ct, &st);
    if depth == Depth::One {
        let events = db::calendar::list_events_in_calendar(
            &state.db.conn,
            account,
            uuid,
            super::MAX_COLLECTION_OBJECTS,
        )
        .await?;
        super::warn_if_capped(events.len(), "calendar PROPFIND listing");
        for ev in &events {
            let href = format!("/dav/calendars/{account}/{calendar_id}/{}.ics", ev.uid);
            let etag = cosmix_lib_davproto::etag::for_event(
                &ev.uid,
                ev.title.as_deref(),
                ev.start_dt.as_deref(),
                ev.end_dt.as_deref(),
                ev.updated_at.as_deref(),
                &ev.data,
            );
            out.push_str(&xml::object_listing(
                &href,
                &etag,
                "text/calendar; charset=utf-8",
            ));
        }
    }
    Ok(Some(out))
}

async fn addressbook_home(state: &Arc<AppState>, account: i32, depth: Depth) -> Result<String> {
    let mut out = xml::addressbook_home_self(account);
    if depth == Depth::One {
        let books = db::contact::get_all_books(&state.db.conn, account).await?;
        let ct = ctag(state, account, "Contact").await;
        let st = super::sync_token_string(&ct);
        for b in &books {
            out.push_str(&xml::addressbook_collection(account, b, &ct, &st));
        }
    }
    Ok(out)
}

async fn single_addressbook(
    state: &Arc<AppState>,
    account: i32,
    book_id: &str,
    depth: Depth,
) -> Result<Option<String>> {
    let Ok(uuid) = book_id.parse::<Uuid>() else {
        return Ok(None);
    };
    let books = db::contact::get_books_by_ids(&state.db.conn, account, &[uuid]).await?;
    let Some(book) = books.first() else {
        return Ok(None);
    };
    let ct = ctag(state, account, "Contact").await;
    let st = super::sync_token_string(&ct);
    let mut out = xml::addressbook_collection(account, book, &ct, &st);
    if depth == Depth::One {
        let contacts = db::contact::list_contacts_in_book(
            &state.db.conn,
            account,
            uuid,
            super::MAX_COLLECTION_OBJECTS,
        )
        .await?;
        super::warn_if_capped(contacts.len(), "addressbook PROPFIND listing");
        for c in &contacts {
            let href = format!("/dav/addressbooks/{account}/{book_id}/{}.vcf", c.uid);
            let etag = cosmix_lib_davproto::etag::for_contact(
                &c.uid,
                c.full_name.as_deref(),
                c.email.as_deref(),
                c.company.as_deref(),
                &c.data,
            );
            out.push_str(&xml::object_listing(
                &href,
                &etag,
                "text/vcard; charset=utf-8",
            ));
        }
    }
    Ok(Some(out))
}
