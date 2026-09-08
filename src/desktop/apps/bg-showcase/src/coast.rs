//! Quiet procedural shore, with an externally supplied, tonemapped sky cubemap.
use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::RenderLayers,
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
    light::{EnvironmentMapLight, Skybox},
    mesh::VertexAttributeValues,
    prelude::*,
    render::render_resource::{TextureViewDescriptor, TextureViewDimension},
};

#[derive(Resource)]
pub struct CoastAssets {
    sky: Handle<Image>,
    sand_basecolor: Handle<Image>,
    sand_normal: Handle<Image>,
    pub ready: bool,
    sand: Handle<StandardMaterial>,
    rock: Handle<StandardMaterial>,
    grass: Handle<StandardMaterial>,
    water: Handle<StandardMaterial>,
    rock_mesh: Handle<Mesh>,
    grass_mesh: Handle<Mesh>,
    shore: Handle<Mesh>,
}

#[derive(Component)]
pub struct Water {
    pub window: Entity,
}

pub fn load(
    mut commands: Commands,
    server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let sand_basecolor = server
        .load_builder()
        .with_settings(|settings: &mut ImageLoaderSettings| {
            settings.sampler = repeating_sampler();
        })
        .load("media://coast/sand-basecolor.png");
    let sand_normal = server
        .load_builder()
        .with_settings(|settings: &mut ImageLoaderSettings| {
            settings.is_srgb = false;
            settings.sampler = repeating_sampler();
        })
        .load("media://coast/sand-normal.png");
    let mut shore = Plane3d::default()
        .mesh()
        .size(100.0, 100.0)
        .subdivisions(256)
        .build();
    let mut colours = Vec::new();
    if let Some(VertexAttributeValues::Float32x3(positions)) =
        shore.attribute_mut(Mesh::ATTRIBUTE_POSITION)
    {
        for [x, y, z] in positions {
            let world_z = *z + 38.0;
            *y = (world_z - 1.5 * (*x * 0.13).sin()) * 0.045 - 0.12;
            *y += 0.035 * (*x * 0.63 + *z * 0.37).sin();
            *y += 0.013 * (*x * 5.7 + *z * 3.2).sin();
            let grain = ((*x * 17.13 + *z * 29.71).sin() * 13.37).fract().abs();
            let wet = (world_z / 7.0).clamp(0.0, 1.0);
            let shade = (0.63 + 0.26 * wet) * (0.86 + grain * 0.14);
            colours.push([shade, shade, shade, 1.0]);
        }
    }
    shore.insert_attribute(Mesh::ATTRIBUTE_COLOR, colours);
    if let Some(VertexAttributeValues::Float32x2(uvs)) = shore.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        for [u, v] in uvs {
            *u *= 100.0 / 3.0;
            *v *= 100.0 / 3.0;
        }
    }
    shore.compute_normals();
    shore
        .generate_tangents()
        .expect("indexed shore has normals and UVs");
    let mut rock_mesh = Sphere::new(1.0).mesh().ico(3).unwrap();
    if let Some(VertexAttributeValues::Float32x3(positions)) =
        rock_mesh.attribute_mut(Mesh::ATTRIBUTE_POSITION)
    {
        for [x, y, z] in positions {
            let stretch = 1.0 + 0.14 * (*x * 7.0 + *y * 3.0 + *z * 5.0).sin();
            *x *= stretch;
            *y *= stretch;
            *z *= stretch;
        }
    }
    rock_mesh.compute_normals();
    commands.insert_resource(CoastAssets {
        sky: server.load("media://coast/skybox.png"),
        ready: false,
        sand: materials.add(StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(sand_basecolor.clone()),
            normal_map_texture: Some(sand_normal.clone()),
            perceptual_roughness: 0.95,
            ..default()
        }),
        rock: materials.add(StandardMaterial {
            base_color: Color::srgb(0.38, 0.40, 0.37),
            base_color_texture: Some(sand_basecolor.clone()),
            perceptual_roughness: 0.88,
            ..default()
        }),
        grass: materials.add(StandardMaterial {
            base_color: Color::srgb(0.3, 0.35, 0.17),
            perceptual_roughness: 0.95,
            ..default()
        }),
        water: materials.add(StandardMaterial {
            base_color: Color::srgb(0.035, 0.19, 0.23),
            metallic: 0.05,
            perceptual_roughness: 0.13,
            reflectance: 0.5,
            ..default()
        }),
        rock_mesh: meshes.add(rock_mesh),
        grass_mesh: meshes.add(Cone::new(0.012, 0.55)),
        shore: meshes.add(shore),
        sand_basecolor,
        sand_normal,
    });
}

