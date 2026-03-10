use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WebConfig {
    /// Listen address (default: 0.0.0.0:3000)
    #[serde(default = "default_listen")]
    pub listen: String,
    /// PostgreSQL connection string
    #[serde(default = "default_database_url")]
    pub database_url: String,
    /// TLS certificate chain (PEM file path)
    pub tls_cert: Option<String>,
    /// TLS private key (PEM file path)
    pub tls_key: Option<String>,
}

fn default_listen() -> String {
    "0.0.0.0:3000".into()
}

fn default_database_url() -> String {
    "postgres://cosmic:cosmic@localhost/markweb".into()
}

impl WebConfig {
    pub fn load() -> Result<Self> {
        let config_dir = directories::ProjectDirs::from("", "", "cosmix")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| {
                dirs_fallback().join("cosmix")
            });

        let path = config_dir.join("web.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let config: WebConfig = toml_cfg::from_str(&text)?;
            Ok(config)
        } else {
            Ok(Self {
                listen: default_listen(),
                database_url: default_database_url(),
                tls_cert: None,
                tls_key: None,
            })
        }
    }
}

fn dirs_fallback() -> std::path::PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".config")
        })
}
