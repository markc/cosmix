//! Capability tokens + trust/grant fixtures + cross-mesh
//! capability-resolution algorithm (plan §3).
//!
//! # Wildcards
//!
//! v1 treats SPEC-12 §7.1 capability tokens as *opaque strings*.
//! Intersection is plain string-set intersection. Wildcards (`*` in
//! the `<svc>.<ns>` slot) are **not** expanded by this layer — a
//! grant of `props.audit:*` and an exposable of
//! `props.audit:maild.accounts` intersect to the empty set, not to
//! the specific token.
//!
//! This is the fail-safe v1 choice: a misconfigured wildcard
//! intersects to nothing rather than accidentally over-granting.
//! Operators register specific tokens on both sides. Wildcard
//! expansion is a v2 concern.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A SPEC-12 §7.1 capability token: `props.<action>:<svc>.<ns>[:<scope>]`.
///
/// Wrapped in a newtype so the type system distinguishes capability
/// tokens from arbitrary strings at call sites. Parsing /
/// validation of the token shape is intentionally not enforced here
/// — the SPEC-12 layer that produces these tokens is authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cap(pub String);

impl Cap {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Cap {
    fn from(s: &str) -> Self {
        Cap(s.to_owned())
    }
}

impl From<String> for Cap {
    fn from(s: String) -> Self {
        Cap(s)
    }
}

/// A set of [`Cap`] tokens. `BTreeSet`-backed so iteration order and
/// canonical serialisation are deterministic across processes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet {
    caps: BTreeSet<Cap>,
}

impl CapabilitySet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, c: Cap) -> bool {
        self.caps.insert(c)
    }

    pub fn contains(&self, c: &Cap) -> bool {
        self.caps.contains(c)
    }

    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }

    pub fn len(&self) -> usize {
        self.caps.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Cap> {
        self.caps.iter()
    }

    pub fn union(&self, other: &Self) -> Self {
        Self {
            caps: self.caps.union(&other.caps).cloned().collect(),
        }
    }

    pub fn intersect(&self, other: &Self) -> Self {
        Self {
            caps: self.caps.intersection(&other.caps).cloned().collect(),
        }
    }
}

impl FromIterator<Cap> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Cap>>(it: I) -> Self {
        Self {
            caps: it.into_iter().collect(),
        }
    }
}

/// Fixture mirror of the `wgd.trusted_meshes` namespace record
/// (plan §4). Field types match the substrate schema; RFC 3339
/// timestamps are pre-parsed to [`DateTime<Utc>`] for in-process
/// use.
#[derive(Debug, Clone)]
pub struct TrustedMesh {
    pub mesh_fqdn: String,
    pub current_pubkey: Vec<u8>,
    pub current_selector: String,
    pub previous_pubkey: Option<Vec<u8>>,
    pub previous_selector: Option<String>,
    pub previous_pubkey_valid_until: Option<DateTime<Utc>>,
    pub last_verified_at: DateTime<Utc>,
    pub max_pubkey_age_secs: u64,
    pub enabled: bool,
}

/// Fixture mirror of the `wgd.grants` namespace record (plan §5).
#[derive(Debug, Clone)]
pub struct Grant {
    pub grant_id: String,
    pub mesh_fqdn: String,
    pub capabilities: CapabilitySet,
    pub valid_from: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub enabled: bool,
}

/// Read-only trust-store surface used by [`resolve_cross_mesh_caps`].
///
/// The cosmix-feature combinator (P0-C4+) implements this against
/// the live `TrustGrantsCache`. Tests implement it against
/// hand-rolled fixture data.
pub trait TrustStore {
    fn trusted_mesh(&self, fqdn: &str) -> Option<TrustedMesh>;

    /// All grants whose `mesh_fqdn` equals `fqdn`, regardless of
    /// `enabled` or window. The resolver applies its own filters so
    /// the implementation can be a dumb mirror of the substrate row
    /// set.
    fn grants_for_mesh(&self, fqdn: &str) -> Vec<Grant>;

    /// The `wgd.cross_mesh_exposable` allowlist for the given
    /// SPEC-12 namespace id (e.g. `"maild.accounts"`).
    fn exposable_for_namespace(&self, ns_id: &str) -> CapabilitySet;
}

