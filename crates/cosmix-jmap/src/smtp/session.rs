//! SMTP session state machine.
//!
//! Handles the SMTP conversation: EHLO → AUTH → MAIL FROM → RCPT TO → DATA.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::SmtpState;

/// SMTP session state.
struct Session {
    state: Arc<SmtpState>,
    peer: SocketAddr,
    require_auth: bool,
    authenticated_account: Option<i32>,
    mail_from: Option<String>,
    rcpt_to: Vec<String>,
    ehlo_host: Option<String>,
}

impl Session {
    fn new(state: Arc<SmtpState>, peer: SocketAddr, require_auth: bool) -> Self {
        Self {
            state,
            peer,
            require_auth,
            authenticated_account: None,
            mail_from: None,
            rcpt_to: Vec::new(),
            ehlo_host: None,
        }
    }

    fn reset_transaction(&mut self) {
        self.mail_from = None;
        self.rcpt_to.clear();
    }
}

/// Handle a single SMTP connection.
pub async fn handle(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<SmtpState>,
    require_auth: bool,
) -> Result<()> {
    tracing::debug!(peer = %peer, submission = require_auth, "SMTP session started");

    let mut session = Session::new(state, peer, require_auth);
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send greeting
    let hostname = &session.state.config.hostname;
    write_line(&mut writer, &format!("220 {hostname} ESMTP cosmix-jmap")).await?;

    let mut buf = Vec::with_capacity(4096);
    let mut data_mode = false;
    let mut data_buf = Vec::new();

    loop {
        // Read a line
        buf.clear();
        let n = read_line(&mut reader, &mut buf).await?;
        if n == 0 {
            break; // Connection closed
        }

        if data_mode {
            // Accumulate DATA until lone ".\r\n"
            if buf == b".\r\n" || buf == b".\n" {
                data_mode = false;
                let result = super::inbound::deliver(
                    &session.state,
                    session.authenticated_account,
                    session.mail_from.as_deref().unwrap_or(""),
                    &session.rcpt_to,
                    &data_buf,
                ).await;

                match result {
                    Ok(()) => {
                        write_line(&mut writer, "250 2.0.0 Message accepted").await?;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Delivery failed");
                        write_line(&mut writer, "451 4.3.0 Temporary delivery failure").await?;
                    }
                }
                session.reset_transaction();
                data_buf.clear();
            } else {
                // Dot-unstuffing: if line starts with ".." remove one dot
                if buf.starts_with(b"..") {
                    data_buf.extend_from_slice(&buf[1..]);
                } else {
                    data_buf.extend_from_slice(&buf);
                }

                // Size limit check
                if data_buf.len() > session.state.config.max_message_size {
                    data_mode = false;
                    data_buf.clear();
                    write_line(&mut writer, "552 5.3.4 Message too big").await?;
                    session.reset_transaction();
                }
            }
            continue;
        }

        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();

        // Parse SMTP command
        let (cmd, args) = match line.find(' ') {
            Some(pos) => (&line[..pos], line[pos + 1..].trim()),
            None => (line, ""),
        };

        match cmd.to_uppercase().as_str() {
            "EHLO" | "HELO" => {
                session.ehlo_host = Some(args.to_string());
                session.reset_transaction();
                let hostname = &session.state.config.hostname;
                let max_size = session.state.config.max_message_size;
                if cmd.eq_ignore_ascii_case("EHLO") {
                    write_line(&mut writer, &format!("250-{hostname}")).await?;
                    write_line(&mut writer, &format!("250-SIZE {max_size}")).await?;
                    write_line(&mut writer, "250-8BITMIME").await?;
                    write_line(&mut writer, "250-PIPELINING").await?;
                    if session.require_auth {
                        write_line(&mut writer, "250-AUTH PLAIN LOGIN").await?;
                    }
                    write_line(&mut writer, "250 ENHANCEDSTATUSCODES").await?;
                } else {
                    write_line(&mut writer, &format!("250 {hostname}")).await?;
                }
            }

            "AUTH" => {
                if !session.require_auth {
                    write_line(&mut writer, "502 5.5.1 AUTH not available on this port").await?;
                    continue;
                }
                if session.authenticated_account.is_some() {
                    write_line(&mut writer, "503 5.5.1 Already authenticated").await?;
                    continue;
                }

                let result = handle_auth(
                    args,
                    &session.state,
                    &mut reader,
                    &mut writer,
                ).await?;

                match result {
                    Some(account_id) => {
                        session.authenticated_account = Some(account_id);
                        write_line(&mut writer, "235 2.7.0 Authentication successful").await?;
                    }
                    None => {
                        write_line(&mut writer, "535 5.7.8 Authentication failed").await?;
                    }
                }
            }

            "MAIL" => {
                if session.ehlo_host.is_none() {
                    write_line(&mut writer, "503 5.5.1 Say EHLO first").await?;
                    continue;
                }
                if session.require_auth && session.authenticated_account.is_none() {
                    write_line(&mut writer, "530 5.7.0 Authentication required").await?;
                    continue;
                }

                // Parse "FROM:<addr>"
                let from = parse_mail_from(args);
                match from {
                    Some(addr) => {
                        session.mail_from = Some(addr);
                        write_line(&mut writer, "250 2.1.0 OK").await?;
                    }
                    None => {
                        write_line(&mut writer, "501 5.1.7 Bad sender address").await?;
                    }
                }
            }

            "RCPT" => {
                if session.mail_from.is_none() {
                    write_line(&mut writer, "503 5.5.1 MAIL FROM first").await?;
                    continue;
                }

                // Parse "TO:<addr>"
                let to = parse_rcpt_to(args);
                match to {
                    Some(addr) => {
                        // For inbound: check if recipient is local
                        if !session.require_auth {
                            let local = crate::db::account::get_by_email(
                                &session.state.db.pool,
                                &addr,
                            ).await;
                            match local {
                                Ok(Some(_)) => {
                                    session.rcpt_to.push(addr);
                                    write_line(&mut writer, "250 2.1.5 OK").await?;
                                }
                                _ => {
                                    write_line(&mut writer, "550 5.1.1 User not found").await?;
                                }
                            }
                        } else {
                            // Submission: allow any recipient
                            session.rcpt_to.push(addr);
                            write_line(&mut writer, "250 2.1.5 OK").await?;
                        }
                    }
                    None => {
                        write_line(&mut writer, "501 5.1.3 Bad recipient address").await?;
                    }
                }
            }

            "DATA" => {
                if session.mail_from.is_none() || session.rcpt_to.is_empty() {
                    write_line(&mut writer, "503 5.5.1 MAIL FROM and RCPT TO required").await?;
                    continue;
                }
                write_line(&mut writer, "354 Start mail input; end with <CRLF>.<CRLF>").await?;
                data_mode = true;
                data_buf.clear();
            }

            "RSET" => {
                session.reset_transaction();
                write_line(&mut writer, "250 2.0.0 OK").await?;
            }

            "NOOP" => {
                write_line(&mut writer, "250 2.0.0 OK").await?;
            }

            "QUIT" => {
                write_line(&mut writer, "221 2.0.0 Bye").await?;
                break;
            }

            _ => {
                write_line(&mut writer, "502 5.5.2 Command not recognized").await?;
            }
        }
    }

    tracing::debug!(peer = %session.peer, "SMTP session ended");
    Ok(())
}

