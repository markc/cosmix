//! Calendar and event storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Calendar {
    pub id: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub account_id: i32,
    pub name: String,
    pub color: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "isVisible")]
    pub is_visible: bool,
    #[serde(rename = "defaultAlerts", skip_serializing_if = "Option::is_none")]
    pub default_alerts: Option<serde_json::Value>,
    pub timezone: Option<String>,
    #[serde(rename = "sortOrder")]
    pub sort_order: i32,
}

#[derive(Debug, Serialize)]
pub struct CalendarEvent {
    pub id: String,
    #[serde(rename = "calendarId")]
    pub calendar_id: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub account_id: i32,
    pub uid: String,
    /// Full JSCalendar Event object
    pub data: serde_json::Value,
    pub title: Option<String>,
    #[serde(rename = "start")]
    pub start_dt: Option<String>,
    #[serde(rename = "end")]
    pub end_dt: Option<String>,
    #[serde(rename = "updated")]
    pub updated_at: Option<String>,
}

fn row_to_calendar(row: &rusqlite::Row<'_>) -> rusqlite::Result<Calendar> {
    let alerts_json: Option<String> = row.get(6)?;
    let default_alerts: Option<serde_json::Value> =
        alerts_json.and_then(|s| serde_json::from_str(&s).ok());

    Ok(Calendar {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        color: row.get(3)?,
        description: row.get(4)?,
        is_visible: row.get::<_, i32>(5)? != 0,
        default_alerts,
        timezone: row.get(7)?,
        sort_order: row.get(8)?,
    })
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarEvent> {
    let data_json: String = row.get(4)?;
    let data: serde_json::Value = serde_json::from_str(&data_json).unwrap_or(serde_json::json!({}));

    Ok(CalendarEvent {
        id: row.get(0)?,
        calendar_id: row.get(1)?,
        account_id: row.get(2)?,
        uid: row.get(3)?,
        data,
        title: row.get(5)?,
        start_dt: row.get(6)?,
        end_dt: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

const CAL_COLUMNS: &str =
    "id, account_id, name, color, description, is_visible, default_alerts, timezone, sort_order";
const EVENT_COLUMNS: &str =
    "id, calendar_id, account_id, uid, data, title, start_dt, end_dt, updated_at";

// -- Calendar CRUD --

pub async fn get_all(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Vec<Calendar>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {CAL_COLUMNS} FROM calendars WHERE account_id = ?1 ORDER BY sort_order, name"
        ))?;
        let rows = stmt.query_map(params![account_id], row_to_calendar)?;
        let mut cals = Vec::new();
        for row in rows {
            cals.push(row?);
        }
        Ok(cals)
    })
    .await?
}

pub async fn get_by_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    ids: &[Uuid],
) -> Result<Vec<Calendar>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect();
        let sql = format!(
            "SELECT {CAL_COLUMNS} FROM calendars WHERE account_id = ?1 AND id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));
        for id in &ids {
            param_values.push(Box::new(id.clone()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_calendar)?;
        let mut cals = Vec::new();
        for row in rows {
            cals.push(row?);
        }
        Ok(cals)
    })
    .await?
}

pub async fn create_calendar(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    name: &str,
    color: Option<&str>,
    description: Option<&str>,
    timezone: Option<&str>,
) -> Result<Uuid> {
    let conn = conn.clone();
    let name = name.to_string();
    let color = color.map(|s| s.to_string());
    let description = description.map(|s| s.to_string());
    let timezone = timezone.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO calendars (id, account_id, name, color, description, timezone) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id_str, account_id, name, color, description, timezone],
        )?;
        Ok(id)
    })
    .await?
}

pub async fn update_calendar(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
    patch: &serde_json::Value,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    let patch = patch.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut sets = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        values.push(Box::new(account_id));
        values.push(Box::new(id_str));

        for key in &["name", "color", "description", "timezone"] {
            if let Some(v) = patch.get(key).and_then(|v| v.as_str()) {
                sets.push(format!("{key} = ?{}", values.len() + 1));
                values.push(Box::new(v.to_string()));
            }
        }
        if let Some(v) = patch.get("isVisible").and_then(|v| v.as_bool()) {
            sets.push(format!("is_visible = ?{}", values.len() + 1));
            values.push(Box::new(if v { 1i32 } else { 0 }));
        }
        if let Some(v) = patch.get("sortOrder").and_then(|v| v.as_i64()) {
            sets.push(format!("sort_order = ?{}", values.len() + 1));
            values.push(Box::new(v as i32));
        }

        if sets.is_empty() {
            return Ok(true);
        }

        let sql = format!(
            "UPDATE calendars SET {} WHERE account_id = ?1 AND id = ?2",
            sets.join(", ")
        );
        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let changes = conn.execute(&sql, refs.as_slice())?;
        Ok(changes > 0)
    })
    .await?
}

