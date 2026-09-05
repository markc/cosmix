//! SMTP session state machine.
//!
//! Handles the SMTP conversation: EHLO → STARTTLS → AUTH → MAIL FROM → RCPT TO → DATA.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use cosmix_props::store::StoreError;
use cosmix_props::{RecordKey, Runtime};
use rusqlite::Connection;
use rustls::server::ServerConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

use super::SmtpState;
use crate::props;
use crate::tls::SniCertResolver;

/// Per-connection TLS snapshot captured at TCP-accept time. The
/// listener calls [`crate::tls::current_tls_pair`] once per accepted
/// connection and threads the result through the session so a
/// mid-connection reload cannot tear the greeting/handshake hostname
/// pair. The resolver replays `pick_for_sni` post-handshake; the
/// `Arc<ServerConfig>` is reused as-is for a STARTTLS upgrade. `None`
/// means TLS is disabled at accept time — STARTTLS is not advertised
/// and the application-layer hostname falls through to
/// `config.hostname`.
pub(super) type TlsPair = (Arc<SniCertResolver>, Arc<ServerConfig>);

/// Smaller `max_message_size` for the token-shaped namespace (C8). A
/// post-by-email / command-by-email body is small; a token-namespace message over
/// this is accept-and-dropped (250, never 451/552) uniformly by SHAPE — not a
/// validity oracle. Bounds the accept-everything surface alongside the C7 rate cap.
const TOKEN_NAMESPACE_MAX_BYTES: usize = 256 * 1024;

/// SMTP session state.
struct Session {
    state: Arc<SmtpState>,
    peer: SocketAddr,
    require_auth: bool,
    /// When true, this inbound bind requires an active STARTTLS upgrade
    /// before a mail transaction is accepted — a plaintext `MAIL FROM`
    /// is rejected `530`. Set per-connection from
    /// `SmtpConfig::require_starttls_inbound` matched against the local
    /// bind address. Always false on the implicit-TLS (`:465`) path,
    /// where TLS is already active.
    require_starttls: bool,
    /// Per-connection TLS snapshot captured at TCP-accept time. The
    /// listener snapshots once per accepted connection and threads the
    /// pair here so the greeting hostname (resolved via the resolver)
    /// and a possible later STARTTLS upgrade (built from the cached
    /// `Arc<ServerConfig>`) cannot diverge under a concurrent reload.
    /// `None` means TLS was disabled at accept time; STARTTLS is not
    /// advertised and `pre_tls_hostname` falls through to
    /// `config.hostname`.
    tls: Option<TlsPair>,
    authenticated_account: Option<i32>,
    /// Email of the authenticated account (lowercased).
    ///
    /// On submission ports (`require_auth = true`) RFC 6409 §6.1 says
    /// the envelope sender MUST match this once AUTH succeeds. Stored
    /// here so the MAIL FROM handler can enforce that without
    /// re-querying the DB on every transaction.
    authenticated_email: Option<String>,
    mail_from: Option<String>,
    rcpt_to: Vec<String>,
    ehlo_host: Option<String>,
    tls_active: bool,
    /// Hostname advertised in the `220` greeting and the `250-<host>`
    /// EHLO/HELO response. SNI-resolved: on SMTPS (implicit TLS) and
    /// after a successful STARTTLS upgrade, the value is the name of
    /// the identity the resolver picked for the negotiated SNI (which
    /// the session recovers by re-running `pick_for_sni` on
    /// `ServerConnection::server_name()`). On the plaintext / pre-
    /// STARTTLS path the value is the no-SNI fallback identity's name
    /// — what the resolver would have served had a client connected
    /// without SNI. With TLS disabled entirely (empty identity list)
    /// or with no matching identity, the value falls through to
    /// `config.hostname`.
    negotiated_hostname: String,
}

impl Session {
    fn new(
        state: Arc<SmtpState>,
        peer: SocketAddr,
        require_auth: bool,
        tls_active: bool,
        tls: Option<TlsPair>,
        require_starttls: bool,
    ) -> Self {
        // Seed to the `default = true` identity name — this is the
        // pre-TLS application-layer hostname (plan §5: pre-STARTTLS
        // greetings use `default`, not `no_sni_fallback`). The two
        // can be split on multi-identity configs (e.g. external FQDN
        // as default, substrate-internal `*.bus` as no_sni_fallback).
        // `handle_tls` and the post-STARTTLS upgrade path both
        // immediately overwrite this with the SNI-resolved name.
        let negotiated_hostname = pre_tls_hostname(tls.as_ref(), &state.config.hostname);
        Self {
            state,
            peer,
            require_auth,
            require_starttls,
            tls,
            authenticated_account: None,
            authenticated_email: None,
            mail_from: None,
            rcpt_to: Vec::new(),
            ehlo_host: None,
            tls_active,
            negotiated_hostname,
        }
    }

    fn reset_transaction(&mut self) {
        self.mail_from = None;
        self.rcpt_to.clear();
    }

    fn tls_available(&self) -> bool {
        !self.tls_active && self.tls.is_some()
    }
}

/// Post-handshake hostname recovery. Replays the same `pick_for_sni`
/// policy the resolver used during the handshake so the application-
/// layer hostname matches the cert that was actually served. `sni =
/// Some(name)` is the typical post-handshake call with
/// `ServerConnection::server_name()`. `sni = None` returns the
/// `no_sni_fallback` identity name — what the resolver would have
/// served had a no-SNI client reached this code path. Falls through
/// to `fallback_hostname` when TLS is disabled (empty pair) or
/// strict-SNI rejected the match.
fn resolve_hostname(tls: Option<&TlsPair>, sni: Option<&str>, fallback_hostname: &str) -> String {
    tls.and_then(|(r, _)| r.pick_for_sni(sni))
        .map(|id| id.name.clone())
        .unwrap_or_else(|| fallback_hostname.to_string())
}

/// Pre-TLS application-layer hostname. The plan §5 reserves the
/// `default = true` identity for pre-STARTTLS SMTP greetings (and the
/// initial seed before the handshake completes on SMTPS, immediately
/// overwritten); pick from there directly rather than going through
/// `pick_for_sni(None)` which prefers `no_sni_fallback`. In single-
/// identity configs the two coincide via the "first becomes both"
/// rule.
fn pre_tls_hostname(tls: Option<&TlsPair>, fallback_hostname: &str) -> String {
    tls.and_then(|(r, _)| r.default_identity())
        .map(|id| id.name.clone())
        .unwrap_or_else(|| fallback_hostname.to_string())
}

/// True if the accepted connection's local bind address `local` matches
/// any `require_starttls_inbound` entry (so this bind requires an active
/// STARTTLS upgrade before mail is accepted). Comparison is by parsed
/// [`SocketAddr`]: an entry with an **unspecified** IP (`0.0.0.0` / `[::]`)
/// matches **any** local address on the same port (so a wildcard-bound
/// listener can be required), otherwise it is exact `SocketAddr` equality
/// (IPv6-spelling-insensitive). Entries are validated as socket addresses
/// at startup, so an unparseable entry can't reach here.
fn bind_requires_starttls(entries: &[String], local: SocketAddr) -> bool {
    entries.iter().any(|e| match e.parse::<SocketAddr>() {
        Ok(want) if want.ip().is_unspecified() => want.port() == local.port(),
        Ok(want) => want == local,
        Err(_) => false,
    })
}