/// Handle AUTH PLAIN or AUTH LOGIN.
async fn handle_auth<R, W>(
    args: &str,
    state: &SmtpState,
    reader: &mut R,
    writer: &mut W,
) -> Result<Option<i32>>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;

    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    let mechanism = parts[0].to_uppercase();

    match mechanism.as_str() {
        "PLAIN" => {
            let encoded = if parts.len() > 1 && !parts[1].is_empty() {
                parts[1].to_string()
            } else {
                // Send challenge
                write_line(writer, "334 ").await?;
                let mut buf = Vec::new();
                read_line(reader, &mut buf).await?;
                String::from_utf8_lossy(&buf).trim().to_string()
            };

            let decoded = b64.decode(&encoded)?;
            // AUTH PLAIN format: \0username\0password
            let parts: Vec<&[u8]> = decoded.splitn(3, |&b| b == 0).collect();
            if parts.len() < 3 {
                return Ok(None);
            }
            let username = std::str::from_utf8(parts[1])?;
            let password = std::str::from_utf8(parts[2])?;

            authenticate(state, username, password).await
        }

        "LOGIN" => {
            // Ask for username
            write_line(writer, "334 VXNlcm5hbWU6").await?; // "Username:" in base64
            let mut buf = Vec::new();
            read_line(reader, &mut buf).await?;
            let username = String::from_utf8(b64.decode(String::from_utf8_lossy(&buf).trim())?)?;

            // Ask for password
            write_line(writer, "334 UGFzc3dvcmQ6").await?; // "Password:" in base64
            buf.clear();
            read_line(reader, &mut buf).await?;
            let password = String::from_utf8(b64.decode(String::from_utf8_lossy(&buf).trim())?)?;

            authenticate(state, &username, &password).await
        }

        _ => {
            bail!("Unsupported auth mechanism: {mechanism}");
        }
    }
}

/// Authenticate against the accounts database.
async fn authenticate(state: &SmtpState, email: &str, password: &str) -> Result<Option<i32>> {
    let account = crate::db::account::get_by_email(&state.db.pool, email).await?;
    match account {
        Some(a) if a.password == password => Ok(Some(a.id)),
        _ => Ok(None),
    }
}

/// Parse MAIL FROM:<addr> or MAIL FROM: <addr>
fn parse_mail_from(args: &str) -> Option<String> {
    let upper = args.to_uppercase();
    if !upper.starts_with("FROM:") {
        return None;
    }
    let rest = args[5..].trim();
    extract_angle_addr(rest)
}

/// Parse RCPT TO:<addr> or RCPT TO: <addr>
fn parse_rcpt_to(args: &str) -> Option<String> {
    let upper = args.to_uppercase();
    if !upper.starts_with("TO:") {
        return None;
    }
    let rest = args[3..].trim();
    extract_angle_addr(rest)
}

/// Extract email from <addr> or bare addr, stripping ESMTP params.
fn extract_angle_addr(s: &str) -> Option<String> {
    if s.starts_with('<') {
        let end = s.find('>')?;
        let addr = &s[1..end];
        if addr.is_empty() {
            Some(String::new()) // null sender <>
        } else {
            Some(addr.to_lowercase())
        }
    } else {
        // Bare address — take up to first space (ESMTP params)
        let addr = s.split_whitespace().next()?;
        Some(addr.to_lowercase())
    }
}

/// Read a line (terminated by \n) from the stream.
async fn read_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<usize> {
    let mut byte = [0u8; 1];
    let mut total = 0;
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Ok(total);
        }
        buf.push(byte[0]);
        total += 1;
        if byte[0] == b'\n' {
            return Ok(total);
        }
        if total > 1024 * 1024 {
            bail!("Line too long");
        }
    }
}

/// Write a line with CRLF.
async fn write_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    line: &str,
) -> Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await?;
    Ok(())
}
