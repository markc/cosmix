//! The **dry-run reconciler**: diff the [`crate::derive::IntendedPeerSet`]
//! against the live kernel peer set and produce a [`DriftReport`]. Pure (no IO,
//! `now_unix` is injected for deterministic liveness).
//!
//! ## Hard no-mutation boundary (Codex pre-impl review, 2026-07-06)
//!
//! P2 is dry-run: it detects drift and never converges it. This module
//! therefore **does not import** `cosmix_wg::wg_set_device_message` /
//! `SetDeviceParams` / `SetPeer` — the netlink SET path is not referenced
//! anywhere in P2. Apply-mode is an explicit P3 addition (a new module that
//! consumes a `DriftReport`), never a dormant verb reachable from the P2
//! dispatch. The only kernel interaction P2 has is the READ in
//! [`crate::live`] (`wg show <iface> dump`).
//!
//! ## What is (and is not) drift
//!
//! The join key is the member's **`mesh_ip`** (stable across a key rotation),
//! not the WG pubkey (which rotates). For each intended peer we ask: does the
//! kernel hold a peer routing that `mesh_ip`, and is its key one this member is
//! allowed to present right now (§6.1 overlap → a set)?
//!
//! Endpoint, persistent-keepalive and preshared-key are **not compared** — the
//! signed inventory does not author them ([`crate::derive`]), so wgd has no
//! intended value to diff against and reporting them would be a false positive
//! (Codex risk 3).

use std::collections::BTreeMap;
use std::net::IpAddr;

use cosmix_wg::{PeerDump, PeerStatus, WgPublicKey, WgShowDump, parse_cidr};

use crate::derive::IntendedPeerSet;

/// A peer that is present and correct in the kernel — carried for the live
/// status surface (`wgd.peer.status`), annotated with liveness.
#[derive(Debug, Clone)]
pub struct SyncedPeer {
    pub name: String,
    pub mesh_ip: IpAddr,
    pub public_key: WgPublicKey,
    pub status: PeerStatus,
}

/// One divergence between intent (the signed inventory) and the live kernel.
/// A P3 apply pass is what would converge each of these; P2 only reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftItem {
    /// Intended peer with no kernel peer routing its `mesh_ip` — P3 would ADD.
    Missing { name: String, mesh_ip: IpAddr },
    /// Kernel peer routing a `mesh_ip` no active member owns — P3 would REMOVE.
    Extra {
        public_key: WgPublicKey,
        allowed_ips: Vec<String>,
    },
    /// Kernel peer routes the right `mesh_ip` but with a key this member is not
    /// currently allowed to present — P3 would ROTATE the peer key.
    KeyMismatch {
        name: String,
        mesh_ip: IpAddr,
        live_pubkey: WgPublicKey,
    },
    /// Right member, right key, but the kernel's `allowed_ips` are not exactly
    /// the intended `{mesh_ip/32}` — P3 would re-set the allowed-ips.
    AllowedIpsDrift {
        name: String,
        mesh_ip: IpAddr,
        live_allowed_ips: Vec<String>,
    },
    /// More than one kernel peer claims the same intended `mesh_ip` — a kernel
    /// anomaly wgd cannot safely auto-resolve; surfaced for the operator.
    DuplicateKernelClaimant { mesh_ip: IpAddr, count: usize },
}

/// The result of one dry-run reconcile pass over a verified inventory snapshot.
#[derive(Debug, Clone)]
pub struct DriftReport {
    pub mesh: String,
    pub epoch: u64,
    /// Peers present + correct in the kernel (with liveness).
    pub synced: Vec<SyncedPeer>,
    /// Divergences P3 would converge; empty == in sync.
    pub drift: Vec<DriftItem>,
}

impl DriftReport {
    /// True iff the kernel matches intent (no divergences).
    pub fn is_clean(&self) -> bool {
        self.drift.is_empty()
    }
}