/// Handle a plaintext SMTP connection (port 25) with optional STARTTLS.
pub async fn handle(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<SmtpState>,
    require_auth: bool,
    tls: Option<TlsPair>,
) -> Result<()> {
    // One bounded (1 s cap, cached) reverse lookup per connection so the
    // connect line — and every later line for this peer — can render
    // postfix-style `name[ip]`. Postfix performs the same lookup before
    // its own connect line.
    crate::rdns::resolve(peer.ip()).await;
    crate::maillog::smtp_connect(peer);

    // Per-bind STARTTLS requirement: if THIS listener's local address
    // matches `require_starttls_inbound`, a plaintext `MAIL FROM` on it
    // is rejected (530), so a multi-homed maild can require TLS on the
    // public bind while the mesh bind stays opportunistic. Matching is by
    // parsed `SocketAddr` (so IPv6 spelling can't cause a silent miss),
    // with an unspecified IP (`0.0.0.0`/`[::]`) meaning "any bind on this
    // port". Entries are validated as socket addresses at startup
    // (`start`), so a hostname/garbage entry fails the daemon loudly
    // rather than silently disabling enforcement here. A `local_addr()`
    // failure (already-broken socket) degrades to "not required" — the
    // connection is about to fail anyway.
    let require_starttls = match stream.local_addr() {
        Ok(local) => bind_requires_starttls(&state.config.require_starttls_inbound, local),
        Err(_) => false,
    };

    // Disable Nagle on the inbound plaintext socket. This is the PRIMARY
    // guard against the STARTTLS "wrong version number" corruption: the
    // upgrade hands THIS SAME socket to the TLS acceptor, so any plaintext
    // control byte still queued by Nagle when the handshake begins would
    // bleed into the TLS stream (see `write_line`). With TCP_NODELAY every
    // plaintext response byte is on the wire immediately, so the
    // "220 ... Ready to start TLS\r\n" terminator can never be delayed
    // past the client's ClientHello regardless of how `write_all` framed
    // it. `write_line`'s single-segment framing reinforces this by keeping
    // the line + CRLF in one write in the common (idle send buffer) case.
    if let Err(e) = stream.set_nodelay(true) {
        if tls.is_some() {
            // STARTTLS is on offer and we cannot guarantee Nagle is off —
            // refuse rather than risk a corrupted handshake. set_nodelay
            // only fails on an already-broken socket, so this is a
            // belt-and-suspenders that should never fire in practice.
            bail!("set_nodelay(true) failed on a STARTTLS-capable :25 socket ({peer}): {e}");
        }
        // Plaintext-only (no STARTTLS): no TLS-bleed risk, non-fatal.
        tracing::debug!(%peer, error = %e, "set_nodelay failed on plaintext SMTP socket (no STARTTLS)");
    }

    let mut session = Session::new(state, peer, require_auth, false, tls, require_starttls);
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send greeting using the no-SNI fallback identity name (set by
    // `Session::new` via `pre_tls_hostname`).
    write_line(
        &mut writer,
        &format!("220 {} ESMTP cosmix-jmap", session.negotiated_hostname),
    )
    .await?;

    // Run the session — may return with a request to upgrade to TLS
    let upgrade = run_session(&mut session, &mut reader, &mut writer).await?;

    if upgrade {
        // Perform TLS upgrade using the per-connection ServerConfig
        // snapshot captured at accept time. A reload that fires after
        // accept but before STARTTLS leaves this handshake on the old
        // stamp; the old `Arc<ServerConfig>` keeps the old resolver
        // alive via `with_cert_resolver`.
        let server_config = session
            .tls
            .as_ref()
            .map(|(_, cfg)| cfg.clone())
            .expect("STARTTLS without per-connection TLS pair");
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config);

        // Reassemble the TcpStream from split halves
        let tcp_stream = reader.into_inner().reunite(writer)?;
        let tls_stream = acceptor.accept(tcp_stream).await?;
        // Recover the SNI the client sent so the post-STARTTLS EHLO
        // hostname matches the cert that was actually served. Done
        // before `tokio::io::split` because the split halves drop the
        // `ServerConnection` handle.
        let sni = tls_stream.get_ref().1.server_name().map(|s| s.to_string());
        let (tls_reader, mut tls_writer) = tokio::io::split(tls_stream);
        let mut tls_reader = BufReader::new(tls_reader);

        let fallback = session.state.config.hostname.clone();
        session.tls_active = true;
        session.negotiated_hostname =
            resolve_hostname(session.tls.as_ref(), sni.as_deref(), &fallback);
        // Reset EHLO state per RFC 3207
        session.ehlo_host = None;
        session.reset_transaction();

        run_session(&mut session, &mut tls_reader, &mut tls_writer).await?;
    }

    crate::maillog::smtp_disconnect(session.peer, "clean");
    Ok(())
}

/// Handle an implicit TLS connection (port 465 SMTPS) — already encrypted.
pub async fn handle_tls(
    tls_stream: TlsStream<TcpStream>,
    peer: SocketAddr,
    state: Arc<SmtpState>,
    tls: TlsPair,
) -> Result<()> {
    // See `handle`: bounded cached PTR resolve before the connect line.
    crate::rdns::resolve(peer.ip()).await;
    crate::maillog::smtp_connect(peer);

    // Recover the SNI the client sent so the greeting hostname matches
    // the cert that was actually served. Done before
    // `tokio::io::split` because the split halves drop the
    // `ServerConnection` handle.
    let sni = tls_stream.get_ref().1.server_name().map(|s| s.to_string());
    // Implicit-TLS (:465): TLS is already active, so require_starttls is moot.
    let mut session = Session::new(state, peer, true, true, Some(tls), false);
    let fallback = session.state.config.hostname.clone();
    session.negotiated_hostname = resolve_hostname(session.tls.as_ref(), sni.as_deref(), &fallback);
    let (reader, mut writer) = tokio::io::split(tls_stream);
    let mut reader = BufReader::new(reader);

    write_line(
        &mut writer,
        &format!("220 {} ESMTP cosmix-jmap", session.negotiated_hostname),
    )
    .await?;

    // Run session — STARTTLS will not be offered (already encrypted)
    run_session(&mut session, &mut reader, &mut writer).await?;

    crate::maillog::smtp_disconnect(peer, "clean");
    Ok(())
}

