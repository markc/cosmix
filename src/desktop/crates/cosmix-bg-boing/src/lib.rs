//! Original CC0-generated Boing imagery; simulation uses Avian 0.7 rigid bodies.
//! Each output has a separate headless physics App. Only admitted time advances it.
//!
//! The caller owns the render camera, root hierarchy, admission clock and output
//! key. This scene has no window-system, shell-host or Bus dependency. Store
//! `Simulations` as a non-send resource; apply each returned visual transform to
//! `Simulation::visual`. Despawn the root and remove its simulation together.

use avian3d::prelude::*;
use bevy::{
    asset::RenderAssetUsages,
    camera::visibility::RenderLayers,
    core_pipeline::tonemapping::Tonemapping,
    ecs::schedule::{ScheduleLabel, SingleThreadedExecutor},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    time::TimeUpdateStrategy,
};
use std::{collections::BTreeMap, time::Duration};

const STEP: f32 = 1.0 / 120.0;
const RADIUS: f32 = 1.8;
const START: Vec3 = Vec3::new(-2.0, 4.8, 0.0);

fn initial_rotation() -> Quat {
    // Bevy's UV sphere has Z-aligned poles. Tilt the pole upright before spin.
    Quat::from_rotation_z(-0.3) * Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)
}

#[derive(Default)]
pub struct Simulations(pub BTreeMap<Entity, Simulation>);

pub struct Simulation {
    app: App,
    body: Entity,
    pub visual: Entity,
    remainder: f32,
    floor_impacts: u32,
    side_impacts: u32,
    kick_pending: bool,
    previous_pose: Transform,
    current_pose: Transform,
}

impl Simulation {
    fn new(visual: Entity) -> Self {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            PhysicsPlugins::new(PostUpdate),
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO))
        .insert_resource(Gravity(Vec3::new(0.0, -9.81, 0.0)))
        .insert_resource(SubstepCount(4));
        for (position, size) in [
            (Vec3::new(0.0, -0.25, 0.0), Vec3::new(11.0, 0.5, 6.0)),
            (Vec3::new(-5.25, 4.0, 0.0), Vec3::new(0.5, 8.0, 6.0)),
            (Vec3::new(5.25, 4.0, 0.0), Vec3::new(0.5, 8.0, 6.0)),
        ] {
            app.world_mut().spawn((
                RigidBody::Static,
                Position(position),
                Collider::cuboid(size.x, size.y, size.z),
                Restitution::new(1.0),
                Friction::ZERO,
            ));
        }
        let rotation = initial_rotation();
        let body = app
            .world_mut()
            .spawn((
                RigidBody::Dynamic,
                Collider::sphere(RADIUS),
                Position(START),
                Rotation(rotation),
                LinearVelocity(Vec3::new(3.0, 0.0, 0.0)),
                AngularVelocity(rotation * Vec3::Z * 1.5),
                Restitution::new(1.0),
                Friction::ZERO,
                LockedAxes::new().lock_translation_z(),
                SleepingDisabled,
            ))
            .id();
        app.finish();
        app.cleanup();
        // Four bodies do not earn thread-pool dispatch on every physics tick.
        for label in [
            First.intern(),
            PreUpdate.intern(),
            Update.intern(),
            PostUpdate.intern(),
            Last.intern(),
        ] {
            app.edit_schedule(label, |schedule| {
                schedule.set_executor(SingleThreadedExecutor::new());
            });
        }
        app.update();
        let pose = Transform::from_translation(app.world().get::<Position>(body).unwrap().0)
            .with_rotation(app.world().get::<Rotation>(body).unwrap().0);
        Self {
            app,
            body,
            visual,
            remainder: 0.0,
            floor_impacts: 0,
            side_impacts: 0,
            kick_pending: false,
            previous_pose: pose,
            current_pose: pose,
        }
    }

    pub fn advance(&mut self, admitted_seconds: f32) -> Transform {
        self.remainder += admitted_seconds.clamp(0.0, 0.05);
        while self.remainder >= STEP {
            self.previous_pose = self.current_pose;
            if self.kick_pending {
                self.apply_kick();
                self.kick_pending = false;
            }
            let before = self.app.world().get::<LinearVelocity>(self.body).unwrap().0;
            self.app
                .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
                    STEP,
                )));
            self.app.update();
            self.current_pose = self.pose();
            let after = self.app.world().get::<LinearVelocity>(self.body).unwrap().0;
            if before.y < -1.0 && after.y > 1.0 {
                self.floor_impacts += 1;
            }
            if before.x * after.x < 0.0 {
                self.side_impacts += 1;
            }
            self.remainder -= STEP;
        }
        // Rendering trails authoritative physics by one fixed tick. Fractional
        // admitted time blends adjacent completed poses; never extrapolate a
        // collision or write the visual pose back into Avian's body.
        interpolate_pose(self.previous_pose, self.current_pose, self.remainder / STEP)
    }

    /// Coalesce requests without advancing a paused output.
    pub fn kick(&mut self) {
        self.kick_pending = true;
    }

    fn apply_kick(&mut self) {
        let position = self.pose().translation;
        let world = self.app.world_mut();
        let velocity = world.get::<LinearVelocity>(self.body).unwrap().0;
        let mass = world.get::<ComputedMass>(self.body).unwrap().value();
        // Restore the launch height without letting repeated kicks pump the ball
        // out of the stage. Impulse changes momentum; never teleport the body.
        let target = Vec3::new(
            if position.x < 0.0 { 3.0 } else { -3.0 },
            (2.0 * 9.81 * (START.y - position.y).max(0.0)).sqrt(),
            0.0,
        );
        world
            .query::<Forces>()
            .get_mut(world, self.body)
            .unwrap()
            .apply_linear_impulse((target - velocity) * mass);
        info!("BG_BOING_KICK_APPLIED");
    }

    fn pose(&self) -> Transform {
        Transform::from_translation(self.app.world().get::<Position>(self.body).unwrap().0)
            .with_rotation(self.app.world().get::<Rotation>(self.body).unwrap().0)
    }
}

