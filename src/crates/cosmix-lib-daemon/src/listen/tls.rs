//! Per-listener TLS material with hot-swap.
//!
//! Generalises the `TlsSlot` + `ServerConfigCache` primitive that
//! `cosmix-maild/src/tls.rs` introduced for its dual-bind listeners.
//! webd is the second citizen (per-interface scoped SNI), so the
//! core-and-citizen pattern says to extract it here; maild migrates
//! onto this in a later phase.
//!
//! Each [`ListenerTls`] owns one swappable [`SniCertResolver`] slot
//! and a small `ServerConfig` cache keyed by the resolver's pointer
//! identity. A listener's certs are hot-swapped by [`ListenerTls::swap`]
//! (e.g. the ACME provisioner on renewal) without disturbing any
//! other listener — which is exactly what per-interface scoped SNI
//! needs: the public listener's resolver carries only its own vhosts'
//! certs, so a foreign SNI fails at the handshake there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use rustls::server::ServerConfig;

use crate::tls::sni::SniCertResolver;

type ResolverSlot = Arc<ArcSwap<Option<Arc<SniCertResolver>>>>;

/// Per-connection `Arc<ServerConfig>` cache, keyed by the *pointer
/// identity* of the resolver `Arc`. Two handshakes that load the same
/// resolver snapshot share one `Arc<ServerConfig>`; a [`swap`] that
/// replaces the resolver clears the cache so the next handshake
/// rebuilds. In-flight handshakes keep their own `Arc<ServerConfig>`
/// (and via it the old resolver) alive, so they drain cleanly.
///
/// [`swap`]: ListenerTls::swap
#[derive(Debug, Default)]
struct ConfigCache {
    by_resolver: Mutex<HashMap<usize, Arc<ServerConfig>>>,
}

impl ConfigCache {
    fn get_or_build(&self, resolver: &Arc<SniCertResolver>) -> Arc<ServerConfig> {
        let key = Arc::as_ptr(resolver) as usize;
        let mut by = self.by_resolver.lock().expect("ConfigCache poisoned");
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

    fn clear(&self) {
        self.by_resolver
            .lock()
            .expect("ConfigCache poisoned")
            .clear();
    }
}

/// Hot-swappable TLS material for a single listener. Cheap to clone
/// (shares the underlying slot + cache via `Arc`), so the accept
/// driver and the daemon's control plane can both hold a handle.
#[derive(Clone, Debug)]
pub struct ListenerTls {
    slot: ResolverSlot,
    cache: Arc<ConfigCache>,
}

impl ListenerTls {
    /// Build a handle primed with `initial`. `None` means TLS is not
    /// yet available (e.g. an ACME listener before first issuance);
    /// [`is_enabled`](Self::is_enabled) reports `false` and a
    /// `Terminate` listener refuses to bind until a resolver is set.
    pub fn new(initial: Option<Arc<SniCertResolver>>) -> Self {
        Self {
            slot: Arc::new(ArcSwap::from_pointee(initial)),
            cache: Arc::new(ConfigCache::default()),
        }
    }

    /// Replace this listener's resolver (and clear the config cache).
    /// In-flight handshakes are unaffected.
    pub fn swap(&self, resolver: Option<Arc<SniCertResolver>>) {
        self.slot.store(Arc::new(resolver));
        self.cache.clear();
    }

    /// Whether a (non-empty) resolver is currently installed. An empty
    /// resolver counts as disabled — it can never resolve a cert.
    pub fn is_enabled(&self) -> bool {
        self.slot
            .load()
            .as_ref()
            .as_ref()
            .is_some_and(|r| !r.is_empty())
    }

    /// The current `(resolver, ServerConfig)` pair, or `None` when TLS
    /// is disabled. The accept driver calls this once per handshake;
    /// a passthrough handler uses the resolver for the pre-TLS
    /// greeting and the `ServerConfig` for the STARTTLS upgrade.
    ///
    /// An *empty* resolver counts as disabled (returns `None`) — it
    /// can never resolve a cert, so handing back a `ServerConfig` that
    /// fails every handshake would contradict [`is_enabled`](Self::is_enabled).
    pub fn current(&self) -> Option<(Arc<SniCertResolver>, Arc<ServerConfig>)> {
        let guard = self.slot.load();
        let resolver = guard.as_ref().as_ref().cloned()?;
        if resolver.is_empty() {
            return None;
        }
        let cfg = self.cache.get_or_build(&resolver);
        Some((resolver, cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_config::node::TlsIdentityConfig;

    fn install_crypto_once() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    fn resolver(dir: &std::path::Path, name: &str) -> Arc<SniCertResolver> {
        let cert = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        let cert_path = dir.join(format!("{name}.crt"));
        let key_path = dir.join(format!("{name}.key"));
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        let cfg = TlsIdentityConfig {
            server_name: name.into(),
            cert: cert_path.to_string_lossy().into_owned(),
            key: key_path.to_string_lossy().into_owned(),
            default: true,
            no_sni_fallback: true,
        };
        Arc::new(SniCertResolver::from_config(&[cfg], false).unwrap())
    }

    #[test]
    fn disabled_when_empty_or_none() {
        let t = ListenerTls::new(None);
        assert!(!t.is_enabled());
        assert!(t.current().is_none());
    }

    #[test]
    fn empty_resolver_is_disabled() {
        // A resolver with zero identities resolves no cert; current()
        // must agree with is_enabled() and report disabled.
        let empty = Arc::new(SniCertResolver::from_config(&[], false).unwrap());
        let t = ListenerTls::new(Some(empty));
        assert!(!t.is_enabled());
        assert!(t.current().is_none());
    }

    #[test]
    fn enabled_after_swap_and_cache_reuse() {
        install_crypto_once();
        let dir = tempfile::tempdir().unwrap();
        let t = ListenerTls::new(None);
        t.swap(Some(resolver(dir.path(), "host.example")));
        assert!(t.is_enabled());
        let (_, c1) = t.current().unwrap();
        let (_, c2) = t.current().unwrap();
        assert!(
            Arc::ptr_eq(&c1, &c2),
            "same resolver snapshot reuses one ServerConfig"
        );
    }

    #[test]
    fn swap_rebuilds_config() {
        install_crypto_once();
        let dir = tempfile::tempdir().unwrap();
        let t = ListenerTls::new(Some(resolver(dir.path(), "one.example")));
        let (_, c1) = t.current().unwrap();
        t.swap(Some(resolver(dir.path(), "two.example")));
        let (_, c2) = t.current().unwrap();
        assert!(
            !Arc::ptr_eq(&c1, &c2),
            "post-swap rebuilds a fresh ServerConfig"
        );
    }
}
