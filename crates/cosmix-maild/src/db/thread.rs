//! Thread storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use uuid::Uuid;

/// A JMAP Thread object (RFC 8621 §4.1).
#[derive(Debug, Serialize)]
pub struct Thread {
    pub id: String,
    #[serde(rename = "emailIds")]
    pub email_ids: Vec<String>,
}

/// Find or create a thread for a message based on in-reply-to / message-id.
pub async fn find_or_create(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    _message_id: Option<&str>,
    in_reply_to: Option<&[String]>,
) -> Result<Uuid> {
    let conn = conn.clone();
    let in_reply_to = in_reply_to.map(|s| s.to_vec());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        // Try to find an existing thread by in-reply-to
        if let Some(refs) = &in_reply_to {
            for ref_id in refs {
                let result: rusqlite::Result<String> = conn.query_row(
                    "SELECT thread_id FROM emails WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
                    params![account_id, ref_id],
                    |row| row.get(0),
                );
                if let Ok(thread_id_str) = result {
                    return Ok(thread_id_str.parse::<Uuid>()?);
                }
            }
        }

        // Create a new thread
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO threads (id, account_id) VALUES (?1, ?2)",
            params![id_str, account_id],
        )?;
        Ok(id)
    }).await?
}

/// Get threads by IDs, returning each thread with its list of email IDs.
pub async fn get_by_ids(conn: &Arc<Mutex<Connection>>, account_id: i32, ids: &[Uuid]) -> Result<Vec<Thread>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut threads = Vec::new();
        for id in &ids {
            // Verify thread belongs to this account
            let exists: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM threads WHERE id = ?1 AND account_id = ?2",
                params![id, account_id],
                |row| row.get(0),
            ).unwrap_or(false);

            if !exists {
                continue;
            }

            // Get email IDs in this thread, ordered by received_at
            let mut stmt = conn.prepare(
                "SELECT id FROM emails WHERE thread_id = ?1 AND account_id = ?2 ORDER BY received_at ASC"
            )?;
            let email_ids: Vec<String> = stmt.query_map(params![id, account_id], |row| {
                row.get(0)
            })?.filter_map(|r| r.ok()).collect();

            threads.push(Thread {
                id: id.clone(),
                email_ids,
            });
        }
        Ok(threads)
    }).await?
}

/// Get all thread IDs for an account.
pub async fn query_ids(conn: &Arc<Mutex<Connection>>, account_id: i32, position: i64, limit: i64) -> Result<(Vec<Uuid>, i64)> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM threads WHERE account_id = ?1",
            params![account_id],
            |row| row.get(0),
        )?;

        let mut stmt = conn.prepare(
            "SELECT id FROM threads WHERE account_id = ?1 LIMIT ?2 OFFSET ?3"
        )?;
        let ids: Vec<Uuid> = stmt.query_map(params![account_id, limit, position], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?.filter_map(|r| r.ok()).filter_map(|s| s.parse().ok()).collect();

        Ok((ids, total))
    }).await?
}
