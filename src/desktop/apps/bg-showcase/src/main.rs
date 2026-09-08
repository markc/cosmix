//! Exploratory scenes: native background surfaces, no input or X11 backend.
//! The host owns presentation; scene code only owns content and camera pose.

use cosmix_bg_boing as boing;
use cosmix_bg_showcase::boids;
mod coast;
mod control;
mod observatory;
mod upstream;

use bevy::{
    asset::{
        AssetApp,
        io::{AssetSource, AssetSourceBuilder},
    },
    camera::visibility::RenderLayers,
    core_pipeline::tonemapping::Tonemapping,
    post_process::bloom::Bloom,
    prelude::*,
    render::view::screenshot::{Screenshot, ScreenshotCaptured},
};
use cosmix_shell_host::scene::{
    SceneAfterRender, SceneCameraKind, SceneFrameResults, SceneHostConfig, SceneTick,
    SceneUpdateDeadline, SceneViews, configure_scene_host_with_config,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scene {
    Boids,
    Boing,
    Bloom,
    Shapes,
    Primitives,
    Orbits,
    Mist,
    Observatory,
    Celestial,
    Coast,
}

impl Scene {
    fn is_upstream(self) -> bool {
        matches!(self, Self::Bloom | Self::Shapes)
    }

    fn is_authored(self) -> bool {
        matches!(self, Self::Observatory | Self::Celestial)
    }

    fn uses_media(self) -> bool {
        self.is_authored() || self == Self::Coast
    }

    fn supports_camera(self) -> bool {
        self.is_upstream() || self == Self::Boing
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CameraMotion {
    #[default]
    Fixed,
    Orbit,
}

#[derive(Resource)]
struct Options {
    scene: Scene,
    camera: CameraMotion,
    fps: u32,
    msaa: u32,
    seconds: u32,
    capture: Option<PathBuf>,
    media_root: PathBuf,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut result = Self {
            scene: Scene::Orbits,
            camera: CameraMotion::Fixed,
            fps: 30,
            msaa: 4,
            seconds: 30,
            capture: None,
            media_root: PathBuf::new(),
        };
        let mut camera_supplied = false;
        for pair in args.chunks(2) {
            let [flag, value] = pair else {
                return Err("every option requires a value".into());
            };
            match flag.as_str() {
                "--scene" => {
                    result.scene = match value.as_str() {
                        "boing" => Scene::Boing,
                        "boids" => Scene::Boids,
                        "bloom" => Scene::Bloom,
                        "shapes" => Scene::Shapes,
                        "primitives" => Scene::Primitives,
                        "orbits" => Scene::Orbits,
                        "mist" => Scene::Mist,
                        "observatory" => Scene::Observatory,
                        "celestial" => Scene::Celestial,
                        "coast" => Scene::Coast,
                        _ => return Err("unknown scene".into()),
                    }
                }
                "--camera" => {
                    result.camera = match value.as_str() {
                        "fixed" => CameraMotion::Fixed,
                        "orbit" => CameraMotion::Orbit,
                        _ => return Err("camera must be fixed or orbit".into()),
                    };
                    camera_supplied = true;
                }
                "--fps" => {
                    result.fps = value
                        .parse()
                        .ok()
                        .filter(|v| (1..=60).contains(v))
                        .ok_or("fps must be 1–60")?
                }
                "--msaa" => {
                    result.msaa = value
                        .parse()
                        .ok()
                        .filter(|v| matches!(v, 1 | 4))
                        .ok_or("msaa must be 1 (off) or 4")?;
                }
                "--seconds" => {
                    result.seconds = value
                        .parse()
                        .ok()
                        .filter(|v| (0..=300).contains(v))
                        .ok_or("seconds must be 0 (continuous) or 1–300")?
                }
                "--capture" => {
                    let path = PathBuf::from(value);
                    if !path.is_absolute()
                        || path.extension().is_none_or(|e| e != "png")
                        || path.exists()
                    {
                        return Err("capture must be a new absolute .png path".into());
                    }
                    result.capture = Some(path);
                }
                "--media-root" => {
                    let path = PathBuf::from(value);
                    if !path.is_absolute() {
                        return Err("media-root must be an absolute directory".into());
                    }
                    result.media_root = path;
                }
                _ => return Err(format!("unknown option: {flag}")),
            }
        }
        if result.seconds == 0 && result.capture.is_some() {
            return Err("capture requires a bounded --seconds value from 1–300".into());
        }
        if camera_supplied && !result.scene.supports_camera() {
            return Err("--camera applies only to bloom, shapes and boing".into());
        }
        if result.scene.uses_media() && result.media_root.as_os_str().is_empty() {
            result.media_root = default_media_root()?;
        }
        Ok(result)
    }
}

fn default_media_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from)
        && path.is_absolute()
    {
        return Ok(path.join("cosmix/media"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(".local/share/cosmix/media"))
        .ok_or_else(|| "HOME or absolute XDG_DATA_HOME is required".into())
}

fn main() -> AppExit {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!(
            "cosmix-bg-showcase [--scene boing|bloom|shapes|boids|primitives|orbits|mist|observatory|celestial|coast] [--camera fixed|orbit] [--media-root /absolute/directory] [--fps 1..60] [--seconds 0..300] [--capture /absolute/new.png]\nNative-Wayland background preview. Default: orbits, 30 fps, 30 seconds.\n--seconds 0 runs continuously until stopped; 1–300 sets a bounded preview.\n--camera applies only to bloom/shapes/boing: fixed (default) preserves the initial view; orbit circles bloom/shapes or follows a front-facing arc for boing.\nBoing uses Avian 0.7 gravity and elastic collisions, independently paused per output.\nMedia defaults to $XDG_DATA_HOME/cosmix/media or $HOME/.local/share/cosmix/media. Renderer never downloads media.\nCapture requires a bounded duration and reads only this app's first output after assets are ready and 60 admitted frames.\nNative ABP bg-showcase: background.list/status/select; boids retains wallpaper.* preferences, pointer and window avoidance."
        );
        println!(
            "--msaa 1|4 selects off or four samples (default 4). Native background.select accepts optional msaa; omission retains current quality."
        );
        return AppExit::Success;
    }
    if args == ["--version"] {
        println!("cosmix-bg-showcase {}", env!("CARGO_PKG_VERSION"));
        return AppExit::Success;
    }
    let options = match Options::parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}; see --help");
            return AppExit::error();
        }
    };
    let mut app = App::new();
    if options.scene.uses_media() {
        app.register_asset_source(
            "media",
            AssetSourceBuilder::new(AssetSource::get_default_reader(
                options.media_root.to_string_lossy().into_owned(),
            )),
        );
    }
    configure_scene_host_with_config(
        &mut app,
        SceneHostConfig {
            fps: options.fps,
            namespace: "cosmix-bg-showcase".into(),
            title: "Cosmix 3D background showcase".into(),
            camera_kind: SceneCameraKind::ThreeD,
        },
    )
    .expect("validated scene configuration");
    control::configure(&mut app);
    boids::configure_content(&mut app);
    app.insert_resource(boids::Active(options.scene == Scene::Boids));
    if options.scene.is_authored() {
        observatory::configure(&mut app);
    }
    if options.scene == Scene::Coast {
        app.add_systems(Startup, coast::load).add_systems(
            Update,
            coast::prepare
                .before(reconcile)
                .run_if(|options: Res<Options>| options.scene == Scene::Coast),
        );
    }
    let ambient = if options.scene.is_upstream() {
        GlobalAmbientLight::default()
    } else {
        GlobalAmbientLight {
            color: Color::WHITE,
            brightness: 180.0,
            ..default()
        }
    };
    app.insert_resource(Preview {
        until: (options.seconds != 0)
            .then(|| Instant::now() + Duration::from_secs(u64::from(options.seconds))),
        frames: 0,
        request: None,
        accepted: None,
        completed: false,
        frame_window: None,
    })
    .insert_resource(options)
    .insert_non_send(boing::Simulations::default())
    .init_resource::<Instances>()
    .insert_resource(ambient)
    .add_systems(Startup, assets)
    .add_systems(
        Update,
        (reconcile, animate, preview)
            .chain()
            .after(control::service),
    )
    .configure_sets(Update, boids::Content.after(reconcile).before(preview))
    .add_systems(SceneAfterRender, capture_submission);
    app.run()
}