fn repeating_sampler() -> ImageSampler {
    ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        ..default()
    })
}

pub fn prepare(
    mut commands: Commands,
    server: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut assets: ResMut<CoastAssets>,
    cameras: Query<Entity, With<Skybox>>,
    mut exit: MessageWriter<AppExit>,
) {
    if assets.ready {
        return;
    }
    let required = [&assets.sky, &assets.sand_basecolor, &assets.sand_normal];
    if required
        .iter()
        .any(|handle| server.load_state(*handle).is_failed())
    {
        error!(
            "BG_COAST_MEDIA_FAILED: supply coast/skybox.png, sand-basecolor.png and sand-normal.png under --media-root"
        );
        exit.write(AppExit::error());
        return;
    }
    if !required
        .iter()
        .all(|handle| server.is_loaded_with_dependencies(*handle))
    {
        return;
    }
    let Some(mut image) = images.get_mut(&assets.sky) else {
        return;
    };
    if image.width() == 0 || image.height() != image.width() * 6 {
        error!("BG_COAST_SKY_INVALID: expected six square faces stacked +X,-X,+Y,-Y,+Z,-Z");
        exit.write(AppExit::error());
        return;
    }
    if let Err(error) = image.reinterpret_stacked_2d_as_array(6) {
        error!(%error, "BG_COAST_SKY_INVALID");
        exit.write(AppExit::error());
        return;
    }
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::Cube),
        ..default()
    });
    assets.ready = true;
    for camera in &cameras {
        commands
            .entity(camera)
            .insert((skybox(&assets), environment(&assets)));
    }
    info!("BG_COAST_READY: tonemapped sky, procedural shore and admitted-tick waves");
}

// Artistic approximation: this is the same LDR image, not prefiltered HDR irradiance.
fn environment(assets: &CoastAssets) -> EnvironmentMapLight {
    EnvironmentMapLight {
        diffuse_map: assets.sky.clone(),
        specular_map: assets.sky.clone(),
        intensity: 450.0,
        ..default()
    }
}

