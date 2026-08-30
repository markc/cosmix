//! Per-service `.conf.mix` I/O.
//!
//! Each service owns its own file at `~/.config/cosmix/<service>.conf.mix`.
//! Define a typed `*Settings` struct with `Default + Serialize + Deserialize`,
//! then call `load_service::<T>("name")` to read it. Missing files are
//! materialized with defaults so `cat ~/.config/cosmix/<name>.conf.mix` is
//! always a self-documenting view of the live configuration.
//!
//! `.conf.mix` is the only config format. The legacy `.toml`
//! read-and-upgrade fallback was removed in C11
//! (`_doc/planned/2026-05-31-c11-toml-fallback-removal.md`); the
//! conf.mix migration (C1–C11) is complete.
//!
//! Convention documented in `_plan/cosmix-config-rebuild.md` §12 Step 2
//! and `_doc/planned/2026-05-29-conf-mix-config-migration.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Configuration directory (resolved via `COSMIX_ETC`).
pub fn config_dir() -> PathBuf {
    crate::cosmix_path(crate::CosmixDir::Etc)
}

/// Project source tree (resolved via `COSMIX_SRC`).
pub fn cosmix_src() -> PathBuf {
    crate::cosmix_path(crate::CosmixDir::Src)
}

fn conf_mix_path(dir: &Path, service: &str) -> PathBuf {
    dir.join(format!("{service}.conf.mix"))
}

/// Load a per-service `.conf.mix` file from `~/.config/cosmix/`.
///
/// Resolution order:
/// 1. `<service>.conf.mix` — parsed via the strict-data serde bridge.
/// 2. missing — `T::default()` is written as `.conf.mix` and returned,
///    so the defaults are discoverable by `cat`.
pub fn load_service<T>(service: &str) -> Result<T>
where
    T: Default + serde::de::DeserializeOwned + serde::Serialize,
{
    load_service_in(&config_dir(), service)
}

/// Save per-service config as `<dir>/<service>.conf.mix`.
pub fn save_service<T>(settings: &T, service: &str) -> Result<()>
where
    T: serde::Serialize,
{
    save_service_in(&config_dir(), settings, service)
}

/// Directory-explicit core of [`load_service`]. Split out so tests can
/// drive it against a temp dir without mutating the `COSMIX_ETC` env var
/// (which would poison the `OnceLock`-cached path resolver in `paths.rs`
/// for sibling tests) — same hermeticity discipline as `node.rs`.
fn load_service_in<T>(dir: &Path, service: &str) -> Result<T>
where
    T: Default + serde::de::DeserializeOwned + serde::Serialize,
{
    let conf = conf_mix_path(dir, service);
    if conf.exists() {
        let content = std::fs::read_to_string(&conf)
            .with_context(|| format!("reading {}", conf.display()))?;
        let settings: T = cosmix_mix::from_conf_mix_str(&content)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", conf.display()))?;
        return Ok(settings);
    }

    let defaults = T::default();
    save_service_in(dir, &defaults, service)?;
    Ok(defaults)
}

/// Read and parse a `.conf.mix` config file from an explicit path.
///
/// The successor to the migration-era extension-dispatching loader: now
/// that the TOML fallback is gone (C11), every config file is strict-data
/// `.conf.mix`, so this just reads the file and parses it via the serde
/// bridge. A read or parse failure is a hard error with the path in the
/// chain — so a typo or wrong perms can't silently re-route a daemon to a
/// different config. The in-a-directory "prefer `.conf.mix`" search is
/// the caller's job (it varies per loader).
pub fn load_conf_mix_path<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    cosmix_mix::from_conf_mix_str(&content)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))
}

/// Directory-explicit core of [`save_service`].
fn save_service_in<T>(dir: &Path, settings: &T, service: &str) -> Result<()>
where
    T: serde::Serialize,
{
    let path = conf_mix_path(dir, service);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let content = cosmix_mix::to_conf_mix_string(settings)
        .map_err(|e| anyhow::anyhow!("serializing {service} settings: {e}"))?;
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Demo {
        name: String,
        port: u16,
        tags: Vec<String>,
    }

    impl Default for Demo {
        fn default() -> Self {
            Self {
                name: "default-name".into(),
                port: 4200,
                tags: vec!["a".into()],
            }
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("cosmix-store-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("roundtrip");
        let want = Demo {
            name: "alpha".into(),
            port: 8080,
            tags: vec!["x".into(), "y".into()],
        };
        save_service_in(&dir, &want, "demo").unwrap();
        assert!(dir.join("demo.conf.mix").exists());
        let got: Demo = load_service_in(&dir, "demo").unwrap();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_materialises_defaults_as_conf_mix() {
        let dir = temp_dir("defaults");
        let got: Demo = load_service_in(&dir, "demo").unwrap();
        assert_eq!(got, Demo::default());
        // Materialised file is `.conf.mix`, not `.toml`.
        assert!(dir.join("demo.conf.mix").exists());
        assert!(!dir.join("demo.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_toml_is_ignored() {
        // C11: the TOML fallback is gone. A lone legacy `demo.toml` is
        // never read — load materialises defaults as `.conf.mix` as if
        // no config existed.
        let dir = temp_dir("legacy");
        std::fs::write(
            dir.join("demo.toml"),
            "name = \"from-toml\"\nport = 25\ntags = [\"t1\", \"t2\"]\n",
        )
        .unwrap();
        let got: Demo = load_service_in(&dir, "demo").unwrap();
        assert_eq!(got, Demo::default());
        assert!(dir.join("demo.conf.mix").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conf_mix_read_when_present() {
        let dir = temp_dir("prefer");
        save_service_in(
            &dir,
            &Demo {
                name: "native".into(),
                port: 1,
                tags: vec![],
            },
            "demo",
        )
        .unwrap();
        let got: Demo = load_service_in(&dir, "demo").unwrap();
        assert_eq!(got.name, "native");
        assert_eq!(got.port, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
