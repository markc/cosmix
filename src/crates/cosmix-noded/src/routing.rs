//! Runtime routing authority derived from the verified signed inventory.
//!
//! The posture and its route table are one immutable snapshot so admission,
//! route classification, `noded.inventory`, and `noded.peers` cannot observe
//! different epochs. An Unverified boot retains the derived `/etc` roster as a
//! compatibility fallback; once Verified, the reload don't-downgrade rule in
//! `noded` keeps the last signed table in force.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use cosmix_mesh::PeerConfig;

use crate::authority::{Posture, RoutingMember};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteSource {
    SignedInventory,
    EtcRosterFallback,
}

impl RouteSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SignedInventory => "signed-inventory",
            Self::EtcRosterFallback => "etc-roster-fallback",
        }
    }
}

/// One resolved remote transport target. It owns a `PeerConfig` so the mesh
/// library's `SocketAddr`-based IPv6-safe URL construction remains the single
/// endpoint renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteTarget {
    peer: PeerConfig,
}

impl RouteTarget {
    fn signed(name: &str, mesh_ip: IpAddr, noded_port: u16) -> Self {
        Self {
            peer: PeerConfig {
                name: name.to_string(),
                mesh_ip,
                noded_port,
            },
        }
    }

    fn fallback(peer: &PeerConfig) -> Self {
        Self { peer: peer.clone() }
    }

    pub(crate) fn name(&self) -> &str {
        &self.peer.name
    }

    pub(crate) fn mesh_ip(&self) -> IpAddr {
        self.peer.mesh_ip
    }

    pub(crate) fn noded_port(&self) -> u16 {
        self.peer.noded_port
    }

