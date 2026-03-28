//! Typed configuration structs — one section per app/service.
//!
//! All structs derive `Default` with values matching what apps currently
//! hardcode, so a fresh `settings.toml` is immediately usable.

use serde::{Deserialize, Serialize};

/// Master settings struct — maps to the top-level TOML file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CosmixSettings {
    pub global: GlobalSettings,
    pub hub: HubSettings,
    pub web: WebSettings,
    pub mail: MailSettings,
    pub mon: MonSettings,
    pub edit: EditSettings,
    pub files: FilesSettings,
    pub view: ViewSettings,
    pub dns: DnsSettings,
    pub wg: WgSettings,
    pub backup: BackupSettings,
    pub embed: EmbedSettings,
    pub mesh: MeshSettings,
    pub launcher: LauncherSettings,
}

/// Settings that apply to all cosmix GUI apps.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    /// Base font size in pixels for all app UI text (default: 14).
    pub font_size: u16,
    /// OKLCH hue angle 0–360 for the colour theme (default: 220.0 = Ocean).
    pub theme_hue: f32,
    /// Dark mode (true) or light mode (false).
    pub theme_dark: bool,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            font_size: 16,
            theme_hue: 220.0,
            theme_dark: true,
        }
    }
}

/// Named theme presets — returns the hue angle for a preset name.
pub fn preset_hue(name: &str) -> f32 {
    match name {
        "ocean" => 220.0,
        "crimson" => 25.0,
        "stone" => 60.0,
        "forest" => 150.0,
        "sunset" => 45.0,
        _ => 220.0,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HubSettings {
    pub port: u16,
    pub node: String,
    pub ws_url: String,
}

impl Default for HubSettings {
    fn default() -> Self {
        Self {
            port: 4200,
            node: "localhost".into(),
            ws_url: "ws://localhost:4200/ws".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSettings {
    pub listen: String,
    pub jmap_upstream: String,
    pub www_dir: String,
    pub hub_ws: String,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            jmap_upstream: "http://127.0.0.1:8080".into(),
            www_dir: "/var/lib/cosmix/www".into(),
            hub_ws: "ws://localhost:4200/ws".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MailSettings {
    pub jmap_url: String,
    pub jmap_user: String,
    pub jmap_password: String,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for MailSettings {
    fn default() -> Self {
        Self {
            jmap_url: String::new(),
            jmap_user: String::new(),
            jmap_password: String::new(),
            window_width: 1400,
            window_height: 900,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonSettings {
    pub refresh_interval_secs: u64,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for MonSettings {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 5,
            window_width: 720,
            window_height: 520,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EditSettings {
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for EditSettings {
    fn default() -> Self {
        Self {
            window_width: 800,
            window_height: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesSettings {
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for FilesSettings {
    fn default() -> Self {
        Self {
            window_width: 900,
            window_height: 640,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            window_width: 960,
            window_height: 800,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsSettings {
    pub refresh_interval_secs: u64,
    pub window_width: u32,
    pub window_height: u32,
    pub zone_dir: String,
}

impl Default for DnsSettings {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 10,
            window_width: 960,
            window_height: 640,
            zone_dir: "/var/lib/hickory".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WgSettings {
    pub refresh_interval_secs: u64,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for WgSettings {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 10,
            window_width: 900,
            window_height: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupSettings {
    pub pbs_api_url: String,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            pbs_api_url: "https://localhost:8007".into(),
            window_width: 960,
            window_height: 640,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedSettings {
    pub vectors_db: String,
}

impl Default for EmbedSettings {
    fn default() -> Self {
        Self {
            vectors_db: "/var/lib/cosmix/vectors.db".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshSettings {
    pub peer_timeout_secs: u64,
}

impl Default for MeshSettings {
    fn default() -> Self {
        Self {
            peer_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherSettings {
    pub lua_scripts_dir: String,
    pub editor: String,
}

impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            lua_scripts_dir: "~/.local/lua".into(),
            editor: "cosmix-edit".into(),
        }
    }
}
