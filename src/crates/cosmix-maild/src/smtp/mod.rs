//! SMTP server — inbound (port 25) and submission (port 465 implicit TLS).

pub mod session;
pub mod inbound;
pub mod queue;
pub mod delivery;
pub mod bounce;

use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tracing;

use crate::db::Db;
use crate::filter::SpamFilter;

/// SMTP server configuration.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub hostname: String,
    pub listen_inbound: Option<String>,
    pub listen_smtps: Option<String>,
    pub max_message_size: usize,
    pub dkim_selector: Option<String>,
    pub dkim_private_key: Option<String>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            hostname: "localhost".into(),
            listen_inbound: Some("0.0.0.0:25".into()),
            listen_smtps: None,
            max_message_size: 25 * 1024 * 1024, // 25 MB
            dkim_selector: None,
            dkim_private_key: None,
            tls_cert: None,
            tls_key: None,
        }
    }
}

/// Shared state for SMTP sessions.
#[derive(Clone)]
pub struct SmtpState {
    pub db: Db,
    pub config: SmtpConfig,
    pub tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    pub spam_filter: Arc<SpamFilter>,
}

/// Start SMTP listeners (inbound + SMTPS submission).
pub async fn start(db: Db, config: SmtpConfig, spam_filter: Arc<SpamFilter>) -> Result<()> {
    // Load TLS certificate if configured
    let tls_acceptor = if let (Some(cert_path), Some(key_path)) = (&config.tls_cert, &config.tls_key) {
        let cert_data = std::fs::read(cert_path)?;
        let key_data = std::fs::read(key_path)?;

        let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_data[..])
            .filter_map(|r| r.ok())
            .collect();
        let key = rustls_pemfile::private_key(&mut &key_data[..])
            .ok()
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("No private key found in {key_path}"))?;

        let tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        Some(tokio_rustls::TlsAcceptor::from(Arc::new(tls_config)))
    } else {
        None
    };

    let state = Arc::new(SmtpState {
        db,
        config,
        tls_acceptor,
        spam_filter,
    });

    // Start queue delivery worker
    let delivery_state = state.clone();
    tokio::spawn(async move {
        delivery::delivery_worker(delivery_state).await;
    });

    // Start inbound listener (port 25) — plaintext with optional STARTTLS
    if let Some(addr) = &state.config.listen_inbound {
        let listener = TcpListener::bind(addr).await?;
        tracing::info!(addr = %addr, "SMTP inbound listening");
        let inbound_state = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let s = inbound_state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = session::handle(stream, peer, s, false).await {
                                tracing::debug!(error = %e, peer = %peer, "SMTP session error");
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

    // Start SMTPS submission listener (port 465) — implicit TLS
    if let Some(addr) = &state.config.listen_smtps {
        let acceptor = state.tls_acceptor.clone()
            .ok_or_else(|| anyhow::anyhow!("SMTPS listener requires tls_cert and tls_key"))?;
        let listener = TcpListener::bind(addr).await?;
        tracing::info!(addr = %addr, "SMTPS submission listening");
        let sub_state = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let s = sub_state.clone();
                        let acc = acceptor.clone();
                        tokio::spawn(async move {
                            match acc.accept(stream).await {
                                Ok(tls_stream) => {
                                    if let Err(e) = session::handle_tls(tls_stream, peer, s).await {
                                        tracing::debug!(error = %e, peer = %peer, "SMTPS session error");
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, peer = %peer, "SMTPS TLS handshake failed");
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

    Ok(())
}
