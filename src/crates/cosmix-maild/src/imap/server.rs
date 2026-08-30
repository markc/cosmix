//! IMAPS listener — implicit TLS on port 993 (RFC 8314).
//!
//! Mirrors the SMTP listener pattern (`crate::smtp::start`):
//! load TLS cert+key once, bind one listener per configured address
//! so multi-homed boxes (WG IP + LAN IP) don't require `0.0.0.0`,
//! and spawn a per-connection task that terminates TLS before
//! handing off to [`crate::imap::session::handle_connection`].

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;

use crate::db::Db;
use crate::imap::config::ImapConfig;
use crate::imap::session::{self, AccountSlots};
use crate::mailstore::MailStore;
use crate::tls::current_tls_pair;

/// Bound IMAPS listener addresses returned by [`start`]. Tests pass
/// `listen_imaps = "127.0.0.1:0"` and read back the resolved port
/// from `imaps_addrs[0]`. The accept loops themselves are detached
/// tokio tasks — dropping the handle does not stop the server.
#[derive(Debug, Clone)]
pub struct ImapHandle {
    pub imaps_addrs: Vec<SocketAddr>,
}

/// Build TLS and start IMAPS listeners.
///
/// Returns an empty [`ImapHandle`] (no addresses) when
/// `listen_imaps` is empty or TLS material is absent — the daemon
/// treats IMAPS as opt-in for Phase 1 so existing deployments do not
/// accidentally start a new public service on `cargo install`.
pub async fn start(
    db: Db,
    mailstore: Arc<dyn MailStore>,
    cfg: ImapConfig,
    slots: Arc<AccountSlots>,
) -> Result<ImapHandle> {
    if cfg.listen_imaps.is_empty() {
        return Ok(ImapHandle {
            imaps_addrs: Vec::new(),
        });
    }
    // Refuse to start IMAPS if the slot is empty at startup. The
    // listener can still bind if the slot is later cleared, but the
    // per-accept call will refuse the handshake — same posture as
    // pre-Phase-5 except the empty-slot check is now a runtime read
    // rather than a config-time field check.
    if current_tls_pair(&cfg.tls_slot, &cfg.tls_config_cache).is_none() {
        return Err(anyhow::anyhow!(
            "IMAPS listener requires at least one [[maild.tls.identity]] (or legacy tls_cert/tls_key)"
        ));
    }

    let cfg = Arc::new(cfg);
    // `slots` is created by the caller (`runtime.rs`) and shared with
    // the `maild.stats.online` Bus surface, so the connection counts
    // this listener maintains are the ones the stats verb reads.

    let mut imaps_addrs: Vec<SocketAddr> = Vec::with_capacity(cfg.listen_imaps.len());
    for addr in cfg.listen_imaps.clone() {
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("IMAPS bind {addr}: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| anyhow::anyhow!("IMAPS local_addr {addr}: {e}"))?;
        imaps_addrs.push(bound);
        tracing::info!(addr = %bound, "IMAPS listening");

        let cfg_task = cfg.clone();
        let slots_task = slots.clone();
        let db_task = db.clone();
        let mailstore_task = mailstore.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let cfg = cfg_task.clone();
                        let slots = slots_task.clone();
                        let db = db_task.clone();
                        let mailstore = mailstore_task.clone();
                        // Captured before `acc.accept` consumes the
                        // stream so the dovecot-style `lip=` field
                        // records which bound IP the client hit (this
                        // daemon dual-binds WG + LAN).
                        let lip = stream.local_addr().ok();
                        // Per-connection TLS snapshot captured at
                        // accept time. A reload that swaps the slot
                        // mid-handshake leaves this connection on the
                        // captured `Arc<ServerConfig>`; future accepts
                        // see the new stamp.
                        let pair = current_tls_pair(&cfg.tls_slot, &cfg.tls_config_cache);
                        let (resolver, server_config) = match pair {
                            Some(p) => p,
                            None => {
                                crate::maillog::imap_tls_handshake_fail(
                                    peer,
                                    lip,
                                    "TLS disabled at accept time",
                                );
                                continue;
                            }
                        };
                        let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
                        tokio::spawn(async move {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    // Recover SNI before splitting —
                                    // `tokio::io::split` consumes the
                                    // `ServerConnection` handle.
                                    let sni =
                                        tls_stream.get_ref().1.server_name().map(|s| s.to_string());
                                    let negotiated_hostname = resolver
                                        .pick_for_sni(sni.as_deref())
                                        .map(|id| id.name.clone())
                                        .unwrap_or_else(|| cfg.hostname.clone());
                                    // `handle_connection` owns the
                                    // `imap-login: Disconnected` line
                                    // for every exit path (clean or
                                    // error); do NOT log a second one
                                    // here or each failed session is
                                    // double-counted.
                                    let _ = session::handle_connection(
                                        tls_stream,
                                        peer,
                                        lip,
                                        cfg,
                                        db,
                                        mailstore,
                                        slots,
                                        negotiated_hostname,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    crate::maillog::imap_tls_handshake_fail(
                                        peer,
                                        lip,
                                        &e.to_string(),
                                    );
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "IMAPS accept error");
                    }
                }
            }
        });
    }

    Ok(ImapHandle { imaps_addrs })
}
