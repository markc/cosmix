//! Account storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};

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
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, email, password, name, quota, \
             COALESCE(spam_enabled, 1) as spam_enabled, \
             COALESCE(spam_threshold, 0.5) as spam_threshold \
             FROM accounts WHERE email = ?1",
        )?;
        let mut rows = stmt.query_map(params![email], row_to_account)?;
        match rows.next() {
            Some(Ok(account)) => Ok(Some(account)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    })
    .await?
}

pub async fn list(conn: &Arc<Mutex<Connection>>) -> Result<Vec<Account>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, email, password, name, quota, \
             COALESCE(spam_enabled, 1) as spam_enabled, \
             COALESCE(spam_threshold, 0.5) as spam_threshold \
             FROM accounts ORDER BY id",
        )?;
        let rows = stmt.query_map([], row_to_account)?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(row?);
        }
        Ok(accounts)
    })
    .await?
}

/// Get an account by ID (for Identity/get).
pub async fn get_by_id(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<Option<Account>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut stmt = conn.prepare(
            "SELECT id, email, password, name, quota, \
             COALESCE(spam_enabled, 1) as spam_enabled, \
             COALESCE(spam_threshold, 0.5) as spam_threshold \
             FROM accounts WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![account_id], row_to_account)?;
        match rows.next() {
            Some(Ok(account)) => Ok(Some(account)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    })
    .await?
}
