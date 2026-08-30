//! Mix-local `node.conf.mix` broker-URL resolver. Inlined so mix has no
//! dependency on the cos-side `cosmix-lib-config` crate; mirrors that
//! crate's `node::load_node_config()` search order so `mix` and any
//! cos daemons running on the same node resolve the same broker URL:
//!
//! 1. `COSMIX_NODE_CONFIG` env var (explicit path override).
//! 2. `cosmix_path(Etc).join("node.conf.mix")` — normally under
//!    `~/.config/cosmix/` for non-root users, `/etc/cosmix/` for root.
//! 3. `/etc/cosmix/node.conf.mix` as the system fallback — suppressed
//!    when `COSMIX_ETC` env var is set (the explicit isolation knob for
//!    tests, chroots, alternate installs).
//!
//! Parses just the two fields the URL format needs — top-level
//! `wg_ip` and `[noded] port` — to keep the surface narrow. The full
//! `NodeConfig` shape lives in `cosmix-lib-config` and is what cos
//! daemons use; mix only needs the broker URL so a slimmer model is
//! sufficient.
//!
//! **READ-ONLY by design.** This resolver never writes a
//! `node.conf.mix`: its slim 2-field schema would emit a lossy file
//! that shadowed the full one cos's `node.rs` owns (the sole writer).
//!
//! `.conf.mix` is the only format — the legacy `node.toml` fallback was
//! removed in C11 (`_doc/planned/2026-05-31-c11-toml-fallback-removal.md`).
//!
//! Fall back to `ws://127.0.0.1:4200/ws` when no file is found, OR
//! the file fails to parse, OR `wg_ip` / `noded.port` are missing —
//! matching `resolve_noded_url()`'s permissive behaviour.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cosmix_paths::{CosmixDir, cosmix_path};

const FALLBACK_URL: &str = "ws://127.0.0.1:4200/ws";

/// Slim NodeConfig schema — only the two fields mix needs to format
/// the broker URL. Top-level `#[serde(default)]` so a file that omits
/// `wg_ip` parses to `""`; `broker_url()` then substitutes loopback
/// (127.0.0.1) rather than emitting a hostless `ws://:PORT/ws`, so a
/// standalone / non-mesh node still reaches its own local broker
/// (markc/mix#1).
///
/// **Documented schema divergence vs upstream.** Upstream `NodeConfig`
/// has typed fields for every section (`[noded]`, `[maild]`, `[tls]`,
/// `[webd]`, `[dnsd]`, etc.). Mix's slim model only types `wg_ip` +
/// `[noded] port`; unknown keys are skipped (the struct is not
/// `deny_unknown_fields`), so the full `node.conf.mix` that cos's
/// `node.rs` writes parses down to the two fields mix needs. Full
/// typed-schema parity would require replicating cos's whole
/// `NodeConfig` tree — that's cos's job, not mix's.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MixNodeConfig {
    wg_ip: String,
    noded: NodedSection,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct NodedSection {
    port: u16,
}

impl Default for NodedSection {
    fn default() -> Self {
        Self {
            port: default_port(),
        }
    }
}

fn default_port() -> u16 {
    4200
}

impl MixNodeConfig {
    /// Broker URL from this config's `wg_ip:noded.port`. An **empty** `wg_ip`
    /// — a present config that omits it, e.g. a standalone / non-mesh node —
    /// falls back to loopback rather than emitting a hostless `ws://:PORT/ws`
    /// (markc/mix#1). A non-empty `wg_ip` is used verbatim.
    fn broker_url(&self) -> String {
        let host = if self.wg_ip.is_empty() {
            "127.0.0.1"
        } else {
            self.wg_ip.as_str()
        };
        format!("ws://{}:{}/ws", host, self.noded.port)
    }
}

pub fn resolve_noded_url() -> String {
    load()
        .map(|c| c.broker_url())
        .unwrap_or_else(|| FALLBACK_URL.to_string())
}

/// Walk `search_paths()` until the first **existing** file. Return the
/// parse result for that one file — success → Some, parse error → None
/// (caller falls back to loopback). **Does NOT continue searching past
/// a file that exists but fails to parse**, matching the upstream
/// `cosmix_config::node::load_node_config()` behaviour: the first
/// existing file is authoritative, even if broken. Falling through
/// past a broken primary to a secondary would let mix dial a
/// different broker than cos daemons running on the same node.
fn load() -> Option<MixNodeConfig> {
    for path in search_paths() {
        if path.exists() {
            return load_from(&path);
        }
    }
    None
}

