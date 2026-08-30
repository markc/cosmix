//! Tower's DCS shell and slice-gated model-to-view synchronisation.

use std::collections::{BTreeMap, BTreeSet};

use bevy::app::AppExit;
use bevy::color::Mix;
use bevy::ecs::observer::On;
use bevy::ecs::schedule::common_conditions::resource_changed;
use bevy::feathers::{dark_theme::create_dark_theme, theme::UiTheme};
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::EditableText;
use ctk::prelude::*;

use crate::config::{SavedFilters, TowerPersistence};
use crate::model::{
    AtlasNode, AtlasState, InventoryMember, NodeDaemons, PeersProjection, RefreshReason,
    ServiceInfo,
};
use crate::panes::{PaneEntities, PropertyDisclosureState, ToolbarEntities, TowerFocusKey};
use crate::props::PropsSurface;
use crate::topology::{EdgeKey, TopologyActivity};
use crate::traffic::{TrafficIntent, TrafficState};

type StatusSignature = (
    BusConnectionState,
    String,
    Option<u64>,
    Option<u64>,
    Option<String>,
    bool,
    Option<RefreshReason>,
    Option<String>,
);
type NodeSignature = (Vec<AtlasNode>, Option<String>);
type TopologySignature = (
    Vec<(InventoryMember, &'static str)>,
    Option<PeersProjection>,
);
type OverviewSignature = (Option<AtlasNode>, Option<NodeDaemons>);
type CitizensSignature = (Option<String>, Option<String>, Vec<ServiceInfo>);
type InspectorSignature = (
    Option<crate::inspector::CitizenInspector>,
    BTreeSet<crate::inspector::MutationTarget>,
);
type PropertiesSignature = (
    Option<String>,
    Option<String>,
    BTreeMap<String, PropsSurface>,
);

#[derive(Resource)]
pub(crate) struct TowerUi {
    toolbar: ToolbarEntities,
    status_text: Entity,
    panes: PaneEntities,
    topology: TopologyCanvasEntities,
    shell: Entity,
    last_revision: u64,
    last_traffic_revision: u64,
    status: Option<StatusSignature>,
    nodes: Option<NodeSignature>,
    filters: Option<(usize, usize, u64)>,
    overview: Option<OverviewSignature>,
    citizens: Option<CitizensSignature>,
    inspector: Option<InspectorSignature>,
    properties: Option<PropertiesSignature>,
    topology_signature: Option<TopologySignature>,
    topology_edges: BTreeMap<EdgeKey, Entity>,
    local_activity_indicator: Option<Entity>,
}

impl TowerUi {
    pub(crate) fn shell_root(&self) -> Entity {
        self.shell
    }

    pub(crate) fn topology_root(&self) -> Entity {
        self.topology.root
    }
}

pub(crate) struct TowerUiPlugin;

impl Plugin for TowerUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PropertyDisclosureState>()
            .init_resource::<crate::panes::ObservedAgeRefresh>()
            .add_observer(crate::panes::on_property_disclosure_changed)
            .add_systems(Update, crate::panes::refresh_observed_age_texts)
            .add_systems(
                Update,
                (
                    sync_traffic_pane,
                    sync_topology_selection,
                    sync_filter_draft,
                    sync_theme_menu_presentation.run_if(resource_changed::<ThemeState>),
                    decay_topology_activity.run_if(activity_is_hot),
                    render_model,
                    sync_topology_activity.run_if(resource_changed::<TopologyActivity>),
                )
                    .chain(),
            );
    }
}

