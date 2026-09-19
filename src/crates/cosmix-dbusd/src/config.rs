//! `dbusd.conf.mix` — which adapters are enabled.

#[cfg(feature = "cosmix")]
use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Typed `~/.config/cosmix/dbusd.conf.mix`. A missing file materialises
/// this default (the store does that for us). `enabled: absent` means
/// every built-in adapter; `enabled: []` explicitly means none; a list
/// names the adapters to run, in any order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DbusdSettings {
    pub enabled: Option<Vec<String>>,
}

/// Pure core of the config decision: the configured list against the
/// built-in registry. Returns the enabled names plus any configured
/// names that are not built (the loader logs them; a typo must be loud
/// but must not stop the daemon or silently disable a working adapter).
pub fn resolve_enabled(
    configured: Option<&[String]>,
    builtin: &[String],
) -> (Vec<String>, Vec<String>) {
    match configured {
        None => (builtin.to_vec(), Vec::new()),
        Some(list) => {
            let mut enabled = Vec::new();
            let mut unknown = Vec::new();
            for name in list {
                if builtin.iter().any(|builtin| builtin == name) {
                    enabled.push(name.clone());
                } else {
                    unknown.push(name.clone());
                }
            }
            (enabled, unknown)
        }
    }
}

/// Load `dbusd.conf.mix` per the store contract (same as every other
/// daemon, e.g. indexd): an absent file materialises the defaults —
/// only NotFound does; a file that exists but cannot be read or parsed
/// is FATAL, surfaced as `Err` so `serve()` exits instead of silently
/// guessing the adapter set. Thin wrapper over the resolver's config
/// dir.
#[cfg(feature = "cosmix")]
pub fn load_settings() -> anyhow::Result<DbusdSettings> {
    load_settings_in(&cosmix_config::store::config_dir())
}

/// Directory-explicit core of [`load_settings`] (the store's own
/// `load_service_in`): same contract, against an explicit dir. The
/// test harness drives this against a throwaway dir rather than
/// mutating `COSMIX_ETC` process-globally — the path resolver caches
/// that in a `OnceLock`, so a set_var in one test would pin every
/// sibling test's config dir too.
#[cfg(feature = "cosmix")]
pub fn load_settings_in(dir: &std::path::Path) -> anyhow::Result<DbusdSettings> {
    cosmix_config::store::load_service_in::<DbusdSettings>(dir, "dbusd")
        .context("dbusd.conf.mix exists but cannot be loaded; refusing to guess the adapter set")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn absent_config_enables_every_builtin() {
        let builtin = names(&["notify", "tray"]);
        let (enabled, unknown) = resolve_enabled(None, &builtin);
        assert_eq!(enabled, builtin);
        assert!(unknown.is_empty());
    }

    #[test]
    fn explicit_list_selects_a_subset_in_registry_order() {
        let builtin = names(&["notify", "tray", "settings"]);
        let configured = names(&["tray", "notify"]);
        let (enabled, unknown) = resolve_enabled(Some(&configured), &builtin);
        assert_eq!(enabled, names(&["tray", "notify"]));
        assert!(unknown.is_empty());
    }

    #[test]
    fn empty_list_enables_nothing() {
        let builtin = names(&["notify"]);
        let configured: Vec<String> = Vec::new();
        let (enabled, unknown) = resolve_enabled(Some(&configured), &builtin);
        assert!(enabled.is_empty());
        assert!(unknown.is_empty());
    }

    #[test]
    fn unknown_names_are_reported_not_ignored_silently() {
        let builtin = names(&["notify"]);
        let configured = names(&["notify", "nootify"]);
        let (enabled, unknown) = resolve_enabled(Some(&configured), &builtin);
        assert_eq!(enabled, names(&["notify"]));
        assert_eq!(unknown, names(&["nootify"]));
    }

    #[test]
    fn default_settings_enable_all_builtins() {
        assert_eq!(DbusdSettings::default().enabled, None);
    }

    /// F4, the store contract from the loader's side: NotFound
    /// materialises defaults; a file that exists but cannot be parsed
    /// is fatal. Driven against a throwaway config dir via
    /// `load_settings_in` — no `COSMIX_ETC` mutation (the resolver
    /// caches it in a `OnceLock`, poisoning sibling tests).
    #[cfg(feature = "cosmix")]
    #[test]
    fn missing_config_is_defaults_broken_config_is_fatal() {
        let dir = std::env::temp_dir().join(format!("cosmix-dbusd-conf-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("temp config dir");

        // NotFound: defaults, materialised to disk so they are
        // discoverable by `cat`.
        let settings = load_settings_in(&dir).expect("absent config must load defaults");
        assert_eq!(settings, DbusdSettings::default());
        assert!(
            dir.join("dbusd.conf.mix").exists(),
            "absent config materialises defaults"
        );

        // Parse error: fatal, naming the file.
        std::fs::write(dir.join("dbusd.conf.mix"), "enabled = [ unclosed").expect("break config");
        let error = load_settings_in(&dir).expect_err("broken config must be fatal");
        assert!(
            format!("{error:#}").contains("dbusd.conf.mix"),
            "the fatal error names the file: {error:#}"
        );

        // And a valid explicit list still parses through the same path.
        std::fs::write(dir.join("dbusd.conf.mix"), "enabled: [\"notify\"]").expect("fix config");
        let settings = load_settings_in(&dir).expect("valid config must load");
        assert_eq!(settings.enabled, Some(names(&["notify"])));

        std::fs::remove_dir_all(&dir).ok();
    }
}
