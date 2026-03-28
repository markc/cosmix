//! Shared infrastructure for cosmix daemon services.
//!
//! Provides common initialization patterns used by all 6 daemons:
//! tracing setup, graceful shutdown signal, and optional TLS configuration.

/// Initialize tracing with env filter and a service-specific default level.
///
/// Reads `RUST_LOG` from the environment; falls back to `"{crate_name}=info"`.
///
/// ```ignore
/// cosmix_daemon::init_tracing("cosmix_maild");
/// ```
pub fn init_tracing(crate_name: &str) {
    let default = format!("{crate_name}=info");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default.into()),
        )
        .init();
}

/// Wait for a shutdown signal (Ctrl+C or SIGTERM on Unix).
///
/// Use with `tokio::select!` in the daemon's main loop:
///
/// ```ignore
/// tokio::select! {
///     _ = serve_forever() => {}
///     _ = cosmix_daemon::shutdown_signal() => {
///         tracing::info!("shutting down");
///     }
/// }
/// ```
pub async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }

    tracing::info!("shutdown signal received");
}

/// Load a TLS server configuration from PEM cert and key files.
///
/// Returns a `tokio_rustls::TlsAcceptor` ready for use in a TCP accept loop.
/// Installs the `ring` crypto provider on first call.
///
/// ```ignore
/// let acceptor = cosmix_daemon::load_tls_config("cert.pem", "key.pem")?;
/// ```
#[cfg(feature = "tls")]
pub fn load_tls_config(cert_path: &str, key_path: &str) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use std::sync::Arc;

    // Install ring crypto provider (idempotent — ignores if already installed)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert_data = std::fs::read(cert_path)?;
    let key_data = std::fs::read(key_path)?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_data[..])
        .filter_map(|r| r.ok())
        .collect();

    let key = rustls_pemfile::private_key(&mut &key_data[..])
        .ok()
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(tls_config)))
}
