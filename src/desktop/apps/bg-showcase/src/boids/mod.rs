//! The wallpaper is a regular native scene citizen, independent of comp and
//! Quoin. All dynamics use output-local logical pixels.

pub mod bus;
mod geometry;
mod occlusion;
mod pointer;
mod preferences;

use bevy::{
    camera::visibility::RenderLayers,
    prelude::*,
    render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk},
};
use cosmix_flock::Flock;
use cosmix_shell_host::scene::{SceneAfterRender, SceneFrameResults, SceneTick, SceneViews};
use std::{collections::BTreeMap, time::Instant};

pub fn run() -> AppExit {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let capture = match args.as_slice() {
        [] => None,
        [flag] if flag == "--version" => {
            println!("cosmix-wallpaper {}", env!("CARGO_PKG_VERSION"));
            return AppExit::Success;
        }
        [flag] if flag == "--help" => {
            println!(
                "cosmix-wallpaper [--capture /absolute/wallpaper.png]\nCapture saves only this application's first output after 20 rendered frames."
            );
            return AppExit::Success;
        }
        [flag, path]
            if flag == "--capture"
                && std::path::Path::new(path).is_absolute()
                && path.ends_with(".png") =>
        {
            Some(path.clone())
        }
        _ => {
            eprintln!("usage: cosmix-wallpaper [--capture /absolute/wallpaper.png]");
            return AppExit::error();
        }
    };
    let mut app = App::new();
    let preferences = preferences::PreferenceStore::load(preferences::state_path());
    if let Some(error) = &preferences.error {
        eprintln!("wallpaper preferences: {error}; using defaults");
    }
    cosmix_shell_host::scene::configure_scene_host_with_config(
        &mut app,
        cosmix_shell_host::scene::SceneHostConfig {
            fps: preferences.current.fps_limit,
            namespace: "cosmix-wallpaper".into(),
            title: "Cosmix Boids".into(),
            camera_kind: cosmix_shell_host::scene::SceneCameraKind::ThreeD,
        },
    )
    .expect("validated wallpaper frame rate");
    app.insert_resource(preferences);
    bus::configure(&mut app);
    if let Some(path) = capture {
        app.insert_resource(CaptureOnce {
            path,
            frames: 0,
            requested: false,
            window: None,
            accepted: None,
            attempts: 0,
            completed: false,
        });
    }
    app.insert_resource(ClearColor(Color::srgb(0.018, 0.028, 0.052)))
        .init_resource::<Flocks>()
        .init_resource::<occlusion::CoveredOutputs>()
        .add_systems(Startup, assets)
        .add_systems(
            Update,
            (
                bus::service,
                update_palette,
                reconcile,
                occlusion::reconcile,
                animate,
                capture_once,
            )
                .chain(),
        )
        .add_systems(
            SceneAfterRender,
            (verify_capture_submission, occlusion::submitted),
        );
    app.run()
}

/// Explicit GPU readback of our own background buffer, never other apps.
#[derive(Resource)]
struct CaptureOnce {
    path: String,
    frames: u32,
    requested: bool,
    window: Option<Entity>,
    accepted: Option<bool>,
    attempts: u8,
    completed: bool,
}

fn capture_once(
    mut commands: Commands,
    capture: Option<ResMut<CaptureOnce>>,
    views: Res<SceneViews>,
    tick: Res<SceneTick>,
) {
    let Some(mut capture) = capture else { return };
    if capture.requested {
        return;
    }
    let Some((name, view)) = views.0.first_key_value() else {
        return;
    };
    if !tick.0.contains(name) {
        return;
    }
    capture.frames += 1;
    if capture.frames >= 20 {
        capture.requested = true;
        capture.window = Some(view.window);
        capture.accepted = None;
        capture.attempts += 1;
        commands
            .spawn(Screenshot::window(view.window))
            .observe(capture_complete);
    }
}

fn verify_capture_submission(
    capture: Option<ResMut<CaptureOnce>>,
    results: Res<SceneFrameResults>,
) {
    if let Some(mut capture) = capture
        && capture.requested
        && capture.accepted.is_none()
    {
        capture.accepted = Some(
            capture
                .window
                .and_then(|window| results.0.get(&window))
                .copied()
                .unwrap_or(false),
        );
    }
}

