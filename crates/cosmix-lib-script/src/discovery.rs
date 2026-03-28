//! Script discovery — scan the scripts directory for TOML definitions.

use std::path::PathBuf;

use crate::types::ScriptDef;

/// Returns the scripts directory: `~/.config/cosmix/scripts/`.
pub fn scripts_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        PathBuf::from("/tmp")
    }
    .join("cosmix")
    .join("scripts")
}

/// Discover scripts for a service by scanning `global/` and `{service_name}/`.
///
/// Returns `(id, ScriptDef)` pairs sorted by script name.
/// The `id` is the TOML filename stem (e.g. "preview-in-viewer").
pub fn discover_scripts(service_name: &str) -> Vec<(String, ScriptDef)> {
    let base = scripts_dir();
    let mut scripts = Vec::new();

    // Scan global/ and {service_name}/ directories
    for dir_name in &["global", service_name] {
        let dir = base.join(dir_name);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml") {
                match parse_script(&path) {
                    Ok(def) => {
                        let id = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        scripts.push((id, def));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse script {}: {e}", path.display());
                    }
                }
            }
        }
    }

    scripts.sort_by(|a, b| a.1.script.name.cmp(&b.1.script.name));
    scripts
}

/// Parse a single TOML script definition file.
fn parse_script(path: &std::path::Path) -> Result<ScriptDef, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    toml_cfg::from_str(&content).map_err(|e| e.to_string())
}
