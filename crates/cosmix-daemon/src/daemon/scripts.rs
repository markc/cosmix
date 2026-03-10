//! Script directory scanning and script-list push to app ports.
//!
//! Scans `~/.config/cosmix/scripts/{appname}/` for .lua files
//! and sends them to running apps via the `__scripts__` port command.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cosmix_port::ScriptInfo;

/// Base directory for user scripts.
pub fn scripts_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".config").join("cosmix").join("scripts")
}

/// Scan a directory for .lua files and return ScriptInfo for each.
pub fn scan_scripts(dir: &Path) -> Vec<ScriptInfo> {
    let mut scripts = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return scripts,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("lua") && path.is_file() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                scripts.push(ScriptInfo {
                    display_name: stem_to_display(stem),
                    path: path.to_string_lossy().to_string(),
                });
            }
        }
    }

    scripts.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    scripts
}

/// Convert a filename stem to a display name.
/// "add_watermark" -> "Add Watermark"
/// "batch-sharpen" -> "Batch Sharpen"
fn stem_to_display(stem: &str) -> String {
    stem.split(|c: char| c == '_' || c == '-')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{upper}{}", chars.as_str())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Scan all app script directories and return a map of port_name -> scripts.
pub fn scan_all_app_scripts() -> HashMap<String, Vec<ScriptInfo>> {
    let base = scripts_dir();
    let mut result = HashMap::new();

    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return result,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let scripts = scan_scripts(&path);
                if !scripts.is_empty() {
                    result.insert(name.to_string(), scripts);
                }
            }
        }
    }

    result
}

/// Push script list to a specific app port via its Unix socket.
pub async fn push_scripts_to_port(socket_path: &str, scripts: &[ScriptInfo]) -> anyhow::Result<()> {
    let args = serde_json::to_value(scripts)?;
    cosmix_port::call_port(socket_path, "__scripts__", args).await?;
    Ok(())
}

/// Push scripts to all registered ports that have matching script directories.
pub async fn push_scripts_to_all_ports(
    ports: &HashMap<String, super::registry::PortInfo>,
) {
    let all_scripts = scan_all_app_scripts();

    for (port_name, info) in ports {
        // Port name in registry is "COSMIX-CALC.1", directory name is "cosmix-calc"
        let dir_name = port_name
            .split('.')
            .next()
            .unwrap_or(port_name)
            .to_lowercase();

        if let Some(scripts) = all_scripts.get(&dir_name) {
            let socket = info.socket.to_string_lossy().to_string();
            if let Err(e) = push_scripts_to_port(&socket, scripts).await {
                tracing::debug!("Failed to push scripts to {port_name}: {e}");
            } else {
                tracing::info!("Pushed {} scripts to {port_name}", scripts.len());
            }
        }
    }
}
