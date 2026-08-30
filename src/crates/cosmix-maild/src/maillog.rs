//! Syslog-style, postfix/dovecot-compatible session + auth logging.
//!
//! ## Why this module exists
//!
//! `cosmix-maild` already logs through `tracing`, but the diagnostically
//! important events were unreachable from `journalctl`:
//!
//! - IMAP TLS-handshake failure and IMAP/SMTP session errors were logged
//!   at `tracing::debug!`. The systemd unit runs with
//!   `RUST_LOG=cosmix_maild=info`, so every one of those lines was
//!   filtered out before it reached the journal. A client that failed
//!   the TLS handshake (cert/protocol mismatch, e.g. Thunderbird) left
//!   **no trace at all**.
//! - SMTP `AUTH` success/failure was not logged anywhere.
//! - IMAP auth failure was a single terse `info` line with no method,
//!   no local IP, and no failure reason — not what an operator (or
//!   `fail2ban`/`logwatch`) expects.
//!
//! ## What it produces
//!
//! Lines shaped like dovecot `imap-login:` and postfix `postfix/smtpd:`
//! so existing mail-admin tooling and muscle memory transfer directly.
//! Every line is emitted via `tracing` at the **default target** (the
//! `cosmix_maild::maillog` module path) so the existing
//! `RUST_LOG=cosmix_maild=info` filter captures it without a unit
//! change. Connection/auth-success lines are `info`; auth-failure,
//! TLS-handshake-failure and session-error lines are `warn` so they
//! survive an `info` floor and stand out.
//!
//! ## Security
//!
//! `user`, SNI and error strings are attacker-controlled bytes off the
//! wire. They are passed through [`sanitize`] before formatting to
//! defend the journal against CRLF log-injection and Trojan-Source /
//! ANSI / bidi-control smuggling (cf. the `sanitize` Mix builtin and the
//! `feedback_mix_sanitize_untrusted_bytes` rule).

use std::net::SocketAddr;

/// Maximum length of a single sanitized field. Auth usernames are the
/// hostile surface; bound them so a megabyte "username" cannot bloat
/// the journal one line at a time.
const MAX_FIELD: usize = 256;

/// Escape an untrusted string into a single safe syslog field.
///
/// - C0/C1 control chars, `DEL`, CR, LF and TAB become `\xNN`
///   (this is what kills CRLF log-injection — the attacker cannot
///   start a forged log line).
/// - Unicode bidi/format/zero-width characters that can reorder or
///   hide text in a terminal (Trojan-Source class) become `\u{NNNN}`.
/// - Result is truncated to [`MAX_FIELD`] with a trailing `…` marker.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_FIELD) + 1);
    for ch in s.chars() {
        if out.len() >= MAX_FIELD {
            out.push('…');
            break;
        }
        match ch {
            // C0 controls (incl. CR/LF/TAB), and DEL.
            '\u{00}'..='\u{1F}' | '\u{7F}' => {
                out.push_str(&format!("\\x{:02X}", ch as u32));
            }
            // C1 controls.
            '\u{80}'..='\u{9F}'
            // Soft hyphen, line/paragraph separators, BOM/ZWNBSP.
            | '\u{00AD}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}'
            // The Unicode `Bidi_Control` set in full: ALM (U+061C),
            // LRM/RLM (U+200E/200F), the embedding/override block
            // (U+202A..U+202E) and the isolates (U+2066..U+2069).
            | '\u{061C}'
            | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            // Zero-width space/non-joiner/joiner + word joiner.
            | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' => {
                out.push_str(&format!("\\u{{{:04X}}}", ch as u32));
            }
            _ => out.push(ch),
        }
    }
    out
}

// --------------------------------------------------------------------
// IMAP — dovecot `imap-login:` shape
// --------------------------------------------------------------------

/// `rip=` / `lip=` clause shared by every dovecot-style line.
fn rip_lip(rip: SocketAddr, lip: Option<SocketAddr>) -> String {
    match lip {
        Some(l) => format!("rip={}, lip={}", rip.ip(), l.ip()),
        None => format!("rip={}", rip.ip()),
    }
}

/// Connection accepted and TLS terminated (port 993 is always TLS).
pub fn imap_connect(rip: SocketAddr, lip: Option<SocketAddr>) {
    tracing::info!("imap-login: Connect: {}, TLS", rip_lip(rip, lip));
}

/// Successful login. `method` is `PLAIN`, `LOGIN`, or `cleartext`.
pub fn imap_login_ok(user: &str, method: &str, rip: SocketAddr, lip: Option<SocketAddr>) {
    tracing::info!(
        "imap-login: Login: user=<{}>, method={}, {}, secured",
        sanitize(user),
        sanitize(method),
        rip_lip(rip, lip),
    );
}

/// Rejected credentials. `reason` is a short non-secret tag
/// (`auth failed`, `unknown user`, `server error`).
pub fn imap_login_fail(
    user: &str,
    method: &str,
    rip: SocketAddr,
    lip: Option<SocketAddr>,
    attempts: u32,
    reason: &str,
) {
    tracing::warn!(
        "imap-login: Login failed: user=<{}>, method={}, {} ({}, {} attempts)",
        sanitize(user),
        sanitize(method),
        rip_lip(rip, lip),
        sanitize(reason),
        attempts,
    );
}

/// TLS handshake failed before any IMAP bytes were exchanged. This is
/// the line that was previously `debug!` and therefore invisible.
pub fn imap_tls_handshake_fail(rip: SocketAddr, lip: Option<SocketAddr>, err: &str) {
    tracing::warn!(
        "imap-login: Disconnected (TLS handshaking): {} ({})",
        rip_lip(rip, lip),
        sanitize(err),
    );
}