fn capture_complete(event: On<ScreenshotCaptured>, mut capture: ResMut<CaptureOnce>) {
    if capture.accepted == Some(true) {
        capture.completed = true;
        save_to_disk(capture.path.clone())(event);
    } else if capture.attempts < 3 {
        // Bevy may return a zero-filled readback after acquisition failure.
        // Do not save that image as evidence of a rendered frame.
        capture.requested = false;
        capture.frames = 19;
        warn!("wallpaper capture frame was not submitted; retrying");
    } else {
        capture.completed = true;
        error!("wallpaper capture failed after three unsubmitted frames");
    }
}

#[derive(Resource)]
struct BirdAssets {
    mesh: Handle<Mesh>,
    colours: Vec<Handle<StandardMaterial>>,
}

fn assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // A small paper dart, shared by every bird and batched by material.
    let mesh = meshes.add(Triangle2d::new(
        Vec2::new(7.0, 0.0),
        Vec2::new(-4.0, 3.0),
        Vec2::new(-4.0, -3.0),
    ));
    let colours = [
        (0.38, 0.86, 0.94),
        (0.66, 0.80, 1.0),
        (0.90, 0.72, 0.42),
        (0.74, 0.61, 0.93),
    ]
    .into_iter()
    .map(|(r, g, b)| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(r, g, b),
            unlit: true,
            ..default()
        })
    })
    .collect();
    commands.insert_resource(BirdAssets { mesh, colours });
}

struct OutputFlock {
    seed: u64,
    window: Entity,
    logical: (u32, u32),
    flock: Flock,
    entities: Vec<Entity>,
    last_frame: Option<Instant>,
}

#[derive(Resource, Default)]
struct Flocks(BTreeMap<String, OutputFlock>);

#[derive(Component)]
struct Paperbird {
    output: Entity,
    index: usize,
}

fn reconcile(
    mut commands: Commands,
    views: Res<SceneViews>,
    assets: Res<BirdAssets>,
    mut flocks: ResMut<Flocks>,
    preferences: Res<preferences::PreferenceStore>,
) {
    flocks.0.retain(|name, flock| {
        if views.0.get(name).is_some_and(|v| v.window == flock.window)
            && flock.seed == preferences.current.seed
            && flock.flock.settings().count == preferences.current.count
        {
            return true;
        }
        for entity in &flock.entities {
            commands.entity(*entity).despawn();
        }
        false
    });
    for (name, view) in &views.0 {
        if flocks
            .0
            .get(name)
            .is_none_or(|flock| flock.logical != view.logical_size)
        {
            commands.entity(view.camera).insert((
                Transform::from_xyz(0.0, 0.0, 100.0),
                Projection::Orthographic(OrthographicProjection {
                    scaling_mode: bevy::camera::ScalingMode::Fixed {
                        width: view.logical_size.0 as f32,
                        height: view.logical_size.1 as f32,
                    },
                    ..OrthographicProjection::default_3d()
                }),
                bevy::core_pipeline::tonemapping::Tonemapping::None,
            ));
            let camera = view.camera;
            commands.queue(move |world: &mut World| {
                if let Some(mut camera) = world.get_mut::<Camera>(camera) {
                    camera.clear_color = ClearColorConfig::Custom(Color::srgb(0.018, 0.028, 0.052));
                }
            });
        }
        if let Some(flock) = flocks.0.get_mut(name) {
            flock
                .flock
                .configure(preferences.current.settings())
                .expect("validated preferences");
            if flock.logical != view.logical_size {
                if let Err(error) = flock
                    .flock
                    .resize(view.logical_size.0 as f32, view.logical_size.1 as f32)
                {
                    error!(%error, "wallpaper output resize refused");
                    continue;
                }
                flock.logical = view.logical_size;
            }
            continue;
        }
        let Ok(flock) = Flock::new(
            view.logical_size.0 as f32,
            view.logical_size.1 as f32,
            preferences.current.seed,
            preferences.current.settings(),
        ) else {
            continue;
        };
        let entities = flock
            .birds()
            .iter()
            .enumerate()
            .map(|(index, _)| {
                commands
                    .spawn((
                        Paperbird {
                            output: view.window,
                            index,
                        },
                        Mesh3d(assets.mesh.clone()),
                        MeshMaterial3d(assets.colours[index % assets.colours.len()].clone()),
                        Transform::default(),
                        Visibility::Hidden,
                        RenderLayers::layer(view.render_layer),
                    ))
                    .id()
            })
            .collect();
        info!(output = %name, birds = flock.birds().len(), width = view.logical_size.0, height = view.logical_size.1, scale = view.scale, "WALLPAPER_OUTPUT_READY");
        flocks.0.insert(
            name.clone(),
            OutputFlock {
                seed: preferences.current.seed,
                window: view.window,
                logical: view.logical_size,
                flock,
                entities,
                last_frame: None,
            },
        );
    }
}

