//! `with_cross_mesh_grants` — wraps a namespace's existing
//! [`AuthPolicy`] so cross-mesh callers are mapped via the
//! trust+grants substrate (plan §3).
//!
//! The combinator is the cosmix-feature half of plan §3. It exists
//! because SPEC-12 §7.2 `AuthPolicy::resolve` is the pure mapping
//! point from `PeerIdentity` to `CapabilitySet`; cross-mesh callers
//! arrive with `signed_ident = "mesh:<fqdn>"` (plan §3) and need to
//! be authorised via the [`TrustGrantsCache`] rather than against
//! local unix/wg identity.
//!
//! Non-cross-mesh callers (`signed_ident` absent, or present but not
//! `mesh:`-prefixed) fall through to the base policy. Cross-mesh
//! callers whose trust record, grants, or exposable allowlist
//! doesn't authorise them get an empty [`CapabilitySet`] — they do
//! **not** fall through, because presenting a `mesh:` claim is a
//! cross-mesh assertion that should be authorised by cross-mesh
//! machinery alone.
//!
//! `resolve` does no IO and no async work: every read hits the
//! synchronous in-memory snapshot owned by [`TrustGrantsCache`].

use chrono::Utc;
use cosmix_props::{AuthPolicy, Capability, CapabilitySet};

use crate::cache::TrustGrantsCacheHandle;
use crate::caps::resolve_cross_mesh_caps;

