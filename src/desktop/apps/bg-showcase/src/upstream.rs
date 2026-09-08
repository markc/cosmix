//! Minimal native-host adaptations of Bevy 0.19.0's `bloom_3d` and `3d_shapes`.
//! Source: https://github.com/bevyengine/bevy/tree/v0.19.0/examples/3d
//! Copyright (c) Bevy contributors. Used under the MIT licence; see
//! ../UPSTREAM-LICENSE.md. UI controls are omitted; scene geometry is retained.

use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::RenderLayers,
    core_pipeline::tonemapping::Tonemapping,
    post_process::bloom::Bloom,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

#[derive(Component)]
pub struct Motion {
    pub window: Entity,
    pub bloom: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    root: Entity,
    window: Entity,
    camera: Entity,
    layer: RenderLayers,
    bloom: bool,
) {
    let pose = camera_pose(bloom, 0.0);
    commands
        .entity(camera)
        .insert((pose, Tonemapping::TonyMcMapface));
    commands.queue(move |world: &mut World| {
        if let Some(mut camera) = world.get_mut::<Camera>(camera) {
            camera.clear_color = ClearColorConfig::Custom(if bloom {
                Color::BLACK
            } else {
                ClearColor::default().0
            });
        }
    });
    if bloom {
        commands.entity(camera).insert(Bloom::NATURAL);
        let palette = [
            materials.add(StandardMaterial {
                emissive: LinearRgba::rgb(0.0, 0.0, 150.0),
                ..default()
            }),
            materials.add(StandardMaterial {
                emissive: LinearRgba::rgb(1000.0, 1000.0, 1000.0),
                ..default()
            }),
            materials.add(StandardMaterial {
                emissive: LinearRgba::rgb(50.0, 0.0, 0.0),
                ..default()
            }),
            materials.add(StandardMaterial {
                base_color: Color::BLACK,
                ..default()
            }),
        ];
        let mesh = meshes.add(Sphere::new(0.4).mesh().ico(5).unwrap());
        for x in -5_i32..5 {
            for z in -5_i32..5 {
                let mut hasher = DefaultHasher::new();
                (x, z).hash(&mut hasher);
                let (index, scale) = match (hasher.finish() + 3) % 6 {
                    0 => (0, 0.5),
                    1 => (1, 0.1),
                    2 => (2, 1.0),
                    _ => (3, 1.5),
                };
                let entity = commands
                    .spawn((
                        Mesh3d(mesh.clone()),
                        MeshMaterial3d(palette[index].clone()),
                        Transform::from_xyz(x as f32 * 2.0, 0.0, z as f32 * 2.0)
                            .with_scale(Vec3::splat(scale)),
                        layer.clone(),
                        Motion { window, bloom },
                    ))
                    .id();
                commands.entity(root).add_child(entity);
            }
        }
        return;
    }
    let material = materials.add(StandardMaterial {
        base_color_texture: Some(images.add(uv_debug_texture())),
        ..default()
    });
    let shapes = vec![
        meshes.add(Cuboid::default()),
        meshes.add(Tetrahedron::default()),
        meshes.add(Capsule3d::default()),
        meshes.add(Torus::default()),
        meshes.add(Cylinder::default()),
        meshes.add(Cone::default()),
        meshes.add(ConicalFrustum::default()),
        meshes.add(Sphere::default().mesh().ico(5).unwrap()),
        meshes.add(Sphere::default().mesh().uv(32, 18)),
        meshes.add(Segment3d::default()),
        meshes.add(Polyline3d::new(vec![
            Vec3::new(-0.5, 0.0, 0.0),
            Vec3::new(0.5, 0.0, 0.0),
            Vec3::new(0.0, 0.5, 0.0),
        ])),
    ];
    let extrusions = vec![
        meshes.add(Extrusion::new(Rectangle::default(), 1.0)),
        meshes.add(Extrusion::new(Capsule2d::default(), 1.0)),
        meshes.add(Extrusion::new(Annulus::default(), 1.0)),
        meshes.add(Extrusion::new(Circle::default(), 1.0)),
        meshes.add(Extrusion::new(Ellipse::default(), 1.0)),
        meshes.add(Extrusion::new(RegularPolygon::default(), 1.0)),
        meshes.add(Extrusion::new(Triangle2d::default(), 1.0)),
        meshes.add(Extrusion::new(
            ConvexPolygon::new(vec![
                Vec2::new(0.0, 0.8),
                Vec2::new(-0.47, 0.25),
                Vec2::new(-0.47, -0.65),
                Vec2::new(0.47, -0.65),
                Vec2::new(0.47, 0.25),
            ])
            .unwrap(),
            1.0,
        )),
    ];
    let outer = Ellipse::default();
    let mut inner = outer;
    inner.half_size -= Vec2::splat(0.1);
    let rings = vec![
        meshes.add(Extrusion::new(Rectangle::default().to_ring(0.1), 1.0)),
        meshes.add(Extrusion::new(Capsule2d::default().to_ring(0.1), 1.0)),
        meshes.add(Extrusion::new(
            Ring::new(Circle::new(1.0), Circle::new(0.5)),
            1.0,
        )),
        meshes.add(Extrusion::new(Circle::default().to_ring(0.1), 1.0)),
        meshes.add(Extrusion::new(Ring::new(outer, inner), 1.0)),
        meshes.add(Extrusion::new(RegularPolygon::default().to_ring(0.1), 1.0)),
        meshes.add(Extrusion::new(Triangle2d::default().to_ring(0.1), 1.0)),
    ];
    for (row, z) in [(shapes, 4.0), (extrusions, 0.0), (rings, -4.0)] {
        let count = row.len();
        for (i, mesh) in row.into_iter().enumerate() {
            let entity = commands
                .spawn((
                    Mesh3d(mesh),
                    MeshMaterial3d(material.clone()),
                    Transform::from_xyz(-7.0 + i as f32 / (count - 1) as f32 * 14.0, 2.0, z)
                        .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_4)),
                    layer.clone(),
                    Motion { window, bloom },
                ))
                .id();
            commands.entity(root).add_child(entity);
        }
    }
    let light = commands
        .spawn((
            PointLight {
                shadow_maps_enabled: true,
                intensity: 10_000_000.0,
                range: 100.0,
                shadow_depth_bias: 0.2,
                ..default()
            },
            Transform::from_xyz(8.0, 16.0, 8.0),
            layer.clone(),
        ))
        .id();
    let floor = commands
        .spawn((
            Mesh3d(meshes.add(Plane3d::default().mesh().size(50.0, 50.0).subdivisions(10))),
            MeshMaterial3d(materials.add(Color::from(bevy::color::palettes::basic::SILVER))),
            layer,
        ))
        .id();
    commands.entity(root).add_children(&[light, floor]);
}

