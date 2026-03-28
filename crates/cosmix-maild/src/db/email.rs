//! Email storage operations.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Email {
    pub id: String,
    #[serde(skip)]
    #[allow(dead_code)]
    pub account_id: i32,
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "mailboxIds")]
    pub mailbox_ids: Vec<String>,
    #[serde(rename = "blobId")]
    pub blob_id: String,
    pub size: i32,
    #[serde(rename = "receivedAt")]
    pub received_at: String,
    #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<Vec<String>>,
    #[serde(rename = "inReplyTo", skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Vec<String>>,
    pub subject: Option<String>,
    #[serde(rename = "from", skip_serializing_if = "Option::is_none")]
    pub from_addr: Option<serde_json::Value>,
    #[serde(rename = "to", skip_serializing_if = "Option::is_none")]
    pub to_addr: Option<serde_json::Value>,
    #[serde(rename = "cc", skip_serializing_if = "Option::is_none")]
    pub cc_addr: Option<serde_json::Value>,
    pub date: Option<String>,
    pub preview: Option<String>,
    #[serde(rename = "hasAttachment")]
    pub has_attachment: bool,
    pub keywords: serde_json::Value,
    #[serde(rename = "spamScore", skip_serializing_if = "Option::is_none")]
    pub spam_score: Option<f64>,
    #[serde(rename = "spamVerdict", skip_serializing_if = "Option::is_none")]
    pub spam_verdict: Option<String>,
}

fn row_to_email(row: &rusqlite::Row<'_>) -> rusqlite::Result<Email> {
    let mailbox_ids_json: String = row.get(3)?;
    let mailbox_ids: Vec<String> = serde_json::from_str(&mailbox_ids_json).unwrap_or_default();

    let in_reply_to_json: Option<String> = row.get(8)?;
    let in_reply_to: Option<Vec<String>> = in_reply_to_json
        .and_then(|s| serde_json::from_str(&s).ok());

    let from_json: Option<String> = row.get(10)?;
    let from_addr: Option<serde_json::Value> = from_json
        .and_then(|s| serde_json::from_str(&s).ok());

    let to_json: Option<String> = row.get(11)?;
    let to_addr: Option<serde_json::Value> = to_json
        .and_then(|s| serde_json::from_str(&s).ok());

    let cc_json: Option<String> = row.get(12)?;
    let cc_addr: Option<serde_json::Value> = cc_json
        .and_then(|s| serde_json::from_str(&s).ok());

    let keywords_json: String = row.get(16)?;
    let keywords: serde_json::Value = serde_json::from_str(&keywords_json).unwrap_or(serde_json::json!({}));

    Ok(Email {
        id: row.get(0)?,
        account_id: row.get(1)?,
        thread_id: row.get(2)?,
        mailbox_ids,
        blob_id: row.get(4)?,
        size: row.get(5)?,
        received_at: row.get(6)?,
        message_id: row.get::<_, Option<String>>(7)?.map(|s| vec![s]),
        in_reply_to,
        subject: row.get(9)?,
        from_addr,
        to_addr,
        cc_addr,
        date: row.get(13)?,
        preview: row.get(14)?,
        has_attachment: row.get::<_, i32>(15)? != 0,
        keywords,
        spam_score: row.get(17)?,
        spam_verdict: row.get(18)?,
    })
}

const EMAIL_COLUMNS: &str = "id, account_id, thread_id, mailbox_ids, blob_id, size, received_at, \
     message_id, in_reply_to, subject, from_addr, to_addr, cc_addr, date, \
     preview, has_attachment, keywords, spam_score, spam_verdict";

pub async fn get_by_ids(conn: &Arc<Mutex<Connection>>, account_id: i32, ids: &[Uuid]) -> Result<Vec<Email>> {
    let conn = conn.clone();
    let ids: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 2)).collect();
        let sql = format!(
            "SELECT {EMAIL_COLUMNS} FROM emails WHERE account_id = ?1 AND id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));
        for id in &ids {
            param_values.push(Box::new(id.clone()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_email)?;
        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }).await?
}

/// RFC 8621 §4.4 Email/query FilterCondition.
#[derive(Debug, Clone, Deserialize)]
pub struct EmailFilter {
    #[serde(rename = "inMailbox")]
    pub in_mailbox: Option<String>,
    #[serde(rename = "inMailboxOtherThan")]
    pub in_mailbox_other_than: Option<Vec<String>>,
    pub before: Option<String>,
    pub after: Option<String>,
    #[serde(rename = "minSize")]
    pub min_size: Option<i64>,
    #[serde(rename = "maxSize")]
    pub max_size: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub text: Option<String>,
    #[serde(rename = "hasAttachment")]
    pub has_attachment: Option<bool>,
    #[serde(rename = "hasKeyword")]
    pub has_keyword: Option<String>,
    #[serde(rename = "notKeyword")]
    pub not_keyword: Option<String>,
}

