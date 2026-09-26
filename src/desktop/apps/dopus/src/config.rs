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
            let (config, file) = ConfigFile::load(dir);
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
}