/// Diff intended vs live. `now_unix` dates liveness (`PeerStatus`).
///
/// This is the whole of P2's reconcile: build the report and return it. There
/// is intentionally no apply/converge step and no netlink SET construction.
pub fn reconcile(intended: &IntendedPeerSet, live: &WgShowDump, now_unix: u64) -> DriftReport {
    // Index intended peers by mesh_ip (the stable join key).
    let intended_by_ip: BTreeMap<IpAddr, &crate::derive::IntendedPeer> =
        intended.peers.iter().map(|p| (p.mesh_ip, p)).collect();

    // Which live peers route each intended mesh_ip (a well-formed kernel has
    // exactly one; 0 = missing, >1 = anomaly).
    let mut claimants: BTreeMap<IpAddr, Vec<&PeerDump>> = BTreeMap::new();
    // Live peers that route NO intended mesh_ip are "extra".
    let mut matched_live: Vec<bool> = vec![false; live.peers.len()];

    for (li, lp) in live.peers.iter().enumerate() {
        let host_ips = host_ips_of(lp);
        let mut matched_any = false;
        for ip in &host_ips {
            if intended_by_ip.contains_key(ip) {
                claimants.entry(*ip).or_default().push(lp);
                matched_any = true;
            }
        }
        matched_live[li] = matched_any;
    }

    let mut synced = Vec::new();
    let mut drift = Vec::new();

    // Walk intended peers in deterministic mesh_ip order.
    for (ip, ip_intended) in &intended_by_ip {
        match claimants.get(ip).map(|v| v.as_slice()) {
            None | Some([]) => drift.push(DriftItem::Missing {
                name: ip_intended.name.clone(),
                mesh_ip: *ip,
            }),
            Some([lp]) => {
                let live_key = lp.public_key;
                if !ip_intended.acceptable_pubkeys.contains(&live_key) {
                    drift.push(DriftItem::KeyMismatch {
                        name: ip_intended.name.clone(),
                        mesh_ip: *ip,
                        live_pubkey: live_key,
                    });
                } else if !allowed_ips_match(lp, ip) {
                    drift.push(DriftItem::AllowedIpsDrift {
                        name: ip_intended.name.clone(),
                        mesh_ip: *ip,
                        live_allowed_ips: lp.allowed_ips.clone(),
                    });
                } else {
                    synced.push(SyncedPeer {
                        name: ip_intended.name.clone(),
                        mesh_ip: *ip,
                        public_key: live_key,
                        status: PeerStatus::from_handshake(lp.latest_handshake_unix, now_unix),
                    });
                }
            }
            Some(multi) => drift.push(DriftItem::DuplicateKernelClaimant {
                mesh_ip: *ip,
                count: multi.len(),
            }),
        }
    }

    // Live peers matching no intended mesh_ip are extra kernel peers.
    for (li, lp) in live.peers.iter().enumerate() {
        if !matched_live[li] {
            drift.push(DriftItem::Extra {
                public_key: lp.public_key,
                allowed_ips: lp.allowed_ips.clone(),
            });
        }
    }

    DriftReport {
        mesh: intended.mesh.clone(),
        epoch: intended.epoch,
        synced,
        drift,
    }
}

/// The set of single-host addresses a live peer routes — the `/32` (v4) or
/// `/128` (v6) allowed-ips, as `IpAddr`. Non-host or unparseable allowed-ips
/// are ignored for the mesh_ip join (they surface as `AllowedIpsDrift` on a
/// matched peer, or leave a peer unmatched → `Extra`).
fn host_ips_of(peer: &PeerDump) -> Vec<IpAddr> {
    peer.allowed_ips
        .iter()
        .filter_map(|s| {
            let c = parse_cidr(s.trim()).ok()?;
            let is_host = matches!(
                (c.network, c.prefix_len),
                (IpAddr::V4(_), 32) | (IpAddr::V6(_), 128)
            );
            is_host.then_some(c.network)
        })
        .collect()
}