pub async fn query_ids(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    filter: Option<&EmailFilter>,
    sort_desc: bool,
    position: i64,
    limit: i64,
) -> Result<(Vec<Uuid>, i64)> {
    let conn = conn.clone();
    let filter = filter.cloned();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let order = if sort_desc { "DESC" } else { "ASC" };

        // Build WHERE clauses dynamically
        let mut conditions = vec!["e.account_id = ?1".to_string()];
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(account_id));
        let mut idx = 2u32;

        // FTS5 join needed?
        let mut use_fts = false;

        if let Some(ref f) = filter {
            if let Some(ref mb_id) = f.in_mailbox {
                conditions.push(format!("e.mailbox_ids LIKE ?{idx}"));
                param_values.push(Box::new(format!("%\"{mb_id}\"%")));
                idx += 1;
            }
            if let Some(ref others) = f.in_mailbox_other_than {
                for mb_id in others {
                    conditions.push(format!("e.mailbox_ids NOT LIKE ?{idx}"));
                    param_values.push(Box::new(format!("%\"{mb_id}\"%")));
                    idx += 1;
                }
            }
            if let Some(ref before) = f.before {
                conditions.push(format!("e.received_at < ?{idx}"));
                param_values.push(Box::new(before.clone()));
                idx += 1;
            }
            if let Some(ref after) = f.after {
                conditions.push(format!("e.received_at >= ?{idx}"));
                param_values.push(Box::new(after.clone()));
                idx += 1;
            }
            if let Some(min) = f.min_size {
                conditions.push(format!("e.size >= ?{idx}"));
                param_values.push(Box::new(min));
                idx += 1;
            }
            if let Some(max) = f.max_size {
                conditions.push(format!("e.size <= ?{idx}"));
                param_values.push(Box::new(max));
                idx += 1;
            }
            if let Some(ref from) = f.from {
                conditions.push(format!("e.from_addr LIKE ?{idx}"));
                param_values.push(Box::new(format!("%{from}%")));
                idx += 1;
            }
            if let Some(ref to) = f.to {
                conditions.push(format!("e.to_addr LIKE ?{idx}"));
                param_values.push(Box::new(format!("%{to}%")));
                idx += 1;
            }
            if let Some(ref cc) = f.cc {
                conditions.push(format!("e.cc_addr LIKE ?{idx}"));
                param_values.push(Box::new(format!("%{cc}%")));
                idx += 1;
            }
            if let Some(ref subject) = f.subject {
                conditions.push(format!("e.subject LIKE ?{idx}"));
                param_values.push(Box::new(format!("%{subject}%")));
                idx += 1;
            }
            if let Some(ref text) = f.text {
                use_fts = true;
                conditions.push(format!("emails_fts MATCH ?{idx}"));
                param_values.push(Box::new(text.clone()));
                idx += 1;
            }
            if let Some(has_att) = f.has_attachment {
                let val = if has_att { 1i32 } else { 0 };
                conditions.push(format!("e.has_attachment = ?{idx}"));
                param_values.push(Box::new(val));
                idx += 1;
            }
            if let Some(ref kw) = f.has_keyword {
                conditions.push(format!("e.keywords LIKE ?{idx}"));
                param_values.push(Box::new(format!("%\"{kw}\"%")));
                idx += 1;
            }
            if let Some(ref kw) = f.not_keyword {
                conditions.push(format!("e.keywords NOT LIKE ?{idx}"));
                param_values.push(Box::new(format!("%\"{kw}\"%")));
                idx += 1;
            }
        }

        let where_clause = conditions.join(" AND ");

        let (from_clause, id_col) = if use_fts {
            ("emails e JOIN emails_fts ON emails_fts.rowid = e.rowid", "e.id")
        } else {
            ("emails e", "e.id")
        };

        let sql = format!(
            "SELECT {id_col} FROM {from_clause} WHERE {where_clause} \
             ORDER BY e.received_at {order} LIMIT ?{idx} OFFSET ?{}",
            idx + 1,
        );
        param_values.push(Box::new(limit));
        param_values.push(Box::new(position));

        let count_sql = format!(
            "SELECT COUNT(*) FROM {from_clause} WHERE {where_clause}"
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();

        // Count query uses only the filter params (not limit/offset)
        let count_refs: Vec<&dyn rusqlite::types::ToSql> = refs[..refs.len() - 2].to_vec();

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(refs.as_slice(), |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?.parse::<Uuid>()?);
        }

        let total: i64 = conn.query_row(&count_sql, count_refs.as_slice(), |row| row.get(0))?;
        Ok((ids, total))
    }).await?
}

