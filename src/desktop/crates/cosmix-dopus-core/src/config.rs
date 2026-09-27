//! Native `.conf.mix` persistence for dopus's local UI/session state.
//!
//! Ported from src/desktop/apps/filemgr/src/config.rs (Bevy/ctk); filemgr
//! stays untouched until retirement. Schema 2 adds plain sidebar state and
//! migrates dopus schema 1. The rejection law is kept: a malformed or
//! unsupported-schema file loads defaults and is never
//! overwritten. Runtime directories are injected by the caller (no ctk
//! AppDirs); the file name stays `config.conf.mix`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use cosmix_config::{from_conf_mix_str, to_conf_mix_string};
use cosmix_files::atomic::write_atomic;

pub const CURRENT_SCHEMA: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub open: bool,
    /// Fraction of available window width, matching filemgr's default.
    pub width: f32,
}
impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            open: true,
            width: 0.15,
        }
    }
}
impl SidebarConfig {
    pub fn normalised(self) -> Self {
        Self {
            width: if self.width.is_finite() {
                self.width.clamp(0.1, 0.3)
            } else {
                Self::default().width
            },
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sidebar {
    Places,
    Properties,
}

impl Sidebar {
    pub fn default_config(self) -> SidebarConfig {
        SidebarConfig {
            width: match self {
                Self::Places => 0.15,
                Self::Properties => 0.22,
            },
            ..SidebarConfig::default()
        }
    }
}

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
    pub places: SidebarConfig,
    #[serde(deserialize_with = "deserialize_properties")]
    pub properties: SidebarConfig,
    pub schema_version: u32,
    pub left: PaneConfig,
    #[serde(default = "default_right_pane")]
    pub right: PaneConfig,
    pub active_pane: String,
    #[serde(default = "default_split_ratio")]
    pub split_ratio: f32,
}

fn deserialize_properties<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SidebarConfig, D::Error> {
    #[derive(Deserialize)]
    struct Partial {
        open: Option<bool>,
        width: Option<f32>,
    }
    let partial = Partial::deserialize(deserializer)?;
    let default = Sidebar::Properties.default_config();
    Ok(SidebarConfig {
        open: partial.open.unwrap_or(default.open),
        width: partial.width.unwrap_or(default.width),
    })
}

impl Default for DOpusConfig {
    fn default() -> Self {
        let left = PaneConfig::default();
        Self {
            schema_version: CURRENT_SCHEMA,
            places: SidebarConfig::default(),
            properties: Sidebar::Properties.default_config(),
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
                Ok(mut config) if matches!(config.schema_version, 1 | CURRENT_SCHEMA) => {
                    if config.schema_version == 1 {
                        config.places = SidebarConfig::default();
                        config.properties = Sidebar::Properties.default_config();
                    }
                    config.schema_version = CURRENT_SCHEMA;
                    config.places = config.places.normalised();
                    config.properties = config.properties.normalised();
                    (
                        config,
                        Self {
                            path,
                            allow_save: true,
                        },
                    )
                }
                // Unknown schemas load defaults without overwriting the file.
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

    /// Persist the config. Returns `Ok(false)` — not an error — when the
    /// poison pill refuses the write (malformed/foreign-schema/unreadable
    /// file at load): the caller must not report the config as saved.
    pub fn save(&self, config: &DOpusConfig) -> Result<bool, String> {
        if !self.allow_save {
            return Ok(false);
        }
        let content = to_conf_mix_string(config)
            .map_err(|error| format!("serialising dopus config: {error}"))?;
        // `write_atomic` returns the typed cosmix-files error; this layer
        // speaks `String` (filemgr's convention, kept throughout the core).
        write_atomic(&self.path, content.as_bytes())
            .map(|()| true)
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
    fn partial_schema_two_panels_keep_sidebar_specific_defaults_and_explicit_widths() {
        let dir = tempfile::tempdir().unwrap();
        for (record, open, width) in [
            ("{}", true, 0.22),
            ("{open: false}", false, 0.22),
            ("{width: 0.27}", true, 0.27),
            ("{open: false, width: 0.18}", false, 0.18),
        ] {
            std::fs::write(
                dir.path().join("config.conf.mix"),
                format!("schema_version: 2\nplaces: {{open: false}}\nproperties: {record}\n"),
            )
            .unwrap();
            let (config, file) = ConfigFile::load(dir.path());
            assert!(file.allow_save, "valid partial record: {record}");
            assert_eq!(config.properties, SidebarConfig { open, width });
            assert_eq!(
                config.places,
                SidebarConfig {
                    open: false,
                    width: 0.15
                }
            );
            file.save(&config).unwrap();
            assert_eq!(ConfigFile::load(dir.path()).0, config);
        }
    }

    #[test]
    fn schema_one_migrates_without_losing_pane_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.conf.mix");
        let raw = "schema_version: 1\nleft: {path: \"/tmp/source\", show_hidden: true, sort: \"modified\", ascending: false}\nright: {path: \"/tmp/target\", sort: \"size\"}\nactive_pane: \"right\"\nsplit_ratio: 0.7\n";
        std::fs::write(&path, raw).unwrap();
        let (config, file) = ConfigFile::load(dir.path());
        assert!(file.allow_save);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            raw,
            "load does not write"
        );
        assert_eq!(config.schema_version, 2);
        assert_eq!(config.left.path, Path::new("/tmp/source"));
        assert!(config.left.show_hidden);
        assert_eq!(config.left.sort, SortColumn::Modified);
        assert!(!config.left.ascending);
        assert_eq!(config.right.path, Path::new("/tmp/target"));
        assert_eq!(config.right.sort, SortColumn::Size);
        assert_eq!(config.active_pane, "right");
        assert_eq!(config.split_ratio, 0.7);
        assert_eq!(config.places, SidebarConfig::default());
        assert_eq!(config.properties, Sidebar::Properties.default_config());
        file.save(&config).unwrap();
        assert_eq!(ConfigFile::load(dir.path()).0, config);
    }

    #[test]
    fn schema_two_preserves_hidden_panels_and_clamps_widths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.conf.mix"), "schema_version: 2\nplaces: {open: false, width: 0.24}\nproperties: {open: false, width: 9.0}\n").unwrap();
        let (config, file) = ConfigFile::load(dir.path());
        assert!(file.allow_save);
        assert_eq!(
            config.places,
            SidebarConfig {
                open: false,
                width: 0.24
            }
        );
        assert_eq!(
            config.properties,
            SidebarConfig {
                open: false,
                width: 0.3
            }
        );
        assert_eq!(
            SidebarConfig {
                width: f32::NAN,
                ..Default::default()
            }
            .normalised()
            .width,
            0.15
        );
    }

    #[test]
    fn malformed_config_remains_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.conf.mix");
        let raw = "schema_version: 2\nplaces: {open: \"invalid\"}\n";
        std::fs::write(&path, raw).unwrap();
        let (_, file) = ConfigFile::load(dir.path());
        assert!(!file.allow_save);
        assert!(!file.save(&DOpusConfig::default()).unwrap());
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    }

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