    pub(crate) fn into_peer(self) -> PeerConfig {
        self.peer
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouterTable {
    source: RouteSource,
    peers: BTreeMap<String, RouteTarget>,
}

impl RouterTable {
    fn signed(view: &[RoutingMember], canonical_node: &str) -> Self {
        // `strict_routing_view` rejects duplicates; collect would otherwise be last-entry-wins while fallback is first-entry-wins.
        let peers = view
            .iter()
            .filter_map(|member| match member {
                RoutingMember::ActiveBus {
                    name,
                    mesh_ip,
                    noded_port,
                } if name != canonical_node => Some((
                    name.clone(),
                    RouteTarget::signed(name, *mesh_ip, *noded_port),
                )),
                _ => None,
            })
            .collect();
        Self {
            source: RouteSource::SignedInventory,
            peers,
        }
    }

    fn empty_signed() -> Self {
        Self {
            source: RouteSource::SignedInventory,
            peers: BTreeMap::new(),
        }
    }

    fn fallback(etc_roster: &[PeerConfig]) -> Self {
        let mut peers = BTreeMap::new();
        for peer in etc_roster {
            // Match the old `MeshConfig::find_peer` first-entry-wins behaviour
            // if a malformed fallback roster repeats a name.
            peers
                .entry(peer.name.clone())
                .or_insert_with(|| RouteTarget::fallback(peer));
        }
        Self {
            source: RouteSource::EtcRosterFallback,
            peers,
        }
    }

    pub(crate) fn source_label(&self) -> &'static str {
        self.source.label()
    }

    pub(crate) fn resolve(&self, node_name: &str) -> Option<RouteTarget> {
        self.peers.get(node_name).cloned()
    }

    pub(crate) fn peers(&self) -> impl Iterator<Item = &RouteTarget> {
        self.peers.values()
    }

    pub(crate) fn desired_endpoints(&self) -> HashMap<String, SocketAddr> {
        self.peers
            .values()
            .map(|target| (target.name().to_string(), target.peer.noded_addr()))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalEligibility {
    ActiveBus { mesh_ip: IpAddr, noded_port: u16 },
    Missing,
    TransportOnly,
    Tombstoned,
}

impl LocalEligibility {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ActiveBus { .. } => "active-bus",
            Self::Missing => "missing",
            Self::TransportOnly => "transport-only",
            Self::Tombstoned => "tombstoned",
        }
    }
}

/// Signed routing is enabled only when the canonical local name is itself an
/// active Bus member. The local configured WG address is deliberately not an
/// eligibility input: bind identity belongs to the §9a `wg_bound` check.
pub(crate) fn local_eligibility(view: &[RoutingMember], canonical_node: &str) -> LocalEligibility {
    view.iter()
        .find_map(|member| match member {
            RoutingMember::ActiveBus {
                name,
                mesh_ip,
                noded_port,
            } if name == canonical_node => Some(LocalEligibility::ActiveBus {
                mesh_ip: *mesh_ip,
                noded_port: *noded_port,
            }),
            RoutingMember::TransportOnly { name, .. } if name == canonical_node => {
                Some(LocalEligibility::TransportOnly)
            }
            RoutingMember::Tombstoned { name } if name == canonical_node => {
                Some(LocalEligibility::Tombstoned)
            }
            _ => None,
        })
        .unwrap_or(LocalEligibility::Missing)
}

/// The one atomically-published authority snapshot used by admission, route
/// classification, and both inventory-reporting verbs.
#[derive(Debug, Clone)]
pub(crate) struct RoutingAuthority {
    pub(crate) posture: Posture,
    pub(crate) routes: RouterTable,
    self_eligibility: Option<LocalEligibility>,
}

impl RoutingAuthority {
    pub(crate) fn new(
        posture: Posture,
        canonical_node: &str,
        configured_wg_ip: &str,
        _listener_port: u16,
        etc_roster: &[PeerConfig],
    ) -> Self {
        let (routes, self_eligibility) = match &posture {
            Posture::Verified(accepted) => {
                warn_port_divergence(&accepted.routing_view, etc_roster);
                let eligibility = local_eligibility(&accepted.routing_view, canonical_node);
                let routes = match eligibility {
                    LocalEligibility::ActiveBus { mesh_ip, .. } => {
                        match configured_wg_ip.parse::<IpAddr>() {
                            Ok(configured) if configured != mesh_ip => tracing::warn!(
                                node = canonical_node,
                                signed_mesh_ip = %mesh_ip,
                                configured_wg_ip = %configured,
                                "signed self mesh_ip differs from local WG configuration; routing remains enabled by name"
                            ),
                            Err(error) => tracing::warn!(
                                node = canonical_node,
                                configured_wg_ip,
                                error = %error,
                                "local WG configuration is not an IP address; signed routing remains enabled by name"
                            ),
                            _ => {}
                        }
                        RouterTable::signed(&accepted.routing_view, canonical_node)
                    }
                    eligibility => {
                        tracing::warn!(
                            node = canonical_node,
                            ?eligibility,
                            "canonical node is not an active Bus member; signed remote routing disabled"
                        );
                        RouterTable::empty_signed()
                    }
                };
                (routes, Some(eligibility))
            }
            Posture::Unverified { .. } => (RouterTable::fallback(etc_roster), None),
        };
        Self {
            posture,
            routes,
            self_eligibility,
        }
    }

    pub(crate) fn self_eligibility_label(&self) -> Option<&'static str> {
        self.self_eligibility.map(LocalEligibility::label)
    }

    pub(crate) fn signed_self_noded_port(&self) -> Option<u16> {
        match self.self_eligibility {
            Some(LocalEligibility::ActiveBus { noded_port, .. }) => Some(noded_port),
            _ => None,
        }
    }

    pub(crate) fn revision(&self) -> cosmix_mesh::AuthorityRevision {
        match &self.posture {
            Posture::Verified(accepted) => cosmix_mesh::AuthorityRevision {
                epoch: accepted.epoch,
                recovery_generation: accepted.recovery_generation,
            },
            Posture::Unverified { .. } => cosmix_mesh::AuthorityRevision::default(),
        }
    }
}