#[derive(Resource)]
struct SceneAssets {
    shapes: Vec<Handle<Mesh>>,
    sphere: Handle<Mesh>,
    cube: Handle<Mesh>,
    torus: Handle<Mesh>,
    materials: Vec<Handle<StandardMaterial>>,
    floor: Handle<StandardMaterial>,
}

fn assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    options: Res<Options>,
) {
    let shapes = vec![
        meshes.add(Cuboid::default()),
        meshes.add(Torus::default()),
        meshes.add(Sphere::new(0.65).mesh().ico(3).unwrap()),
        meshes.add(Capsule3d::default()),
        meshes.add(Tetrahedron::default()),
    ];
    let palette = if options.scene == Scene::Mist {
        [
            (0.10, 0.25, 0.30),
            (0.18, 0.32, 0.37),
            (0.25, 0.39, 0.42),
            (0.32, 0.45, 0.46),
            (0.42, 0.53, 0.51),
        ]
    } else {
        [
            (0.04, 0.55, 0.85),
            (0.55, 0.12, 0.95),
            (1.0, 0.2, 0.32),
            (0.1, 0.95, 0.65),
            (1.0, 0.65, 0.15),
        ]
    };
    let floor = materials.add(StandardMaterial {
        base_color: Color::srgb(0.025, 0.04, 0.065),
        metallic: 0.35,
        perceptual_roughness: 0.5,
        ..default()
    });
    let materials = palette
        .into_iter()
        .map(|(r, g, b)| {
            materials.add(StandardMaterial {
                base_color: Color::srgb(r, g, b),
                metallic: 0.55,
                perceptual_roughness: 0.27,
                emissive: if options.scene == Scene::Orbits {
                    LinearRgba::rgb(r * 8.0, g * 8.0, b * 8.0)
                } else {
                    LinearRgba::BLACK
                },
                ..default()
            })
        })
        .collect();
    // Reuse one mesh/material per family across all output-local scene instances.
    commands.insert_resource(SceneAssets {
        shapes,
        sphere: meshes.add(Sphere::new(0.18).mesh().ico(2).unwrap()),
        cube: meshes.add(Cuboid::default()),
        torus: meshes.add(
            Torus::new(2.95, 3.0)
                .mesh()
                .major_resolution(128)
                .minor_resolution(12),
        ),
        materials,
        floor,
    });
}

