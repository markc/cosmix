use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MailConfig {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountConfig {
    pub name: String,
    pub url: String,
    pub user: String,
    pub pass: String,
}

impl MailConfig {
    fn config_path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        std::path::Path::new(&home)
            .join(".config")
            .join("cosmix")
            .join("mail.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();

        if !path.exists() {
            anyhow::bail!(
                "Config not found: {}\n\nCreate it with:\n\n\
                 [[accounts]]\n\
                 name = \"myserver\"\n\
                 url = \"https://mail.example.com:8443\"\n\
                 user = \"me@example.com\"\n\
                 pass = \"secret\"\n",
                path.display()
            );
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let config: MailConfig = toml_cfg::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;

        if config.accounts.is_empty() {
            anyhow::bail!("No accounts configured in {}", path.display());
        }

        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let content = toml_cfg::to_string_pretty(self)
            .with_context(|| "Failed to serialize config")?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }
}
