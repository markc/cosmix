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
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute(
            "INSERT INTO changelog (account_id, object_type, object_id, change_type) \
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, object_type, object_id_str, change_type],
        )?;
        let id = conn.last_insert_rowid();
        Ok(id)
    })
    .await?
}

/// Synchronous changelog insert on an existing connection/transaction —
/// for callers (the DAV write path) that must record the change in the
/// **same transaction** as the row mutation + tombstone, so a committed
/// write is never sync-invisible. Returns the new rowid (state).
pub fn record_tx(
    conn: &Connection,
    account_id: i32,
    object_type: &str,
    object_id: Uuid,
    change_type: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO changelog (account_id, object_type, object_id, change_type) \
         VALUES (?1, ?2, ?3, ?4)",
        params![account_id, object_type, object_id.to_string(), change_type],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Get the current state (highest modseq) for an object type.
pub async fn current_state(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    object_type: &str,
) -> Result<String> {
    let conn = conn.clone();
    let object_type = object_type.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id: i64 = conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM changelog WHERE account_id = ?1 AND object_type = ?2",
            params![account_id, object_type],
            |row| row.get(0),
        )?;
        Ok(id.to_string())
    })
    .await?
}

/// Per RFC 8620 §5.2, a `sinceState` is valid only if it was a
/// state previously emitted by the server. The legacy changelog
/// allocates ROWIDs globally across object types, so a token
/// `<= current_state` is *not* automatically a valid prior
/// `(account, object_type)` state — a client that passes a token
/// belonging to a sibling object type would land on `id > since`
/// and silently skip every Mailbox row whose id is `<= since`. The
/// only honest check is to confirm the token is either 0 (initial
/// sync) or matches an actual changelog row for this exact
/// `(account, object_type)`.
pub async fn is_valid_state(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    object_type: &str,
    since_state: i64,
) -> Result<bool> {
    if since_state == 0 {
        return Ok(true);
    }
    if since_state < 0 {
        return Ok(false);
    }
    let conn = conn.clone();
    let object_type = object_type.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let exists: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM changelog \
             WHERE account_id = ?1 AND object_type = ?2 AND id = ?3)",
            params![account_id, object_type, since_state],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    })
    .await?
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
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT object_id, change_type, id FROM changelog \
             WHERE account_id = ?1 AND object_type = ?2 AND id > ?3 \
             ORDER BY id LIMIT ?4",
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
    })
    .await?
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};

    fn open_changelog() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE changelog (\
                id INTEGER PRIMARY KEY AUTOINCREMENT,\
                account_id INTEGER NOT NULL,\
                object_type TEXT NOT NULL,\
                object_id TEXT NOT NULL,\
                change_type TEXT NOT NULL\
             );",
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn insert(
        conn: &Arc<Mutex<Connection>>,
        account: i32,
        object_type: &str,
        object_id: &str,
        change_type: &str,
    ) -> i64 {
        let conn = conn.lock().unwrap();
        conn.execute(
            "INSERT INTO changelog (account_id, object_type, object_id, change_type) \
             VALUES (?1, ?2, ?3, ?4)",
            params![account, object_type, object_id, change_type],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[tokio::test]
    async fn is_valid_state_accepts_zero_initial_sync() {
        let conn = open_changelog();
        assert!(is_valid_state(&conn, 1, "Mailbox", 0).await.unwrap());
    }

    #[tokio::test]
    async fn is_valid_state_rejects_future_token_above_head() {
        // Empty stream → head is 0, any positive token is "future"
        // and therefore not valid.
        let conn = open_changelog();
        assert!(!is_valid_state(&conn, 1, "Mailbox", 1).await.unwrap());
        assert!(!is_valid_state(&conn, 1, "Mailbox", 999).await.unwrap());
    }

    #[tokio::test]
    async fn is_valid_state_rejects_negative_token() {
        let conn = open_changelog();
        assert!(!is_valid_state(&conn, 1, "Mailbox", -1).await.unwrap());
    }

    #[tokio::test]
    async fn is_valid_state_rejects_sibling_object_type_token() {
        // The load-bearing test for C3 MAJOR 1. Legacy changelog
        // ids are global ROWIDs interleaved across object types.
        // Fixture interleaves so a sibling token actually exhibits
        // the skip shape: Mailbox at id 1, Email at id 2, Mailbox
        // at id 3. A client passing token=2 (which it could only
        // have received from `Email/changes`) lands on
        // `id > 2` in `changes_since` for Mailbox, returning *only*
        // the Mailbox row at id 3 and silently dropping the one at
        // id 1 — the exact skip RFC 8620 §5.2 prohibits. The bare
        // `<= current` check would have accepted token=2 (current
        // is 3); strict stream-membership rejects it.
        let conn = open_changelog();
        let _m1 = insert(&conn, 1, "Mailbox", "m1", "created"); // id = 1
        let _e1 = insert(&conn, 1, "Email", "e1", "created"); // id = 2
        let _m2 = insert(&conn, 1, "Mailbox", "m2", "created"); // id = 3

        assert!(
            !is_valid_state(&conn, 1, "Mailbox", 2).await.unwrap(),
            "id 2 belongs to Email, must not validate as a Mailbox state"
        );
        assert!(
            is_valid_state(&conn, 1, "Mailbox", 1).await.unwrap(),
            "id 1 is a real Mailbox row, must validate"
        );
        assert!(
            is_valid_state(&conn, 1, "Mailbox", 3).await.unwrap(),
            "id 3 is a real Mailbox row, must validate"
        );
    }

    #[tokio::test]
    async fn is_valid_state_rejects_other_account_token() {
        // Account isolation: id 1 belongs to account 1, account 2
        // querying with that token must not validate.
        let conn = open_changelog();
        let _id = insert(&conn, 1, "Mailbox", "m1", "created");
        assert!(
            !is_valid_state(&conn, 2, "Mailbox", 1).await.unwrap(),
            "account 2 must not see account 1's token as valid"
        );
    }
}
