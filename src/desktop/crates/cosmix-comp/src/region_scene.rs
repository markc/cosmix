//! Native selection furniture. The protocol thread owns input and completion;
//! this bridge carries only a coalesced view and exact clean-frame evidence.
use crate::{
    compositor_scene::{CompositorSceneSet, SceneContentRevision},
    occlusion::OutputGeometry,
};
use bevy::{
    prelude::*,
    render::extract_resource::{ExtractResource, ExtractResourcePlugin},
};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct View {
    pub id: u64,
    pub active: bool,
    pub outputs: Vec<OutputGeometry>,
    pub selected: Option<String>,
    pub start: Option<(f64, f64)>,
    pub pointer: (f64, f64),
}

#[derive(Default)]
struct State {
    view: View,
    revision: u64,
    clean_revision: Option<u64>,
    clean_outputs: Vec<String>,
}

#[derive(Resource, Clone, Default, ExtractResource)]
pub(crate) struct RegionBridge(Arc<Mutex<State>>);

impl RegionBridge {
    pub(crate) fn set(&self, view: View) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.view = view;
        state.revision += 1;
        state.clean_revision = None;
        state.clean_outputs.clear();
    }
    #[cfg(feature = "embedded-quoin")]
    pub(crate) fn active(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).view.active
    }
    pub(crate) fn clean(&self, id: u64) -> bool {
        let s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        s.view.id == id
            && !s.view.active
            && s.clean_revision.is_some()
            && s.view
                .outputs
                .iter()
                .all(|o| s.clean_outputs.contains(&o.name))
    }
    pub(crate) fn presented(&self, output: &str, generation: u64, revision: Option<u64>) {
        let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !s.view.active
            && s.clean_revision
                .zip(revision)
                .is_some_and(|(min, got)| got >= min)
            && s.view
                .outputs
                .iter()
                .any(|o| o.name == output && o.generation == generation)
            && !s.clean_outputs.iter().any(|o| o == output)
        {
            s.clean_outputs.push(output.to_owned());
        }
    }
}

#[cfg(test)]
pub(crate) fn test_remove_frame(bridge: &RegionBridge) -> u64 {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(bridge.clone())
        .init_resource::<SceneContentRevision>()
        .add_systems(First, draw);
    app.update();
    app.world().resource::<SceneContentRevision>().0.unwrap()
}

pub(crate) fn install(app: &mut App) {
    app.init_resource::<RegionBridge>()
        .add_plugins(ExtractResourcePlugin::<RegionBridge>::default())
        .add_systems(Startup, attach)
        .add_systems(First, draw.after(CompositorSceneSet));
}

fn attach(feed: Res<crate::protocol::ClientSceneFeed>, bridge: Res<RegionBridge>) {
    feed.install_region_bridge(bridge.clone());
}

#[derive(Component)]
struct RegionOverlay;