pub(crate) fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    mut metrics: ResMut<CtkThemeMetrics>,
    asset_server: Res<AssetServer>,
    persistence: Res<TowerPersistence>,
) {
    *theme = UiTheme(create_dark_theme());
    let app_dirs = AppDirs::resolve(crate::identity::IDENTITY.slug);
    let theme_dir = app_dirs.as_ref().map(|dirs| dirs.config());
    let spec = resolve_app_theme(theme_dir.as_deref());
    *metrics = spec.metrics.clone();
    apply_theme(&mut theme, &mut theme_state, &spec);

    commands.spawn(Camera2d);
    let icons = IconSet::load(&asset_server);
    let toolbar = crate::panes::spawn_toolbar(&mut commands, &icons, &theme);
    let status = spawn_status_bar(&mut commands, "Connecting to local noded...");
    commands
        .entity(status.text)
        .insert(crate::panes::ObservedAgeText::status(
            "Connecting to local noded - ",
            None,
            "",
        ));
    let panes = crate::panes::spawn_panels(&mut commands);
    let topology = spawn_topology_canvas(&mut commands, TopologyCanvasProps::default());
    let initial = persistence.initial.clone();
    let menu_bar = spawn_menu_bar_with_icons(&mut commands, &menu_defs(), &icons, &theme);
    commands
        .entity(menu_bar)
        .observe(on_menu_application)
        .observe(on_menu_theme);
    let shell = spawn_dcs_app_shell_with_icons(
        &mut commands,
        DcsAppShellProps::new(DcsShellProps {
            toolbar: toolbar.root,
            centre: topology.root,
            left_panels: vec![
                DcsPanel::new("nodes", "Nodes", panes.nodes),
                DcsPanel::new("filters", "Filters", panes.filters),
            ],
            right_panels: vec![
                DcsPanel::new("overview", "Overview", panes.overview),
                DcsPanel::new("citizens", "Citizens", panes.citizens),
                DcsPanel::new("inspector", "Inspector", panes.inspector),
                DcsPanel::new("properties", "Properties", panes.properties),
                DcsPanel::new("traffic", "Traffic", panes.traffic),
            ],
            left_width: initial.left_sidebar.width,
            right_width: initial.right_sidebar.width,
            left_open: initial.left_sidebar.open,
            right_open: initial.right_sidebar.open,
            left_pinned: initial.left_sidebar.pinned,
            right_pinned: initial.right_sidebar.pinned,
            pin_breakpoint: 1120.0,
            left_controls: None,
            right_controls: None,
        })
        .with_menu_bar(menu_bar)
        .with_status_bar(status.root),
        &icons,
        &theme,
    );
    let shell_root = shell.dcs.root;
    let canvas_root = topology.root;
    commands.queue(move |world: &mut World| {
        if let Some(mut shell) = world.get_mut::<DcsShellState>(shell_root) {
            shell
                .left
                .select_panel_id(&initial.left_sidebar.active_panel);
            shell
                .right
                .select_panel_id(&initial.right_sidebar.active_panel);
        }
        if let Some(mut canvas) = world.get_mut::<TopologyCanvasState>(canvas_root) {
            canvas.pan_by(Vec2::new(initial.topology.pan_x, initial.topology.pan_y));
            let current = canvas.zoom();
            canvas.zoom_by(initial.topology.zoom - current);
        }
    });
    commands.insert_resource(icons);
    commands.insert_resource(TowerUi {
        toolbar,
        status_text: status.text,
        panes,
        topology,
        shell: shell.dcs.root,
        last_revision: u64::MAX,
        last_traffic_revision: u64::MAX,
        status: None,
        nodes: None,
        filters: None,
        overview: None,
        citizens: None,
        inspector: None,
        properties: None,
        topology_signature: None,
        topology_edges: BTreeMap::new(),
        local_activity_indicator: None,
    });
}

const MENU_FILE_REFRESH: &str = "tower.file.refresh-mesh";
const MENU_FILE_QUIT: &str = "tower.file.quit";
const MENU_VIEW_RESET_TOPOLOGY: &str = "tower.view.reset-topology";
const MENU_THEME_MODE: &str = "tower.theme.mode";
const MENU_THEME_OCEAN: &str = "tower.theme.ocean";
const MENU_THEME_CRIMSON: &str = "tower.theme.crimson";
const MENU_THEME_STONE: &str = "tower.theme.stone";
const MENU_THEME_FOREST: &str = "tower.theme.forest";
const MENU_THEME_SUNSET: &str = "tower.theme.sunset";
const MENU_THEME_MONO: &str = "tower.theme.mono";