/// Pure cross-mesh capability resolution per plan §3.
///
/// Returns the [`CapabilitySet`] a peer with `signed_ident` is
/// entitled to when calling into `namespace_id`. Empty set on any
/// failure: missing trust record, disabled mesh, stale trust,
/// non-mesh principal, no overlapping grants, etc. **No IO.**
///
/// `signed_ident` is the SPEC-12 §7.3 field on the synthesised
/// `PeerIdentity`; for cross-mesh callers it is
/// `"mesh:<caller_mesh>"` (plan §3). Other principals (unix-socket,
/// WireGuard, local broker) fall outside this resolver — the
/// combinator dispatches them to the base policy instead.
pub fn resolve_cross_mesh_caps(
    store: &dyn TrustStore,
    signed_ident: &str,
    namespace_id: &str,
    now: DateTime<Utc>,
) -> CapabilitySet {
    let Some(fqdn) = signed_ident.strip_prefix("mesh:") else {
        return CapabilitySet::empty();
    };
    let Some(trusted) = store.trusted_mesh(fqdn) else {
        return CapabilitySet::empty();
    };
    if !trusted.enabled {
        return CapabilitySet::empty();
    }

    // Plan §4 freshness gate: last_verified_at + max_pubkey_age_secs
    // < now → reject. `max_pubkey_age_secs` is operator-supplied, so
    // both the seconds→Duration conversion and the timestamp addition
    // must use checked APIs — a malicious / accidental u64::MAX (or
    // any value outside chrono::TimeDelta's range) must not panic.
    // Both forms of overflow fail closed.
    let clamped = trusted.max_pubkey_age_secs.min(i64::MAX as u64) as i64;
    let Some(max_age) = Duration::try_seconds(clamped) else {
        return CapabilitySet::empty();
    };
    match trusted.last_verified_at.checked_add_signed(max_age) {
        Some(t) if now <= t => {}
        _ => return CapabilitySet::empty(),
    }

    let mut granted_union = CapabilitySet::empty();
    for grant in store.grants_for_mesh(fqdn) {
        if !grant.enabled {
            continue;
        }
        if now < grant.valid_from || now > grant.valid_until {
            continue;
        }
        granted_union = granted_union.union(&grant.capabilities);
    }

    let exposable = store.exposable_for_namespace(namespace_id);
    granted_union.intersect(&exposable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn caps(tokens: &[&str]) -> CapabilitySet {
        tokens.iter().map(|s| Cap::new(*s)).collect()
    }

    #[test]
    fn capability_set_union_and_intersect() {
        let a = caps(&["props.read:maild.accounts", "props.write:maild.accounts"]);
        let b = caps(&[
            "props.read:maild.accounts",
            "props.read:maild.account_overrides",
        ]);

        let u = a.union(&b);
        assert_eq!(u.len(), 3);
        assert!(u.contains(&Cap::new("props.read:maild.accounts")));
        assert!(u.contains(&Cap::new("props.write:maild.accounts")));
        assert!(u.contains(&Cap::new("props.read:maild.account_overrides")));

        let i = a.intersect(&b);
        assert_eq!(i.len(), 1);
        assert!(i.contains(&Cap::new("props.read:maild.accounts")));
    }

    #[test]
    fn capability_set_with_empty() {
        let a = caps(&["props.read:maild.accounts"]);
        let e = CapabilitySet::empty();
        assert_eq!(a.union(&e).len(), 1);
        assert!(a.intersect(&e).is_empty());
    }

    #[test]
    fn wildcard_tokens_are_opaque_strings_v1() {
        // Plan §3 documents v1 intersection is plain string-set:
        // `props.audit:*` does not expand to specific tokens.
        let granted = caps(&["props.audit:*"]);
        let exposable = caps(&["props.audit:maild.accounts"]);
        assert!(granted.intersect(&exposable).is_empty());
    }

    // ---- Fixture builders ----

    fn trusted_mesh_ok(now: DateTime<Utc>) -> TrustedMesh {
        TrustedMesh {
            mesh_fqdn: "example.com".into(),
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

    fn grant_ok(now: DateTime<Utc>, capabilities: CapabilitySet) -> Grant {
        Grant {
            grant_id: "01HXEXAMPLE".into(),
            mesh_fqdn: "example.com".into(),
            capabilities,
            valid_from: now - Duration::days(1),
            valid_until: now + Duration::days(30),
            enabled: true,
        }
    }

    struct FixtureStore {
        trusted: Option<TrustedMesh>,
        grants: Vec<Grant>,
        exposable: std::collections::HashMap<String, CapabilitySet>,
    }

    impl TrustStore for FixtureStore {
        fn trusted_mesh(&self, fqdn: &str) -> Option<TrustedMesh> {
            match &self.trusted {
                Some(t) if t.mesh_fqdn == fqdn => Some(t.clone()),
                _ => None,
            }
        }
        fn grants_for_mesh(&self, fqdn: &str) -> Vec<Grant> {
            self.grants
                .iter()
                .filter(|g| g.mesh_fqdn == fqdn)
                .cloned()
                .collect()
        }
        fn exposable_for_namespace(&self, ns_id: &str) -> CapabilitySet {
            self.exposable.get(ns_id).cloned().unwrap_or_default()
        }
    }

    fn store_with(
        trusted: TrustedMesh,
        grants: Vec<Grant>,
        exposable: &[(&str, &[&str])],
    ) -> FixtureStore {
        FixtureStore {
            trusted: Some(trusted),
            grants,
            exposable: exposable
                .iter()
                .map(|(k, toks)| (k.to_string(), caps(toks)))
                .collect(),
        }
    }

    #[test]
    fn resolve_happy_path() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let store = store_with(
            trusted_mesh_ok(now),
            vec![grant_ok(now, caps(&["props.read:maild.accounts"]))],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert_eq!(resolved.len(), 1);
        assert!(resolved.contains(&Cap::new("props.read:maild.accounts")));
    }

    #[test]
    fn resolve_returns_empty_when_signed_ident_not_mesh_prefixed() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let store = store_with(
            trusted_mesh_ok(now),
            vec![grant_ok(now, caps(&["props.read:maild.accounts"]))],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved = resolve_cross_mesh_caps(&store, "unix:1000", "maild.accounts", now);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_returns_empty_when_mesh_unknown() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let store = store_with(
            trusted_mesh_ok(now),
            vec![grant_ok(now, caps(&["props.read:maild.accounts"]))],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved =
            resolve_cross_mesh_caps(&store, "mesh:unknown.example", "maild.accounts", now);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_returns_empty_when_mesh_disabled() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let mut trusted = trusted_mesh_ok(now);
        trusted.enabled = false;
        let store = store_with(
            trusted,
            vec![grant_ok(now, caps(&["props.read:maild.accounts"]))],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_returns_empty_when_trust_stale() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let mut trusted = trusted_mesh_ok(now);
        trusted.last_verified_at = now - Duration::days(30); // past max_pubkey_age_secs (7 days)
        let store = store_with(
            trusted,
            vec![grant_ok(now, caps(&["props.read:maild.accounts"]))],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_returns_empty_for_max_pubkey_age_overflow() {
        // u64::MAX seconds added to a timestamp must not panic; the
        // resolver treats overflow as stale (fail-closed).
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let mut trusted = trusted_mesh_ok(now);
        trusted.max_pubkey_age_secs = u64::MAX;
        // With i64::MAX seconds (post-saturation), chrono's
        // checked_add_signed should return None for many starting
        // timestamps. We don't assert the exact boundary; we assert
        // no panic and a fail-closed result on the path that does
        // overflow.
        trusted.last_verified_at = Utc.with_ymd_and_hms(9999, 1, 1, 0, 0, 0).unwrap();
        let store = store_with(
            trusted,
            vec![grant_ok(now, caps(&["props.read:maild.accounts"]))],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert!(resolved.is_empty());
    }

    #[test]
    fn disabled_grants_are_ignored() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let mut grant = grant_ok(now, caps(&["props.read:maild.accounts"]));
        grant.enabled = false;
        let store = store_with(
            trusted_mesh_ok(now),
            vec![grant],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert!(resolved.is_empty());
    }

    #[test]
    fn grants_outside_window_are_ignored() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

        // Not yet valid.
        let mut early = grant_ok(now, caps(&["props.read:maild.accounts"]));
        early.valid_from = now + Duration::days(1);
        let store = store_with(
            trusted_mesh_ok(now),
            vec![early],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        assert!(
            resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now).is_empty()
        );

        // Expired.
        let mut late = grant_ok(now, caps(&["props.read:maild.accounts"]));
        late.valid_until = now - Duration::days(1);
        let store = store_with(
            trusted_mesh_ok(now),
            vec![late],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        assert!(
            resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now).is_empty()
        );
    }

    #[test]
    fn multiple_grants_union_before_intersection() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let store = store_with(
            trusted_mesh_ok(now),
            vec![
                grant_ok(now, caps(&["props.read:maild.accounts"])),
                grant_ok(
                    now,
                    caps(&[
                        "props.write:maild.accounts",
                        "props.read:maild.engine_config",
                    ]),
                ),
            ],
            &[(
                "maild.accounts",
                &[
                    "props.read:maild.accounts",
                    "props.write:maild.accounts",
                    "props.read:maild.engine_config",
                ],
            )],
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert_eq!(resolved.len(), 3);
    }

    #[test]
    fn exposable_allowlist_narrows_grant() {
        // Plan §5: even a malformed grant listing capabilities outside
        // the exposable allowlist is neutered by the intersection.
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let store = store_with(
            trusted_mesh_ok(now),
            vec![grant_ok(
                now,
                caps(&[
                    "props.read:maild.accounts",
                    "props.write:wgd.peers", // not exposable
                ]),
            )],
            &[("maild.accounts", &["props.read:maild.accounts"])],
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert_eq!(resolved.len(), 1);
        assert!(resolved.contains(&Cap::new("props.read:maild.accounts")));
        assert!(!resolved.contains(&Cap::new("props.write:wgd.peers")));
    }

    #[test]
    fn empty_exposable_for_namespace_yields_empty_caps() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let store = store_with(
            trusted_mesh_ok(now),
            vec![grant_ok(now, caps(&["props.read:maild.accounts"]))],
            &[], // no exposable entry for the namespace
        );
        let resolved = resolve_cross_mesh_caps(&store, "mesh:example.com", "maild.accounts", now);
        assert!(resolved.is_empty());
    }
}
