//! JMAP EmailSubmission methods (RFC 8621 §7).

use anyhow::Result;
use serde::Deserialize;
use uuid::Uuid;

use crate::db::{self, Db};
use super::types::*;

/// EmailSubmission/set — queue messages for outbound delivery.
pub async fn set(db: &Db, account_id: i32, args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();
    let old_state = db::changelog::current_state(&db.pool, account_id, "EmailSubmission").await?;

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

    let new_state = db::changelog::current_state(&db.pool, account_id, "EmailSubmission").await?;

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
    let emails = db::email::get_by_ids(&db.pool, account_id, &[email_uuid]).await?;
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

    // Queue for delivery
    let queue_id = crate::smtp::queue::enqueue(&db.pool, &from_addr, &to_addrs, email.blob_id).await?;

    // Move to Sent mailbox if one exists
    if let Ok(Some(sent_id)) = db::mailbox::get_by_role(&db.pool, account_id, "sent").await {
        let _ = db::email::update_mailboxes(&db.pool, account_id, email_uuid, &[sent_id]).await;
    }

    // Generate a submission ID from the queue ID
    let submission_id = Uuid::new_v4();

    tracing::info!(
        queue_id = queue_id,
        from = %from_addr,
        to = ?to_addrs,
        "EmailSubmission queued"
    );

    Ok(submission_id)
}

/// Identity/get — list configured sender identities.
pub async fn identity_get(db: &Db, account_id: i32, _args: serde_json::Value) -> Result<serde_json::Value> {
    let acct = account_id.to_string();

    // For now, create an identity from the account's email
    let account = sqlx::query_as::<_, db::account::Account>(
        "SELECT id, email, password, name, quota FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(&db.pool)
    .await?;

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