struct Instance {
    window: Entity,
    root: Entity,
    camera: Entity,
    elapsed: f32,
    last: Option<Instant>,
}
#[derive(Resource, Default)]
struct Instances(BTreeMap<String, Instance>);
#[derive(Component)]
struct Motion {
    window: Entity,
    index: usize,
    base: Vec3,
}

#[allow(clippy::too_many_arguments)]
fn reconcile(
    mut commands: Commands,
    views: Res<SceneViews>,
    options: Res<Options>,
    assets: Res<SceneAssets>,
    mut instances: ResMut<Instances>,
    authored: Option<Res<observatory::ObservatoryAsset>>,
    coast_assets: Option<Res<coast::CoastAssets>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut simulations: NonSendMut<boing::Simulations>,
    camera_msaa: Query<&Msaa>,
) {
    simulations
        .0
        .retain(|window, _| views.0.values().any(|view| view.window == *window));
    instances.0.retain(|name, instance| {
        if views
            .0
            .get(name)
            .is_some_and(|v| v.window == instance.window)
        {
            true
        } else {
            commands.entity(instance.root).despawn();
            false
        }
    });
    for (name, view) in &views.0 {
        let msaa = Msaa::from_samples(options.msaa);
        if camera_msaa.get(view.camera).ok() != Some(&msaa) {
            commands.entity(view.camera).insert(msaa);
        }
        if instances.0.contains_key(name) {
            continue;
        }
        let layer = RenderLayers::layer(view.render_layer);
        let root = commands
            .spawn((Transform::default(), Visibility::default()))
            .id();
        if options.scene == Scene::Boids {
            instances.0.insert(
                name.clone(),
                Instance {
                    window: view.window,
                    root,
                    camera: view.camera,
                    elapsed: 0.0,
                    last: None,
                },
            );
            continue;
        }
        if options.scene == Scene::Boing {
            boing::spawn(
                &mut commands,
                &mut meshes,
                &mut materials,
                &mut images,
                root,
                view.window,
                view.camera,
                layer,
                &mut simulations,
            );
            instances.0.insert(
                name.clone(),
                Instance {
                    window: view.window,
                    root,
                    camera: view.camera,
                    elapsed: 0.0,
                    last: None,
                },
            );
            continue;
        }
        if options.scene.is_upstream() {
            upstream::spawn(
                &mut commands,
                &mut meshes,
                &mut materials,
                &mut images,
                root,
                view.window,
                view.camera,
                layer,
                options.scene == Scene::Bloom,
            );
            instances.0.insert(
                name.clone(),
                Instance {
                    window: view.window,
                    root,
                    camera: view.camera,
                    elapsed: 0.0,
                    last: None,
                },
            );
            continue;
        }
        if options.scene == Scene::Coast {
            coast::spawn(
                &mut commands,
                &mut meshes,
                coast_assets.as_ref().expect("coast assets"),
                root,
                view.window,
                view.camera,
                layer,
            );
            instances.0.insert(
                name.clone(),
                Instance {
                    window: view.window,
                    root,
                    camera: view.camera,
                    elapsed: 0.0,
                    last: None,
                },
            );
            continue;
        }
        let background = match options.scene {
            Scene::Mist => Color::srgb(0.09, 0.15, 0.23),
            Scene::Celestial => Color::BLACK,
            _ => Color::srgb(0.004, 0.007, 0.019),
        };
        let initial_camera = if options.scene == Scene::Celestial {
            Transform::from_xyz(0.0, 6.0, 18.0)
        } else {
            Transform::from_xyz(0.0, 5.0, 13.0)
        };
        commands.entity(view.camera).insert((
            initial_camera.looking_at(Vec3::ZERO, Vec3::Y),
            Tonemapping::TonyMcMapface,
        ));
        // Modify only clear colour: never replace the host's Camera (activation/target).
        let camera = view.camera;
        commands.queue(move |world: &mut World| {
            if let Some(mut camera) = world.get_mut::<Camera>(camera) {
                camera.clear_color = ClearColorConfig::Custom(background);
            }
        });
        if options.scene == Scene::Orbits || options.scene.is_authored() {
            commands.entity(view.camera).insert(Bloom::NATURAL);
        }
        if options.scene == Scene::Mist {
            commands.entity(view.camera).insert(DistanceFog {
                color: background,
                falloff: FogFalloff::Linear {
                    start: 8.0,
                    end: 45.0,
                },
                ..default()
            });
        }
        let light = commands
            .spawn((
                DirectionalLight {
                    illuminance: 8000.0,
                    shadow_maps_enabled: options.scene != Scene::Orbits,
                    ..default()
                },
                Transform::from_xyz(4.0, 8.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
                layer.clone(),
            ))
            .id();
        commands.entity(root).add_child(light);
        let count = match options.scene {
            Scene::Primitives => 15,
            Scene::Orbits => 72,
            Scene::Mist => 100,
            Scene::Observatory
            | Scene::Celestial
            | Scene::Coast
            | Scene::Bloom
            | Scene::Shapes
            | Scene::Boids
            | Scene::Boing => 0,
        };
        for index in 0..count {
            let (mesh, base, scale) = match options.scene {
                Scene::Primitives => (
                    assets.shapes[index % 5].clone(),
                    Vec3::new(
                        (index % 5) as f32 * 2.8 - 5.6,
                        0.0,
                        (index / 5) as f32 * 2.8 - 2.8,
                    ),
                    Vec3::splat(0.9),
                ),
                Scene::Orbits => (
                    assets.sphere.clone(),
                    Vec3::ZERO,
                    Vec3::splat(0.5 + (index % 4) as f32 * 0.2),
                ),
                Scene::Mist => {
                    let height = 0.7 + ((index * 37) % 19) as f32 * 0.3;
                    (
                        assets.cube.clone(),
                        Vec3::new(
                            (index % 10) as f32 * 3.2 - 14.4,
                            height / 2.0 - 2.5,
                            (index / 10) as f32 * 3.2 - 14.4,
                        ),
                        Vec3::new(1.3, height, 1.3),
                    )
                }
                Scene::Observatory
                | Scene::Celestial
                | Scene::Coast
                | Scene::Bloom
                | Scene::Shapes
                | Scene::Boids
                | Scene::Boing => {
                    unreachable!("authored geometry has no procedural instances")
                }
            };
            let entity = commands
                .spawn((
                    Mesh3d(mesh),
                    MeshMaterial3d(assets.materials[index % 5].clone()),
                    Transform::from_translation(base).with_scale(scale),
                    layer.clone(),
                    Motion {
                        window: view.window,
                        index,
                        base,
                    },
                ))
                .id();
            commands.entity(root).add_child(entity);
        }
        if options.scene.is_authored() {
            let authored = authored.as_ref().expect("authored asset loaded at startup");
            let entity = commands
                .spawn((
                    WorldAssetRoot(authored.0.clone()),
                    observatory::AuthoredInstance {
                        window: view.window,
                        layer: layer.clone(),
                        ready: false,
                    },
                ))
                .observe(observatory::ready)
                .id();
            commands.entity(root).add_child(entity);
        }
        if options.scene == Scene::Orbits {
            for index in 0..3 {
                let entity = commands
                    .spawn((
                        Mesh3d(assets.torus.clone()),
                        MeshMaterial3d(assets.materials[index].clone()),
                        Transform::from_rotation(Quat::from_euler(
                            EulerRot::XYZ,
                            0.4 + index as f32 * 0.5,
                            0.3 * index as f32,
                            0.4,
                        ))
                        .with_scale(Vec3::splat(1.0 + index as f32 * 0.35)),
                        layer.clone(),
                    ))
                    .id();
                commands.entity(root).add_child(entity);
            }
        } else if options.scene != Scene::Celestial {
            let entity = commands
                .spawn((
                    Mesh3d(assets.cube.clone()),
                    MeshMaterial3d(assets.floor.clone()),
                    Transform::from_xyz(0.0, -2.7, 0.0).with_scale(Vec3::new(80.0, 0.2, 80.0)),
                    layer.clone(),
                ))
                .id();
            commands.entity(root).add_child(entity);
        }
        instances.0.insert(
            name.clone(),
            Instance {
                window: view.window,
                root,
                camera: view.camera,
                elapsed: 0.0,
                last: None,
            },
        );
    }
}

fn step(last: &mut Option<Instant>, now: Instant) -> f32 {
    let dt = last.map_or(0.0, |at| {
        now.saturating_duration_since(at).as_secs_f32().min(0.05)
    });
    *last = Some(now);
    dt
}

#[allow(clippy::too_many_arguments)]
fn animate(
    tick: Res<SceneTick>,
    options: Res<Options>,
    mut instances: ResMut<Instances>,
    mut transforms: Query<&mut Transform>,
    motions: Query<(Entity, &Motion)>,
    upstream_motions: Query<(Entity, &upstream::Motion)>,
    spins: Query<(Entity, &observatory::Spin)>,
    water: Query<(&Mesh3d, &coast::Water)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut simulations: NonSendMut<boing::Simulations>,
) {
    let now = Instant::now();
    for (name, instance) in &mut instances.0 {
        if !tick.0.contains(name) {
            instance.last = None;
            continue;
        }
        let dt = step(&mut instance.last, now);
        instance.elapsed += dt;
        let t = instance.elapsed;
        if options.scene == Scene::Boids {
            continue;
        }
        if options.scene == Scene::Boing {
            if let Some(simulation) = simulations.0.get_mut(&instance.window) {
                let pose = simulation.advance(dt);
                if let Ok(mut transform) = transforms.get_mut(simulation.visual) {
                    *transform = pose;
                }
            }
            if options.camera == CameraMotion::Orbit
                && let Ok(mut camera) = transforms.get_mut(instance.camera)
            {
                *camera = boing::camera_pose(t);
            }
            continue;
        }
        if options.scene.is_upstream() {
            if options.camera == CameraMotion::Orbit
                && let Ok(mut camera) = transforms.get_mut(instance.camera)
            {
                *camera = upstream::camera_pose(options.scene == Scene::Bloom, t);
            }
            for (entity, motion) in &upstream_motions {
                if motion.window == instance.window
                    && let Ok(mut transform) = transforms.get_mut(entity)
                {
                    upstream::animate(&mut transform, motion.bloom, t);
                }
            }
            continue;
        }
        if options.scene == Scene::Coast {
            for (mesh, water) in &water {
                if water.window == instance.window
                    && let Some(mut mesh) = meshes.get_mut(&mesh.0)
                {
                    coast::waves(&mut mesh, t);
                }
            }
            continue;
        }
        for (entity, spin) in &spins {
            if spin.window == instance.window
                && let Ok(mut transform) = transforms.get_mut(entity)
            {
                transform.rotation = spin.base * Quat::from_axis_angle(spin.axis, spin.speed * t);
            }
        }
        if let Ok(mut camera) = transforms.get_mut(instance.camera) {
            let (radius, height, speed) = match options.scene {
                Scene::Mist => (20.0, 7.0, 0.045),
                Scene::Celestial => (18.0, 6.0, 0.035),
                _ => (13.5, 5.0, 0.09),
            };
            *camera = Transform::from_xyz(
                (t * speed).sin() * radius,
                height,
                (t * speed).cos() * radius,
            )
            .looking_at(Vec3::ZERO, Vec3::Y);
        }
        for (entity, motion) in &motions {
            if motion.window != instance.window {
                continue;
            }
            let Ok(mut transform) = transforms.get_mut(entity) else {
                continue;
            };
            let phase = motion.index as f32 * 2.399963;
            match options.scene {
                Scene::Primitives => {
                    transform.translation.y = motion.base.y + (t * 0.55 + phase).sin() * 0.4;
                    transform.rotation =
                        Quat::from_euler(EulerRot::XYZ, t * 0.15 + phase, t * 0.25, 0.2);
                }
                Scene::Orbits => {
                    let radius = 2.0 + (motion.index % 6) as f32 * 0.55;
                    let a = phase + t * (0.15 + (motion.index % 3) as f32 * 0.06);
                    transform.translation = Vec3::new(
                        a.cos() * radius,
                        (a * 1.7 + phase).sin() * 1.5,
                        a.sin() * radius,
                    );
                }
                Scene::Mist => {}
                Scene::Observatory
                | Scene::Celestial
                | Scene::Coast
                | Scene::Bloom
                | Scene::Shapes
                | Scene::Boids
                | Scene::Boing => {}
            }
        }
    }
}

#[derive(Resource)]
struct Preview {
    until: Option<Instant>,
    frames: u32,
    request: Option<Entity>,
    accepted: Option<bool>,
    completed: bool,
    frame_window: Option<Entity>,
}

// Bevy injects these independent resources and queries as system parameters.
#[allow(clippy::too_many_arguments)]
fn preview(
    mut commands: Commands,
    options: Res<Options>,
    views: Res<SceneViews>,
    tick: Res<SceneTick>,
    mut preview: ResMut<Preview>,
    mut deadline: ResMut<SceneUpdateDeadline>,
    mut exit: MessageWriter<AppExit>,
    authored: Option<Res<observatory::ObservatoryAsset>>,
    authored_instances: Query<&observatory::AuthoredInstance>,
    server: Res<AssetServer>,
    coast_assets: Option<Res<coast::CoastAssets>>,
) {
    deadline.0 = match (deadline.0, preview.until) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if options.scene.is_authored() && authored.as_ref().is_some_and(|asset| asset.failed(&server)) {
        error!("BG_OBSERVATORY_ASSET_FAILED");
        exit.write(AppExit::error());
        return;
    }
    let ready = |window| {
        if options.scene == Scene::Coast {
            return coast_assets.as_ref().is_some_and(|asset| asset.ready);
        }
        !options.scene.is_authored()
            || (authored.as_ref().is_some_and(|asset| asset.loaded(&server))
                && authored_instances
                    .iter()
                    .any(|instance| instance.window == window && instance.ready))
    };
    if preview.until.is_some_and(|until| Instant::now() >= until) {
        if (options.scene.is_authored() || options.scene == Scene::Coast)
            && !views.0.values().any(|view| ready(view.window))
        {
            error!("BG_OBSERVATORY_NOT_READY");
            exit.write(AppExit::error());
        } else if options.capture.is_some() && !preview.completed {
            error!("BG_CAPTURE_TIMEOUT");
            exit.write(AppExit::error());
        } else {
            exit.write(AppExit::Success);
        }
        return;
    }
    if preview.completed {
        exit.write(AppExit::Success);
        return;
    }
    if options.capture.is_none() || preview.request.is_some() {
        return;
    }
    if let Some((name, view)) = views.0.first_key_value()
        && tick.0.contains(name)
    {
        if preview.frame_window != Some(view.window) {
            preview.frame_window = Some(view.window);
            preview.frames = 0;
        }
        if !ready(view.window) {
            preview.frames = 0;
            return;
        }
        preview.frames += 1;
        if preview.frames >= 60 {
            preview.request = Some(view.window);
            commands
                .spawn(Screenshot::window(view.window))
                .observe(captured);
        }
    }
}

fn capture_submission(mut preview: ResMut<Preview>, results: Res<SceneFrameResults>) {
    if preview.accepted.is_none()
        && let Some(window) = preview.request
        && let Some(submitted) = results.0.get(&window)
    {
        preview.accepted = Some(*submitted);
    }
}

fn captured(
    event: On<ScreenshotCaptured>,
    options: Res<Options>,
    mut preview: ResMut<Preview>,
    mut exit: MessageWriter<AppExit>,
) {
    if preview.accepted != Some(true) {
        error!("BG_CAPTURE_UNSUBMITTED");
        exit.write(AppExit::error());
        return;
    }
    let Some(path) = options.capture.as_ref() else {
        return;
    };
    match event
        .image
        .clone()
        .try_into_dynamic()
        .map_err(|e| e.to_string())
        .and_then(|image| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| e.to_string())?;
            image
                .write_to(&mut file, image::ImageFormat::Png)
                .map_err(|e| e.to_string())
        }) {
        Ok(()) => {
            preview.completed = true;
            info!(
                "BG_CAPTURE_SAVED scene={:?} frames={}",
                options.scene, preview.frames
            );
        }
        Err(error) => {
            error!(%error,"BG_CAPTURE_FAILED");
            exit.write(AppExit::error());
        }
    }
}

