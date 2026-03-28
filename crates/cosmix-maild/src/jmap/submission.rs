//! JMAP EmailSubmission methods (RFC 8621 §7).

use anyhow::Result;
use serde::Deserialize;
use uuid::Uuid;

use crate::db::{self, Db};
use super::types::*;

/// EmailSubmission/set — queue messages for outbound delivery.
pub async fn set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.conn, account_id, "EmailSubmission").await?;

    let mut created_map = std::collections::HashMap::new();

    // Handle create
    if let Some(create) = args.get("create").and_then(|v| v.as_object()) {
        for (client_id, obj) in create {
            let result = create_submission(db, account_id, obj).await;
            match result {
                Ok(submission_id) => {
                    created_map.insert(client_id.clone(), serde_json::json!({
                        "id": submission_id.to_string(),
                        "sendAt": chrono::Utc::now().to_rfc3339(),
                    }));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "EmailSubmission create failed");
                }
            }
        }
    }

    let new_state = db::changelog::current_state(&db.conn, account_id, "EmailSubmission").await?;

    let resp = SetResponse {
        account_id: acct,
        old_state,
        new_state,
        created: if created_map.is_empty() { None } else { Some(created_map) },
        updated: None,
        destroyed: None,
        not_created: None,
        not_updated: None,
        not_destroyed: None,
    };

    Ok(serde_json::to_value(resp)?)
}

/// Create a single email submission — load the email blob and queue it.
async fn create_submission(db: &Db, account_id: i32, obj: &serde_json::Value) -> Result<Uuid> {
    #[derive(Deserialize)]
    struct SubmissionCreate {
        #[serde(rename = "emailId")]
        email_id: String,
        #[serde(rename = "identityId")]
        _identity_id: Option<String>,
        envelope: Option<Envelope>,
    }

    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "mailFrom")]
        mail_from: EnvelopeAddr,
        #[serde(rename = "rcptTo")]
        rcpt_to: Vec<EnvelopeAddr>,
    }

    #[derive(Deserialize)]
    struct EnvelopeAddr {
        email: String,
    }

    let sub: SubmissionCreate = serde_json::from_value(obj.clone())?;
    let email_uuid: Uuid = sub.email_id.parse()?;

    // Load the email record
    let emails = db::email::get_by_ids(&db.conn, account_id, &[email_uuid]).await?;
    let email = emails.into_iter().next()
        .ok_or_else(|| anyhow::anyhow!("Email not found: {}", sub.email_id))?;

    // Determine envelope
    let (from_addr, to_addrs) = if let Some(env) = sub.envelope {
        (env.mail_from.email, env.rcpt_to.into_iter().map(|a| a.email).collect())
    } else {
        // Auto-detect from message headers
        let from = email.from_addr
            .as_ref()
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|o| o.get("email"))
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .to_string();

        let mut tos: Vec<String> = Vec::new();
        for field in [&email.to_addr, &email.cc_addr] {
            if let Some(arr) = field.as_ref().and_then(|v| v.as_array()) {
                for addr in arr {
                    if let Some(e) = addr.get("email").and_then(|e| e.as_str()) {
                        if !e.is_empty() {
                            tos.push(e.to_string());
                        }
                    }
                }
            }
        }
        (from, tos)
    };

    if to_addrs.is_empty() {
        anyhow::bail!("No recipients");
    }

    let blob_uuid: Uuid = email.blob_id.parse()?;

    // Separate local vs remote recipients
    let mut remote_addrs = Vec::new();
    for addr in &to_addrs {
        let local_account = db::account::get_by_email(&db.conn, addr).await?;
        if let Some(rcpt_account) = local_account {
            // Local delivery — copy blob to recipient's store and create email record
            let blob_data = db::blob::load(&db.conn, &db.blob_dir, blob_uuid).await?;
            if let Some(data) = blob_data {
                let rcpt_blob_id = db::blob::store(&db.conn, &db.blob_dir, rcpt_account.id, &data).await?;
                let thread_id = db::thread::find_or_create(
                    &db.conn, rcpt_account.id, None, None,
                ).await?;
                let inbox_id = db::mailbox::get_inbox(&db.conn, rcpt_account.id).await?;

                // Parse basic headers from the blob for the email record
                let msg = mail_parser::MessageParser::default().parse(&data);
                let subject = msg.as_ref().and_then(|m| m.subject()).map(|s| s.to_string());
                let preview = msg.as_ref().and_then(|m| m.body_preview(256)).map(|s| s.to_string());

                db::email::create(
                    &db.conn,
                    rcpt_account.id,
                    thread_id,
                    &[inbox_id],
                    rcpt_blob_id,
                    data.len() as i32,
                    None, None,
                    subject.as_deref(),
                    Some(&serde_json::json!([{"name": "", "email": &from_addr}])),
                    Some(&serde_json::json!([{"name": "", "email": addr}])),
                    None,
                    Some(chrono::Utc::now()),
                    preview.as_deref(),
                    false,
                    None, None,
                ).await?;

                tracing::info!(from = %from_addr, to = %addr, "Local delivery completed");
            }
        } else {
            remote_addrs.push(addr.clone());
        }
    }

    // Queue remote recipients for outbound SMTP
    if !remote_addrs.is_empty() {
        let queue_id = crate::smtp::queue::enqueue(&db.conn, &from_addr, &remote_addrs, blob_uuid).await?;
        tracing::info!(queue_id = queue_id, from = %from_addr, to = ?remote_addrs, "EmailSubmission queued for remote delivery");
    }

    // Move to Sent mailbox if one exists
    if let Ok(Some(sent_id)) = db::mailbox::get_by_role(&db.conn, account_id, "sent").await {
        let _ = db::email::update_mailboxes(&db.conn, account_id, email_uuid, &[sent_id]).await;
    }

    // Persist submission record
    let submission_id = Uuid::new_v4();
    let to_json = serde_json::to_string(&to_addrs)?;
    let sub_conn = db.conn.clone();
    let sub_id_str = submission_id.to_string();
    let email_id_str = email_uuid.to_string();
    let from_clone = from_addr.clone();
    tokio::task::spawn_blocking(move || {
        let conn = sub_conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        conn.execute(
            "INSERT INTO email_submissions (id, account_id, email_id, envelope_from, envelope_to, undo_status) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'final')",
            rusqlite::params![sub_id_str, account_id, email_id_str, from_clone, to_json],
        )?;
        Ok::<_, anyhow::Error>(())
    }).await??;

    db::changelog::record(&db.conn, account_id, "EmailSubmission", submission_id, "created").await?;

    Ok(submission_id)
}

