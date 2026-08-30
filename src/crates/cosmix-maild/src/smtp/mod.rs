//! SMTP server — inbound (port 25) and submission (port 465 implicit TLS).

pub mod bounce;
pub mod delivery;
pub mod headers;
pub mod inbound;
pub mod queue;
pub mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;

use arc_swap::ArcSwap;
use cosmix_maild_auth::{MailAuthSigner, MailAuthVerifier};
use cosmix_maild_bayesian::DefaultClassifier;
use cosmix_maild_rules::DefaultRuleEngine;
use cosmix_props::runtime::Runtime;
use tokio::sync::broadcast;

use crate::bus::verdict::VerdictEvent;
use crate::db::Db;
use crate::mailstore::SqliteMailStore;
use crate::rule_stats::RuleStats;
use crate::tls::{ServerConfigCache, TlsSlot, current_tls_pair};

/// SMTP server configuration.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub hostname: String,
    /// Inbound listen addresses (port 25 / 2525). Empty = disabled.
    pub listen_inbound: Vec<String>,
    /// Inbound bind addresses that require an active STARTTLS upgrade
    /// before accepting mail (a plaintext `MAIL FROM` on a listed bind →
    /// `530`). Empty = opportunistic on every bind. See
    /// [`crate::config::Config::require_starttls_inbound`].
    pub require_starttls_inbound: Vec<String>,
    /// SMTPS submission listen addresses (port 465). Empty = disabled.
    pub listen_smtps: Vec<String>,
    /// Parsed outbound source-bind addresses (≤1 per family used).
    /// See [`crate::config::Config::smtp_outbound_bind`].
    pub outbound_bind: Vec<std::net::IpAddr>,
    pub max_message_size: usize,
    /// Whether opaque vtoken RCPTs (the C9 single-segment HMAC namespace) are
    /// accepted at RCPT TO. Set at startup by the C9 pre-flight scan
    /// ([`crate::runtime`]): `false` (fail-safe) if any existing account/alias
    /// local-part already collides with the opaque token shape, so an opaque
    /// RCPT can never silently swallow a real mailbox. Segmented vtoken
    /// acceptance is unaffected by this flag.
    pub opaque_rcpt_enabled: bool,
    pub dkim_selector: Option<String>,
    pub dkim_private_key: Option<String>,
    /// Per-domain signer built from `[[dkim.domain]]` rows and
    /// `maild.domains` substrate rows. The inner `Option` is `None`
    /// when neither source is configured — the legacy
    /// `dkim_selector` / `dkim_private_key` path runs verbatim in
    /// that case. When `Some`, the outbound deliver path tries this
    /// first (parent walk via `MailAuthSigner::lookup_active`) and
    /// falls back to the legacy single-key signer only on
    /// `NoSignerForDomain`. The outer `Arc<ArcSwap<...>>` is shared
    /// with `bus::run` (`bus::dkim::dispatch` swaps a freshly-built
    /// signer in atomically); each delivery thread captures a
    /// `load_full()` snapshot per sign so a mid-flight rotation
    /// never tears the active key.
    pub mail_auth_signer: Arc<ArcSwap<Option<Arc<MailAuthSigner>>>>,
    /// Live-swappable TLS resolver slot. The listener accept loops
    /// snapshot this once per accepted connection via
    /// [`current_tls_pair`] and pass the captured `Arc<SniCertResolver>`
    /// into the session so a mid-session reload cannot tear the
    /// greeting/handshake hostname pair. `None` (`load_full()`) means
    /// TLS is disabled — SMTPS refuses to start, plaintext SMTP serves
    /// without STARTTLS.
    pub tls_slot: TlsSlot,
    /// Per-resolver `Arc<ServerConfig>` cache shared with the IMAPS
    /// listener. Phase 5a-commit-2's `maild.tls.reload` verb stores a
    /// new resolver in `tls_slot` and `clear()`s this cache; in-flight
    /// handshakes keep the old `Arc<ServerConfig>` alive via their
    /// captured snapshot and drain cleanly without coordination.
    pub tls_config_cache: Arc<ServerConfigCache>,
    /// Path to a Mix script for inbound mail routing (optional).
    pub inbound_filter: Option<String>,
    /// Suppress the outbound delivery worker spawn. Integration tests
    /// that enqueue a message to a fake / RFC-2606 remote rely on this
    /// — without it, the worker reaches into real DNS/MX for the
    /// recipient domain on a fast retry loop, slowing CI and leaving
    /// rescheduled-retry rows in the queue that the assertion code
    /// then has to filter against. Production always leaves this
    /// `false`.
    pub disable_outbound_delivery: bool,
    /// Test-only MX override map. When populated, `deliver_to_domain`
    /// short-circuits both the `mx_lookup` and `TcpStream::connect`
    /// paths and dials the mapped `SocketAddr` directly. The full
    /// EHLO / MAIL FROM / RCPT TO / DATA wire flow still runs — only
    /// the resolver/transport seam is replaced — so per-domain DKIM
    /// signing and per-domain HELO selection on `delivery_worker` are
    /// exercised end-to-end.
    ///
    /// Operator-invisible: production config leaves this empty and
    /// the loader does not parse it from TOML. Populated only by
    /// in-process integration tests that bind a fake MX on
    /// `127.0.0.1` and need to capture the wire-level outbound
    /// session. Mirrors the [`disable_outbound_delivery`] class —
    /// test-only knob with a single production default.
    pub test_mx_overrides: HashMap<String, SocketAddr>,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            hostname: "localhost".into(),
            listen_inbound: vec!["0.0.0.0:25".into()],
            require_starttls_inbound: Vec::new(),
            listen_smtps: vec![],
            outbound_bind: Vec::new(),
            max_message_size: 25 * 1024 * 1024, // 25 MB
            opaque_rcpt_enabled: true,
            dkim_selector: None,
            dkim_private_key: None,
            mail_auth_signer: Arc::new(ArcSwap::from_pointee(None)),
            tls_slot: crate::tls::new_tls_slot(None),
            tls_config_cache: Arc::new(ServerConfigCache::new()),
            inbound_filter: None,
            disable_outbound_delivery: false,
            test_mx_overrides: HashMap::new(),
        }
    }
}

