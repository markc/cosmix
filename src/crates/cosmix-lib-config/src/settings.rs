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
    pub llm: LlmSettings,
    pub skills: SkillsSettings,
    pub knowledge: KnowledgeSettings,
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
    /// Path to the sqlite-vec database file.
    pub vectors_db: String,
    /// HuggingFace model ID for embeddings.
    pub model_id: String,
    /// Unix socket path for the indexd daemon.
    pub socket_path: String,
    /// Seconds before unloading the model from memory when idle.
    pub idle_timeout_secs: u64,
    /// Model precision: "f16" or "f32".
    pub dtype: String,
}

impl Default for EmbedSettings {
    fn default() -> Self {
        Self {
            vectors_db: "/var/lib/cosmix/vectors.db".into(),
            model_id: "nomic-ai/nomic-embed-text-v1.5".into(),
            socket_path: "/run/cosmix/embed.sock".into(),
            idle_timeout_secs: 60,
            dtype: "f16".into(),
        }
    }
}

/// LLM backend configuration — supports multiple named backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmSettings {
    /// Which backend to use by default (key into `backends` table).
    pub default: String,
    /// Named backend configurations.
    pub backends: std::collections::BTreeMap<String, LlmBackendConfig>,
}

impl Default for LlmSettings {
    fn default() -> Self {
        let mut backends = std::collections::BTreeMap::new();
        backends.insert("ollama".into(), LlmBackendConfig {
            provider: "ollama".into(),
            model: "qwen3:30b-a3b-nt".into(),
            base_url: "http://localhost:11434".into(),
            api_key_env: String::new(),
            api_key_cmd: String::new(),
            port: String::new(),
            command: String::new(),
        });
        backends.insert("claude-api".into(), LlmBackendConfig {
            provider: "anthropic".into(),
            model: "claude-haiku-4-5-20251001".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            api_key_cmd: String::new(),
            port: String::new(),
            command: String::new(),
        });
        backends.insert("claud".into(), LlmBackendConfig {
            provider: "amp".into(),
            model: String::new(),
            base_url: String::new(),
            api_key_env: String::new(),
            api_key_cmd: String::new(),
            port: "claud".into(),
            command: "ask".into(),
        });
        Self {
            default: "claude-api".into(),
            backends,
        }
    }
}

/// Configuration for a single LLM backend.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LlmBackendConfig {
    /// Provider type: "anthropic", "openai", "ollama", "amp".
    pub provider: String,
    /// Model identifier (e.g. "claude-haiku-4-5-20251001", "gpt-4o-mini", "qwen3:30b-a3b-nt").
    pub model: String,
    /// Base URL for HTTP-based providers.
    pub base_url: String,
    /// Environment variable name containing the API key.
    pub api_key_env: String,
    /// Shell command that outputs the API key (alternative to env var).
    pub api_key_cmd: String,
    /// AMP port name (for "amp" provider only).
    pub port: String,
    /// AMP command name (for "amp" provider, default "ask").
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsSettings {
    /// Minimum confidence threshold for skill retrieval (0.0–1.0).
    pub min_confidence: f64,
    /// Maximum skills injected into agent prompts.
    pub max_skills: u32,
    /// LLM backend name to use (key in [llm.backends], empty = use [llm].default).
    pub llm_backend: String,
    /// Minimum confidence for skill graduation to CLAUDE.md (0.0–1.0).
    pub graduation_confidence: f64,
    /// Minimum use count for skill graduation.
    pub graduation_min_uses: u32,
    /// Minimum success count for skill graduation.
    pub graduation_min_successes: u32,
}

impl Default for SkillsSettings {
    fn default() -> Self {
        Self {
            min_confidence: 0.3,
            max_skills: 3,
            llm_backend: String::new(),
            graduation_confidence: 0.9,
            graduation_min_uses: 5,
            graduation_min_successes: 4,
        }
    }
}

/// Knowledge base ranking: trust weights per source type + temporal decay.
/// Applied in MCP context_search after indexd returns raw vector-similarity results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeSettings {
    /// Distance bonus for `_doc/` chunks (curated design truth). Subtracted from distance.
    pub trust_weight_doc: f64,
    /// Distance bonus for skills that have graduated to CLAUDE.md.
    pub trust_weight_graduated: f64,
    /// Distance bonus for ungraduated (provisional) skills.
    pub trust_weight_skill: f64,
    /// Distance bonus for journal entries (operational residue, usually 0.0).
    pub trust_weight_journal: f64,
    /// Distance bonus for _memory/ chunks (CMM-generated observations).
    pub trust_weight_memory: f64,
    /// Journal decay: distance penalty per month of age.
    pub journal_decay_per_month: f64,
    /// Journals older than this many days are excluded from results entirely.
    pub journal_max_age_days: u32,
}

impl Default for KnowledgeSettings {
    fn default() -> Self {
        Self {
            trust_weight_doc: 0.08,
            trust_weight_graduated: 0.06,
            trust_weight_skill: 0.03,
            trust_weight_journal: 0.0,
            trust_weight_memory: 0.01,
            journal_decay_per_month: 0.02,
            journal_max_age_days: 180,
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
    pub scripts_dir: String,
    pub editor: String,
}

impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            scripts_dir: "~/.mix/src/scripts:~/.local/share/mix/scripts".into(),
            editor: "cosmix-edit".into(),
        }
    }
}
