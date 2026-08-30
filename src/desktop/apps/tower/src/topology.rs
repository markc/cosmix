//! Deterministic node-level topology layout and CTK canvas reconciliation.

use std::collections::{BTreeMap, BTreeSet};

use bevy::math::Vec2;
use bevy::picking::Pickable;
use bevy::prelude::{
    default, BackgroundColor, BorderRadius, Color, Commands, Entity, Node, Resource,
};
use bevy::ui::{px, UiRect};
use ctk::prelude::{
    spawn_topology_edge, spawn_topology_node, TopologyCanvasEntities, TopologyEdgeProps,
    TopologyNodeProps,
};

use crate::model::{AtlasState, Reachability};
use crate::panes::{ascii_ui_text_bounded, TowerFocusKey};
use crate::traffic::TrafficEvent;

const ORIGIN: Vec2 = Vec2::new(480.0, 310.0);
const INNER_RADIUS: f32 = 230.0;
const OUTER_RADIUS: f32 = 390.0;
const ACTIVITY_PULSE: f32 = 0.45;
const ACTIVITY_DECAY_PER_SECOND: f32 = 0.8;

pub(crate) type EdgeKey = (String, String);

pub(crate) struct TopologyBuild {
    pub focus: Vec<(TowerFocusKey, Entity)>,
    pub edges: BTreeMap<EdgeKey, Entity>,
    pub local_indicator: Option<Entity>,
}