pub async fn delete_calendar(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "DELETE FROM calendars WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        Ok(changes > 0)
    })
    .await?
}

pub async fn query_calendar_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
) -> Result<Vec<Uuid>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT id FROM calendars WHERE account_id = ?1 ORDER BY sort_order, name")?;
        let rows = stmt.query_map(params![account_id], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?.parse::<Uuid>()?);
        }
        Ok(ids)
    })
    .await?
}

// -- CalendarEvent CRUD --

/// Fetch one event by its DAV identity `(account, calendar, uid)`. The
/// `UNIQUE(calendar_id, uid)` constraint guarantees at most one row, so a
/// DAV `{uid}.ics` GET resolves deterministically.
pub async fn get_event_by_uid(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    calendar_id: Uuid,
    uid: &str,
) -> Result<Option<CalendarEvent>> {
    let conn = conn.clone();
    let calendar_id = calendar_id.to_string();
    let uid = uid.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM calendar_events \
             WHERE account_id = ?1 AND calendar_id = ?2 AND uid = ?3"
        ))?;
        let mut rows = stmt.query_map(params![account_id, calendar_id, uid], row_to_event)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    })
    .await?
}

/// All events in one calendar (DAV collection listing + calendar-query),
/// bounded by `limit` (a DoS backstop — the caller logs if it's hit).
pub async fn list_events_in_calendar(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    calendar_id: Uuid,
    limit: i64,
) -> Result<Vec<CalendarEvent>> {
    let conn = conn.clone();
    let calendar_id = calendar_id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM calendar_events \
             WHERE account_id = ?1 AND calendar_id = ?2 ORDER BY start_dt LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![account_id, calendar_id, limit], row_to_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await?
}

/// Atomically (under one DB lock) verify the collection, evaluate the DAV
/// precondition against the current row's ETag, and insert-or-update the
/// event by its DAV identity `(account, calendar, uid)`. Folding the
/// read-check-write into a single lock closes the TOCTOU window two
/// concurrent conditional PUTs would otherwise have. Indexed columns come
/// from `meta` (derived from the same parse as `data`).
pub async fn conditional_upsert_event_by_uid(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    calendar_id: Uuid,
    uid: &str,
    data: &serde_json::Value,
    meta: EventMetadata<'_>,
    precond: &crate::db::DavPrecondition,
) -> Result<crate::db::UpsertOutcome> {
    use crate::db::UpsertOutcome;
    let EventMetadata {
        title,
        start_dt,
        end_dt,
    } = meta;
    let conn = conn.clone();
    let cal = calendar_id.to_string();
    let uid = uid.to_string();
    let data_json = serde_json::to_string(data)?;
    let title = title.map(|s| s.to_string());
    let start = start_dt.map(|d| d.to_rfc3339());
    let end = end_dt.map(|d| d.to_rfc3339());
    let now = chrono::Utc::now().to_rfc3339();
    let precond = precond.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        // 1. Collection must exist for THIS account (the FK alone is not
        //    account-scoped — a foreign calendar id would otherwise bind).
        let owns = conn
            .prepare("SELECT 1 FROM calendars WHERE id = ?1 AND account_id = ?2")?
            .query_map(params![cal, account_id], |_| Ok(()))?
            .next()
            .is_some();
        if !owns {
            return Ok(UpsertOutcome::CollectionMissing);
        }

        // 2. Read the existing row (if any) and compute its strong ETag —
        //    the same tuple GET hashes, so the precondition compares like
        //    for like.
        let existing = read_event_etag_row(&conn, account_id, &cal, &uid)?;
        let current_etag = existing.as_ref().map(|(_, e)| e.clone());

        // 3. Precondition (inside the lock).
        if !precond.allows(current_etag.as_deref(), existing.is_some()) {
            return Ok(UpsertOutcome::PreconditionFailed);
        }

        // 4. Write + changelog in ONE transaction so a committed write is
        //    never sync-invisible.
        let tx = conn.unchecked_transaction()?;
        let outcome = match existing {
            Some((id_str, _)) => {
                let id = id_str.parse::<Uuid>()?;
                tx.execute(
                    "UPDATE calendar_events SET data = ?4, title = ?5, start_dt = ?6, \
                     end_dt = ?7, updated_at = ?8 \
                     WHERE account_id = ?1 AND calendar_id = ?2 AND uid = ?3",
                    params![account_id, cal, uid, data_json, title, start, end, now],
                )?;
                crate::db::changelog::record_tx(&tx, account_id, "CalendarEvent", id, "updated")?;
                UpsertOutcome::Updated(id)
            }
            None => {
                let id = Uuid::new_v4();
                tx.execute(
                    "INSERT INTO calendar_events \
                     (id, account_id, calendar_id, uid, data, title, start_dt, end_dt) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        id.to_string(),
                        account_id,
                        cal,
                        uid,
                        data_json,
                        title,
                        start,
                        end
                    ],
                )?;
                crate::db::changelog::record_tx(&tx, account_id, "CalendarEvent", id, "created")?;
                UpsertOutcome::Created(id)
            }
        };
        tx.commit()?;
        Ok(outcome)
    })
    .await?
}

