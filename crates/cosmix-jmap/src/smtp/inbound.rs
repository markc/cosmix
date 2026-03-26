//! Inbound mail delivery — verify, classify, parse, store in JMAP.

use std::net::IpAddr;

use anyhow::Result;
use mail_auth::{AuthenticatedMessage, DkimResult, MessageAuthenticator};
use mail_auth::spf::verify::SpfParameters;
use mail_parser::{Address, HeaderValue, MessageParser};

use super::SmtpState;
use crate::db;

/// Deliver a received message to the appropriate mailboxes.
pub async fn deliver(
    state: &SmtpState,
    _sender_account_id: Option<i32>,
    mail_from: &str,
    rcpt_to: &[String],
    data: &[u8],
    remote_ip: IpAddr,
    ehlo_host: &str,
) -> Result<()> {
    let hostname = &state.config.hostname;

    // --- Authentication checks (SPF, DKIM) ---
    let auth_results = verify_authentication(data, mail_from, remote_ip, ehlo_host, hostname).await;

    // Prepend Authentication-Results header to stored message
    let auth_header = format!(
        "Authentication-Results: {hostname}; {auth_results}\r\n"
    );
    let mut augmented_data = Vec::with_capacity(auth_header.len() + data.len());
    augmented_data.extend_from_slice(auth_header.as_bytes());
    augmented_data.extend_from_slice(data);

    // --- Parse the message ---
    let parser = MessageParser::default();
    let message = parser.parse(&augmented_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse message"))?;

    // Extract headers
    let subject = message.subject().map(|s| s.to_string());
    let message_id = message.message_id().map(|s| s.to_string());
    let date = message.date().map(|d| {
        chrono::DateTime::from_timestamp(d.to_timestamp(), 0)
            .unwrap_or_else(chrono::Utc::now)
    });

    let in_reply_to: Option<Vec<String>> = match message.in_reply_to() {
        HeaderValue::Text(s) => Some(vec![s.to_string()]),
        HeaderValue::TextList(list) => Some(list.iter().map(|s| s.to_string()).collect()),
        _ => None,
    };

    let from_addr = extract_addresses(message.from());
    let to_addr = extract_addresses(message.to());
    let cc_addr = extract_addresses(message.cc());

    let preview = message.body_preview(256).map(|s| s.to_string());
    let has_attachment = message.attachment_count() > 0;
    let size = augmented_data.len() as i32;

    // --- Deliver to each recipient ---
    for rcpt in rcpt_to {
        let account = db::account::get_by_email(&state.db.conn, rcpt).await?;
        let Some(account) = account else { continue };

        // Spam classification
        let (spam_verdict, spam_score) = if account.spam_enabled {
            match state.spam_filter.classify(account.id, data, account.spam_threshold) {
                Ok((verdict, score)) => {
                    let verdict_str = verdict.to_string();
                    tracing::info!(
                        to = %rcpt,
                        verdict = %verdict_str,
                        score = score,
                        "Spam classification"
                    );
                    (Some(verdict_str), Some(score))
                }
                Err(e) => {
                    tracing::warn!(error = %e, to = %rcpt, "Spam classification failed");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // Route based on spam verdict
        let target_mailbox = if spam_verdict.as_deref() == Some("SPAM") {
            db::mailbox::get_by_role(&state.db.conn, account.id, "junk").await?
                .unwrap_or(db::mailbox::get_inbox(&state.db.conn, account.id).await?)
        } else {
            db::mailbox::get_inbox(&state.db.conn, account.id).await?
        };

        // Store blob
        let blob_id = db::blob::store(&state.db.conn, &state.db.blob_dir, account.id, &augmented_data).await?;

        // Find or create thread
        let thread_id = db::thread::find_or_create(
            &state.db.conn,
            account.id,
            message_id.as_deref(),
            in_reply_to.as_deref(),
        ).await?;

        // Create email record
        db::email::create(
            &state.db.conn,
            account.id,
            thread_id,
            &[target_mailbox],
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
            spam_score,
            spam_verdict.as_deref(),
        ).await?;

        let target = if spam_verdict.as_deref() == Some("SPAM") { "Junk" } else { "Inbox" };
        tracing::info!(
            to = %rcpt,
            subject = subject.as_deref().unwrap_or("(none)"),
            target,
            "Delivered inbound message"
        );

        // Vacation auto-reply (skip for spam, bounces, and Auto-Submitted messages)
        if spam_verdict.as_deref() != Some("SPAM")
            && !mail_from.is_empty()
            && !is_auto_submitted(&message)
        {
            if let Ok(Some(vr)) = db::vacation::is_active(&state.db.conn, account.id).await {
                let vacation_subject = vr.subject.unwrap_or_else(|| {
                    format!("Auto: {}", subject.as_deref().unwrap_or("Out of Office"))
                });
                let vacation_body = vr.text_body.unwrap_or_else(|| {
                    "I am currently out of the office and will respond when I return.".to_string()
                });

                let reply_msg = format!(
                    "From: <{rcpt}>\r\n\
                     To: <{mail_from}>\r\n\
                     Subject: {vacation_subject}\r\n\
                     Date: {}\r\n\
                     Message-ID: <vacation-{}@{}>\r\n\
                     In-Reply-To: {}\r\n\
                     Auto-Submitted: auto-replied\r\n\
                     MIME-Version: 1.0\r\n\
                     Content-Type: text/plain; charset=utf-8\r\n\
                     \r\n\
                     {vacation_body}",
                    chrono::Utc::now().to_rfc2822(),
                    uuid::Uuid::new_v4(),
                    state.config.hostname,
                    message_id.as_deref().unwrap_or(""),
                );

                // Queue the auto-reply for outbound delivery
                let reply_data = reply_msg.into_bytes();
                let reply_blob = db::blob::store(
                    &state.db.conn, &state.db.blob_dir, account.id, &reply_data,
                ).await;
                if let Ok(blob_id) = reply_blob {
                    let _ = crate::smtp::queue::enqueue(
                        &state.db.conn,
                        rcpt,
                        &[mail_from.to_string()],
                        blob_id,
                    ).await;
                    tracing::info!(to = %mail_from, "Vacation auto-reply queued");
                }
            }
        }
    }

    Ok(())
}

/// Verify SPF and DKIM authentication. Returns a summary string for Authentication-Results.
async fn verify_authentication(
    data: &[u8],
    mail_from: &str,
    remote_ip: IpAddr,
    ehlo_host: &str,
    hostname: &str,
) -> String {
    let mut results = Vec::new();

    let authenticator = match MessageAuthenticator::new_system_conf() {
        Ok(a) => a,
        Err(_) => match MessageAuthenticator::new_cloudflare_tls() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to create authenticator");
                return "dkim=temperror; spf=temperror".to_string();
            }
        }
    };

    // DKIM verification
    match AuthenticatedMessage::parse(data) {
        Some(auth_msg) => {
            let dkim_output = authenticator.verify_dkim(&auth_msg).await;
            let dkim_result = if dkim_output.iter().any(|r| r.result() == &DkimResult::Pass) {
                "pass"
            } else if dkim_output.is_empty() {
                "none"
            } else {
                "fail"
            };
            results.push(format!("dkim={dkim_result}"));
        }
        None => {
            results.push("dkim=none".to_string());
        }
    }

    // SPF verification
    let from_domain = mail_from.rsplit('@').next().unwrap_or(ehlo_host);
    let spf_params = if mail_from.is_empty() || !mail_from.contains('@') {
        SpfParameters::verify_ehlo(remote_ip, from_domain, hostname)
    } else {
        SpfParameters::verify_mail_from(remote_ip, from_domain, hostname, mail_from)
    };

    let spf_output = authenticator.verify_spf(spf_params).await;
    results.push(format!("spf={:?}", spf_output.result()));

    results.join("; ")
}

/// Check if a message has Auto-Submitted header (prevents vacation reply loops).
fn is_auto_submitted(message: &mail_parser::Message<'_>) -> bool {
    // Check for Auto-Submitted header (RFC 3834)
    for header in message.headers() {
        if header.name() == "Auto-Submitted" {
            if let HeaderValue::Text(val) = header.value() {
                if val.as_ref() != "no" {
                    return true;
                }
            }
        }
        // Also check Precedence: bulk/list/junk
        if header.name() == "Precedence" {
            if let HeaderValue::Text(val) = header.value() {
                let v = val.to_lowercase();
                if v == "bulk" || v == "list" || v == "junk" {
                    return true;
                }
            }
        }
    }
    false
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