fn skybox(assets: &CoastAssets) -> Skybox {
    Skybox {
        image: assets.ready.then(|| assets.sky.clone()),
        brightness: 1000.0,
        ..default()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    assets: &CoastAssets,
    root: Entity,
    window: Entity,
    camera: Entity,
    layer: RenderLayers,
) {
    commands.entity(camera).insert((
        Transform::from_xyz(0.0, 4.2, 17.0).looking_at(Vec3::new(0.0, 0.0, -42.0), Vec3::Y),
        skybox(assets),
        DistanceFog {
            color: Color::srgb(0.59, 0.68, 0.69),
            falloff: FogFalloff::Linear {
                start: 90.0,
                end: 290.0,
            },
            ..default()
        },
    ));
    if assets.ready {
        commands.entity(camera).insert(environment(assets));
    }
    let light = commands
        .spawn((
            DirectionalLight {
                illuminance: 9000.0,
                shadow_maps_enabled: true,
                ..default()
            },
            Transform::from_xyz(-20.0, 35.0, -20.0).looking_at(Vec3::ZERO, Vec3::Y),
            layer.clone(),
        ))
        .id();
    commands.entity(root).add_child(light);
    let terrain = commands
        .spawn((
            Mesh3d(assets.shore.clone()),
            MeshMaterial3d(assets.sand.clone()),
            Transform::from_xyz(0.0, 0.0, 38.0),
            layer.clone(),
        ))
        .id();
    commands.entity(root).add_child(terrain);
    let mut ocean = Plane3d::default()
        .mesh()
        .size(120.0, 180.0)
        .subdivisions(256)
        .build();
    ocean.asset_usage = RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD;
    waves(&mut ocean, 0.0);
    let ocean = commands
        .spawn((
            Mesh3d(meshes.add(ocean)),
            MeshMaterial3d(assets.water.clone()),
            Transform::from_xyz(0.0, 0.0, -70.0),
            layer.clone(),
            Water { window },
        ))
        .id();
    commands.entity(root).add_child(ocean);
    let distant = commands
        .spawn((
            Mesh3d(meshes.add(Plane3d::default().mesh().size(1200.0, 1200.0))),
            MeshMaterial3d(assets.water.clone()),
            Transform::from_xyz(0.0, -0.3, -350.0),
            layer.clone(),
        ))
        .id();
    commands.entity(root).add_child(distant);
    for index in 0..23 {
        let phase = index as f32 * 2.399_963;
        let x = if index % 2 == 0 { -3.5 } else { 5.2 } + phase.sin() * 1.5;
        let z = 4.5 + (index % 6) as f32 * 1.05;
        let scale = 0.35 + (index % 5) as f32 * 0.19;
        let rock = commands
            .spawn((
                Mesh3d(assets.rock_mesh.clone()),
                MeshMaterial3d(assets.rock.clone()),
                Transform::from_xyz(x, z * 0.045 + scale * 0.24, z)
                    .with_scale(Vec3::new(scale * 1.6, scale * 0.65, scale))
                    .with_rotation(Quat::from_euler(
                        EulerRot::XYZ,
                        phase * 0.1,
                        phase,
                        phase * 0.08,
                    )),
                layer.clone(),
            ))
            .id();
        commands.entity(root).add_child(rock);
    }
    for index in 0..180 {
        let phase = index as f32 * 2.399_963;
        let x = phase.sin() * 1.2 + if index % 2 == 0 { -3.2 } else { 4.2 };
        let z = 6.0 + (index % 12) as f32 * 0.28;
        let grass = commands
            .spawn((
                Mesh3d(assets.grass_mesh.clone()),
                MeshMaterial3d(assets.grass.clone()),
                Transform::from_xyz(x, z * 0.045 + 0.25, z)
                    .with_rotation(Quat::from_euler(
                        EulerRot::XYZ,
                        phase.cos() * 0.4,
                        phase,
                        phase.sin() * 0.4,
                    ))
                    .with_scale(Vec3::splat(0.6 + (index % 4) as f32 * 0.2)),
                layer.clone(),
            ))
            .id();
        commands.entity(root).add_child(grass);
    }
}

pub fn waves(mesh: &mut Mesh, elapsed: f32) {
    let mut normals = Vec::new();
    if let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute_mut(Mesh::ATTRIBUTE_POSITION)
    {
        for [x, y, z] in positions {
            let a = *z * 1.4 + elapsed * 0.65;
            let b = *x * 1.2 + *z * 2.1 - elapsed * 0.47;
            let c = *x * 3.4 - *z * 2.8 + elapsed * 0.72;
            *y = 0.07 * a.sin() + 0.028 * b.sin() + 0.012 * c.sin();
            let dx = 0.028 * 1.2 * b.cos() + 0.012 * 3.4 * c.cos();
            let dz = 0.07 * 1.4 * a.cos() + 0.028 * 2.1 * b.cos() - 0.012 * 2.8 * c.cos();
            normals.push(Vec3::new(-dx, 1.0, -dz).normalize().to_array());
        }
    }
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
}
