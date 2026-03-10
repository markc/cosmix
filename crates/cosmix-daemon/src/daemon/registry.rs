//! Port registry — discovers and tracks running cosmix-port app sockets.
//!
//! Watches `/run/user/$UID/cosmix/ports/` for `.sock` files, performs HELP
//! handshakes to learn each app's commands, and periodically cleans up dead ports.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use super::state::SharedState;

/// Metadata for a discovered app port.
#[derive(Debug, Clone, Serialize)]
pub struct PortInfo {
    /// Uppercase port name, e.g. "COSMIX-CALC.1"
    pub name: String,
    /// Socket path on disk
    pub socket: PathBuf,
    /// Commands the app supports (populated via HELP handshake)
    pub commands: Vec<String>,
    /// When the port was first discovered
    #[serde(skip)]
    pub discovered_at: Instant,
}

/// Registry of all known app ports (NOT built-in daemon ports).
#[derive(Debug, Default)]
pub struct PortRegistry {
    /// Ports keyed by lowercase base name (e.g. "cosmix-calc")
    pub ports: HashMap<String, PortInfo>,
}

impl PortRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a port by name (case-insensitive, with or without instance suffix).
    ///
    /// Accepts: "cosmix-calc", "COSMIX-CALC", "COSMIX-CALC.1", "cosmix-calc.1"
    pub fn find(&self, query: &str) -> Option<&PortInfo> {
        let q = query.to_lowercase();
        // Strip ".N" instance suffix for lookup
        let base = q.strip_suffix(|c: char| c == '.' || c.is_ascii_digit())
            .map(|s| s.trim_end_matches('.'))
            .unwrap_or(&q);

        // Try exact lowercase key first, then base name
        self.ports.get(&q)
            .or_else(|| self.ports.get(base))
    }

    /// Get socket path for a port name, if registered.
    pub fn socket_path(&self, query: &str) -> Option<PathBuf> {
        self.find(query).map(|p| p.socket.clone())
    }
}

/// Convert a socket filename to an uppercase port name.
///
/// `cosmix-calc.sock` → `COSMIX-CALC.1`
/// `cosmix-view-2.sock` → `COSMIX-VIEW.2`
fn socket_to_port_name(filename: &str) -> (String, String) {
    let base = filename.strip_suffix(".sock").unwrap_or(filename);

    // Check for instance suffix: "appname-N" where N is a number
    let (name_part, instance) = if let Some(pos) = base.rfind('-') {
        let suffix = &base[pos + 1..];
        if suffix.chars().all(|c| c.is_ascii_digit()) && !suffix.is_empty() {
            (&base[..pos], suffix.to_string())
        } else {
            (base, "1".to_string())
        }
    } else {
        (base, "1".to_string())
    };

    let upper = format!("{}.{instance}", name_part.to_uppercase());
    let lower = name_part.to_lowercase();
    (lower, upper)
}

/// Scan the port directory and update the registry.
///
/// - New sockets get a HELP handshake to discover commands.
/// - Missing sockets get removed from the registry.
pub async fn scan_ports(state: &SharedState, port_dir: &str) {
    let dir = Path::new(port_dir);
    if !dir.exists() {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("Failed to read port dir: {e}");
            return;
        }
    };

    // Collect current socket files
    let mut found_sockets: HashMap<String, PathBuf> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sock") {
            if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                let (lower_key, _upper_name) = socket_to_port_name(filename);
                found_sockets.insert(lower_key, path);
            }
        }
    }

    // Remove ports whose sockets no longer exist
    {
        let mut s = state.write().unwrap();
        let dead: Vec<String> = s.port_registry.ports.keys()
            .filter(|k| !found_sockets.contains_key(*k))
            .cloned()
            .collect();
        for key in dead {
            tracing::info!("Port deregistered: {}", s.port_registry.ports[&key].name);
            s.port_registry.ports.remove(&key);
        }
    }

    // Discover new ports
    let mut newly_registered = Vec::new();
    for (key, socket_path) in &found_sockets {
        let already_known = {
            let s = state.read().unwrap();
            s.port_registry.ports.contains_key(key)
        };

        if !already_known {
            let filename = socket_path.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("");
            let (_lower, upper_name) = socket_to_port_name(filename);

            // HELP handshake: ask the app what commands it supports
            let commands = match handshake_help(socket_path).await {
                Ok(cmds) => {
                    tracing::info!("Port registered: {upper_name} ({} commands)", cmds.len());
                    cmds
                }
                Err(e) => {
                    tracing::debug!("HELP handshake failed for {upper_name}: {e}");
                    // Register anyway with empty commands — the port exists
                    Vec::new()
                }
            };

            let info = PortInfo {
                name: upper_name.clone(),
                socket: socket_path.clone(),
                commands,
                discovered_at: Instant::now(),
            };

            newly_registered.push((key.clone(), upper_name, socket_path.clone()));
            let mut s = state.write().unwrap();
            s.port_registry.ports.insert(key.clone(), info);
        }
    }

    // Push scripts to newly registered ports
    for (_key, port_name, socket_path) in &newly_registered {
        let dir_name = port_name
            .split('.')
            .next()
            .unwrap_or(port_name)
            .to_lowercase();
        let script_dir = super::scripts::scripts_dir().join(&dir_name);
        let scripts = super::scripts::scan_scripts(&script_dir);
        if !scripts.is_empty() {
            let sock = socket_path.to_string_lossy().to_string();
            if let Err(e) = super::scripts::push_scripts_to_port(&sock, &scripts).await {
                tracing::debug!("Failed to push scripts to {port_name}: {e}");
            } else {
                tracing::info!("Pushed {} scripts to {port_name}", scripts.len());
            }
        }
    }
}

