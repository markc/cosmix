//! Opt-in, single-output comparison: shared Boing renderer plus native HUD.
//! The ordinary Wayland client scene remains the final output/capture owner.

use super::{KmsOutputCamera, idle};
use bevy::{
    camera::{CameraOutputMode, RenderTarget, visibility::RenderLayers},
    ecs::system::SystemState,
    prelude::*,
    render::render_resource::BlendState,
};
use cosmix_bg_boing::Simulations;
use std::time::Instant;

#[derive(Resource)]
struct Probe {
    started: Instant,
    output: Option<ProbeOutput>,
    phase: Option<u8>,
}

struct ProbeOutput {
    owner: Entity,
    root: Entity,
    camera: Entity,
    panel: Entity,
}

#[cfg(not(test))]
pub(super) fn install_from_environment(app: &mut App) {
    if std::env::var("COSMIX_COMP_HUD_PROBE").as_deref() != Ok("1") {
        return;
    }
    install(app);
}

fn install(app: &mut App) {
    app.insert_resource(idle::ContinuousRendering)
        .insert_resource(GlobalAmbientLight {
            brightness: 180.0,
            ..default()
        })
        .insert_non_send(Simulations::default())
        .insert_resource(Probe {
            started: Instant::now(),
            output: None,
            phase: None,
        })
        .add_systems(Update, update);
    #[cfg(feature = "native-quoin")]
    {
        app.configure_sets(
            Update,
            cosmix_shell::runtime::ShellRuntimeSet::Input.after(update),
        );
        crate::native_shell::install(app);
    }
    tracing::warn!("Native Boing/HUD comparison enabled; first output only, automatic panel cycle");
}

fn create(world: &mut World, owner: Entity, target: RenderTarget) -> ProbeOutput {
    let mut state = SystemState::<(
        Commands,
        ResMut<Assets<Mesh>>,
        ResMut<Assets<StandardMaterial>>,
        ResMut<Assets<Image>>,
        NonSendMut<Simulations>,
    )>::new(world);
    let (mut commands, mut meshes, mut materials, mut images, mut simulations) = state
        .get_mut(world)
        .expect("probe render resources installed");
    let root = commands
        .spawn((Transform::default(), Visibility::default()))
        .id();
    let camera = commands
        .spawn((
            Camera3d::default(),
            Camera {
                order: -1,
                ..default()
            },
            target,
            RenderLayers::layer(2),
            Msaa::Sample4,
        ))
        .id();
    cosmix_bg_boing::spawn(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        root,
        owner,
        camera,
        RenderLayers::layer(2),
        &mut simulations,
    );
    let panel = commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(-440), top: px(32), width: px(420), height: px(260),
            padding: UiRect::all(px(24)),
            flex_direction: FlexDirection::Column,
            row_gap: px(20),
            ..default()
        },
        BackgroundColor(Color::srgba(0.025, 0.045, 0.075, 0.92)),
        UiTargetCamera(owner),
    )).with_children(|parent| {
        parent.spawn((Text::new("CosMix · native HUD"), TextFont { font_size: FontSize::Px(28.0), ..default() },
            TextColor(Color::srgb(0.35, 0.85, 1.0))));
        parent.spawn((Text::new("Boing + panel share one renderer\n\nReal Avian physics · 4× MSAA\nPanel opens and closes automatically"),
            TextFont { font_size: FontSize::Px(20.0), ..default() }, TextColor(Color::WHITE)));
    }).id();
    state.apply(world);
    ProbeOutput {
        owner,
        root,
        camera,
        panel,
    }
}

fn remove(world: &mut World, output: ProbeOutput) {
    if let Some(mut camera) = world.get_mut::<Camera>(output.owner) {
        camera.clear_color = ClearColorConfig::Default;
        camera.output_mode = CameraOutputMode::default();
    }
    for entity in [output.root, output.camera, output.panel] {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
        }
    }
    world.non_send_mut::<Simulations>().0.remove(&output.owner);
}

/// Six-second cycle: hidden, opening, held, closing. Wall time deliberately
/// exposes stalls instead of slowing the test animation to conceal them.
fn panel_cycle(seconds: f32) -> (u8, f32) {
    let t = seconds.rem_euclid(6.0);
    let (phase, amount) = if t < 2.0 {
        (0, 0.0)
    } else if t < 2.5 {
        (1, (t - 2.0) * 2.0)
    } else if t < 4.0 {
        (2, 1.0)
    } else if t < 4.5 {
        (3, (4.5 - t) * 2.0)
    } else {
        (0, 0.0)
    };
    (phase, amount * amount * (3.0 - 2.0 * amount))
}

