//! Typed configuration structs — one struct per service.
//!
//! Every struct derives `Default` with values matching what apps currently
//! hardcode, so a freshly-materialised `~/.config/cosmix/<service>.toml`
//! is immediately usable.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Path-prefix → domain name mapping.
///
/// Used by the knowledge base to map workspace paths to domain keys.
/// Entries are checked longest-prefix-first.
///
/// ```toml
/// [domains.map]
/// "~/Projects/cosmix" = "cosmix"
/// "~/.ns" = "ns"
/// "~/.mc" = "mc"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DomainsSettings {
    pub map: BTreeMap<String, String>,
}

impl Default for DomainsSettings {
    fn default() -> Self {
        let map = BTreeMap::from([
            ("~/Projects/cosmix".into(), "cosmix".into()),
            ("~/.ns".into(), "ns".into()),
            ("~/.mc".into(), "mc".into()),
        ]);
        Self { map }
    }
}

impl DomainsSettings {
    /// Resolve a filesystem path to a domain name via prefix matching.
    ///
    /// Expands `~` in map keys to `$HOME`. Returns `None` if no prefix matches.
    /// Longest prefix wins when multiple entries match.
    pub fn resolve(&self, path: &Path) -> Option<String> {
        let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
        let path_str = path.to_string_lossy();

        let mut best: Option<(usize, &str)> = None;
        for (prefix, domain) in &self.map {
            let expanded = if let Some(rest) = prefix.strip_prefix("~/") {
                format!("{}/{rest}", home.display())
            } else {
                prefix.clone()
            };
            if path_str.starts_with(&expanded) {
                let len = expanded.len();
                if best.is_none() || len > best.unwrap().0 {
                    best = Some((len, domain));
                }
            }
        }
        best.map(|(_, d)| d.to_string())
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
        backends.insert(
            "ollama".into(),
            LlmBackendConfig {
                provider: "ollama".into(),
                model: "qwen3:30b-a3b-nt".into(),
                base_url: "http://localhost:11434".into(),
                api_key_env: String::new(),
                api_key_cmd: String::new(),
                port: String::new(),
                command: String::new(),
            },
        );
        // Default: routes through the Claude Code CLI's OAuth session (MAX
        // plan) via `claude -p`. Zero incremental cost. Text-only — no tool
        // use, so agent loops must use `claude-api` or `ollama` instead.
        backends.insert(
            "claude-cli".into(),
            LlmBackendConfig {
                provider: "claude-cli".into(),
                model: "haiku".into(),
                base_url: String::new(),
                api_key_env: String::new(),
                api_key_cmd: String::new(),
                port: String::new(),
                command: String::new(),
            },
        );
        // Paid Anthropic Messages API. Required for tool-calling workloads
        // (cosmix-agentd). Keyed by COSMIX_ANTHROPIC_KEY to avoid collision
        // with ANTHROPIC_API_KEY, which Claude Code picks up and uses to
        // bypass the MAX subscription.
        backends.insert(
            "claude-api".into(),
            LlmBackendConfig {
                provider: "anthropic".into(),
                model: "claude-haiku-4-5-20251001".into(),
                base_url: "https://api.anthropic.com".into(),
                api_key_env: "COSMIX_ANTHROPIC_KEY".into(),
                api_key_cmd: String::new(),
                port: String::new(),
                command: String::new(),
            },
        );
        backends.insert(
            "claud".into(),
            LlmBackendConfig {
                provider: "bus".into(),
                model: String::new(),
                base_url: String::new(),
                api_key_env: String::new(),
                api_key_cmd: String::new(),
                port: "claud".into(),
                command: "ask".into(),
            },
        );
        Self {
            default: "claude-cli".into(),
            backends,
        }
    }
}

/// Configuration for a single LLM backend.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LlmBackendConfig {
    /// Provider type: "anthropic", "openai", "ollama", "bus".
    pub provider: String,
    /// Model identifier (e.g. "claude-haiku-4-5-20251001", "gpt-4o-mini", "qwen3:30b-a3b-nt").
    pub model: String,
    /// Base URL for HTTP-based providers.
    pub base_url: String,
    /// Environment variable name containing the API key.
    pub api_key_env: String,
    /// Shell command that outputs the API key (alternative to env var).
    pub api_key_cmd: String,
    /// Bus port name (for "bus" provider only).
    pub port: String,
    /// Bus command name (for "bus" provider, default "ask").
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsSettings {
    /// Minimum confidence threshold for skill retrieval (0.0–1.0).
    pub min_confidence: f64,
    /// Maximum skills injected into agent prompts.
    pub max_skills: u32,
    /// LLM backend name to use (key in `[llm.backends]`, empty = use `[llm].default`).
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
    /// Distance bonus for `_spec/` chunks (normative contracts, highest authority).
    pub trust_weight_spec: f64,
    /// Distance bonus for `_doc/` chunks (curated design truth). Subtracted from distance.
    pub trust_weight_doc: f64,
    /// Distance bonus for `_notes.md` files (persistent operational facts).
    pub trust_weight_notes: f64,
    /// Distance bonus for `_plan/` chunks (provisional implementation plans).
    pub trust_weight_plan: f64,
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
            trust_weight_spec: 0.10,
            trust_weight_doc: 0.08,
            trust_weight_notes: 0.06,
            trust_weight_plan: 0.04,
            trust_weight_graduated: 0.06,
            trust_weight_skill: 0.03,
            trust_weight_journal: 0.0,
            trust_weight_memory: 0.01,
            journal_decay_per_month: 0.02,
            journal_max_age_days: 180,
        }
    }
}

// ── Per-service settings (loaded via cosmix_config::store::load_service) ──

