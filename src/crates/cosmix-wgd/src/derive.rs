//! Derive the **intended** WireGuard peer set from a verified signed inventory.
//!
//! This is the load-bearing, PURE core of P2 (no IO, no clock, no netlink):
//! given the inventory payload (mesh, subnet, epoch, opaque `members` JSON) and
//! this node's own mesh name, it produces the [`IntendedPeerSet`] — the peers
//! this node's kernel *should* hold — or a hard [`DeriveError`] if the
//! membership data is structurally unsound.
//!
//! ## SPEC-13 posture
//!
//! Membership is authored **only** in the signed inventory (INV-1/INV-5,
//! §12a). wgd never authors it back; it derives config *from* it. So this
//! module reads `members` and emits an intended set — it has no write path.
//! Fields the inventory does not author — endpoint, persistent-keepalive,
//! preshared-key — are **not modelled here** and are therefore **not drift**
//! (a P2 dry-run reporting endpoint drift on data the inventory cannot carry
//! would be a false positive; see [`crate::reconcile`]).
//!
//! ## Hard-error posture (Codex pre-impl review, 2026-07-06)
//!
//! A verified-but-incoherent inventory must not silently yield a corrupt peer
//! set, so the structural/security anomalies are **hard errors**: malformed
//! `members`, a member with a missing/bad `mesh_ip`, a `mesh_ip` outside the
//! mesh subnet, two active members sharing a `mesh_ip`, two active members
//! sharing a WG pubkey, self absent, or self appearing twice. A member whose
//! `kind:"wg"` credential window has simply lapsed is a **non-fatal warning**
//! (recorded in [`IntendedPeerSet::warnings`]) rather than a hard error — one
//! member's key gap must not take down the whole peer set.
//!
//! ## Rotation overlap (§6.1) is NOT an error
//!
//! During a WG-key rotation a member legitimately carries two current
//! `kind:"wg"` credentials whose epoch windows overlap (§6.1) — both are valid.
//! We model that as [`IntendedPeer::acceptable_pubkeys`] (normally one, two in
//! overlap): the kernel installs one of them at a time (kernel rotation is
//! atomic — plan §11.1), and the reconciler treats **either** installed key as
//! non-drift. (This is a deliberate divergence from the Codex note suggesting
//! "overlapping credential epochs = hard error": SPEC-13 §6.1 makes overlap the
//! normal rotation mechanism, so the governing spec wins — see the review
//! journal.)

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use cosmix_mesh_trust::admission::select_wg_pubkeys;
use cosmix_wg::{Cidr, WgPublicKey, parse_cidr};
use serde_json::Value;

/// One member's intended presence as a WG peer in this node's kernel.
#[derive(Debug, Clone)]
pub struct IntendedPeer {
    /// The member's immutable mesh name (§6.1).
    pub name: String,
    /// The member's mesh address — the stable identity across a key rotation
    /// and the join key the reconciler diffs on.
    pub mesh_ip: IpAddr,
    /// The peer's host route: `mesh_ip/32` (v4) or `/128` (v6).
    pub allowed_ip: Cidr,
    /// Every currently-valid `kind:"wg"` key for this member at the accepted
    /// epoch. Normally one; **two** during a §6.1 rotation overlap, in which
    /// case EITHER key installed in the kernel is accepted (not drift).
    /// Never empty (an empty selection means the member is skipped + warned).
    pub acceptable_pubkeys: Vec<WgPublicKey>,
}

/// A non-fatal anomaly encountered while deriving — surfaced in logs / the
/// drift report but not fatal to the whole derive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeriveWarning {
    /// An active member with no current `kind:"wg"` credential at the epoch —
    /// skipped from the peer set. (Self having no current wg key is also this,
    /// not a hard error: P2 does not consume self's wg key.)
    MemberNoCurrentWgKey { name: String },
}

/// The peers this node's kernel should hold, derived from one verified
/// inventory snapshot at a fixed epoch.
#[derive(Debug, Clone)]
pub struct IntendedPeerSet {
    pub mesh: String,
    pub subnet: Cidr,
    pub epoch: u64,
    pub self_name: String,
    pub self_mesh_ip: IpAddr,
    /// Every OTHER active member with a current wg credential — this node's
    /// intended kernel peer set. Sorted by `mesh_ip` for deterministic output.
    pub peers: Vec<IntendedPeer>,
    /// Non-fatal anomalies (see [`DeriveWarning`]).
    pub warnings: Vec<DeriveWarning>,
}

