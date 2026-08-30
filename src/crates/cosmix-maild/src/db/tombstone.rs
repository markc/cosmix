//! DAV tombstones — retain `(collection_id, uid)` for deleted objects so
//! RFC 6578 sync-collection can report a removed member's href after its
//! live row is gone. Written in the same DB transaction as the delete.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use uuid::Uuid;

/// Record a tombstone for a just-deleted object. Called **inside** the
/// delete's DB lock (takes `&Connection`), so the tombstone and the row
/// removal commit atomically. `INSERT OR REPLACE` keeps the latest mapping
/// if an `object_id` were ever reused (it isn't — UUIDs are fresh — but
/// this is robust either way).
pub fn insert(
    conn: &Connection,
    account_id: i32,
    object_type: &str,
    object_id: Uuid,
    collection_id: &str,
    uid: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO dav_tombstones \
         (object_id, account_id, object_type, collection_id, uid) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            object_id.to_string(),
            account_id,
            object_type,
            collection_id,
            uid
        ],
    )?;
    Ok(())
}

/// A tombstone's retained identity.
pub struct Tombstone {
    pub object_id: String,
    pub collection_id: String,
    pub uid: String,
}

/// Look up tombstones for a set of object ids (the `destroyed` list from
/// `changelog::changes_since`). Returns those found, for href rebuilding.
pub async fn lookup(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    object_type: &str,
    ids: &[Uuid],
) -> Result<Vec<Tombstone>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let conn = conn.clone();
    let object_type = object_type.to_string();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let placeholders: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 3))
            .collect();
        let sql = format!(
            "SELECT object_id, collection_id, uid FROM dav_tombstones \
             WHERE account_id = ?1 AND object_type = ?2 AND object_id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        values.push(Box::new(account_id));
        values.push(Box::new(object_type));
        for id in &ids {
            values.push(Box::new(id.clone()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok(Tombstone {
                object_id: r.get(0)?,
                collection_id: r.get(1)?,
                uid: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await?
}

/// Build a quick `object_id -> Tombstone` index.
pub fn index(tombs: Vec<Tombstone>) -> HashMap<String, Tombstone> {
    tombs
        .into_iter()
        .map(|t| (t.object_id.clone(), t))
        .collect()
}
