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

pub struct EmbeddedQuoinPlugin;

impl Plugin for EmbeddedQuoinPlugin {
    fn build(&self, app: &mut App) {
        let registry = crate::page_registry();
        let store = crate::state::StateStore::startup(false);
        let mut model = model("primary", Vec2::new(1920.0, 1080.0), &registry);
        store.snapshot().restore(&mut model);
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
            name: "primary".into(),
            size: Vec2::new(1920.0, 1080.0),
        });
        let mut bus = BusBridgeConfig::new("shell", resolve_noded_url());
        bus.provenance = provenance_from_build(cosmix_buildinfo::build_info!());
        bus.inbound_prefixes.push("shell.".into());
        bus.subscriptions.extend(
            [
                "power.props.changed",
                "wallpaper.props.changed",
                "bg-showcase.props.changed",
            ]
            .map(str::to_owned),
        );
        crate::configure_content(app, bus, registry, store, false, false);
        app.add_systems(Update, prepare.in_set(ShellRuntimeSet::Input))
            .add_systems(Update, present.in_set(ShellRuntimeSet::Host))
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
        let replacement = model(&name, size, world.resource());
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut model = model("test", Vec2::new(1000., 800.), &crate::page_registry());
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
        let mut model = model("test", Vec2::new(1000., 800.), &crate::page_registry());
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
