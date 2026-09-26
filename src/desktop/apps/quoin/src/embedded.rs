//! Quoin hosted in an existing Bevy renderer, without Wayland panel surfaces.
//!
//! The compositor supplies output geometry, visibility and pointer samples.
//! Application content, chrome, persistence and the `shell` Bus service are
//! shared with the standalone layer-shell executable.
use bevy::{
    picking::events::{Cancel, Drag, DragEnd, DragStart, Pointer},
    prelude::*,
};
use cosmix_shell::{
    chrome::{QuoinPanelMounts, QuoinResizeGrip},
    core::{
        CornerDetector, CornerDetectorConfig, Edge, LogicalPoint, LogicalSize, OutputKey,
        PanelInput, PanelMode, PointerSample, ShellModel,
    },
    host::{PanelRect, panel_layout},
    runtime::{
        ShellCommand, ShellCommandKind, ShellFrameState, ShellRuntimePlugin, ShellRuntimeSet,
        replace_shell_model,
    },
};
use ctk::bus::{BusBridgeConfig, provenance_from_build, resolve_noded_url};
use std::time::Duration;

#[derive(Resource)]
pub(crate) struct EmbeddedPanelMounts(pub QuoinPanelMounts);

/// The output identity used until the host reports the real one. It lives in
/// the non-persistent `wl-output-` namespace (see `state`'s identity rule):
/// the placeholder never restores, claims or persists state. The first real
/// connector observation replaces it and restores then, so the migrated
/// legacy default entry stays unclaimed until a real output can take it.
const PLACEHOLDER_OUTPUT: &str = "wl-output-embedded";

/// Host updates this before `ShellRuntimeSet::Input`. Coordinates are logical.
#[derive(Resource, Default)]
pub struct EmbeddedOutput {
    pub camera: Option<Entity>,
    pub size: Vec2,
    pub name: String,
    pub active: bool,
    pub pointer: Option<Vec2>,
}

/// Visible panel rectangles, including the chrome animation offset. Hosts use
/// these for input ownership, never the full invisible mount rectangles.
#[derive(Resource, Default)]
pub struct EmbeddedPanelRegions(pub Vec<PanelRect>);
#[derive(Resource, Default)]
pub struct EmbeddedWorkArea(pub Option<PanelRect>);

#[derive(Resource)]
struct EmbeddedHost {
    detector: CornerDetector,
    name: String,
    size: Vec2,
}

#[derive(Resource, Default)]
struct GripDrag(Option<(Edge, f32)>);

/// Quoin hosted inside the compositor's renderer. `comp_service` names the
/// compositor's registered Bus service (default `comp`) for the hotspot
/// observer's deadzone mirror, supplied by the host the same way the
/// layer host takes `--comp-service`.
pub struct EmbeddedQuoinPlugin {
    comp_service: String,
}

impl Default for EmbeddedQuoinPlugin {
    fn default() -> Self {
        Self {
            comp_service: "comp".to_owned(),
        }
    }
}

impl EmbeddedQuoinPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_comp_service(mut self, service: impl Into<String>) -> Self {
        self.comp_service = service.into();
        self
    }
}

impl Plugin for EmbeddedQuoinPlugin {
    fn build(&self, app: &mut App) {
        let config = crate::config::startup_config(false);
        let store = crate::state::StateStore::startup(false);
        let bus = BusBridgeConfig::new("shell", resolve_noded_url());
        self.configure(app, config, store, bus);
    }
}

