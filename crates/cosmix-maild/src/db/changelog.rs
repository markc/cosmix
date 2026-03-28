//! JMAP change tracking (modification sequences).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use uuid::Uuid;

/// Record a change and return the new state (modseq).
pub async fn record(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    object_type: &str,
    object_id: Uuid,
    change_type: &str,
) -> Result<i64> {
    let conn = conn.clone();
    let object_type = object_type.to_string();
    let object_id_str = object_id.to_string();
    let change_type = change_type.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute(
            "INSERT INTO changelog (account_id, object_type, object_id, change_type) \
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, object_type, object_id_str, change_type],
        )?;
        let id = conn.last_insert_rowid();
        Ok(id)
    }).await?
}

/// Get the current state (highest modseq) for an object type.
pub async fn current_state(conn: &Arc<Mutex<Connection>>, account_id: i32, object_type: &str) -> Result<String> {
    let conn = conn.clone();
    let object_type = object_type.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id: i64 = conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM changelog WHERE account_id = ?1 AND object_type = ?2",
            params![account_id, object_type],
            |row| row.get(0),
        )?;
        Ok(id.to_string())
    }).await?
}

/// Get changes since a given state.
pub async fn changes_since(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    object_type: &str,
    since_state: i64,
    max_changes: i64,
) -> Result<ChangesResult> {
    let conn = conn.clone();
    let object_type = object_type.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT object_id, change_type, id FROM changelog \
             WHERE account_id = ?1 AND object_type = ?2 AND id > ?3 \
             ORDER BY id LIMIT ?4"
        )?;
        let rows = stmt.query_map(
            params![account_id, object_type, since_state, max_changes + 1],
            |row| {
                Ok(ChangeRow {
                    object_id: row.get(0)?,
                    change_type: row.get(1)?,
                    id: row.get(2)?,
                })
            },
        )?;

        let all_rows: Vec<ChangeRow> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = all_rows.len() as i64 > max_changes;
        let rows: Vec<ChangeRow> = all_rows.into_iter().take(max_changes as usize).collect();
        let new_state = rows.last().map(|r| r.id).unwrap_or(since_state);

        // Deduplicate
        use std::collections::HashMap;
        let mut seen: HashMap<String, String> = HashMap::new();
        for row in &rows {
            seen.insert(row.object_id.clone(), row.change_type.clone());
        }

        let mut created = Vec::new();
        let mut updated = Vec::new();
        let mut destroyed = Vec::new();

        for (id, change) in seen {
            match change.as_str() {
                "created" => created.push(id),
                "updated" => updated.push(id),
                "destroyed" => destroyed.push(id),
                _ => {}
            }
        }

        Ok(ChangesResult {
            new_state: new_state.to_string(),
            has_more_changes: has_more,
            created,
            updated,
            destroyed,
        })
    }).await?
}

struct ChangeRow {
    object_id: String,
    change_type: String,
    id: i64,
}

pub struct ChangesResult {
    pub new_state: String,
    pub has_more_changes: bool,
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
}
