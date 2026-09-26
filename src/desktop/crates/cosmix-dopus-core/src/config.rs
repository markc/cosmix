//! Native `.conf.mix` persistence for dopus's local UI/session state.
//!
//! Ported from src/desktop/apps/filemgr/src/config.rs (Bevy/ctk); filemgr
//! stays untouched until retirement. Schema is fresh at 1: the v1→v2 sidebar
//! migration is dropped (dopus has no DCS sidebars) but the rejection law is
//! kept — a malformed or unsupported-schema file loads defaults and is never
//! overwritten. Runtime directories are injected by the caller (no ctk
//! AppDirs); the file name stays `config.conf.mix`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use cosmix_config::{from_conf_mix_str, to_conf_mix_string};
use cosmix_files::atomic::write_atomic;

pub const CURRENT_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SortColumn {
    #[default]
    Name,
    Size,
    Modified,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct PaneConfig {
    pub path: PathBuf,
    pub show_hidden: bool,
    pub sort: SortColumn,
    pub ascending: bool,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            path: default_home(),
            show_hidden: false,
            sort: SortColumn::Name,
            ascending: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DOpusConfig {
    pub schema_version: u32,
    pub left: PaneConfig,
    #[serde(default = "default_right_pane")]
    pub right: PaneConfig,
    pub active_pane: String,
    #[serde(default = "default_split_ratio")]
    pub split_ratio: f32,
}

impl Default for DOpusConfig {
    fn default() -> Self {
        let left = PaneConfig::default();
        Self {
            schema_version: CURRENT_SCHEMA,
            left,
            right: default_right_pane(),
            active_pane: "left".into(),
            split_ratio: default_split_ratio(),
        }
    }
}

fn default_split_ratio() -> f32 {
    0.5
}

fn default_right_pane() -> PaneConfig {
    let mut pane = PaneConfig::default();
    let downloads = default_home().join("Downloads");
    if downloads.is_dir() {
        pane.path = downloads;
    }
    pane
}

/// Persistence target plus protection against overwriting malformed config
/// (the poison-pill law, filemgr config.rs:132-136).
pub struct ConfigFile {
    pub path: PathBuf,
    pub allow_save: bool,
}

impl ConfigFile {
    /// Load `config.conf.mix` from the given config directory. Unlike filemgr,
    /// the directory is injected: the core has no AppDirs and no environment
    /// assumptions beyond what the caller hands it.
    pub fn load(dir: &Path) -> (DOpusConfig, Self) {
        let path = dir.join("config.conf.mix");
        match std::fs::read_to_string(&path) {
            Ok(raw) => match from_conf_mix_str::<DOpusConfig>(&raw) {
                Ok(config) if config.schema_version == CURRENT_SCHEMA => (
                    config,
                    Self {
                        path,
                        allow_save: true,
                    },
                ),
                // No migration: schema is fresh at 1. Anything else on disk was
                // written by something this build does not understand, so the
                // defaults load but the file is never overwritten.
                Ok(config) => {
                    eprintln!(
                        "dopus: refusing to overwrite unsupported config schema {} in {}",
                        config.schema_version,
                        path.display()
                    );
                    (
                        DOpusConfig::default(),
                        Self {
                            path,
                            allow_save: false,
                        },
                    )
                }
                Err(error) => {
                    eprintln!(
                        "dopus: refusing to overwrite invalid config {}: {error}",
                        path.display()
                    );
                    (
                        DOpusConfig::default(),
                        Self {
                            path,
                            allow_save: false,
                        },
                    )
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                DOpusConfig::default(),
                Self {
                    path,
                    allow_save: true,
                },
            ),
            Err(error) => {
                eprintln!("dopus: cannot read config {}: {error}", path.display());
                (
                    DOpusConfig::default(),
                    Self {
                        path,
                        allow_save: false,
                    },
                )
            }
        }
    }

    pub fn save(&self, config: &DOpusConfig) -> Result<(), String> {
        if !self.allow_save {
            return Ok(());
        }
        let content =
            to_conf_mix_string(config).map_err(|error| format!("serialising dopus config: {error}"))?;
        // `write_atomic` returns the typed cosmix-files error; this layer
        // speaks `String` (filemgr's convention, kept throughout the core).
        write_atomic(&self.path, content.as_bytes())
            .map_err(|error| format!("writing {}: {error}", self.path.display()))
    }
}

fn default_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_native_mix_data() {
        let config = DOpusConfig::default();
        let raw = to_conf_mix_string(&config).unwrap();
        let reparsed: DOpusConfig = from_conf_mix_str(&raw).unwrap();
        assert_eq!(reparsed, config);
    }

    #[test]
    fn executable_config_is_rejected() {
        let raw = "schema_version: $executable\n";
        assert!(from_conf_mix_str::<DOpusConfig>(raw).is_err());
    }

    #[test]
    fn atomic_save_round_trips_without_leaving_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let file = ConfigFile {
            path: directory.path().join("dopus.conf.mix"),
            allow_save: true,
        };
        let config = DOpusConfig::default();
        file.save(&config).unwrap();
        let raw = std::fs::read_to_string(&file.path).unwrap();
        let reparsed: DOpusConfig = from_conf_mix_str(&raw).unwrap();
        assert_eq!(reparsed, config);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn unsupported_schema_loads_defaults_and_is_never_overwritten() {
        // The poison-pill law (filemgr config.rs:167-180), pinned at schema 1:
        // a future schema must survive an older binary untouched.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.conf.mix");
        std::fs::write(&path, b"schema_version: 99\n").unwrap();
        let before = std::fs::read(&path).unwrap();

        let (config, file) = ConfigFile::load(directory.path());

        assert_eq!(config, DOpusConfig::default());
        assert!(!file.allow_save);
        // Saves are silently refused — the on-disk bytes never change.
        file.save(&DOpusConfig::default()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn missing_config_file_loads_defaults_and_allows_saving() {
        let directory = tempfile::tempdir().unwrap();
        let (config, file) = ConfigFile::load(directory.path());
        assert_eq!(config, DOpusConfig::default());
        assert!(file.allow_save);
        assert_eq!(file.path, directory.path().join("config.conf.mix"));
    }
}
