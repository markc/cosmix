//! Outbound SMTP queue — SQLite-backed message queue.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use uuid::Uuid;

#[derive(Debug)]
pub struct QueueEntry {
    pub id: i64,
    pub from_addr: String,
    pub to_addrs: Vec<String>,
    pub blob_id: Uuid,
    pub attempts: i32,
    pub next_retry: String,
    pub last_error: Option<String>,
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEntry> {
    let to_addrs_json: String = row.get(2)?;
    let to_addrs: Vec<String> = serde_json::from_str(&to_addrs_json).unwrap_or_default();
    let blob_id_str: String = row.get(3)?;
    let blob_id: Uuid = blob_id_str.parse().unwrap_or_else(|_| Uuid::nil());

    Ok(QueueEntry {
        id: row.get(0)?,
        from_addr: row.get(1)?,
        to_addrs,
        blob_id,
        attempts: row.get(4)?,
        next_retry: row.get(5)?,
        last_error: row.get(6)?,
    })
}

/// Enqueue a message for outbound delivery.
pub async fn enqueue(
    conn: &Arc<Mutex<Connection>>,
    from_addr: &str,
    to_addrs: &[String],
    blob_id: Uuid,
) -> Result<i64> {
    let conn = conn.clone();
    let from_addr = from_addr.to_string();
    let to_addrs_json = serde_json::to_string(to_addrs)?;
    let blob_id_str = blob_id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute(
            "INSERT INTO smtp_queue (from_addr, to_addrs, blob_id) VALUES (?1, ?2, ?3)",
            params![from_addr, to_addrs_json, blob_id_str],
        )?;
        let id = conn.last_insert_rowid();
        tracing::info!(queue_id = id, from = from_addr.as_str(), "Message queued for delivery");
        Ok(id)
    }).await?
}

/// Fetch messages ready for delivery (next_retry <= now, attempts < 10).
pub async fn fetch_ready(conn: &Arc<Mutex<Connection>>, limit: i64) -> Result<Vec<QueueEntry>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, from_addr, to_addrs, blob_id, attempts, next_retry, last_error \
             FROM smtp_queue WHERE next_retry <= datetime('now') AND attempts < 10 \
             ORDER BY next_retry LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit], row_to_entry)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }).await?
}

/// Mark a queue entry as successfully delivered (delete it).
pub async fn mark_delivered(conn: &Arc<Mutex<Connection>>, id: i64) -> Result<()> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute("DELETE FROM smtp_queue WHERE id = ?1", params![id])?;
        Ok(())
    }).await?
}

/// Mark a queue entry as failed — increment attempts, set next retry with backoff.
pub async fn mark_failed(conn: &Arc<Mutex<Connection>>, id: i64, error: &str) -> Result<()> {
    let conn = conn.clone();
    let error = error.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // Exponential backoff: multiply by power of 3
        // SQLite doesn't have POWER, so we compute the interval in Rust
        let attempts: i32 = conn.query_row(
            "SELECT attempts FROM smtp_queue WHERE id = ?1",
            params![id],
            |row| row.get(0),
        ).unwrap_or(0);
        let backoff_minutes = 3i64.pow(attempts.min(8) as u32);
        conn.execute(
            "UPDATE smtp_queue SET \
             attempts = attempts + 1, \
             last_error = ?2, \
             next_retry = datetime('now', '+' || ?3 || ' minutes') \
             WHERE id = ?1",
            params![id, error, backoff_minutes],
        )?;
        Ok(())
    }).await?
}

/// Mark a permanently failed entry (attempts >= 10) — remove from queue.
pub async fn mark_permanent_failure(conn: &Arc<Mutex<Connection>>, id: i64, error: &str) -> Result<()> {
    tracing::error!(queue_id = id, error = error, "Permanent delivery failure — removing from queue");
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute("DELETE FROM smtp_queue WHERE id = ?1", params![id])?;
        Ok(())
    }).await?
}

/// List queue entries for admin inspection.
pub async fn list(conn: &Arc<Mutex<Connection>>, limit: i64) -> Result<Vec<QueueEntry>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, from_addr, to_addrs, blob_id, attempts, next_retry, last_error \
             FROM smtp_queue ORDER BY id LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit], row_to_entry)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }).await?
}

/// Flush (retry now) all queued messages.
pub async fn flush(conn: &Arc<Mutex<Connection>>) -> Result<u64> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "UPDATE smtp_queue SET next_retry = datetime('now') WHERE attempts < 10",
            [],
        )?;
        Ok(changes as u64)
    }).await?
}