fn update_palette(
    preferences: Res<preferences::PreferenceStore>,
    assets: Res<BirdAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !preferences.is_changed() {
        return;
    }
    let colours = match preferences.current.preset {
        preferences::Preset::Ocean => [
            (0.38, 0.86, 0.94),
            (0.66, 0.80, 1.0),
            (0.90, 0.72, 0.42),
            (0.74, 0.61, 0.93),
        ],
        preferences::Preset::Ember => [
            (1.0, 0.40, 0.18),
            (1.0, 0.68, 0.25),
            (0.93, 0.31, 0.40),
            (1.0, 0.87, 0.56),
        ],
        preferences::Preset::Twilight => [
            (0.72, 0.48, 0.98),
            (0.96, 0.56, 0.78),
            (0.43, 0.59, 0.97),
            (0.87, 0.80, 1.0),
        ],
    };
    for (handle, (r, g, b)) in assets.colours.iter().zip(colours) {
        if let Some(mut material) = materials.get_mut(handle) {
            material.base_color = Color::srgb(r, g, b);
        }
    }
}

fn animate(
    scene: (Res<SceneTick>, Res<SceneViews>),
    geometry: Res<bus::SceneGeometry>,
    pointer: Res<pointer::ScenePointer>,
    preferences: Res<preferences::PreferenceStore>,
    mut flocks: ResMut<Flocks>,
    mut birds: Query<(&Paperbird, &mut Transform, &mut Visibility)>,
    mut metrics: ResMut<bus::SceneBus>,
) {
    let (tick, views) = scene;
    let now = Instant::now();
    if !preferences.current.enabled
        || preferences.current.paused
        || geometry.0.as_ref().is_none_or(|s| s.locked)
    {
        for flock in flocks.0.values_mut() {
            flock.last_frame = None;
        }
        return;
    }
    // Clear visible birds even on a control-only wake. The host admits one
    // real frame before occlusion::submitted can suspend this output.
    for (name, output) in &mut flocks.0 {
        if views.0.get(name).is_some_and(|view| {
            geometry
                .0
                .as_ref()
                .is_some_and(|scene| scene.covers_output(name, view.logical_size, view.origin))
        }) {
            output.last_frame = None;
            for (bird, _, mut visibility) in &mut birds {
                if bird.output == output.window {
                    visibility.set_if_neq(Visibility::Hidden);
                }
            }
        }
    }
    for name in &tick.0 {
        let Some(output) = flocks.0.get_mut(name) else {
            continue;
        };
        if views.0.get(name).is_some_and(|view| {
            geometry
                .0
                .as_ref()
                .is_some_and(|scene| scene.covers_output(name, view.logical_size, view.origin))
        }) {
            continue;
        }
        let obstacles = geometry.0.as_ref().filter(|s| !s.locked).and_then(|s| {
            let view = views.0.get(name)?;
            s.for_output(name, view.logical_size, view.origin)
        });
        let Some(obstacles) = obstacles else {
            for (bird, _, mut visibility) in &mut birds {
                if bird.output == output.window {
                    visibility.set_if_neq(Visibility::Hidden);
                }
            }
            output.last_frame = None;
            continue;
        };
        if output.flock.set_obstacles(&obstacles).is_err() {
            continue;
        }
        output.flock.set_pointer(pointer.for_output(name));
        let elapsed = output.last_frame.map_or(0.0, |last| {
            now.saturating_duration_since(last).as_secs_f32()
        });
        output.last_frame = Some(now);
        let simulation_started = Instant::now();
        let steps = output
            .flock
            .advance_scheduled(elapsed, preferences.current.fps_limit);
        metrics.simulation_steps = metrics.simulation_steps.saturating_add(steps as u64);
        metrics.simulation_ns = metrics.simulation_ns.saturating_add(
            u64::try_from(simulation_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        for (bird, mut transform, mut visibility) in &mut birds {
            if bird.output != output.window {
                continue;
            }
            let Some(state) = output.flock.birds().get(bird.index) else {
                continue;
            };
            // Wayland's top-left/y-down coordinates become camera-centred/y-up.
            let mut next_transform = *transform;
            next_transform.translation = Vec3::new(
                state.position.x - output.logical.0 as f32 * 0.5,
                output.logical.1 as f32 * 0.5 - state.position.y,
                0.0,
            );
            next_transform.rotation =
                Quat::from_rotation_z((-state.velocity.y).atan2(state.velocity.x));
            transform.set_if_neq(next_transform);
            visibility.set_if_neq(if state.visible {
                Visibility::Visible
            } else {
                Visibility::Hidden
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_capture_submission_discards_readback_and_bounds_retries() {
        let mut app = App::new();
        let window = app.world_mut().spawn_empty().id();
        app.insert_resource(CaptureOnce {
            path: String::new(),
            frames: 20,
            requested: true,
            window: Some(window),
            accepted: None,
            attempts: 1,
            completed: false,
        })
        .init_resource::<SceneFrameResults>()
        .add_systems(SceneAfterRender, verify_capture_submission)
        .add_observer(capture_complete);
        app.world_mut().run_schedule(SceneAfterRender);
        assert_eq!(app.world().resource::<CaptureOnce>().accepted, Some(false));
        app.world_mut().trigger(ScreenshotCaptured {
            entity: window,
            image: Image::default(),
        });
        assert!(!app.world().resource::<CaptureOnce>().requested);
        assert_eq!(app.world().resource::<CaptureOnce>().frames, 19);

        {
            let mut capture = app.world_mut().resource_mut::<CaptureOnce>();
            capture.requested = true;
            capture.attempts = 3;
        }
        app.world_mut().trigger(ScreenshotCaptured {
            entity: window,
            image: Image::default(),
        });
        assert!(app.world().resource::<CaptureOnce>().requested);
    }

    #[test]
    fn capture_submission_result_is_pinned_before_later_frames() {
        let mut app = App::new();
        let window = app.world_mut().spawn_empty().id();
        app.insert_resource(CaptureOnce {
            path: String::new(),
            frames: 20,
            requested: true,
            window: Some(window),
            accepted: None,
            attempts: 1,
            completed: false,
        })
        .insert_resource(SceneFrameResults(BTreeMap::from([(window, true)])))
        .add_systems(SceneAfterRender, verify_capture_submission);
        app.world_mut().run_schedule(SceneAfterRender);
        app.world_mut()
            .resource_mut::<SceneFrameResults>()
            .0
            .clear();
        app.world_mut().run_schedule(SceneAfterRender);
        assert_eq!(app.world().resource::<CaptureOnce>().accepted, Some(true));
    }
}

/// Shared scheduling seam: one instance is active in the catalogue host.
#[derive(Resource)]
pub struct Active(pub bool);

pub fn active(active: Res<Active>) -> bool {
    active.0
}

/// Installs boids content only. The caller owns the single scene host and Bus bridge.
pub fn configure_content(app: &mut App) {
    let mut preferences = preferences::PreferenceStore::load(preferences::state_path());
    preferences.set_service("bg-showcase");
    app.insert_resource(preferences)
        .init_resource::<Flocks>()
        .init_resource::<occlusion::CoveredOutputs>()
        .add_systems(Startup, assets)
        .add_systems(
            Update,
            (update_palette, reconcile, occlusion::reconcile, animate)
                .chain()
                .run_if(active)
                .in_set(Content),
        )
        .add_systems(SceneAfterRender, occlusion::submitted.run_if(active));
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Content;

/// Drop all output-local entities, clocks and pending clear acknowledgements.
pub fn clear(world: &mut World) {
    let flocks = std::mem::take(&mut world.resource_mut::<Flocks>().0);
    for flock in flocks.into_values() {
        for entity in flock.entities {
            if let Ok(entity) = world.get_entity_mut(entity) {
                entity.despawn();
            }
        }
    }
    world.insert_resource(occlusion::CoveredOutputs::default());
}