fn search_paths() -> Vec<PathBuf> {
    if let Ok(path) = std::env::var("COSMIX_NODE_CONFIG") {
        return vec![PathBuf::from(path)];
    }

    let etc = cosmix_path(CosmixDir::Etc);
    let cosmix_etc_set = std::env::var_os("COSMIX_ETC").is_some();

    let mut dirs = vec![etc];
    if !cosmix_etc_set {
        let system = PathBuf::from("/etc/cosmix");
        if !dirs.contains(&system) {
            dirs.push(system);
        }
    }
    expand_node_paths(dirs)
}

/// Expand candidate directories to concrete `node.conf.mix` paths.
/// Pure (no env / no `cosmix_path` caching) so the ordering is unit-
/// testable without mutating process env vars.
fn expand_node_paths(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join("node.conf.mix"))
        .collect()
}

/// Parse one `node.conf.mix` file as strict-data via the serde bridge.
/// Unknown top-level / `noded` keys are skipped (the struct is not
/// `deny_unknown_fields`), so the full node.conf.mix that cos's
/// `node.rs` writes parses down to the two fields mix needs. Returns
/// `None` (→ loopback) on any read/parse failure, matching the
/// permissive fallback contract. READ-ONLY — never writes.
fn load_from(path: &Path) -> Option<MixNodeConfig> {
    let contents = std::fs::read_to_string(path).ok()?;
    cosmix_mix::from_conf_mix_str(&contents).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("cosmix-mix-nodecfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn url(c: &MixNodeConfig) -> String {
        c.broker_url()
    }

    #[test]
    fn conf_mix_missing_wg_ip_defaults_loopback() {
        // A present config that omits wg_ip (standalone / non-mesh node) must
        // resolve to the local loopback broker, not a hostless ws://:PORT/ws
        // (markc/mix#1).
        let dir = temp_dir("nowgip");
        let p = dir.join("node.conf.mix");
        std::fs::write(&p, "node: \"solo\"\nnoded: { port: 4200 }\n").unwrap();
        let c = load_from(&p).expect("parse");
        assert_eq!(url(&c), "ws://127.0.0.1:4200/ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conf_mix_extracts_wg_ip_and_port() {
        let dir = temp_dir("confmix");
        let p = dir.join("node.conf.mix");
        std::fs::write(&p, "wg_ip: \"192.0.2.5\"\nnoded: { port: 4300 }\n").unwrap();
        let c = load_from(&p).expect("parse");
        assert_eq!(url(&c), "ws://192.0.2.5:4300/ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conf_mix_missing_port_defaults_4200() {
        let dir = temp_dir("defaultport");
        let p = dir.join("node.conf.mix");
        std::fs::write(&p, "wg_ip: \"192.0.2.9\"\n").unwrap();
        let c = load_from(&p).expect("parse");
        assert_eq!(url(&c), "ws://192.0.2.9:4200/ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conf_mix_ignores_extra_sections() {
        // A full node.conf.mix (as cos node.rs writes it) parses down to
        // the two fields mix needs; unknown top-level / noded keys are
        // skipped (not deny_unknown_fields).
        let dir = temp_dir("full");
        let p = dir.join("node.conf.mix");
        std::fs::write(
            &p,
            "node: \"alpha\"\nwg_ip: \"192.0.2.5\"\nmesh: \"example.org\"\n\
             noded: { port: 4321, mesh_config: \"/etc/x\" }\n\
             maild: { enabled: true, jmap_port: 8443 }\n",
        )
        .unwrap();
        let c = load_from(&p).expect("parse");
        assert_eq!(url(&c), "ws://192.0.2.5:4321/ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_toml_is_not_parsed() {
        // C11: the TOML fallback is gone. TOML `key = value` syntax is
        // not valid strict-data, so a node.toml fails to parse → None
        // (→ loopback), and the resolver stays read-only.
        let dir = temp_dir("legacy");
        let toml_path = dir.join("node.toml");
        std::fs::write(&toml_path, "wg_ip = \"192.0.2.7\"\n[noded]\nport = 4400\n").unwrap();
        assert!(load_from(&toml_path).is_none());
        assert!(
            !dir.join("node.conf.mix").exists(),
            "mix resolver must be read-only — no .conf.mix sibling written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_conf_mix_returns_none() {
        let dir = temp_dir("broken");
        let p = dir.join("node.conf.mix");
        // A variable reference is not strict-data → parse_data rejects.
        std::fs::write(&p, "wg_ip: $x\n").unwrap();
        assert!(load_from(&p).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_node_paths_emits_conf_mix_per_dir() {
        let dirs = vec![
            PathBuf::from("/home/alice/.config/cosmix"),
            PathBuf::from("/etc/cosmix"),
        ];
        assert_eq!(
            expand_node_paths(dirs),
            vec![
                PathBuf::from("/home/alice/.config/cosmix/node.conf.mix"),
                PathBuf::from("/etc/cosmix/node.conf.mix"),
            ]
        );
    }
}
