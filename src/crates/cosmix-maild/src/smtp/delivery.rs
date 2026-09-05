//! Outbound delivery worker — polls queue, delivers via SMTP with STARTTLS, handles retries.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use hickory_resolver::{Resolver, TokioResolver};

use mail_auth::common::crypto::RsaKey;
use mail_auth::common::headers::HeaderWriter;
use mail_auth::dkim::DkimSigner;

use cosmix_maild_auth::{Error as MailAuthError, Signer};
use cosmix_mds::Mds;
use cosmix_props::RecordKey;
use cosmix_props::store::StoreError;

use super::SmtpState;
use super::headers::from_header_domain;
use super::{bounce, queue};
use crate::db;
use crate::props;

/// A delivery failure the remote server told us is **permanent** (SMTP 5xx):
/// an unknown recipient, a domain it does not serve, a rejected message.
///
/// Retrying cannot change the answer, so the queue bounces immediately instead
/// of grinding through its full retry ladder. Before this existed every failure
/// was treated as transient, so a `550 No such domain here` was retried for
/// hours and the sender learned nothing until the attempts ran out.
///
/// Carried through `anyhow` and recovered at the queue boundary — the
/// intervening layers (`deliver_to_domain`, the MX rotation) are transport
/// plumbing and have no reason to know about it.
#[derive(Debug)]
pub struct PermanentFailure(pub String);

impl std::fmt::Display for PermanentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PermanentFailure {}

/// True when this error chain carries a [`PermanentFailure`].
fn is_permanent(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<PermanentFailure>())
}

/// The STARTTLS handshake completed the network exchange but the peer's
/// certificate did not validate against our trust store.
///
/// Distinct from every other TLS error because it is the one case where
/// retrying *without* verification is the correct MTA behaviour — see
/// [`TlsPolicy`]. A handshake that fails for any other reason (protocol
/// mismatch, connection reset) is a genuine transport failure and stays
/// on the normal retry ladder.
#[derive(Debug)]
pub struct TlsVerifyFailure(pub String);

impl std::fmt::Display for TlsVerifyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TlsVerifyFailure {}

/// True when this error chain carries a [`TlsVerifyFailure`].
fn is_tls_verify_failure(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<TlsVerifyFailure>())
}

/// How hard to insist on a valid certificate for an outbound STARTTLS
/// upgrade.
///
/// MTA-to-MTA TLS is *opportunistic* (RFC 7435): absent MTA-STS or DANE
/// there is no way to know the peer was supposed to present a valid
/// certificate, so a verification failure must not strand the mail.
/// Postfix encodes the same rule as its default
/// `smtp_tls_security_level = may` — encrypt, but do not verify.
///
/// We still try [`Verify`](Self::Verify) first so the common case gets
/// authenticated TLS and the failure is visible in the log, then
/// reconnect once at [`Unverified`](Self::Unverified). Encrypted-but-
/// unauthenticated beats both cleartext and a deferred queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsPolicy {
    /// Full chain + hostname validation against [`outbound_root_store`].
    Verify,
    /// Accept any certificate. Only ever reached as the second attempt
    /// after `Verify` failed on the same host.
    Unverified,
}

/// Trust anchors for outbound delivery: the OS store **unioned with**
/// the compiled-in Mozilla set.
///
/// Neither alone is sufficient. `webpki-roots` is a snapshot of
/// Mozilla's program frozen at build time, and Mozilla removes roots
/// that the wider mail world still uses — 0.26.11 dropped `DigiCert
/// Global Root CA`, which is exactly what every
/// `*.mail.protection.outlook.com` host chains to, so every message to
/// Microsoft-hosted mail failed `UnknownIssuer` while the OS store on
/// the same box verified it fine. The OS store alone would in turn
/// leave us with nothing on a minimal image that ships no
/// `ca-certificates`.
///
/// Built once — loading the system store touches the filesystem and
/// there is one delivery worker doing this per message.
fn outbound_root_store() -> &'static Arc<rustls::RootCertStore> {
    static ROOTS: std::sync::OnceLock<Arc<rustls::RootCertStore>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut store = rustls::RootCertStore::empty();

        let native = rustls_native_certs::load_native_certs();
        for err in &native.errors {
            tracing::warn!(error = %err, "loading a native trust anchor failed");
        }
        let (native_added, native_ignored) = store.add_parsable_certificates(native.certs);

        let builtin_before = store.len();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let builtin_added = store.len() - builtin_before;

        if store.is_empty() {
            tracing::error!(
                "outbound TLS trust store is empty — every STARTTLS upgrade will fall back to unverified"
            );
        } else {
            tracing::info!(
                native_added,
                native_ignored,
                builtin_added,
                total = store.len(),
                "built outbound TLS trust store"
            );
        }

        Arc::new(store)
    })
}

/// Certificate verifier that accepts anything, for [`TlsPolicy::Unverified`].
///
/// Signature verification itself is left intact — only the trust-chain
/// and hostname checks are dropped — so the handshake still proves the
/// peer holds the key it presented.
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Client config for an outbound STARTTLS upgrade under `policy`.
/// Both variants are built once and shared.
fn outbound_tls_config(policy: TlsPolicy) -> Arc<rustls::ClientConfig> {
    static VERIFY: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    static UNVERIFIED: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();

    match policy {
        TlsPolicy::Verify => VERIFY.get_or_init(|| {
            Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(outbound_root_store().clone())
                    .with_no_client_auth(),
            )
        }),
        TlsPolicy::Unverified => UNVERIFIED.get_or_init(|| {
            let provider = rustls::crypto::CryptoProvider::get_default()
                .cloned()
                .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
            Arc::new(
                rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider)))
                    .with_no_client_auth(),
            )
        }),
    }
    .clone()
}

/// True when a `tokio-rustls` connect error was a certificate-validation
/// rejection rather than a transport or protocol failure.
fn is_cert_rejection(e: &std::io::Error) -> bool {
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(rustls::Error::InvalidCertificate(_)) => true,
        // tokio-rustls does not always preserve the source error across
        // its io::Error wrapping; fall back to the rendered form rather
        // than silently classing a cert rejection as transport failure.
        _ => e.to_string().contains("invalid peer certificate"),
    }
}

/// Which `maild.domains` field starts the effective-host fallback
/// chain. The chain is identical past the first step:
/// `<kind-field>` → `primary_hostname` → row key → `config.hostname`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKind {
    /// EHLO/HELO identity on outbound SMTP — starts at
    /// `helo_identity`.
    Helo,
    /// Right-hand side of `Message-ID:` on locally-originated mail
    /// — starts at `message_id_host`.
    MessageId,
}