/// EmailSubmission/get — query submission status.
pub async fn get(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    let ids: Option<Vec<String>> = args.get("ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let conn = db.conn.clone();
    let submissions = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        let mut results = Vec::new();

        if let Some(ids) = ids {
            for id in &ids {
                let result: rusqlite::Result<(String, String, String, String, String, String)> = conn.query_row(
                    "SELECT id, email_id, envelope_from, envelope_to, send_at, undo_status \
                     FROM email_submissions WHERE id = ?1 AND account_id = ?2",
                    rusqlite::params![id, account_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
                );
                if let Ok((id, email_id, from, to_json, send_at, undo_status)) = result {
                    let to_addrs: Vec<String> = serde_json::from_str(&to_json).unwrap_or_default();
                    let rcpt_to: Vec<serde_json::Value> = to_addrs.iter()
                        .map(|e| serde_json::json!({"email": e}))
                        .collect();
                    results.push(serde_json::json!({
                        "id": id,
                        "emailId": email_id,
                        "envelope": {
                            "mailFrom": {"email": from},
                            "rcptTo": rcpt_to,
                        },
                        "sendAt": send_at,
                        "undoStatus": undo_status,
                        "deliveryStatus": {},
                    }));
                }
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, email_id, envelope_from, envelope_to, send_at, undo_status \
                 FROM email_submissions WHERE account_id = ?1 ORDER BY created_at DESC LIMIT 100"
            )?;
            let rows = stmt.query_map(rusqlite::params![account_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let (id, email_id, from, to_json, send_at, undo_status) = row?;
                let to_addrs: Vec<String> = serde_json::from_str(&to_json).unwrap_or_default();
                let rcpt_to: Vec<serde_json::Value> = to_addrs.iter()
                    .map(|e| serde_json::json!({"email": e}))
                    .collect();
                results.push(serde_json::json!({
                    "id": id,
                    "emailId": email_id,
                    "envelope": {
                        "mailFrom": {"email": from},
                        "rcptTo": rcpt_to,
                    },
                    "sendAt": send_at,
                    "undoStatus": undo_status,
                    "deliveryStatus": {},
                }));
            }
        }
        Ok::<_, anyhow::Error>(results)
    }).await??;

    let state = db::changelog::current_state(&db.conn, account_id, "EmailSubmission").await?;

    Ok(serde_json::json!({
        "accountId": acct,
        "state": state,
        "list": submissions,
        "notFound": [],
    }))
}

/// EmailSubmission/changes — returns changes since a given state.
pub async fn changes(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let since_state = args.get("sinceState")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let max = args.get("maxChanges").and_then(|v| v.as_i64()).unwrap_or(500);

    let result = db::changelog::changes_since(&db.conn, account_id, "EmailSubmission", since_state, max).await?;

    let resp = ChangesResponse {
        account_id: acct,
        old_state: since_state.to_string(),
        new_state: result.new_state,
        has_more_changes: result.has_more_changes,
        created: result.created,
        updated: result.updated,
        destroyed: result.destroyed,
    };

    Ok(serde_json::to_value(resp)?)
}

/// Identity/get — list configured sender identities.
pub async fn identity_get(db: &Db, account_id: i32, _args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    // For now, create an identity from the account's email
    let account = db::account::get_by_id(&db.conn, account_id).await?;

    let list = if let Some(a) = account {
        vec![serde_json::json!({
            "id": a.id.to_string(),
            "name": a.name.unwrap_or_default(),
            "email": a.email,
            "replyTo": null,
            "bcc": null,
            "textSignature": "",
            "htmlSignature": "",
            "mayDelete": false,
        })]
    } else {
        vec![]
    };

    Ok(serde_json::json!({
        "accountId": acct,
        "state": "0",
        "list": list,
        "notFound": [],
    }))
}
