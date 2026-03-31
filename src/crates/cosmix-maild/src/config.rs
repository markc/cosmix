//! Configuration for cosmix-jmap.

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// JMAP HTTP listen address
    pub listen: String,
    /// Public base URL for JMAP
    pub base_url: String,
    /// SQLite database file path
    pub database_path: String,
    /// Blob storage directory
    pub blob_dir: String,
    /// Server hostname (used in SMTP EHLO and DKIM)
    pub hostname: String,

    // SMTP settings
    /// SMTP inbound listen address (port 25). None to disable.
    pub smtp_inbound: Option<String>,
    /// SMTPS submission listen address (port 465, implicit TLS). None to disable.
    pub smtp_smtps: Option<String>,
    /// Maximum message size in bytes
    pub max_message_size: Option<usize>,
    /// DKIM selector (e.g., "default")
    pub dkim_selector: Option<String>,
    /// Path to DKIM private key (PEM)
    pub dkim_private_key: Option<String>,
    /// Path to TLS certificate (PEM)
    pub tls_cert: Option<String>,
    /// Path to TLS private key (PEM)
    pub tls_key: Option<String>,

    // Spam filter settings
    /// Enable spam filtering (default: true)
    pub spam_enabled: Option<bool>,
    /// Spamlite per-user database directory
    pub spam_db_dir: Option<String>,
    /// Baseline spamlite database for new accounts
    pub spam_baseline_db: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8088".into(),
            base_url: "http://127.0.0.1:8088".into(),
            database_path: default_db_path(),
            blob_dir: default_blob_dir(),
            hostname: "localhost".into(),
            smtp_inbound: Some("0.0.0.0:2525".into()),
            smtp_smtps: None,
            max_message_size: None,
            dkim_selector: None,
            dkim_private_key: None,
            tls_cert: None,
            tls_key: None,
            spam_enabled: Some(true),
            spam_db_dir: None,
            spam_baseline_db: None,
        }
    }
}

fn default_db_path() -> String {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().join("cosmix-jmap").join("mail.db").to_string_lossy().into_owned())
        .unwrap_or_else(|| "/var/lib/cosmix-jmap/mail.db".into())
}

fn default_blob_dir() -> String {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().join("cosmix-jmap").join("blobs").to_string_lossy().into_owned())
        .unwrap_or_else(|| "/var/lib/cosmix-jmap/blobs".into())
}

fn default_spam_dir() -> String {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().join("cosmix-jmap").join("spamlite").to_string_lossy().into_owned())
        .unwrap_or_else(|| "/var/lib/cosmix-jmap/spamlite".into())
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml_cfg::from_str(&contents)?;
        Ok(config)
    }

    pub fn config_path() -> PathBuf {
        directories::BaseDirs::new()
            .map(|d| d.config_dir().join("cosmix").join("jmap.toml"))
            .unwrap_or_else(|| PathBuf::from("/etc/cosmix/jmap.toml"))
    }

    pub fn spam_db_dir(&self) -> String {
        self.spam_db_dir.clone().unwrap_or_else(default_spam_dir)
    }
}
