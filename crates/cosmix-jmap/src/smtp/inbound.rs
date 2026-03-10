//! Inbound mail delivery — parse message, assign thread, store in JMAP.

use anyhow::Result;
use mail_parser::{Address, HeaderValue, MessageParser};

use super::SmtpState;
use crate::db;

/// Deliver a received message to the appropriate mailboxes.
pub async fn deliver(
    state: &SmtpState,
    _sender_account_id: Option<i32>,
    _mail_from: &str,
    rcpt_to: &[String],
    data: &[u8],
) -> Result<()> {
    // Parse the message
    let parser = MessageParser::default();
    let message = parser.parse(data)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse message"))?;

    // Extract headers
    let subject = message.subject().map(|s| s.to_string());
    let message_id = message.message_id().map(|s| s.to_string());
    let date = message.date().map(|d| {
        chrono::DateTime::from_timestamp(d.to_timestamp(), 0)
            .unwrap_or_else(chrono::Utc::now)
    });

    // in_reply_to returns &HeaderValue — extract text list
    let in_reply_to: Option<Vec<String>> = match message.in_reply_to() {
        HeaderValue::Text(s) => Some(vec![s.to_string()]),
        HeaderValue::TextList(list) => Some(list.iter().map(|s| s.to_string()).collect()),
        _ => None,
    };

    let from_addr = extract_addresses(message.from());
    let to_addr = extract_addresses(message.to());
    let cc_addr = extract_addresses(message.cc());

    // Body preview — first 256 chars of text body
    let preview = message.body_preview(256)
        .map(|s| s.to_string());

    // Has attachments?
    let has_attachment = message.attachment_count() > 0;

    let size = data.len() as i32;

    // Deliver to each recipient
    for rcpt in rcpt_to {
        let account = db::account::get_by_email(&state.db.pool, rcpt).await?;
        let Some(account) = account else { continue };

        // Find inbox mailbox
        let inbox_id = db::mailbox::get_inbox(&state.db.pool, account.id).await?;

        // Store blob
        let blob_id = db::blob::store(&state.db.pool, &state.db.blob_dir, account.id, data).await?;

        // Find or create thread
        let thread_id = db::thread::find_or_create(
            &state.db.pool,
            account.id,
            message_id.as_deref(),
            in_reply_to.as_deref(),
        ).await?;

        // Create email record
        db::email::create(
            &state.db.pool,
            account.id,
            thread_id,
            &[inbox_id],
            blob_id,
            size,
            message_id.as_deref(),
            in_reply_to.as_deref(),
            subject.as_deref(),
            from_addr.as_ref(),
            to_addr.as_ref(),
            cc_addr.as_ref(),
            date,
            preview.as_deref(),
            has_attachment,
        ).await?;

        tracing::info!(
            to = %rcpt,
            subject = subject.as_deref().unwrap_or("(none)"),
            "Delivered inbound message"
        );
    }

    Ok(())
}

/// Extract addresses from a mail-parser Address into JSON.
fn extract_addresses(addr: Option<&Address<'_>>) -> Option<serde_json::Value> {
    let addr = addr?;
    match addr {
        Address::List(list) => {
            let addrs: Vec<serde_json::Value> = list.iter()
                .map(|a| {
                    serde_json::json!({
                        "name": a.name.as_deref().unwrap_or(""),
                        "email": a.address.as_deref().unwrap_or("")
                    })
                })
                .collect();
            Some(serde_json::Value::Array(addrs))
        }
        Address::Group(groups) => {
            let addrs: Vec<serde_json::Value> = groups.iter()
                .flat_map(|g| g.addresses.iter())
                .map(|a| {
                    serde_json::json!({
                        "name": a.name.as_deref().unwrap_or(""),
                        "email": a.address.as_deref().unwrap_or("")
                    })
                })
                .collect();
            Some(serde_json::Value::Array(addrs))
        }
    }
}
