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
    let default_alerts: Option<serde_json::Value> = alerts_json
        .and_then(|s| serde_json::from_str(&s).ok());

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

const CAL_COLUMNS: &str = "id, account_id, name, color, description, is_visible, default_alerts, timezone, sort_order";
const EVENT_COLUMNS: &str = "id, calendar_id, account_id, uid, data, title, start_dt, end_dt, updated_at";

// -- Calendar CRUD --

pub async fn get_all(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Vec<Calendar>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {CAL_COLUMNS} FROM calendars WHERE account_id = ?1 ORDER BY sort_order, name"
        ))?;
        let rows = stmt.query_map(params![account_id], row_to_calendar)?;
        let mut cals = Vec::new();
        for row in rows {
            cals.push(row?);
        }
        Ok(cals)
    }).await?
}

pub async fn get_by_ids(conn: &Arc<Mutex<Connection>>, account_id: i32, ids: &[Uuid]) -> Result<Vec<Calendar>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 2)).collect();
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
        let refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_calendar)?;
        let mut cals = Vec::new();
        for row in rows {
            cals.push(row?);
        }
        Ok(cals)
    }).await?
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
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO calendars (id, account_id, name, color, description, timezone) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id_str, account_id, name, color, description, timezone],
        )?;
        Ok(id)
    }).await?
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
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
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
    }).await?
}

pub async fn delete_calendar(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "DELETE FROM calendars WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        Ok(changes > 0)
    }).await?
}

pub async fn query_calendar_ids(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Vec<Uuid>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id FROM calendars WHERE account_id = ?1 ORDER BY sort_order, name"
        )?;
        let rows = stmt.query_map(params![account_id], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?.parse::<Uuid>()?);
        }
        Ok(ids)
    }).await?
}

// -- CalendarEvent CRUD --

pub async fn get_events_by_ids(conn: &Arc<Mutex<Connection>>, account_id: i32, ids: &[Uuid]) -> Result<Vec<CalendarEvent>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 2)).collect();
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
        let refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_event)?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }).await?
}

pub async fn get_all_events(conn: &Arc<Mutex<Connection>>, account_id: i32, limit: i64) -> Result<Vec<CalendarEvent>> {
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

        // Select query with offset/limit
        let offset_idx = param_values.len() + 1;
        let limit_idx = param_values.len() + 2;
        let select_sql = format!(
            "SELECT id FROM calendar_events WHERE {where_clause} ORDER BY start_dt OFFSET ?{offset_idx} LIMIT ?{limit_idx}"
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

pub async fn create_event(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    calendar_id: Uuid,
    uid: &str,
    data: &serde_json::Value,
    title: Option<&str>,
    start_dt: Option<chrono::DateTime<chrono::Utc>>,
    end_dt: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Uuid> {
    let conn = conn.clone();
    let calendar_id_str = calendar_id.to_string();
    let uid = uid.to_string();
    let data_json = serde_json::to_string(data)?;
    let title = title.map(|s| s.to_string());
    let start_str = start_dt.map(|d| d.to_rfc3339());
    let end_str = end_dt.map(|d| d.to_rfc3339());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
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
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "UPDATE calendar_events SET data = ?3, title = ?4, start_dt = ?5, end_dt = ?6, \
             updated_at = ?7 WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str, data_json, title, start_str, end_str, now],
        )?;
        Ok(changes > 0)
    }).await?
}

pub async fn delete_event(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "DELETE FROM calendar_events WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        Ok(changes > 0)
    }).await?
}