/// Run the SMTP command loop. Returns true if STARTTLS upgrade was requested.
async fn run_session<R, W>(
    session: &mut Session,
    reader: &mut BufReader<R>,
    writer: &mut W,
) -> Result<bool>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(4096);
    let mut data_mode = false;
    let mut data_buf = Vec::new();
    // On the submission path, a DATA line that isn't strictly CRLF-framed
    // sets this; we drain to the terminator (bounded by max_message_size)
    // then reject the whole message rather than desync the protocol.
    let mut data_invalid = false;

    loop {
        buf.clear();
        let n = read_line(reader, &mut buf).await?;
        if n == 0 {
            break;
        }

        if data_mode {
            // End-of-DATA. The submission path (`require_auth`) accepts
            // ONLY the strict `.\r\n` terminator; the public MX path also
            // tolerates a bare-LF `.\n` (SMTP-smuggling guard scoped to
            // submission so we don't bounce mail from senders we can't
            // control — see `data_line_crlf_strict`).
            let is_terminator = buf == b".\r\n" || (!session.require_auth && buf == b".\n");
            if is_terminator {
                data_mode = false;
                // A bad line earlier in this submission DATA → reject the
                // whole message rather than deliver a smuggling-ambiguous
                // one.
                if data_invalid {
                    data_invalid = false;
                    data_buf.clear();
                    crate::maillog::smtp_reject(
                        session.peer,
                        "end-of-data",
                        "550 5.6.0 Bare newline or CR in DATA",
                        session.mail_from.as_deref(),
                        None,
                        session.ehlo_host.as_deref(),
                    );
                    write_line(
                        writer,
                        "550 5.6.0 Bare newline or CR in DATA; submission requires CRLF line endings",
                    )
                    .await?;
                    session.reset_transaction();
                    continue;
                }
                // Classify the sender's authentication from the SMTP SESSION (never
                // from message headers) for the vtoken sender-lock. A successful
                // AUTH on a submission port is genuine mailbox-owner identity;
                // everything else (MX inbound) is external-unverified.
                let ingress = match (
                    session.authenticated_account,
                    session.authenticated_email.as_deref(),
                ) {
                    (Some(account_id), Some(email)) => super::inbound::IngressAuth::LocalAuth {
                        account_id,
                        email: email.to_string(),
                    },
                    _ => super::inbound::IngressAuth::ExternalUnverified,
                };
                // C8: fire-and-forget for an ALL-token-shaped message closes the
                // timing oracle. We write 250 IMMEDIATELY — no resolve/delivery
                // work and NO permit await before it (either re-introduces a
                // load-dependent timing channel) — then dispatch on a bounded,
                // SHED-on-overflow background task. The token namespace never
                // returns 451/552 from delivery; an oversize message is 250+dropped.
                // (Mixed/normal recipients keep the synchronous path below.)
                // Flag-aware: when the C9 pre-flight disabled opaque acceptance,
                // an opaque-shaped recipient is a NORMAL mailbox and must take the
                // synchronous path (not fire-and-forget accept-drop), so a real
                // mailbox of the opaque shape is delivered, never silently dropped.
                let opaque_enabled = session.state.config.opaque_rcpt_enabled;
                let all_token_shaped = !session.rcpt_to.is_empty()
                    && session
                        .rcpt_to
                        .iter()
                        .all(|r| crate::vtoken::is_active_token_shaped(r, opaque_enabled));
                if all_token_shaped {
                    crate::maillog::smtp_accepted(
                        session.peer,
                        session.mail_from.as_deref().unwrap_or(""),
                        session.rcpt_to.len(),
                        data_buf.len(),
                    );
                    write_line(writer, "250 2.0.0 Message accepted").await?;
                    if data_buf.len() <= TOKEN_NAMESPACE_MAX_BYTES {
                        // try_acquire — NEVER await a permit before the 250 (the
                        // timing-channel BLOCKER); shed on overflow, still 250.
                        match session.state.token_dispatch_sem.clone().try_acquire_owned() {
                            Ok(permit) => {
                                let state = session.state.clone();
                                let ingress = ingress.clone();
                                let from = session.mail_from.clone().unwrap_or_default();
                                let rcpts = session.rcpt_to.clone();
                                let data = data_buf.clone();
                                let ehlo = session
                                    .ehlo_host
                                    .clone()
                                    .unwrap_or_else(|| "unknown".to_string());
                                let ip = session.peer.ip();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(e) = super::inbound::deliver(
                                        &state, &ingress, &from, &rcpts, &data, ip, &ehlo,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            error = %e,
                                            "async token dispatch failed; dead-lettered (no SMTP effect)"
                                        );
                                    }
                                });
                            }
                            Err(_) => {
                                tracing::warn!(
                                    "token dispatch at capacity; accept-and-drop (250 already sent)"
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            size = data_buf.len(),
                            "token-namespace message over size cap; accept-and-drop"
                        );
                    }
                    session.reset_transaction();
                    data_buf.clear();
                    continue;
                }

                // Mixed / all-normal: synchronous delivery — normal recipients
                // need their real result; mixed-message token-splitting is deferred.
                let result = super::inbound::deliver(
                    &session.state,
                    &ingress,
                    session.mail_from.as_deref().unwrap_or(""),
                    &session.rcpt_to,
                    &data_buf,
                    session.peer.ip(),
                    session.ehlo_host.as_deref().unwrap_or("unknown"),
                )
                .await;

                match result {
                    Ok(()) => {
                        crate::maillog::smtp_accepted(
                            session.peer,
                            session.mail_from.as_deref().unwrap_or(""),
                            session.rcpt_to.len(),
                            data_buf.len(),
                        );
                        write_line(writer, "250 2.0.0 Message accepted").await?;
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "Delivery failed: {}",
                            crate::maillog::sanitize(&e.to_string()),
                        );
                        crate::maillog::smtp_reject(
                            session.peer,
                            "end-of-data",
                            "451 4.3.0 Temporary delivery failure",
                            session.mail_from.as_deref(),
                            None,
                            session.ehlo_host.as_deref(),
                        );
                        write_line(writer, "451 4.3.0 Temporary delivery failure").await?;
                    }
                }
                session.reset_transaction();
                data_buf.clear();
            } else {
                // Submission: flag a non-CRLF / bare-CR line. We keep
                // accumulating (bounded by the size cap below) so the
                // protocol stays in sync; rejection happens at the
                // terminator.
                if session.require_auth && !data_line_crlf_strict(&buf) {
                    data_invalid = true;
                }
                // Dot-unstuffing
                if buf.starts_with(b"..") {
                    data_buf.extend_from_slice(&buf[1..]);
                } else {
                    data_buf.extend_from_slice(&buf);
                }

                if data_buf.len() > session.state.config.max_message_size {
                    data_mode = false;
                    data_invalid = false;
                    data_buf.clear();
                    crate::maillog::smtp_reject(
                        session.peer,
                        "DATA",
                        "552 5.3.4 Message too big",
                        session.mail_from.as_deref(),
                        None,
                        session.ehlo_host.as_deref(),
                    );
                    write_line(writer, "552 5.3.4 Message too big").await?;
                    session.reset_transaction();
                }
            }
            continue;
        }

        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();

        let (cmd, args) = match line.find(' ') {
            Some(pos) => (&line[..pos], line[pos + 1..].trim()),
            None => (line, ""),
        };

        match cmd.to_uppercase().as_str() {
            "EHLO" | "HELO" => {
                session.ehlo_host = Some(args.to_string());
                session.reset_transaction();
                let hostname = &session.negotiated_hostname;
                let max_size = session.state.config.max_message_size;
                if cmd.eq_ignore_ascii_case("EHLO") {
                    write_line(writer, &format!("250-{hostname}")).await?;
                    write_line(writer, &format!("250-SIZE {max_size}")).await?;
                    write_line(writer, "250-8BITMIME").await?;
                    write_line(writer, "250-PIPELINING").await?;
                    if session.tls_available() {
                        write_line(writer, "250-STARTTLS").await?;
                    }
                    if session.tls_active && session.require_auth {
                        write_line(writer, "250-AUTH PLAIN LOGIN").await?;
                    }
                    write_line(writer, "250 ENHANCEDSTATUSCODES").await?;
                } else {
                    write_line(writer, &format!("250 {hostname}")).await?;
                }
            }

            "STARTTLS" => {
                if !session.tls_available() {
                    write_line(writer, "502 5.5.1 STARTTLS not available").await?;
                    continue;
                }
                write_line(writer, "220 2.0.0 Ready to start TLS").await?;
                return Ok(true); // Signal caller to upgrade
            }

            "AUTH" => {
                if !session.require_auth {
                    write_line(writer, "502 5.5.1 AUTH not available on this port").await?;
                    continue;
                }
                if session.authenticated_account.is_some() {
                    write_line(writer, "503 5.5.1 Already authenticated").await?;
                    continue;
                }
                if !session.tls_active {
                    crate::maillog::smtp_reject(
                        session.peer,
                        "AUTH",
                        "530 5.7.0 Must use TLS",
                        None,
                        None,
                        session.ehlo_host.as_deref(),
                    );
                    write_line(writer, "530 5.7.0 Must use TLS").await?;
                    continue;
                }

                let mechanism = args.split(' ').next().unwrap_or("").to_uppercase();
                let result = handle_auth(args, &session.state, reader, writer).await?;

                match result {
                    Some((account_id, account_email)) => {
                        crate::maillog::smtp_sasl_ok(&account_email, &mechanism, session.peer);
                        session.authenticated_account = Some(account_id);
                        session.authenticated_email = Some(account_email.to_lowercase());
                        write_line(writer, "235 2.7.0 Authentication successful").await?;
                    }
                    None => {
                        crate::maillog::smtp_sasl_fail(&mechanism, session.peer);
                        write_line(writer, "535 5.7.8 Authentication failed").await?;
                    }
                }
            }

            "MAIL" => {
                if session.ehlo_host.is_none() {
                    write_line(writer, "503 5.5.1 Say EHLO first").await?;
                    continue;
                }
                // Parsed up front (pure, no side effects) so every reject
                // below can log the attempted envelope sender; an
                // unparsable argument falls back to the raw bytes
                // (sanitized inside `smtp_reject`).
                let from = parse_mail_from(args);
                let from_log = from.as_deref().unwrap_or(args);
                // Per-bind STARTTLS requirement (e.g. a public :25 listed
                // in `require_starttls_inbound`): refuse a plaintext
                // transaction. STARTTLS was advertised in the EHLO reply;
                // the client must upgrade before MAIL FROM.
                if session.require_starttls && !session.tls_active {
                    crate::maillog::smtp_reject(
                        session.peer,
                        "MAIL",
                        "530 5.7.0 Must issue STARTTLS first",
                        Some(from_log),
                        None,
                        session.ehlo_host.as_deref(),
                    );
                    write_line(writer, "530 5.7.0 Must issue STARTTLS first").await?;
                    continue;
                }
                if session.require_auth && session.authenticated_account.is_none() {
                    crate::maillog::smtp_reject(
                        session.peer,
                        "MAIL",
                        "530 5.7.0 Authentication required",
                        Some(from_log),
                        None,
                        session.ehlo_host.as_deref(),
                    );
                    write_line(writer, "530 5.7.0 Authentication required").await?;
                    continue;
                }

                match from {
                    Some(addr) => {
                        // RFC 6409 §6.1: on a submission port the
                        // envelope sender MUST match the authenticated
                        // identity — OR be an enabled alias OF that
                        // identity (Phase 1 aliases: `admin@` may send
                        // `From: mc@` when `mc@ → admin@`). Empty sender
                        // (`MAIL FROM:<>`, bounces) has no authed identity
                        // to match and is rejected here too — bounces
                        // don't belong on a submission port.
                        if let Some(authed) = session.authenticated_email.as_deref()
                            && !props::aliases::sender_authorized(
                                &session.state.aliases_runtime,
                                &addr,
                                authed,
                            )
                            .await
                        {
                            crate::maillog::smtp_reject(
                                session.peer,
                                "MAIL",
                                "530 5.7.1 Sender address must match authenticated identity",
                                Some(&addr),
                                None,
                                session.ehlo_host.as_deref(),
                            );
                            write_line(
                                writer,
                                "530 5.7.1 Sender address must match authenticated identity",
                            )
                            .await?;
                            continue;
                        }
                        session.mail_from = Some(addr);
                        write_line(writer, "250 2.1.0 OK").await?;
                    }
                    None => {
                        crate::maillog::smtp_reject(
                            session.peer,
                            "MAIL",
                            "501 5.1.7 Bad sender address",
                            Some(args),
                            None,
                            session.ehlo_host.as_deref(),
                        );
                        write_line(writer, "501 5.1.7 Bad sender address").await?;
                    }
                }
            }

            "RCPT" => {
                if session.mail_from.is_none() {
                    write_line(writer, "503 5.5.1 MAIL FROM first").await?;
                    continue;
                }

                let decision = decide_rcpt_to(
                    &session.state.domains_runtime,
                    &session.state.aliases_runtime,
                    &session.state.db.conn,
                    session.require_auth,
                    args,
                    session.state.config.opaque_rcpt_enabled,
                )
                .await;
                match decision {
                    RcptDecision::Accept(addr) => {
                        session.rcpt_to.push(addr);
                        write_line(writer, "250 2.1.5 OK").await?;
                    }
                    RcptDecision::PermanentReject(msg) | RcptDecision::TransientReject(msg) => {
                        crate::maillog::smtp_reject(
                            session.peer,
                            "RCPT",
                            msg,
                            session.mail_from.as_deref(),
                            Some(parse_rcpt_to(args).as_deref().unwrap_or(args)),
                            session.ehlo_host.as_deref(),
                        );
                        write_line(writer, msg).await?;
                    }
                }
            }

            "DATA" => {
                if session.mail_from.is_none() || session.rcpt_to.is_empty() {
                    write_line(writer, "503 5.5.1 MAIL FROM and RCPT TO required").await?;
                    continue;
                }
                write_line(writer, "354 Start mail input; end with <CRLF>.<CRLF>").await?;
                data_mode = true;
                data_invalid = false;
                data_buf.clear();
            }

            "RSET" => {
                session.reset_transaction();
                write_line(writer, "250 2.0.0 OK").await?;
            }

            "NOOP" => {
                write_line(writer, "250 2.0.0 OK").await?;
            }

            "QUIT" => {
                write_line(writer, "221 2.0.0 Bye").await?;
                break;
            }

            _ => {
                write_line(writer, "502 5.5.2 Command not recognized").await?;
            }
        }
    }

    Ok(false)
}

