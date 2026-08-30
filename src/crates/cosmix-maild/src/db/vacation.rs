//! VacationResponse storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct VacationResponse {
    pub id: String,
    #[serde(rename = "isEnabled")]
    pub is_enabled: bool,
    #[serde(rename = "fromDate", skip_serializing_if = "Option::is_none")]
    pub from_date: Option<String>,
    #[serde(rename = "toDate", skip_serializing_if = "Option::is_none")]
    pub to_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(rename = "textBody", skip_serializing_if = "Option::is_none")]
    pub text_body: Option<String>,
    #[serde(rename = "htmlBody", skip_serializing_if = "Option::is_none")]
    pub html_body: Option<String>,
}

impl Default for VacationResponse {
    fn default() -> Self {
        Self {
            id: "singleton".to_string(),
            is_enabled: false,
            from_date: None,
            to_date: None,
            subject: None,
            text_body: None,
            html_body: None,
        }
    }
}

/// Get the vacation response config for an account.
pub async fn get(conn: &Arc<Mutex<Connection>>, account_id: i32) -> Result<VacationResponse> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let result = conn.query_row(
            "SELECT id, is_enabled, from_date, to_date, subject, text_body, html_body \
             FROM vacation_responses WHERE account_id = ?1",
            params![account_id],
            |row| {
                Ok(VacationResponse {
                    id: row.get(0)?,
                    is_enabled: row.get::<_, i32>(1)? != 0,
                    from_date: row.get(2)?,
                    to_date: row.get(3)?,
                    subject: row.get(4)?,
                    text_body: row.get(5)?,
                    html_body: row.get(6)?,
                })
            },
        );
        match result {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(VacationResponse::default()),
            Err(e) => Err(e.into()),
        }
    })
    .await?
}

/// Create or update the vacation response config.
pub async fn upsert(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    vr: &VacationResponse,
) -> Result<()> {
    let conn = conn.clone();
    let vr = vr.clone();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute(
            "INSERT INTO vacation_responses (id, account_id, is_enabled, from_date, to_date, subject, text_body, html_body) \
             VALUES ('singleton', ?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(account_id) DO UPDATE SET \
             is_enabled = excluded.is_enabled, \
             from_date = excluded.from_date, \
             to_date = excluded.to_date, \
             subject = excluded.subject, \
             text_body = excluded.text_body, \
             html_body = excluded.html_body",
            params![
                account_id,
                vr.is_enabled as i32,
                vr.from_date,
                vr.to_date,
                vr.subject,
                vr.text_body,
                vr.html_body,
            ],
        )?;
        Ok(())
    }).await?
}

/// RFC 3834 §2 re-reply suppression window: at most one auto-reply per
/// (account, sender) pair per this many days.
pub const REPLY_WINDOW_DAYS: u32 = 7;

/// Atomically check-and-record that a vacation auto-reply is due for
/// `sender`: returns `true` (and stamps `sent_at = now`) when no reply
/// was recorded within [`REPLY_WINDOW_DAYS`], `false` when one was.
/// Single upsert statement, so two concurrent deliveries from the same
/// sender cannot both win. Callers stamp BEFORE queueing the reply —
/// the fail-safe direction is "recorded but not sent", never "sent
/// twice".
pub async fn mark_reply_if_due(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    sender: &str,
) -> Result<bool> {
    let conn = conn.clone();
    let sender = sender.to_string();
    tokio::task::spawn_blocking(move || -> Result<bool> {
        let conn = conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        // INSERT stamps a first-contact sender; the conflict arm re-stamps
        // ONLY when the previous reply is older than the window (the WHERE
        // makes an in-window conflict a no-op → 0 rows changed → not due).
        let changed = conn.execute(
            "INSERT INTO vacation_replies (account_id, sender, sent_at) \
             VALUES (?1, ?2, datetime('now')) \
             ON CONFLICT(account_id, sender) DO UPDATE SET sent_at = datetime('now') \
             WHERE sent_at <= datetime('now', ?3)",
            params![account_id, sender, format!("-{REPLY_WINDOW_DAYS} days")],
        )?;
        Ok(changed > 0)
    })
    .await?
}

/// Check if vacation auto-reply is active for an account right now.
pub async fn is_active(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
) -> Result<Option<VacationResponse>> {
    let vr = get(conn, account_id).await?;
    if !vr.is_enabled {
        return Ok(None);
    }

    let now = chrono::Utc::now().to_rfc3339();

    if let Some(ref from) = vr.from_date
        && now < *from
    {
        return Ok(None);
    }
    if let Some(ref to) = vr.to_date
        && now > *to
    {
        return Ok(None);
    }

    Ok(Some(vr))
}