/// Resolve the effective host for a sender domain via the
/// `maild.domains` substrate row.
///
/// Used by:
///
/// - Outbound delivery EHLO: `HostKind::Helo`
/// - Bounce / NDR Message-ID: `HostKind::MessageId`
/// - Vacation auto-reply Message-ID: `HostKind::MessageId`
///
/// Outbound is best-effort — substrate read errors do **not** stop
/// delivery; the chain falls through to `config.hostname` in that
/// case. The fail-closed posture on the RCPT TO path is a receiver-
/// side property; an outbound MTA returning 451 to itself would just
/// retry and re-hit the same error.
///
/// Fallback chain (per Phase 3 doc § Sender path):
///
/// 1. The kind-specific field (`helo_identity` or `message_id_host`)
///    if `Some(..)`.
/// 2. `primary_hostname` if `Some(..)`.
/// 3. The row's primary key (the FQDN itself).
/// 4. `config.hostname` (substrate read error, missing row, empty
///    namespace, or `view(...)` projection failure).
pub(crate) async fn sender_effective_host(
    domains_runtime: &std::sync::Arc<cosmix_props::Runtime>,
    config_hostname: &str,
    sender_domain: &str,
    kind: HostKind,
) -> String {
    let fallback = || config_hostname.to_string();
    let domain = sender_domain.to_ascii_lowercase();
    let key = RecordKey::collection(props::domains::namespace_name(), domain.clone());
    let record = match domains_runtime.store().get(&key).await {
        Ok(snap) => snap.value,
        Err(StoreError::NotFound) => return fallback(),
        Err(e) => {
            tracing::warn!(
                domain = %domain,
                error = %e,
                "maild.domains read error on sender_effective_host — falling back to config.hostname"
            );
            return fallback();
        }
    };
    let view = match props::domains::view(&record) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                domain = %domain,
                error = %e,
                "maild.domains row failed view projection on sender_effective_host — falling back to config.hostname"
            );
            return fallback();
        }
    };
    let first = match kind {
        HostKind::Helo => view.helo_identity,
        HostKind::MessageId => view.message_id_host,
    };
    if let Some(h) = first {
        return h;
    }
    if let Some(h) = view.primary_hostname {
        return h;
    }
    domain
}

/// Extract the domain from a `local@domain` envelope address (or any
/// string with an `@`). Empty / no-`@` strings fall through to the
/// caller's `default_domain` so the bounce path on a null sender
/// still gets a sensible host.
pub(crate) fn sender_domain_of(addr: &str, default_domain: &str) -> String {
    match addr.rsplit_once('@') {
        Some((_, d)) if !d.is_empty() => d.to_ascii_lowercase(),
        _ => default_domain.to_ascii_lowercase(),
    }
}

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
                    tracing::info!(
                        delivered = count,
                        "Queue processing complete: {count} delivered"
                    );
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
    let entries = queue::fetch_ready(&state.db.conn, 50).await?;
    let mut delivered = 0;

    for entry in entries {
        // Task 5.4b: load via CAS (`mds.get_blob`) when `blob_hash` is
        // populated; fall back to the legacy `db::blob::load` path for
        // pre-migration in-flight rows that still carry only `blob_id`.
        // After Phase 6 retires `db::blob`, the fallback arm goes away
        // along with the `blob_id` column.
        let data = match (&entry.blob_hash, entry.blob_id) {
            (Some(hash), _) => {
                let mds = state.mailstore.mds().clone();
                let hash_owned = *hash;
                match tokio::task::spawn_blocking(move || mds.get_blob(&hash_owned)).await {
                    Ok(Ok(bytes)) => Some(bytes),
                    Ok(Err(cosmix_mds::Error::BlobNotFound(_))) => {
                        // CAS doesn't have it — permanent: nothing to
                        // retry. Fall through to the `None` arm below
                        // which emits `mark_permanent_failure`.
                        None
                    }
                    Ok(Err(e)) => {
                        // Transient (IO, lock, etc): bump the retry
                        // counter and move on. `fetch_ready` filters
                        // `attempts < 10`, so we must escalate to
                        // `mark_permanent_failure` at the boundary
                        // ourselves — `mark_failed` alone leaves the
                        // row stuck and invisible to future polls.
                        // Mirrors the `entry.attempts >= 9` ladder on
                        // `deliver_to_domain` errors below.
                        tracing::error!(queue_id = entry.id, error = %e, "CAS get_blob failed transiently");
                        if entry.attempts >= 9 {
                            queue::mark_permanent_failure(&state.db.conn, entry.id, &e.to_string())
                                .await?;
                        } else {
                            queue::mark_failed(&state.db.conn, entry.id, &e.to_string()).await?;
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::error!(queue_id = entry.id, error = %e, "spawn_blocking failed");
                        if entry.attempts >= 9 {
                            queue::mark_permanent_failure(&state.db.conn, entry.id, &e.to_string())
                                .await?;
                        } else {
                            queue::mark_failed(&state.db.conn, entry.id, &e.to_string()).await?;
                        }
                        continue;
                    }
                }
            }
            (None, Some(blob_id)) => {
                db::blob::load(&state.db.conn, &state.db.blob_dir, blob_id).await?
            }
            (None, None) => {
                tracing::error!(
                    queue_id = entry.id,
                    "Queue entry missing both blob_hash and blob_id"
                );
                queue::mark_permanent_failure(
                    &state.db.conn,
                    entry.id,
                    "queue entry missing blob reference",
                )
                .await?;
                continue;
            }
        };
        let Some(data) = data else {
            tracing::error!(queue_id = entry.id, "Blob not found for queue entry");
            queue::mark_permanent_failure(&state.db.conn, entry.id, "blob not found").await?;
            continue;
        };

        // DKIM-sign the message if configured. Triadic dispatch
        // (new per-domain signer → legacy single-key → unsigned)
        // lives in `dkim_sign`; the caller treats `None` as
        // "send unsigned", same as before.
        let data = if let Some(signed) = dkim_sign(&state.config, &data).await {
            signed
        } else {
            data
        };

        // Group recipients by domain for efficient delivery
        let by_domain = group_by_domain(&entry.to_addrs);

        // Per-domain EHLO host derived from the *sender*'s
        // maild.domains row. Computed once per queue entry; the
        // outbound MX rotation reuses the same host for every
        // recipient on that entry. Substrate read errors fall
        // through to `config.hostname` (outbound is best-effort).
        let sender_domain = sender_domain_of(&entry.from_addr, &state.config.hostname);
        let helo_host = sender_effective_host(
            &state.domains_runtime,
            &state.config.hostname,
            &sender_domain,
            HostKind::Helo,
        )
        .await;

        let mut all_ok = true;
        for (domain, recipients) in &by_domain {
            match deliver_to_domain(
                state,
                resolver,
                &helo_host,
                &entry.from_addr,
                recipients,
                domain,
                &data,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!(
                        queue_id = entry.id,
                        domain = domain,
                        recipients = ?recipients,
                        "Delivered to domain {domain}: queue_id={} nrcpt={}",
                        entry.id,
                        recipients.len(),
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        queue_id = entry.id,
                        domain = domain,
                        error = %e,
                        "Delivery to domain {domain} failed (queue_id={}, attempt {}): {}",
                        entry.id,
                        entry.attempts + 1,
                        // The error chain embeds remote SMTP response
                        // bytes (multiline CRLF) — sanitize before the
                        // inline journal MESSAGE.
                        crate::maillog::sanitize(&e.to_string()),
                    );
                    all_ok = false;
                    // A 5xx is the remote server telling us the answer will
                    // never change: bounce NOW rather than burning the retry
                    // ladder. Retrying a `550 No such domain here` for hours
                    // delays the NDR the sender needs and wastes attempts on a
                    // decided outcome.
                    let permanent = is_permanent(&e);
                    if permanent || entry.attempts >= 9 {
                        if permanent {
                            tracing::info!(
                                queue_id = entry.id,
                                domain = domain,
                                "Permanent failure (5xx) — bouncing without retry: {}",
                                crate::maillog::sanitize(&e.to_string()),
                            );
                        }
                        queue::mark_permanent_failure(&state.db.conn, entry.id, &e.to_string())
                            .await?;

                        // Generate and deliver bounce to sender
                        if let Err(be) = generate_bounce(
                            state,
                            &entry.from_addr,
                            &entry.to_addrs,
                            &e.to_string(),
                        )
                        .await
                        {
                            tracing::warn!(error = %be, "Failed to generate bounce");
                        }
                    } else {
                        queue::mark_failed(&state.db.conn, entry.id, &e.to_string()).await?;
                    }
                }
            }
        }

        if all_ok {
            queue::mark_delivered(&state.db.conn, entry.id).await?;
            delivered += 1;
        }
    }

    Ok(delivered)
}