impl EmbeddedQuoinPlugin {
    /// Shared production assembly; callers supply startup I/O so the full
    /// application can also be exercised without a display or a live broker.
    fn configure(
        &self,
        app: &mut App,
        config: crate::config::ShellConfig,
        store: crate::state::StateStore,
        mut bus: BusBridgeConfig,
    ) {
        let registry = crate::startup_page_registry(&config);
        // The placeholder model restores nothing: comp has not named the
        // output yet, and claiming under a placeholder identity would take
        // the migrated legacy entry away from the real connector. `prepare`
        // restores when the first real observation arrives.
        let mut model = model(PLACEHOLDER_OUTPUT, Vec2::new(1920.0, 1080.0), &registry);
        model.start_intro(Duration::from_secs(2));
        app.add_plugins(ShellRuntimePlugin::new(model));
        let mounts: [Entity; 4] = std::array::from_fn(|i| {
            app.world_mut()
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        display: Display::None,
                        ..default()
                    },
                    panel_z_index(Edge::ALL[i], PanelMode::Hidden),
                ))
                .id()
        });
        app.insert_resource(EmbeddedPanelMounts(QuoinPanelMounts::new(
            mounts[0], mounts[1], mounts[2], mounts[3],
        )))
        .init_resource::<EmbeddedOutput>()
        .init_resource::<EmbeddedPanelRegions>()
        .init_resource::<EmbeddedWorkArea>()
        .init_resource::<GripDrag>()
        .insert_resource(EmbeddedHost {
            detector: CornerDetector::new(
                CornerDetectorConfig::new(8.0, Duration::from_millis(250), 100.0)
                    .expect("valid corner tuning"),
            ),
            name: PLACEHOLDER_OUTPUT.into(),
            size: Vec2::new(1920.0, 1080.0),
        });
        crate::hotspot::install(app, &mut bus, self.comp_service.clone());
        crate::hotspot::arm_first_run(app, store.first_run());
        bus.provenance = provenance_from_build(cosmix_buildinfo::build_info!());
        bus.inbound_prefixes.push("shell.".into());
        bus.subscriptions.push("noded.props.changed".into());
        crate::configure_content(app, bus, registry, store, false, false, config);
        // Output preparation runs BEFORE the Bus dispatch drains: a
        // dispatch reserves its registry seat and queues its command
        // against the current frame's output, so the model replacement
        // must land first or the command targets an output the Model stage
        // would drop — a reserved seat no page ever fills, and an acked
        // removal lost.
        app.add_systems(
            Update,
            prepare
                .in_set(ShellRuntimeSet::Input)
                .before(crate::bus_service::ShellBusDispatch),
        )
            .add_systems(
                Update,
                (present, present_dialog).chain().in_set(ShellRuntimeSet::Host),
            )
            .add_observer(grip_start)
            .add_observer(grip_move)
            .add_observer(grip_end)
            .add_observer(grip_cancel);
        tracing_notice();
    }
}

fn command(frame: &ShellFrameState, time: &Time<Real>, kind: ShellCommandKind) -> ShellCommand {
    ShellCommand {
        output: frame.0.geometry.output.clone(),
        at: time.elapsed(),
        kind,
    }
}

fn grip_start(
    event: On<Pointer<DragStart>>,
    grips: Query<&QuoinResizeGrip>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut drag: ResMut<GripDrag>,
    mut commands: MessageWriter<ShellCommand>,
) {
    let Ok(grip) = grips.get(event.entity) else {
        return;
    };
    if event.button != bevy::picking::pointer::PointerButton::Primary {
        return;
    }
    drag.0 = Some((grip.0, frame.0.panel(grip.0).thickness_px));
    commands.write(command(
        &frame,
        &time,
        ShellCommandKind::Panel {
            edge: grip.0,
            input: PanelInput::ResizeStarted,
        },
    ));
}

fn grip_move(
    event: On<Pointer<Drag>>,
    grips: Query<&QuoinResizeGrip>,
    drag: Res<GripDrag>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    scale: Res<UiScale>,
    mut commands: MessageWriter<ShellCommand>,
) {
    if grips.get(event.entity).is_err() {
        return;
    }
    let Some((edge, start)) = drag.0 else {
        return;
    };
    let distance = event.distance / scale.0;
    let delta = match edge {
        Edge::Left => distance.x,
        Edge::Right => -distance.x,
        Edge::Top => distance.y,
        Edge::Bottom => -distance.y,
    };
    commands.write(command(
        &frame,
        &time,
        ShellCommandKind::Resize {
            edge,
            thickness_px: (start + delta).max(1.0),
        },
    ));
}

fn grip_end(
    event: On<Pointer<DragEnd>>,
    grips: Query<&QuoinResizeGrip>,
    mut drag: ResMut<GripDrag>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    if grips.get(event.entity).is_err() {
        return;
    }
    if let Some((edge, _)) = drag.0.take() {
        commands.write(command(
            &frame,
            &time,
            ShellCommandKind::Panel {
                edge,
                input: PanelInput::ResizeCompleted,
            },
        ));
    }
}