/// Wrap a base [`AuthPolicy`] with cross-mesh grant resolution.
///
/// `namespace_id` is the SPEC-12 fully-qualified namespace identifier
/// (e.g. `"maild.accounts"`) used to look up the exposable cap
/// allowlist. `cache` is a clone-cheap handle to the per-process
/// [`crate::cache::TrustGrantsCache`].
///
/// The returned policy is constructed via [`AuthPolicy::new`] per
/// SPEC-12 v0.2 §7.2 — the closure captures `base`, `namespace_id`,
/// and `cache` through `Arc`s internal to those types.
pub fn with_cross_mesh_grants(
    base: AuthPolicy,
    namespace_id: impl Into<String>,
    cache: TrustGrantsCacheHandle,
) -> AuthPolicy {
    let namespace_id: String = namespace_id.into();
    AuthPolicy::new(move |peer| {
        // Plan §3: only `signed_ident = "mesh:<fqdn>"` callers go
        // through the cross-mesh path. Empty / non-mesh-prefixed
        // identifiers fall through to the base policy — which
        // handles unix-socket, WireGuard, and local-broker peers.
        let Some(ident) = peer.signed_ident.as_deref() else {
            return base.resolve(peer);
        };
        if !ident.starts_with("mesh:") {
            return base.resolve(peer);
        }

        // From here, the caller asserted a cross-mesh identity. The
        // resolver returns an empty CapabilitySet on every failure
        // mode (no trust, stale, no grants, exposable mismatch); we
        // do NOT fall through to the base policy, because the base
        // policy answers questions about local transport identity —
        // not cross-mesh claims.
        let core_caps = resolve_cross_mesh_caps(&cache, ident, namespace_id.as_str(), Utc::now());
        core_caps
            .iter()
            // Core grants are opaque strings; malformed empty grants confer
            // no capability and must not panic or fall back to the base policy.
            .filter_map(|c| Capability::new(c.as_str()).ok())
            .collect::<CapabilitySet>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Snapshot, TrustGrantsCache};
    use crate::caps::{Cap, CapabilitySet as CoreCapSet, Grant, TrustedMesh};
    use chrono::Duration;
    use cosmix_props::PeerIdentity;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn now() -> chrono::DateTime<chrono::Utc> {
        // Note: combinator uses real `Utc::now()` internally. Tests
        // therefore build trust/grant records using the current wall
        // clock as the anchor, with generous windows so freshness
        // and grant-window checks always pass for a healthy fixture.
        Utc::now()
    }

    fn trusted(fqdn: &str) -> TrustedMesh {
        TrustedMesh {
            mesh_fqdn: fqdn.into(),
            current_pubkey: vec![0u8; 32],
            current_selector: "2026q3".into(),
            previous_pubkey: None,
            previous_selector: None,
            previous_pubkey_valid_until: None,
            last_verified_at: now(),
            max_pubkey_age_secs: 365 * 86400,
            enabled: true,
        }
    }

    fn grant(fqdn: &str, caps: &[&str]) -> Grant {
        Grant {
            grant_id: format!("grant-for-{fqdn}"),
            mesh_fqdn: fqdn.into(),
            capabilities: caps.iter().map(|s| Cap::new(*s)).collect(),
            valid_from: now() - Duration::days(1),
            valid_until: now() + Duration::days(365),
            enabled: true,
        }
    }

    fn core_caps(tokens: &[&str]) -> CoreCapSet {
        tokens.iter().map(|s| Cap::new(*s)).collect()
    }

    /// Build a base AuthPolicy that records how many times it was
    /// called and what it returns. Used to verify fall-through.
    fn counting_base(returns: CapabilitySet) -> (AuthPolicy, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        let policy = AuthPolicy::new(move |_peer| {
            count_clone.fetch_add(1, Ordering::SeqCst);
            returns.clone()
        });
        (policy, count)
    }

    fn peer_with(signed: Option<&str>) -> PeerIdentity {
        PeerIdentity {
            unix_uid: None,
            unix_gid: None,
            unix_groups: vec![],
            wg_peer: None,
            signed_ident: signed.map(str::to_owned),
            service_name: None,
        }
    }

    #[test]
    fn non_cross_mesh_peer_falls_through_to_base() {
        // signed_ident=None → base policy handles it.
        let base_caps: CapabilitySet =
            [Capability::new("props.read:maild.accounts").expect("non-empty capability")]
                .into_iter()
                .collect();
        let (base, count) = counting_base(base_caps.clone());

        let cache = TrustGrantsCache::for_testing(Snapshot::default());
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());

        let resolved = wrapped.resolve(&peer_with(None));
        assert_eq!(resolved, base_caps);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn non_mesh_signed_ident_falls_through_to_base() {
        // signed_ident = "wg:..." or similar → base handles it.
        let base_caps: CapabilitySet =
            [Capability::new("props.read:maild.accounts").expect("non-empty capability")]
                .into_iter()
                .collect();
        let (base, count) = counting_base(base_caps.clone());

        let cache = TrustGrantsCache::for_testing(Snapshot::default());
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());

        let resolved = wrapped.resolve(&peer_with(Some("wg:192.0.2.4")));
        assert_eq!(resolved, base_caps);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mesh_peer_with_unknown_fqdn_returns_empty_no_fallthrough() {
        // signed_ident = "mesh:..." but the mesh isn't trusted. The
        // contract is empty CapabilitySet, NOT fall-through — a
        // cross-mesh assertion is answered by the cross-mesh layer
        // alone, never by base.
        let base_caps: CapabilitySet =
            [Capability::new("props.read:maild.accounts").expect("non-empty capability")]
                .into_iter()
                .collect();
        let (base, count) = counting_base(base_caps);

        let cache = TrustGrantsCache::for_testing(Snapshot::default());
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());

        let resolved = wrapped.resolve(&peer_with(Some("mesh:unknown.example")));
        assert!(resolved.is_empty());
        assert_eq!(count.load(Ordering::SeqCst), 0, "base must NOT be called");
    }

    #[test]
    fn empty_cross_mesh_grant_confers_no_capability() {
        let (base, count) = counting_base(CapabilitySet::empty());
        let cache = TrustGrantsCache::for_testing(Snapshot::from_parts(
            [trusted("node.example")],
            [grant("node.example", &["", "props.read:maild.accounts"])],
            [(
                "maild.accounts".into(),
                core_caps(&["", "props.read:maild.accounts"]),
            )],
        ));
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());
        let resolved = wrapped.resolve(&peer_with(Some("mesh:node.example")));
        assert_eq!(
            resolved.iter().map(Capability::as_str).collect::<Vec<_>>(),
            ["props.read:maild.accounts"]
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mesh_peer_with_grant_returns_intersected_caps() {
        let (base, _) = counting_base(CapabilitySet::empty());

        let cache = TrustGrantsCache::for_testing(Snapshot::from_parts(
            [trusted("channie.net")],
            [grant(
                "channie.net",
                &[
                    "props.read:maild.accounts",
                    "props.write:maild.accounts", // exists in grant, not in exposable
                ],
            )],
            [(
                "maild.accounts".into(),
                core_caps(&["props.read:maild.accounts"]),
            )],
        ));
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());

        let resolved = wrapped.resolve(&peer_with(Some("mesh:channie.net")));
        assert_eq!(resolved.iter().count(), 1);
        assert!(resolved.contains(
            &Capability::new("props.read:maild.accounts").expect("non-empty capability")
        ));
        assert!(!resolved.contains(
            &Capability::new("props.write:maild.accounts").expect("non-empty capability")
        ));
    }

    #[test]
    fn mesh_peer_with_disabled_trust_returns_empty() {
        let (base, _) = counting_base(CapabilitySet::empty());

        let mut t = trusted("channie.net");
        t.enabled = false;
        let cache = TrustGrantsCache::for_testing(Snapshot::from_parts(
            [t],
            [grant("channie.net", &["props.read:maild.accounts"])],
            [(
                "maild.accounts".into(),
                core_caps(&["props.read:maild.accounts"]),
            )],
        ));
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());
        let resolved = wrapped.resolve(&peer_with(Some("mesh:channie.net")));
        assert!(resolved.is_empty());
    }

    #[test]
    fn mesh_peer_with_no_exposable_for_namespace_returns_empty() {
        // Plan §5: exposable allowlist is the final gate. A grant
        // for a namespace the receiving service never registered as
        // exposable yields empty.
        let (base, _) = counting_base(CapabilitySet::empty());

        let cache = TrustGrantsCache::for_testing(Snapshot::from_parts(
            [trusted("channie.net")],
            [grant("channie.net", &["props.read:maild.accounts"])],
            // no exposable entry registered
            std::iter::empty::<(String, CoreCapSet)>(),
        ));
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());
        let resolved = wrapped.resolve(&peer_with(Some("mesh:channie.net")));
        assert!(resolved.is_empty());
    }

    #[test]
    fn empty_mesh_prefix_treated_as_cross_mesh_unknown_not_fallthrough() {
        // "mesh:" alone (empty fqdn) is a cross-mesh assertion for
        // an empty FQDN; the cache won't have a record, so empty
        // CapabilitySet is returned and base is NOT called.
        let base_caps: CapabilitySet =
            [Capability::new("props.read:maild.accounts").expect("non-empty capability")]
                .into_iter()
                .collect();
        let (base, count) = counting_base(base_caps);
        let cache = TrustGrantsCache::for_testing(Snapshot::default());
        let wrapped = with_cross_mesh_grants(base, "maild.accounts", cache.handle());
        let resolved = wrapped.resolve(&peer_with(Some("mesh:")));
        assert!(resolved.is_empty());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn factory_namespace_id_scopes_exposable_lookup() {
        // Same cache, two wrapped policies for different namespaces;
        // each sees only its own exposable allowlist.
        let cache = TrustGrantsCache::for_testing(Snapshot::from_parts(
            [trusted("channie.net")],
            [grant(
                "channie.net",
                &[
                    "props.read:maild.accounts",
                    "props.read:maild.engine_config",
                ],
            )],
            [
                (
                    "maild.accounts".into(),
                    core_caps(&["props.read:maild.accounts"]),
                ),
                (
                    "maild.engine_config".into(),
                    core_caps(&["props.read:maild.engine_config"]),
                ),
            ],
        ));

        let p_accounts =
            with_cross_mesh_grants(AuthPolicy::deny_all(), "maild.accounts", cache.handle());
        let p_engine = with_cross_mesh_grants(
            AuthPolicy::deny_all(),
            "maild.engine_config",
            cache.handle(),
        );

        let peer = peer_with(Some("mesh:channie.net"));
        let r_accounts = p_accounts.resolve(&peer);
        let r_engine = p_engine.resolve(&peer);

        assert!(r_accounts.contains(
            &Capability::new("props.read:maild.accounts").expect("non-empty capability")
        ));
        assert!(!r_accounts.contains(
            &Capability::new("props.read:maild.engine_config").expect("non-empty capability")
        ));
        assert!(r_engine.contains(
            &Capability::new("props.read:maild.engine_config").expect("non-empty capability")
        ));
        assert!(!r_engine.contains(
            &Capability::new("props.read:maild.accounts").expect("non-empty capability")
        ));
    }
}