/// Shared state for SMTP sessions.
#[derive(Clone)]
pub struct SmtpState {
    pub db: Db,
    pub config: SmtpConfig,
    pub mail_auth: Arc<MailAuthVerifier>,
    pub rule_engine: Arc<DefaultRuleEngine>,
    pub rule_stats: Arc<RuleStats>,
    pub classifier: Arc<DefaultClassifier>,
    /// MailStore handle for inbound DATA delivery. The deliver path
    /// chains `mds.put_blob` then `create_email` to land the message
    /// in MDS via the recipient's `with_set_tx` boundary. Shared
    /// across all SMTP sessions; each delivery clones the `Arc`, not
    /// the underlying store.
    pub mailstore: Arc<SqliteMailStore>,
    /// Broadcast handle for the `maild.verdict` topic. The deliver
    /// path does `verdict_tx.send(event).ok()` post-commit; the Bus
    /// publisher task drains receivers. `Sender` is `Clone`, so each
    /// delivery clone of `SmtpState` carries an independent handle
    /// without needing an `Arc` wrapper.
    pub verdict_tx: broadcast::Sender<VerdictEvent>,
    /// Property-substrate runtime for `maild.account_overrides`. The
    /// inbound classify path reads the override row keyed by recipient
    /// email; an absent or tombstoned row resolves to
    /// `AccountOverrides::default()` (the pre-Phase-2 behaviour).
    pub overrides_runtime: Arc<Runtime>,
    /// Property-substrate runtime for `maild.domains`. Plumbed in
    /// Phase 3 commit 1 ahead of the receiver / sender consumers in
    /// commits 2–3. Commit 1 only stores the field; the RCPT TO gate
    /// and per-domain HELO/Message-ID lookups land in subsequent
    /// commits. See `_doc/planned/maild-multi-vhost-phase3.md`.
    pub domains_runtime: Arc<Runtime>,
    /// Property-substrate runtime for `maild.aliases` (Phase 1 local
    /// aliases). Inbound: recipient lookups resolve through it before
    /// `get_by_email` so mail to an alias lands in the target's mailbox.
    /// Outbound: the submission sender-authz check accepts a `MAIL FROM`
    /// that is an alias of the authenticated account.
    pub aliases_runtime: Arc<Runtime>,
    /// vtoken registry. Inbound delivery resolves each recipient through
    /// it (parse `$TO` → look up `user_id` → validate PIN → resolve the
    /// segment-3 service) before the normal alias path; a valid token
    /// delivers into its content account and injects
    /// `$VTOKEN_{VALID,USER,SERVICE}` into the inbound filter.
    pub vtoken_store: Arc<crate::vtoken::VtokenStore>,
    /// Validity-blind rate cap on the token-shaped namespace (C7) — sheds a flood
    /// before any resolve work, identically for valid + invalid tokens. `Arc` so
    /// every cloned `SmtpState` shares ONE limiter (a per-clone counter would defeat
    /// the global ceiling).
    pub token_rate: Arc<crate::vtoken::TokenRateLimiter>,
    /// Bounds concurrent fire-and-forget token-dispatch tasks (C8). The accept path
    /// `try_acquire`s (NEVER awaits before the 250 — that would re-introduce a
    /// load-dependent timing channel) and sheds on overflow, so a slow MDS/filter
    /// stall can't grow an unbounded background-task backlog.
    pub token_dispatch_sem: Arc<tokio::sync::Semaphore>,
}

