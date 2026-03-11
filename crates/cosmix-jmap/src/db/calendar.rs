//! Calendar and event storage operations.

use anyhow::Result;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Calendar {
    pub id: Uuid,
    #[serde(skip)]
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

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CalendarEvent {
    pub id: Uuid,
    #[serde(rename = "calendarId")]
    pub calendar_id: Uuid,
    #[serde(skip)]
    pub account_id: i32,
    pub uid: String,
    /// Full JSCalendar Event object
    pub data: serde_json::Value,
    pub title: Option<String>,
    #[serde(rename = "start")]
    pub start_dt: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(rename = "end")]
    pub end_dt: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(rename = "updated")]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ── Calendar CRUD ──

pub async fn get_all(pool: &PgPool, account_id: i32) -> Result<Vec<Calendar>> {
    let rows = sqlx::query_as::<_, Calendar>(
        "SELECT id, account_id, name, color, description, is_visible, default_alerts, \
         timezone, sort_order FROM calendars WHERE account_id = $1 ORDER BY sort_order, name",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_by_ids(pool: &PgPool, account_id: i32, ids: &[Uuid]) -> Result<Vec<Calendar>> {
    let rows = sqlx::query_as::<_, Calendar>(
        "SELECT id, account_id, name, color, description, is_visible, default_alerts, \
         timezone, sort_order FROM calendars WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn create_calendar(
    pool: &PgPool,
    account_id: i32,
    name: &str,
    color: Option<&str>,
    description: Option<&str>,
    timezone: Option<&str>,
) -> Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO calendars (account_id, name, color, description, timezone) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(account_id)
    .bind(name)
    .bind(color)
    .bind(description)
    .bind(timezone)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn update_calendar(
    pool: &PgPool,
    account_id: i32,
    id: Uuid,
    patch: &serde_json::Value,
) -> Result<bool> {
    let mut sets = Vec::new();
    let mut idx = 2u32;
    let mut binds: Vec<String> = Vec::new();

    for (key, val) in [
        ("name", patch.get("name")),
        ("color", patch.get("color")),
        ("description", patch.get("description")),
        ("timezone", patch.get("timezone")),
    ] {
        if let Some(v) = val {
            idx += 1;
            sets.push(format!("{key} = ${idx}"));
            binds.push(v.as_str().unwrap_or("").to_string());
        }
    }
    if let Some(v) = patch.get("isVisible") {
        idx += 1;
        sets.push(format!("is_visible = ${idx}"));
        binds.push(v.as_bool().unwrap_or(true).to_string());
    }
    if let Some(v) = patch.get("sortOrder") {
        idx += 1;
        sets.push(format!("sort_order = ${idx}"));
        binds.push(v.as_i64().unwrap_or(0).to_string());
    }

    if sets.is_empty() {
        return Ok(true);
    }

    let sql = format!(
        "UPDATE calendars SET {} WHERE account_id = $1 AND id = $2",
        sets.join(", ")
    );
    let mut query = sqlx::query(&sql).bind(account_id).bind(id);
    for b in &binds {
        query = query.bind(b);
    }
    let result = query.execute(pool).await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_calendar(pool: &PgPool, account_id: i32, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM calendars WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn query_calendar_ids(pool: &PgPool, account_id: i32) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM calendars WHERE account_id = $1 ORDER BY sort_order, name",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

// ── CalendarEvent CRUD ──

pub async fn get_events_by_ids(pool: &PgPool, account_id: i32, ids: &[Uuid]) -> Result<Vec<CalendarEvent>> {
    let rows = sqlx::query_as::<_, CalendarEvent>(
        "SELECT id, calendar_id, account_id, uid, data, title, start_dt, end_dt, updated_at \
         FROM calendar_events WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_all_events(pool: &PgPool, account_id: i32, limit: i64) -> Result<Vec<CalendarEvent>> {
    let rows = sqlx::query_as::<_, CalendarEvent>(
        "SELECT id, calendar_id, account_id, uid, data, title, start_dt, end_dt, updated_at \
         FROM calendar_events WHERE account_id = $1 ORDER BY start_dt LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn query_event_ids(
    pool: &PgPool,
    account_id: i32,
    calendar_id: Option<Uuid>,
    after: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
    position: i64,
    limit: i64,
) -> Result<(Vec<Uuid>, i64)> {
    // Build dynamic WHERE clause
    let mut where_parts = vec!["account_id = $1".to_string()];
    let mut param_idx = 1u32;

    if calendar_id.is_some() {
        param_idx += 1;
        where_parts.push(format!("calendar_id = ${param_idx}"));
    }
    if after.is_some() {
        param_idx += 1;
        where_parts.push(format!("end_dt >= ${param_idx}"));
    }
    if before.is_some() {
        param_idx += 1;
        where_parts.push(format!("start_dt < ${param_idx}"));
    }

    let where_clause = where_parts.join(" AND ");
    let offset_idx = param_idx + 1;
    let limit_idx = param_idx + 2;

    let select_sql = format!(
        "SELECT id FROM calendar_events WHERE {where_clause} ORDER BY start_dt OFFSET ${offset_idx} LIMIT ${limit_idx}"
    );
    let count_sql = format!(
        "SELECT COUNT(*) FROM calendar_events WHERE {where_clause}"
    );

    let mut select_q = sqlx::query_as::<_, (Uuid,)>(&select_sql).bind(account_id);
    let mut count_q = sqlx::query_as::<_, (i64,)>(&count_sql).bind(account_id);

    if let Some(cal_id) = calendar_id {
        select_q = select_q.bind(cal_id);
        count_q = count_q.bind(cal_id);
    }
    if let Some(a) = after {
        select_q = select_q.bind(a);
        count_q = count_q.bind(a);
    }
    if let Some(b) = before {
        select_q = select_q.bind(b);
        count_q = count_q.bind(b);
    }

    select_q = select_q.bind(position).bind(limit);

    let ids: Vec<(Uuid,)> = select_q.fetch_all(pool).await?;
    let total: (i64,) = count_q.fetch_one(pool).await?;

    Ok((ids.into_iter().map(|r| r.0).collect(), total.0))
}

pub async fn create_event(
    pool: &PgPool,
    account_id: i32,
    calendar_id: Uuid,
    uid: &str,
    data: &serde_json::Value,
    title: Option<&str>,
    start_dt: Option<chrono::DateTime<chrono::Utc>>,
    end_dt: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO calendar_events (account_id, calendar_id, uid, data, title, start_dt, end_dt) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(account_id)
    .bind(calendar_id)
    .bind(uid)
    .bind(data)
    .bind(title)
    .bind(start_dt)
    .bind(end_dt)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn update_event(
    pool: &PgPool,
    account_id: i32,
    id: Uuid,
    data: &serde_json::Value,
    title: Option<&str>,
    start_dt: Option<chrono::DateTime<chrono::Utc>>,
    end_dt: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE calendar_events SET data = $3, title = $4, start_dt = $5, end_dt = $6, \
         updated_at = NOW() WHERE account_id = $1 AND id = $2",
    )
    .bind(account_id)
    .bind(id)
    .bind(data)
    .bind(title)
    .bind(start_dt)
    .bind(end_dt)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_event(pool: &PgPool, account_id: i32, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM calendar_events WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