/// Connect to a port socket and send HELP to discover its commands.
async fn handshake_help(socket_path: &Path) -> Result<Vec<String>> {
    let socket_str = socket_path.to_string_lossy();
    let data = cosmix_port::call_port(
        &socket_str,
        "help",
        serde_json::Value::Null,
    ).await?;

    // Parse the HELP response — expect { "commands": [...] } or a list
    if let Some(cmds) = data.get("commands").and_then(|v| v.as_array()) {
        Ok(cmds.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect())
    } else if let Some(arr) = data.as_array() {
        Ok(arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect())
    } else {
        Ok(Vec::new())
    }
}

/// Heartbeat check: verify all registered ports are still alive.
/// Dead ports are removed from the registry.
pub async fn heartbeat(state: &SharedState) {
    let ports: Vec<(String, PathBuf)> = {
        let s = state.read().unwrap();
        s.port_registry.ports.iter()
            .map(|(k, v)| (k.clone(), v.socket.clone()))
            .collect()
    };

    for (key, socket_path) in ports {
        if !socket_path.exists() {
            let mut s = state.write().unwrap();
            if let Some(info) = s.port_registry.ports.remove(&key) {
                tracing::info!("Port dead (socket gone): {}", info.name);
            }
            continue;
        }

        // Try a quick connect to verify the port is alive
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::UnixStream::connect(&socket_path),
        ).await {
            Ok(Ok(_stream)) => {
                // Port is alive — we don't need to send anything,
                // successful connect is proof enough
            }
            _ => {
                let mut s = state.write().unwrap();
                if let Some(info) = s.port_registry.ports.remove(&key) {
                    tracing::info!("Port dead (connect failed): {}", info.name);
                }
            }
        }
    }
}

/// Spawn the port discovery + heartbeat background loop.
///
/// Scans every `scan_interval_ms` milliseconds and runs heartbeat every 10s.
pub fn spawn_port_watcher(state: SharedState, port_dir: String, scan_interval_ms: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut heartbeat_counter = 0u64;
        let heartbeat_every = 10_000 / scan_interval_ms; // heartbeat every ~10s

        loop {
            {
                let s = state.read().unwrap();
                if !s.running {
                    break;
                }
            }

            scan_ports(&state, &port_dir).await;

            heartbeat_counter += 1;
            if heartbeat_counter >= heartbeat_every {
                heartbeat(&state).await;
                heartbeat_counter = 0;
            }

            tokio::time::sleep(std::time::Duration::from_millis(scan_interval_ms)).await;
        }
        tracing::debug!("Port watcher stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_naming() {
        let (lower, upper) = socket_to_port_name("cosmix-calc.sock");
        assert_eq!(lower, "cosmix-calc");
        assert_eq!(upper, "COSMIX-CALC.1");

        let (lower, upper) = socket_to_port_name("cosmix-view-2.sock");
        assert_eq!(lower, "cosmix-view");
        assert_eq!(upper, "COSMIX-VIEW.2");

        let (lower, upper) = socket_to_port_name("cosmix-mail.sock");
        assert_eq!(lower, "cosmix-mail");
        assert_eq!(upper, "COSMIX-MAIL.1");
    }

    #[test]
    fn registry_find_case_insensitive() {
        let mut reg = PortRegistry::new();
        reg.ports.insert("cosmix-calc".into(), PortInfo {
            name: "COSMIX-CALC.1".into(),
            socket: PathBuf::from("/tmp/test.sock"),
            commands: vec!["calc".into(), "result".into()],
            discovered_at: Instant::now(),
        });

        assert!(reg.find("cosmix-calc").is_some());
        assert!(reg.find("COSMIX-CALC").is_some());
        assert!(reg.find("COSMIX-CALC.1").is_some());
        assert!(reg.find("cosmix-calc.1").is_some());
        assert!(reg.find("nonexistent").is_none());
    }
}
