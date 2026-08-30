//! TLS surface for maild — re-exports the shared SNI resolver from
//! `cosmix_daemon::tls::sni`, plus the [`ServerConfigCache`] and
//! [`TlsSlot`] hot-swap primitives that the SMTP and IMAPS listeners
//! use to capture a per-connection resolver snapshot at TCP-accept
//! time (umbrella Phase 5a-commit-1, see
//! `_doc/planned/maild-multi-vhost-phase5.md`).
//!
//! The shared resolver implementation lives in `cosmix-lib-daemon` so
//! webd and maild share one code path rather than two divergent
//! copies. The cache + slot live here (in the maild citizen) because
//! they are a maild-side listener concern; if a second daemon citizen
//! later wants the same primitive, the core-and-citizen pattern says
//! to extract at that point.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use rustls::server::ServerConfig;

pub use cosmix_daemon::tls::sni::{SniCertResolver, TlsIdentity, pick_identity};

/// Live-swappable TLS resolver slot. The Phase 5a `maild.tls.reload`
/// verb (commit-2) `store`s a freshly-built `Arc<SniCertResolver>`
/// here; the listener accept loops `load_full()` it once per
/// connection and pass the captured snapshot to the session for the
/// duration of that connection.
///
/// `None` means TLS is disabled (no `[[tls.identity]]` rows and no
/// `tls_identity_name`-declaring `maild.domains` row at startup) —
/// SMTPS and IMAPS listeners refuse to bind in that case, plaintext
/// SMTP still serves without STARTTLS.
pub type TlsSlot = Arc<ArcSwap<Option<Arc<SniCertResolver>>>>;

/// Build a fresh `TlsSlot` primed with the given initial resolver.
pub fn new_tls_slot(initial: Option<Arc<SniCertResolver>>) -> TlsSlot {
    Arc::new(ArcSwap::from_pointee(initial))
}

/// Per-connection `Arc<ServerConfig>` cache, keyed by the *pointer
/// identity* of the resolver `Arc` (`Arc::as_ptr(&resolver) as usize`).
/// Two connections that accepted under the same `TlsSlot` snapshot
/// share one `Arc<ServerConfig>`; a reload that swaps the slot
/// invalidates the cache via [`clear`](Self::clear).
///
/// The cache is intentionally small: at most one entry while the slot
/// is steady; transiently two during the post-`store` / pre-`clear`
/// window in the reload verb body. A live SMTPS handshake holding
/// the old `Arc<ServerConfig>` keeps the old `Arc<SniCertResolver>`
/// alive via the `with_cert_resolver` reference, so in-flight
/// connections drain cleanly without coordination.
#[derive(Debug)]
pub struct ServerConfigCache {
    by_resolver: Mutex<HashMap<usize, Arc<ServerConfig>>>,
}

impl ServerConfigCache {
    pub fn new() -> Self {
        Self {
            by_resolver: Mutex::new(HashMap::new()),
        }
    }

    /// Return the cached `Arc<ServerConfig>` for this resolver `Arc`,
    /// building one on cache miss.
    pub fn get_or_build(&self, resolver: &Arc<SniCertResolver>) -> Arc<ServerConfig> {
        let key = Arc::as_ptr(resolver) as usize;
        let mut by = self.by_resolver.lock().expect("ServerConfigCache poisoned");
        if let Some(cfg) = by.get(&key) {
            return cfg.clone();
        }
        let cfg = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(resolver.clone()),
        );
        by.insert(key, cfg.clone());
        cfg
    }

    /// Drop all cached `ServerConfig`s. The Phase 5a-commit-2 reload
    /// verb calls this after `slot.store(new_resolver)`. In-flight
    /// handshakes hold their own `Arc<ServerConfig>` and are
    /// unaffected.
    pub fn clear(&self) {
        self.by_resolver
            .lock()
            .expect("ServerConfigCache poisoned")
            .clear();
    }
}

