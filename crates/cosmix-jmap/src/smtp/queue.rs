//! Outbound SMTP queue — PostgreSQL-backed message queue.

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
pub struct QueueEntry {
    pub id: i64,
    pub from_addr: String,
    pub to_addrs: Vec<String>,
    pub blob_id: Uuid,
    pub attempts: i32,
    pub next_retry: chrono::DateTime<chrono::Utc>,
    pub last_error: Option<String>,
}

/// Enqueue a message for outbound delivery.
pub async fn enqueue(
    pool: &PgPool,
    from_addr: &str,
    to_addrs: &[String],
    blob_id: Uuid,
) -> Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO smtp_queue (from_addr, to_addrs, blob_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(from_addr)
    .bind(to_addrs)
    .bind(blob_id)
    .fetch_one(pool)
    .await?;

    tracing::info!(queue_id = id, from = from_addr, "Message queued for delivery");
    Ok(id)
}

/// Fetch messages ready for delivery (next_retry <= now, attempts < 10).
pub async fn fetch_ready(pool: &PgPool, limit: i64) -> Result<Vec<QueueEntry>> {
    let rows = sqlx::query_as::<_, QueueEntry>(
        "SELECT id, from_addr, to_addrs, blob_id, attempts, next_retry, last_error \
         FROM smtp_queue WHERE next_retry <= NOW() AND attempts < 10 \
         ORDER BY next_retry LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Mark a queue entry as successfully delivered (delete it).
pub async fn mark_delivered(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM smtp_queue WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark a queue entry as failed — increment attempts, set next retry with backoff.
pub async fn mark_failed(pool: &PgPool, id: i64, error: &str) -> Result<()> {
    // Exponential backoff: 1m, 5m, 30m, 2h, 8h, 24h, 48h, 72h, 96h, fail
    sqlx::query(
        "UPDATE smtp_queue SET \
         attempts = attempts + 1, \
         last_error = $2, \
         next_retry = NOW() + (INTERVAL '1 minute' * POWER(3, LEAST(attempts, 8))) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a permanently failed entry (attempts >= 10) — remove from queue.
pub async fn mark_permanent_failure(pool: &PgPool, id: i64, error: &str) -> Result<()> {
    tracing::error!(queue_id = id, error = error, "Permanent delivery failure — removing from queue");
    sqlx::query("DELETE FROM smtp_queue WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// List queue entries for admin inspection.
pub async fn list(pool: &PgPool, limit: i64) -> Result<Vec<QueueEntry>> {
    let rows = sqlx::query_as::<_, QueueEntry>(
        "SELECT id, from_addr, to_addrs, blob_id, attempts, next_retry, last_error \
         FROM smtp_queue ORDER BY id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Flush (retry now) all queued messages.
pub async fn flush(pool: &PgPool) -> Result<u64> {
    let result = sqlx::query("UPDATE smtp_queue SET next_retry = NOW() WHERE attempts < 10")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