fn menu_defs() -> Vec<MenuDef> {
    vec![
        MenuDef {
            label: "File".into(),
            items: vec![
                MenuItemDef::new(MENU_FILE_REFRESH, "Refresh Mesh").with_icon(Icon::Refresh),
                MenuItemDef::new(MENU_FILE_QUIT, "Quit").with_icon(Icon::LogOut),
            ],
        },
        MenuDef {
            label: "View".into(),
            items: vec![
                MenuItemDef::new(MENU_VIEW_RESET_TOPOLOGY, "Reset Topology").with_icon(Icon::Grid)
            ],
        },
        MenuDef {
            label: "Themes".into(),
            items: vec![
                MenuItemDef::new(MENU_THEME_MODE, "Dark Mode").with_icon(Icon::Eye),
                MenuItemDef::new(MENU_THEME_OCEAN, "Ocean").with_icon(Icon::Grid),
                MenuItemDef::new(MENU_THEME_CRIMSON, "Crimson").with_icon(Icon::Grid),
                MenuItemDef::new(MENU_THEME_STONE, "Stone").with_icon(Icon::Grid),
                MenuItemDef::new(MENU_THEME_FOREST, "Forest").with_icon(Icon::Grid),
                MenuItemDef::new(MENU_THEME_SUNSET, "Sunset").with_icon(Icon::Grid),
                MenuItemDef::new(MENU_THEME_MONO, "Mono").with_icon(Icon::List),
            ],
        },
    ]
}

fn on_menu_application(
    activation: On<MenuActivated>,
    ui: Res<TowerUi>,
    mut canvases: Query<&mut TopologyCanvasState>,
    mut refresh: MessageWriter<crate::atlas::RefreshMesh>,
    mut exit: MessageWriter<AppExit>,
) {
    match activation.id {
        MENU_FILE_REFRESH => {
            refresh.write(crate::atlas::RefreshMesh);
        }
        MENU_FILE_QUIT => {
            exit.write(AppExit::Success);
        }
        MENU_VIEW_RESET_TOPOLOGY => {
            if let Ok(mut canvas) = canvases.get_mut(ui.topology.root) {
                canvas.reset_view();
            }
        }
        _ => {}
    }
}

fn on_menu_theme(
    activation: On<MenuActivated>,
    theme: Res<ThemeState>,
    mut apply: MessageWriter<ApplyTheme>,
    mut persist: MessageWriter<ThemeWriteRequest>,
) {
    let Some((scheme, mode)) = theme_selection(activation.id, theme.scheme, theme.mode) else {
        return;
    };
    let app_dirs = AppDirs::resolve(crate::identity::IDENTITY.slug);
    let theme_dir = app_dirs.as_ref().map(|dirs| dirs.config());
    apply.write(ApplyTheme(resolve_app_theme_with_selection(
        theme_dir.as_deref(),
        scheme,
        mode,
    )));
    persist.write(ThemeWriteRequest::shared(scheme, mode));
}

fn theme_selection(id: &str, scheme: Scheme, mode: Mode) -> Option<(Scheme, Mode)> {
    if id == MENU_THEME_MODE {
        return Some((
            scheme,
            if mode == Mode::Dark {
                Mode::Light
            } else {
                Mode::Dark
            },
        ));
    }
    let scheme = match id {
        MENU_THEME_OCEAN => Scheme::Ocean,
        MENU_THEME_CRIMSON => Scheme::Crimson,
        MENU_THEME_STONE => Scheme::Stone,
        MENU_THEME_FOREST => Scheme::Forest,
        MENU_THEME_SUNSET => Scheme::Sunset,
        MENU_THEME_MONO => Scheme::Mono,
        _ => return None,
    };
    Some((scheme, mode))
}