/// Handle AUTH PLAIN or AUTH LOGIN.
async fn handle_auth<R, W>(
    args: &str,
    state: &SmtpState,
    reader: &mut R,
    writer: &mut W,
) -> Result<Option<(i32, String)>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
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

/// Authenticate against the accounts database using bcrypt.
///
/// Returns `(account_id, account_email)` on success — the email is
/// the canonical address stored on the account row, which the MAIL
/// FROM handler binds against (RFC 6409 §6.1). It is *not* the
/// `username` argument: a client that submits a mixed-case username
/// must still be bound against the canonical form so the sender check
/// is comparing apples to apples.
///
/// Constant-work on the unknown-account branch: a bcrypt verify runs
/// against a process-local dummy hash whether or not the account
/// exists, so AUTH response timing cannot enumerate mailboxes on the
/// submission port (an early return on the `None` arm was a timing
/// oracle — 2026-07 audit).
async fn authenticate(
    state: &SmtpState,
    email: &str,
    password: &str,
) -> Result<Option<(i32, String)>> {
    let account = crate::db::account::get_by_email(&state.db.conn, email).await?;
    match account {
        Some(a) => {
            let hash = a.password.clone();
            // Run bcrypt verify on blocking thread to avoid stalling async runtime
            let pwd = password.to_string();
            let valid =
                tokio::task::spawn_blocking(move || bcrypt::verify(&pwd, &hash).unwrap_or(false))
                    .await
                    .unwrap_or(false);
            if valid {
                Ok(Some((a.id, a.email)))
            } else {
                Ok(None)
            }
        }
        None => {
            // Burn the same bcrypt cost as the account-exists arm.
            let pwd = password.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                bcrypt::verify(&pwd, dummy_bcrypt_hash()).unwrap_or(false)
            })
            .await;
            Ok(None)
        }
    }
}

