mod config;
mod db;
mod ipc;
mod routes;

use anyhow::Result;
use std::net::SocketAddr;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let config = config::WebConfig::load()?;
    let addr: SocketAddr = config.listen.parse()?;

    // Connect to database
    let db = db::connect(&config.database_url).await?;
    info!("Connected to database");

    let app = routes::router(&config, db).await?;

    if let (Some(cert_path), Some(key_path)) = (&config.tls_cert, &config.tls_key) {
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path).await?;
        info!("cosmix-web listening on https://{addr}");
        axum_server::bind_rustls(addr, tls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        info!("cosmix-web listening on http://{addr}");
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
    }

    Ok(())
}