/// True iff the live peer's allowed-ips are exactly the intended single host
/// route `{mesh_ip/32}` — no extra routes, no missing route.
fn allowed_ips_match(peer: &PeerDump, mesh_ip: &IpAddr) -> bool {
    let hosts = host_ips_of(peer);
    peer.allowed_ips.len() == 1 && hosts.as_slice() == [*mesh_ip]
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    use cosmix_wg::{Cidr, WgInterfaceDump};

    use crate::derive::IntendedPeer;

    fn key(seed: u8) -> WgPublicKey {
        WgPublicKey::from_bytes([seed; 32])
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn intended(peers: Vec<IntendedPeer>) -> IntendedPeerSet {
        IntendedPeerSet {
            mesh: "bus".into(),
            subnet: parse_cidr("192.0.2.0/24").unwrap(),
            epoch: 7,
            self_name: "alpha".into(),
            self_mesh_ip: ip("192.0.2.2"),
            peers,
            warnings: vec![],
        }
    }

    fn ipeer(name: &str, mesh_ip: &str, keys: Vec<u8>) -> IntendedPeer {
        IntendedPeer {
            name: name.into(),
            mesh_ip: ip(mesh_ip),
            allowed_ip: Cidr {
                network: ip(mesh_ip),
                prefix_len: 32,
            },
            acceptable_pubkeys: keys.into_iter().map(key).collect(),
        }
    }

    fn live_peer(seed: u8, allowed: &[&str], handshake: u64) -> PeerDump {
        PeerDump {
            public_key: key(seed),
            has_preshared_key: false,
            endpoint: None,
            allowed_ips: allowed.iter().map(|s| s.to_string()).collect(),
            latest_handshake_unix: handshake,
            rx_bytes: 0,
            tx_bytes: 0,
            persistent_keepalive: None,
        }
    }

    fn dump(peers: Vec<PeerDump>) -> WgShowDump {
        WgShowDump {
            interface: WgInterfaceDump {
                public_key: WgPublicKey::from_base64(&B64.encode([0u8; 32])).unwrap(),
                listen_port: 51820,
                fwmark: None,
            },
            peers,
        }
    }

    #[test]
    fn clean_when_kernel_matches_intent() {
        let set = intended(vec![ipeer("beta", "192.0.2.5", vec![2])]);
        let live = dump(vec![live_peer(2, &["192.0.2.5/32"], 1_000)]);
        let report = reconcile(&set, &live, 1_050);
        assert!(report.is_clean(), "drift: {:?}", report.drift);
        assert_eq!(report.synced.len(), 1);
        assert_eq!(report.synced[0].status, PeerStatus::Connected);
    }

    #[test]
    fn missing_when_intended_peer_absent_from_kernel() {
        let set = intended(vec![ipeer("beta", "192.0.2.5", vec![2])]);
        let live = dump(vec![]);
        let report = reconcile(&set, &live, 1_050);
        assert_eq!(
            report.drift,
            vec![DriftItem::Missing {
                name: "beta".into(),
                mesh_ip: ip("192.0.2.5")
            }]
        );
    }

    #[test]
    fn extra_when_kernel_has_unknown_peer() {
        let set = intended(vec![]);
        let live = dump(vec![live_peer(9, &["192.0.2.99/32"], 0)]);
        let report = reconcile(&set, &live, 1_050);
        assert_eq!(report.drift.len(), 1);
        assert!(matches!(report.drift[0], DriftItem::Extra { .. }));
    }

    #[test]
    fn key_mismatch_when_kernel_key_not_acceptable() {
        let set = intended(vec![ipeer("beta", "192.0.2.5", vec![2])]);
        let live = dump(vec![live_peer(3, &["192.0.2.5/32"], 1_000)]); // wrong key
        let report = reconcile(&set, &live, 1_050);
        assert!(matches!(report.drift[0], DriftItem::KeyMismatch { .. }));
    }

    #[test]
    fn rotation_overlap_either_key_is_clean() {
        // acceptable {2, 9}; kernel holds 9 → not drift.
        let set = intended(vec![ipeer("beta", "192.0.2.5", vec![2, 9])]);
        let live = dump(vec![live_peer(9, &["192.0.2.5/32"], 1_000)]);
        let report = reconcile(&set, &live, 1_050);
        assert!(report.is_clean(), "either overlapping key is accepted");
    }

    #[test]
    fn allowed_ips_drift_when_extra_route_present() {
        let set = intended(vec![ipeer("beta", "192.0.2.5", vec![2])]);
        // right key + mesh_ip, but an extra route the inventory did not author.
        let live = dump(vec![live_peer(2, &["192.0.2.5/32", "10.0.0.0/8"], 1_000)]);
        let report = reconcile(&set, &live, 1_050);
        assert!(matches!(report.drift[0], DriftItem::AllowedIpsDrift { .. }));
    }

    #[test]
    fn endpoint_and_keepalive_are_never_drift() {
        let set = intended(vec![ipeer("beta", "192.0.2.5", vec![2])]);
        let mut lp = live_peer(2, &["192.0.2.5/32"], 1_000);
        lp.endpoint = Some("203.0.113.9:51820".into()); // inventory authors none
        lp.persistent_keepalive = Some(25);
        let live = dump(vec![lp]);
        let report = reconcile(&set, &live, 1_050);
        assert!(
            report.is_clean(),
            "endpoint/keepalive are not authored → not drift"
        );
    }

    #[test]
    fn offline_peer_is_synced_but_marked_offline() {
        let set = intended(vec![ipeer("beta", "192.0.2.5", vec![2])]);
        // handshake far in the past → Offline (> CONNECTED_THRESHOLD_SECS).
        let live = dump(vec![live_peer(2, &["192.0.2.5/32"], 1)]);
        let report = reconcile(&set, &live, 1_000_000);
        assert!(report.is_clean());
        assert_eq!(report.synced[0].status, PeerStatus::Offline);
    }
}