fn grip_cancel(
    _event: On<Pointer<Cancel>>,
    mut drag: ResMut<GripDrag>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    if let Some((edge, _)) = drag.0.take() {
        commands.write(command(
            &frame,
            &time,
            ShellCommandKind::Panel {
                edge,
                input: PanelInput::ResizeCancelled,
            },
        ));
    }
}

fn tracing_notice() {
    eprintln!("QUOIN_EMBEDDED_ENABLED renderer=comp panels=4 bus=shell");
}

fn model(name: &str, size: Vec2, registry: &cosmix_shell::chrome::QuoinPageRegistry) -> ShellModel {
    let mut model = ShellModel::new(
        OutputKey::new(name).expect("valid native output name"),
        LogicalSize::new(size.x, size.y).expect("positive native output size"),
        Duration::ZERO,
        Duration::from_millis(800),
        Duration::from_millis(200),
    )
    .expect("valid shell timing");
    model.suppress_empty_edges(registry.declarations_only());
    for edge in Edge::ALL {
        model.set_carousel(edge, registry.carousel(edge));
    }
    model
}

fn prepare(world: &mut World) {
    let output = world.resource::<EmbeddedOutput>();
    let (active, size, name, pointer) = (
        output.active,
        output.size,
        output.name.clone(),
        output.pointer,
    );
    if !active || size.min_element() <= 0.0 || !size.is_finite() || OutputKey::new(&name).is_err() {
        return;
    }
    let host = world.resource::<EmbeddedHost>();
    if host.name != name || host.size != size {
        let mut replacement = model(&name, size, world.resource());
        if host.name != name {
            // A different output restores its own remembered state (per-
            // (output, edge) persistence); a same-output resize keeps the
            // fresh-model rebuild.
            world.resource::<crate::state::StateStore>().restore(&mut replacement);
            // Replacing the placeholder is the first real observation:
            // replay the startup intro on the output the user can see (the
            // placeholder never renders).
            if host.name == PLACEHOLDER_OUTPUT {
                replacement.start_intro(Duration::from_secs(2));
            }
        }
        replace_shell_model(world, replacement);
        let mut host = world.resource_mut::<EmbeddedHost>();
        host.name = name.clone();
        host.size = size;
    }
    let now = world.resource::<Time<Real>>().elapsed();
    let events = {
        let mut host = world.resource_mut::<EmbeddedHost>();
        match pointer {
            Some(p) => host.detector.sample(PointerSample::new(
                now,
                LogicalPoint::new(p.x, p.y),
                LogicalSize::new(size.x, size.y).expect("validated size"),
            )),
            None => host.detector.leave_output(now),
        }
        .unwrap_or_default()
    };
    for event in events {
        world.write_message(ShellCommand {
            output: OutputKey::new(&name).expect("validated name"),
            at: now,
            kind: ShellCommandKind::Corner(event),
        });
    }
}

/// Bevy uses the UI stack for both drawing and picking. Keep the existing
/// edge order within each band, with every overlay above every dock. Hidden
/// panels retain the overlay band throughout reveal and conceal animations.
fn panel_z_index(edge: Edge, mode: PanelMode) -> GlobalZIndex {
    let base = if mode == PanelMode::Docked { 110 } else { 150 };
    GlobalZIndex(base + edge.index() as i32 * 10)
}

