//! Email storage operations.

use anyhow::Result;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Email {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: i32,
    #[serde(rename = "threadId")]
    pub thread_id: Uuid,
    #[serde(rename = "mailboxIds")]
    pub mailbox_ids: Vec<Uuid>,
    #[serde(rename = "blobId")]
    pub blob_id: Uuid,
    pub size: i32,
    #[serde(rename = "receivedAt")]
    pub received_at: chrono::DateTime<chrono::Utc>,
    #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(rename = "inReplyTo", skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Vec<String>>,
    pub subject: Option<String>,
    #[serde(rename = "from", skip_serializing_if = "Option::is_none")]
    pub from_addr: Option<serde_json::Value>,
    #[serde(rename = "to", skip_serializing_if = "Option::is_none")]
    pub to_addr: Option<serde_json::Value>,
    #[serde(rename = "cc", skip_serializing_if = "Option::is_none")]
    pub cc_addr: Option<serde_json::Value>,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub preview: Option<String>,
    #[serde(rename = "hasAttachment")]
    pub has_attachment: bool,
    pub keywords: serde_json::Value,
}

pub async fn get_by_ids(pool: &PgPool, account_id: i32, ids: &[Uuid]) -> Result<Vec<Email>> {
    let rows = sqlx::query_as::<_, Email>(
        "SELECT id, account_id, thread_id, mailbox_ids, blob_id, size, received_at, \
         message_id, in_reply_to, subject, from_addr, to_addr, cc_addr, date, \
         preview, has_attachment, keywords \
         FROM emails WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn query_ids(
    pool: &PgPool,
    account_id: i32,
    mailbox_id: Option<Uuid>,
    sort_desc: bool,
    position: i64,
    limit: i64,
) -> Result<(Vec<Uuid>, i64)> {
    let (where_clause, count_clause) = if let Some(_mb_id) = mailbox_id {
        (
            format!(
                "WHERE account_id = $1 AND $2 = ANY(mailbox_ids) ORDER BY received_at {} OFFSET $3 LIMIT $4",
                if sort_desc { "DESC" } else { "ASC" }
            ),
            "SELECT COUNT(*) FROM emails WHERE account_id = $1 AND $2 = ANY(mailbox_ids)".to_string(),
        )
    } else {
        (
            format!(
                "WHERE account_id = $1 ORDER BY received_at {} OFFSET $2 LIMIT $3",
                if sort_desc { "DESC" } else { "ASC" }
            ),
            "SELECT COUNT(*) FROM emails WHERE account_id = $1".to_string(),
        )
    };

    let sql = format!("SELECT id FROM emails {where_clause}");

    let ids: Vec<(Uuid,)> = if let Some(mb_id) = mailbox_id {
        sqlx::query_as(&sql)
            .bind(account_id)
            .bind(mb_id)
            .bind(position)
            .bind(limit)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_as(&sql)
            .bind(account_id)
            .bind(position)
            .bind(limit)
            .fetch_all(pool)
            .await?
    };

    let total: (i64,) = if let Some(mb_id) = mailbox_id {
        sqlx::query_as(&count_clause)
            .bind(account_id)
            .bind(mb_id)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_as(&count_clause)
            .bind(account_id)
            .fetch_one(pool)
            .await?
    };

    Ok((ids.into_iter().map(|r| r.0).collect(), total.0))
}

pub async fn update_keywords(pool: &PgPool, account_id: i32, id: Uuid, keywords: &serde_json::Value) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE emails SET keywords = $3 WHERE account_id = $1 AND id = $2",
    )
    .bind(account_id)
    .bind(id)
    .bind(keywords)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn update_mailboxes(pool: &PgPool, account_id: i32, id: Uuid, mailbox_ids: &[Uuid]) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE emails SET mailbox_ids = $3 WHERE account_id = $1 AND id = $2",
    )
    .bind(account_id)
    .bind(id)
    .bind(mailbox_ids)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Create an email record (used by inbound delivery).
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    account_id: i32,
    thread_id: Uuid,
    mailbox_ids: &[Uuid],
    blob_id: Uuid,
    size: i32,
    message_id: Option<&str>,
    in_reply_to: Option<&[String]>,
    subject: Option<&str>,
    from_addr: Option<&serde_json::Value>,
    to_addr: Option<&serde_json::Value>,
    cc_addr: Option<&serde_json::Value>,
    date: Option<chrono::DateTime<chrono::Utc>>,
    preview: Option<&str>,
    has_attachment: bool,
) -> Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO emails (account_id, thread_id, mailbox_ids, blob_id, size, received_at, \
         message_id, in_reply_to, subject, from_addr, to_addr, cc_addr, date, preview, has_attachment) \
         VALUES ($1, $2, $3, $4, $5, NOW(), $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
    )
    .bind(account_id)
    .bind(thread_id)
    .bind(mailbox_ids)
    .bind(blob_id)
    .bind(size)
    .bind(message_id)
    .bind(in_reply_to)
    .bind(subject)
    .bind(from_addr)
    .bind(to_addr)
    .bind(cc_addr)
    .bind(date)
    .bind(preview)
    .bind(has_attachment)
    .fetch_one(pool)
    .await?;

    // Record in changelog
    crate::db::changelog::record(pool, account_id, "Email", id, "created").await?;

    Ok(id)
}

pub async fn delete(pool: &PgPool, account_id: i32, id: Uuid) -> Result<bool> {
    let result = sqlx::query("DELETE FROM emails WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
