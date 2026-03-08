use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn config_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join(".config").join("cosmic")
}

pub fn list_components() -> Result<Vec<String>> {
    let base = config_base();
    let mut components = Vec::new();
    if !base.exists() {
        return Ok(components);
    }
    for entry in std::fs::read_dir(&base).context("Failed to read COSMIC config dir")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                components.push(name.to_string());
            }
        }
    }
    components.sort();
    Ok(components)
}

pub fn list_keys(component: &str) -> Result<Vec<String>> {
    let dir = config_base().join(component).join("v1");
    let mut keys = Vec::new();
    if !dir.exists() {
        return Ok(keys);
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            if let Some(name) = entry.file_name().to_str() {
                keys.push(name.to_string());
            }
        }
    }
    keys.sort();
    Ok(keys)
}

pub fn read_key(component: &str, key: &str) -> Result<String> {
    let path = config_base().join(component).join("v1").join(key);
    std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))
}

pub fn write_key(component: &str, key: &str, value: &str) -> Result<()> {
    let dir = config_base().join(component).join("v1");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(key);
    std::fs::write(&path, value)
        .with_context(|| format!("Failed to write {}", path.display()))
}
