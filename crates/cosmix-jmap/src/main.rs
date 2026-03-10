mod auth;
mod config;
mod db;
mod jmap;
mod smtp;

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cosmix_jmap=info".parse()?)
        )
        .init();

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

    let database = db::Db::connect(&cfg.database_url, &cfg.blob_dir).await?;

    match cli.command {
        Command::Migrate => {
            database.migrate().await?;
            println!("Migrations applied successfully.");
        }

        Command::Account { action } => match action {
            AccountAction::Add { email, password, name } => {
                let id = db::account::create(&database.pool, &email, &password, name.as_deref()).await?;
                println!("Created account {email} (id: {id})");
            }
            AccountAction::List => {
                let accounts = db::account::list(&database.pool).await?;
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
                if db::account::delete(&database.pool, &email).await? {
                    println!("Deleted account {email}");
                } else {
                    println!("Account {email} not found");
                }
            }
        },

        Command::Queue { action } => match action {
            QueueAction::List => {
                let entries = smtp::queue::list(&database.pool, 50).await?;
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
                            e.next_retry.format("%Y-%m-%d %H:%M:%S"),
                            e.last_error.unwrap_or_default()
                        );
                    }
                }
            }
            QueueAction::Flush => {
                let count = smtp::queue::flush(&database.pool).await?;
                println!("Flushed {count} queue entries for immediate retry.");
            }
        },

        Command::Serve => {
            // Start SMTP server
            let smtp_config = smtp::SmtpConfig {
                hostname: cfg.hostname.clone(),
                listen_inbound: cfg.smtp_inbound.clone(),
                listen_submission: cfg.smtp_submission.clone(),
                max_message_size: cfg.max_message_size.unwrap_or(25 * 1024 * 1024),
                dkim_selector: cfg.dkim_selector.clone(),
                dkim_private_key: cfg.dkim_private_key.clone(),
            };
            smtp::start(database.clone(), smtp_config).await?;

            // Start JMAP HTTP server
            let state = Arc::new(jmap::AppState {
                db: database,
                base_url: cfg.base_url.clone(),
            });

            let app = Router::new()
                .route("/.well-known/jmap", axum::routing::get(jmap::session))
                .route("/jmap", axum::routing::post(jmap::api))
                .route("/jmap/blob/{blobId}", axum::routing::get(jmap::blob_download))
                .route("/jmap/upload/{accountId}", axum::routing::post(jmap::blob_upload))
                .with_state(state);

            let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
            tracing::info!(addr = %cfg.listen, "cosmix-jmap JMAP listening");
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}
