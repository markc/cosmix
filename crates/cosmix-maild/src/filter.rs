//! Spam filtering via spamlite — per-user Bayesian classification.

use std::path::PathBuf;

use anyhow::Result;
use spamlite::classifier::{self, Params, Verdict};
use spamlite::storage::Database;
use spamlite::tokenizer;

/// Per-user spam filter backed by spamlite SQLite databases.
pub struct SpamFilter {
    base_dir: PathBuf,
    baseline_db: Option<PathBuf>,
}

impl SpamFilter {
    pub fn new(base_dir: PathBuf, baseline_db: Option<PathBuf>) -> Self {
        Self { base_dir, baseline_db }
    }

    /// Classify a raw message. Returns (verdict, score).
    /// Score: 0.0 = definitely good, 1.0 = definitely spam.
    pub fn classify(&self, account_id: i32, raw_message: &[u8], threshold: f64) -> Result<(Verdict, f64)> {
        let db_path = self.db_path(account_id);
        if !db_path.exists() {
            self.ensure_db(account_id)?;
        }

        let db = Database::open(&db_path)?;
        let tokens = tokenizer::tokenize(raw_message);

        let params = Params {
            threshold,
            ..Params::default()
        };

        let (verdict, score) = classifier::classify(&db, &tokens, &params)?;
        Ok((verdict, score))
    }

    /// Train a raw message as spam.
    pub fn train_spam(&self, account_id: i32, raw_message: &[u8]) -> Result<()> {
        let db_path = self.db_path(account_id);
        if !db_path.exists() {
            self.ensure_db(account_id)?;
        }

        let db = Database::open(&db_path)?;
        let tokens = tokenizer::tokenize(raw_message);
        db.train(&tokens, true)?;

        tracing::info!(account_id, "Trained message as spam");
        Ok(())
    }

    /// Train a raw message as good/ham.
    pub fn train_good(&self, account_id: i32, raw_message: &[u8]) -> Result<()> {
        let db_path = self.db_path(account_id);
        if !db_path.exists() {
            self.ensure_db(account_id)?;
        }

        let db = Database::open(&db_path)?;
        let tokens = tokenizer::tokenize(raw_message);
        db.train(&tokens, false)?;

        tracing::info!(account_id, "Trained message as good");
        Ok(())
    }

    /// Ensure the spamlite database exists for an account.
    /// Copies from baseline if available, otherwise creates empty.
    pub fn ensure_db(&self, account_id: i32) -> Result<()> {
        let db_path = self.db_path(account_id);
        if db_path.exists() {
            return Ok(());
        }

        let dir = db_path.parent().unwrap();
        std::fs::create_dir_all(dir)?;

        if let Some(baseline) = &self.baseline_db {
            if baseline.exists() {
                std::fs::copy(baseline, &db_path)?;
                tracing::info!(account_id, "Seeded spamlite DB from baseline");
                return Ok(());
            }
        }

        // Create empty database
        let _db = Database::open(&db_path)?;
        tracing::info!(account_id, "Created empty spamlite DB");
        Ok(())
    }

    fn db_path(&self, account_id: i32) -> PathBuf {
        self.base_dir.join(format!("{account_id}")).join("db.sqlite")
    }
}