/// A structural/security defect that makes the membership data unsafe to derive
/// a peer set from. Hard: the caller must refuse to reconcile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeriveError {
    #[error("inventory `members` is not a JSON array")]
    MalformedMembers,
    #[error("inventory subnet `{0}` is not a valid CIDR")]
    BadSubnet(String),
    #[error("member #{index} has no string `name`")]
    MemberMissingName { index: usize },
    #[error("member {name:?} has no string `status`")]
    MemberMissingStatus { name: String },
    #[error(
        "member {name:?} has unknown status {status:?} (expected \"active\" or \"tombstoned\")"
    )]
    UnknownStatus { name: String, status: String },
    #[error("the name {name:?} is used by two active members")]
    DuplicateActiveName { name: String },
    #[error("member {name:?} has no string `mesh_ip`")]
    MemberMissingMeshIp { name: String },
    #[error("member {name:?} mesh_ip {value:?} is not a valid IP address")]
    BadMeshIp { name: String, value: String },
    #[error("member {name:?} mesh_ip {ip} is outside the mesh subnet {subnet}")]
    MeshIpOutsideSubnet {
        name: String,
        ip: IpAddr,
        subnet: String,
    },
    #[error("mesh_ip {ip} is claimed by two active members ({first:?} and {second:?})")]
    DuplicateMeshIp {
        ip: IpAddr,
        first: String,
        second: String,
    },
    #[error("the same WireGuard pubkey is used by two active members ({first:?} and {second:?})")]
    DuplicateWgPubkey { first: String, second: String },
    #[error("this node ({self_name:?}) is not an active member of the inventory")]
    SelfNotFound { self_name: String },
}

/// The member `status` values (§6.1). `active` members are peers; `tombstoned`
/// names are retired and skipped. Any OTHER value is malformed membership and
/// fails the derive (Codex 2026-07-06) — an unknown status must not be silently
/// treated as a tombstone (which could hide a malformed active peer, or turn a
/// malformed self into a misleading `SelfNotFound`).
const STATUS_ACTIVE: &str = "active";
const STATUS_TOMBSTONED: &str = "tombstoned";