fn update(world: &mut World) {
    let _trace = crate::frame_trace::span("comp_native_hud_update", 0);
    if let Some(scale) = world.get_resource::<crate::compositor_scene::RendererOutputScale120>() {
        let scale = scale.0 as f32 / 120.0;
        if world
            .get_resource::<UiScale>()
            .is_none_or(|current| current.0 != scale)
        {
            world.insert_resource(UiScale(scale));
        }
    }
    let mut probe = world.remove_resource::<Probe>().expect("installed probe");
    let chosen = world
        .query_filtered::<(Entity, &RenderTarget), With<KmsOutputCamera>>()
        .iter(world)
        .min_by_key(|(entity, _)| *entity)
        .map(|(entity, target)| (entity, target.clone()));
    if probe
        .output
        .as_ref()
        .is_some_and(|output| chosen.as_ref().map(|v| v.0) != Some(output.owner))
    {
        remove(world, probe.output.take().expect("existing output"));
    }
    if probe.output.is_none()
        && let Some((owner, target)) = chosen.as_ref()
    {
        probe.output = Some(create(world, *owner, target.clone()));
        probe.started = Instant::now();
    }
    #[cfg(feature = "native-quoin")]
    if probe.output.is_none()
        && let Some(mut native) = world.get_resource_mut::<cosmix_quoin::native::NativeOutput>()
    {
        native.camera = None;
        native.active = false;
        native.pointer = None;
    }
    if let Some(output) = &probe.output {
        let locked = crate::compositor_scene::security_scene_active(world);
        let active = !locked
            && world
                .get::<Camera>(output.owner)
                .is_some_and(|c| c.is_active);
        #[cfg(feature = "native-quoin")]
        if world.contains_resource::<cosmix_quoin::native::NativeOutput>() {
            let size = world
                .resource::<crate::compositor_scene::LogicalCanvasSize>()
                .0;
            let pointer = world
                .resource::<crate::protocol::ClientSceneFeed>()
                .cursor_position();
            let name = world
                .get::<crate::capture::CaptureOutputSource>(output.owner)
                .map(|source| source.output_name.clone())
                .unwrap_or_else(|| "primary".into());
            if let Some(mut native) = world.get_resource_mut::<cosmix_quoin::native::NativeOutput>()
            {
                native.camera = Some(output.owner);
                native.size = size;
                native.name = name;
                native.active = active;
                native.pointer = (active && pointer.on_output)
                    .then_some(Vec2::new(pointer.x as f32, pointer.y as f32));
            }
        }
        if let Some(mut camera) = world.get_mut::<Camera>(output.camera) {
            camera.is_active = active;
        }
        if let Some((_, target)) = chosen {
            world.entity_mut(output.camera).insert(target);
        }
        if let Some(mut camera) = world.get_mut::<Camera>(output.owner) {
            camera.clear_color = if active {
                ClearColorConfig::Custom(Color::NONE)
            } else {
                ClearColorConfig::Custom(Color::BLACK)
            };
            camera.output_mode = if active {
                CameraOutputMode::Write {
                    blend_state: Some(BlendState::ALPHA_BLENDING),
                    clear_color: ClearColorConfig::None,
                }
            } else {
                CameraOutputMode::default()
            };
        }
        let (phase, amount) = panel_cycle(probe.started.elapsed().as_secs_f32());
        #[cfg(feature = "native-quoin")]
        let active_hud = active && !world.contains_resource::<cosmix_quoin::native::NativeOutput>();
        #[cfg(not(feature = "native-quoin"))]
        let active_hud = active;
        if let Some(mut node) = world.get_mut::<Node>(output.panel) {
            node.display = if active_hud {
                Display::Flex
            } else {
                Display::None
            };
            node.left = px(-440.0 + 472.0 * amount);
        }
        if active {
            let dt = world.resource::<Time>().delta_secs();
            let pose = world
                .non_send_mut::<Simulations>()
                .0
                .get_mut(&output.owner)
                .map(|sim| (sim.visual, sim.advance(dt)));
            if let Some((entity, pose)) = pose {
                world.entity_mut(entity).insert(pose);
            }
            world
                .resource::<crate::capture::OutputDamageJournal>()
                .mark_all_base_full();
            let mut revision =
                world.resource_mut::<crate::compositor_scene::SceneContentRevision>();
            revision.0 = revision.0.and_then(|value| value.checked_add(1));
        }
        if probe.phase != Some(phase) {
            tracing::info!(phase, active, "FRAME_TEST native_hud_phase");
            probe.phase = Some(phase);
        }
    }
    world.insert_resource(probe);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_tracks_output_deactivation_retarget_and_removal() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<Image>>()
            .init_resource::<crate::capture::OutputDamageJournal>()
            .init_resource::<crate::compositor_scene::SceneContentRevision>();
        install(&mut app);
        let owner = app
            .world_mut()
            .spawn((Camera2d, KmsOutputCamera, RenderTarget::default()))
            .id();
        app.update();
        let output = app.world().resource::<Probe>().output.as_ref().unwrap();
        let (camera, panel, root) = (output.camera, output.panel, output.root);
        assert!(app.world().get::<Camera>(camera).unwrap().is_active);
        let target = RenderTarget::TextureView(bevy::camera::ManualTextureViewHandle(77));
        app.world_mut().entity_mut(owner).insert(target.clone());
        app.world_mut().get_mut::<Camera>(owner).unwrap().is_active = false;
        app.update();
        assert!(!app.world().get::<Camera>(camera).unwrap().is_active);
        assert!(matches!(app.world().get::<RenderTarget>(camera).unwrap(),
            RenderTarget::TextureView(handle) if handle.0 == 77));
        assert_eq!(
            app.world().get::<Node>(panel).unwrap().display,
            Display::None
        );
        app.world_mut().get_mut::<Camera>(owner).unwrap().is_active = true;
        app.update();
        assert!(app.world().get::<Camera>(camera).unwrap().is_active);
        app.world_mut().entity_mut(owner).despawn();
        app.update();
        for entity in [camera, panel, root] {
            assert!(app.world().get_entity(entity).is_err());
        }
        assert!(app.world().non_send::<Simulations>().0.is_empty());
    }

    #[test]
    fn panel_cycle_has_stationary_and_smooth_transition_phases() {
        assert_eq!(panel_cycle(1.0), (0, 0.0));
        assert_eq!(panel_cycle(3.0), (2, 1.0));
        assert_eq!(panel_cycle(2.25), (1, 0.5));
        assert_eq!(panel_cycle(4.25), (3, 0.5));
        assert_eq!(panel_cycle(7.0), panel_cycle(1.0));
    }
}
