use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A single entry in the Clip List (ARexx SETCLIP/GETCLIP equivalent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipEntry {
    pub value: serde_json::Value,
    pub set_by: String,
    pub set_at: u64, // seconds since UNIX epoch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

impl ClipEntry {
    pub fn new(value: serde_json::Value, set_by: String, ttl_secs: Option<u64>) -> Self {
        let set_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self { value, set_by, set_at, ttl_secs }
    }

    pub fn is_expired(&self) -> bool {
        if let Some(ttl) = self.ttl_secs {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            now > self.set_at + ttl
        } else {
            false
        }
    }

    /// Remaining TTL in seconds, or None if no TTL set.
    pub fn remaining_ttl(&self) -> Option<u64> {
        self.ttl_secs.map(|ttl| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let expires_at = self.set_at + ttl;
            expires_at.saturating_sub(now)
        })
    }
}

/// The persistent clip list store.
pub type ClipList = HashMap<String, ClipEntry>;

/// Remove expired entries from a clip list.
pub fn prune_expired(clips: &mut ClipList) {
    clips.retain(|_, entry| !entry.is_expired());
}

/// Load clip list from a JSON file, pruning expired entries.
pub fn load_from_file(path: &Path) -> ClipList {
    match std::fs::read_to_string(path) {
        Ok(data) => {
            match serde_json::from_str::<ClipList>(&data) {
                Ok(mut clips) => {
                    prune_expired(&mut clips);
                    tracing::info!("Loaded {} clips from {}", clips.len(), path.display());
                    clips
                }
                Err(e) => {
                    tracing::warn!("Failed to parse cliplist.json: {e}");
                    HashMap::new()
                }
            }
        }
        Err(_) => HashMap::new(),
    }
}

/// Save clip list to a JSON file.
pub fn save_to_file(path: &Path, clips: &ClipList) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(clips)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json)
}

/// Config directory path for cliplist.json
pub fn cliplist_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("cosmix")
        .join("cliplist.json")
}