fn interpolate_pose(previous: Transform, current: Transform, fraction: f32) -> Transform {
    let fraction = fraction.clamp(0.0, 1.0);
    Transform {
        translation: previous.translation.lerp(current.translation, fraction),
        rotation: previous.rotation.slerp(current.rotation, fraction),
        scale: previous.scale.lerp(current.scale, fraction),
    }
}

impl Drop for Simulation {
    fn drop(&mut self) {
        info!(
            floor_impacts = self.floor_impacts,
            side_impacts = self.side_impacts,
            "BG_BOING_PHYSICS_FINISHED"
        );
    }
}

pub fn camera_pose(elapsed: f32) -> Transform {
    let target = Vec3::new(0.0, 3.0, 0.0);
    // Keep the wall behind the ball: a broad arc, not a trip behind the stage.
    let angle = 0.35 * (elapsed * std::f32::consts::TAU / 120.0).sin();
    Transform::from_translation(target + Quat::from_rotation_y(angle) * Vec3::new(0.0, 1.2, 12.8))
        .looking_at(target, Vec3::Y)
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
    simulations: &mut Simulations,
) {
    commands
        .entity(camera)
        .insert((camera_pose(0.0), Tonemapping::TonyMcMapface));
    commands.queue(move |world: &mut World| {
        if let Some(mut camera) = world.get_mut::<Camera>(camera) {
            camera.clear_color = ClearColorConfig::Custom(Color::srgb(0.40, 0.40, 0.43));
        }
    });
    let checker = materials.add(StandardMaterial {
        base_color_texture: Some(images.add(checker_texture())),
        perceptual_roughness: 0.48,
        ..default()
    });
    let ball = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(RADIUS).mesh().uv(96, 64))),
            MeshMaterial3d(checker),
            Transform::from_translation(START).with_rotation(initial_rotation()),
            layer.clone(),
        ))
        .id();
    commands.entity(root).add_child(ball);
    let grey = materials.add(StandardMaterial {
        base_color: Color::srgb(0.55, 0.55, 0.57),
        perceptual_roughness: 1.0,
        ..default()
    });
    let purple = materials.add(StandardMaterial {
        base_color: Color::srgb(0.32, 0.08, 0.38),
        perceptual_roughness: 1.0,
        ..default()
    });
    let mut stage: Vec<(Handle<StandardMaterial>, Mesh)> = Vec::new();
    let mut cuboid = |size: Vec3, position: Vec3, material: Handle<StandardMaterial>| {
        let mesh = Mesh::from(Cuboid::from_size(size))
            .transformed_by(Transform::from_translation(position));
        if let Some((_, combined)) = stage.iter_mut().find(|(handle, _)| *handle == material) {
            combined
                .merge(&mesh)
                .expect("cuboids share vertex attributes");
        } else {
            stage.push((material, mesh));
        }
    };
    cuboid(
        Vec3::new(64.0, 32.0, 0.2),
        Vec3::new(0.0, 16.0, -2.1),
        grey.clone(),
    );
    cuboid(
        Vec3::new(64.0, 0.25, 34.0),
        Vec3::new(0.0, -0.125, 15.0),
        grey.clone(),
    );
    // Extend the visual grid beyond the camera frustum; the simulation's
    // play area stays unchanged. Both surfaces remain one mesh per material.
    for x in -64..=64 {
        cuboid(
            Vec3::new(0.025, 32.0, 0.02),
            Vec3::new(x as f32 * 0.5, 16.0, -1.985),
            purple.clone(),
        );
        cuboid(
            Vec3::new(0.025, 0.012, 34.0),
            Vec3::new(x as f32 * 0.5, 0.008, 15.0),
            purple.clone(),
        );
    }
    for y in 0..=64 {
        cuboid(
            Vec3::new(64.0, 0.025, 0.02),
            Vec3::new(0.0, y as f32 * 0.5, -1.985),
            purple.clone(),
        );
    }
    for z in -4..=64 {
        cuboid(
            Vec3::new(64.0, 0.012, 0.025),
            Vec3::new(0.0, 0.008, z as f32 * 0.5),
            purple.clone(),
        );
    }
    for (material, mesh) in stage {
        let entity = commands
            .spawn((
                Mesh3d(meshes.add(mesh)),
                MeshMaterial3d(material),
                Transform::default(),
                layer.clone(),
            ))
            .id();
        commands.entity(root).add_child(entity);
    }
    let light = commands
        .spawn((
            PointLight {
                intensity: 3_000_000.0,
                range: 40.0,
                shadow_maps_enabled: true,
                radius: 0.4,
                ..default()
            },
            Transform::from_xyz(-3.5, 7.0, 6.0),
            layer,
        ))
        .id();
    commands.entity(root).add_child(light);
    simulations.0.insert(window, Simulation::new(ball));
    info!(
        ?window,
        "BG_BOING_PHYSICS_READY avian=0.7.0 timestep=1/120 isolation=per-output"
    );
}