/// Bound SMTP listener addresses returned by [`start`]. Test fixtures
/// pass `listen_inbound = "127.0.0.1:0"` and read the resolved port
/// out of `inbound_addrs[0]`; production callers can ignore the
/// handle. The listener tasks themselves are detached on the runtime
/// — dropping the handle does not stop the SMTP server.
#[derive(Debug, Clone)]
pub struct SmtpHandle {
    pub inbound_addrs: Vec<SocketAddr>,
    pub smtps_addrs: Vec<SocketAddr>,
}

/// Start SMTP listeners (inbound + SMTPS submission). Returns the
/// resolved [`SocketAddr`]s so callers binding `:0` for ephemeral
/// ports (e.g. integration tests) can discover the assigned port.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    db: Db,
    config: SmtpConfig,
    mail_auth: Arc<MailAuthVerifier>,
    rule_engine: Arc<DefaultRuleEngine>,
    rule_stats: Arc<RuleStats>,
    classifier: Arc<DefaultClassifier>,
    mailstore: Arc<SqliteMailStore>,
    verdict_tx: broadcast::Sender<VerdictEvent>,
    overrides_runtime: Arc<Runtime>,
    domains_runtime: Arc<Runtime>,
    aliases_runtime: Arc<Runtime>,
    vtoken_store: Arc<crate::vtoken::VtokenStore>,
) -> Result<SmtpHandle> {
    // SMTPS requires TLS material at startup; if `listen_smtps` is set
    // but the slot is empty, refuse to bind (matches pre-Phase-5
    // behaviour). Plaintext SMTP serves regardless — STARTTLS is
    // advertised only when the per-connection snapshot is non-empty.
    if !config.listen_smtps.is_empty()
        && current_tls_pair(&config.tls_slot, &config.tls_config_cache).is_none()
    {
        return Err(anyhow::anyhow!(
            "SMTPS listener requires at least one [[maild.tls.identity]] (or legacy tls_cert/tls_key)"
        ));
    }

    // Fail loud on a malformed `require_starttls_inbound` entry rather
    // than silently not-matching it at connection time (which would
    // disable the TLS requirement the operator asked for). Entries must
    // be `ip:port` socket addresses — a hostname or typo can't be matched
    // safely against an accepted connection's local `SocketAddr`.
    for entry in &config.require_starttls_inbound {
        entry.parse::<SocketAddr>().map_err(|e| {
            anyhow::anyhow!(
                "require_starttls_inbound entry {entry:?} is not an ip:port socket address \
                 ({e}) — use the specific bind IP (e.g. \"203.0.113.5:25\"), or \
                 \"0.0.0.0:25\"/\"[::]:25\" to require STARTTLS on every bind of that port"
            )
        })?;
    }

    let state = Arc::new(SmtpState {
        db,
        config,
        mail_auth,
        rule_engine,
        rule_stats,
        classifier,
        mailstore,
        verdict_tx,
        overrides_runtime,
        domains_runtime,
        aliases_runtime,
        vtoken_store,
        token_rate: Arc::new(crate::vtoken::TokenRateLimiter::with_defaults()),
        token_dispatch_sem: Arc::new(tokio::sync::Semaphore::new(128)),
    });

    // Start queue delivery worker (suppressed in tests that enqueue
    // RFC-2606 / fake-remote recipients — the worker would otherwise
    // hammer real DNS for the bogus domain).
    if !state.config.disable_outbound_delivery {
        let delivery_state = state.clone();
        tokio::spawn(async move {
            delivery::delivery_worker(delivery_state).await;
        });
    } else {
        tracing::info!(
            "outbound delivery worker suppressed via SmtpConfig.disable_outbound_delivery"
        );
    }

    let mut inbound_addrs: Vec<SocketAddr> = Vec::with_capacity(state.config.listen_inbound.len());
    let mut smtps_addrs: Vec<SocketAddr> = Vec::with_capacity(state.config.listen_smtps.len());

    // Start inbound listener (port 25) — plaintext with optional STARTTLS.
    // One listener+accept-loop per configured address so we can multi-home
    // (e.g. WG IP + LAN IP) without binding 0.0.0.0.
    for addr in state.config.listen_inbound.clone() {
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("SMTP inbound bind {}: {}", addr, e))?;
        let bound = listener
            .local_addr()
            .map_err(|e| anyhow::anyhow!("SMTP inbound local_addr {}: {}", addr, e))?;
        inbound_addrs.push(bound);
        tracing::info!(addr = %bound, "SMTP inbound listening");
        let inbound_state = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let s = inbound_state.clone();
                        // Snapshot the resolver + cached ServerConfig
                        // at TCP-accept time. A reload that swaps the
                        // slot mid-connection cannot tear this pair:
                        // the session uses the captured resolver for
                        // greeting hostname resolution and the same
                        // ServerConfig for a possible STARTTLS upgrade
                        // later in the conversation.
                        let tls_pair =
                            current_tls_pair(&s.config.tls_slot, &s.config.tls_config_cache);
                        tokio::spawn(async move {
                            if let Err(e) = session::handle(stream, peer, s, false, tls_pair).await
                            {
                                crate::maillog::smtp_disconnect(peer, &e.to_string());
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "SMTP accept error");
                    }
                }
            }
        });
    }

    // Start SMTPS submission listener (port 465) — implicit TLS.
    for addr in state.config.listen_smtps.clone() {
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("SMTPS bind {}: {}", addr, e))?;
        let bound = listener
            .local_addr()
            .map_err(|e| anyhow::anyhow!("SMTPS local_addr {}: {}", addr, e))?;
        smtps_addrs.push(bound);
        tracing::info!(addr = %bound, "SMTPS submission listening");
        let sub_state = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let s = sub_state.clone();
                        // Capture the resolver + ServerConfig snapshot
                        // at TCP-accept time. A reload after this point
                        // affects future connections only; this
                        // handshake completes against the stamp it
                        // accepted under.
                        let tls_pair =
                            current_tls_pair(&s.config.tls_slot, &s.config.tls_config_cache);
                        let pair = match tls_pair {
                            Some(pair) => pair,
                            None => {
                                crate::maillog::smtp_disconnect(
                                    peer,
                                    "SMTPS accept: TLS disabled at accept time",
                                );
                                continue;
                            }
                        };
                        let acceptor = tokio_rustls::TlsAcceptor::from(pair.1.clone());
                        tokio::spawn(async move {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    if let Err(e) =
                                        session::handle_tls(tls_stream, peer, s, pair).await
                                    {
                                        crate::maillog::smtp_disconnect(peer, &e.to_string());
                                    }
                                }
                                Err(e) => {
                                    crate::maillog::smtp_disconnect(
                                        peer,
                                        &format!("TLS handshake: {e}"),
                                    );
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "SMTPS accept error");
                    }
                }
            }
        });
    }

    Ok(SmtpHandle {
        inbound_addrs,
        smtps_addrs,
    })
}
