//! `TrustGrantsCache` — in-memory mirror of wgd's cross-mesh trust /
//! grants / exposable namespaces (plan §3.1).
//!
//! P0-C4 lands the synchronous **read surface** — [`Snapshot`],
//! [`TrustGrantsCache::for_testing`], and [`TrustGrantsCacheHandle`]
//! implementing the core [`TrustStore`] trait. The combinator (§3)
//! reads through the handle on every request; the handle does no IO
//! and no async work.
//!
//! P0-C5 will add the **write surface**: `TrustGrantsCache::start`
//! spawning a background subscriber that uses SPEC-12 §5.5's
//! `props.list + props.watch since_nseq` seam against wgd to
//! seed-then-subscribe the three namespaces, with disconnect-reseed
//! and ready-gating semantics per plan §3.1 steps 1–4.
//!
//! The cache deliberately stores **core** [`TrustedMesh`] / [`Grant`]
//! / [`CapabilitySet`] values, not their SPEC-12 wire forms. The
//! subscriber (P0-C5) is the conversion seam; everything downstream
//! of the snapshot operates on core types so [`resolve_cross_mesh_caps`]
//! can run without a SPEC-12 round-trip.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::caps::{CapabilitySet, Grant, TrustStore, TrustedMesh};

/// In-memory mirror of the three wgd-owned namespaces the combinator
/// reads. Owned by [`TrustGrantsCache`], read through
/// [`TrustGrantsCacheHandle`].
///
/// Three independent maps — trust records are keyed by mesh FQDN,
/// grants are bucketed by the granted mesh so the combinator can
/// `O(grants_for_mesh)` iterate, and exposable allowlists are keyed
/// by SPEC-12 namespace id (e.g. `"maild.accounts"`).
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub trusted: HashMap<String, TrustedMesh>,
    pub grants_by_mesh: HashMap<String, Vec<Grant>>,
    pub exposable: HashMap<String, CapabilitySet>,
}

impl Snapshot {
    /// Build a snapshot from explicit collections — primary entry
    /// point for tests and for the P0-C5 subscriber's atomic
    /// snapshot-replace path.
    pub fn from_parts(
        trusted: impl IntoIterator<Item = TrustedMesh>,
        grants: impl IntoIterator<Item = Grant>,
        exposable: impl IntoIterator<Item = (String, CapabilitySet)>,
    ) -> Self {
        let mut trusted_map: HashMap<String, TrustedMesh> = HashMap::new();
        for t in trusted {
            trusted_map.insert(t.mesh_fqdn.clone(), t);
        }
        let mut grants_by_mesh: HashMap<String, Vec<Grant>> = HashMap::new();
        for g in grants {
            grants_by_mesh
                .entry(g.mesh_fqdn.clone())
                .or_default()
                .push(g);
        }
        let exposable = exposable.into_iter().collect();
        Self {
            trusted: trusted_map,
            grants_by_mesh,
            exposable,
        }
    }
}

/// Owns the trust-grants snapshot for one process and (P0-C5+) the
/// background subscriber task that keeps it up to date.
///
/// P0-C4 exposes only [`TrustGrantsCache::for_testing`] and
/// [`TrustGrantsCache::handle`]. The async start path lands in P0-C5
/// once the fake wgd-client + real subscriber are in place.
#[derive(Debug, Clone)]
pub struct TrustGrantsCache {
    inner: Arc<RwLock<Snapshot>>,
}

impl TrustGrantsCache {
    /// Build a cache pre-populated from fixtures. Used in tests of
    /// the combinator (P0-C4) and by P0-C5's subscriber tests for
    /// initial-state shaping.
    pub fn for_testing(snapshot: Snapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
        }
    }

    /// Clone-cheap read handle. Every wrapped namespace's
    /// `AuthPolicy` carries one of these.
    pub fn handle(&self) -> TrustGrantsCacheHandle {
        TrustGrantsCacheHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Atomically replace the snapshot. Used by tests and (P0-C5) by
    /// the subscriber on a reseed.
    pub fn replace(&self, snapshot: Snapshot) {
        let mut guard = self.inner.write().expect("snapshot lock poisoned");
        *guard = snapshot;
    }

    /// Atomically mutate the snapshot in place. Used by the P0-C5b
    /// subscriber tasks to rebuild their owned slice (trusted /
    /// grants_by_mesh / exposable) after each list or watch event.
    pub fn mutate(&self, f: impl FnOnce(&mut Snapshot)) {
        let mut guard = self.inner.write().expect("snapshot lock poisoned");
        f(&mut guard);
    }
}

/// Clone-cheap read handle to a [`TrustGrantsCache`]. Implements
/// [`TrustStore`] so the combinator can hand it directly to
/// [`crate::caps::resolve_cross_mesh_caps`].
#[derive(Debug, Clone)]
pub struct TrustGrantsCacheHandle {
    inner: Arc<RwLock<Snapshot>>,
}

