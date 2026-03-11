//! SMTP server — inbound (port 25) and submission (port 587).

pub mod session;
pub mod inbound;
pub mod queue;
pub mod delivery;

use std::sync::Arc;

use anyhow::Result;
use tokio::net::TcpListener;
use tracing;

use crate::db::Db;

/// SMTP server configuration.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub hostname: String,
    pub listen_inbound: Option<String>,
    pub listen_submission: Option<String>,
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
            listen_submission: Some("0.0.0.0:587".into()),
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
}

/// Start SMTP listeners (inbound + submission).
pub async fn start(db: Db, config: SmtpConfig) -> Result<()> {
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
    });

    // Start queue delivery worker
    let delivery_state = state.clone();
    tokio::spawn(async move {
        delivery::delivery_worker(delivery_state).await;
    });

    // Start inbound listener (port 25)
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

    // Start submission listener (port 587)
    if let Some(addr) = &state.config.listen_submission {
        let listener = TcpListener::bind(addr).await?;
        tracing::info!(addr = %addr, "SMTP submission listening");
        let sub_state = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let s = sub_state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = session::handle(stream, peer, s, true).await {
                                tracing::debug!(error = %e, peer = %peer, "SMTP submission error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "SMTP submission accept error");
                    }
                }
            }
        });
    }

    Ok(())
}
