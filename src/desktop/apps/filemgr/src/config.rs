//! Native `.conf.mix` persistence for FileMgr's local UI/session state.

use std::path::PathBuf;

use ctk::app_dirs::AppDirs;
use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA: u32 = 2;

/// FileMgr's runtime directories, keyed on the stable component slug
/// (display renames never move data). `filemgr` is a valid slug, so the
/// only way this fails is a broken environment with no $HOME/$XDG_STATE_HOME —
/// in which we refuse (clean exit) rather than persist to an unsafe or
/// launch-relative directory.
pub fn app_dirs() -> AppDirs {
    AppDirs::resolve(crate::IDENTITY.slug).unwrap_or_else(|| {
        eprintln!(
            "filemgr: cannot determine a runtime directory — set $HOME, \
             $XDG_STATE_HOME, or $COSMIX_APP_HOME"
        );
        std::process::exit(1);
    })
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
    pub path: String,
    pub show_hidden: bool,
    pub sort: SortColumn,
    pub ascending: bool,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            path: default_home().to_string_lossy().into_owned(),
            show_hidden: false,
            sort: SortColumn::Name,
            ascending: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub open: bool,
    pub pinned: bool,
    pub width: f32,
    pub active_panel: String,
}

impl SidebarConfig {
    fn left_default() -> Self {
        Self {
            open: true,
            pinned: true,
            width: 0.15,
            active_panel: "places".into(),
        }
    }

    fn right_default() -> Self {
        Self {
            open: true,
            pinned: true,
            width: 0.15,
            active_panel: "information".into(),
        }
    }
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self::left_default()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct FileMgrConfig {
    pub schema_version: u32,
    pub left: PaneConfig,
    #[serde(default = "default_right_pane")]
    pub right: PaneConfig,
    pub active_pane: String,
    #[serde(default = "default_split_ratio")]
    pub split_ratio: f32,
    pub left_sidebar: SidebarConfig,
    #[serde(default = "SidebarConfig::right_default")]
    pub right_sidebar: SidebarConfig,
}

impl Default for FileMgrConfig {
    fn default() -> Self {
        let left = PaneConfig::default();
        Self {
            schema_version: CURRENT_SCHEMA,
            left,
            right: default_right_pane(),
            active_pane: "left".into(),
            split_ratio: default_split_ratio(),
            left_sidebar: SidebarConfig::left_default(),
            right_sidebar: SidebarConfig::right_default(),
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
        pane.path = downloads.to_string_lossy().into_owned();
    }
    pane
}

/// Persistence target plus protection against overwriting malformed config.
pub struct ConfigFile {
    pub path: PathBuf,
    pub allow_save: bool,
}

impl ConfigFile {
    pub fn load(dirs: &AppDirs) -> (FileMgrConfig, Self) {
        let path = dirs.config().join("config.conf.mix");
        // Pre-XDG discovery paths (the retired dotdir and the shared
        // `<name>.conf.mix` fallback) are gone: no file was ever written to
        // either under any name, so probing them was a silent no-op
        // masquerading as a migration. The XDG per-app root is the only
        // config location.
        match std::fs::read_to_string(&path) {
            Ok(raw) => match cosmix_config::from_conf_mix_str::<FileMgrConfig>(&raw) {
                Ok(config) if config.schema_version == CURRENT_SCHEMA => (
                    config,
                    Self {
                        path,
                        allow_save: true,
                    },
                ),
                Ok(mut config) if config.schema_version == 1 => {
                    config.schema_version = CURRENT_SCHEMA;
                    config.left_sidebar.width = 0.15;
                    config.right_sidebar.width = 0.15;
                    (
                        config,
                        Self {
                            path,
                            allow_save: true,
                        },
                    )
                }
                Ok(config) => {
                    eprintln!(
                        "filemgr: refusing to overwrite unsupported config schema {} in {}",
                        config.schema_version,
                        path.display()
                    );
                    (
                        FileMgrConfig::default(),
                        Self {
                            path,
                            allow_save: false,
                        },
                    )
                }
                Err(error) => {
                    eprintln!(
                        "filemgr: refusing to overwrite invalid config {}: {error}",
                        path.display()
                    );
                    (
                        FileMgrConfig::default(),
                        Self {
                            path,
                            allow_save: false,
                        },
                    )
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                FileMgrConfig::default(),
                Self {
                    path,
                    allow_save: true,
                },
            ),
            Err(error) => {
                eprintln!("filemgr: cannot read config {}: {error}", path.display());
                (
                    FileMgrConfig::default(),
                    Self {
                        path,
                        allow_save: false,
                    },
                )
            }
        }
    }

    pub fn save(&self, config: &FileMgrConfig) -> Result<(), String> {
        if !self.allow_save {
            return Ok(());
        }
        let content = cosmix_config::to_conf_mix_string(config)
            .map_err(|error| format!("serialising FileMgr config: {error}"))?;
        ctk::fs::write_atomic(&self.path, content.as_bytes())
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
        let config = FileMgrConfig::default();
        let raw = cosmix_config::to_conf_mix_string(&config).unwrap();
        let reparsed: FileMgrConfig = cosmix_config::from_conf_mix_str(&raw).unwrap();
        assert_eq!(reparsed, config);
    }

    #[test]
    fn executable_config_is_rejected() {
        let raw = "schema_version: $executable\n";
        assert!(cosmix_config::from_conf_mix_str::<FileMgrConfig>(raw).is_err());
    }

    #[test]
    fn atomic_save_round_trips_without_leaving_temporary_file() {
        let directory =
            std::env::temp_dir().join(format!("filemgr-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let file = ConfigFile {
            path: directory.join("filemgr.conf.mix"),
            allow_save: true,
        };
        let config = FileMgrConfig::default();
        file.save(&config).unwrap();
        let raw = std::fs::read_to_string(&file.path).unwrap();
        let reparsed: FileMgrConfig = cosmix_config::from_conf_mix_str(&raw).unwrap();
        assert_eq!(reparsed, config);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
