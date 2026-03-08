use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct MailConfig {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub url: String,
    pub user: String,
    pub pass: String,
}

impl MailConfig {
    pub fn load() -> Result<Self> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let path = std::path::Path::new(&home)
            .join(".config")
            .join("cosmix")
            .join("mail.toml");

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
}