/// Build the intended peer set from a verified inventory payload.
///
/// `members` is the opaque `SignedInventory::payload.members` — the caller MUST
/// have `verify()`-ed the inventory first (a stale/rolled-back cache would
/// otherwise feed bad peer config). `epoch` is the accepted epoch; credential
/// windows are evaluated against it (clock-free, §7.5/§16a).
pub fn derive_intended(
    mesh: &str,
    subnet: &str,
    epoch: u64,
    members: &Value,
    self_name: &str,
) -> Result<IntendedPeerSet, DeriveError> {
    let subnet_cidr = parse_cidr(subnet).map_err(|_| DeriveError::BadSubnet(subnet.to_string()))?;
    let members = members.as_array().ok_or(DeriveError::MalformedMembers)?;

    // Cross-member uniqueness guards (Codex risk 1). Keyed maps carry the
    // first-seen owner so a collision names both members.
    let mut by_mesh_ip: BTreeMap<IpAddr, String> = BTreeMap::new();
    let mut pubkey_owner: BTreeMap<[u8; 32], String> = BTreeMap::new();
    // Active member names must be unique (§6.1 — names are never reused). Guard
    // it before deriving: two active records sharing a name make peer
    // status/topology ambiguous AND would defeat the same-owner exemption in
    // the duplicate-pubkey guard below (Codex 2026-07-06).
    let mut seen_names: BTreeSet<String> = BTreeSet::new();

    let mut peers: Vec<IntendedPeer> = Vec::new();
    let mut warnings: Vec<DeriveWarning> = Vec::new();
    let mut self_mesh_ip: Option<IpAddr> = None;

    for (index, member) in members.iter().enumerate() {
        let name = member
            .get("name")
            .and_then(Value::as_str)
            .ok_or(DeriveError::MemberMissingName { index })?
            .to_string();

        // Status must be present + a known value. `tombstoned` → skip (retired);
        // `active` → derive; anything else (incl. missing/non-string) fails
        // closed rather than being silently skipped as if tombstoned.
        match member.get("status").and_then(Value::as_str) {
            Some(STATUS_ACTIVE) => {}
            Some(STATUS_TOMBSTONED) => continue,
            Some(other) => {
                return Err(DeriveError::UnknownStatus {
                    name,
                    status: other.to_string(),
                });
            }
            None => return Err(DeriveError::MemberMissingStatus { name }),
        }

        if !seen_names.insert(name.clone()) {
            return Err(DeriveError::DuplicateActiveName { name });
        }

        // mesh_ip: present, parseable, and inside the mesh subnet.
        let mesh_ip_str = member
            .get("mesh_ip")
            .and_then(Value::as_str)
            .ok_or_else(|| DeriveError::MemberMissingMeshIp { name: name.clone() })?;
        let mesh_ip: IpAddr = mesh_ip_str.parse().map_err(|_| DeriveError::BadMeshIp {
            name: name.clone(),
            value: mesh_ip_str.to_string(),
        })?;
        if !subnet_cidr.contains(&mesh_ip) {
            return Err(DeriveError::MeshIpOutsideSubnet {
                name: name.clone(),
                ip: mesh_ip,
                subnet: subnet.to_string(),
            });
        }
        if let Some(first) = by_mesh_ip.get(&mesh_ip) {
            return Err(DeriveError::DuplicateMeshIp {
                ip: mesh_ip,
                first: first.clone(),
                second: name,
            });
        }
        by_mesh_ip.insert(mesh_ip, name.clone());

        // Current wg credentials (0, 1, or 2-during-overlap). Each raw 32-byte
        // key becomes a typed WgPublicKey; register it against its owner so a
        // pubkey shared by two DISTINCT members is caught (a member's own two
        // rotation keys share `name`, so they don't trip the guard).
        let raw_keys = select_wg_pubkeys(member, epoch);
        let mut acceptable = Vec::with_capacity(raw_keys.len());
        for raw in raw_keys {
            let bytes: [u8; 32] = match raw.try_into() {
                Ok(b) => b,
                // select_wg_pubkeys already guarantees 32 bytes; defensive.
                Err(_) => continue,
            };
            if let Some(owner) = pubkey_owner.get(&bytes) {
                if owner != &name {
                    return Err(DeriveError::DuplicateWgPubkey {
                        first: owner.clone(),
                        second: name,
                    });
                }
            } else {
                pubkey_owner.insert(bytes, name.clone());
            }
            acceptable.push(WgPublicKey::from_bytes(bytes));
        }

        let is_self = name == self_name;
        if is_self {
            // A second self is impossible here: two members named `self_name`
            // would already have tripped `DuplicateActiveName` above.
            self_mesh_ip = Some(mesh_ip);
            // self is the interface, not a peer of itself — never added to
            // `peers`. Its wg key is not consumed by P2 (that is the interface
            // private key's job in P3), so a self with no current wg key is a
            // warning, not a hard error.
            if acceptable.is_empty() {
                warnings.push(DeriveWarning::MemberNoCurrentWgKey { name });
            }
            continue;
        }

        if acceptable.is_empty() {
            // An active peer with no current wg key can't be installed — skip
            // it but record the anomaly rather than nuking the whole set.
            warnings.push(DeriveWarning::MemberNoCurrentWgKey { name });
            continue;
        }

        let allowed_ip = host_route(mesh_ip);
        peers.push(IntendedPeer {
            name,
            mesh_ip,
            allowed_ip,
            acceptable_pubkeys: acceptable,
        });
    }

    let self_mesh_ip = self_mesh_ip.ok_or_else(|| DeriveError::SelfNotFound {
        self_name: self_name.to_string(),
    })?;

    // Deterministic order (the drift report + status verbs must not be
    // order-noisy across reconciles).
    peers.sort_by_key(|p| p.mesh_ip);

    Ok(IntendedPeerSet {
        mesh: mesh.to_string(),
        subnet: subnet_cidr,
        epoch,
        self_name: self_name.to_string(),
        self_mesh_ip,
        peers,
        warnings,
    })
}