/// Session ended. `reason` is `clean` for an orderly LOGOUT/EOF or a
/// sanitized error string for an abnormal end.
pub fn imap_disconnect(rip: SocketAddr, lip: Option<SocketAddr>, reason: &str) {
    if reason == "clean" {
        tracing::info!("imap-login: Disconnected: {}", rip_lip(rip, lip));
    } else {
        tracing::warn!(
            "imap-login: Disconnected ({}): {}",
            sanitize(reason),
            rip_lip(rip, lip),
        );
    }
}

// --------------------------------------------------------------------
// SMTP — postfix `postfix/smtpd:` shape
// --------------------------------------------------------------------

/// postfix renders the rDNS name then `[ip]`. The name comes from the
/// `rdns` cache only (strictly validated PTR, populated by the bounded
/// `rdns::resolve` await at connection start) — this function never
/// does network I/O, so it is safe on every log path including
/// accept-time error arms that run before any resolve.
fn client(rip: SocketAddr) -> String {
    match crate::rdns::cached(rip.ip()) {
        Some(name) => format!("{}[{}]", name, rip.ip()),
        None => format!("unknown[{}]", rip.ip()),
    }
}

/// Connection accepted (inbound :25 or submission :465/:587).
pub fn smtp_connect(rip: SocketAddr) {
    tracing::info!("postfix/smtpd: connect from {}", client(rip));
}

/// Successful SASL auth on the submission path.
pub fn smtp_sasl_ok(user: &str, method: &str, rip: SocketAddr) {
    tracing::info!(
        "postfix/smtpd: {}: client={}, sasl_method={}, sasl_username={}",
        client(rip),
        client(rip),
        sanitize(method),
        sanitize(user),
    );
}

/// Rejected SASL auth.
pub fn smtp_sasl_fail(method: &str, rip: SocketAddr) {
    tracing::warn!(
        "postfix/smtpd: warning: {}: SASL {} authentication failed",
        client(rip),
        sanitize(method),
    );
}

/// A rejected SMTP command — postfix `NOQUEUE: reject:` shape, so
/// fail2ban/logwatch-class tooling and operator muscle memory apply.
///
/// `stage` is the protocol verb the reject fired on (`MAIL`, `RCPT`,
/// `DATA`, `AUTH`, `end-of-data`); `reply` is the literal SMTP reply
/// already written to the client (status code first, so the reason is
/// grep-able); `from`/`to`/`helo` are included when known at that
/// stage. This is what makes a policy gate visible server-side —
/// `require_starttls_inbound`'s 530, for example, previously left no
/// trace beyond a bare connect/disconnect pair, so the only record of
/// WHY mail never arrived was the sender's own MTA log.
pub fn smtp_reject(
    rip: SocketAddr,
    stage: &str,
    reply: &str,
    from: Option<&str>,
    to: Option<&str>,
    helo: Option<&str>,
) {
    let mut ctx = String::new();
    if let Some(f) = from {
        ctx.push_str(&format!(" from=<{}>", sanitize(f)));
    }
    if let Some(t) = to {
        ctx.push_str(&format!(" to=<{}>", sanitize(t)));
    }
    if let Some(h) = helo {
        ctx.push_str(&format!(" helo=<{}>", sanitize(h)));
    }
    let sep = if ctx.is_empty() { "" } else { ";" };
    tracing::warn!(
        "postfix/smtpd: NOQUEUE: reject: {} from {}: {}{}{}",
        stage,
        client(rip),
        reply,
        sep,
        ctx,
    );
}

/// Message accepted at end-of-DATA (inbound MX or submission). The
/// postfix equivalent spans the smtpd `client=` and qmgr
/// `from=<…>, size=…, nrcpt=…` lines; maild assigns no queue id on
/// the inbound path, so one line carries the whole envelope summary.
pub fn smtp_accepted(rip: SocketAddr, from: &str, nrcpt: usize, size: usize) {
    tracing::info!(
        "postfix/smtpd: client={}, from=<{}>, nrcpt={}, size={}, status=accepted",
        client(rip),
        sanitize(from),
        nrcpt,
        size,
    );
}

/// Session ended. `reason` is `clean` or a sanitized error string.
pub fn smtp_disconnect(rip: SocketAddr, reason: &str) {
    if reason == "clean" {
        tracing::info!("postfix/smtpd: disconnect from {}", client(rip));
    } else {
        tracing::warn!(
            "postfix/smtpd: warning: {}: session error ({})",
            client(rip),
            sanitize(reason),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_escapes_crlf_log_injection() {
        // The classic attack: a "username" that forges a second line.
        let evil = "victim\r\nMay 15 imap-login: Login: user=<admin>";
        let clean = sanitize(evil);
        assert!(!clean.contains('\n'));
        assert!(!clean.contains('\r'));
        assert!(clean.contains("\\x0D\\x0A"));
    }

    #[test]
    fn sanitize_escapes_bidi_trojan_source() {
        let clean = sanitize("a\u{202E}b");
        assert_eq!(clean, "a\\u{202E}b");
        // ALM (U+061C) is part of Bidi_Control and must not slip
        // through invisibly.
        assert_eq!(sanitize("x\u{061C}y"), "x\\u{061C}y");
    }

    #[test]
    fn sanitize_passes_normal_text() {
        assert_eq!(
            sanitize("markc@alpha.example.org"),
            "markc@alpha.example.org"
        );
    }

    #[test]
    fn sanitize_truncates_oversized_field() {
        let huge = "x".repeat(MAX_FIELD * 4);
        let clean = sanitize(&huge);
        // MAX_FIELD chars then the ellipsis marker.
        assert!(clean.chars().count() <= MAX_FIELD + 1);
        assert!(clean.ends_with('…'));
    }
}
