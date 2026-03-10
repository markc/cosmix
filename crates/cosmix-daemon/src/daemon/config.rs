//! Daemon configuration loaded from `~/.config/cosmix/config.toml`.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Returns the current user's UID as a string.
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Returns the machine hostname, falling back to "unknown".
fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn default_socket() -> String {
    format!("/run/user/{}/cosmix/cosmix.sock", uid())
}

fn default_port_dir() -> String {
    format!("/run/user/{}/cosmix/ports", uid())
}

fn default_node_name() -> String {
    hostname()
}

fn default_listen_port() -> u16 {
    9800
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub daemon: DaemonSection,
    pub mesh: MeshSection,
    pub web: WebSection,
    pub lua: LuaSection,
    pub clipboard: ClipboardSection,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            daemon: DaemonSection::default(),
            mesh: MeshSection::default(),
            web: WebSection::default(),
            lua: LuaSection::default(),
            clipboard: ClipboardSection::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DaemonSection {
    #[serde(default = "default_socket")]
    pub socket: String,

    #[serde(default = "default_port_dir")]
    pub port_dir: String,
}

impl Default for DaemonSection {
    fn default() -> Self {
        Self {
            socket: default_socket(),
            port_dir: default_port_dir(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MeshSection {
    pub enabled: bool,

    #[serde(default = "default_node_name")]
    pub node_name: String,

    #[serde(default = "default_listen_port")]
    pub listen_port: u16,

    #[serde(default)]
    pub wg_ip: String,

    #[serde(default)]
    pub peers: HashMap<String, PeerConfig>,
}

impl Default for MeshSection {
    fn default() -> Self {
        Self {
            enabled: false,
            node_name: default_node_name(),
            listen_port: default_listen_port(),
            wg_ip: String::new(),
            peers: HashMap::new(),
        }
    }
}

/// Configuration for a mesh peer.
#[derive(Debug, Deserialize, Clone)]
pub struct PeerConfig {
    pub wg_ip: String,
    #[serde(default = "default_listen_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct WebSection {
    pub enabled: bool,
    pub url: String,
    pub token: String,
}

impl Default for WebSection {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            token: String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct LuaSection {
    pub watch_dirs: Vec<String>,
    pub startup_scripts: Vec<String>,
}

impl Default for LuaSection {
    fn default() -> Self {
        Self {
            watch_dirs: Vec::new(),
            startup_scripts: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
pub struct ClipboardSection {
    #[serde(default = "default_true")]
    pub serve: bool,
}

impl Default for ClipboardSection {
    fn default() -> Self {
        Self { serve: true }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl DaemonConfig {
    /// Load configuration from `~/.config/cosmix/config.toml`.
    ///
    /// If the file does not exist, returns the default configuration.
    /// If the file exists but is malformed, returns an error.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();

        if !path.exists() {
            tracing::debug!("No config file at {}, using defaults", path.display());
            return Ok(Self::default());
        }

        tracing::info!("Loading config from {}", path.display());
        let contents = std::fs::read_to_string(&path)?;
        let config: DaemonConfig = toml_cfg::from_str(&contents)?;
        Ok(config)
    }

    /// Returns the expected config file path.
    pub fn config_path() -> PathBuf {
        directories::BaseDirs::new()
            .map(|dirs| dirs.config_dir().join("cosmix").join("config.toml"))
            .unwrap_or_else(|| {
                // Fallback if BaseDirs cannot determine home
                PathBuf::from(format!("/home/{}/.config/cosmix/config.toml", whoami()))
            })
    }
}

/// Simple whoami fallback using the USER env var.
fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = DaemonConfig::default();
        let uid = uid();

        assert!(cfg.daemon.socket.contains(&uid.to_string()));
        assert!(cfg.daemon.port_dir.contains("ports"));
        assert!(!cfg.mesh.enabled);
        assert!(!cfg.web.enabled);
        assert!(cfg.clipboard.serve);
        assert!(cfg.lua.watch_dirs.is_empty());
        assert!(cfg.lua.startup_scripts.is_empty());
    }

    #[test]
    fn parse_partial_toml() {
        let toml = r#"
[mesh]
enabled = true
node_name = "testbox"

[clipboard]
serve = false
"#;
        let cfg: DaemonConfig = toml_cfg::from_str(toml).unwrap();
        assert!(cfg.mesh.enabled);
        assert_eq!(cfg.mesh.node_name, "testbox");
        assert!(!cfg.clipboard.serve);
        // Unspecified sections get defaults
        assert!(!cfg.web.enabled);
        assert!(cfg.daemon.socket.contains("cosmix.sock"));
    }

    #[test]
    fn parse_empty_toml() {
        let cfg: DaemonConfig = toml_cfg::from_str("").unwrap();
        assert!(!cfg.mesh.enabled);
        assert!(cfg.clipboard.serve);
    }

    #[test]
    fn parse_mesh_with_peers() {
        let toml = r#"
[mesh]
enabled = true
node_name = "cachyos"
listen_port = 9800
wg_ip = "172.16.2.5"

[mesh.peers.mko]
wg_ip = "172.16.2.210"
port = 9800
"#;
        let cfg: DaemonConfig = toml_cfg::from_str(toml).unwrap();
        assert!(cfg.mesh.enabled);
        assert_eq!(cfg.mesh.node_name, "cachyos");
        assert_eq!(cfg.mesh.listen_port, 9800);
        assert_eq!(cfg.mesh.wg_ip, "172.16.2.5");
        assert_eq!(cfg.mesh.peers.len(), 1);
        assert_eq!(cfg.mesh.peers["mko"].wg_ip, "172.16.2.210");
        assert_eq!(cfg.mesh.peers["mko"].port, 9800);
    }
}
