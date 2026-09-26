//! Per-app directories, ctk's `AppDirs` convention — copied from ced's
//! `dirs.rs` (itself copied from `src/desktop/ctk/src/app_dirs.rs`, the
//! source of truth; keep in step) so dopus needs no ctk/Bevy dependency.
//!
//! Root resolution, first match wins, absolute values only:
//!   1. `$COSMIX_APP_HOME`
//!   2. `$COSMIX_APPS_HOME/<component>`
//!   3. `$XDG_STATE_HOME/cosmix/apps/<component>`
//!   4. `$HOME/.local/state/cosmix/apps/<component>`
//!
//! dopus's layout: `config/{config.conf.mix, theme.conf.mix, keymap.conf.mix}`,
//! `state/`, `cache/`.

use std::path::{Component, Path, PathBuf};

/// dopus's component slug (`desktop/APPS.md`).
pub const COMPONENT: &str = "dopus";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    root: PathBuf,
}

fn is_valid_component(component: &str) -> bool {
    if component.is_empty() || component == "." || component == ".." || component.contains(['/', '\\']) {
        return false;
    }
    let mut comps = Path::new(component).components();
    matches!((comps.next(), comps.next()), (Some(Component::Normal(_)), None))
}

impl AppDirs {
    /// Resolve from the process environment.
    pub fn resolve(component: &str) -> Option<Self> {
        Self::resolve_with(component, |k| std::env::var_os(k).map(PathBuf::from))
    }

    /// Resolve with an injected environment (tests).
    pub fn resolve_with(component: &str, get: impl Fn(&str) -> Option<PathBuf>) -> Option<Self> {
        if !is_valid_component(component) {
            return None;
        }
        let absolute = |p: PathBuf| p.is_absolute().then_some(p);
        let root = get("COSMIX_APP_HOME")
            .and_then(absolute)
            .or_else(|| get("COSMIX_APPS_HOME").and_then(absolute).map(|b| b.join(component)))
            .or_else(|| get("XDG_STATE_HOME").and_then(absolute).map(|b| b.join("cosmix/apps").join(component)))
            .or_else(|| get("HOME").and_then(absolute).map(|h| h.join(".local/state/cosmix/apps").join(component)))?;
        Some(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// The directory the core's `ConfigFile::load` reads `config.conf.mix`
    /// from (the core owns the file name).
    pub fn config_dir(&self) -> PathBuf {
        self.config()
    }
    /// Per-app theme override (layered over the shared `theme.conf.mix`).
    pub fn theme_override(&self) -> PathBuf {
        self.config().join("theme.conf.mix")
    }
    /// Per-app keymap overlay over `cosmix-actions`' packaged filemgr defaults.
    pub fn keymap_file(&self) -> PathBuf {
        self.config().join("keymap.conf.mix")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<PathBuf> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| PathBuf::from(v))
    }

    #[test]
    fn resolution_order() {
        let all = &[("COSMIX_APP_HOME", "/a"), ("COSMIX_APPS_HOME", "/b"), ("XDG_STATE_HOME", "/c"), ("HOME", "/h")];
        assert_eq!(AppDirs::resolve_with("dopus", env(all)).unwrap().root(), Path::new("/a"));
        let three = &[("COSMIX_APPS_HOME", "/b"), ("XDG_STATE_HOME", "/c"), ("HOME", "/h")];
        assert_eq!(AppDirs::resolve_with("dopus", env(three)).unwrap().root(), Path::new("/b/dopus"));
        let one = &[("HOME", "/h")];
        let d = AppDirs::resolve_with("dopus", env(one)).unwrap();
        assert_eq!(d.root(), Path::new("/h/.local/state/cosmix/apps/dopus"));
        assert_eq!(d.keymap_file(), Path::new("/h/.local/state/cosmix/apps/dopus/config/keymap.conf.mix"));
        assert_eq!(d.theme_override(), Path::new("/h/.local/state/cosmix/apps/dopus/config/theme.conf.mix"));
    }

    #[test]
    fn relative_values_and_bad_slugs_are_refused() {
        let rel = &[("COSMIX_APP_HOME", "rel"), ("HOME", "/h")];
        assert_eq!(AppDirs::resolve_with("dopus", env(rel)).unwrap().root(), Path::new("/h/.local/state/cosmix/apps/dopus"));
        assert!(AppDirs::resolve_with("../x", env(&[("HOME", "/h")])).is_none());
        assert!(AppDirs::resolve_with("", env(&[("HOME", "/h")])).is_none());
    }
}