/// Process-local dummy bcrypt hash at `DEFAULT_COST` — the same cost real
/// account hashes are created with — computed once at first unknown-account
/// AUTH. Deriving it (rather than a hardcoded literal) keeps the dummy's
/// cost in lockstep if `DEFAULT_COST` ever changes.
fn dummy_bcrypt_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        bcrypt::hash("cosmix-timing-equalizer", bcrypt::DEFAULT_COST)
            .unwrap_or_else(|_| String::new())
    })
}

/// Parse `MAIL FROM:<addr>` or `MAIL FROM: <addr>`
fn parse_mail_from(args: &str) -> Option<String> {
    let upper = args.to_uppercase();
    if !upper.starts_with("FROM:") {
        return None;
    }
    let rest = args[5..].trim();
    extract_angle_addr(rest)
}

/// Parse `RCPT TO:<addr>` or `RCPT TO: <addr>`
fn parse_rcpt_to(args: &str) -> Option<String> {
    let upper = args.to_uppercase();
    if !upper.starts_with("TO:") {
        return None;
    }
    let rest = args[3..].trim();
    extract_angle_addr(rest)
}

/// Extract email from `<addr>` or bare addr, stripping ESMTP params.
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
        let addr = s.split_whitespace().next()?;
        Some(addr.to_lowercase())
    }
}

/// Outcome of a single RCPT TO state-machine evaluation.
///
/// `Accept` carries the parsed recipient address so the caller pushes
/// it onto `session.rcpt_to`; reject variants carry the literal SMTP
/// reply the caller writes verbatim. Splitting permanent vs transient
/// at the type level keeps the substrate-error tests honest — a
/// substrate read error must be transient (so the MTA retries),
/// substrate `NotFound` must be permanent.
#[derive(Debug, PartialEq, Eq)]
enum RcptDecision {
    Accept(String),
    PermanentReject(&'static str),
    TransientReject(&'static str),
}

/// Role-aware RCPT TO state machine (Phase 3 commit 2).
///
/// Pure of the [`Session`] type so Bucket B tests can hit it directly
/// against an in-memory [`Runtime`] + an in-memory [`Db`] without
/// constructing a full `SmtpState`. The caller in `run_session` is the
/// only production callsite and is responsible for pushing the parsed
/// address onto `session.rcpt_to` on `Accept`.
///
/// Cutover rules:
///
/// - `require_auth = true` (submission ports) — bypasses the substrate
///   entirely; the AUTH credential is the gate, the outbound path
///   handles the recipient downstream.
/// - Substrate `list(domains)` errors → `451 4.3.0` transient. The
///   reconcile diagnostic exists to surface chronic read failures;
///   the RCPT path fails closed so the sender's MTA retries.
/// - Empty `maild.domains` namespace → legacy account lookup
///   (pre-Phase-3 behavior preserved for un-cutover deployments).
/// - Substrate `get(domain)` `NotFound` → `550 5.1.2` (domain not
///   hosted here); non-`NotFound` errors → `451 4.3.0` transient.
/// - Row exists but `view(...)` errs → `451 4.3.0` (on-disk corruption
///   / post-write schema drift; surface it rather than silently fall
///   back, since the substrate validated the row at write time).
/// - `enabled = false` → `550 5.1.2` (operator explicitly disabled).
/// - `role = "authoritative"` → legacy account lookup.
/// - `role = "backup_mx"` / `"relay"` / unknown → `550 5.7.1` (deferred
///   posture; the backup_mx follow-up flips the backup_mx arm to
///   accept-and-enqueue once the queue-schema change lands).
async fn decide_rcpt_to(
    domains_runtime: &Arc<Runtime>,
    aliases_runtime: &Arc<Runtime>,
    db_conn: &Arc<Mutex<Connection>>,
    require_auth: bool,
    to_arg: &str,
    opaque_rcpt_enabled: bool,
) -> RcptDecision {
    let addr = match parse_rcpt_to(to_arg) {
        Some(a) => a,
        None => return RcptDecision::PermanentReject("501 5.1.3 Bad recipient address"),
    };

    // AUTH gate: submission-port sessions bypass the substrate check.
    if require_auth {
        return RcptDecision::Accept(addr);
    }

    let recipient_domain = match addr.rsplit_once('@') {
        Some((_, d)) => d.to_ascii_lowercase(),
        None => return RcptDecision::PermanentReject("550 5.1.3 Bad recipient address"),
    };

    // Opaque vtoken acceptance (C9). Gated on the startup pre-flight flag so an
    // opaque-shaped RCPT can never swallow a real mailbox of the same shape; the
    // local-part is the shape unit (`is_opaque_shaped` ignores the domain).
    let opaque_rcpt_ok = opaque_rcpt_enabled
        && addr
            .rsplit_once('@')
            .is_some_and(|(local, _)| crate::vtoken::is_opaque_shaped(local));

    // Phase 3 cutover rule: empty namespace → legacy; read error →
    // transient (NOT silent fallback to legacy — the operator may
    // have intended substrate-authoritative semantics and the row
    // simply failed to load).
    let namespace_empty = match domains_runtime
        .store()
        .list(&props::domains::namespace_name())
        .await
    {
        Ok(snap) => snap.value.is_empty(),
        Err(_) => return RcptDecision::TransientReject("451 4.3.0 Temporary system error"),
    };

    // Phase 1 aliases: resolve the recipient through `maild.aliases` for
    // the account-existence check, but Accept the ORIGINAL address so the
    // envelope keeps what the sender wrote (delivery re-resolves in
    // `inbound.rs`). A transient alias-store error is a retriable 451 —
    // NOT a permanent 550 that would bounce a valid alias.
    let resolved = match props::aliases::resolve_recipient(aliases_runtime, &addr).await {
        Ok(r) => r,
        Err(_) => return RcptDecision::TransientReject("451 4.3.0 Temporary system error"),
    };

    if namespace_empty {
        return match crate::db::account::get_by_email(db_conn, &resolved).await {
            Ok(Some(_)) => RcptDecision::Accept(addr),
            // opaque vtoken-shaped recipient → accept opaquely (anti-enumeration);
            // validated/discarded after DATA. See the authoritative arm.
            _ if opaque_rcpt_ok => RcptDecision::Accept(addr),
            _ => RcptDecision::PermanentReject("550 5.1.1 User not found"),
        };
    }

    let record = match domains_runtime
        .store()
        .get(&RecordKey::collection(
            props::domains::namespace_name(),
            recipient_domain.clone(),
        ))
        .await
    {
        Ok(snap) => snap.value,
        Err(StoreError::NotFound) => {
            return RcptDecision::PermanentReject("550 5.1.2 No such domain here");
        }
        Err(_) => {
            return RcptDecision::TransientReject("451 4.3.0 Temporary system error");
        }
    };

    let view = match props::domains::view(&record) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                domain = %recipient_domain,
                error = %e,
                "maild.domains row failed view projection — fail-closed"
            );
            return RcptDecision::TransientReject("451 4.3.0 Temporary system error");
        }
    };

    if !view.enabled {
        return RcptDecision::PermanentReject("550 5.1.2 No such domain here");
    }

    match view.role.as_str() {
        "authoritative" => {
            // Existence check on the alias-resolved address; Accept the
            // original envelope address (resolved computed once above).
            match crate::db::account::get_by_email(db_conn, &resolved).await {
                Ok(Some(_)) => RcptDecision::Accept(addr),
                // An opaque vtoken-shaped recipient on a domain we host is
                // accepted opaquely — no registry consult, so RCPT reveals
                // nothing about token validity (the anti-enumeration property).
                // `deliver()` (inbound.rs) validates after DATA and silently
                // discards an invalid token.
                _ if opaque_rcpt_ok => RcptDecision::Accept(addr),
                _ => RcptDecision::PermanentReject("550 5.1.1 User not found"),
            }
        }
        other => {
            tracing::warn!(
                domain = %recipient_domain,
                role = %other,
                "RCPT TO rejected — role not yet implemented at receiver"
            );
            RcptDecision::PermanentReject("550 5.7.1 Relaying denied")
        }
    }
}

