//! Settings file I/O and generic dot-path accessors.
//!
//! The typed API (`CosmixSettings::load()`) is what apps use directly.
//! The generic API (`get_value`, `set_value`, `list_section`) is used by
//! cosmix-configd to serve settings over AMP.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_cfg as toml;

use crate::CosmixSettings;

/// Default settings file path: `~/.config/cosmix/settings.toml`
pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "cosmix")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/etc/cosmix"))
}

/// Full path to the settings file.
pub fn config_path() -> PathBuf {
    config_dir().join("settings.toml")
}

/// Load settings from the default path. Creates the file with defaults
/// if it doesn't exist.
pub fn load() -> Result<CosmixSettings> {
    let path = config_path();
    if !path.exists() {
        let defaults = CosmixSettings::default();
        save_to(&defaults, &path)?;
        return Ok(defaults);
    }
    load_from(&path)
}

/// Load settings from a specific path.
pub fn load_from(path: &Path) -> Result<CosmixSettings> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let settings: CosmixSettings = toml::from_str(&content)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(settings)
}

/// Save settings to the default path.
pub fn save(settings: &CosmixSettings) -> Result<()> {
    save_to(settings, &config_path())
}

/// Save settings to a specific path. Creates parent directories.
pub fn save_to(settings: &CosmixSettings, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(settings)
        .context("serializing settings")?;
    std::fs::write(path, content)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ── Generic dot-path accessors (for AMP daemon) ──

/// Get a value by dot-path, e.g. "mon.refresh_interval_secs".
pub fn get_value(settings: &CosmixSettings, dotpath: &str) -> Result<serde_json::Value> {
    let table = to_toml_table(settings)?;
    let (section, key) = split_dotpath(dotpath)?;

    let section_table = table.get(section)
        .and_then(|v| v.as_table())
        .with_context(|| format!("section '{section}' not found"))?;

    let val = section_table.get(key)
        .with_context(|| format!("key '{key}' not found in [{section}]"))?;

    Ok(toml_to_json(val))
}

/// Set a value by dot-path. Returns the updated settings.
pub fn set_value(settings: &mut CosmixSettings, dotpath: &str, value: serde_json::Value) -> Result<()> {
    let mut table = to_toml_table(settings)?;
    let (section, key) = split_dotpath(dotpath)?;

    let section_table = table.get_mut(section)
        .and_then(|v| v.as_table_mut())
        .with_context(|| format!("section '{section}' not found"))?;

    section_table.insert(key.to_string(), json_to_toml(&value));

    // Deserialize back into the typed struct
    let updated: CosmixSettings = table.try_into()
        .context("applying updated value to settings")?;
    *settings = updated;

    Ok(())
}

/// List all keys in a section as JSON key-value pairs.
pub fn list_section(settings: &CosmixSettings, section: &str) -> Result<serde_json::Value> {
    let table = to_toml_table(settings)?;
    let section_table = table.get(section)
        .and_then(|v| v.as_table())
        .with_context(|| format!("section '{section}' not found"))?;

    let map: serde_json::Map<String, serde_json::Value> = section_table.iter()
        .map(|(k, v)| (k.clone(), toml_to_json(v)))
        .collect();

    Ok(serde_json::Value::Object(map))
}

/// List all section names.
pub fn list_sections(settings: &CosmixSettings) -> Result<Vec<String>> {
    let table = to_toml_table(settings)?;
    Ok(table.keys().cloned().collect())
}

/// List all settings as a flat JSON object with dot-path keys.
pub fn list_all(settings: &CosmixSettings) -> Result<BTreeMap<String, serde_json::Value>> {
    let table = to_toml_table(settings)?;
    let mut out = BTreeMap::new();

    for (section, val) in &table {
        if let Some(section_table) = val.as_table() {
            for (key, v) in section_table {
                out.insert(format!("{section}.{key}"), toml_to_json(v));
            }
        }
    }

    Ok(out)
}

// ── Helpers ──

fn split_dotpath(dotpath: &str) -> Result<(&str, &str)> {
    let (section, key) = dotpath.split_once('.')
        .with_context(|| format!("invalid dot-path '{dotpath}' — expected 'section.key'"))?;
    if section.is_empty() || key.is_empty() {
        bail!("invalid dot-path '{dotpath}' — section and key must be non-empty");
    }
    Ok((section, key))
}

fn to_toml_table(settings: &CosmixSettings) -> Result<toml::map::Map<String, toml::Value>> {
    let val = toml::Value::try_from(settings)
        .context("serializing settings to TOML value")?;
    match val {
        toml::Value::Table(t) => Ok(t),
        _ => bail!("settings did not serialize to a TOML table"),
    }
}

fn toml_to_json(val: &toml::Value) -> serde_json::Value {
    match val {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::json!(*i),
        toml::Value::Float(f) => serde_json::json!(*f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Array(a) => serde_json::Value::Array(
            a.iter().map(toml_to_json).collect()
        ),
        toml::Value::Table(t) => {
            let map: serde_json::Map<String, serde_json::Value> = t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
    }
}

fn json_to_toml(val: &serde_json::Value) -> toml::Value {
    match val {
        serde_json::Value::String(s) => toml::Value::String(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                toml::Value::String(n.to_string())
            }
        }
        serde_json::Value::Bool(b) => toml::Value::Boolean(*b),
        serde_json::Value::Array(a) => toml::Value::Array(
            a.iter().map(json_to_toml).collect()
        ),
        serde_json::Value::Object(m) => {
            let mut table = toml::map::Map::new();
            for (k, v) in m {
                table.insert(k.clone(), json_to_toml(v));
            }
            toml::Value::Table(table)
        }
        serde_json::Value::Null => toml::Value::String(String::new()),
    }
}