impl TrustGrantsCacheHandle {
    fn read<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> R {
        let guard = self.inner.read().expect("snapshot lock poisoned");
        f(&guard)
    }

    pub fn trusted_mesh(&self, fqdn: &str) -> Option<TrustedMesh> {
        self.read(|s| s.trusted.get(fqdn).cloned())
    }

    pub fn grants_for_mesh(&self, fqdn: &str) -> Vec<Grant> {
        self.read(|s| s.grants_by_mesh.get(fqdn).cloned().unwrap_or_default())
    }

    pub fn exposable_for_namespace(&self, ns_id: &str) -> CapabilitySet {
        self.read(|s| s.exposable.get(ns_id).cloned().unwrap_or_default())
    }
}

impl TrustStore for TrustGrantsCacheHandle {
    fn trusted_mesh(&self, fqdn: &str) -> Option<TrustedMesh> {
        Self::trusted_mesh(self, fqdn)
    }

    fn grants_for_mesh(&self, fqdn: &str) -> Vec<Grant> {
        Self::grants_for_mesh(self, fqdn)
    }

    fn exposable_for_namespace(&self, ns_id: &str) -> CapabilitySet {
        Self::exposable_for_namespace(self, ns_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Cap;
    use chrono::{DateTime, Duration, TimeZone as _, Utc};

    fn fixture_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn fixture_trusted(now: DateTime<Utc>) -> TrustedMesh {
        TrustedMesh {
            mesh_fqdn: "channie.net".into(),
            current_pubkey: vec![0u8; 32],
            current_selector: "2026q3".into(),
            previous_pubkey: None,
            previous_selector: None,
            previous_pubkey_valid_until: None,
            last_verified_at: now - Duration::hours(1),
            max_pubkey_age_secs: 7 * 86400,
            enabled: true,
        }
    }

    fn fixture_grant(now: DateTime<Utc>, caps: &[&str]) -> Grant {
        Grant {
            grant_id: "01HXEXAMPLE".into(),
            mesh_fqdn: "channie.net".into(),
            capabilities: caps.iter().map(|s| Cap::new(*s)).collect(),
            valid_from: now - Duration::days(1),
            valid_until: now + Duration::days(30),
            enabled: true,
        }
    }

    fn caps_of(tokens: &[&str]) -> CapabilitySet {
        tokens.iter().map(|s| Cap::new(*s)).collect()
    }

    #[test]
    fn handle_returns_inserted_records() {
        let now = fixture_now();
        let cache = TrustGrantsCache::for_testing(Snapshot::from_parts(
            [fixture_trusted(now)],
            [fixture_grant(now, &["props.read:maild.accounts"])],
            [(
                "maild.accounts".into(),
                caps_of(&["props.read:maild.accounts"]),
            )],
        ));
        let h = cache.handle();
        assert!(h.trusted_mesh("channie.net").is_some());
        assert!(h.trusted_mesh("unknown.example").is_none());
        assert_eq!(h.grants_for_mesh("channie.net").len(), 1);
        assert!(h.grants_for_mesh("example.com").is_empty());
        assert_eq!(h.exposable_for_namespace("maild.accounts").len(), 1);
        assert!(h.exposable_for_namespace("maild.engine_config").is_empty());
    }

    #[test]
    fn handle_is_clone_cheap_and_sees_replacements() {
        // Plan §3.1: subscriber atomically replaces; outstanding
        // handles see the new state on next read.
        let now = fixture_now();
        let cache = TrustGrantsCache::for_testing(Snapshot::default());
        let h1 = cache.handle();
        let h2 = cache.handle();
        assert!(h1.trusted_mesh("channie.net").is_none());

        cache.replace(Snapshot::from_parts(
            [fixture_trusted(now)],
            [],
            std::iter::empty::<(String, CapabilitySet)>(),
        ));

        assert!(h1.trusted_mesh("channie.net").is_some());
        assert!(h2.trusted_mesh("channie.net").is_some());
    }

    #[test]
    fn handle_implements_trust_store() {
        // The combinator hands the handle directly to the core
        // resolve algorithm via the TrustStore trait; verify the
        // delegation rather than asserting it indirectly through the
        // combinator's surface (which sits in combinator.rs tests).
        let now = fixture_now();
        let cache = TrustGrantsCache::for_testing(Snapshot::from_parts(
            [fixture_trusted(now)],
            [fixture_grant(now, &["props.read:maild.accounts"])],
            [(
                "maild.accounts".into(),
                caps_of(&["props.read:maild.accounts"]),
            )],
        ));
        let h = cache.handle();
        let store: &dyn TrustStore = &h;
        assert!(store.trusted_mesh("channie.net").is_some());
        assert_eq!(store.grants_for_mesh("channie.net").len(), 1);
        assert_eq!(store.exposable_for_namespace("maild.accounts").len(), 1);
    }
}
