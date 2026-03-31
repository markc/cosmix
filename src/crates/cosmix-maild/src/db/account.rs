//! Account storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use uuid::Uuid;

#[derive(Debug)]
pub struct Account {
    pub id: i32,
    pub email: String,
    pub password: String,
    pub name: Option<String>,
    #[allow(dead_code)]
    pub quota: i64,
    pub spam_enabled: bool,
    pub spam_threshold: f64,
}

fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get(0)?,
        email: row.get(1)?,
        password: row.get(2)?,
        name: row.get(3)?,
        quota: row.get(4)?,
        spam_enabled: row.get::<_, i32>(5)? != 0,
        spam_threshold: row.get(6)?,
    })
}

pub async fn get_by_email(conn: &Arc<Mutex<Connection>>, email: &str) -> Result<Option<Account>> {
    let conn = conn.clone();
    let email = email.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, email, password, name, quota, \
             COALESCE(spam_enabled, 1) as spam_enabled, \
             COALESCE(spam_threshold, 0.5) as spam_threshold \
             FROM accounts WHERE email = ?1"
        )?;
        let mut rows = stmt.query_map(params![email], row_to_account)?;
        match rows.next() {
            Some(Ok(account)) => Ok(Some(account)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }).await?
}

pub async fn create(conn: &Arc<Mutex<Connection>>, email: &str, password_hash: &str, name: Option<&str>) -> Result<i32> {
    let conn = conn.clone();
    let email = email.to_string();
    let password_hash = password_hash.to_string();
    let name = name.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        conn.execute(
            "INSERT INTO accounts (email, password, name) VALUES (?1, ?2, ?3)",
            params![email, password_hash, name],
        )?;
        let id = conn.last_insert_rowid() as i32;

        // Create default mailboxes
        let default_mailboxes = [
            ("Inbox", "inbox"),
            ("Drafts", "drafts"),
            ("Sent", "sent"),
            ("Trash", "trash"),
            ("Junk", "junk"),
            ("Archive", "archive"),
        ];
        for (mbox_name, role) in &default_mailboxes {
            let mbox_id = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO mailboxes (id, account_id, name, role) VALUES (?1, ?2, ?3, ?4)",
                params![mbox_id, id, mbox_name, role],
            )?;
        }

        // Create default calendar and addressbook
        let cal_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT OR IGNORE INTO calendars (id, account_id, name, color) VALUES (?1, ?2, ?3, ?4)",
            params![cal_id, id, "Personal", "#4285f4"],
        )?;
        let ab_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT OR IGNORE INTO addressbooks (id, account_id, name) VALUES (?1, ?2, ?3)",
            params![ab_id, id, "Contacts"],
        )?;

        Ok(id)
    }).await?
}

pub async fn list(conn: &Arc<Mutex<Connection>>) -> Result<Vec<Account>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, email, password, name, quota, \
             COALESCE(spam_enabled, 1) as spam_enabled, \
             COALESCE(spam_threshold, 0.5) as spam_threshold \
             FROM accounts ORDER BY id"
        )?;
        let rows = stmt.query_map([], row_to_account)?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(row?);
        }
        Ok(accounts)
    }).await?
}

pub async fn delete(conn: &Arc<Mutex<Connection>>, email: &str) -> Result<bool> {
    let conn = conn.clone();
    let email = email.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute("DELETE FROM accounts WHERE email = ?1", params![email])?;
        Ok(changes > 0)
    }).await?
}

/// Get an account by ID (for Identity/get).
pub async fn get_by_id(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Option<Account>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, email, password, name, quota, \
             COALESCE(spam_enabled, 1) as spam_enabled, \
             COALESCE(spam_threshold, 0.5) as spam_threshold \
             FROM accounts WHERE id = ?1"
        )?;
        let mut rows = stmt.query_map(params![account_id], row_to_account)?;
        match rows.next() {
            Some(Ok(account)) => Ok(Some(account)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }).await?
}
