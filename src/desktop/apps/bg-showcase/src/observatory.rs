//! Authored backgrounds share one explicit metadata contract, not a general ECS importer.
use bevy::{
    asset::{DependencyLoadState, LoadState, RecursiveDependencyLoadState},
    camera::visibility::RenderLayers,
    gltf::{GltfExtras, GltfLoaderSettings},
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use serde::Deserialize;

pub fn configure(app: &mut App) {
    app.add_systems(Startup, load);
}

#[derive(Resource)]
pub struct ObservatoryAsset(pub Handle<WorldAsset>);

fn load(mut commands: Commands, server: Res<AssetServer>, options: Res<super::Options>) {
    let path = match options.scene {
        super::Scene::Celestial => "media://celestial-engine.glb",
        _ => "media://kinetic-observatory.glb",
    };
    commands.insert_resource(ObservatoryAsset(
        server
            .load_builder()
            .with_settings(|settings: &mut GltfLoaderSettings| {
                settings.load_cameras = false;
                settings.load_lights = false;
                settings.load_animations = false;
            })
            .load(GltfAssetLabel::Scene(0).from_asset(path)),
    ));
}

#[derive(Component)]
pub struct AuthoredInstance {
    pub window: Entity,
    pub layer: RenderLayers,
    pub ready: bool,
}

#[derive(Component)]
pub struct Spin {
    pub window: Entity,
    pub axis: Vec3,
    pub speed: f32,
    pub base: Quat,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    axis: String,
    radians_per_second: f32,
}

fn parse(value: &str) -> Result<Option<(Vec3, f32)>, String> {
    let extras: serde_json::Value = serde_json::from_str(value).map_err(|e| e.to_string())?;
    let Some(metadata) = extras.get("cosmix_bg") else {
        return Ok(None);
    };
    let metadata: Metadata = serde_json::from_value(metadata.clone()).map_err(|e| e.to_string())?;
    if metadata.version != 1 {
        return Err("unsupported cosmix_bg metadata version".into());
    }
    let axis = match metadata.axis.as_str() {
        "x" => Vec3::X,
        "y" => Vec3::Y,
        "z" => Vec3::Z,
        _ => return Err("cosmix_bg axis must be x, y or z in glTF local coordinates".into()),
    };
    let speed = metadata.radians_per_second;
    if !speed.is_finite() || speed.abs() > 1.0 {
        return Err("cosmix_bg radians_per_second must be finite and within -1..1".into());
    }
    Ok(Some((axis, speed)))
}

pub fn ready(
    event: On<WorldInstanceReady>,
    mut commands: Commands,
    children: Query<&Children>,
    mut instances: Query<&mut AuthoredInstance>,
    extras: Query<(&GltfExtras, &Transform)>,
    meshes: Query<&Mesh3d>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(mut instance) = instances.get_mut(event.entity) else {
        return;
    };
    let mut mesh_count = 0;
    let mut spin_count = 0;
    for entity in children.iter_descendants(event.entity) {
        commands.entity(entity).insert(instance.layer.clone());
        mesh_count += usize::from(meshes.contains(entity));
        if let Ok((extras, transform)) = extras.get(entity) {
            match parse(&extras.value) {
                Ok(Some((axis, speed))) => {
                    commands.entity(entity).insert(Spin {
                        window: instance.window,
                        axis,
                        speed,
                        base: transform.rotation,
                    });
                    spin_count += 1;
                }
                Ok(None) => {}
                Err(error) => {
                    error!(%error, "BG_OBSERVATORY_METADATA_FAILED");
                    exit.write(AppExit::error());
                    return;
                }
            }
        }
    }
    if mesh_count == 0 || spin_count == 0 {
        error!(mesh_count, spin_count, "BG_OBSERVATORY_EMPTY");
        exit.write(AppExit::error());
        return;
    }
    instance.ready = true;
    info!(mesh_count, spin_count, "BG_OBSERVATORY_READY");
}

impl ObservatoryAsset {
    pub fn loaded(&self, server: &AssetServer) -> bool {
        server.is_loaded_with_dependencies(&self.0)
    }

    pub fn failed(&self, server: &AssetServer) -> bool {
        server
            .get_load_states(&self.0)
            .is_some_and(|(root, direct, recursive)| {
                matches!(root, LoadState::Failed(_))
                    || matches!(direct, DependencyLoadState::Failed(_))
                    || matches!(recursive, RecursiveDependencyLoadState::Failed(_))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_bounded_version_one_spin_is_accepted() {
        assert_eq!(parse(r#"{"other":42}"#).unwrap(), None);
        assert_eq!(
            parse(r#"{"cosmix_bg":{"version":1,"axis":"y","radians_per_second":-0.2}}"#).unwrap(),
            Some((Vec3::Y, -0.2))
        );
        for value in [
            r#"{"cosmix_bg":{"version":2,"axis":"y","radians_per_second":0.2}}"#,
            r#"{"cosmix_bg":{"version":1,"axis":"q","radians_per_second":0.2}}"#,
            r#"{"cosmix_bg":{"version":1,"axis":"y","radians_per_second":1.01}}"#,
            r#"{"cosmix_bg":{"version":1,"axis":"y","radians_per_second":0.2,"script":"anything"}}"#,
            r#"{"cosmix_bg":null}"#,
        ] {
            assert!(parse(value).is_err(), "{value}");
        }
    }
}