/// Begin at the upstream view and orbit at constant height and distance.
/// `elapsed` is output-local admitted time, so hidden time never advances it.
pub fn camera_pose(bloom: bool, elapsed: f32) -> Transform {
    let (position, target) = if bloom {
        (Vec3::new(-2.0, 2.5, 5.0), Vec3::ZERO)
    } else {
        (Vec3::new(0.0, 7.0, 14.0), Vec3::new(0.0, 1.0, 0.0))
    };
    // One revolution in two minutes; no zoom, roll or height change.
    let angle = elapsed * std::f32::consts::TAU / 120.0;
    Transform::from_translation(target + Quat::from_rotation_y(angle) * (position - target))
        .looking_at(target, Vec3::Y)
}

pub fn animate(transform: &mut Transform, bloom: bool, elapsed: f32) {
    if bloom {
        transform.translation.y =
            (transform.translation.x + transform.translation.z + elapsed).sin();
    } else {
        transform.rotation = Quat::from_rotation_y(elapsed / 2.0)
            * Quat::from_rotation_x(-std::f32::consts::FRAC_PI_4);
    }
}

fn uv_debug_texture() -> Image {
    let mut palette: [u8; 32] = [
        255, 102, 159, 255, 255, 159, 102, 255, 236, 255, 102, 255, 121, 255, 102, 255, 102, 255,
        198, 255, 102, 198, 255, 255, 121, 102, 255, 255, 236, 102, 255, 255,
    ];
    let mut data = [0; 8 * 8 * 4];
    for row in data.chunks_exact_mut(32) {
        row.copy_from_slice(&palette);
        palette.rotate_right(4);
    }
    let mut image = Image::new_fill(
        Extent3d {
            width: 8,
            height: 8,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = bevy::image::ImageSampler::nearest();
    image
}
