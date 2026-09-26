//! Session config: the core's `ConfigFile` pointed at dopus's config
//! directory. The file, schema and poison-pill law all live in the core
//! (`cosmix_dopus_core::config`); this module only resolves the directory and
//! renders the resolved config for `--print-config`.

use std::path::Path;

use cosmix_dopus_core::{ConfigFile, DOpusConfig};

/// Load `config.conf.mix` from `dir` (the core handles missing files,
/// malformed files and foreign schemas). Without a directory there is
/// nothing to load and nothing to save into. The directory is created when
/// missing: `write_atomic` does not create parents (cosmix-lib-files'
/// contract), so an uncreated dir would otherwise turn every settle into a
/// silent ENOENT — the write failure surfaces per-save as a Status line.
pub fn load(dir: Option<&Path>) -> (DOpusConfig, Option<ConfigFile>) {
    match dir {
        Some(dir) => {
            if let Err(error) = std::fs::create_dir_all(dir) {
                eprintln!("cosmix-dopus: cannot create config dir {}: {error}", dir.display());
            }
            let (mut config, file) = ConfigFile::load(dir);
            // A stale or hand-edited ratio clamps at load to the divider
            // drag contract: the view clamps at render too, but the app
            // caches the raw value at boot and `dopus.state` reports it.
            config.split_ratio =
                config.split_ratio.clamp(crate::view::panes::SPLIT_MIN, crate::view::panes::SPLIT_MAX);
            (config, Some(file))
        }
        None => (DOpusConfig::default(), None),
    }
}

/// The resolved configuration as pretty JSON (`--print-config`).
pub fn to_json(config: &DOpusConfig) -> String {
    serde_json::to_string_pretty(config).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_directory_is_defaults_without_a_file() {
        let (config, file) = load(None);
        assert_eq!(config, DOpusConfig::default());
        assert!(file.is_none());
    }

    #[test]
    fn missing_file_is_defaults_with_a_writable_target() {
        let dir = tempfile::tempdir().unwrap();
        let (config, file) = load(Some(dir.path()));
        assert_eq!(config, DOpusConfig::default());
        let file = file.unwrap();
        assert!(file.allow_save);
        assert!(file.path.ends_with("config.conf.mix"));
    }

    #[test]
    fn boot_split_ratio_clamps_to_the_drag_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.conf.mix");
        std::fs::write(&path, b"schema_version: 1\nsplit_ratio: 5.0\n").unwrap();
        let (config, _) = load(Some(dir.path()));
        assert_eq!(config.split_ratio, crate::view::panes::SPLIT_MAX);
        std::fs::write(&path, b"schema_version: 1\nsplit_ratio: 0.01\n").unwrap();
        let (config, _) = load(Some(dir.path()));
        assert_eq!(config.split_ratio, crate::view::panes::SPLIT_MIN);
    }
}