/// Atomically evaluate the precondition and delete an event by DAV
/// identity, returning the outcome (with the deleted id for the changelog).
pub async fn conditional_delete_event_by_uid(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    calendar_id: Uuid,
    uid: &str,
    precond: &crate::db::DavPrecondition,
) -> Result<crate::db::DeleteOutcome> {
    use crate::db::DeleteOutcome;
    let conn = conn.clone();
    let cal = calendar_id.to_string();
    let uid = uid.to_string();
    let precond = precond.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let existing = read_event_etag_row(&conn, account_id, &cal, &uid)?;
        let Some((id_str, current_etag)) = existing else {
            // Absent: an If-Match guard still applies (RFC 7232 → 412, not
            // 404), so evaluate it against the empty state first.
            return Ok(if precond.allows(None, false) {
                DeleteOutcome::NotFound
            } else {
                DeleteOutcome::PreconditionFailed
            });
        };
        if !precond.allows(Some(&current_etag), true) {
            return Ok(DeleteOutcome::PreconditionFailed);
        }
        let id = id_str.parse::<Uuid>()?;
        // Delete + tombstone in ONE transaction so a crash can't leave the
        // row gone without a tombstone (which would make sync-collection
        // silently miss the removal).
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM calendar_events WHERE account_id = ?1 AND calendar_id = ?2 AND uid = ?3",
            params![account_id, cal, uid],
        )?;
        crate::db::tombstone::insert(&tx, account_id, "CalendarEvent", id, &cal, &uid)?;
        crate::db::changelog::record_tx(&tx, account_id, "CalendarEvent", id, "destroyed")?;
        tx.commit()?;
        Ok(DeleteOutcome::Deleted(id))
    })
    .await?
}

/// Read `(id, strong_etag)` for one event under the lock, computing the
/// ETag from the same field tuple GET hashes so preconditions compare
/// like for like. `None` if the row is absent.
fn read_event_etag_row(
    conn: &Connection,
    account_id: i32,
    calendar_id: &str,
    uid: &str,
) -> Result<Option<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, start_dt, end_dt, updated_at, data FROM calendar_events \
         WHERE account_id = ?1 AND calendar_id = ?2 AND uid = ?3",
    )?;
    let mut rows = stmt.query_map(params![account_id, calendar_id, uid], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    match rows.next() {
        Some(row) => {
            let (id, title, start, end, updated, data_json) = row?;
            let data: serde_json::Value =
                serde_json::from_str(&data_json).unwrap_or(serde_json::json!({}));
            let etag = cosmix_lib_davproto::etag::for_event(
                uid,
                title.as_deref(),
                start.as_deref(),
                end.as_deref(),
                updated.as_deref(),
                &data,
            );
            Ok(Some((id, etag)))
        }
        None => Ok(None),
    }
}

pub async fn get_events_by_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    ids: &[Uuid],
) -> Result<Vec<CalendarEvent>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect();
        let sql = format!(
            "SELECT {EVENT_COLUMNS} FROM calendar_events WHERE account_id = ?1 AND id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));
        for id in &ids {
            param_values.push(Box::new(id.clone()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_event)?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    })
    .await?
}

pub async fn get_all_events(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    limit: i64,
) -> Result<Vec<CalendarEvent>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM calendar_events WHERE account_id = ?1 ORDER BY start_dt LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![account_id, limit], row_to_event)?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }).await?
}

