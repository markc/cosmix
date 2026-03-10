//! PostgreSQL database layer.

pub mod account;
pub mod blob;
pub mod calendar;
pub mod changelog;
pub mod contact;
pub mod email;
pub mod mailbox;
pub mod thread;

use anyhow::Result;
use sqlx::PgPool;

/// Application database state.
#[derive(Clone)]
pub struct Db {
    pub pool: PgPool,
    pub blob_dir: std::path::PathBuf,
}

impl Db {
    pub async fn connect(database_url: &str, blob_dir: &str) -> Result<Self> {
        let pool = PgPool::connect(database_url).await?;
        let blob_dir = std::path::PathBuf::from(blob_dir);
        std::fs::create_dir_all(&blob_dir)?;
        Ok(Self { pool, blob_dir })
    }

    pub async fn migrate(&self) -> Result<()> {
        // Run migrations from embedded SQL files
        let migrations = [
            include_str!("../../migrations/001_initial.sql"),
            include_str!("../../migrations/002_smtp_queue.sql"),
            include_str!("../../migrations/003_calendars_contacts.sql"),
        ];
        for sql in migrations {
            sqlx::raw_sql(sql).execute(&self.pool).await?;
        }
        tracing::info!("Database migrations applied");
        Ok(())
    }
}