fn draw(
    mut commands: Commands,
    bridge: Res<RegionBridge>,
    cameras: Query<(Entity, &crate::capture::CaptureOutputSource)>,
    overlays: Query<Entity, With<RegionOverlay>>,
    mut seen: Local<u64>,
    mut revision: ResMut<SceneContentRevision>,
    damage: Option<Res<crate::capture::OutputDamageJournal>>,
) {
    let mut s = bridge.0.lock().unwrap_or_else(|p| p.into_inner());
    if *seen == s.revision {
        return;
    }
    if s.view.active
        && s.view
            .outputs
            .iter()
            .any(|o| !cameras.iter().any(|(_, c)| c.source_id == o.source_id))
    {
        // Output-camera installation can lag the protocol's ready snapshot.
        // Retry next update rather than losing the first overlay projection.
        return;
    }
    *seen = s.revision;
    for entity in &overlays {
        commands.entity(entity).despawn();
    }
    revision.advance();
    if let Some(damage) = damage {
        damage.mark_all_base_full();
    }
    if !s.view.active {
        // First's deferred commands apply before extraction. Only a submission
        // carrying this revision (never an older in-flight frame) can satisfy it.
        s.clean_revision = revision.0;
        return;
    }
    let v = &s.view;
    for output in &v.outputs {
        let Some((camera, _)) = cameras
            .iter()
            .find(|(_, c)| c.source_id == output.source_id)
        else {
            continue;
        };
        let b = output.bounds;
        let p = (
            (v.pointer.0 - b.x).clamp(0.0, b.w) as f32,
            (v.pointer.1 - b.y).clamp(0.0, b.h) as f32,
        );
        let root = commands
            .spawn((
                RegionOverlay,
                UiTargetCamera(camera),
                GlobalZIndex(1000),
                Pickable::IGNORE,
                Node {
                    position_type: PositionType::Absolute,
                    width: px(b.w as f32),
                    height: px(b.h as f32),
                    ..default()
                },
            ))
            .id();
        let mut quad = |x, y, w, h, colour| {
            commands.spawn((
                ChildOf(root),
                Pickable::IGNORE,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(x),
                    top: px(y),
                    width: px(w),
                    height: px(h),
                    ..default()
                },
                BackgroundColor(colour),
            ));
        };
        let dim = Color::srgba(0.0, 0.0, 0.0, 0.35);
        if let Some(start) = v
            .start
            .filter(|_| v.selected.as_deref() == Some(&output.name))
        {
            let a = ((start.0 - b.x) as f32, (start.1 - b.y) as f32);
            let (l, t, r, bottom) = (a.0.min(p.0), a.1.min(p.1), a.0.max(p.0), a.1.max(p.1));
            quad(0., 0., b.w as f32, t, dim);
            quad(0., bottom, b.w as f32, b.h as f32 - bottom, dim);
            quad(0., t, l, bottom - t, dim);
            quad(r, t, b.w as f32 - r, bottom - t, dim);
            for (x, y, w, h) in [
                (l, t, r - l, 1.),
                (l, bottom, r - l, 1.),
                (l, t, 1., bottom - t),
                (r, t, 1., bottom - t),
            ] {
                quad(x, y, w, h, Color::WHITE);
            }
        } else {
            quad(0., 0., b.w as f32, b.h as f32, dim);
        }
        if v.pointer.0 >= b.x
            && v.pointer.0 < b.x + b.w
            && v.pointer.1 >= b.y
            && v.pointer.1 < b.y + b.h
        {
            quad(p.0 - 8., p.1, 17., 1., Color::WHITE);
            quad(p.0, p.1 - 8., 1., 17., Color::WHITE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn region_overlay_targets_output_above_quoin_and_removal_needs_matching_frame() {
        let bridge = RegionBridge::default();
        let source = crate::backend::CaptureSourceId::Nested {
            output_name: "Output-1".into(),
        };
        let output = OutputGeometry {
            name: "Output-1".into(),
            source_id: source.clone(),
            bounds: crate::occlusion::Bounds::new(0., 0., 320., 240.),
            scale: 1.,
            scale_y: 1.,
            generation: 1,
            transform: smithay::utils::Transform::Normal,
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(bridge.clone())
            .init_resource::<SceneContentRevision>()
            .add_systems(First, draw);
        let camera = app
            .world_mut()
            .spawn(crate::capture::CaptureOutputSource {
                source_id: source,
                output_name: output.name.clone(),
            })
            .id();
        let mut view = View {
            id: 7,
            active: true,
            outputs: vec![output],
            selected: Some("Output-1".into()),
            start: Some((30., 40.)),
            pointer: (100., 120.),
        };
        bridge.set(view.clone());
        app.update();
        let (target, z) = app
            .world_mut()
            .query_filtered::<(&UiTargetCamera, &GlobalZIndex), With<RegionOverlay>>()
            .single(app.world())
            .unwrap();
        assert_eq!(target.0, camera);
        assert!(z.0 > 140);
        view.active = false;
        bridge.set(view);
        app.update();
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<RegionOverlay>>()
                .iter(app.world())
                .count(),
            0
        );
        let revision = app.world().resource::<SceneContentRevision>().0;
        assert!(!bridge.clean(7));
        bridge.presented("Output-1", 2, revision);
        assert!(
            !bridge.clean(7),
            "another output generation is not evidence"
        );
        bridge.presented("Output-1", 1, revision);
        assert!(bridge.clean(7));
        assert!(!bridge.clean(8), "another operation is not evidence");
    }
}