impl Default for ServerConfigCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Load the current resolver snapshot and the cached `ServerConfig`
/// for it. Returns `None` when TLS is disabled. The listener accept
/// loops call this once per accepted connection and pass the resolver
/// `Arc` to the session for greeting recovery.
pub fn current_tls_pair(
    slot: &TlsSlot,
    cache: &ServerConfigCache,
) -> Option<(Arc<SniCertResolver>, Arc<ServerConfig>)> {
    let guard = slot.load();
    let resolver = guard.as_ref().as_ref().cloned()?;
    let cfg = cache.get_or_build(&resolver);
    Some((resolver, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_config::node::TlsIdentityConfig;
    use std::io::Write;
    use tempfile::TempDir;

    // Compile-time assertion that the public surface the maild
    // callers (`smtp/mod.rs`, `imap/server.rs`, `props/tls_identities
    // .rs`) rely on is preserved across the lift. If the resolver's
    // `from_config` signature changes upstream this stops compiling,
    // which is the loud signal we want before runtime.
    #[allow(dead_code)]
    fn _surface_preserved() {
        let _r: fn(&[TlsIdentityConfig], bool) -> anyhow::Result<SniCertResolver> =
            SniCertResolver::from_config;
    }

    /// Write a self-signed cert + key pair into `dir` and return the
    /// `TlsIdentityConfig` rows pointing at them. Single helper so
    /// the three cache tests below share one path.
    fn write_identity(dir: &TempDir, server_name: &str) -> TlsIdentityConfig {
        let cert = rcgen::generate_simple_self_signed(vec![server_name.into()])
            .expect("rcgen self-signed");
        let cert_path = dir.path().join(format!("{server_name}.cert.pem"));
        let key_path = dir.path().join(format!("{server_name}.key.pem"));
        std::fs::File::create(&cert_path)
            .unwrap()
            .write_all(cert.cert.pem().as_bytes())
            .unwrap();
        std::fs::File::create(&key_path)
            .unwrap()
            .write_all(cert.key_pair.serialize_pem().as_bytes())
            .unwrap();
        TlsIdentityConfig {
            server_name: server_name.into(),
            cert: cert_path.to_string_lossy().into_owned(),
            key: key_path.to_string_lossy().into_owned(),
            default: true,
            no_sni_fallback: true,
        }
    }

    fn build_resolver(dir: &TempDir, server_name: &str) -> Arc<SniCertResolver> {
        let ident = write_identity(dir, server_name);
        Arc::new(SniCertResolver::from_config(&[ident], false).expect("build resolver"))
    }

    #[test]
    fn cache_reuses_same_serverconfig_for_same_resolver_arc() {
        let dir = TempDir::new().unwrap();
        let resolver = build_resolver(&dir, "host.example");
        let cache = ServerConfigCache::new();
        let a = cache.get_or_build(&resolver);
        let b = cache.get_or_build(&resolver);
        assert!(
            Arc::ptr_eq(&a, &b),
            "cache must return the same Arc<ServerConfig> for the same resolver Arc"
        );
    }

    #[test]
    fn cache_builds_fresh_after_clear() {
        let dir = TempDir::new().unwrap();
        let resolver = build_resolver(&dir, "host.example");
        let cache = ServerConfigCache::new();
        let a = cache.get_or_build(&resolver);
        cache.clear();
        let b = cache.get_or_build(&resolver);
        assert!(
            !Arc::ptr_eq(&a, &b),
            "post-clear, the cache must build a fresh ServerConfig even \
             for the same resolver Arc (the old one stays alive only via \
             outstanding callers, not via the cache)"
        );
    }

    #[test]
    fn cache_returns_distinct_configs_for_ptr_eq_distinct_resolvers() {
        let dir = TempDir::new().unwrap();
        let r1 = build_resolver(&dir, "one.example");
        let r2 = build_resolver(&dir, "two.example");
        let cache = ServerConfigCache::new();
        let c1 = cache.get_or_build(&r1);
        let c2 = cache.get_or_build(&r2);
        assert!(
            !Arc::ptr_eq(&c1, &c2),
            "distinct resolver Arcs must produce distinct ServerConfig Arcs"
        );
    }

    #[test]
    fn current_tls_pair_returns_none_when_slot_empty() {
        let slot: TlsSlot = new_tls_slot(None);
        let cache = ServerConfigCache::new();
        assert!(current_tls_pair(&slot, &cache).is_none());
    }

    #[test]
    fn current_tls_pair_swap_replace_reflects_new_resolver() {
        let dir = TempDir::new().unwrap();
        let r1 = build_resolver(&dir, "one.example");
        let r2 = build_resolver(&dir, "two.example");
        let slot: TlsSlot = new_tls_slot(Some(r1.clone()));
        let cache = ServerConfigCache::new();
        let (first_resolver, _) = current_tls_pair(&slot, &cache).expect("present");
        assert!(Arc::ptr_eq(&first_resolver, &r1));
        slot.store(Arc::new(Some(r2.clone())));
        let (second_resolver, _) = current_tls_pair(&slot, &cache).expect("present");
        assert!(
            Arc::ptr_eq(&second_resolver, &r2),
            "after slot.store, current_tls_pair must reflect the new resolver Arc"
        );
        assert!(
            !Arc::ptr_eq(&first_resolver, &second_resolver),
            "ptr_eq sanity: r1 and r2 are distinct Arcs"
        );
    }
}
