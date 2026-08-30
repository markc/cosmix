//! Schema-versioned native persistence for Tower's filters and view state.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use bevy::app::{App, AppExit, Plugin, Startup, Update};
use bevy::ecs::message::MessageReader;
use bevy::prelude::{
    Entity, IntoScheduleConfigs, Query, Res, ResMut, Resource, Time, Timer, TimerMode,
};
use ctk::prelude::{DcsShellState, TopologyCanvasState};
use serde::{Deserialize, Serialize};

use crate::model::AtlasState;
use crate::traffic::{TrafficFilter, TrafficState};
use crate::ui::TowerUi;

pub(crate) const CURRENT_SCHEMA: u32 = 1;
pub(crate) const MAX_SAVED_FILTERS: usize = 32;
const MAX_FILTER_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct SidebarConfig {
    pub open: bool,
    pub pinned: bool,
    pub width: f32,
    pub active_panel: String,
}

impl SidebarConfig {
    fn left_default() -> Self {
        Self {
            open: true,
            pinned: true,
            width: 0.22,
            active_panel: "nodes".into(),
        }
    }

    fn right_default() -> Self {
        Self {
            open: true,
            pinned: true,
            width: 0.28,
            active_panel: "overview".into(),
        }
    }
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self::left_default()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct TopologyViewConfig {
    pub pan_x: f32,
    pub pan_y: f32,
    pub zoom: f32,
}

impl Default for TopologyViewConfig {
    fn default() -> Self {
        Self {
            pan_x: 0.0,
            pan_y: 0.0,
            zoom: 1.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct NamedTrafficFilter {
    pub name: String,
    pub filter: TrafficFilter,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct TowerConfig {
    // Deliberately required on the wire. Missing means legacy/corrupt, not v1.
    pub schema_version: u32,
    #[serde(default)]
    pub current_filter: TrafficFilter,
    #[serde(default)]
    pub active_filter: Option<String>,
    #[serde(default)]
    pub saved_filters: Vec<NamedTrafficFilter>,
    #[serde(default)]
    pub topology: TopologyViewConfig,
    #[serde(default)]
    pub left_sidebar: SidebarConfig,
    #[serde(default = "SidebarConfig::right_default")]
    pub right_sidebar: SidebarConfig,
}

impl Default for TowerConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA,
            current_filter: TrafficFilter::default(),
            active_filter: None,
            saved_filters: Vec::new(),
            topology: TopologyViewConfig::default(),
            left_sidebar: SidebarConfig::left_default(),
            right_sidebar: SidebarConfig::right_default(),
        }
    }
}

/// Runtime saved-filter catalogue. References to absent services are retained:
/// inventory is not part of this state and filters may legitimately match
/// nothing until that service returns.
#[derive(Resource, Clone, Debug, Default)]
pub(crate) struct SavedFilters {
    pub filters: BTreeMap<String, TrafficFilter>,
    pub active: Option<String>,
    pub draft_name: String,
    pub revision: u64,
}

impl SavedFilters {
    fn from_config(config: &TowerConfig) -> Self {
        let mut filters = BTreeMap::new();
        for named in config.saved_filters.iter().take(MAX_SAVED_FILTERS) {
            if let Some(name) = valid_filter_name(&named.name) {
                filters.insert(name, named.filter.clone());
            }
        }
        let active = config
            .active_filter
            .as_ref()
            .filter(|name| filters.contains_key(*name))
            .cloned();
        Self {
            filters,
            active,
            draft_name: String::new(),
            revision: 1,
        }
    }

    pub(crate) fn save_current(
        &mut self,
        name: &str,
        filter: &TrafficFilter,
    ) -> Result<String, String> {
        let name = valid_filter_name(name)
            .ok_or_else(|| "Filter name must contain 1-64 characters".to_string())?;
        if !self.filters.contains_key(&name) && self.filters.len() >= MAX_SAVED_FILTERS {
            return Err(format!(
                "At most {MAX_SAVED_FILTERS} saved filters are allowed"
            ));
        }
        self.filters.insert(name.clone(), filter.clone());
        self.active = Some(name.clone());
        self.draft_name.clear();
        self.bump();
        Ok(name)
    }