/// The single-host CIDR for a mesh address: `/32` for v4, `/128` for v6.
fn host_route(ip: IpAddr) -> Cidr {
    let prefix_len = match ip {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    Cidr {
        network: ip,
        prefix_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    use serde_json::json;

    fn wgkey(seed: u8) -> String {
        B64.encode([seed; 32])
    }

    fn member(name: &str, ip: &str, status: &str, creds: Value) -> Value {
        json!({ "name": name, "mesh_ip": ip, "bus": true, "status": status, "credentials": creds })
    }

    fn wg_cred(seed: u8, from: u64, until: Value) -> Value {
        json!({ "kind": "wg", "pubkey": wgkey(seed), "from_epoch": from, "until_epoch": until })
    }

    fn inv(members: Vec<Value>) -> Value {
        Value::Array(members)
    }

    #[test]
    fn derives_peers_excluding_self_sorted_by_mesh_ip() {
        let members = inv(vec![
            member(
                "gamma",
                "192.0.2.9",
                "active",
                json!([wg_cred(3, 1, Value::Null)]),
            ),
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            member(
                "beta",
                "192.0.2.5",
                "active",
                json!([wg_cred(2, 1, Value::Null)]),
            ),
        ]);
        let set = derive_intended("bus", "192.0.2.0/24", 7, &members, "alpha").unwrap();
        assert_eq!(set.self_mesh_ip, "192.0.2.2".parse::<IpAddr>().unwrap());
        // self excluded; the other two, sorted by mesh_ip.
        let names: Vec<_> = set.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["beta", "gamma"]);
        assert_eq!(set.peers[0].allowed_ip.prefix_len, 32);
        assert_eq!(set.peers[0].acceptable_pubkeys.len(), 1);
        assert!(set.warnings.is_empty());
    }

    #[test]
    fn tombstoned_members_are_skipped_not_errors() {
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            member(
                "old",
                "192.0.2.3",
                "tombstoned",
                json!([wg_cred(2, 1, Value::Null)]),
            ),
        ]);
        let set = derive_intended("bus", "192.0.2.0/24", 7, &members, "alpha").unwrap();
        assert!(set.peers.is_empty(), "the only other member is tombstoned");
    }

    #[test]
    fn rotation_overlap_yields_two_acceptable_keys() {
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            member(
                "beta",
                "192.0.2.5",
                "active",
                json!([
                    wg_cred(2, 1, json!(10)),   // outgoing [1,10)
                    wg_cred(9, 8, Value::Null), // incoming [8, open)
                ]),
            ),
        ]);
        let set = derive_intended("bus", "192.0.2.0/24", 9, &members, "alpha").unwrap();
        let beta = set.peers.iter().find(|p| p.name == "beta").unwrap();
        assert_eq!(
            beta.acceptable_pubkeys.len(),
            2,
            "overlap: both keys accepted"
        );
    }

    #[test]
    fn active_member_with_no_current_wg_key_is_a_warning_not_an_error() {
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            // beta's only wg cred is in the future (from_epoch 50).
            member(
                "beta",
                "192.0.2.5",
                "active",
                json!([wg_cred(2, 50, Value::Null)]),
            ),
        ]);
        let set = derive_intended("bus", "192.0.2.0/24", 7, &members, "alpha").unwrap();
        assert!(set.peers.is_empty(), "beta has no current key → skipped");
        assert_eq!(
            set.warnings,
            vec![DeriveWarning::MemberNoCurrentWgKey {
                name: "beta".into()
            }]
        );
    }

    #[test]
    fn duplicate_mesh_ip_is_a_hard_error() {
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            member(
                "beta",
                "192.0.2.2",
                "active",
                json!([wg_cred(2, 1, Value::Null)]),
            ),
        ]);
        let err = derive_intended("bus", "192.0.2.0/24", 7, &members, "alpha").unwrap_err();
        assert!(matches!(err, DeriveError::DuplicateMeshIp { .. }));
    }

    #[test]
    fn duplicate_wg_pubkey_across_members_is_a_hard_error() {
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(7, 1, Value::Null)]),
            ),
            member(
                "beta",
                "192.0.2.5",
                "active",
                json!([wg_cred(7, 1, Value::Null)]),
            ),
        ]);
        let err = derive_intended("bus", "192.0.2.0/24", 7, &members, "alpha").unwrap_err();
        assert!(matches!(err, DeriveError::DuplicateWgPubkey { .. }));
    }

    #[test]
    fn mesh_ip_outside_subnet_is_a_hard_error() {
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            member(
                "beta",
                "10.9.9.9",
                "active",
                json!([wg_cred(2, 1, Value::Null)]),
            ),
        ]);
        let err = derive_intended("bus", "192.0.2.0/24", 7, &members, "alpha").unwrap_err();
        assert!(matches!(err, DeriveError::MeshIpOutsideSubnet { .. }));
    }

    #[test]
    fn self_absent_is_a_hard_error() {
        let members = inv(vec![member(
            "alpha",
            "192.0.2.2",
            "active",
            json!([wg_cred(1, 1, Value::Null)]),
        )]);
        let err = derive_intended("bus", "192.0.2.0/24", 7, &members, "nobody").unwrap_err();
        assert!(matches!(err, DeriveError::SelfNotFound { .. }));
    }

    #[test]
    fn duplicate_active_name_is_a_hard_error() {
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            // same name, different mesh_ip + key → still a hard error (names
            // are unique among active members).
            member(
                "alpha",
                "192.0.2.5",
                "active",
                json!([wg_cred(2, 1, Value::Null)]),
            ),
        ]);
        let err = derive_intended("bus", "192.0.2.0/24", 7, &members, "alpha").unwrap_err();
        assert!(matches!(err, DeriveError::DuplicateActiveName { .. }));
    }

    #[test]
    fn unknown_or_missing_status_is_a_hard_error_not_a_silent_skip() {
        // Unknown status → error (must not be silently treated as tombstoned).
        let unknown = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            member(
                "weird",
                "192.0.2.5",
                "paused",
                json!([wg_cred(2, 1, Value::Null)]),
            ),
        ]);
        assert!(matches!(
            derive_intended("bus", "192.0.2.0/24", 7, &unknown, "alpha").unwrap_err(),
            DeriveError::UnknownStatus { .. }
        ));
        // Missing status → error (a member with no status is malformed).
        let missing = Value::Array(vec![json!({
            "name": "alpha", "mesh_ip": "192.0.2.2", "bus": true,
            "credentials": [wg_cred(1, 1, Value::Null)],
        })]);
        assert!(matches!(
            derive_intended("bus", "192.0.2.0/24", 7, &missing, "alpha").unwrap_err(),
            DeriveError::MemberMissingStatus { .. }
        ));
    }

    #[test]
    fn malformed_members_and_bad_subnet_are_hard_errors() {
        assert!(matches!(
            derive_intended("bus", "192.0.2.0/24", 7, &json!({}), "alpha").unwrap_err(),
            DeriveError::MalformedMembers
        ));
        assert!(matches!(
            derive_intended("bus", "not-a-cidr", 7, &json!([]), "alpha").unwrap_err(),
            DeriveError::BadSubnet(_)
        ));
    }

    #[test]
    fn a_members_own_two_rotation_keys_do_not_trip_the_dup_pubkey_guard() {
        // same seed reused within ONE member across a rotation window is fine.
        let members = inv(vec![
            member(
                "alpha",
                "192.0.2.2",
                "active",
                json!([wg_cred(1, 1, Value::Null)]),
            ),
            member(
                "beta",
                "192.0.2.5",
                "active",
                json!([wg_cred(2, 1, json!(10)), wg_cred(2, 8, Value::Null)]),
            ),
        ]);
        // beta's two creds share seed 2 but belong to beta — not a cross-member dup.
        let set = derive_intended("bus", "192.0.2.0/24", 9, &members, "alpha").unwrap();
        let beta = set.peers.iter().find(|p| p.name == "beta").unwrap();
        assert_eq!(beta.acceptable_pubkeys.len(), 2);
    }
}
