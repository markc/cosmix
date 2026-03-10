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
    /// PostgreSQL connection URL
    pub database_url: String,
    /// Blob storage directory
    pub blob_dir: String,
    /// Server hostname (used in SMTP EHLO and DKIM)
    pub hostname: String,

    // SMTP settings
    /// SMTP inbound listen address (port 25). None to disable.
    pub smtp_inbound: Option<String>,
    /// SMTP submission listen address (port 587). None to disable.
    pub smtp_submission: Option<String>,
    /// Maximum message size in bytes
    pub max_message_size: Option<usize>,
    /// DKIM selector (e.g., "default")
    pub dkim_selector: Option<String>,
    /// Path to DKIM private key (PEM)
    pub dkim_private_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8088".into(),
            base_url: "http://127.0.0.1:8088".into(),
            database_url: "postgres://localhost/cosmix_jmap".into(),
            blob_dir: default_blob_dir(),
            hostname: "localhost".into(),
            smtp_inbound: Some("0.0.0.0:2525".into()),
            smtp_submission: Some("0.0.0.0:2587".into()),
            max_message_size: None,
            dkim_selector: None,
            dkim_private_key: None,
        }
    }
}

fn default_blob_dir() -> String {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().join("cosmix-jmap").join("blobs").to_string_lossy().into_owned())
        .unwrap_or_else(|| "/var/lib/cosmix-jmap/blobs".into())
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
}
