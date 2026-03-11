//! Outbound delivery worker — polls queue, delivers via SMTP, handles retries.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use hickory_resolver::{Resolver, TokioResolver};

use mail_auth::common::crypto::RsaKey;
use mail_auth::common::headers::HeaderWriter;
use mail_auth::dkim::DkimSigner;

use super::SmtpState;
use super::queue;
use crate::db;

/// Background delivery worker — polls the queue every 30 seconds.
pub async fn delivery_worker(state: Arc<SmtpState>) {
    tracing::info!("SMTP delivery worker started");

    let resolver = match Resolver::builder_tokio().map(|b| b.build()) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create DNS resolver — delivery worker disabled");
            return;
        }
    };

    loop {
        match process_queue(&state, &resolver).await {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(delivered = count, "Queue processing complete");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Queue processing error");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// Process all ready queue entries.
async fn process_queue(state: &SmtpState, resolver: &TokioResolver) -> Result<usize> {
    let entries = queue::fetch_ready(&state.db.pool, 50).await?;
    let mut delivered = 0;

    for entry in entries {
        // Load blob data
        let data = db::blob::load(&state.db.pool, &state.db.blob_dir, entry.blob_id).await?;
        let Some(data) = data else {
            tracing::error!(queue_id = entry.id, "Blob not found for queue entry");
            queue::mark_permanent_failure(&state.db.pool, entry.id, "blob not found").await?;
            continue;
        };

        // DKIM-sign the message if configured
        let data = if let Some(signed) = dkim_sign(state, &data) {
            signed
        } else {
            data
        };

        // Group recipients by domain for efficient delivery
        let by_domain = group_by_domain(&entry.to_addrs);

        let mut all_ok = true;
        for (domain, recipients) in &by_domain {
            match deliver_to_domain(
                resolver,
                &state.config.hostname,
                &entry.from_addr,
                recipients,
                domain,
                &data,
            ).await {
                Ok(()) => {
                    tracing::info!(
                        queue_id = entry.id,
                        domain = domain,
                        recipients = ?recipients,
                        "Delivered to domain"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        queue_id = entry.id,
                        domain = domain,
                        error = %e,
                        "Delivery to domain failed"
                    );
                    all_ok = false;
                    if entry.attempts >= 9 {
                        queue::mark_permanent_failure(
                            &state.db.pool,
                            entry.id,
                            &e.to_string(),
                        ).await?;
                    } else {
                        queue::mark_failed(&state.db.pool, entry.id, &e.to_string()).await?;
                    }
                }
            }
        }

        if all_ok {
            queue::mark_delivered(&state.db.pool, entry.id).await?;
            delivered += 1;
        }
    }

    Ok(delivered)
}

/// Deliver a message to all recipients at a specific domain via MX lookup.
async fn deliver_to_domain(
    resolver: &TokioResolver,
    helo_host: &str,
    from: &str,
    recipients: &[&String],
    domain: &str,
    data: &[u8],
) -> Result<()> {
    // MX lookup
    let mx_hosts = resolve_mx(resolver, domain).await?;
    if mx_hosts.is_empty() {
        anyhow::bail!("No MX records found for {domain}");
    }

    // Try MX hosts in preference order
    let mut last_error = None;
    for mx_host in &mx_hosts {
        match try_deliver(mx_host, 25, helo_host, from, recipients, data).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(mx = mx_host, error = %e, "MX delivery attempt failed");
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All MX hosts failed for {domain}")))
}

/// Resolve MX records for a domain, sorted by preference.
async fn resolve_mx(resolver: &TokioResolver, domain: &str) -> Result<Vec<String>> {
    match resolver.mx_lookup(domain).await {
        Ok(mx) => {
            let mut hosts: Vec<(u16, String)> = mx.iter()
                .map(|r| (r.preference(), r.exchange().to_ascii()))
                .collect();
            hosts.sort_by_key(|h| h.0);
            Ok(hosts.into_iter().map(|h| h.1.trim_end_matches('.').to_string()).collect())
        }
        Err(_) => {
            // Fall back to A/AAAA record on the domain itself
            Ok(vec![domain.to_string()])
        }
    }
}

/// Try to deliver to a specific SMTP host.
async fn try_deliver(
    host: &str,
    port: u16,
    helo_host: &str,
    from: &str,
    recipients: &[&String],
    data: &[u8],
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let addr = format!("{host}:{port}");
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        TcpStream::connect(&addr),
    ).await??;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let greeting = read_response(&mut reader).await?;
    if !greeting.starts_with('2') {
        anyhow::bail!("Bad greeting from {host}: {greeting}");
    }

    // EHLO
    send_cmd(&mut writer, &format!("EHLO {helo_host}")).await?;
    let resp = read_response(&mut reader).await?;
    if !resp.starts_with('2') {
        // Fall back to HELO
        send_cmd(&mut writer, &format!("HELO {helo_host}")).await?;
        let resp = read_response(&mut reader).await?;
        if !resp.starts_with('2') {
            anyhow::bail!("EHLO/HELO rejected by {host}: {resp}");
        }
    }

    // MAIL FROM
    send_cmd(&mut writer, &format!("MAIL FROM:<{from}>")).await?;
    let resp = read_response(&mut reader).await?;
    if !resp.starts_with('2') {
        anyhow::bail!("MAIL FROM rejected by {host}: {resp}");
    }

    // RCPT TO for each recipient
    for rcpt in recipients {
        send_cmd(&mut writer, &format!("RCPT TO:<{rcpt}>")).await?;
        let resp = read_response(&mut reader).await?;
        if !resp.starts_with('2') {
            tracing::warn!(rcpt = rcpt.as_str(), host = host, response = %resp, "RCPT TO rejected");
        }
    }

    // DATA
    send_cmd(&mut writer, "DATA").await?;
    let resp = read_response(&mut reader).await?;
    if !resp.starts_with('3') {
        anyhow::bail!("DATA rejected by {host}: {resp}");
    }

    // Send message body with dot-stuffing
    for line in data.split(|&b| b == b'\n') {
        if line.starts_with(b".") {
            writer.write_all(b".").await?;
        }
        writer.write_all(line).await?;
        if !line.ends_with(b"\r") {
            writer.write_all(b"\r").await?;
        }
        writer.write_all(b"\n").await?;
    }
    writer.write_all(b".\r\n").await?;
    writer.flush().await?;

    let resp = read_response(&mut reader).await?;
    if !resp.starts_with('2') {
        anyhow::bail!("Message rejected by {host}: {resp}");
    }

    // QUIT
    let _ = send_cmd(&mut writer, "QUIT").await;

    Ok(())
}

/// Send an SMTP command.
async fn send_cmd<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, cmd: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    writer.write_all(cmd.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Read a complete SMTP response (may be multi-line).
async fn read_response<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut result = String::new();
    let mut buf = [0u8; 1];
    let mut line = String::new();

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("Connection closed during response");
        }

        line.push(buf[0] as char);

        if line.ends_with('\n') {
            result.push_str(&line);
            // Multi-line: "250-..." continues, "250 ..." is final
            if line.len() >= 4 && line.as_bytes()[3] == b' ' {
                break;
            }
            line.clear();
        }
    }

    Ok(result.trim().to_string())
}