fn present(
    mut commands: Commands,
    output: Res<EmbeddedOutput>,
    mounts: Res<EmbeddedPanelMounts>,
    frame: Res<ShellFrameState>,
    mut regions: ResMut<EmbeddedPanelRegions>,
    mut nodes: Query<(&mut Node, &mut GlobalZIndex, Option<&UiTargetCamera>)>,
    mut work_area: ResMut<EmbeddedWorkArea>,
) {
    regions.0.clear();
    let layout = panel_layout(&frame.0);
    work_area.0 = output.active.then_some(layout.canvas);
    for edge in Edge::ALL {
        let mount = mounts.0.get(edge);
        let Ok((mut node, mut z_index, target)) = nodes.get_mut(mount) else {
            continue;
        };
        let desired_z = panel_z_index(edge, frame.0.panel(edge).mode);
        if *z_index != desired_z {
            *z_index = desired_z;
        }
        let display = if output.active && output.camera.is_some() {
            Display::Flex
        } else {
            Display::None
        };
        let rect = layout.panels[edge.index()];
        let desired = (
            display,
            px(rect.x),
            px(rect.y),
            px(rect.width),
            px(rect.height),
        );
        if (node.display, node.left, node.top, node.width, node.height) != desired {
            (node.display, node.left, node.top, node.width, node.height) = desired;
        }
        if let Some(camera) = output.camera
            && target.is_none_or(|target| target.0 != camera)
        {
            commands.entity(mount).insert(UiTargetCamera(camera));
        }
        if output.active && frame.0.panel(edge).mapped {
            let mut visible = rect;
            let hidden =
                (1.0 - frame.0.panel(edge).visible_fraction) * frame.0.panel(edge).thickness_px;
            match edge {
                Edge::Left => visible.x -= hidden,
                Edge::Right => visible.x += hidden,
                Edge::Top => visible.y -= hidden,
                Edge::Bottom => visible.y += hidden,
            }
            regions.0.push(visible);
        }
    }
}

/// The dialog (scene-editor plan §4.3 Q2) in comp's renderer: the chrome
/// root, centred in the canvas the docked panels leave, above every panel
/// band, and an input region like a panel's. `origin` tells
/// `shell.scene.layout` where it is.
fn present_dialog(
    mut commands: Commands,
    output: Res<EmbeddedOutput>,
    frame: Res<ShellFrameState>,
    mut dialog: ResMut<cosmix_shell::chrome::dialog::QuoinDialog>,
    mut regions: ResMut<EmbeddedPanelRegions>,
    mut nodes: Query<(&mut Node, Option<&UiTargetCamera>)>,
) {
    let placed = match (output.active, output.camera, dialog.root, dialog.size()) {
        (true, Some(camera), Some(root), Some(size)) if dialog.wants_surface() => {
            let canvas = panel_layout(&frame.0).canvas;
            let origin = (Vec2::new(canvas.x, canvas.y)
                + (Vec2::new(canvas.width, canvas.height) - size) / 2.0)
                .round();
            if let Ok((mut node, target)) = nodes.get_mut(root) {
                let desired = (
                    PositionType::Absolute,
                    px(origin.x),
                    px(origin.y),
                    px(size.x),
                    px(size.y),
                );
                if (node.position_type, node.left, node.top, node.width, node.height) != desired {
                    (node.position_type, node.left, node.top, node.width, node.height) = desired;
                }
                if target.is_none_or(|target| target.0 != camera) {
                    commands
                        .entity(root)
                        .insert((UiTargetCamera(camera), GlobalZIndex(DIALOG_Z_INDEX)));
                }
            }
            regions.0.push(PanelRect {
                x: origin.x,
                y: origin.y,
                width: size.x,
                height: size.y,
            });
            Some(origin)
        }
        _ => None,
    };
    if dialog.origin != placed {
        dialog.origin = placed;
    }
}