/// Policy for a single `source` value accepted by `cosmix-indexd`.
///
/// Declares which metadata fields must be present on incoming chunks
/// and, optionally, which of those fields must match YYYY-MM-DD.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceTypeSpec {
    /// Required metadata fields for this source type. Validation rejects
    /// any entry missing these.
    pub required: Vec<String>,
    /// Optional: metadata field whose value must be a valid YYYY-MM-DD
    /// date string. Rust retains the date-format check as a mechanism;
    /// TOML declares only *which* field to check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_field: Option<String>,
}

/// Settings for `cosmix-indexd`, loaded from `~/.config/cosmix/indexd.toml`.
///
/// Reference implementation of the per-service TOML convention described in
/// `_plan/cosmix-config-rebuild.md` §12 Step 2. Adding a new source type to
/// the validator is a TOML edit plus `systemctl restart cosmix-indexd` —
/// no rebuild required.
/// Runtime service parameters for `cosmix-indexd` — the `[service]` block
/// of `indexd.toml`. Clients that only need to reach the daemon (e.g. the
/// skills CLI) can load `IndexdSettings` and read this field alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexdServiceSettings {
    /// Path to the sqlite-vec database file.
    pub vectors_db: String,
    /// HuggingFace model ID for embeddings.
    pub model_id: String,
    /// Unix socket path for the indexd daemon.
    pub socket_path: String,
    /// Seconds before unloading the model from memory when idle.
    /// `0` means never unload (keep the model resident).
    pub idle_timeout_secs: u64,
    /// Model precision: "f16" or "f32".
    pub dtype: String,
}

impl Default for IndexdServiceSettings {
    fn default() -> Self {
        Self {
            vectors_db: "/var/lib/cosmix/vectors.db".into(),
            model_id: "nomic-ai/nomic-embed-text-v1.5".into(),
            socket_path: "/run/cosmix/indexd/embed.sock".into(),
            // 0 means never unload (see indexd watchdog); 1800s (30 min)
            // avoids reload thrash on brief idle gaps while still freeing
            // the ~522MB model on a genuinely idle daemon.
            idle_timeout_secs: 1800,
            dtype: "f16".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexdSettings {
    /// Schema version for future migrations.
    pub schema_version: u32,
    /// Runtime service parameters (DB path, socket path, model, dtype, idle timeout).
    pub service: IndexdServiceSettings,
    /// Map of `source` string → policy. Consulted by `validate_store_entry`
    /// in cosmix-indexd. An empty map is a configuration error; defaults
    /// include all source types the stack currently emits.
    pub source_types: BTreeMap<String, SourceTypeSpec>,
}

impl Default for IndexdSettings {
    fn default() -> Self {
        let st = |required: &[&str], date_field: Option<&str>| SourceTypeSpec {
            required: required.iter().map(|s| (*s).to_string()).collect(),
            date_field: date_field.map(String::from),
        };
        let mut source_types = BTreeMap::new();
        source_types.insert("skill".into(), st(&["name", "trigger", "approach"], None));
        source_types.insert("doc".into(), st(&["path", "domain"], None));
        source_types.insert(
            "journal".into(),
            st(&["path", "domain", "date"], Some("date")),
        );
        source_types.insert(
            "memory".into(),
            st(&["path", "domain", "date", "generator"], Some("date")),
        );
        source_types.insert("rust-doc".into(), st(&["file", "kind", "domain"], None));
        source_types.insert("mix-script".into(), st(&["file", "domain"], None));
        source_types.insert(
            "observation".into(),
            st(&["observer", "tool", "observed_at", "domain"], None),
        );
        source_types.insert("plan".into(), st(&["path", "domain"], None));
        source_types.insert("spec".into(), st(&["path", "domain"], None));
        source_types.insert("notes".into(), st(&["path", "domain"], None));
        Self {
            schema_version: 1,
            service: IndexdServiceSettings::default(),
            source_types,
        }
    }
}

/// Configuration for `cosmix-musicd` — the `musicd.conf.mix` surface.
///
/// The daemon fetches `soundfont_name` from `soundfont_url` into `state_dir`
/// on first run if absent, verifying it against `soundfont_sha256` (fail-closed).
/// The default is Frank Wen's FluidR3 GM+GS (MIT), pinned to its archive.org copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MusicdSettings {
    /// Schema version for future migrations.
    pub schema_version: u32,
    /// Daemon state directory — holds the SoundFont and rendered WAVs.
    pub state_dir: String,
    /// SoundFont filename within `state_dir`.
    pub soundfont_name: String,
    /// URL the SoundFont is fetched from on first run when absent.
    pub soundfont_url: String,
    /// SHA-256 (lowercase hex) the fetched SoundFont must match — fail-closed.
    pub soundfont_sha256: String,
    /// Default render / playback sample rate in Hz (rustysynth accepts 16k..192k).
    pub sample_rate: u32,
    /// Voice cap (rustysynth range 8..=256).
    pub max_polyphony: u32,
    /// Default output gain multiplier (lower if dense passages clip).
    pub gain: f32,
}

impl Default for MusicdSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            state_dir: "/var/lib/cosmix/musicd".into(),
            soundfont_name: "FluidR3_GM_GS.sf2".into(),
            soundfont_url: "https://archive.org/download/fluidr3-gm-gs/FluidR3_GM_GS.sf2".into(),
            soundfont_sha256: "545b2833936f15f04df5f0c5c4096b3ba6ced46ec7031f61991cae46f8681986"
                .into(),
            sample_rate: 44_100,
            max_polyphony: 64,
            gain: 1.0,
        }
    }
}
