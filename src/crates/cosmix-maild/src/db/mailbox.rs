//! Mailbox storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Mailbox {
    pub id: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub account_id: i32,
    pub name: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub role: Option<String>,
    #[serde(rename = "sortOrder")]
    pub sort_order: i32,
    #[serde(rename = "totalEmails")]
    pub total_emails: i64,
    #[serde(rename = "unreadEmails")]
    pub unread_emails: i64,
}

fn row_to_mailbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<Mailbox> {
    Ok(Mailbox {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        parent_id: row.get(3)?,
        role: row.get(4)?,
        sort_order: row.get(5)?,
        total_emails: row.get(6)?,
        unread_emails: row.get(7)?,
    })
}

/// Count emails per mailbox for an account. Returns (mailbox_id -> (total, unread)).
fn count_emails_per_mailbox(conn: &Connection, account_id: i32) -> Result<std::collections::HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT mailbox_ids, keywords FROM emails WHERE account_id = ?1"
    )?;
    let rows = stmt.query_map(params![account_id], |row| {
        let mailbox_ids_json: String = row.get(0)?;
        let keywords_json: String = row.get(1)?;
        Ok((mailbox_ids_json, keywords_json))
    })?;

    let mut counts: std::collections::HashMap<String, (i64, i64)> = std::collections::HashMap::new();
    for row in rows {
        let (mbox_json, kw_json) = row?;
        let mbox_ids: Vec<String> = serde_json::from_str(&mbox_json).unwrap_or_default();
        let keywords: serde_json::Value = serde_json::from_str(&kw_json).unwrap_or(serde_json::json!({}));
        let is_seen = keywords.get("$seen").is_some();

        for mbox_id in mbox_ids {
            let entry = counts.entry(mbox_id).or_insert((0, 0));
            entry.0 += 1;
            if !is_seen {
                entry.1 += 1;
            }
        }
    }
    Ok(counts)
}

pub async fn get_by_ids(conn: &Arc<Mutex<Connection>>, account_id: i32, ids: &[Uuid]) -> Result<Vec<Mailbox>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let counts = count_emails_per_mailbox(&conn, account_id)?;

        let placeholders: Vec<String> = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 2)).collect();
        let sql = format!(
            "SELECT id, account_id, name, parent_id, role, sort_order, 0, 0 \
             FROM mailboxes WHERE account_id = ?1 AND id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));
        for id in &ids {
            param_values.push(Box::new(id.clone()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_mailbox)?;
        let mut mailboxes = Vec::new();
        for row in rows {
            let mut mbox = row?;
            if let Some((total, unread)) = counts.get(&mbox.id) {
                mbox.total_emails = *total;
                mbox.unread_emails = *unread;
            }
            mailboxes.push(mbox);
        }
        Ok(mailboxes)
    }).await?
}

pub async fn get_all(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Vec<Mailbox>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let counts = count_emails_per_mailbox(&conn, account_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, account_id, name, parent_id, role, sort_order, 0, 0 \
             FROM mailboxes WHERE account_id = ?1 ORDER BY sort_order, name"
        )?;
        let rows = stmt.query_map(params![account_id], row_to_mailbox)?;
        let mut mailboxes = Vec::new();
        for row in rows {
            let mut mbox = row?;
            if let Some((total, unread)) = counts.get(&mbox.id) {
                mbox.total_emails = *total;
                mbox.unread_emails = *unread;
            }
            mailboxes.push(mbox);
        }
        Ok(mailboxes)
    }).await?
}

pub async fn create(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    name: &str,
    parent_id: Option<Uuid>,
    role: Option<&str>,
) -> Result<Uuid> {
    let conn = conn.clone();
    let name = name.to_string();
    let parent_id = parent_id.map(|u| u.to_string());
    let role = role.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        conn.execute(
            "INSERT INTO mailboxes (id, account_id, name, parent_id, role) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id_str, account_id, name, parent_id, role],
        )?;
        Ok(id)
    }).await?
}

pub async fn update(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    id: Uuid,
    name: Option<&str>,
    parent_id: Option<Option<Uuid>>,
    sort_order: Option<i32>,
) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    let name = name.map(|s| s.to_string());
    let parent_id = parent_id.map(|o| o.map(|u| u.to_string()));
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut sets = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        values.push(Box::new(account_id));
        values.push(Box::new(id_str));

        if let Some(n) = &name {
            sets.push(format!("name = ?{}", values.len() + 1));
            values.push(Box::new(n.clone()));
        }
        if let Some(p) = &parent_id {
            sets.push(format!("parent_id = ?{}", values.len() + 1));
            values.push(Box::new(p.clone()));
        }
        if let Some(s) = sort_order {
            sets.push(format!("sort_order = ?{}", values.len() + 1));
            values.push(Box::new(s));
        }

        if sets.is_empty() {
            return Ok(true);
        }

        let sql = format!(
            "UPDATE mailboxes SET {} WHERE account_id = ?1 AND id = ?2",
            sets.join(", ")
        );
        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let changes = conn.execute(&sql, refs.as_slice())?;
        Ok(changes > 0)
    }).await?
}

pub async fn delete(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "DELETE FROM mailboxes WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        Ok(changes > 0)
    }).await?
}

/// Get the inbox mailbox ID for an account.
pub async fn get_inbox(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Uuid> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let id_str: String = conn.query_row(
            "SELECT id FROM mailboxes WHERE account_id = ?1 AND role = 'inbox' LIMIT 1",
            params![account_id],
            |row| row.get(0),
        )?;
        Ok(id_str.parse::<Uuid>()?)
    }).await?
}

/// Get a mailbox by role.
pub async fn get_by_role(conn: &Arc<Mutex<Connection>>, account_id: i32, role: &str) -> Result<Option<Uuid>> {
    let conn = conn.clone();
    let role = role.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let result: rusqlite::Result<String> = conn.query_row(
            "SELECT id FROM mailboxes WHERE account_id = ?1 AND role = ?2 LIMIT 1",
            params![account_id, role],
            |row| row.get(0),
        );
        match result {
            Ok(id_str) => Ok(Some(id_str.parse::<Uuid>()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }).await?
}

pub async fn query_ids(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Vec<Uuid>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id FROM mailboxes WHERE account_id = ?1 ORDER BY sort_order, name"
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
