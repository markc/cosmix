//! Runtime-directory resolution for CTK apps.
//!
//! One coherent per-app root with `config/`, `state/`, and `cache/` as
//! subdirs — located XDG-politely and overridable by env. Deliberately NOT a
//! three-way XDG split: a single legible root serves the reconstructibility
//! criterion (copy one dir, clone the app's world).
//!
//! The on-disk identity is the app's stable **component slug** (`studio`,
//! `filemgr`, … — registry: `desktop/APPS.md`), never its display name.
//! Display/brand renames ("CosMix Studio" → anything) therefore move
//! nobody's data. Retiring a *slug* is different: it requires a one-time
//! state migration (old root → new root), and retired slugs are never
//! reused — a new app must never inherit a dead app's state (precedent:
//! `midiseq` → `studio`, 2026-07-24).
//!
//! Resolution order (first match wins; only absolute values are accepted):
//!   1. `$COSMIX_APP_HOME`                     — exact per-process root
//!   2. `$COSMIX_APPS_HOME/<component>`        — shared apps base
//!   3. `$XDG_STATE_HOME/cosmix/apps/<component>`
//!   4. `$HOME/.local/state/cosmix/apps/<component>`
//!
//! `COSMIX_APP_HOME` is launcher-scoped: it names ONE app's exact root, so a
//! launcher sets it per-process. Exporting it globally would collide every
//! app onto one root — use `COSMIX_APPS_HOME` (a base) for a fleet-wide
//! relocation instead.

use std::path::{Component, Path, PathBuf};

/// True iff `component` is a single, normal path segment — the contract the
/// resolver relies on so `<base>.join(component)` can never escape the base,
/// replace it, or alias another slug. Rejects empties, `.`/`..`, anything
/// containing a separator (so `filemgr/`, `filemgr//`, `filemgr/.` are OUT,
/// not silently normalised to `filemgr`), and non-normal path shapes.
fn is_valid_component(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains(['/', '\\'])
    {
        return false;
    }
    // Belt-and-braces: exactly one normal component and nothing else.
    let mut parts = Path::new(component).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

/// Resolved runtime root for an app. `config()`, `state()`, and `cache()` are
/// its subdirs. Nothing is created until a caller asks (create lazily on first
/// write, e.g. via [`crate::fs::write_atomic`]). The root is always absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    root: PathBuf,
}

impl AppDirs {
    /// Resolve against the real process environment for the given stable
    /// component slug. Returns `None` if `component` is not a single normal
    /// path segment (a programming error — slugs are fixed literals) OR if the
    /// environment names no absolute base at all (no `$COSMIX_APP_HOME`,
    /// `$COSMIX_APPS_HOME`, `$XDG_STATE_HOME`, or `$HOME`). A normal desktop
    /// always has `$HOME`, so it never hits `None`; a caller that does gets a
    /// clean signal to refuse rather than write to an unsafe/relative path.
    pub fn resolve(component: &str) -> Option<Self> {
        Self::resolve_with(component, |k| std::env::var_os(k).map(PathBuf::from))
    }

    /// Pure resolver core — `get` supplies env values, so this is testable
    /// without touching (racy, process-global) real env vars. The resolved
    /// root is always absolute (only absolute env values are accepted).
    fn resolve_with(component: &str, get: impl Fn(&str) -> Option<PathBuf>) -> Option<Self> {
        if !is_valid_component(component) {
            return None;
        }
        let absolute = |p: PathBuf| p.is_absolute().then_some(p);
        let root = get("COSMIX_APP_HOME")
            .and_then(absolute)
            .or_else(|| {
                get("COSMIX_APPS_HOME")
                    .and_then(absolute)
                    .map(|base| base.join(component))
            })
            .or_else(|| {
                get("XDG_STATE_HOME")
                    .and_then(absolute)
                    .map(|base| base.join("cosmix/apps").join(component))
            })
            .or_else(|| {
                get("HOME")
                    .and_then(absolute)
                    .map(|home| home.join(".local/state/cosmix/apps").join(component))
            })?;
        Some(Self { root })
    }

    /// The app's absolute runtime root.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<PathBuf> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| PathBuf::from(v))
        }
    }

    fn resolve(component: &str, pairs: &[(&str, &str)]) -> Option<AppDirs> {
        AppDirs::resolve_with(component, env(pairs))
    }

    #[test]
    fn cosmix_app_home_wins_as_exact_root() {
        let d = resolve(
            "studio",
            &[
                ("COSMIX_APP_HOME", "/run/app-xyz"),
                ("COSMIX_APPS_HOME", "/apps"),
                ("XDG_STATE_HOME", "/xdg"),
                ("HOME", "/home/u"),
            ],
        )
        .unwrap();
        assert_eq!(d.root(), Path::new("/run/app-xyz"));
        assert_eq!(d.config(), PathBuf::from("/run/app-xyz/config"));
        assert_eq!(d.state(), PathBuf::from("/run/app-xyz/state"));
        assert_eq!(d.cache(), PathBuf::from("/run/app-xyz/cache"));
    }

    #[test]
    fn apps_home_appends_stable_component() {
        let d = resolve(
            "filemgr",
            &[
                ("COSMIX_APPS_HOME", "/apps"),
                ("XDG_STATE_HOME", "/xdg"),
                ("HOME", "/home/u"),
            ],
        )
        .unwrap();
        assert_eq!(d.root(), Path::new("/apps/filemgr"));
    }

    #[test]
    fn xdg_state_home_used_before_home() {
        let d = resolve("studio", &[("XDG_STATE_HOME", "/xdg"), ("HOME", "/home/u")]).unwrap();
        assert_eq!(d.root(), Path::new("/xdg/cosmix/apps/studio"));
    }

    #[test]
    fn falls_back_to_home_local_state() {
        let d = resolve("filemgr", &[("HOME", "/home/u")]).unwrap();
        assert_eq!(
            d.root(),
            Path::new("/home/u/.local/state/cosmix/apps/filemgr")
        );
    }

    #[test]
    fn relative_values_are_ignored() {
        let d = resolve(
            "studio",
            &[
                ("COSMIX_APP_HOME", "relative/root"),
                ("XDG_STATE_HOME", "also/relative"),
                ("HOME", "/home/u"),
            ],
        )
        .unwrap();
        assert_eq!(
            d.root(),
            Path::new("/home/u/.local/state/cosmix/apps/studio")
        );
    }

    #[test]
    fn no_absolute_base_yields_none() {
        // A broken environment (no HOME/XDG/COSMIX) refuses rather than writing
        // to an unsafe or launch-relative path.
        assert!(resolve("studio", &[]).is_none());
        // A relative HOME is not an absolute base either.
        assert!(resolve("studio", &[("HOME", "relative")]).is_none());
    }

    #[test]
    fn invalid_component_yields_none() {
        // Escapes, empties, and separator-bearing aliases of a valid slug.
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "/abs",
            "filemgr/",
            "filemgr//",
            "filemgr/.",
            "..\\x",
        ] {
            assert!(
                resolve(bad, &[("HOME", "/home/u")]).is_none(),
                "component {bad:?} should be rejected"
            );
        }
    }
}
