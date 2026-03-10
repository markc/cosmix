//! Mailbox storage operations.

use anyhow::Result;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Mailbox {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: i32,
    pub name: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<Uuid>,
    pub role: Option<String>,
    #[serde(rename = "sortOrder")]
    pub sort_order: i32,
}

pub async fn get_by_ids(pool: &PgPool, account_id: i32, ids: &[Uuid]) -> Result<Vec<Mailbox>> {
    let rows = sqlx::query_as::<_, Mailbox>(
        "SELECT id, account_id, name, parent_id, role, sort_order \
         FROM mailboxes WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_all(pool: &PgPool, account_id: i32) -> Result<Vec<Mailbox>> {
    let rows = sqlx::query_as::<_, Mailbox>(
        "SELECT id, account_id, name, parent_id, role, sort_order \
         FROM mailboxes WHERE account_id = $1 ORDER BY sort_order, name",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn create(
    pool: &PgPool,
    account_id: i32,
    name: &str,
    parent_id: Option<Uuid>,
    role: Option<&str>,
) -> Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO mailboxes (account_id, name, parent_id, role) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(account_id)
    .bind(name)
    .bind(parent_id)
    .bind(role)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn update(pool: &PgPool, account_id: i32, id: Uuid, name: Option<&str>, parent_id: Option<Option<Uuid>>, sort_order: Option<i32>) -> Result<bool> {
    // Build dynamic update
    let mut sets = Vec::new();
    let mut param_idx = 2u32;

    if name.is_some() {
        param_idx += 1;
        sets.push(format!("name = ${param_idx}"));
    }
    if parent_id.is_some() {
        param_idx += 1;
        sets.push(format!("parent_id = ${param_idx}"));
    }
    if sort_order.is_some() {
        param_idx += 1;
        sets.push(format!("sort_order = ${param_idx}"));
    }

    if sets.is_empty() {
        return Ok(true);
    }

    let sql = format!(
        "UPDATE mailboxes SET {} WHERE account_id = $1 AND id = $2",
        sets.join(", ")
    );

    let mut query = sqlx::query(&sql).bind(account_id).bind(id);
    // Unused but keeps the param count valid
    let _ = param_idx;

    if let Some(n) = name {
        query = query.bind(n);
    }
    if let Some(p) = parent_id {
        query = query.bind(p);
    }
    if let Some(s) = sort_order {
        query = query.bind(s);
    }

    let result = query.execute(pool).await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete(pool: &PgPool, account_id: i32, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM mailboxes WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Get the inbox mailbox ID for an account.
pub async fn get_inbox(pool: &PgPool, account_id: i32) -> Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "SELECT id FROM mailboxes WHERE account_id = $1 AND role = 'inbox' LIMIT 1",
    )
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Get a mailbox by role.
pub async fn get_by_role(pool: &PgPool, account_id: i32, role: &str) -> Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM mailboxes WHERE account_id = $1 AND role = $2 LIMIT 1",
    )
    .bind(account_id)
    .bind(role)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

pub async fn query_ids(pool: &PgPool, account_id: i32) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM mailboxes WHERE account_id = $1 ORDER BY sort_order, name",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}
