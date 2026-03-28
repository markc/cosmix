#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod auth;
mod config;
mod db;
mod filter;
mod jmap;
mod smtp;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cosmix-jmap", about = "Minimal JMAP + SMTP server")]
struct Cli {
    /// Config file path
    #[arg(short, long)]
    config: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the JMAP + SMTP server
    Serve,
    /// Run database migrations
    Migrate,
    /// Account management
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// SMTP queue management
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
}

#[derive(Subcommand)]
enum AccountAction {
    /// Add a new account
    Add {
        /// Email address
        email: String,
        /// Password
        password: String,
        /// Display name
        #[arg(short, long)]
        name: Option<String>,
    },
    /// List all accounts
    List,
    /// Delete an account
    Delete {
        /// Email address
        email: String,
    },
}

#[derive(Subcommand)]
enum QueueAction {
    /// List queued messages
    List,
    /// Flush queue (retry all now)
    Flush,
}

#[tokio::main]
async fn main() -> Result<()> {
    cosmix_daemon::init_tracing("cosmix_maild");

    let cli = Cli::parse();

    let cfg = if let Some(path) = &cli.config {
        config::Config::load(path)?
    } else {
        let default_path = config::Config::config_path();
        if default_path.exists() {
            config::Config::load(&default_path.to_string_lossy())?
        } else {
            config::Config::default()
        }
    };

    let database = db::Db::connect(&cfg.database_path, &cfg.blob_dir).await?;

    match cli.command {
        Command::Migrate => {
            database.migrate().await?;
            println!("Migrations applied successfully.");
        }

        Command::Account { action } => match action {
            AccountAction::Add { email, password, name } => {
                let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
                    .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))?;
                let id = db::account::create(&database.conn, &email, &hash, name.as_deref()).await?;
                println!("Created account {email} (id: {id})");
            }
            AccountAction::List => {
                let accounts = db::account::list(&database.conn).await?;
                if accounts.is_empty() {
                    println!("No accounts.");
                } else {
                    println!("{:<6} {:<40} {}", "ID", "Email", "Name");
                    println!("{}", "-".repeat(60));
                    for a in accounts {
                        println!("{:<6} {:<40} {}", a.id, a.email, a.name.unwrap_or_default());
                    }
                }
            }
            AccountAction::Delete { email } => {
                if db::account::delete(&database.conn, &email).await? {
                    println!("Deleted account {email}");
                } else {
                    println!("Account {email} not found");
                }
            }
        },

        Command::Queue { action } => match action {
            QueueAction::List => {
                let entries = smtp::queue::list(&database.conn, 50).await?;
                if entries.is_empty() {
                    println!("Queue is empty.");
                } else {
                    println!("{:<6} {:<30} {:<6} {:<20} {}", "ID", "From", "Tries", "Next Retry", "Error");
                    println!("{}", "-".repeat(90));
                    for e in entries {
                        println!(
                            "{:<6} {:<30} {:<6} {:<20} {}",
                            e.id,
                            e.from_addr,
                            e.attempts,
                            e.next_retry,
                            e.last_error.unwrap_or_default()
                        );
                    }
                }
            }
            QueueAction::Flush => {
                let count = smtp::queue::flush(&database.conn).await?;
                println!("Flushed {count} queue entries for immediate retry.");
            }
        },

        Command::Serve => {
            // Initialize spam filter
            let spam_filter = Arc::new(filter::SpamFilter::new(
                PathBuf::from(cfg.spam_db_dir()),
                cfg.spam_baseline_db.as_ref().map(PathBuf::from),
            ));

            // Start SMTP server
            let smtp_config = smtp::SmtpConfig {
                hostname: cfg.hostname.clone(),
                listen_inbound: cfg.smtp_inbound.clone(),
                listen_smtps: cfg.smtp_smtps.clone(),
                max_message_size: cfg.max_message_size.unwrap_or(25 * 1024 * 1024),
                dkim_selector: cfg.dkim_selector.clone(),
                dkim_private_key: cfg.dkim_private_key.clone(),
                tls_cert: cfg.tls_cert.clone(),
                tls_key: cfg.tls_key.clone(),
            };
            smtp::start(database.clone(), smtp_config, spam_filter.clone()).await?;

            // State change broadcast channel for EventSource push
            let (state_tx, _) = tokio::sync::broadcast::channel::<jmap::StateChange>(256);

            // Start JMAP HTTP server
            let state = Arc::new(jmap::AppState {
                db: database,
                base_url: cfg.base_url.clone(),
                spam_filter,
                state_tx,
            });

            let cors = tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any);

            let app = Router::new()
                .route("/.well-known/jmap", axum::routing::get(jmap::session))
                .route("/jmap", axum::routing::post(jmap::api))
                .route("/jmap/blob/{blobId}", axum::routing::get(jmap::blob_download))
                .route("/jmap/upload/{accountId}", axum::routing::post(jmap::blob_upload))
                .route("/jmap/eventsource", axum::routing::get(jmap::eventsource))
                .layer(cors)
                .with_state(state);

            let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
            tracing::info!(addr = %cfg.listen, "cosmix-jmap JMAP listening");

            // Use TLS if cert/key configured
            if let (Some(cert_path), Some(key_path)) = (&cfg.tls_cert, &cfg.tls_key) {
                let tls_acceptor = cosmix_daemon::load_tls_config(cert_path, key_path)?;

                tracing::info!("JMAP HTTPS enabled");
                loop {
                    let (stream, _peer) = listener.accept().await?;
                    let acceptor = tls_acceptor.clone();
                    let app = app.clone();
                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let io = hyper_util::rt::TokioIo::new(tls_stream);
                                let service = hyper_util::service::TowerToHyperService::new(app);
                                if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                                    hyper_util::rt::TokioExecutor::new(),
                                )
                                .serve_connection(io, service)
                                .await
                                {
                                    tracing::debug!(error = %e, "HTTPS connection error");
                                }
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "TLS handshake failed");
                            }
                        }
                    });
                }
            } else {
                axum::serve(listener, app).await?;
            }
        }
    }

    Ok(())
}