fn sync_theme_menu_presentation(
    theme: Res<ThemeState>,
    mut presentation: ResMut<MenuPresentation>,
) {
    let scheme_id = match theme.scheme {
        Scheme::Ocean => MENU_THEME_OCEAN,
        Scheme::Crimson => MENU_THEME_CRIMSON,
        Scheme::Stone => MENU_THEME_STONE,
        Scheme::Forest => MENU_THEME_FOREST,
        Scheme::Sunset => MENU_THEME_SUNSET,
        Scheme::Mono => MENU_THEME_MONO,
    };
    let mut items = vec![(
        scheme_id,
        MenuItemPresentation {
            enabled: true,
            marker: MenuItemMarker::Radio,
        },
    )];
    if theme.mode == Mode::Dark {
        items.push((
            MENU_THEME_MODE,
            MenuItemPresentation {
                enabled: true,
                marker: MenuItemMarker::Checked,
            },
        ));
    }
    presentation.replace(theme.revision, items);
}

fn activity_is_hot(activity: Option<Res<TopologyActivity>>) -> bool {
    activity.as_deref().is_some_and(TopologyActivity::is_hot)
}

fn decay_topology_activity(time: Res<Time>, mut activity: ResMut<TopologyActivity>) {
    activity.decay(time.delta_secs());
}

fn sync_filter_draft(
    inputs: Query<&EditableText, (With<crate::panes::FilterNameInput>, Changed<EditableText>)>,
    mut saved: ResMut<SavedFilters>,
) {
    if let Ok(input) = inputs.single() {
        saved.set_draft_name(input.value().to_string());
    }
}

fn sync_topology_activity(
    ui: Option<Res<TowerUi>>,
    activity: Res<TopologyActivity>,
    theme: Res<UiTheme>,
    mut backgrounds: Query<&mut BackgroundColor>,
) {
    let Some(ui) = ui else {
        return;
    };
    let base = theme.color(&ctk::theme::tokens::BORDER);
    let accent = theme.color(&ctk::theme::tokens::CONTROL_ACTIVE);
    for (edge, entity) in &ui.topology_edges {
        if let Ok(mut background) = backgrounds.get_mut(*entity) {
            let desired = base.mix(&accent, activity.edge_intensity(edge));
            if background.0 != desired {
                background.0 = desired;
            }
        }
    }
    if let Some(indicator) = ui.local_activity_indicator {
        if let Ok(mut background) = backgrounds.get_mut(indicator) {
            let intensity = activity.local_intensity();
            let desired = if intensity > 0.0 {
                accent.with_alpha(0.25 + intensity * 0.75)
            } else {
                Color::NONE
            };
            if background.0 != desired {
                background.0 = desired;
            }
        }
    }
}

fn sync_traffic_pane(
    ui: Option<Res<TowerUi>>,
    shells: Query<&DcsShellState>,
    mut previous: Local<Option<bool>>,
    mut intents: bevy::ecs::message::MessageWriter<TrafficIntent>,
) {
    let Some(ui) = ui else {
        return;
    };
    let Ok(shell) = shells.get(ui.shell) else {
        return;
    };
    let open = shell.right.open && shell.right.active_panel_id() == Some("traffic");
    if *previous != Some(open) {
        *previous = Some(open);
        intents.write(TrafficIntent::SetOpen(open));
    }
}