impl Scene {
    fn keeper(name: &str) -> Option<Self> {
        match name {
            "boing" => Some(Self::Boing),
            "bloom" => Some(Self::Bloom),
            "shapes" => Some(Self::Shapes),
            "boids" => Some(Self::Boids),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Boing => "boing",
            Self::Bloom => "bloom",
            Self::Shapes => "shapes",
            Self::Boids => "boids",
            Self::Primitives => "primitives",
            Self::Orbits => "orbits",
            Self::Mist => "mist",
            Self::Observatory => "observatory",
            Self::Celestial => "celestial",
            Self::Coast => "coast",
        }
    }
}
impl CameraMotion {
    fn name(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Orbit => "orbit",
        }
    }
}

/// Host windows/cameras survive selection; all scene content and simulation clocks do not.
fn reset_scene(world: &mut World) {
    let instances = std::mem::take(&mut world.resource_mut::<Instances>().0);
    for instance in instances.into_values() {
        if let Ok(entity) = world.get_entity_mut(instance.root) {
            entity.despawn();
        }
    }
    world.non_send_mut::<boing::Simulations>().0.clear();
    boids::clear(world);
    let scene = world.resource::<Options>().scene;
    world.resource_mut::<boids::Active>().0 = scene == Scene::Boids;
    world.insert_resource(if scene.is_upstream() {
        GlobalAmbientLight::default()
    } else {
        GlobalAmbientLight {
            color: Color::WHITE,
            brightness: 180.0,
            ..default()
        }
    });
    let cameras: Vec<_> = world
        .resource::<SceneViews>()
        .0
        .values()
        .map(|view| view.camera)
        .collect();
    for camera in cameras {
        if let Ok(mut camera) = world.get_entity_mut(camera) {
            camera.remove::<(
                Bloom,
                DistanceFog,
                bevy::light::Skybox,
                bevy::light::EnvironmentMapLight,
            )>();
            camera.insert((
                Projection::Perspective(PerspectiveProjection::default()),
                Tonemapping::TonyMcMapface,
                Transform::default(),
            ));
        }
    }
    let mut control = world.resource_mut::<cosmix_shell_host::scene::SceneControl>();
    control.suspended_outputs.clear();
    control.paused = false;
    control.hidden = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn msaa_options_are_bounded_and_default_to_existing_quality() {
        assert_eq!(Options::parse(&[]).unwrap().msaa, 4);
        for samples in [1, 4] {
            assert_eq!(
                Options::parse(&["--msaa".into(), samples.to_string()])
                    .unwrap()
                    .msaa,
                samples
            );
        }
        for value in ["0", "2", "8", "-1", "off"] {
            assert!(Options::parse(&["--msaa".into(), value.into()]).is_err());
        }
    }
    #[test]
    fn switching_clears_content_and_camera_effects_but_keeps_host_targets() {
        let mut app = App::new();
        boids::configure_content(&mut app);
        app.insert_resource(Options::parse(&["--scene".into(), "boing".into()]).unwrap())
            .insert_resource(boids::Active(false))
            .init_resource::<Instances>()
            .init_resource::<SceneViews>()
            .init_resource::<cosmix_shell_host::scene::SceneControl>()
            .insert_non_send(boing::Simulations::default());
        let window = app.world_mut().spawn_empty().id();
        let camera = app
            .world_mut()
            .spawn((
                Camera::default(),
                Msaa::Off,
                Transform::default(),
                Bloom::NATURAL,
                DistanceFog::default(),
                Projection::Orthographic(OrthographicProjection::default_3d()),
            ))
            .id();
        app.world_mut().resource_mut::<SceneViews>().0.insert(
            "TEST".into(),
            cosmix_shell_host::scene::SceneView {
                window,
                camera,
                logical_size: (800, 600),
                origin: (0, 0),
                scale: 1.0,
                render_layer: 0,
                configuration: 1,
            },
        );
        for next in [
            Scene::Boids,
            Scene::Bloom,
            Scene::Shapes,
            Scene::Boing,
            Scene::Boids,
        ] {
            let root = app.world_mut().spawn_empty().id();
            let child = app.world_mut().spawn(ChildOf(root)).id();
            app.world_mut().resource_mut::<Instances>().0.insert(
                "TEST".into(),
                Instance {
                    window,
                    root,
                    camera,
                    elapsed: 20.0,
                    last: Some(Instant::now()),
                },
            );
            app.world_mut().resource_mut::<Options>().scene = next;
            reset_scene(app.world_mut());
            assert!(app.world().get_entity(root).is_err());
            assert!(app.world().get_entity(child).is_err());
            assert!(app.world().get_entity(window).is_ok());
            assert!(app.world().get::<Camera>(camera).is_some());
            assert_eq!(app.world().get::<Msaa>(camera), Some(&Msaa::Off));
            assert!(app.world().get::<Bloom>(camera).is_none());
            assert!(app.world().get::<DistanceFog>(camera).is_none());
            assert!(matches!(
                app.world().get::<Projection>(camera),
                Some(Projection::Perspective(_))
            ));
            assert!(app.world().resource::<Instances>().0.is_empty());
            assert_eq!(
                app.world().resource::<boids::Active>().0,
                next == Scene::Boids
            );
        }
    }
    #[test]
    fn camera_option_is_explicit_and_limited_to_upstream_scenes() {
        for scene in ["bloom", "shapes", "boing"] {
            assert_eq!(
                Options::parse(&["--scene".into(), scene.into()])
                    .unwrap()
                    .camera,
                CameraMotion::Fixed
            );
            assert_eq!(
                Options::parse(&[
                    "--camera".into(),
                    "orbit".into(),
                    "--scene".into(),
                    scene.into()
                ])
                .unwrap()
                .camera,
                CameraMotion::Orbit
            );
            assert_eq!(
                Options::parse(&[
                    "--scene".into(),
                    scene.into(),
                    "--camera".into(),
                    "fixed".into()
                ])
                .unwrap()
                .camera,
                CameraMotion::Fixed
            );
        }
        assert!(
            Options::parse(&[
                "--scene".into(),
                "bloom".into(),
                "--camera".into(),
                "fly".into()
            ])
            .is_err()
        );
        assert!(Options::parse(&["--camera".into(), "orbit".into()]).is_err());
        assert!(
            Options::parse(&[
                "--scene".into(),
                "coast".into(),
                "--camera".into(),
                "fixed".into()
            ])
            .is_err()
        );
    }
    #[test]
    fn preview_bounds_and_scene_selection_are_explicit() {
        assert_eq!(Options::parse(&[]).unwrap().seconds, 30);
        assert_eq!(
            Options::parse(&["--seconds".into(), "0".into()])
                .unwrap()
                .seconds,
            0
        );
        let capture = format!(
            "/tmp/cosmix-continuous-capture-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        for args in [
            vec!["--seconds", "0", "--capture", &capture],
            vec!["--capture", &capture, "--seconds", "0"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            assert!(
                Options::parse(&args)
                    .err()
                    .unwrap()
                    .contains("bounded --seconds")
            );
        }
        assert!(Options::parse(&["--fps".into(), "0".into()]).is_err());
        assert!(Options::parse(&["--seconds".into(), "301".into()]).is_err());
        assert!(Options::parse(&["--scene".into(), "riverbed".into()]).is_err());
        assert_eq!(
            Options::parse(&["--scene".into(), "mist".into()])
                .unwrap()
                .scene,
            Scene::Mist
        );
        assert_eq!(
            Options::parse(&["--scene".into(), "observatory".into()])
                .unwrap()
                .scene,
            Scene::Observatory
        );
        assert_eq!(
            Options::parse(&["--scene".into(), "celestial".into()])
                .unwrap()
                .scene,
            Scene::Celestial
        );
    }
    #[test]
    fn resume_never_catches_up_hidden_time() {
        let now = Instant::now();
        let mut last = None;
        assert_eq!(step(&mut last, now), 0.0);
        assert_eq!(step(&mut last, now + Duration::from_secs(60)), 0.05);
        last = None;
        assert_eq!(step(&mut last, now + Duration::from_secs(120)), 0.0);
    }
}