/// Generate a bounce message and deliver to the sender if they're local.
async fn generate_bounce(state: &SmtpState, from: &str, to: &[String], error: &str) -> Result<()> {
    if from.is_empty() {
        return Ok(()); // Don't bounce bounces (null sender)
    }

    // Phase 1 aliases: a message sent AS a local alias (`from = mc@`)
    // must still bounce to the alias's target account. Resolve `from`
    // for the is-local gate; the `deliver` call below passes the
    // original `from` and re-resolves the recipient itself.
    let resolved_from =
        crate::props::aliases::resolve_recipient(&state.aliases_runtime, from).await?;
    let account = db::account::get_by_email(&state.db.conn, &resolved_from).await?;
    if let Some(_account) = account {
        // Bounce goes to the *original sender*, so the Message-ID
        // identifies our domain as the sender sees us. Lookup keyed
        // on the sender's domain.
        let sender_domain = sender_domain_of(from, &state.config.hostname);
        let ndr_host = sender_effective_host(
            &state.domains_runtime,
            &state.config.hostname,
            &sender_domain,
            HostKind::MessageId,
        )
        .await;
        let ndr = bounce::generate_ndr(&ndr_host, from, to, error)?;
        // A system-generated bounce is not an authenticated mailbox-owner
        // submission — classify it external-unverified so it can never satisfy
        // a vtoken sender-lock.
        super::inbound::deliver(
            state,
            &super::inbound::IngressAuth::ExternalUnverified,
            "",
            &[from.to_string()],
            &ndr,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "localhost",
        )
        .await?;
        tracing::info!(
            to = from,
            "Delivered bounce notification to <{}>",
            crate::maillog::sanitize(from),
        );
    }
    Ok(())
}

/// Deliver a message to all recipients at a specific domain via MX lookup.
async fn deliver_to_domain(
    state: &SmtpState,
    resolver: &TokioResolver,
    helo_host: &str,
    from: &str,
    recipients: &[&String],
    domain: &str,
    data: &[u8],
) -> Result<()> {
    // Test seam: `SmtpConfig::test_mx_overrides` short-circuits both
    // the MX lookup and the `host:port` resolution paths, dialing
    // the mapped `SocketAddr` directly. The rest of the wire flow
    // (EHLO, MAIL FROM, RCPT TO, DATA, optional STARTTLS) runs
    // unchanged — production paths share `try_deliver_session` so
    // the override does not duplicate SMTP transaction logic.
    // Production always leaves the map empty; see
    // `SmtpConfig::test_mx_overrides` for the contract.
    if let Some(addr) = state.config.test_mx_overrides.get(domain).copied() {
        return try_deliver_addr(addr, domain, helo_host, from, recipients, data).await;
    }

    // MX lookup
    let mx_hosts = resolve_mx(resolver, domain).await?;
    if mx_hosts.is_empty() {
        anyhow::bail!("No MX records found for {domain}");
    }

    // Try MX hosts in preference order.
    //
    // A PERMANENT (5xx) rejection stops the rotation: every MX for a domain
    // answers from the same recipient table, so a `550 No such domain here` from
    // the first will be repeated by the rest — walking on would just re-offer
    // the message to each backup MX before failing anyway. Only a transient
    // error (connect refused, 4xx, TLS failure) is worth the next host.
    let mut last_error = None;
    for mx_host in &mx_hosts {
        match try_deliver(
            mx_host,
            25,
            helo_host,
            from,
            recipients,
            data,
            &state.config.outbound_bind,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                if is_permanent(&e) {
                    tracing::debug!(mx = mx_host, error = %e, "MX rejected permanently — not trying further MX hosts");
                    return Err(e);
                }
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
            let mut hosts: Vec<(u16, String)> = mx
                .iter()
                .map(|r| (r.preference(), r.exchange().to_ascii()))
                .collect();
            hosts.sort_by_key(|h| h.0);
            Ok(hosts
                .into_iter()
                .map(|h| h.1.trim_end_matches('.').to_string())
                .collect())
        }
        Err(_) => {
            // Fall back to A/AAAA record on the domain itself
            Ok(vec![domain.to_string()])
        }
    }
}

/// Try to deliver to a specific SMTP host:port with opportunistic
/// STARTTLS. Resolves `host:port` via `TcpStream::connect`'s built-in
/// DNS path and delegates to [`try_deliver_session`] post-connect.
async fn try_deliver(
    host: &str,
    port: u16,
    helo_host: &str,
    from: &str,
    recipients: &[&String],
    data: &[u8],
    outbound_bind: &[std::net::IpAddr],
) -> Result<()> {
    let stream = connect_smtp(host, port, outbound_bind).await?;
    match try_deliver_session(
        stream,
        host,
        helo_host,
        from,
        recipients,
        data,
        TlsPolicy::Verify,
    )
    .await
    {
        Err(e) if is_tls_verify_failure(&e) => {
            // The peer's certificate did not validate. Opportunistic TLS
            // says deliver anyway rather than sit in the queue until the
            // message bounces — the alternative on the wire would have
            // been cleartext. The handshake consumed the connection, so
            // this needs a fresh one.
            tracing::warn!(
                host,
                error = %e,
                "outbound certificate verification failed; retrying with unverified TLS"
            );
            let stream = connect_smtp(host, port, outbound_bind).await?;
            try_deliver_session(
                stream,
                host,
                helo_host,
                from,
                recipients,
                data,
                TlsPolicy::Unverified,
            )
            .await
        }
        other => other,
    }
}

/// Open a TCP connection to `host:port`, honouring `outbound_bind`.
///
/// Split out of [`try_deliver`] because an unverified-TLS retry needs a
/// second connection: the failed handshake takes the first one with it.
async fn connect_smtp(
    host: &str,
    port: u16,
    outbound_bind: &[std::net::IpAddr],
) -> Result<tokio::net::TcpStream> {
    use tokio::net::{TcpSocket, TcpStream, lookup_host};

    let addr = format!("{host}:{port}");
    let stream = if outbound_bind.is_empty() {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            TcpStream::connect(&addr),
        )
        .await??
    } else {
        // Source-bound connect: resolve ourselves, then dial each
        // address with the matching-family bind (if one is
        // configured). getaddrinfo order is preserved, and a failed
        // address falls through to the next — same semantics as
        // `TcpStream::connect(&str)`, plus the bind.
        let mut last_err: Option<anyhow::Error> = None;
        let mut stream = None;
        for peer in lookup_host(&addr).await? {
            let bind_ip = outbound_bind
                .iter()
                .copied()
                .find(|b| b.is_ipv4() == peer.is_ipv4());
            let attempt = async {
                let socket = if peer.is_ipv4() {
                    TcpSocket::new_v4()?
                } else {
                    TcpSocket::new_v6()?
                };
                if let Some(ip) = bind_ip {
                    socket.bind(std::net::SocketAddr::new(ip, 0))?;
                }
                anyhow::Ok(socket.connect(peer).await?)
            };
            match tokio::time::timeout(std::time::Duration::from_secs(30), attempt).await {
                Ok(Ok(s)) => {
                    stream = Some(s);
                    break;
                }
                Ok(Err(e)) => last_err = Some(e),
                Err(elapsed) => last_err = Some(elapsed.into()),
            }
        }
        match stream {
            Some(s) => s,
            None => {
                return Err(
                    last_err.unwrap_or_else(|| anyhow::anyhow!("no addresses resolved for {addr}"))
                );
            }
        }
    };
    Ok(stream)
}