pub(crate) fn rebuild(
    commands: &mut Commands,
    canvas: TopologyCanvasEntities,
    atlas: &AtlasState,
) -> TopologyBuild {
    commands.entity(canvas.edge_layer).despawn_children();
    commands.entity(canvas.node_layer).despawn_children();
    let positions = positions(atlas);
    let mut focus = Vec::new();
    let mut local_indicator = None;
    for (name, position) in &positions {
        let Some(node) = atlas.nodes.get(name) else {
            continue;
        };
        let label = ascii_ui_text_bounded(
            &format!(
                "{}\n{}\n{}",
                node.member.name,
                node.status_label(),
                node.member.mesh_ip
            ),
            256,
        );
        let entities = spawn_topology_node(
            commands,
            canvas.root,
            canvas.node_layer,
            TopologyNodeProps::new(name, label, *position),
        );
        let key = TowerFocusKey::Topology(name.clone());
        commands.entity(entities.root).insert(key.clone());
        focus.push((key, entities.root));
        if atlas.local_node() == Some(name.as_str()) {
            let indicator = commands
                .spawn((
                    Node {
                        position_type: bevy::ui::PositionType::Absolute,
                        right: px(7),
                        bottom: px(7),
                        width: px(12),
                        height: px(12),
                        border: UiRect::all(px(1)),
                        border_radius: BorderRadius::MAX,
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                    Pickable::IGNORE,
                ))
                .id();
            commands.entity(entities.root).add_child(indicator);
            local_indicator = Some(indicator);
        }
    }
    let mut edges = BTreeMap::new();
    for (from, to) in atlas.local_edges() {
        let entity = spawn_topology_edge(
            commands,
            canvas.root,
            canvas.edge_layer,
            TopologyEdgeProps::new(&from, &to),
        );
        edges.insert(edge_key(&from, &to), entity);
    }
    TopologyBuild {
        focus,
        edges,
        local_indicator,
    }
}

#[derive(Resource, Clone, Debug, Default)]
pub(crate) struct TopologyActivity {
    edges: BTreeMap<EdgeKey, f32>,
    local_unattributed: f32,
    pub revision: u64,
}

impl TopologyActivity {
    pub(crate) fn record(&mut self, event: &TrafficEvent, atlas: &AtlasState) {
        match activity_target(event, atlas) {
            ActivityTarget::Edge(edge) => pulse(self.edges.entry(edge).or_default()),
            ActivityTarget::LocalIndicator => pulse(&mut self.local_unattributed),
        }
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn edge_intensity(&self, edge: &EdgeKey) -> f32 {
        self.edges.get(edge).copied().unwrap_or_default()
    }

    pub(crate) fn local_intensity(&self) -> f32 {
        self.local_unattributed
    }

    pub(crate) fn is_hot(&self) -> bool {
        self.local_unattributed > 0.0 || !self.edges.is_empty()
    }

    /// Decay is called only while [`Self::is_hot`] by the UI run condition.
    /// Returning to an empty map makes the system fully idle on later frames.
    pub(crate) fn decay(&mut self, delta_seconds: f32) {
        if !self.is_hot() || !delta_seconds.is_finite() || delta_seconds <= 0.0 {
            return;
        }
        let amount = ACTIVITY_DECAY_PER_SECOND * delta_seconds;
        self.local_unattributed = (self.local_unattributed - amount).max(0.0);
        self.edges.retain(|_, intensity| {
            *intensity = (*intensity - amount).max(0.0);
            *intensity > 0.0
        });
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

fn pulse(intensity: &mut f32) {
    *intensity = (*intensity + ACTIVITY_PULSE).clamp(ACTIVITY_PULSE, 1.0);
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ActivityTarget {
    Edge(EdgeKey),
    LocalIndicator,
}

fn activity_target(event: &TrafficEvent, atlas: &AtlasState) -> ActivityTarget {
    let Some(local) = atlas.local_node() else {
        return ActivityTarget::LocalIndicator;
    };
    let from = event
        .from
        .as_deref()
        .and_then(|service| endpoint_node(service, atlas));
    let to = event
        .to
        .as_deref()
        .and_then(|service| endpoint_node(service, atlas));
    let declared: BTreeSet<_> = atlas
        .fresh_local_edges()
        .into_iter()
        .map(|(from, to)| edge_key(&from, &to))
        .collect();

    if let (Some(from), Some(to)) = (from.as_deref(), to.as_deref()) {
        if from != to {
            let edge = edge_key(from, to);
            if declared.contains(&edge) {
                return ActivityTarget::Edge(edge);
            }
        }
    }

    let remote = match event.direction.as_str() {
        "mesh_in" => from
            .as_deref()
            .filter(|node| *node != local)
            .or_else(|| to.as_deref().filter(|node| *node != local)),
        "mesh_out" => to
            .as_deref()
            .filter(|node| *node != local)
            .or_else(|| from.as_deref().filter(|node| *node != local)),
        _ => from
            .as_deref()
            .filter(|node| *node != local)
            .or_else(|| to.as_deref().filter(|node| *node != local)),
    };
    if let Some(remote) = remote {
        let edge = edge_key(local, remote);
        if declared.contains(&edge) {
            return ActivityTarget::Edge(edge);
        }
    }
    ActivityTarget::LocalIndicator
}

fn endpoint_node(service: &str, atlas: &AtlasState) -> Option<String> {
    for name in atlas.nodes.keys() {
        if service == name
            || service == format!("{name}.bus")
            || service.ends_with(&format!(".{name}.bus"))
            || service == format!("bridge-{name}")
        {
            return Some(name.clone());
        }
    }
    // Uniqueness is judged over ALL claimants, fresh or stale: a stale
    // roster can't attribute traffic, but it can still contest uniqueness —
    // otherwise removing it would make a same-named fresh citizen falsely
    // unique and mis-pulse an edge for traffic that originated elsewhere.
    let claimants: Vec<_> = atlas
        .nodes
        .iter()
        .filter(|(_, node)| node.citizens.iter().any(|citizen| citizen.name == service))
        .collect();
    if claimants.len() == 1 && claimants[0].1.citizens_reachability == Reachability::Observed {
        return Some(claimants[0].0.clone());
    }
    // Ambiguous or stale-only: never attribute to a specific remote. The
    // local-claimant fallback below is the honest aggregate bucket.
    let matches: Vec<_> = claimants
        .into_iter()
        .map(|(name, _)| name.clone())
        .collect();
    let local = atlas.local_node()?;
    matches
        .into_iter()
        .find(|candidate| candidate.as_str() == local)
}

pub(crate) fn edge_key(from: &str, to: &str) -> EdgeKey {
    if from <= to {
        (from.to_owned(), to.to_owned())
    } else {
        (to.to_owned(), from.to_owned())
    }
}

pub(crate) fn positions(atlas: &AtlasState) -> BTreeMap<String, Vec2> {
    let local = atlas.local_node().map(str::to_owned);
    let direct: BTreeSet<_> = atlas
        .peers
        .as_ref()
        .map(|peers| peers.peers.iter().map(|peer| peer.name.clone()).collect())
        .unwrap_or_default();
    let mut result = BTreeMap::new();
    if let Some(local) = &local {
        if atlas.nodes.contains_key(local) {
            result.insert(local.clone(), ORIGIN);
        }
    }
    let inner: Vec<_> = atlas
        .nodes
        .keys()
        .filter(|name| Some(name.as_str()) != local.as_deref() && direct.contains(*name))
        .cloned()
        .collect();
    let outer: Vec<_> = atlas
        .nodes
        .keys()
        .filter(|name| Some(name.as_str()) != local.as_deref() && !direct.contains(*name))
        .cloned()
        .collect();
    place_ring(&mut result, &inner, INNER_RADIUS);
    place_ring(&mut result, &outer, OUTER_RADIUS);
    result
}

fn place_ring(result: &mut BTreeMap<String, Vec2>, names: &[String], radius: f32) {
    let count = names.len().max(1) as f32;
    for (index, name) in names.iter().enumerate() {
        let angle = std::f32::consts::TAU * index as f32 / count - std::f32::consts::FRAC_PI_2;
        result.insert(
            name.clone(),
            ORIGIN + Vec2::new(angle.cos() * radius, angle.sin() * radius),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InventoryProjection, PeersProjection, ServiceInfo};

    fn activity_atlas() -> AtlasState {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(
            InventoryProjection::parse(include_str!("fixtures/inventory_verified.json")).unwrap(),
            1,
        ));
        atlas.observe_peers(
            PeersProjection::parse(
                r#"{"node":"alpha","peers":[{"name":"delta","wg_ip":"192.0.2.11","port":4200}]}"#,
            )
            .unwrap(),
        );
        atlas.observe_citizens(
            "alpha",
            vec![ServiceInfo {
                name: "tower-bevy-100".into(),
                binary: None,
                version: None,
                git_sha: None,
                git_dirty: None,
                pid: Some(100),
                started_at: None,
            }],
            1,
        );
        atlas.observe_citizens(
            "delta",
            vec![ServiceInfo {
                name: "maild-delta-200".into(),
                binary: None,
                version: None,
                git_sha: None,
                git_dirty: None,
                pid: Some(200),
                started_at: None,
            }],
            1,
        );
        atlas
    }

    fn traffic_event(direction: &str, from: Option<&str>, to: Option<&str>) -> TrafficEvent {
        TrafficEvent {
            seq: 1,
            ts: "2026-07-25T00:00:00Z".into(),
            direction: direction.into(),
            outcome: "delivered".into(),
            message_type: "request".into(),
            from: from.map(str::to_owned),
            to: to.map(str::to_owned),
            verb: Some("maild.info".into()),
            size: 90,
            correlation_id: None,
            rc: None,
            dropped_count: 0,
            payload: None,
            payload_omitted: None,
        }
    }

    #[test]
    fn only_declared_local_peers_receive_edges_and_inner_positions() {
        let mut atlas = AtlasState::default();
        assert!(atlas.apply_inventory(
            InventoryProjection::parse(include_str!("fixtures/inventory_verified.json")).unwrap(),
            1,
        ));
        atlas.observe_peers(
            PeersProjection::parse(
                r#"{"node":"alpha","peers":[{"name":"delta","wg_ip":"192.0.2.11","port":4200}]}"#,
            )
            .unwrap(),
        );
        let layout = positions(&atlas);
        assert_eq!(layout["alpha"], ORIGIN);
        assert!((layout["delta"].distance(ORIGIN) - INNER_RADIUS).abs() < 0.1);
        assert!((layout["storage"].distance(ORIGIN) - OUTER_RADIUS).abs() < 0.1);
        assert_eq!(
            atlas.local_edges(),
            [("alpha".to_string(), "delta".to_string())]
        );
    }

    #[test]
    fn traffic_maps_to_declared_edges_and_unattributable_local_indicator() {
        let atlas = activity_atlas();
        assert_eq!(
            activity_target(
                &traffic_event("mesh_out", Some("tower-bevy-100"), Some("maild.delta.bus")),
                &atlas
            ),
            ActivityTarget::Edge(edge_key("alpha", "delta"))
        );
        assert_eq!(
            activity_target(
                &traffic_event("mesh_in", Some("bridge-delta"), Some("tower-bevy-100")),
                &atlas
            ),
            ActivityTarget::Edge(edge_key("alpha", "delta"))
        );
        assert_eq!(
            activity_target(
                &traffic_event("local", Some("unknown"), Some("missing")),
                &atlas
            ),
            ActivityTarget::LocalIndicator
        );
        // A known inventory member without a declared local edge remains
        // honest: it does not fabricate an alpha↔storage canvas edge.
        assert_eq!(
            activity_target(
                &traffic_event("mesh_out", Some("alpha"), Some("storage")),
                &atlas
            ),
            ActivityTarget::LocalIndicator
        );
    }

    #[test]
    fn activity_decay_reaches_a_stable_idle_state() {
        let atlas = activity_atlas();
        let mut activity = TopologyActivity::default();
        activity.record(
            &traffic_event("mesh_out", Some("alpha"), Some("delta")),
            &atlas,
        );
        assert!(activity.is_hot());
        activity.decay(10.0);
        assert!(!activity.is_hot());
        let revision = activity.revision;
        activity.decay(1.0);
        assert_eq!(
            activity.revision, revision,
            "idle activity does no ticking work"
        );
    }

    #[test]
    fn stale_peers_cannot_authorise_a_specific_edge_pulse() {
        let mut atlas = activity_atlas();
        atlas.mark_all_remote_stale();
        let mut activity = TopologyActivity::default();
        let edge = edge_key("alpha", "delta");
        activity.record(
            &traffic_event("mesh_out", Some("alpha"), Some("maild.delta.bus")),
            &atlas,
        );
        assert_eq!(activity.edge_intensity(&edge), 0.0);
        assert!(activity.local_intensity() > 0.0);
    }

    #[test]
    fn stale_citizen_roster_cannot_resolve_a_service_to_a_node() {
        let mut atlas = activity_atlas();
        atlas.mark_node_citizens_unknown("delta", "timeout");
        assert_eq!(
            activity_target(
                &traffic_event("mesh_out", Some("tower-bevy-100"), Some("maild-delta-200")),
                &atlas,
            ),
            ActivityTarget::LocalIndicator
        );
    }

    #[test]
    fn stale_claimant_still_contests_service_name_uniqueness() {
        let mut atlas = activity_atlas();
        // Both delta (about to go stale) and beta (fresh) claim bare "maild".
        atlas.observe_citizens(
            "delta",
            vec![ServiceInfo {
                name: "maild".into(),
                binary: None,
                version: None,
                git_sha: None,
                git_dirty: None,
                pid: Some(200),
                started_at: None,
            }],
            1,
        );
        atlas.observe_citizens(
            "beta",
            vec![ServiceInfo {
                name: "maild".into(),
                binary: None,
                version: None,
                git_sha: None,
                git_dirty: None,
                pid: Some(300),
                started_at: None,
            }],
            1,
        );
        atlas.mark_node_citizens_unknown("delta", "timeout");
        // If stale delta were dropped before uniqueness, beta would become
        // falsely unique and traffic really from delta could pulse alpha↔beta.
        assert_eq!(endpoint_node("maild", &atlas), None);
        assert_eq!(
            activity_target(
                &traffic_event("mesh_in", Some("maild"), Some("tower-bevy-100")),
                &atlas,
            ),
            ActivityTarget::LocalIndicator
        );
    }
}