pub async fn query_event_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    calendar_id: Option<Uuid>,
    after: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
    position: i64,
    limit: i64,
) -> Result<(Vec<Uuid>, i64)> {
    let conn = conn.clone();
    let calendar_id = calendar_id.map(|u| u.to_string());
    let after = after.map(|d| d.to_rfc3339());
    let before = before.map(|d| d.to_rfc3339());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        let mut where_parts = vec!["account_id = ?1".to_string()];
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));

        if let Some(ref cal_id) = calendar_id {
            where_parts.push(format!("calendar_id = ?{}", param_values.len() + 1));
            param_values.push(Box::new(cal_id.clone()));
        }
        if let Some(ref a) = after {
            where_parts.push(format!("end_dt >= ?{}", param_values.len() + 1));
            param_values.push(Box::new(a.clone()));
        }
        if let Some(ref b) = before {
            where_parts.push(format!("start_dt < ?{}", param_values.len() + 1));
            param_values.push(Box::new(b.clone()));
        }

        let where_clause = where_parts.join(" AND ");

        // Count query first (uses same base params)
        let count_sql = format!(
            "SELECT COUNT(*) FROM calendar_events WHERE {where_clause}"
        );
        let count_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| &**b as &dyn rusqlite::types::ToSql).collect();
        let total: i64 = conn.query_row(&count_sql, count_refs.as_slice(), |row| row.get(0))?;

        // Select query with limit/offset. SQLite syntax requires LIMIT before
        // OFFSET (`LIMIT x OFFSET y`); the reverse is a syntax error — matches
        // query_contact_ids. The placeholder indices stay bound to their pushed
        // params (offset=position, limit), so only the clause order changes.
        let offset_idx = param_values.len() + 1;
        let limit_idx = param_values.len() + 2;
        let select_sql = format!(
            "SELECT id FROM calendar_events WHERE {where_clause} ORDER BY start_dt LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
        );
        param_values.push(Box::new(position));
        param_values.push(Box::new(limit));
        let select_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| &**b as &dyn rusqlite::types::ToSql).collect();

        let mut stmt = conn.prepare(&select_sql)?;
        let rows = stmt.query_map(select_refs.as_slice(), |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?.parse::<Uuid>()?);
        }

        Ok((ids, total))
    }).await?
}

/// Indexed event metadata (extracted from JSCalendar payload).
pub struct EventMetadata<'a> {
    pub title: Option<&'a str>,
    pub start_dt: Option<chrono::DateTime<chrono::Utc>>,
    pub end_dt: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn create_event(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    calendar_id: Uuid,
    uid: &str,
    data: &serde_json::Value,
    meta: EventMetadata<'_>,
) -> Result<Uuid> {
    let EventMetadata {
        title,
        start_dt,
        end_dt,
    } = meta;
    let conn = conn.clone();
    let calendar_id_str = calendar_id.to_string();
    let uid = uid.to_string();
    let data_json = serde_json::to_string(data)?;
    let title = title.map(|s| s.to_string());
    let start_str = start_dt.map(|d| d.to_rfc3339());
    let end_str = end_dt.map(|d| d.to_rfc3339());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // The FK only checks the calendar id exists globally, not that it
        // belongs to this account — verify ownership so a caller can't
        // attach an event to another account's calendar.
        let owns = conn
            .prepare("SELECT 1 FROM calendars WHERE id = ?1 AND account_id = ?2")?
            .query_map(params![calendar_id_str, account_id], |_| Ok(()))?
            .next()
            .is_some();
        if !owns {
            return Err(anyhow::anyhow!(
                "calendar {calendar_id_str} does not belong to account {account_id}"
            ));
        }
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO calendar_events (id, account_id, calendar_id, uid, data, title, start_dt, end_dt) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id_str, account_id, calendar_id_str, uid, data_json, title, start_str, end_str],
        )?;
        Ok(id)
    }).await?
}

pub async fn update_event(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
    data: &serde_json::Value,
    title: Option<&str>,
    start_dt: Option<chrono::DateTime<chrono::Utc>>,
    end_dt: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    let data_json = serde_json::to_string(data)?;
    let title = title.map(|s| s.to_string());
    let start_str = start_dt.map(|d| d.to_rfc3339());
    let end_str = end_dt.map(|d| d.to_rfc3339());
    let now = chrono::Utc::now().to_rfc3339();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "UPDATE calendar_events SET data = ?3, title = ?4, start_dt = ?5, end_dt = ?6, \
             updated_at = ?7 WHERE account_id = ?1 AND id = ?2",
            params![
                account_id, id_str, data_json, title, start_str, end_str, now
            ],
        )?;
        Ok(changes > 0)
    })
    .await?
}

pub async fn delete_event(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // Capture (calendar_id, uid) before deleting so a tombstone can
        // be written in the same transaction — DAV sync-collection needs
        // it to report this removal even when the delete came via JMAP.
        let ids: Option<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT calendar_id, uid FROM calendar_events WHERE account_id = ?1 AND id = ?2",
            )?;
            let mut rows = stmt.query_map(params![account_id, id_str], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            match rows.next() {
                Some(r) => Some(r?),
                None => None,
            }
        };
        let tx = conn.unchecked_transaction()?;
        let changes = tx.execute(
            "DELETE FROM calendar_events WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        if changes > 0
            && let Some((calendar_id, uid)) = ids
        {
            crate::db::tombstone::insert(&tx, account_id, "CalendarEvent", id, &calendar_id, &uid)?;
        }
        tx.commit()?;
        Ok(changes > 0)
    })
    .await?
}