/// Test seam: deliver to a resolved `SocketAddr` directly, skipping
/// DNS / MX lookup. The `sni` arg threads through to the TLS
/// `ServerName` if STARTTLS upgrades succeed and is also used for
/// error-message labelling. Shares the same `try_deliver_session`
/// body as the production `try_deliver` path so wire behaviour is
/// observably identical past the connect.
async fn try_deliver_addr(
    addr: std::net::SocketAddr,
    sni: &str,
    helo_host: &str,
    from: &str,
    recipients: &[&String],
    data: &[u8],
) -> Result<()> {
    use tokio::net::TcpStream;

    let stream = tokio::time::timeout(std::time::Duration::from_secs(30), TcpStream::connect(addr))
        .await??;
    match try_deliver_session(
        stream,
        sni,
        helo_host,
        from,
        recipients,
        data,
        TlsPolicy::Verify,
    )
    .await
    {
        Err(e) if is_tls_verify_failure(&e) => {
            tracing::warn!(
                host = sni,
                error = %e,
                "outbound certificate verification failed; retrying with unverified TLS"
            );
            let stream =
                tokio::time::timeout(std::time::Duration::from_secs(30), TcpStream::connect(addr))
                    .await??;
            try_deliver_session(
                stream,
                sni,
                helo_host,
                from,
                recipients,
                data,
                TlsPolicy::Unverified,
            )
            .await
        }
        other => other,
    }
}

/// Shared post-connect SMTP transaction body: greeting / EHLO /
/// optional STARTTLS / MAIL FROM / RCPT TO / DATA / QUIT. Both
/// production (`try_deliver`) and test-seam (`try_deliver_addr`)
/// entry points reach this with a connected [`tokio::net::TcpStream`].
///
/// `tls` selects the certificate policy for the STARTTLS upgrade.
/// Callers pass [`TlsPolicy::Verify`] first; on a [`TlsVerifyFailure`]
/// they reconnect and call again with [`TlsPolicy::Unverified`].
async fn try_deliver_session(
    stream: tokio::net::TcpStream,
    host: &str,
    helo_host: &str,
    from: &str,
    recipients: &[&String],
    data: &[u8],
    tls: TlsPolicy,
) -> Result<()> {
    use tokio::io::BufReader;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Read greeting
    let greeting = read_response(&mut reader).await?;
    if !greeting.starts_with('2') {
        anyhow::bail!("Bad greeting from {host}: {greeting}");
    }

    // EHLO
    send_cmd(&mut writer, &format!("EHLO {helo_host}")).await?;
    let ehlo_resp = read_response(&mut reader).await?;
    if !ehlo_resp.starts_with('2') {
        // Fall back to HELO
        send_cmd(&mut writer, &format!("HELO {helo_host}")).await?;
        let resp = read_response(&mut reader).await?;
        if !resp.starts_with('2') {
            anyhow::bail!("EHLO/HELO rejected by {host}: {resp}");
        }
        // No STARTTLS possible with HELO, proceed plaintext
        return deliver_message(&mut reader, &mut writer, host, from, recipients, data).await;
    }

    // Attempt STARTTLS if advertised
    if ehlo_resp.contains("STARTTLS") {
        send_cmd(&mut writer, "STARTTLS").await?;
        let resp = read_response(&mut reader).await?;
        if resp.starts_with('2') {
            // Upgrade to TLS
            let tcp_stream = reader.into_inner().reunite(writer)?;

            let connector = tokio_rustls::TlsConnector::from(outbound_tls_config(tls));

            let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
                .unwrap_or_else(|_| {
                    rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap()
                });

            match connector.connect(server_name, tcp_stream).await {
                Ok(tls_stream) => {
                    let (tls_reader, mut tls_writer) = tokio::io::split(tls_stream);
                    let mut tls_reader = BufReader::new(tls_reader);

                    // Re-EHLO after STARTTLS
                    send_cmd(&mut tls_writer, &format!("EHLO {helo_host}")).await?;
                    let resp = read_response(&mut tls_reader).await?;
                    if !resp.starts_with('2') {
                        anyhow::bail!("Post-STARTTLS EHLO rejected by {host}: {resp}");
                    }

                    tracing::debug!(host, "Upgraded to TLS");
                    return deliver_message(
                        &mut tls_reader,
                        &mut tls_writer,
                        host,
                        from,
                        recipients,
                        data,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::debug!(host, error = %e, "STARTTLS upgrade failed, connection lost");
                    // A cert rejection under `Verify` is recoverable by the
                    // caller (reconnect, retry unverified); tag it so it can
                    // tell that apart from a dead transport.
                    if tls == TlsPolicy::Verify && is_cert_rejection(&e) {
                        return Err(anyhow::Error::new(TlsVerifyFailure(format!(
                            "STARTTLS TLS handshake failed with {host}: {e}"
                        ))));
                    }
                    anyhow::bail!("STARTTLS TLS handshake failed with {host}: {e}");
                }
            }
        }
        // STARTTLS command rejected — fall through to plaintext
        tracing::debug!(host, "STARTTLS rejected, falling back to plaintext");
    }

    // Plaintext delivery (no STARTTLS or STARTTLS failed gracefully)
    deliver_message(&mut reader, &mut writer, host, from, recipients, data).await
}

/// Send MAIL FROM, RCPT TO, DATA on an already-greeted SMTP connection.
async fn deliver_message<R, W>(
    reader: &mut R,
    writer: &mut W,
    host: &str,
    from: &str,
    recipients: &[&String],
    data: &[u8],
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    // MAIL FROM
    send_cmd(writer, &format!("MAIL FROM:<{from}>")).await?;
    let resp = read_response(reader).await?;
    if !resp.starts_with('2') {
        anyhow::bail!("MAIL FROM rejected by {host}: {resp}");
    }

    // RCPT TO for each recipient.
    //
    // Track whether ANY recipient was accepted. Previously this loop only
    // logged a warning and then sent DATA unconditionally — so when every
    // recipient was rejected we asked the peer to accept a message with no
    // envelope, and it answered `503 5.5.1 MAIL FROM and RCPT TO required`.
    // That buried the *real* diagnosis (e.g. `550 5.1.2 No such domain here`)
    // behind a protocol error of our own making, and turned a permanent
    // rejection into something the queue retried for hours. Found by inter-node
    // smoke testing on the WG mesh, 2026-07-14.
    let mut accepted = 0usize;
    let mut last_reject: Option<String> = None;
    let mut all_rejects_permanent = true;
    for rcpt in recipients {
        send_cmd(writer, &format!("RCPT TO:<{rcpt}>")).await?;
        let resp = read_response(reader).await?;
        if resp.starts_with('2') {
            accepted += 1;
        } else {
            // A 4xx is transient (greylisting, over quota, temp DNS): the
            // message should be retried. Only an all-5xx set is a permanent
            // failure — one transient reject makes the whole attempt worth
            // retrying, since a later try may yet get that recipient accepted.
            if !resp.starts_with('5') {
                all_rejects_permanent = false;
            }
            // `resp` is remote-server bytes — sanitized before going
            // inline in the journal MESSAGE.
            tracing::warn!(
                rcpt = rcpt.as_str(),
                host = host,
                response = %resp,
                "RCPT TO:<{}> rejected by {host}: {}",
                crate::maillog::sanitize(rcpt),
                crate::maillog::sanitize(&resp),
            );
            last_reject = Some(resp);
        }
    }

    if accepted == 0 {
        // No envelope survived — DATA would be meaningless. Close the
        // transaction cleanly and surface the peer's OWN reason.
        let reason = last_reject.unwrap_or_else(|| "no recipients accepted".to_string());
        send_cmd(writer, "RSET").await.ok();
        let _ = read_response(reader).await;
        if all_rejects_permanent {
            return Err(
                PermanentFailure(format!("all recipients rejected by {host}: {reason}")).into(),
            );
        }
        anyhow::bail!("all recipients rejected by {host}: {reason}");
    }

    // DATA
    send_cmd(writer, "DATA").await?;
    let resp = read_response(reader).await?;
    if !resp.starts_with('3') {
        if resp.starts_with('5') {
            return Err(PermanentFailure(format!("DATA rejected by {host}: {resp}")).into());
        }
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

    let resp = read_response(reader).await?;
    if !resp.starts_with('2') {
        if resp.starts_with('5') {
            return Err(PermanentFailure(format!("Message rejected by {host}: {resp}")).into());
        }
        anyhow::bail!("Message rejected by {host}: {resp}");
    }

    // QUIT
    let _ = send_cmd(writer, "QUIT").await;

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

/// Triadic dispatch for outbound DKIM signing:
///
///   1. If `[[dkim.domain]]` is configured (`mail_auth_signer.is_some()`)
///      and the message's first `From:` header has a domain matching
///      a configured row (parent-label walk via
///      `MailAuthSigner::lookup_active`), sign with that row's key
///      and return the signed bytes.
///   2. On `NoSignerForDomain` (no matching row), or no parseable
///      `From:` header, or no `mail_auth_signer` configured at all,
///      fall through to the legacy single-key signer keyed by
///      `dkim_selector` + `dkim_private_key`.
///   3. If neither is configured, return `None` and the message
///      ships unsigned.
///
/// The fall-through path on `NoSignerForDomain` is deliberate: an
/// operator with one configured row for `example.com` plus the legacy
/// hostname-keyed pair still wants outbound mail from `noreply@`
/// to be signed (with `d=hostname`), not unsigned. Other
/// `MailAuthSigner` errors (key decode failures, internal errors)
/// log and return `None` — they indicate a configured-but-broken
/// state worth failing closed on.
async fn dkim_sign(cfg: &super::SmtpConfig, data: &[u8]) -> Option<Vec<u8>> {
    let snapshot = cfg.mail_auth_signer.load_full();
    if let Some(signer) = snapshot.as_ref() {
        if let Some(domain) = from_header_domain(data) {
            let mut buf = data.to_vec();
            match signer.sign_dkim(&mut buf, &domain).await {
                Ok(()) => return Some(buf),
                Err(MailAuthError::NoSignerForDomain(_)) => {
                    tracing::warn!(
                        from_domain = domain.as_str(),
                        hostname = %cfg.hostname,
                        "no [[dkim.domain]] row matches From-header domain; \
                         falling back to legacy hostname-keyed signer"
                    );
                    // fall through
                }
                Err(e) => {
                    tracing::error!(error = %e, "MailAuthSigner failed; not signing");
                    return None;
                }
            }
        } else {
            tracing::warn!(
                "outbound message has no parseable From: header; \
                 falling back to legacy hostname-keyed signer"
            );
            // fall through
        }
    }
    legacy_dkim_sign(cfg, data)
}

/// Legacy single-key signer keyed by `dkim_selector` +
/// `dkim_private_key`, with `d=` pinned to the daemon hostname.
/// Stays synchronous — it does one `std::fs::read_to_string` per call
/// plus an in-memory mail-auth invocation; wrapping in async buys
/// nothing. Returns `None` when no key is configured (the entire
/// pre-`[[dkim.domain]]` no-DKIM deployment shape) or when key
/// parsing / signing fails after logging the error.
fn legacy_dkim_sign(cfg: &super::SmtpConfig, data: &[u8]) -> Option<Vec<u8>> {
    let selector = cfg.dkim_selector.as_deref()?;
    let key_path = cfg.dkim_private_key.as_deref()?;

    let key_pem = match std::fs::read_to_string(key_path) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, path = key_path, "Failed to read DKIM private key");
            return None;
        }
    };

    #[allow(deprecated)]
    let pk = match RsaKey::from_pkcs8_pem(&key_pem).or_else(|_| RsaKey::from_rsa_pem(&key_pem)) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "Failed to parse DKIM RSA key");
            return None;
        }
    };

    let signer = DkimSigner::from_key(pk)
        .domain(&cfg.hostname)
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
        let domain = addr
            .rsplit('@')
            .next()
            .unwrap_or("localhost")
            .to_lowercase();
        map.entry(domain).or_default().push(addr);
    }
    map
}

