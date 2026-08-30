//! Pure node-atlas model and tolerant wire decoders.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::Resource;
use serde::Deserialize;

use crate::inspector::{CitizenInspector, MutationTarget, ProcessIdentity};
use crate::props::PropsSurface;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct InventoryMember {
    pub name: String,
    pub mesh_ip: String,
    pub bus: bool,
    pub status: String,
}

impl InventoryMember {
    pub(crate) fn active_bus(&self) -> bool {
        self.bus && self.status == "active"
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct InventoryProjection {
    pub posture: String,
    #[serde(default)]
    pub epoch: Option<u64>,
    #[serde(default)]
    pub mesh: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub members: Vec<InventoryMember>,
}

impl InventoryProjection {
    pub(crate) fn parse(body: &str) -> Result<Self, String> {
        let mut projection: Self = serde_json::from_str(body)
            .map_err(|error| format!("invalid noded.inventory: {error}"))?;
        projection.members.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(projection)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct PeerSummary {
    pub name: String,
    #[serde(default)]
    pub wg_ip: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct PeersProjection {
    pub node: String,
    #[serde(default)]
    pub wg_ip: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub peers: Vec<PeerSummary>,
}

impl PeersProjection {
    pub(crate) fn parse(body: &str) -> Result<Self, String> {
        let mut projection: Self =
            serde_json::from_str(body).map_err(|error| format!("invalid noded.peers: {error}"))?;
        projection.peers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(projection)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct NodeInfo {
    pub node: String,
    #[serde(default)]
    pub wg_ip: Option<String>,
    #[serde(default)]
    pub uptime_s: Option<u64>,
    #[serde(default)]
    pub service_count: Option<u16>,
    #[serde(default)]
    pub noded: Option<ServiceInfo>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ServiceInfo {
    pub name: String,
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub git_sha: Option<String>,
    #[serde(default)]
    pub git_dirty: Option<bool>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at: Option<String>,
}

impl ServiceInfo {
    pub(crate) fn process_identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            service: self.name.clone(),
            pid: self.pid,
            started_at: self.started_at.clone(),
        }
    }
}

pub(crate) fn parse_service_list(body: &str) -> Result<Vec<ServiceInfo>, String> {
    let values: Vec<serde_json::Value> =
        serde_json::from_str(body).map_err(|error| format!("invalid noded.list: {error}"))?;
    let mut services = Vec::with_capacity(values.len());
    for value in values {
        match value {
            serde_json::Value::String(name) => services.push(ServiceInfo {
                name,
                binary: None,
                version: None,
                git_sha: None,
                git_dirty: None,
                pid: None,
                started_at: None,
            }),
            serde_json::Value::Object(_) => {
                let service: ServiceInfo = serde_json::from_value(value)
                    .map_err(|error| format!("invalid ServiceInfo in noded.list: {error}"))?;
                services.push(service);
            }
            _ => return Err("noded.list entries must be strings or ServiceInfo objects".into()),
        }
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshReason {
    Startup,
    Reconnect,
    Manual,
}

impl RefreshReason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Reconnect => "reconnect",
            Self::Manual => "manual",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reachability {
    Unknown,
    Observed,
    Stale,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AtlasNode {
    pub member: InventoryMember,
    pub info: Option<NodeInfo>,
    pub citizens: Vec<ServiceInfo>,
    pub info_reachability: Reachability,
    pub info_observed_at_ms: Option<u64>,
    pub info_error: Option<String>,
    pub citizens_reachability: Reachability,
    pub citizens_observed_at_ms: Option<u64>,
    pub citizens_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonUnitStatus {
    Active,
    Inactive,
    Failed,
}

impl DaemonUnitStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DaemonUnit {
    pub unit: String,
    pub status: DaemonUnitStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NodeDaemons {
    pub units: Vec<DaemonUnit>,
    pub observed_at_ms: Option<u64>,
    pub loading: bool,
    pub error: Option<String>,
    pub result: Option<String>,
    pub result_observed_at_ms: Option<u64>,
}

impl AtlasNode {
    fn from_member(member: InventoryMember) -> Self {
        Self {
            member,
            info: None,
            citizens: Vec::new(),
            info_reachability: Reachability::Unknown,
            info_observed_at_ms: None,
            info_error: None,
            citizens_reachability: Reachability::Unknown,
            citizens_observed_at_ms: None,
            citizens_error: None,
        }
    }

    pub(crate) fn status_label(&self) -> &'static str {
        match (self.info_reachability, self.citizens_reachability) {
            (Reachability::Observed, Reachability::Observed) => "observed",
            _ => "unknown",
        }
    }
}

#[derive(Resource, Clone, Debug)]
pub(crate) struct AtlasState {
    pub connection: ctk::prelude::BusConnectionState,
    pub connection_generation: u64,
    pub inventory: Option<InventoryProjection>,
    pub inventory_observed_at_ms: Option<u64>,
    pub mesh_posture: String,
    pub mesh_reason: Option<String>,
    pub peers: Option<PeersProjection>,
    pub peers_reachability: Reachability,
    pub nodes: BTreeMap<String, AtlasNode>,
    pub properties: BTreeMap<String, PropsSurface>,
    pub selected: Option<String>,
    pub selected_citizen: Option<String>,
    pub inspector: Option<CitizenInspector>,
    pub active_mutations: BTreeSet<MutationTarget>,
    pub daemons: BTreeMap<String, NodeDaemons>,
    pub refreshing: bool,
    pub last_refresh_reason: Option<RefreshReason>,
    pub notice: Option<String>,
    pub revision: u64,
    pub(crate) lifecycle_epoch: Arc<RwLock<Option<u64>>>,
}

impl Default for AtlasState {
    fn default() -> Self {
        Self {
            connection: ctk::prelude::BusConnectionState::Connecting,
            connection_generation: 0,
            inventory: None,
            inventory_observed_at_ms: None,
            mesh_posture: "unknown".into(),
            mesh_reason: None,
            peers: None,
            peers_reachability: Reachability::Unknown,
            nodes: BTreeMap::new(),
            properties: BTreeMap::new(),
            selected: None,
            selected_citizen: None,
            inspector: None,
            active_mutations: BTreeSet::new(),
            daemons: BTreeMap::new(),
            refreshing: false,
            last_refresh_reason: None,
            notice: None,
            revision: 0,
            lifecycle_epoch: Arc::new(RwLock::new(None)),
        }
    }
}

impl AtlasState {
    pub(crate) fn apply_inventory(
        &mut self,
        inventory: InventoryProjection,
        observed_at_ms: u64,
    ) -> bool {
        self.mesh_posture = inventory.posture.clone();
        self.mesh_reason = inventory.reason.clone();
        if inventory.posture != "verified" {
            self.set_lifecycle_epoch(None);
            self.mark_all_nodes_unknown();
            return false;
        }
        self.set_lifecycle_epoch(inventory.epoch);
        let declared: BTreeSet<_> = inventory
            .members
            .iter()
            .map(|member| member.name.clone())
            .collect();
        self.nodes.retain(|name, _| declared.contains(name));
        self.daemons.retain(|name, _| declared.contains(name));
        for member in &inventory.members {
            self.nodes
                .entry(member.name.clone())
                .and_modify(|node| node.member = member.clone())
                .or_insert_with(|| AtlasNode::from_member(member.clone()));
        }
        if self
            .selected
            .as_ref()
            .is_none_or(|selected| !declared.contains(selected))
        {
            self.selected = inventory.members.first().map(|member| member.name.clone());
            self.selected_citizen = None;
            self.inspector = None;
        }
        self.inventory = Some(inventory);
        self.inventory_observed_at_ms = Some(observed_at_ms);
        self.bump();
        true
    }

    pub(crate) fn mark_inventory_failed(&mut self, error: impl Into<String>) {
        self.set_lifecycle_epoch(None);
        self.mesh_posture = "unknown".into();
        self.mesh_reason = Some(error.into());
        self.mark_all_nodes_unknown();
    }

    pub(crate) fn observe_node_info(&mut self, node: &str, info: NodeInfo, observed_at_ms: u64) {
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.info = Some(info);
            entry.info_reachability = Reachability::Observed;
            entry.info_observed_at_ms = Some(observed_at_ms);
            entry.info_error = None;
            self.bump();
        }
    }

    pub(crate) fn observe_citizens(
        &mut self,
        node: &str,
        citizens: Vec<ServiceInfo>,
        observed_at_ms: u64,
    ) {
        let selected_identity = self
            .inspector
            .as_ref()
            .map(|inspector| inspector.identity.clone());
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.citizens = citizens;
            entry.citizens_reachability = Reachability::Observed;
            entry.citizens_observed_at_ms = Some(observed_at_ms);
            entry.citizens_error = None;
            if self.selected.as_deref() == Some(node)
                && selected_identity.as_ref().is_some_and(|selected| {
                    entry
                        .citizens
                        .iter()
                        .find(|citizen| citizen.name == selected.service)
                        .map(ServiceInfo::process_identity)
                        .is_none_or(|current| !selected.same_process(&current))
                })
            {
                self.selected_citizen = None;
                self.inspector = None;
            }
            self.bump();
        }
    }

    pub(crate) fn mark_node_info_unknown(&mut self, node: &str, error: impl Into<String>) {
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.info_reachability = if entry.info_observed_at_ms.is_some() {
                Reachability::Stale
            } else {
                Reachability::Unknown
            };
            entry.info_error = Some(error.into());
            self.bump();
        }
    }

    pub(crate) fn mark_node_citizens_unknown(&mut self, node: &str, error: impl Into<String>) {
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.citizens_reachability = if entry.citizens_observed_at_ms.is_some() {
                Reachability::Stale
            } else {
                Reachability::Unknown
            };
            entry.citizens_error = Some(error.into());
            self.bump();
        }
    }

    pub(crate) fn mark_all_remote_stale(&mut self) {
        self.mark_peers_stale();
        let local = self.local_node().map(str::to_owned);
        for (name, node) in &mut self.nodes {
            if local.as_deref() != Some(name.as_str()) {
                if node.info_observed_at_ms.is_some() {
                    node.info_reachability = Reachability::Stale;
                }
                if node.citizens_observed_at_ms.is_some() {
                    node.citizens_reachability = Reachability::Stale;
                }
            }
        }
        self.bump();
    }

    fn mark_all_nodes_unknown(&mut self) {
        self.mark_peers_stale();
        for node in self.nodes.values_mut() {
            node.info_reachability = if node.info_observed_at_ms.is_some() {
                Reachability::Stale
            } else {
                Reachability::Unknown
            };
            node.citizens_reachability = if node.citizens_observed_at_ms.is_some() {
                Reachability::Stale
            } else {
                Reachability::Unknown
            };
        }
        self.bump();
    }

    pub(crate) fn local_node(&self) -> Option<&str> {
        self.peers.as_ref().map(|peers| peers.node.as_str())
    }

    pub(crate) fn local_edges(&self) -> Vec<(String, String)> {
        let Some(peers) = &self.peers else {
            return Vec::new();
        };
        peers
            .peers
            .iter()
            .filter(|peer| self.nodes.contains_key(&peer.name))
            .map(|peer| (peers.node.clone(), peer.name.clone()))
            .collect()
    }

    pub(crate) fn fresh_local_edges(&self) -> Vec<(String, String)> {
        if self.peers_reachability != Reachability::Observed {
            return Vec::new();
        }
        self.local_edges()
    }

    pub(crate) fn observe_peers(&mut self, peers: PeersProjection) {
        self.peers = Some(peers);
        self.peers_reachability = Reachability::Observed;
        self.bump();
    }

    fn mark_peers_stale(&mut self) {
        self.peers_reachability = if self.peers.is_some() {
            Reachability::Stale
        } else {
            Reachability::Unknown
        };
    }

    pub(crate) fn observed_label_at(observed_at_ms: Option<u64>, now_ms: u64) -> String {
        observed_at_ms.map_or_else(
            || "never observed".into(),
            |value| format!("{}s ago", now_ms.saturating_sub(value) / 1_000),
        )
    }

    pub(crate) fn select(&mut self, node: impl Into<String>) {
        let node = node.into();
        if self.nodes.contains_key(&node) && self.selected.as_deref() != Some(node.as_str()) {
            self.selected = Some(node);
            self.selected_citizen = None;
            self.inspector = None;
            self.bump();
        }
    }

    pub(crate) fn select_citizen(&mut self, identity: ProcessIdentity) {
        self.selected_citizen = Some(identity.service.clone());
        self.inspector = Some(CitizenInspector::pending(identity));
        self.bump();
    }

    pub(crate) fn selected_process_identity(&self, service: &str) -> Option<ProcessIdentity> {
        let local = self.local_node()?;
        if self.selected.as_deref() != Some(local)
            || self.selected_citizen.as_deref() != Some(service)
        {
            return None;
        }
        self.nodes
            .get(local)?
            .citizens
            .iter()
            .find(|citizen| citizen.name == service)
            .map(ServiceInfo::process_identity)
    }

    pub(crate) fn lifecycle_epoch_handle(&self) -> Arc<RwLock<Option<u64>>> {
        Arc::clone(&self.lifecycle_epoch)
    }

    pub(crate) fn lifecycle_epoch(&self) -> Option<u64> {
        *self
            .lifecycle_epoch
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_lifecycle_epoch(&self, epoch: Option<u64>) {
        *self
            .lifecycle_epoch
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = epoch;
    }

    pub(crate) fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERIFIED: &str = include_str!("fixtures/inventory_verified.json");
    const UNVERIFIED: &str = include_str!("fixtures/inventory_unverified.json");
    const LEGACY_LIST: &str = include_str!("fixtures/noded_list_legacy.json");
    const NEW_LIST: &str = include_str!("fixtures/noded_list_new.json");

    #[test]
    fn parses_verified_and_unverified_inventory_fixtures() {
        let verified = InventoryProjection::parse(VERIFIED).unwrap();
        assert_eq!(verified.posture, "verified");
        assert_eq!(verified.epoch, Some(42));
        assert_eq!(verified.members.len(), 3);

        let unverified = InventoryProjection::parse(UNVERIFIED).unwrap();
        assert_eq!(unverified.posture, "unverified");
        assert_eq!(unverified.members.len(), 0);
        assert_eq!(
            unverified.reason.as_deref(),
            Some("genesis key unavailable")
        );
    }

    #[test]
    fn dual_parses_legacy_and_service_info_lists() {
        let legacy = parse_service_list(LEGACY_LIST).unwrap();
        assert_eq!(legacy[0].name, "indexd");
        assert_eq!(legacy[0].version, None);

        let current = parse_service_list(NEW_LIST).unwrap();
        assert_eq!(current[0].name, "indexd");
        assert_eq!(current[0].version.as_deref(), Some("0.4.0"));
        assert_eq!(current[1].pid, Some(77));
    }

    #[test]
    fn anonymous_process_identity_never_survives_a_roster_refresh() {
        let anonymous = ServiceInfo {
            name: "legacy-bevy-4242".into(),
            binary: None,
            version: None,
            git_sha: None,
            git_dirty: None,
            pid: None,
            started_at: None,
        };
        let identity = anonymous.process_identity();
        assert!(!identity.is_known());
        assert!(!identity.same_process(&identity));

        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(InventoryProjection::parse(VERIFIED).unwrap(), 100));
        atlas.select("alpha");
        atlas.observe_citizens("alpha", vec![anonymous.clone()], 110);
        atlas.select_citizen(identity);
        assert!(atlas.inspector.is_some());

        atlas.observe_citizens("alpha", vec![anonymous], 120);
        assert!(atlas.selected_citizen.is_none());
        assert!(atlas.inspector.is_none());
    }

    #[test]
    fn failed_refresh_never_renders_remote_down() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(InventoryProjection::parse(VERIFIED).unwrap(), 100));
        atlas.observe_citizens("delta", Vec::new(), 110);
        atlas.mark_node_citizens_unknown("delta", "timeout");
        let delta = &atlas.nodes["delta"];
        assert_eq!(delta.citizens_reachability, Reachability::Stale);
        assert_eq!(delta.status_label(), "unknown");
        assert_eq!(
            AtlasState::observed_label_at(delta.citizens_observed_at_ms, 12_110),
            "12s ago"
        );
    }

    #[test]
    fn manual_refresh_presentation_retains_last_observation() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(InventoryProjection::parse(VERIFIED).unwrap(), 500));
        atlas.refreshing = true;
        atlas.last_refresh_reason = Some(RefreshReason::Manual);
        assert_eq!(atlas.last_refresh_reason.unwrap().label(), "manual");
        assert_eq!(
            AtlasState::observed_label_at(atlas.inventory_observed_at_ms, 12_999),
            "12s ago"
        );
    }

    #[test]
    fn observation_age_handles_missing_and_future_timestamps() {
        assert_eq!(
            AtlasState::observed_label_at(None, 12_000),
            "never observed"
        );
        assert_eq!(
            AtlasState::observed_label_at(Some(13_000), 12_000),
            "0s ago"
        );
    }

    #[test]
    fn unverified_inventory_retains_verified_atlas_and_timestamp() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(InventoryProjection::parse(VERIFIED).unwrap(), 500));
        atlas.observe_node_info(
            "delta",
            NodeInfo {
                node: "delta".into(),
                wg_ip: None,
                uptime_s: Some(10),
                service_count: None,
                noded: None,
            },
            510,
        );
        assert!(!atlas.apply_inventory(InventoryProjection::parse(UNVERIFIED).unwrap(), 900,));
        assert_eq!(atlas.nodes.len(), 3);
        assert_eq!(atlas.inventory_observed_at_ms, Some(500));
        assert_eq!(atlas.mesh_posture, "unverified");
        assert_eq!(atlas.nodes["delta"].info_reachability, Reachability::Stale);
    }

    #[test]
    fn info_and_citizen_freshness_are_independent() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(InventoryProjection::parse(VERIFIED).unwrap(), 100));
        atlas.observe_node_info(
            "delta",
            NodeInfo {
                node: "delta".into(),
                wg_ip: None,
                uptime_s: None,
                service_count: None,
                noded: None,
            },
            110,
        );
        atlas.observe_citizens("delta", Vec::new(), 120);
        atlas.mark_node_info_unknown("delta", "late info timeout");
        let delta = &atlas.nodes["delta"];
        assert_eq!(delta.info_observed_at_ms, Some(110));
        assert_eq!(delta.info_reachability, Reachability::Stale);
        assert_eq!(delta.citizens_observed_at_ms, Some(120));
        assert_eq!(delta.citizens_reachability, Reachability::Observed);
    }
}