/// DKIM-sign a message if selector and private key are configured.
fn dkim_sign(state: &SmtpState, data: &[u8]) -> Option<Vec<u8>> {
    let selector = state.config.dkim_selector.as_deref()?;
    let key_path = state.config.dkim_private_key.as_deref()?;

    let key_pem = match std::fs::read_to_string(key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, path = key_path, "Failed to read DKIM private key");
            return None;
        }
    };

    let pk = match RsaKey::from_pkcs8_pem(&key_pem)
        .or_else(|_| RsaKey::from_rsa_pem(&key_pem))
    {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse DKIM RSA key");
            return None;
        }
    };

    let signer = DkimSigner::from_key(pk)
        .domain(&state.config.hostname)
        .selector(selector)
        .headers(["From", "To", "Subject", "Date", "Message-ID"]);

    match signer.sign(data) {
        Ok(signature) => {
            let header = signature.to_header();
            let mut signed = Vec::with_capacity(header.len() + data.len());
            signed.extend_from_slice(header.as_bytes());
            signed.extend_from_slice(data);
            Some(signed)
        }
        Err(e) => {
            tracing::error!(error = %e, "DKIM signing failed");
            None
        }
    }
}

/// Group email addresses by domain.
fn group_by_domain<'a>(addrs: &'a [String]) -> HashMap<String, Vec<&'a String>> {
    let mut map: HashMap<String, Vec<&'a String>> = HashMap::new();
    for addr in addrs {
        let domain = addr.rsplit('@').next().unwrap_or("localhost").to_lowercase();
        map.entry(domain).or_default().push(addr);
    }
    map
}