#[cfg(test)]
mod dkim_dispatch_tests {
    //! Triadic dispatch coverage for `dkim_sign`:
    //!  - bucket A: legacy compat (no `mail_auth_signer`)
    //!  - bucket B: new signer happy path (per-domain match, parent walk)
    //!  - bucket C: fall-through (no domain match, no parseable From)
    //!
    //! Tests build a `SmtpConfig` (no DB / no SMTP listeners) and call
    //! `dkim_sign(&cfg, ...)` directly — the function only touches
    //! `cfg`, which is why commit 2 of the carving doc lifted the
    //! `&SmtpState` argument up to `&SmtpConfig`.

    use super::*;
    use cosmix_maild_auth::{
        Canon, DkimAlgorithm, DkimSignerConfig, Domain, HeaderSpec, MailAuthSigner,
    };
    use std::sync::Arc;

    // Mirrors the mail-auth published RSA-2048 test key embedded in
    // `cosmix-maild-auth::signer::tests` and `config::tests` — inline
    // here to keep this `#[cfg(test)]` module self-contained.
    const RSA_TEST_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
MIIEowIBAAKCAQEAv9XYXG3uK95115mB4nJ37nGeNe2CrARm1agrbcnSk5oIaEfM\n\
ZLUR/X8gPzoiNHZcfMZEVR6bAytxUhc5EvZIZrjSuEEeny+fFd/cTvcm3cOUUbIa\n\
UmSACj0dL2/KwW0LyUaza9z9zor7I5XdIl1M53qVd5GI62XBB76FH+Q0bWPZNkT4\n\
NclzTLspD/MTpNCCPhySM4Kdg5CuDczTH4aNzyS0TqgXdtw6A4Sdsp97VXT9fkPW\n\
9rso3lrkpsl/9EQ1mR/DWK6PBmRfIuSFuqnLKY6v/z2hXHxF7IoojfZLa2kZr9Ae\n\
d4l9WheQOTA19k5r2BmlRw/W9CrgCBo0Sdj+KQIDAQABAoIBAFPChEi/OvnulReB\n\
ECQWhOUYuNKlFKQU++2YEvZJ4+bMn5UgnE7wfJ1pj2Pr9xlfALz+OMHNrjMxGbaV\n\
KzdrT2uCkYcf78XjnhuH9gKIiXDUv4L4N+P3u6w8yOx4bFgOS9IjS53yDOPM7SC5\n\
g6dIg5aigHaHlffqIuFFv4yQMI/+Ai+zBKxS7wRhxK/7nnAuo28fe5MEdp57ho9/\n\
AGlDNsdg9zCgjwhokwFE3+AaD+bkUFm4gQ1XjkUFrlmnQn8vDQ0i9toEWhCj+UPY\n\
iOKL63MJnr90MXTXWLHoFj99wBp//mYygbF9Lj8fa28/oa8LWp3Jhb7QeMgH46iv\n\
3aLHbTECgYEA5M2dAw+nyMw9vYlkMejhwObKYP8Mr/6zcGMLCalYvRJM5iUAM0JI\n\
H6sM6pV9/nv167cbKocj3xYPdtE7FPOn4132MLM8Ne1f8nPE64Qrcbj5WBXvLnU8\n\
hpWbwe2Z8h7UUMKx6q4F1/TXYkc3ScxYwfjM4mP/pLsAOgVzRSEEgrUCgYEA1qNQ\n\
xaQHNWZ1O8WuTnqWd5JSsic6iURAmUcLeFDZY2PWhVoaQ8L/xMQhDYs1FIbLWArW\n\
4Qq3Ibu8AbSejAKuaJz7Uf26PX+PYVUwAOO0qamCJ8d/qd6So7qWMDyAY2yXI39Y\n\
1nMqRjr7bkEsggAZao7BKqA7ZtmogjOusBT38iUCgYEA06agJ8TDoKvOMRZ26PRU\n\
YO0dKLzGL8eclcoI29cbj0rud7aiiMg3j5PbTuUat95TjsjDCIQaWrM9etvxm2AJ\n\
Xfn9Uu96MyhyKQWOk46f4YMKpMElkARDCPw8KRhx39dE77AqhLyWCz8iPndCXbH6\n\
KPTOEl4OjYOuof2Is9nnIkECgYBh948RdsnXhNlzm8nwhiGRmBbou+EK8D0v+O5y\n\
Tyy6IcKzgSnFzgZh8EdJ4EUtBk1f9SqY8wQdgIvSl3daXorusuA/TzkngsaV3YUY\n\
ktZOLlF7CKLrjOyPkMWmZKcROmpNyH1q/IvKHHfQnizLdXIkYd4nL5WNX0F7lE1i\n\
j1+QhQKBgB2lviBK7rJFwlFYdQUP1NAN2dKxMZk8uJS8JglHrM0+8nRI83HbTdEQ\n\
vB0ManEKBkbS4T5n+gRtdEqKSDmWDTXDlrBfcdCHNQLwYtBpOotCqQn/AmfjcPBl\n\
byAbwh4+HiZ5JISoRZpiZqy67aJNVoXmdtb/E9mi7ozzytpxMNql\n\
-----END RSA PRIVATE KEY-----\n";