/// Read a line (terminated by \n) from the stream.
/// Whether a DATA line (as returned by `read_line`, i.e. ending at the
/// first `\n`) is strictly CRLF-framed: it must end with `\r\n` and carry
/// no embedded bare CR. A bare LF cannot occur mid-line (read_line stops
/// at the first `\n`), so checking the terminator + embedded CR suffices.
/// This is the post-2023 SMTP-smuggling guard (cf. Postfix
/// `smtpd_forbid_bare_newline`), applied only on the submission path.
fn data_line_crlf_strict(buf: &[u8]) -> bool {
    let Some(body) = buf.strip_suffix(b"\r\n") else {
        return false;
    };
    !body.contains(&b'\r')
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> Result<usize> {
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

/// Write a line terminated by CRLF, emitting the text and its
/// terminator in a **single** `write_all` so the CRLF can never be
/// split into its own tiny TCP segment.
///
/// This matters at the STARTTLS boundary. The old two-write form
/// (`write_all(text)` then `write_all(b"\r\n")`) sends the 2-byte CRLF
/// as a separate segment; with Nagle's algorithm the CRLF of
/// "220 ... Ready to start TLS" is held (its text segment is still
/// unacked) until the client ACKs — by which point a lock-stepping
/// client (verified with `openssl s_client -starttls smtp`) has already
/// sent its TLS ClientHello. The delayed CRLF then arrives as the first
/// two bytes of what should be the TLS stream (`0x0d 0x0a` → an invalid
/// record content-type), so the handshake dies with "wrong version
/// number". Framing the line and CRLF as one write keeps the terminator
/// with its line in the common (idle send buffer) case so the response
/// is a single segment. `write_all` does not *guarantee* one kernel
/// write under backpressure, so the hard guarantee against a Nagle-
/// delayed terminator is `TCP_NODELAY`, set on the inbound socket in
/// `handle` (fatal there when STARTTLS is on offer). (Packet-trace-
/// confirmed on a live `:25` listener, 2026-06-09.)
async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> Result<()> {
    let mut framed = String::with_capacity(line.len() + 2);
    framed.push_str(line);
    framed.push_str("\r\n");
    writer.write_all(framed.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Phase 3 commit 2 — Bucket B state-machine tests for
    //! [`decide_rcpt_to`].
    //!
    //! These hit the helper directly against an in-memory
    //! [`cosmix_props::Runtime`] over [`cosmix_props::MemoryStore`]
    //! and an in-memory sqlite [`Db`]. They bypass
    //! [`props::domains::register`] (which is `SqliteStore`-bound) and
    //! construct the runtime directly — see Phase 3 doc § Bucket B.

    use super::*;
    use crate::db::Db;

    #[test]
    fn data_line_crlf_strict_accepts_crlf_rejects_bare() {
        // Proper CRLF line: accepted.
        assert!(data_line_crlf_strict(b"Subject: hi\r\n"));
        assert!(data_line_crlf_strict(b"\r\n")); // empty CRLF line
        // Bare LF (no preceding CR): rejected.
        assert!(!data_line_crlf_strict(b"Subject: hi\n"));
        // Embedded bare CR before the terminator: rejected (smuggling).
        assert!(!data_line_crlf_strict(b"a\rb\r\n"));
        // No terminator at all (EOF mid-line): rejected.
        assert!(!data_line_crlf_strict(b"no terminator"));
    }

    /// Records each `poll_write` call's bytes so a test can assert how
    /// the writer framed its output into discrete writes. `accept_cap`
    /// caps how many bytes a single `poll_write` accepts (`None` = all),
    /// to simulate the partial-write / backpressure case.
    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
        accept_cap: Option<usize>,
    }
    impl tokio::io::AsyncWrite for RecordingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let n = self.accept_cap.map_or(buf.len(), |cap| cap.min(buf.len()));
            self.writes.push(buf[..n].to_vec());
            std::task::Poll::Ready(Ok(n))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Regression for the STARTTLS "wrong version number" bug: the line
    /// and its CRLF terminator MUST be emitted as a single write so the
    /// terminator can never be split into its own Nagle-delayed TCP
    /// segment that bleeds into the TLS handshake. (The old form did
    /// `write_all(text)` then `write_all(b"\r\n")` — two writes.)
    #[tokio::test]
    async fn write_line_frames_text_and_crlf_in_one_write() {
        let mut w = RecordingWriter::default();
        write_line(&mut w, "220 2.0.0 Ready to start TLS")
            .await
            .expect("write_line");
        assert_eq!(
            w.writes.len(),
            1,
            "line + CRLF must be a single write so Nagle cannot split the \
             CRLF into the start of the TLS stream"
        );
        assert_eq!(w.writes[0], b"220 2.0.0 Ready to start TLS\r\n");
    }

    /// Under backpressure `write_all` may split across `poll_write`s, but
    /// `write_line` must still deliver the COMPLETE line + CRLF (content
    /// integrity). Segment-level Nagle protection is `TCP_NODELAY`'s job,
    /// not this helper's — this pins that no bytes are dropped/duplicated
    /// when the socket accepts only part of the buffer at a time.
    #[tokio::test]
    async fn write_line_delivers_full_content_under_partial_writes() {
        let mut w = RecordingWriter {
            // Accept only 5 bytes per poll_write — forces write_all to loop.
            accept_cap: Some(5),
            ..Default::default()
        };
        write_line(&mut w, "220 2.0.0 Ready to start TLS")
            .await
            .expect("write_line under partial writes");
        let total: Vec<u8> = w.writes.concat();
        assert_eq!(total, b"220 2.0.0 Ready to start TLS\r\n");
        assert!(
            w.writes.len() > 1,
            "the 5-byte cap should force multiple writes"
        );
    }

    #[test]
    fn bind_requires_starttls_matching() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        // Exact specific-IP match (a public bind with a specific address).
        assert!(bind_requires_starttls(
            &["203.0.113.5:25".into()],
            at("203.0.113.5:25")
        ));
        // Different IP on the same port → NOT required (mesh bind stays opportunistic).
        assert!(!bind_requires_starttls(
            &["203.0.113.5:25".into()],
            at("198.51.100.2:25")
        ));
        // Unspecified IPv4 → matches any address on that port…
        assert!(bind_requires_starttls(
            &["0.0.0.0:25".into()],
            at("203.0.113.5:25")
        ));
        // …but only that port.
        assert!(!bind_requires_starttls(
            &["0.0.0.0:25".into()],
            at("203.0.113.5:587")
        ));
        // IPv6 spelling-insensitive (parsed SocketAddr equality).
        assert!(bind_requires_starttls(
            &["[::1]:25".into()],
            at("[0:0:0:0:0:0:0:1]:25")
        ));
        // Empty list → never required (default opportunistic).
        assert!(!bind_requires_starttls(&[], at("203.0.113.5:25")));
        // An unparseable entry never matches here (it is rejected loudly at
        // startup; this is the defensive belt).
        assert!(!bind_requires_starttls(
            &["localhost:25".into()],
            at("127.0.0.1:25")
        ));
    }

    use cosmix_props::namespace::NamespaceName;
    use cosmix_props::record::{AuditEpoch, Nseq, Record, RecordEvent};
    use cosmix_props::store::{
        CompleteCommit, DeleteCommit, PropertyStore, ReconcileCommit, SetCommit, Snapshot,
        StoreFuture,
    };
    use cosmix_props::{
        Actor, Hooks, MemoryStore, MergeMode, RecordKey, Runtime, SetOpts, Version,
        value::PropValue,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Fault-injection wrapper around an inner [`PropertyStore`].
    ///
    /// Test-only — delegates every trait method to `inner` except when
    /// a fault flag is set, in which case the affected method returns
    /// `Err(StoreError::Storage)`. The corrupt-view branch is exercised
    /// by `corrupt_get_value`: when `Some`, the next `get(...)` returns
    /// a [`Snapshot`] whose [`Record`] carries the supplied
    /// [`PropValue`] verbatim (bypassing the inner store's hook
    /// validation). This is the only safe path for testing a row that
    /// would have been rejected at write time but might appear on disk
    /// after a schema drift or hand-edit.
    struct FaultyStore {
        inner: Arc<dyn PropertyStore>,
        fail_list: AtomicBool,
        fail_get: AtomicBool,
        corrupt_get_value: std::sync::Mutex<Option<PropValue>>,
    }

    impl FaultyStore {
        fn new(inner: Arc<dyn PropertyStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                fail_list: AtomicBool::new(false),
                fail_get: AtomicBool::new(false),
                corrupt_get_value: std::sync::Mutex::new(None),
            })
        }
    }

    impl PropertyStore for FaultyStore {
        fn get<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Snapshot<Record>> {
            if self.fail_get.load(Ordering::SeqCst) {
                return Box::pin(async {
                    Err(StoreError::storage("FaultyStore: injected get error"))
                });
            }
            if let Some(value) = self.corrupt_get_value.lock().unwrap().clone() {
                let key_clone = key.clone();
                return Box::pin(async move {
                    Ok(Snapshot {
                        value: Record {
                            key: key_clone,
                            value,
                            version: Version::zero(),
                            nseq: Nseq::zero(),
                            lifecycle: None,
                        },
                        observed_nseq: Nseq::zero(),
                    })
                });
            }
            self.inner.get(key)
        }

        fn list<'a>(
            &'a self,
            namespace: &'a NamespaceName,
        ) -> StoreFuture<'a, Snapshot<Vec<Record>>> {
            if self.fail_list.load(Ordering::SeqCst) {
                return Box::pin(async {
                    Err(StoreError::storage("FaultyStore: injected list error"))
                });
            }
            self.inner.list(namespace)
        }

        fn commit_set<'a>(&'a self, op: SetCommit) -> StoreFuture<'a, (Record, RecordEvent)> {
            self.inner.commit_set(op)
        }

        fn commit_delete<'a>(&'a self, op: DeleteCommit) -> StoreFuture<'a, RecordEvent> {
            self.inner.commit_delete(op)
        }

        fn commit_complete<'a>(
            &'a self,
            op: CompleteCommit,
        ) -> StoreFuture<'a, (Record, RecordEvent)> {
            self.inner.commit_complete(op)
        }

        fn commit_reconcile<'a>(
            &'a self,
            op: ReconcileCommit,
        ) -> StoreFuture<'a, (Record, RecordEvent)> {
            self.inner.commit_reconcile(op)
        }

        fn events_since<'a>(
            &'a self,
            namespace: &'a NamespaceName,
            since_nseq: Nseq,
        ) -> StoreFuture<'a, Vec<RecordEvent>> {
            self.inner.events_since(namespace, since_nseq)
        }

        fn audit_epoch<'a>(&'a self, namespace: &'a NamespaceName) -> StoreFuture<'a, AuditEpoch> {
            self.inner.audit_epoch(namespace)
        }

        fn version_anchor<'a>(&'a self, key: &'a RecordKey) -> StoreFuture<'a, Option<Version>> {
            self.inner.version_anchor(key)
        }
    }

    /// Build a Runtime over a plain [`MemoryStore`]. Returns both the
    /// `Arc<Runtime>` (for `decide_rcpt_to`) and the raw inner store
    /// `Arc` (for callers that want to wrap it in [`FaultyStore`]).
    fn build_runtime() -> Arc<Runtime> {
        let store = Arc::new(MemoryStore::new("maild"));
        Arc::new(Runtime::new(
            "maild",
            props::domains::spec(Hooks::new(props::domains::DomainsHooks::new())),
            store as Arc<dyn PropertyStore>,
        ))
    }

    /// Empty `maild.aliases` runtime over a [`MemoryStore`] — these RCPT
    /// tests don't exercise aliases, so an alias-free table makes
    /// `resolve_recipient` a no-op (returns the original address). The
    /// hooks' `accounts_runtime` is never read (no alias writes here), so
    /// any runtime instance suffices.
    fn build_aliases_runtime() -> Arc<Runtime> {
        let store = Arc::new(MemoryStore::new("maild"));
        let dummy_accounts = Arc::new(Runtime::new(
            "maild",
            props::domains::spec(Hooks::noop()),
            store.clone() as Arc<dyn PropertyStore>,
        ));
        Arc::new(Runtime::new(
            "maild",
            props::aliases::spec(Hooks::new(props::aliases::AliasesHooks::new(
                dummy_accounts,
            ))),
            store as Arc<dyn PropertyStore>,
        ))
    }

    /// Build a Runtime whose backing store is a [`FaultyStore`] over a
    /// [`MemoryStore`]. Returns both the runtime and the wrapper so the
    /// test can flip fault flags after the seed write.
    fn build_runtime_with_faulty() -> (Arc<Runtime>, Arc<FaultyStore>) {
        let inner: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let faulty = FaultyStore::new(inner);
        let runtime = Arc::new(Runtime::new(
            "maild",
            props::domains::spec(Hooks::new(props::domains::DomainsHooks::new())),
            faulty.clone() as Arc<dyn PropertyStore>,
        ));
        (runtime, faulty)
    }

    async fn seed_domain(
        runtime: &Arc<Runtime>,
        domain: &str,
        mutate: impl FnOnce(&mut PropValue),
    ) {
        let mut value = props::domains::defaults_value(domain);
        mutate(&mut value);
        runtime
            .set(
                RecordKey::collection(props::domains::namespace_name(), domain.to_string()),
                value,
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("phase3-tests").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .expect("seed domain");
    }

    /// Test fixture that owns the [`Db`] **and** the backing tempdir.
    /// Drop order guarantees the tempdir outlives every `db.conn` use
    /// in the test: fields drop top-to-bottom, so `db` (and its
    /// `Arc<Mutex<Connection>>`) drops before `_tmp` is removed.
    struct DbFixture {
        db: Db,
        _tmp: tempfile::TempDir,
    }

    async fn make_db_fixture() -> DbFixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("maild.sqlite");
        let blob_dir = tmp.path().join("blobs");
        let db = Db::connect(db_path.to_str().unwrap(), blob_dir.to_str().unwrap())
            .await
            .expect("Db::connect");
        db.migrate().await.expect("migrate");
        DbFixture { db, _tmp: tmp }
    }

    fn seed_account(db: &Db, email: &str) {
        let conn = db.conn.lock().expect("lock db");
        let hash = bcrypt::hash("changeme", bcrypt::DEFAULT_COST).expect("hash");
        conn.execute(
            "INSERT INTO accounts (email, password, name) VALUES (?, ?, ?)",
            rusqlite::params![email, hash, "Test"],
        )
        .expect("insert account");
    }

    #[tokio::test]
    async fn rcpt_empty_namespace_unauthd_legacy_account_hit_accepts() {
        let runtime = build_runtime();
        let fx = make_db_fixture().await;
        let db = &fx.db;
        seed_account(db, "user@legacy.example");
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<user@legacy.example>",
            false,
        )
        .await;
        assert_eq!(outcome, RcptDecision::Accept("user@legacy.example".into()));
    }

    #[tokio::test]
    async fn rcpt_empty_namespace_unauthd_legacy_account_miss_rejects() {
        let runtime = build_runtime();
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<nobody@legacy.example>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("550 5.1.1 User not found")
        );
    }

    #[tokio::test]
    async fn rcpt_authoritative_account_hit_accepts() {
        let runtime = build_runtime();
        seed_domain(&runtime, "example.com", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        seed_account(db, "markc@example.com");
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<markc@example.com>",
            false,
        )
        .await;
        assert_eq!(outcome, RcptDecision::Accept("markc@example.com".into()));
    }

    #[tokio::test]
    async fn rcpt_authoritative_account_miss_rejects_511() {
        let runtime = build_runtime();
        seed_domain(&runtime, "example.com", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<nobody@example.com>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("550 5.1.1 User not found")
        );
    }

    #[tokio::test]
    async fn rcpt_opaque_shaped_accepted_when_enabled() {
        // C9: an opaque-token-shaped recipient (single segment, no `-`, len 6/10
        // over the Crockford alphabet) on an authoritative domain is accepted at
        // RCPT — opaquely, validated after DATA — ONLY when the pre-flight flag
        // enabled opaque acceptance. No account of that local-part exists, so
        // acceptance is purely the opaque-shape gate.
        let runtime = build_runtime();
        seed_domain(&runtime, "example.com", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<k4m9p2@example.com>",
            true,
        )
        .await;
        assert_eq!(outcome, RcptDecision::Accept("k4m9p2@example.com".into()));
    }

    #[tokio::test]
    async fn rcpt_opaque_shaped_rejected_when_disabled() {
        // C9 fail-safe: with opaque acceptance disabled (a pre-flight collision
        // or a namespace read error), the SAME opaque-shaped recipient is
        // rejected as unknown — it can never silently swallow a real mailbox of
        // the same shape.
        let runtime = build_runtime();
        seed_domain(&runtime, "example.com", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<k4m9p2@example.com>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("550 5.1.1 User not found")
        );
    }

    #[tokio::test]
    async fn rcpt_unknown_domain_rejects_512() {
        let runtime = build_runtime();
        seed_domain(&runtime, "a.example", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<user@b.example>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("550 5.1.2 No such domain here")
        );
    }

    #[tokio::test]
    async fn rcpt_disabled_row_rejects_512() {
        let runtime = build_runtime();
        seed_domain(&runtime, "example.com", |v| {
            if let PropValue::Object(map) = v {
                map.insert("enabled".into(), PropValue::Bool(false));
            }
        })
        .await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        // Account exists — disable still wins.
        seed_account(db, "markc@example.com");
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<markc@example.com>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("550 5.1.2 No such domain here")
        );
    }

    #[tokio::test]
    async fn rcpt_backup_mx_rejects_571_phase3() {
        let runtime = build_runtime();
        seed_domain(&runtime, "example.com", |v| {
            if let PropValue::Object(map) = v {
                map.insert("role".into(), PropValue::String("backup_mx".into()));
            }
        })
        .await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<anyone@example.com>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("550 5.7.1 Relaying denied")
        );
    }

    #[tokio::test]
    async fn rcpt_authd_session_bypasses_substrate_check() {
        // Even with the domain *absent* from a non-empty namespace,
        // an authd session accepts. AUTH is the gate.
        let runtime = build_runtime();
        seed_domain(&runtime, "example.com", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            true,
            "TO:<external@elsewhere.example>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::Accept("external@elsewhere.example".into())
        );
    }

    #[tokio::test]
    async fn rcpt_parse_failure_rejects_513() {
        let runtime = build_runtime();
        let fx = make_db_fixture().await;
        let db = &fx.db;
        // Missing `TO:` prefix → parse_rcpt_to returns None → 501.
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "garbage",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("501 5.1.3 Bad recipient address")
        );
    }

    #[tokio::test]
    async fn rcpt_address_without_at_rejects_513() {
        let runtime = build_runtime();
        let fx = make_db_fixture().await;
        let db = &fx.db;
        // Address has no `@` — rsplit_once returns None.
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<noatsign>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::PermanentReject("550 5.1.3 Bad recipient address")
        );
    }

    #[tokio::test]
    async fn rcpt_substrate_list_error_returns_451_transient() {
        // Empty namespace check fails. With the fail-closed rule, the
        // call returns 451 transient — NOT a silent fallback to the
        // legacy account lookup. (Without this rule a temporary
        // substrate outage would silently mask the substrate-
        // authoritative configuration.)
        let (runtime, faulty) = build_runtime_with_faulty();
        let fx = make_db_fixture().await;
        let db = &fx.db;
        // Even an account row that the legacy path would accept must
        // NOT be reached on the fault path.
        seed_account(db, "markc@example.com");
        faulty.fail_list.store(true, Ordering::SeqCst);
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<markc@example.com>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::TransientReject("451 4.3.0 Temporary system error")
        );
    }

    #[tokio::test]
    async fn rcpt_substrate_get_error_returns_451_transient() {
        // Seed one row so the namespace is non-empty (list succeeds),
        // then fail the per-key get. Non-NotFound get errors must be
        // transient — substrate read failure is not "domain absent".
        let (runtime, faulty) = build_runtime_with_faulty();
        seed_domain(&runtime, "example.com", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        faulty.fail_get.store(true, Ordering::SeqCst);
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<markc@example.com>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::TransientReject("451 4.3.0 Temporary system error")
        );
    }

    #[tokio::test]
    async fn rcpt_view_corrupt_returns_451() {
        // Inject a get(...) override whose PropValue body fails
        // `view(...)` (role typed as Int rather than String). A row
        // that survives writeside validation but later fails the
        // typed projection is corrupt-on-disk; the gate is
        // fail-closed transient so an operator can intervene.
        let (runtime, faulty) = build_runtime_with_faulty();
        seed_domain(&runtime, "example.com", |_| {}).await;
        let fx = make_db_fixture().await;
        let db = &fx.db;
        let mut corrupt = std::collections::BTreeMap::new();
        corrupt.insert("domain".into(), PropValue::String("example.com".into()));
        corrupt.insert("enabled".into(), PropValue::Bool(true));
        // Wrong variant — view(...) requires String role.
        corrupt.insert("role".into(), PropValue::Int(7));
        corrupt.insert("relay_recipients".into(), PropValue::List(Vec::new()));
        *faulty.corrupt_get_value.lock().unwrap() = Some(PropValue::Object(corrupt));
        let outcome = decide_rcpt_to(
            &runtime,
            &build_aliases_runtime(),
            &db.conn,
            false,
            "TO:<markc@example.com>",
            false,
        )
        .await;
        assert_eq!(
            outcome,
            RcptDecision::TransientReject("451 4.3.0 Temporary system error")
        );
    }
}