fn warn_port_divergence(view: &[RoutingMember], etc_roster: &[PeerConfig]) {
    for member in view {
        let RoutingMember::ActiveBus {
            name, noded_port, ..
        } = member
        else {
            continue;
        };
        // First-entry-wins by name, matching `RouterTable::fallback` — a later
        // duplicate roster entry's port is never effective, so never warn on it.
        let Some(peer) = etc_roster.iter().find(|peer| peer.name == *name) else {
            continue;
        };
        if peer.noded_port == *noded_port {
            continue;
        }
        tracing::warn!(
            member = name,
            etc_noded_port = peer.noded_port,
            signed_noded_port = noded_port,
            "ignoring divergent /etc broker port; signed inventory endpoint is authoritative"
        );
    }
}

pub(crate) fn validate_node_name(mesh_node_name: &str, canonical_node: &str) -> Result<()> {
    anyhow::ensure!(
        mesh_node_name == canonical_node,
        "mesh config node_name {mesh_node_name:?} does not match canonical node {canonical_node:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    use cosmix_mesh::DEFAULT_NODED_PORT;

    use super::*;
    use crate::authority::Accepted;

    fn accepted(routing_view: Vec<RoutingMember>) -> Posture {
        Posture::Verified(Accepted {
            epoch: 7,
            recovery_generation: 0,
            via_recovery: false,
            mesh: "mesh.example".into(),
            hash: "authority-hash".into(),
            verified_by: vec![],
            verify_keys: vec![],
            members: vec![],
            routing_view,
            members_full: BTreeMap::new(),
        })
    }

    fn active_at(name: &str, ip: &str, noded_port: u16) -> RoutingMember {
        RoutingMember::ActiveBus {
            name: name.into(),
            mesh_ip: ip.parse().unwrap(),
            noded_port,
        }
    }

    fn active(name: &str, ip: &str) -> RoutingMember {
        active_at(name, ip, DEFAULT_NODED_PORT)
    }

    fn etc_peer(name: &str, ip: &str, port: u16) -> PeerConfig {
        PeerConfig {
            name: name.into(),
            mesh_ip: ip.parse().unwrap(),
            noded_port: port,
        }
    }

    #[test]
    fn verified_routes_active_bus_members_minus_self_with_signed_port() {
        let posture = accepted(vec![
            active("alpha", "192.0.2.1"),
            active_at("beta", "192.0.2.2", 4300),
            RoutingMember::TransportOnly {
                name: "gamma".into(),
                mesh_ip: "192.0.2.3".parse().unwrap(),
            },
        ]);
        let authority = RoutingAuthority::new(
            posture,
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[etc_peer("beta", "192.0.2.99", 4300)],
        );

        assert_eq!(authority.routes.source_label(), "signed-inventory");
        assert!(authority.routes.resolve("alpha").is_none());
        assert!(authority.routes.resolve("gamma").is_none());
        let beta = authority.routes.resolve("beta").unwrap();
        assert_eq!(beta.mesh_ip(), "192.0.2.2".parse::<IpAddr>().unwrap());
        assert_eq!(beta.noded_port(), 4300);
    }

    #[test]
    fn verified_membership_replaces_etc_membership() {
        let authority = RoutingAuthority::new(
            accepted(vec![
                active("alpha", "192.0.2.1"),
                active("gamma", "192.0.2.3"),
            ]),
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[etc_peer("beta", "192.0.2.2", 4300)],
        );

        assert!(authority.routes.resolve("beta").is_none());
        assert_eq!(
            authority.routes.resolve("gamma").unwrap().mesh_ip(),
            "192.0.2.3".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn unverified_fallback_preserves_etc_addresses_and_custom_ports() {
        let authority = RoutingAuthority::new(
            Posture::Unverified {
                reason: "migration".into(),
            },
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[
                etc_peer("beta", "192.0.2.2", 4300),
                etc_peer("gamma", "192.0.2.3", 4400),
            ],
        );

        assert_eq!(authority.routes.source_label(), "etc-roster-fallback");
        assert_eq!(authority.routes.resolve("beta").unwrap().noded_port(), 4300);
        assert_eq!(
            authority.routes.resolve("gamma").unwrap().noded_port(),
            4400
        );
    }

    #[test]
    fn missing_transport_only_or_tombstoned_self_disables_signed_routes() {
        let cases = [
            (vec![active("beta", "192.0.2.2")], "missing"),
            (
                vec![
                    RoutingMember::TransportOnly {
                        name: "alpha".into(),
                        mesh_ip: "192.0.2.1".parse().unwrap(),
                    },
                    active("beta", "192.0.2.2"),
                ],
                "transport-only",
            ),
            (
                vec![
                    RoutingMember::Tombstoned {
                        name: "alpha".into(),
                    },
                    active("beta", "192.0.2.2"),
                ],
                "tombstoned",
            ),
        ];

        for (view, expected_eligibility) in cases {
            let authority = RoutingAuthority::new(
                accepted(view),
                "alpha",
                "192.0.2.1",
                DEFAULT_NODED_PORT,
                &[],
            );
            assert_eq!(authority.routes.source_label(), "signed-inventory");
            assert!(authority.routes.peers().next().is_none());
            assert_eq!(
                authority.self_eligibility_label(),
                Some(expected_eligibility)
            );
        }
    }

    struct CountWarns(Arc<AtomicU64>);

    impl<S: tracing::Subscriber> Layer<S> for CountWarns {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn wrong_local_wg_ip_warns_but_does_not_disable_name_eligible_routes() {
        let warns = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(CountWarns(warns.clone()));
        let authority = tracing::subscriber::with_default(subscriber, || {
            RoutingAuthority::new(
                accepted(vec![
                    active("alpha", "192.0.2.1"),
                    active("beta", "192.0.2.2"),
                ]),
                "alpha",
                "192.0.2.99",
                DEFAULT_NODED_PORT,
                &[],
            )
        });

        assert!(authority.routes.resolve("beta").is_some());
        assert_eq!(warns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn wrong_local_listener_port_is_exposed_without_disabling_routes() {
        let warns = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(CountWarns(warns.clone()));
        let authority = tracing::subscriber::with_default(subscriber, || {
            RoutingAuthority::new(
                accepted(vec![
                    active_at("alpha", "192.0.2.1", 4300),
                    active_at("beta", "192.0.2.2", 4400),
                ]),
                "alpha",
                "192.0.2.1",
                DEFAULT_NODED_PORT,
                &[],
            )
        });

        assert_eq!(authority.routes.resolve("beta").unwrap().noded_port(), 4400);
        assert_eq!(authority.signed_self_noded_port(), Some(4300));
        assert_eq!(warns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn signed_active_members_with_custom_etc_ports_warn_once_each() {
        let warns = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(CountWarns(warns.clone()));
        let authority = tracing::subscriber::with_default(subscriber, || {
            RoutingAuthority::new(
                accepted(vec![
                    active("alpha", "192.0.2.1"),
                    active("beta", "192.0.2.2"),
                    RoutingMember::TransportOnly {
                        name: "gamma".into(),
                        mesh_ip: "192.0.2.3".parse().unwrap(),
                    },
                ]),
                "alpha",
                "192.0.2.1",
                DEFAULT_NODED_PORT,
                &[
                    etc_peer("alpha", "192.0.2.1", 4300),
                    etc_peer("beta", "192.0.2.2", 4400),
                    etc_peer("gamma", "192.0.2.3", 4500),
                ],
            )
        });

        assert_eq!(warns.load(Ordering::Relaxed), 2);
        assert_eq!(
            authority.routes.resolve("beta").unwrap().noded_port(),
            DEFAULT_NODED_PORT
        );
    }

    #[test]
    fn duplicate_etc_name_warns_by_first_entry_only() {
        // /etc roster semantics are first-entry-wins (`RouterTable::fallback`),
        // so a later duplicate's divergent port must not produce a warning.
        let warns = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(CountWarns(warns.clone()));
        tracing::subscriber::with_default(subscriber, || {
            RoutingAuthority::new(
                accepted(vec![
                    active("alpha", "192.0.2.1"),
                    active("beta", "192.0.2.2"),
                ]),
                "alpha",
                "192.0.2.1",
                DEFAULT_NODED_PORT,
                &[
                    etc_peer("beta", "192.0.2.2", DEFAULT_NODED_PORT),
                    etc_peer("beta", "192.0.2.2", 4300),
                ],
            )
        });

        assert_eq!(warns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn matching_non_default_etc_and_signed_ports_do_not_warn() {
        let warns = Arc::new(AtomicU64::new(0));
        let subscriber = tracing_subscriber::registry().with(CountWarns(warns.clone()));
        tracing::subscriber::with_default(subscriber, || {
            RoutingAuthority::new(
                accepted(vec![
                    active("alpha", "192.0.2.1"),
                    active_at("beta", "192.0.2.2", 4300),
                ]),
                "alpha",
                "192.0.2.1",
                DEFAULT_NODED_PORT,
                &[etc_peer("beta", "192.0.2.2", 4300)],
            )
        });

        assert_eq!(warns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn port_only_epoch_change_preserves_the_owned_message_snapshot() {
        let old = RoutingAuthority::new(
            accepted(vec![
                active("alpha", "192.0.2.1"),
                active_at("beta", "192.0.2.2", DEFAULT_NODED_PORT),
            ]),
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[],
        );
        let captured = old.routes.resolve("beta").unwrap();
        let new = RoutingAuthority::new(
            accepted(vec![
                active("alpha", "192.0.2.1"),
                active_at("beta", "192.0.2.2", 4300),
            ]),
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[],
        );

        assert_eq!(captured.mesh_ip(), "192.0.2.2".parse::<IpAddr>().unwrap());
        assert_eq!(captured.noded_port(), DEFAULT_NODED_PORT);
        assert_eq!(new.routes.resolve("beta").unwrap().noded_port(), 4300);
    }

    #[test]
    fn one_arc_swap_publication_replaces_posture_and_routes_together() {
        let fallback = RoutingAuthority::new(
            Posture::Unverified {
                reason: "migration".into(),
            },
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[etc_peer("beta", "192.0.2.2", 4300)],
        );
        let cell = arc_swap::ArcSwap::from_pointee(fallback);
        let verified = RoutingAuthority::new(
            accepted(vec![
                active("alpha", "192.0.2.1"),
                active("gamma", "192.0.2.3"),
            ]),
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[],
        );
        cell.store(Arc::new(verified));

        let loaded = cell.load();
        assert!(matches!(
            &loaded.posture,
            Posture::Verified(accepted) if accepted.epoch == 7
        ));
        assert_eq!(loaded.routes.source_label(), "signed-inventory");
        assert!(loaded.routes.resolve("beta").is_none());
        assert!(loaded.routes.resolve("gamma").is_some());
    }

    #[test]
    fn removed_member_is_not_resolved_after_publication() {
        let old = RoutingAuthority::new(
            accepted(vec![
                active("alpha", "192.0.2.1"),
                active("beta", "192.0.2.2"),
            ]),
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[],
        );
        let already_classified = old.routes.resolve("beta").unwrap();
        let new = RoutingAuthority::new(
            accepted(vec![
                active("alpha", "192.0.2.1"),
                active("gamma", "192.0.2.3"),
            ]),
            "alpha",
            "192.0.2.1",
            DEFAULT_NODED_PORT,
            &[],
        );

        assert_eq!(already_classified.name(), "beta");
        assert!(new.routes.resolve("beta").is_none());
    }

    #[test]
    fn node_name_validation_rejects_split_local_identity() {
        assert!(validate_node_name("alpha", "alpha").is_ok());
        let error = validate_node_name("beta", "alpha").unwrap_err();
        assert!(error.to_string().contains("does not match canonical node"));
    }
}