    fn write_test_key(dir: &std::path::Path) -> String {
        let p = dir.join("dkim.pem");
        std::fs::write(&p, RSA_TEST_PEM).expect("write key");
        p.to_string_lossy().into_owned()
    }

    fn make_signer(domain: &str, selector: &str) -> Arc<MailAuthSigner> {
        let cfg = DkimSignerConfig {
            domain: Domain::new(domain),
            selector: selector.into(),
            algorithm: DkimAlgorithm::RsaSha256,
            private_key_pem: RSA_TEST_PEM.as_bytes().to_vec(),
            canonicalization: (Canon::Relaxed, Canon::Relaxed),
            headers_to_sign: vec![
                HeaderSpec::Oversign("From".into()),
                HeaderSpec::Single("To".into()),
                HeaderSpec::Single("Subject".into()),
            ],
            active_for_signing: true,
            allow_body_length_tag: false,
        };
        Arc::new(MailAuthSigner::new(vec![cfg]))
    }

    fn base_cfg(hostname: &str) -> super::super::SmtpConfig {
        super::super::SmtpConfig {
            hostname: hostname.into(),
            ..Default::default()
        }
    }

    fn legacy_only_cfg(
        hostname: &str,
        key_path: String,
        selector: &str,
    ) -> super::super::SmtpConfig {
        let mut cfg = base_cfg(hostname);
        cfg.dkim_selector = Some(selector.into());
        cfg.dkim_private_key = Some(key_path);
        cfg
    }

    /// Extract the value of a DKIM tag (e.g. `"d"`) from a
    /// `DKIM-Signature:` header byte slice. Returns the substring up
    /// to the next `;` or end-of-header. Tolerates the
    /// `tag-name=value` form with optional whitespace.
    fn dkim_tag<'a>(signed: &'a [u8], tag: &str) -> Option<&'a str> {
        let header_end = signed.windows(2).position(|w| w == b"\r\n")?;
        let header = std::str::from_utf8(&signed[..header_end]).ok()?;
        // Skip the `DKIM-Signature:` prefix; mail-auth emits it.
        let body = header.split_once(':').map(|(_, v)| v).unwrap_or(header);
        for part in body.split(';') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix(&format!("{tag}=")) {
                return Some(rest);
            }
        }
        None
    }

    fn sample_message(from: &str) -> Vec<u8> {
        format!(
            "From: {from}\r\nTo: bob@example.com\r\n\
             Subject: hello\r\nDate: Tue, 19 May 2026 10:00:00 +1000\r\n\
             \r\nhello world\r\n"
        )
        .into_bytes()
    }

    // ---------- Bucket A — legacy compat ----------

    #[tokio::test]
    async fn dkim_sign_legacy_returns_none_when_unconfigured() {
        let cfg = base_cfg("mail.example.org");
        let msg = sample_message("alice@example.org");
        assert!(dkim_sign(&cfg, &msg).await.is_none());
    }

    #[tokio::test]
    async fn dkim_sign_legacy_path_unchanged_when_no_signer_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_test_key(dir.path());
        let cfg = legacy_only_cfg("mail.example.org", kp, "k1");
        let msg = sample_message("alice@example.org");

        let signed = dkim_sign(&cfg, &msg).await.expect("legacy signs");
        assert_eq!(dkim_tag(&signed, "d"), Some("mail.example.org"));
        assert_eq!(dkim_tag(&signed, "s"), Some("k1"));
        // Sanity: ends with the original body.
        assert!(signed.ends_with(b"hello world\r\n"));
    }

    // ---------- Bucket B — new signer happy path ----------

    #[tokio::test]
    async fn dkim_sign_new_signer_matches_from_domain() {
        let mut cfg = base_cfg("mail.example.org");
        cfg.mail_auth_signer =
            arc_swap::ArcSwap::from_pointee(Some(make_signer("example.org", "k2"))).into();
        let msg = sample_message("alice@example.org");

        let signed = dkim_sign(&cfg, &msg).await.expect("new signer signs");
        assert_eq!(dkim_tag(&signed, "d"), Some("example.org"));
        assert_eq!(dkim_tag(&signed, "s"), Some("k2"));
    }

    #[tokio::test]
    async fn dkim_sign_new_signer_walks_parent_label() {
        let mut cfg = base_cfg("mail.example.org");
        cfg.mail_auth_signer =
            arc_swap::ArcSwap::from_pointee(Some(make_signer("example.org", "k2"))).into();
        let msg = sample_message("alice@sub.example.org");

        let signed = dkim_sign(&cfg, &msg).await.expect("parent walk signs");
        assert_eq!(dkim_tag(&signed, "d"), Some("example.org"));
    }

    #[tokio::test]
    async fn dkim_sign_new_signer_lowercases_match() {
        // From-header domain is mixed-case; `from_header_domain`
        // lowercases before the lookup, so a row keyed `example.org`
        // still matches.
        let mut cfg = base_cfg("mail.example.org");
        cfg.mail_auth_signer =
            arc_swap::ArcSwap::from_pointee(Some(make_signer("example.org", "k2"))).into();
        let msg = sample_message("alice@EXAMPLE.org");

        let signed = dkim_sign(&cfg, &msg).await.expect("lowercase match");
        assert_eq!(dkim_tag(&signed, "d"), Some("example.org"));
    }

    // ---------- Bucket C — fall-through ----------

    #[tokio::test]
    async fn dkim_sign_no_match_falls_back_to_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_test_key(dir.path());
        let mut cfg = legacy_only_cfg("mail.example.org", kp, "k1");
        cfg.mail_auth_signer =
            arc_swap::ArcSwap::from_pointee(Some(make_signer("example.org", "k2"))).into();
        // From-header domain has no [[dkim.domain]] row → fall to legacy.
        let msg = sample_message("bob@other.com");

        let signed = dkim_sign(&cfg, &msg).await.expect("legacy fallback");
        assert_eq!(dkim_tag(&signed, "d"), Some("mail.example.org"));
        assert_eq!(dkim_tag(&signed, "s"), Some("k1"));
    }

    #[tokio::test]
    async fn dkim_sign_no_match_no_legacy_returns_none() {
        let mut cfg = base_cfg("mail.example.org");
        cfg.mail_auth_signer =
            arc_swap::ArcSwap::from_pointee(Some(make_signer("example.org", "k2"))).into();
        let msg = sample_message("bob@other.com");

        assert!(
            dkim_sign(&cfg, &msg).await.is_none(),
            "no match + no legacy must yield unsigned"
        );
    }

    #[tokio::test]
    async fn dkim_sign_unparseable_from_falls_back_to_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_test_key(dir.path());
        let mut cfg = legacy_only_cfg("mail.example.org", kp, "k1");
        cfg.mail_auth_signer =
            arc_swap::ArcSwap::from_pointee(Some(make_signer("example.org", "k2"))).into();
        // No From: header at all.
        let msg = b"To: bob@example.com\r\nSubject: hi\r\nDate: Tue, 19 May 2026 10:00:00 +1000\r\n\r\nbody\r\n".to_vec();

        let signed = dkim_sign(&cfg, &msg).await.expect("legacy fallback");
        assert_eq!(dkim_tag(&signed, "d"), Some("mail.example.org"));
    }

    #[tokio::test]
    async fn dkim_sign_unparseable_from_no_legacy_returns_none() {
        let mut cfg = base_cfg("mail.example.org");
        cfg.mail_auth_signer =
            arc_swap::ArcSwap::from_pointee(Some(make_signer("example.org", "k2"))).into();
        let msg = b"To: bob@example.com\r\n\r\nbody\r\n".to_vec();
        assert!(dkim_sign(&cfg, &msg).await.is_none());
    }
}