    pub(crate) fn select(&mut self, name: &str) -> Option<TrafficFilter> {
        let filter = self.filters.get(name)?.clone();
        self.active = Some(name.to_owned());
        self.bump();
        Some(filter)
    }

    pub(crate) fn delete(&mut self, name: &str) -> bool {
        if self.filters.remove(name).is_none() {
            return false;
        }
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
        self.bump();
        true
    }

    pub(crate) fn clear_active(&mut self) {
        if self.active.take().is_some() {
            self.bump();
        }
    }

    pub(crate) fn set_draft_name(&mut self, value: String) {
        if self.draft_name != value {
            self.draft_name = value;
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

fn valid_filter_name(value: &str) -> Option<String> {
    let value = value.trim();
    let count = value.chars().count();
    (count > 0 && count <= MAX_FILTER_NAME_CHARS).then(|| value.to_owned())
}

#[derive(Clone, Debug)]
struct ConfigFile {
    path: Option<PathBuf>,
    allow_save: bool,
    notice: Option<String>,
}

impl ConfigFile {
    fn load(path: Option<PathBuf>) -> (TowerConfig, Self) {
        let Some(path) = path else {
            return (
                TowerConfig::default(),
                Self {
                    path: None,
                    allow_save: false,
                    notice: None,
                },
            );
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => match cosmix_config::from_conf_mix_str::<TowerConfig>(&raw) {
                Ok(config) if config.schema_version == CURRENT_SCHEMA => {
                    let notice = sanitisation_notice(&config);
                    if let Some(notice) = &notice {
                        eprintln!(
                            "tower: refusing to overwrite state requiring sanitisation in {}: {notice}",
                            path.display()
                        );
                    }
                    (
                        config,
                        Self {
                            path: Some(path),
                            allow_save: notice.is_none(),
                            notice,
                        },
                    )
                }
                Ok(config) => {
                    let notice = format!("unsupported state schema {}", config.schema_version);
                    eprintln!(
                        "tower: refusing to overwrite {notice} in {}",
                        path.display()
                    );
                    (
                        TowerConfig::default(),
                        Self {
                            path: Some(path),
                            allow_save: false,
                            notice: Some(notice),
                        },
                    )
                }
                Err(error) => {
                    let notice = format!("invalid or legacy state: {error}");
                    eprintln!(
                        "tower: refusing to overwrite invalid or legacy state {}: {error}",
                        path.display()
                    );
                    (
                        TowerConfig::default(),
                        Self {
                            path: Some(path),
                            allow_save: false,
                            notice: Some(notice),
                        },
                    )
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                TowerConfig::default(),
                Self {
                    path: Some(path),
                    allow_save: true,
                    notice: None,
                },
            ),
            Err(error) => {
                let notice = format!("state unreadable: {error}");
                eprintln!("tower: cannot read state {}: {error}", path.display());
                (
                    TowerConfig::default(),
                    Self {
                        path: Some(path),
                        allow_save: false,
                        notice: Some(notice),
                    },
                )
            }
        }
    }

    fn save(&self, config: &TowerConfig) -> Result<(), String> {
        if !self.allow_save {
            return Ok(());
        }
        let Some(path) = &self.path else {
            return Ok(());
        };
        let content = cosmix_config::to_conf_mix_string(config)
            .map_err(|error| format!("serialising Tower state: {error}"))?;
        ctk::fs::write_atomic(path, content.as_bytes())
    }
}

fn sanitisation_notice(config: &TowerConfig) -> Option<String> {
    let mut reasons = Vec::new();
    if config.saved_filters.len() > MAX_SAVED_FILTERS {
        reasons.push(format!(
            "{} filters exceed the {MAX_SAVED_FILTERS}-filter limit",
            config.saved_filters.len()
        ));
    }
    let mut names = BTreeMap::new();
    for named in &config.saved_filters {
        let Some(normalised) = valid_filter_name(&named.name) else {
            reasons.push(format!("invalid filter name {:?}", named.name));
            continue;
        };
        if normalised != named.name {
            reasons.push(format!("filter name {:?} requires trimming", named.name));
        }
        if names.insert(normalised, ()).is_some() {
            reasons.push("duplicate filter names".into());
        }
    }
    if config
        .active_filter
        .as_ref()
        .is_some_and(|active| !names.contains_key(active))
    {
        reasons.push("active_filter does not name a retained filter".into());
    }
    // View state gets the same fail-closed treatment as the filter catalogue:
    // anything the runtime would silently clamp or normalise instead marks the
    // file read-only, so a hand-edited or corrupt value is never rewritten.
    // Bounds mirror the RUNTIME contract exactly (a value the runtime would
    // clamp is corrupt here): zoom = ctk TopologyCanvasProps::default()
    // 0.45..=2.5 (Tower spawns default props), width = DCS sane_width
    // 0.10..=1.0, panels = the DcsPanel ids ui.rs registers per side.
    if !(config.topology.zoom.is_finite() && (0.45..=2.5).contains(&config.topology.zoom)) {
        reasons.push(format!("zoom {} outside 0.45..=2.5", config.topology.zoom));
    }
    if !config.topology.pan_x.is_finite() || !config.topology.pan_y.is_finite() {
        reasons.push("non-finite pan".into());
    }
    for (side, sidebar, panels) in [
        ("left", &config.left_sidebar, &["nodes", "filters"][..]),
        (
            "right",
            &config.right_sidebar,
            &["overview", "citizens", "inspector", "properties", "traffic"][..],
        ),
    ] {
        if !(sidebar.width.is_finite() && (0.10..=1.0).contains(&sidebar.width)) {
            reasons.push(format!(
                "{side} sidebar width {} outside 0.10..=1.0",
                sidebar.width
            ));
        }
        if !panels.contains(&sidebar.active_panel.as_str()) {
            reasons.push(format!(
                "{side} sidebar names unknown panel {:?}",
                sidebar.active_panel
            ));
        }
        if sidebar.pinned && !sidebar.open {
            reasons.push(format!("{side} sidebar pinned while closed"));
        }
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

#[derive(Resource, Debug)]
pub(crate) struct TowerPersistence {
    pub initial: TowerConfig,
    load_notice: Option<String>,
    file: ConfigFile,
    last_observed: Option<TowerConfig>,
    pending: Option<TowerConfig>,
    settle: Timer,
}

pub(crate) struct TowerPersistencePlugin {
    path: Option<PathBuf>,
}

impl TowerPersistencePlugin {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }
}

impl Plugin for TowerPersistencePlugin {
    fn build(&self, app: &mut App) {
        let (config, file) = ConfigFile::load(self.path.clone());
        let saved = SavedFilters::from_config(&config);
        let filter = saved
            .active
            .as_ref()
            .and_then(|name| saved.filters.get(name))
            .cloned()
            .unwrap_or_else(|| config.current_filter.clone());
        let mut traffic = TrafficState::with_filter(filter);
        if let Some(notice) = &file.notice {
            traffic.notice(format!("Tower state loaded read-only: {notice}"));
        }
        let load_notice = file.notice.clone();
        app.insert_resource(traffic)
            .insert_resource(saved)
            .insert_resource(TowerPersistence {
                initial: config.clone(),
                load_notice,
                file,
                last_observed: Some(config),
                pending: None,
                settle: Timer::from_seconds(0.25, TimerMode::Once),
            })
            .add_systems(Startup, surface_load_notice)
            .add_systems(Update, (persist_state, flush_state_on_exit).chain());
    }
}

fn surface_load_notice(persistence: Res<TowerPersistence>, mut atlas: ResMut<AtlasState>) {
    if let Some(notice) = &persistence.load_notice {
        atlas.notice = Some(format!("Tower state loaded read-only: {notice}"));
        atlas.bump();
    }
}

fn snapshot(
    traffic: &TrafficState,
    saved: &SavedFilters,
    shell: &DcsShellState,
    canvas: &TopologyCanvasState,
) -> TowerConfig {
    let sidebar = |state: &ctk::prelude::DcsSidebarState, fallback: &str| SidebarConfig {
        open: state.open,
        pinned: state.pin_preference,
        width: state.width(),
        active_panel: state.active_panel_id().unwrap_or(fallback).to_string(),
    };
    TowerConfig {
        schema_version: CURRENT_SCHEMA,
        current_filter: traffic.filter.clone(),
        active_filter: saved.active.clone(),
        saved_filters: saved
            .filters
            .iter()
            .map(|(name, filter)| NamedTrafficFilter {
                name: name.clone(),
                filter: filter.clone(),
            })
            .collect(),
        topology: TopologyViewConfig {
            pan_x: canvas.pan().x,
            pan_y: canvas.pan().y,
            zoom: canvas.zoom(),
        },
        left_sidebar: sidebar(&shell.left, "nodes"),
        right_sidebar: sidebar(&shell.right, "overview"),
    }
}

fn snapshot_for_entities(
    traffic: &TrafficState,
    saved: &SavedFilters,
    shell_entity: Entity,
    canvas_entity: Entity,
    shells: &Query<&DcsShellState>,
    canvases: &Query<&TopologyCanvasState>,
) -> Option<TowerConfig> {
    Some(snapshot(
        traffic,
        saved,
        shells.get(shell_entity).ok()?,
        canvases.get(canvas_entity).ok()?,
    ))
}

fn persist_state(
    mut exits: MessageReader<AppExit>,
    time: Res<Time>,
    ui: Option<Res<TowerUi>>,
    traffic: Res<TrafficState>,
    saved: Res<SavedFilters>,
    view: (Query<&DcsShellState>, Query<&TopologyCanvasState>),
    mut persistence: ResMut<TowerPersistence>,
) {
    let exiting = exits.read().next().is_some();
    let Some(ui) = ui else {
        return;
    };
    let Some(current) = snapshot_for_entities(
        &traffic,
        &saved,
        ui.shell_root(),
        ui.topology_root(),
        &view.0,
        &view.1,
    ) else {
        return;
    };
    if persistence.last_observed.as_ref() != Some(&current) {
        persistence.last_observed = Some(current.clone());
        persistence.pending = Some(current);
        persistence.settle.reset();
    }
    if persistence.pending.is_none() {
        return;
    }
    persistence.settle.tick(time.delta());
    if exiting || !persistence.settle.is_finished() {
        // The chained shutdown hook owns exit writes so the wait stays
        // explicitly bounded even if the regular atomic writer blocks.
        return;
    }
    save_pending(&mut persistence);
}

fn save_pending(persistence: &mut TowerPersistence) {
    let Some(current) = persistence.pending.clone() else {
        return;
    };
    match persistence.file.save(&current) {
        Ok(()) => persistence.pending = None,
        Err(error) => {
            // Keep the exact snapshot dirty. A finished one-shot timer means
            // the next Update retries without waiting for another UI change.
            eprintln!("tower: {error}; state remains pending for retry");
        }
    }
}

fn flush_state_on_exit(
    mut exits: MessageReader<AppExit>,
    ui: Option<Res<TowerUi>>,
    traffic: Res<TrafficState>,
    saved: Res<SavedFilters>,
    view: (Query<&DcsShellState>, Query<&TopologyCanvasState>),
    mut persistence: ResMut<TowerPersistence>,
) {
    if exits.read().next().is_none() {
        return;
    }
    let Some(ui) = ui else {
        return;
    };
    if let Some(current) = snapshot_for_entities(
        &traffic,
        &saved,
        ui.shell_root(),
        ui.topology_root(),
        &view.0,
        &view.1,
    ) {
        persistence.last_observed = Some(current.clone());
        persistence.pending = Some(current);
    }
    let Some(current) = persistence.pending.clone() else {
        return;
    };
    flush_pending_bounded(&mut persistence, current, Duration::from_millis(250));
}

fn flush_pending_bounded(
    persistence: &mut TowerPersistence,
    current: TowerConfig,
    timeout: Duration,
) {
    let file = persistence.file.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(file.save(&current));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => persistence.pending = None,
        Ok(Err(error)) => {
            eprintln!("tower: shutdown state flush failed: {error}");
        }
        Err(error) => {
            eprintln!(
                "tower: shutdown state flush did not finish within {}ms ({error}); atomic writer may complete before process exit",
                timeout.as_millis()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traffic::{TrafficBody, TrafficDirection};
    use bevy::ecs::system::{Commands, RunSystemOnce};
    use bevy::prelude::Vec2;
    use ctk::prelude::{
        spawn_dcs_shell, spawn_topology_canvas, DcsShellProps, TopologyCanvasProps,
    };

    fn filter(service: &str) -> TrafficFilter {
        TrafficFilter {
            verb_glob: "maild.*".into(),
            service: Some(service.into()),
            direction: TrafficDirection::MeshIn,
            body: TrafficBody::Redacted,
        }
    }

    #[test]
    fn snapshot_targets_the_ui_entities_when_other_shells_exist() {
        let mut world = bevy::ecs::world::World::new();
        let (target_shell, target_canvas, other_shell, other_canvas) = world
            .run_system_once(|mut commands: Commands| {
                let spawn_shell = |commands: &mut Commands| {
                    let toolbar = commands.spawn_empty().id();
                    let centre = commands.spawn_empty().id();
                    spawn_dcs_shell(
                        commands,
                        DcsShellProps::new(toolbar, centre, Vec::new(), Vec::new()),
                    )
                    .root
                };
                (
                    spawn_shell(&mut commands),
                    spawn_topology_canvas(&mut commands, TopologyCanvasProps::default()).root,
                    spawn_shell(&mut commands),
                    spawn_topology_canvas(&mut commands, TopologyCanvasProps::default()).root,
                )
            })
            .unwrap();

        world
            .entity_mut(target_shell)
            .get_mut::<DcsShellState>()
            .unwrap()
            .left
            .set_width(0.37);
        {
            let mut entity = world.entity_mut(target_canvas);
            let mut canvas = entity.get_mut::<TopologyCanvasState>().unwrap();
            canvas.pan_by(Vec2::new(41.0, -13.0));
            canvas.zoom_by(0.4);
        }
        world
            .entity_mut(other_shell)
            .get_mut::<DcsShellState>()
            .unwrap()
            .left
            .set_width(0.71);
        world
            .entity_mut(other_canvas)
            .get_mut::<TopologyCanvasState>()
            .unwrap()
            .pan_by(Vec2::new(-99.0, 87.0));

        let snapshot = world
            .run_system_once(
                move |shells: Query<&DcsShellState>, canvases: Query<&TopologyCanvasState>| {
                    snapshot_for_entities(
                        &TrafficState::default(),
                        &SavedFilters::default(),
                        target_shell,
                        target_canvas,
                        &shells,
                        &canvases,
                    )
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.left_sidebar.width, 0.37);
        assert_eq!(snapshot.topology.pan_x, 41.0);
        assert_eq!(snapshot.topology.pan_y, -13.0);
        assert_eq!(snapshot.topology.zoom, 1.4);
    }

    #[test]
    fn state_round_trips_atomically_through_conf_mix() {
        let directory =
            std::env::temp_dir().join(format!("tower-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("state.conf.mix");
        let file = ConfigFile {
            path: Some(path.clone()),
            allow_save: true,
            notice: None,
        };
        let config = TowerConfig {
            saved_filters: vec![NamedTrafficFilter {
                name: "mail ingress".into(),
                filter: filter("maild"),
            }],
            active_filter: Some("mail ingress".into()),
            current_filter: filter("maild"),
            topology: TopologyViewConfig {
                pan_x: 42.0,
                pan_y: -9.0,
                zoom: 1.4,
            },
            ..TowerConfig::default()
        };
        file.save(&config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let reparsed: TowerConfig = cosmix_config::from_conf_mix_str(&raw).unwrap();
        assert_eq!(reparsed, config);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unsupported_schema_is_fail_closed_for_saves() {
        let directory =
            std::env::temp_dir().join(format!("tower-schema-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.conf.mix");
        std::fs::write(&path, "schema_version: 999\n").unwrap();
        let (config, file) = ConfigFile::load(Some(path.clone()));
        assert_eq!(config, TowerConfig::default());
        assert!(!file.allow_save);
        file.save(&TowerConfig::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "schema_version: 999\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_schema_is_legacy_and_never_overwritten() {
        let directory =
            std::env::temp_dir().join(format!("tower-missing-schema-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.conf.mix");
        let original = "saved_filters: []\n";
        std::fs::write(&path, original).unwrap();
        let (config, file) = ConfigFile::load(Some(path.clone()));
        assert_eq!(config, TowerConfig::default());
        assert!(!file.allow_save);
        assert!(file
            .notice
            .as_deref()
            .is_some_and(|notice| { notice.contains("invalid") || notice.contains("legacy") }));
        file.save(&TowerConfig::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn out_of_range_view_state_loads_read_only() {
        let mut config = TowerConfig::default();
        config.topology.zoom = 99.0;
        assert!(sanitisation_notice(&config)
            .expect("zoom 99 must be flagged")
            .contains("zoom"));

        let mut config = TowerConfig::default();
        config.left_sidebar.width = 9.0;
        assert!(sanitisation_notice(&config)
            .expect("width 9 must be flagged")
            .contains("width"));

        let mut config = TowerConfig::default();
        config.right_sidebar.active_panel = "future-pane".into();
        assert!(sanitisation_notice(&config)
            .expect("unknown panel must be flagged")
            .contains("future-pane"));

        // Runtime-boundary cases: values the runtime would clamp are flagged…
        let mut config = TowerConfig::default();
        config.topology.zoom = 10.0;
        assert!(sanitisation_notice(&config).is_some());
        let mut config = TowerConfig::default();
        config.left_sidebar.width = 0.07;
        assert!(sanitisation_notice(&config).is_some());
        // …while every real runtime value is accepted, including the Citizens
        // panel Tower actually registers (regression: rejecting own state).
        let mut config = TowerConfig::default();
        config.right_sidebar.active_panel = "citizens".into();
        config.topology.zoom = 2.5;
        config.left_sidebar.width = 0.10;
        assert!(sanitisation_notice(&config).is_none());

        assert!(sanitisation_notice(&TowerConfig::default()).is_none());
    }

    #[test]
    fn sanitised_catalogue_loads_read_only_without_rewriting_source() {
        let directory =
            std::env::temp_dir().join(format!("tower-sanitise-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.conf.mix");
        let mut config = TowerConfig::default();
        config.saved_filters.push(NamedTrafficFilter {
            name: " ".into(),
            filter: filter("invalid-name"),
        });
        for index in 0..=MAX_SAVED_FILTERS {
            config.saved_filters.push(NamedTrafficFilter {
                name: format!("filter-{index}"),
                filter: filter(&format!("service-{index}")),
            });
        }
        let original = cosmix_config::to_conf_mix_string(&config).unwrap();
        std::fs::write(&path, &original).unwrap();
        let (loaded, file) = ConfigFile::load(Some(path.clone()));
        assert_eq!(loaded, config);
        assert!(!file.allow_save);
        assert!(file
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("limit") && notice.contains("invalid")));
        assert!(SavedFilters::from_config(&loaded).filters.len() < MAX_SAVED_FILTERS);
        file.save(&TowerConfig::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_write_stays_pending_and_shutdown_flush_is_bounded() {
        let directory =
            std::env::temp_dir().join(format!("tower-retry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let current = TowerConfig::default();
        let mut persistence = TowerPersistence {
            initial: current.clone(),
            load_notice: None,
            file: ConfigFile {
                path: Some(directory.clone()),
                allow_save: true,
                notice: None,
            },
            last_observed: Some(current.clone()),
            pending: Some(current.clone()),
            settle: Timer::from_seconds(0.0, TimerMode::Once),
        };
        save_pending(&mut persistence);
        assert_eq!(persistence.pending, Some(current.clone()));

        persistence.file.path = Some(directory.join("state.conf.mix"));
        flush_pending_bounded(&mut persistence, current, Duration::from_secs(1));
        assert!(persistence.pending.is_none());
        assert!(directory.join("state.conf.mix").is_file());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saved_filters_save_select_and_delete_without_inventory_validation() {
        let mut saved = SavedFilters::default();
        let absent_service = filter("retired-service");
        assert_eq!(
            saved
                .save_current("Retired service", &absent_service)
                .unwrap(),
            "Retired service"
        );
        assert_eq!(saved.select("Retired service"), Some(absent_service));
        assert!(saved.delete("Retired service"));
        assert_eq!(saved.active, None);
        assert!(saved.filters.is_empty());
    }
}
