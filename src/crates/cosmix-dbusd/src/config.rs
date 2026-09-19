//! `dbusd.conf.mix` — which adapters are enabled.

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

/// Load `dbusd.conf.mix`. Read/parse problems are loudly reported and
/// answered with the default (all built-ins): a broken config file must
/// not take the boundary daemon down — that is the same containment
/// law the supervisor applies to adapters.
#[cfg(feature = "cosmix")]
pub fn load_settings() -> DbusdSettings {
    match cosmix_config::store::load_service::<DbusdSettings>("dbusd") {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "cosmix-dbusd: reading dbusd.conf.mix failed; using defaults \
                 (all built-in adapters): {error:#}"
            );
            DbusdSettings::default()
        }
    }
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
}