#[cfg(test)]
mod effective_host_tests {
    //! Bucket C tests for the `sender_effective_host` fallback chain
    //! (Phase 3 commit 3). The helper takes `(&Arc<Runtime>,
    //! &config_hostname, sender_domain, HostKind)` so it's exercised
    //! directly against a `MemoryStore`-backed runtime — no full
    //! `SmtpState` fixture required.
    //!
    //! The chain under test (per `_doc/planned/maild-multi-vhost-phase3.md`):
    //!
    //!   1. kind-specific field (`helo_identity` / `message_id_host`)
    //!   2. `primary_hostname`
    //!   3. row key (the FQDN)
    //!   4. `config.hostname` (read error, missing row, empty namespace,
    //!      or view-projection failure — outbound is best-effort).
    //!
    //! Fault injection mirrors the `FaultyStore` shape Phase 3 commit 2
    //! introduced for the receiver tests in `smtp/session.rs::tests`;
    //! duplicated here as Bucket C lives in a separate `#[cfg(test)]`
    //! module and cannot reach those helpers.
    use super::*;
    use cosmix_props::namespace::NamespaceName;
    use cosmix_props::record::{AuditEpoch, Nseq, Record, RecordEvent};
    use cosmix_props::store::{
        CompleteCommit, DeleteCommit, PropertyStore, ReconcileCommit, SetCommit, Snapshot,
        StoreError, StoreFuture,
    };
    use cosmix_props::{
        Actor, Hooks, MemoryStore, MergeMode, RecordKey, Runtime, SetOpts, Version,
        value::PropValue,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn build_runtime() -> Arc<Runtime> {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        Arc::new(Runtime::new(
            "maild",
            crate::props::domains::spec(Hooks::new(crate::props::domains::DomainsHooks::new())),
            store,
        ))
    }

    async fn seed(runtime: &Arc<Runtime>, domain: &str, mutate: impl FnOnce(&mut PropValue)) {
        let mut value = crate::props::domains::defaults_value(domain);
        mutate(&mut value);
        runtime
            .set(
                RecordKey::collection(crate::props::domains::namespace_name(), domain.to_string()),
                value,
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("phase3-bucket-c-tests").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .expect("seed row");
    }

    #[tokio::test]
    async fn returns_helo_identity_when_set() {
        let rt = build_runtime();
        seed(&rt, "example.com", |v| {
            if let PropValue::Object(o) = v {
                o.insert(
                    "helo_identity".into(),
                    PropValue::String("mail.example.com".into()),
                );
                o.insert(
                    "primary_hostname".into(),
                    PropValue::String("mx.example.com".into()),
                );
            }
        })
        .await;
        let got = sender_effective_host(&rt, "config.example", "example.com", HostKind::Helo).await;
        assert_eq!(got, "mail.example.com");
    }

    #[tokio::test]
    async fn falls_back_to_primary_hostname() {
        let rt = build_runtime();
        seed(&rt, "example.com", |v| {
            if let PropValue::Object(o) = v {
                o.insert(
                    "primary_hostname".into(),
                    PropValue::String("mx.example.com".into()),
                );
            }
        })
        .await;
        let got = sender_effective_host(&rt, "config.example", "example.com", HostKind::Helo).await;
        assert_eq!(got, "mx.example.com");
    }

    #[tokio::test]
    async fn falls_back_to_row_key() {
        let rt = build_runtime();
        // defaults_value leaves helo_identity / primary_hostname / message_id_host as Null.
        seed(&rt, "example.com", |_| {}).await;
        let got = sender_effective_host(&rt, "config.example", "example.com", HostKind::Helo).await;
        assert_eq!(got, "example.com");
    }

    #[tokio::test]
    async fn falls_back_to_config_hostname_when_row_absent() {
        // Empty namespace: nothing seeded — get(...) returns NotFound.
        let rt = build_runtime();
        let got = sender_effective_host(&rt, "config.example", "example.com", HostKind::Helo).await;
        assert_eq!(got, "config.example");
    }

    #[tokio::test]
    async fn message_id_kind_starts_at_message_id_host() {
        let rt = build_runtime();
        seed(&rt, "example.com", |v| {
            if let PropValue::Object(o) = v {
                o.insert(
                    "helo_identity".into(),
                    PropValue::String("mail.example.com".into()),
                );
                o.insert(
                    "message_id_host".into(),
                    PropValue::String("id.example.com".into()),
                );
                o.insert(
                    "primary_hostname".into(),
                    PropValue::String("mx.example.com".into()),
                );
            }
        })
        .await;
        let got =
            sender_effective_host(&rt, "config.example", "example.com", HostKind::MessageId).await;
        assert_eq!(got, "id.example.com");
        // Sanity: Helo kind on the same row hits helo_identity, not message_id_host.
        let got = sender_effective_host(&rt, "config.example", "example.com", HostKind::Helo).await;
        assert_eq!(got, "mail.example.com");
    }

    #[tokio::test]
    async fn sender_domain_of_extracts_domain() {
        assert_eq!(
            sender_domain_of("user@Example.ORG", "default.example"),
            "example.org"
        );
        assert_eq!(
            sender_domain_of("no-at-sign", "default.example"),
            "default.example"
        );
        assert_eq!(sender_domain_of("", "default.example"), "default.example");
        assert_eq!(
            sender_domain_of("user@", "default.example"),
            "default.example"
        );
    }

    // -- FaultyStore: delegates to an inner PropertyStore, with
    // injection toggles for list/get errors. Used by the
    // best-effort-on-substrate-error tests below.

    struct FaultyStore {
        inner: Arc<dyn PropertyStore>,
        fail_get: AtomicBool,
        corrupt_get_value: std::sync::Mutex<Option<PropValue>>,
    }

    impl FaultyStore {
        fn new(inner: Arc<dyn PropertyStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                fail_get: AtomicBool::new(false),
                corrupt_get_value: std::sync::Mutex::new(None),
            })
        }
        fn set_fail_get(&self, v: bool) {
            self.fail_get.store(v, Ordering::SeqCst);
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

    fn build_runtime_with_faulty() -> (Arc<Runtime>, Arc<FaultyStore>) {
        let inner: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let faulty = FaultyStore::new(inner);
        let runtime = Arc::new(Runtime::new(
            "maild",
            crate::props::domains::spec(Hooks::new(crate::props::domains::DomainsHooks::new())),
            faulty.clone() as Arc<dyn PropertyStore>,
        ));
        (runtime, faulty)
    }

    #[tokio::test]
    async fn falls_back_to_config_hostname_on_substrate_get_error() {
        let (rt, faulty) = build_runtime_with_faulty();
        // Seed a real row first so namespace isn't empty (though
        // sender_effective_host doesn't check list — only get). After
        // seeding, flip the get-fault.
        seed(&rt, "example.com", |v| {
            if let PropValue::Object(o) = v {
                o.insert(
                    "helo_identity".into(),
                    PropValue::String("mail.example.com".into()),
                );
            }
        })
        .await;
        faulty.set_fail_get(true);
        let got = sender_effective_host(&rt, "config.example", "example.com", HostKind::Helo).await;
        assert_eq!(got, "config.example");
    }

    #[tokio::test]
    async fn falls_back_to_config_hostname_on_view_corrupt() {
        let (rt, faulty) = build_runtime_with_faulty();
        seed(&rt, "example.com", |_| {}).await;
        // Override the get(...) response with a value whose `role` is
        // an int — view(...) will reject this.
        let bogus = {
            let mut o = std::collections::BTreeMap::new();
            o.insert("enabled".to_string(), PropValue::Bool(true));
            o.insert("role".to_string(), PropValue::Int(7));
            o.insert("helo_identity".to_string(), PropValue::Null);
            o.insert("message_id_host".to_string(), PropValue::Null);
            o.insert("primary_hostname".to_string(), PropValue::Null);
            o.insert("relay_recipients".to_string(), PropValue::List(Vec::new()));
            PropValue::Object(o)
        };
        *faulty.corrupt_get_value.lock().unwrap() = Some(bogus);
        let got = sender_effective_host(&rt, "config.example", "example.com", HostKind::Helo).await;
        assert_eq!(got, "config.example");
    }
}

#[cfg(test)]
mod rcpt_rejection_tests {
    //! The RCPT/DATA envelope contract, pinned at the protocol level.
    //!
    //! Regression cover for the bug found by inter-node smoke testing on the WG
    //! mesh (2026-07-14): `deliver_message` logged RCPT rejections but sent DATA
    //! regardless. With every recipient rejected the peer answered
    //! `503 5.5.1 MAIL FROM and RCPT TO required` — masking the real reason
    //! (`550 5.1.2 No such domain here`) behind a protocol error of our own
    //! making, and turning a permanent rejection into one the queue retried for
    //! hours.
    //!
    //! `deliver_message` is generic over AsyncRead/AsyncWrite, so these drive it
    //! against a scripted in-memory peer and assert on the bytes we actually put
    //! on the wire. No network, no sleeps.

    use super::*;

    /// Run `deliver_message` against a peer that replies with `replies` in
    /// order. Returns the error (if any) and everything we wrote.
    async fn run(replies: &[&str], recipients: &[&str]) -> (Option<anyhow::Error>, String) {
        let script = replies
            .iter()
            .map(|r| format!("{r}\r\n"))
            .collect::<String>();
        let mut reader = std::io::Cursor::new(script.into_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let owned: Vec<String> = recipients.iter().map(|s| s.to_string()).collect();
        let refs: Vec<&String> = owned.iter().collect();

        let res = deliver_message(
            &mut reader,
            &mut writer,
            "peer.test",
            "sender@from.test",
            &refs,
            b"Subject: t\r\n\r\nbody\r\n",
        )
        .await;
        (res.err(), String::from_utf8_lossy(&writer).into_owned())
    }

    #[tokio::test]
    async fn all_recipients_rejected_does_not_send_data() {
        // MAIL FROM ok, the only RCPT rejected 550, then RSET ok.
        let (err, wire) = run(
            &["250 ok", "550 5.1.2 No such domain here", "250 reset"],
            &["nobody@peer.test"],
        )
        .await;

        // The whole point: we must NOT ask for DATA with an empty envelope.
        assert!(
            !wire.contains("DATA"),
            "sent DATA with no accepted recipients; wire was:\n{wire}"
        );
        assert!(wire.contains("RSET"), "should reset the transaction");

        let err = err.expect("delivery must fail when no recipient is accepted");
        // The peer's OWN reason must survive — that is what the postmaster reads.
        assert!(
            err.to_string().contains("No such domain here"),
            "the remote reason must not be masked: {err}"
        );
        // 550 is final: retrying cannot change it.
        assert!(is_permanent(&err), "an all-5xx rejection must be permanent");
    }

    #[tokio::test]
    async fn transient_rejection_is_not_permanent() {
        // A 4xx (greylisting) must stay retryable, or we would bounce mail that
        // would have gone through on the next attempt.
        let (err, wire) = run(
            &["250 ok", "451 4.7.1 Greylisted, try again", "250 reset"],
            &["someone@peer.test"],
        )
        .await;

        assert!(!wire.contains("DATA"), "no envelope → still no DATA");
        let err = err.expect("delivery fails");
        assert!(
            !is_permanent(&err),
            "a 4xx must remain transient so the queue retries it"
        );
    }

    #[tokio::test]
    async fn partial_acceptance_still_delivers() {
        // One rejected, one accepted: the message is still owed to the accepted
        // recipient, so DATA must proceed. Dropping it would silently lose mail.
        let (err, wire) = run(
            &[
                "250 ok",           // MAIL FROM
                "550 no such user", // rcpt 1 rejected
                "250 ok",           // rcpt 2 accepted
                "354 go ahead",     // DATA
                "250 queued",       // end of body
                "221 bye",          // QUIT
            ],
            &["bad@peer.test", "good@peer.test"],
        )
        .await;

        assert!(
            err.is_none(),
            "must deliver to the accepted recipient: {err:?}"
        );
        assert!(
            wire.contains("DATA"),
            "DATA must be sent when ANY rcpt is accepted"
        );
    }

    #[tokio::test]
    async fn mixed_transient_and_permanent_rejects_stay_retryable() {
        // 5xx for one, 4xx for another, none accepted. The 4xx recipient may yet
        // succeed, so the attempt must be retried rather than bounced.
        let (err, _wire) = run(
            &["250 ok", "550 gone", "451 later", "250 reset"],
            &["a@peer.test", "b@peer.test"],
        )
        .await;
        let err = err.expect("delivery fails");
        assert!(
            !is_permanent(&err),
            "one transient reject makes the whole attempt retryable"
        );
    }

    #[tokio::test]
    async fn permanent_body_rejection_is_permanent() {
        // The envelope is fine but the peer refuses the message itself (5xx at
        // end-of-DATA) — e.g. a content or size rejection. Retrying re-sends the
        // identical bytes, so it is decided.
        let (err, _wire) = run(
            &[
                "250 ok",
                "250 ok",
                "354 go ahead",
                "552 5.3.4 Message too big",
            ],
            &["good@peer.test"],
        )
        .await;
        let err = err.expect("delivery fails");
        assert!(is_permanent(&err), "a 5xx on the body is final");
    }
}