pub async fn update_keywords(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid, keywords: &serde_json::Value) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    let keywords_str = serde_json::to_string(keywords)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "UPDATE emails SET keywords = ?3 WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str, keywords_str],
        )?;
        Ok(changes > 0)
    }).await?
}

pub async fn update_mailboxes(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid, mailbox_ids: &[Uuid]) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    let mbox_strs: Vec<String> = mailbox_ids.iter().map(|u| u.to_string()).collect();
    let mbox_json = serde_json::to_string(&mbox_strs)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "UPDATE emails SET mailbox_ids = ?3 WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str, mbox_json],
        )?;
        Ok(changes > 0)
    }).await?
}

/// Get the current mailbox_ids and blob_id for an email (for spam retraining).
pub async fn get_mailbox_and_blob(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid) -> Result<Option<(Vec<Uuid>, Uuid)>> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let result: rusqlite::Result<(String, String)> = conn.query_row(
            "SELECT mailbox_ids, blob_id FROM emails WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match result {
            Ok((mbox_json, blob_id_str)) => {
                let mbox_strs: Vec<String> = serde_json::from_str(&mbox_json).unwrap_or_default();
                let mbox_ids: Vec<Uuid> = mbox_strs.iter().filter_map(|s| s.parse().ok()).collect();
                let blob_id: Uuid = blob_id_str.parse()?;
                Ok(Some((mbox_ids, blob_id)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }).await?
}

/// Create an email record (used by inbound delivery).
#[allow(clippy::too_many_arguments)]
pub async fn create(
    conn: &Arc<Mutex<Connection>>,
    account_id: i32,
    thread_id: Uuid,
    mailbox_ids: &[Uuid],
    blob_id: Uuid,
    size: i32,
    message_id: Option<&str>,
    in_reply_to: Option<&[String]>,
    subject: Option<&str>,
    from_addr: Option<&serde_json::Value>,
    to_addr: Option<&serde_json::Value>,
    cc_addr: Option<&serde_json::Value>,
    date: Option<chrono::DateTime<chrono::Utc>>,
    preview: Option<&str>,
    has_attachment: bool,
    spam_score: Option<f64>,
    spam_verdict: Option<&str>,
) -> Result<Uuid> {
    let conn = conn.clone();
    let id = Uuid::new_v4();
    let id_str = id.to_string();
    let thread_id_str = thread_id.to_string();
    let mbox_strs: Vec<String> = mailbox_ids.iter().map(|u| u.to_string()).collect();
    let mbox_json = serde_json::to_string(&mbox_strs)?;
    let blob_id_str = blob_id.to_string();
    let message_id = message_id.map(|s| s.to_string());
    let in_reply_to_json = in_reply_to.map(|s| serde_json::to_string(s).unwrap_or_default());
    let subject = subject.map(|s| s.to_string());
    let from_json = from_addr.map(|v| serde_json::to_string(v).unwrap_or_default());
    let to_json = to_addr.map(|v| serde_json::to_string(v).unwrap_or_default());
    let cc_json = cc_addr.map(|v| serde_json::to_string(v).unwrap_or_default());
    let date_str = date.map(|d| d.to_rfc3339());
    let preview = preview.map(|s| s.to_string());
    let has_attachment_int = if has_attachment { 1i32 } else { 0 };
    let spam_verdict = spam_verdict.map(|s| s.to_string());
    let now = chrono::Utc::now().to_rfc3339();

    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute(
            "INSERT INTO emails (id, account_id, thread_id, mailbox_ids, blob_id, size, received_at, \
             message_id, in_reply_to, subject, from_addr, to_addr, cc_addr, date, preview, \
             has_attachment, spam_score, spam_verdict) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                id_str, account_id, thread_id_str, mbox_json, blob_id_str, size, now,
                message_id, in_reply_to_json, subject, from_json, to_json, cc_json,
                date_str, preview, has_attachment_int, spam_score, spam_verdict
            ],
        )?;

        // Record in changelog
        let changelog_object_id = id_str.clone();
        conn.execute(
            "INSERT INTO changelog (account_id, object_type, object_id, change_type) VALUES (?1, ?2, ?3, ?4)",
            params![account_id, "Email", changelog_object_id, "created"],
        )?;

        Ok(id)
    }).await?
}

pub async fn delete(conn: &Arc<Mutex<Connection>>, account_id: i32, id: Uuid) -> Result<bool> {
    let conn = conn.clone();
    let id_str = id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let changes = conn.execute(
            "DELETE FROM emails WHERE account_id = ?1 AND id = ?2",
            params![account_id, id_str],
        )?;
        Ok(changes > 0)
    }).await?
}