fn sync_topology_selection(
    ui: Option<Res<TowerUi>>,
    canvases: Query<&TopologyCanvasState>,
    mut atlas: ResMut<AtlasState>,
) {
    let Some(ui) = ui else {
        return;
    };
    let Ok(canvas) = canvases.get(ui.topology.root) else {
        return;
    };
    if let Some(selected) = canvas.selected() {
        if atlas.selected.as_deref() != Some(selected) {
            atlas.select(selected.to_owned());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_model(
    mut commands: Commands,
    ui: Option<ResMut<TowerUi>>,
    atlas: Res<AtlasState>,
    traffic: Res<TrafficState>,
    saved_filters: Res<SavedFilters>,
    disclosure: Res<PropertyDisclosureState>,
    input_focus: Res<InputFocus>,
    focus_keys: Query<&TowerFocusKey>,
    mut statuses: Query<&mut StatusText>,
    mut ages: Query<&mut crate::panes::ObservedAgeText>,
    mut canvases: Query<&mut TopologyCanvasState>,
) {
    let Some(mut ui) = ui else {
        return;
    };
    if ui.last_revision == atlas.revision && ui.last_traffic_revision == traffic.revision {
        return;
    }
    let retained_focus = input_focus
        .get()
        .and_then(|entity| focus_keys.get(entity).ok())
        .cloned();
    let mut focus_targets = Vec::new();

    let status = status_signature(&atlas);
    if ui.status.as_ref() != Some(&status) {
        crate::panes::render_status(
            &mut commands,
            &mut statuses,
            &mut ages,
            ui.status_text,
            ui.toolbar,
            &atlas,
        );
        ui.status = Some(status);
    }

    let nodes = node_signature(&atlas);
    if ui.nodes.as_ref() != Some(&nodes) {
        focus_targets.extend(crate::panes::render_nodes(
            &mut commands,
            ui.panes.nodes,
            &atlas,
        ));
        ui.nodes = Some(nodes);
    }

    let filters = (
        atlas.nodes.len(),
        atlas
            .nodes
            .values()
            .filter(|node| node.member.active_bus())
            .count(),
        saved_filters.revision,
    );
    if ui.filters != Some(filters) {
        focus_targets.extend(crate::panes::render_filters(
            &mut commands,
            ui.panes.filters,
            &atlas,
            &saved_filters,
        ));
        ui.filters = Some(filters);
    }

    let selected_node = atlas
        .selected
        .as_ref()
        .and_then(|name| atlas.nodes.get(name))
        .cloned();
    let overview = (
        selected_node,
        atlas
            .selected
            .as_ref()
            .and_then(|node| atlas.daemons.get(node))
            .cloned(),
    );
    if ui.overview.as_ref() != Some(&overview) {
        focus_targets.extend(crate::panes::render_overview(
            &mut commands,
            ui.panes.overview,
            &atlas,
        ));
        ui.overview = Some(overview);
    }

    let citizens = citizens_signature(&atlas);
    if ui.citizens.as_ref() != Some(&citizens) {
        focus_targets.extend(crate::panes::render_citizens(
            &mut commands,
            ui.panes.citizens,
            &atlas,
        ));
        ui.citizens = Some(citizens);
    }

    let inspector = (atlas.inspector.clone(), atlas.active_mutations.clone());
    if ui.inspector.as_ref() != Some(&inspector) {
        focus_targets.extend(crate::panes::render_inspector(
            &mut commands,
            ui.panes.inspector,
            &atlas,
        ));
        ui.inspector = Some(inspector);
    }

    let properties = properties_signature(&atlas);
    if ui.properties.as_ref() != Some(&properties) {
        focus_targets.extend(crate::panes::render_properties(
            &mut commands,
            ui.panes.properties,
            &atlas,
            &disclosure,
        ));
        ui.properties = Some(properties);
    }

    if ui.last_traffic_revision != traffic.revision {
        focus_targets.extend(crate::panes::render_traffic(
            &mut commands,
            ui.panes.traffic,
            &traffic,
        ));
        ui.last_traffic_revision = traffic.revision;
    }

    let topology_signature = topology_signature(&atlas);
    if ui.topology_signature.as_ref() != Some(&topology_signature) {
        let build = crate::topology::rebuild(&mut commands, ui.topology, &atlas);
        focus_targets.extend(build.focus);
        ui.topology_edges = build.edges;
        ui.local_activity_indicator = build.local_indicator;
        commands.queue(|world: &mut World| {
            world.resource_mut::<TopologyActivity>().touch();
        });
        ui.topology_signature = Some(topology_signature);
    }
    if let Some(selected) = &atlas.selected {
        if let Ok(mut canvas) = canvases.get_mut(ui.topology.root) {
            if canvas.selected() != Some(selected.as_str()) {
                canvas.select(selected.clone());
            }
        }
    }

    if let Some(retained_focus) = retained_focus {
        if let Some((_, entity)) = focus_targets
            .into_iter()
            .find(|(key, _)| key == &retained_focus)
        {
            commands.queue(move |world: &mut World| {
                world
                    .resource_mut::<InputFocus>()
                    .set(entity, FocusCause::Navigated);
            });
        }
    }
    ui.last_revision = atlas.revision;
}

fn status_signature(atlas: &AtlasState) -> StatusSignature {
    (
        atlas.connection,
        atlas.mesh_posture.clone(),
        atlas
            .inventory
            .as_ref()
            .and_then(|inventory| inventory.epoch),
        atlas.inventory_observed_at_ms,
        atlas.mesh_reason.clone(),
        atlas.refreshing,
        atlas.last_refresh_reason,
        atlas.notice.clone(),
    )
}

fn node_signature(atlas: &AtlasState) -> NodeSignature {
    (
        atlas.nodes.values().cloned().collect(),
        atlas.selected.clone(),
    )
}

fn citizens_signature(atlas: &AtlasState) -> CitizensSignature {
    let citizens = atlas
        .selected
        .as_ref()
        .and_then(|name| atlas.nodes.get(name))
        .map_or_else(Vec::new, |node| node.citizens.clone());
    (
        atlas.selected.clone(),
        atlas.selected_citizen.clone(),
        citizens,
    )
}

fn properties_signature(atlas: &AtlasState) -> PropertiesSignature {
    (
        atlas.selected.clone(),
        atlas.local_node().map(str::to_owned),
        atlas.properties.clone(),
    )
}

fn topology_signature(atlas: &AtlasState) -> TopologySignature {
    (
        atlas
            .nodes
            .values()
            .map(|node| (node.member.clone(), node.status_label()))
            .collect(),
        atlas.peers.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tower_menu_declares_file_view_and_theme_commands() {
        let menus = menu_defs();
        let labels: Vec<_> = menus.iter().map(|menu| menu.label.as_str()).collect();
        assert_eq!(labels, ["File", "View", "Themes"]);
        let ids: Vec<_> = menus
            .iter()
            .flat_map(|menu| menu.items.iter().map(|item| item.id))
            .collect();
        assert_eq!(
            ids,
            [
                MENU_FILE_REFRESH,
                MENU_FILE_QUIT,
                MENU_VIEW_RESET_TOPOLOGY,
                MENU_THEME_MODE,
                MENU_THEME_OCEAN,
                MENU_THEME_CRIMSON,
                MENU_THEME_STONE,
                MENU_THEME_FOREST,
                MENU_THEME_SUNSET,
                MENU_THEME_MONO,
            ]
        );
    }

    #[test]
    fn theme_menu_preserves_the_other_selection_dimension() {
        assert_eq!(
            theme_selection(MENU_THEME_MODE, Scheme::Forest, Mode::Dark),
            Some((Scheme::Forest, Mode::Light))
        );
        assert_eq!(
            theme_selection(MENU_THEME_CRIMSON, Scheme::Forest, Mode::Light),
            Some((Scheme::Crimson, Mode::Light))
        );
        assert_eq!(
            theme_selection("tower.unknown", Scheme::Ocean, Mode::Dark),
            None
        );
    }
}