fn checker_texture() -> Image {
    const WIDTH: usize = 320;
    const HEIGHT: usize = 192;
    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            pixels.extend_from_slice(if ((x / 32) + (y / 32)) % 2 == 0 {
                &[235, 12, 18, 255]
            } else {
                &[255, 255, 255, 255]
            });
        }
    }
    Image::new_fill(
        Extent3d {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &pixels,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kick_waits_for_admission_and_repeated_kicks_stay_in_stage() {
        let mut simulation = Simulation::new(Entity::PLACEHOLDER);
        for _ in 0..40 {
            simulation.advance(1.0 / 60.0);
        }
        let before = simulation.advance(0.0);
        let physics_before = simulation.pose();
        simulation.kick();
        assert_eq!(simulation.advance(0.0), before);
        assert_eq!(simulation.pose(), physics_before);
        simulation.advance(1.0 / 60.0);
        assert!(
            simulation
                .app
                .world()
                .get::<LinearVelocity>(simulation.body)
                .unwrap()
                .y
                > 0.0
        );
        for frame in 0..1200 {
            if frame % 15 == 0 {
                simulation.kick();
            }
            let position = simulation.advance(1.0 / 60.0).translation;
            assert!((RADIUS - 0.15..5.5).contains(&position.y));
            assert!(position.x.abs() < 5.0 - RADIUS + 0.15);
        }
    }

    #[test]
    fn half_step_interpolates_translation_and_rotation_without_advancing_physics() {
        let mut simulation = Simulation::new(Entity::PLACEHOLDER);
        simulation.advance(STEP);
        let previous = simulation.previous_pose;
        let current = simulation.current_pose;
        let physics = simulation.pose();
        let half = simulation.advance(STEP * 0.5);
        assert!(
            half.translation
                .abs_diff_eq(previous.translation.lerp(current.translation, 0.5), 1e-6)
        );
        assert!(
            half.rotation
                .abs_diff_eq(previous.rotation.slerp(current.rotation, 0.5), 1e-6)
        );
        assert_ne!(
            previous.rotation, current.rotation,
            "real angular velocity must advance rotation"
        );
        assert_eq!(
            simulation.pose(),
            physics,
            "interpolation cannot move the physics body"
        );
        simulation.kick();
        assert_eq!(
            simulation.advance(0.0),
            half,
            "pause preserves fractional visual phase"
        );
        assert!(simulation.kick_pending, "paused kick remains queued");
        simulation.advance(STEP * 0.5);
        assert!(
            !simulation.kick_pending,
            "kick applies only at the next physics step"
        );
    }

    #[test]
    fn real_physics_repeated_impacts_are_bounded_and_outputs_are_isolated() {
        let mut moving = Simulation::new(Entity::PLACEHOLDER);
        let mut paused = Simulation::new(Entity::PLACEHOLDER);
        let initial = paused.pose();
        moving.advance(0.05);
        assert!(
            moving.pose().translation.y < START.y,
            "gravity must pull down"
        );
        for _ in 0..2400 {
            let pose = moving.advance(1.0 / 60.0);
            assert!(pose.translation.is_finite());
            assert!(
                pose.translation.x.abs() < 5.0 - RADIUS + 0.15,
                "escaped side colliders"
            );
            assert!(
                (RADIUS - 0.15..5.5).contains(&pose.translation.y),
                "escaped floor/energy bound"
            );
            assert!(pose.translation.z.abs() < 0.001);
            paused.advance(0.0);
        }
        assert!(
            moving.floor_impacts > 5,
            "expected repeated floor restitution"
        );
        assert!(
            moving.side_impacts > 5,
            "expected repeated side restitution"
        );
        assert_eq!(paused.pose(), initial, "other output must remain paused");
        paused.advance(1.0 / 60.0);
        assert_ne!(paused.pose(), initial, "resume advances only admitted time");
    }
}