/// Above the highest panel band (`panel_z_index` tops out below 200).
const DIALOG_Z_INDEX: i32 = 300;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_plugins_update_headlessly_without_scenes() {
        use bevy::asset::AssetApp;

        let mut app = App::new();
        // Headless Bevy platform services: keep real input, picking, text and
        // UI schedules, without a window, renderer or display event loop.
        app.add_plugins((
            MinimalPlugins,
            bevy::asset::AssetPlugin::default(),
            bevy::transform::TransformPlugin,
            bevy::camera::CameraPlugin,
            ImagePlugin::default(),
            bevy::image::TextureAtlasPlugin,
            bevy::mesh::MeshPlugin,
            bevy::input::InputPlugin,
            bevy::input_focus::InputFocusPlugin,
            bevy::input_focus::InputDispatchPlugin,
            bevy::window::WindowPlugin {
                primary_window: None,
                exit_condition: bevy::window::ExitCondition::DontExit,
                ..default()
            },
        ))
        // Bevy's plugin tuples stop at 15 elements; the rest go in a second call.
        .add_plugins((
            bevy::picking::DefaultPickingPlugins,
            bevy::clipboard::ClipboardPlugin,
            bevy::text::TextPlugin,
            bevy::ui::UiPlugin,
            bevy::ui_widgets::UiWidgetsPlugins,
        ))
        // Feathers loads shader assets even without a RenderApp.
        .init_asset::<bevy::shader::Shader>()
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(250),
        ));

        // Call the production assembly used by Plugin::build, including
        // configure_content shared with standalone startup. Only startup I/O
        // differs: no saved state, empty config, and an unsupported URL scheme
        // so the real BusBridgePlugin cannot contact an operator's broker.
        EmbeddedQuoinPlugin::default().configure(
            &mut app,
            crate::config::ShellConfig::default(),
            crate::state::StateStore::load(None),
            BusBridgeConfig::new("quoin-headless-test", "unsupported://headless"),
        );
        app.finish();
        app.cleanup();

        for step in 0..12 {
            if step == 1 {
                // Exercise both the startup placeholder and the host's first
                // real output observation, including its intro and settings.
                *app.world_mut().resource_mut::<EmbeddedOutput>() = EmbeddedOutput {
                    size: Vec2::new(1000.0, 800.0),
                    name: "test-output".into(),
                    active: true,
                    ..default()
                };
            }
            // Bevy's default error handler panics on a missing system resource.
            // Do not seed Quoin resources here or skip any application systems.
            app.update();
            let world = app.world();
            let frame = &world.resource::<ShellFrameState>().0;
            let seats = &world.resource::<cosmix_shell::runtime::SubPanelRegistryState>().0;
            assert!(
                world
                    .resource::<cosmix_scene_bevy::SceneStore>()
                    .list(seats, &frame.geometry.output)
                    .as_array()
                    .is_some_and(Vec::is_empty)
            );
            for edge in Edge::ALL {
                assert!(frame.panel(edge).page_ids.is_empty());
                assert!(!frame.panel(edge).mapped);
                assert_eq!(frame.panel(edge).exclusive_zone_px, 0.0);
            }
            assert!(world.resource::<EmbeddedPanelRegions>().0.is_empty());
        }
    }

    /// A legacy v2 state file as today's Quoin writes it, for the migration
    /// path (mirrors `state`'s `v2_source` fixture).
    fn v2_state_file() -> String {
        let mut source = String::from("{version: 2, scheme: \"v2\"");
        for (edge, page) in [
            (Edge::Left, "places"),
            (Edge::Bottom, "launcher"),
            (Edge::Right, "monitor"),
            (Edge::Top, "status"),
        ] {
            source.push_str(&format!(
                ", {}: {{thickness_px: {}, mode: \"{}\", page: \"{}\"}}",
                crate::edge_name(edge),
                150 + edge.index(),
                if edge == Edge::Left { "docked" } else { "hidden" },
                page,
            ));
        }
        source.push('}');
        source
    }

    #[test]
    fn output_change_repopulates_carousel_from_migrated_seats() {
        use cosmix_shell::runtime::{SubPanelRegistryState, register_shell_page};
        let registry = crate::tests::fixture_registry();
        let size = Vec2::new(1920.0, 1080.0);
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            ShellRuntimePlugin::new(model("DP-1", size, &registry)),
        ))
        .insert_resource(registry)
        .insert_resource(crate::state::StateStore::load(None))
        .insert_resource(EmbeddedHost {
            detector: CornerDetector::new(
                CornerDetectorConfig::new(8.0, Duration::from_millis(250), 100.0).unwrap(),
            ),
            name: "DP-1".into(),
            size,
        })
        .init_resource::<EmbeddedOutput>();
        let world = app.world_mut();
        crate::config::ingest_test_config(
            world,
            r#"{panels: {left: ["scene-mounted", "verb-only", "nav"]}}"#,
        );
        // Receipt order deliberately differs from declaration order.
        for (name, receipt) in [("verb-only", 1), ("scene-mounted", 2), ("tail", 3)] {
            world
                .resource_mut::<SubPanelRegistryState>()
                .0
                .mount(name, OutputKey::new("DP-1").unwrap(), Edge::Left, "owner", receipt)
                .unwrap();
            register_shell_page(world, Edge::Left, name);
        }
        {
            let mut output = world.resource_mut::<EmbeddedOutput>();
            output.name = "HDMI-1".into();
            output.size = size;
            output.active = true;
        }
        // The production host constructs a static model, restores state and
        // calls replace_shell_model. No declarations are seeded into it here.
        prepare(world);
        let panel = world.resource::<ShellFrameState>().0.panel(Edge::Left);
        assert_eq!(
            panel.page_ids.as_ref(),
            ["scene-mounted", "verb-only", "nav", "places", "info", "tail"]
        );
        assert!(!panel.mapped);
        for name in ["verb-only", "scene-mounted", "tail"] {
            assert_eq!(
                world.resource::<SubPanelRegistryState>().0.seat(name)
                    .unwrap().output.as_str(),
                "HDMI-1"
            );
        }
    }

    #[test]
    fn startup_placeholder_restores_nothing_until_the_first_real_observation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoin.state.mix");
        std::fs::write(&path, v2_state_file()).unwrap();

        // Mirrors EmbeddedQuoinPlugin::build: a placeholder model with no
        // restore, waiting for the host's first real observation.
        let registry = crate::tests::fixture_registry();
        let fresh = model(PLACEHOLDER_OUTPUT, Vec2::new(1920.0, 1080.0), &registry);
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            ShellRuntimePlugin::new(model(
                PLACEHOLDER_OUTPUT,
                Vec2::new(1920.0, 1080.0),
                &registry,
            )),
        ))
        .insert_resource(crate::state::StateStore::load(Some(path)))
        .insert_resource(crate::tests::fixture_registry())
        .insert_resource(EmbeddedHost {
            detector: CornerDetector::new(
                CornerDetectorConfig::new(8.0, Duration::from_millis(250), 100.0)
                    .expect("valid corner tuning"),
            ),
            name: PLACEHOLDER_OUTPUT.into(),
            size: Vec2::new(1920.0, 1080.0),
        })
        .init_resource::<EmbeddedOutput>();

        // The placeholder claims nothing: it keeps the fresh-model defaults,
        // so the migrated legacy entry waits for a real connector.
        let frame = app.world().resource::<ShellFrameState>().0.clone();
        assert_eq!(frame.geometry.output.as_str(), PLACEHOLDER_OUTPUT);
        for edge in Edge::ALL {
            assert_eq!(
                frame.panel(edge).thickness_px,
                fresh.panel(edge).thickness_px
            );
            assert_eq!(frame.panel(edge).mode, PanelMode::Hidden);
        }

        // The first real connector observation restores through prepare()
        // and claims the migrated v2 state for that connector.
        {
            let mut output = app.world_mut().resource_mut::<EmbeddedOutput>();
            output.size = Vec2::new(1920.0, 1080.0);
            output.name = "DP-1".into();
            output.active = true;
        }
        prepare(app.world_mut());
        let frame = app.world().resource::<ShellFrameState>().0.clone();
        assert_eq!(frame.geometry.output.as_str(), "DP-1");
        assert_eq!(frame.panel(Edge::Left).thickness_px, 150.0);
        assert_eq!(frame.panel(Edge::Left).mode, PanelMode::Docked);
        assert_eq!(
            frame.panel(Edge::Left).active_page_id.as_deref(),
            Some("places")
        );
        assert_eq!(frame.panel(Edge::Right).thickness_px, 152.0);
        assert_eq!(frame.panel(Edge::Bottom).thickness_px, 151.0);
    }

    #[test]
    fn every_overlay_edge_stacks_above_every_dock_edge() {
        for overlay in Edge::ALL {
            for dock in Edge::ALL {
                for mode in [PanelMode::Pinned, PanelMode::Hidden] {
                    assert!(
                        panel_z_index(overlay, mode).0 > panel_z_index(dock, PanelMode::Docked).0
                    );
                }
            }
        }
    }

    #[test]
    fn overlapping_overlay_mount_updates_stacking_on_mode_changes() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let camera = app.world_mut().spawn_empty().id();
        let mounts: [Entity; 4] = std::array::from_fn(|_| {
            app.world_mut()
                .spawn((Node::default(), GlobalZIndex(0)))
                .id()
        });
        let mut model = model("test", Vec2::new(1000., 800.), &crate::tests::fixture_registry());
        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Dock)
            .unwrap();
        app.insert_resource(EmbeddedOutput {
            camera: Some(camera),
            size: Vec2::new(1000., 800.),
            name: "test".into(),
            active: true,
            pointer: None,
        })
        .insert_resource(EmbeddedPanelMounts(QuoinPanelMounts::new(
            mounts[0], mounts[1], mounts[2], mounts[3],
        )))
        .init_resource::<EmbeddedPanelRegions>()
        .init_resource::<EmbeddedWorkArea>()
        .add_systems(Update, present);

        // A deliberate undock hides immediately when nothing holds the panel,
        // so hold the bottom edge with the pointer first: the undock iteration
        // must still land in an overlay state (transient reveal) for this
        // stacking walk.
        model
            .panel_input(Edge::Bottom, Duration::ZERO, PanelInput::PointerEntered)
            .unwrap();
        for input in [PanelInput::Pin, PanelInput::Dock, PanelInput::Undock] {
            model
                .panel_input(Edge::Bottom, Duration::ZERO, input)
                .unwrap();
            let frame = cosmix_shell::runtime::ShellFrame::from_model(&model);
            let overlay = frame.panel(Edge::Bottom).mode != PanelMode::Docked;
            if overlay {
                assert!(
                    frame.panel(Edge::Bottom).mode == PanelMode::Pinned
                        || frame.panel(Edge::Bottom).transient_revealed
                );
                let layout = panel_layout(&frame);
                let left = layout.panels[Edge::Left.index()];
                let bottom = layout.panels[Edge::Bottom.index()];
                assert!(left.x < bottom.x + bottom.width && bottom.x < left.x + left.width);
                assert!(left.y < bottom.y + bottom.height && bottom.y < left.y + left.height);
            }
            app.insert_resource(ShellFrameState(frame));
            app.update();
            for edge in Edge::ALL {
                assert_eq!(
                    *app.world()
                        .get::<GlobalZIndex>(mounts[edge.index()])
                        .unwrap(),
                    panel_z_index(edge, model.panel(edge).mode),
                );
            }
            if overlay {
                assert!(
                    app.world()
                        .get::<GlobalZIndex>(mounts[Edge::Bottom.index()])
                        .unwrap()
                        .0
                        > app
                            .world()
                            .get::<GlobalZIndex>(mounts[Edge::Left.index()])
                            .unwrap()
                            .0
                );
            }
        }
    }

    #[test]
    fn stable_mounts_do_not_relayout_and_deactivation_clears_hit_regions() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let camera = app.world_mut().spawn_empty().id();
        let mounts: [Entity; 4] = std::array::from_fn(|_| {
            app.world_mut()
                .spawn((Node::default(), GlobalZIndex(0)))
                .id()
        });
        let mut model = model("test", Vec2::new(1000., 800.), &crate::tests::fixture_registry());
        model
            .panel_input(Edge::Right, Duration::ZERO, PanelInput::Dock)
            .unwrap();
        app.insert_resource(EmbeddedOutput {
            camera: Some(camera),
            size: Vec2::new(1000., 800.),
            name: "test".into(),
            active: true,
            pointer: None,
        })
        .insert_resource(EmbeddedPanelMounts(QuoinPanelMounts::new(
            mounts[0], mounts[1], mounts[2], mounts[3],
        )))
        .insert_resource(ShellFrameState(
            cosmix_shell::runtime::ShellFrame::from_model(&model),
        ))
        .init_resource::<EmbeddedPanelRegions>()
        .init_resource::<EmbeddedWorkArea>()
        .add_systems(Update, present);
        app.update();
        assert!(!app.world().resource::<EmbeddedPanelRegions>().0.is_empty());
        app.world_mut().clear_trackers();
        app.update();
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, Changed<Node>>()
                .iter(app.world())
                .count(),
            0
        );
        app.world_mut().resource_mut::<EmbeddedOutput>().active = false;
        app.update();
        assert!(app.world().resource::<EmbeddedPanelRegions>().0.is_empty());
        assert!(app.world().resource::<EmbeddedWorkArea>().0.is_none());
        for mount in mounts {
            assert_eq!(
                app.world().get::<Node>(mount).unwrap().display,
                Display::None
            );
        }
    }
}
