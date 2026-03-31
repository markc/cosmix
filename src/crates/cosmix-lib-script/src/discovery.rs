//! Script discovery — scan the scripts directory for TOML and Mix definitions.

use std::path::PathBuf;

use crate::types::{Script, ScriptDef, ScriptMeta};

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
/// Returns `(id, Script)` pairs sorted by script name.
/// The `id` is the filename stem (e.g. "preview-in-viewer").
pub fn discover_scripts(service_name: &str) -> Vec<(String, Script)> {
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
            let ext = path.extension().and_then(|e| e.to_str());
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            match ext {
                Some("toml") => match parse_toml_script(&path) {
                    Ok(def) => scripts.push((id, Script::Toml(def))),
                    Err(e) => {
                        tracing::warn!("Failed to parse script {}: {e}", path.display());
                    }
                },
                Some("mix") => match parse_mix_meta(&path) {
                    Ok(meta) => scripts.push((id, Script::Mix { meta, path })),
                    Err(e) => {
                        tracing::warn!("Failed to read script {}: {e}", path.display());
                    }
                },
                _ => {}
            }
        }
    }

    scripts.sort_by(|a, b| a.1.meta().name.cmp(&b.1.meta().name));
    scripts
}

/// Parse a single TOML script definition file.
fn parse_toml_script(path: &std::path::Path) -> Result<ScriptDef, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    toml_cfg::from_str(&content).map_err(|e| e.to_string())
}

/// Parse Mix script metadata from comment headers.
///
/// Recognises `-- @script`, `-- @shortcut`, and `-- @description` directives:
///
/// ```mix
/// -- @script Preview in Viewer
/// -- @shortcut Ctrl+Shift+V
/// -- @description Opens current file in viewer
/// ```
///
/// Falls back to the filename stem as the script name if no `@script` directive.
fn parse_mix_meta(path: &std::path::Path) -> Result<ScriptMeta, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;

    let mut name = None;
    let mut shortcut = None;
    let mut description = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("--") {
            // Stop scanning after the comment header block
            if !trimmed.is_empty() {
                break;
            }
            continue;
        }
        let comment = trimmed.strip_prefix("--").unwrap().trim();
        if let Some(val) = comment.strip_prefix("@script ") {
            name = Some(val.trim().to_string());
        } else if let Some(val) = comment.strip_prefix("@shortcut ") {
            shortcut = Some(val.trim().to_string());
        } else if let Some(val) = comment.strip_prefix("@description ") {
            description = Some(val.trim().to_string());
        }
    }

    // Fall back to filename stem
    let name = name.unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Untitled".to_string())
    });

    Ok(ScriptMeta {
        name,
        shortcut,
        description,
    })
}
