//! Per-output clear-then-suspend handshake using real renderer results.
use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use cosmix_shell_host::scene::{SceneControl, SceneFrameResults, SceneTick, SceneViews};

use crate::boids::{CaptureOnce, Flocks, bus::SceneGeometry};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Content {
    configuration: u64,
    seed: u64,
    count: usize,
}

struct Output {
    content: Content,
    suspended: bool,
    clear_attempt: bool,
}

#[derive(Resource, Default)]
pub(crate) struct CoveredOutputs(BTreeMap<Entity, Output>);

/// Runs after geometry and flock reconciliation, even on control-only wakes.
/// Coverage is geometric exclusion, not a claim about client pixel opacity.
pub(crate) fn reconcile(
    views: Res<SceneViews>,
    tick: Res<SceneTick>,
    geometry: Res<SceneGeometry>,
    flocks: Res<Flocks>,
    capture: Option<Res<CaptureOnce>>,
    mut covered: ResMut<CoveredOutputs>,
    mut control: ResMut<SceneControl>,
) {
    let capturing = capture.is_some_and(|capture| !capture.completed);
    let mut retained = BTreeSet::new();
    for (name, view) in &views.0 {
        let Some(flock) = flocks.0.get(name) else {
            continue;
        };
        if !geometry.0.as_ref().is_some_and(|geometry| {
            !geometry.locked && geometry.covers_output(name, view.logical_size, view.origin)
        }) {
            continue;
        }
        retained.insert(view.window);
        let content = Content {
            configuration: view.configuration,
            seed: flock.seed,
            count: flock.flock.birds().len(),
        };
        let output = covered.0.entry(view.window).or_insert(Output {
            content,
            suspended: false,
            clear_attempt: false,
        });
        if output.content != content || capturing {
            output.content = content;
            output.suspended = false;
        }
        output.clear_attempt = !capturing
            && !control.paused
            && !control.hidden
            && !output.suspended
            && tick.0.contains(name);
    }
    covered.0.retain(|window, _| retained.contains(window));
    control.suspended_outputs = covered
        .0
        .iter()
        .filter(|(_, output)| output.suspended)
        .map(|(window, _)| *window)
        .collect();
}

/// A previous successful frame, a control-only wake, or a failed acquisition
/// cannot acknowledge a new clear. Attempts are reset on every app update.
pub(crate) fn submitted(
    results: Res<SceneFrameResults>,
    mut covered: ResMut<CoveredOutputs>,
    mut control: ResMut<SceneControl>,
) {
    for (window, output) in &mut covered.0 {
        if output.clear_attempt && results.0.get(window) == Some(&true) {
            output.suspended = true;
            control.suspended_outputs.insert(*window);
        }
        output.clear_attempt = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boids::{
        OutputFlock, Paperbird, bus::SceneBus, geometry::Geometry, pointer::ScenePointer,
        preferences::PreferenceStore,
    };
    use cosmix_flock::{Flock, Rect, Settings};
    use cosmix_shell_host::scene::{SceneAfterRender, SceneView};

    fn app() -> (App, Entity, Entity) {
        let mut app = App::new();
        let mut views = SceneViews::default();
        let mut flocks = Flocks::default();
        let mut outputs = BTreeMap::new();
        for (index, name) in ["LEFT", "RIGHT"].into_iter().enumerate() {
            let window = app.world_mut().spawn_empty().id();
            let bird = app
                .world_mut()
                .spawn((
                    Paperbird {
                        output: window,
                        index: 0,
                    },
                    Transform::default(),
                    Visibility::Visible,
                ))
                .id();
            views.0.insert(
                name.into(),
                SceneView {
                    window,
                    camera: Entity::PLACEHOLDER,
                    logical_size: (200, 100),
                    origin: (index as i32 * 200, 0),
                    scale: 2.5,
                    render_layer: index,
                    configuration: 1,
                },
            );
            outputs.insert(
                name.into(),
                Rect::new(index as f32 * 200.0, 0.0, 200.0, 100.0).unwrap(),
            );
            flocks.0.insert(
                name.into(),
                OutputFlock {
                    seed: 42,
                    window,
                    logical: (200, 100),
                    flock: Flock::new(
                        200.0,
                        100.0,
                        42,
                        Settings {
                            count: 1,
                            ..Settings::default()
                        },
                    )
                    .unwrap(),
                    entities: vec![bird],
                    last_frame: None,
                },
            );
        }
        let left = views.0["LEFT"].window;
        let right = views.0["RIGHT"].window;
        let obstacle = outputs["LEFT"];
        app.insert_resource(views)
            .insert_resource(flocks)
            .insert_resource(SceneGeometry(Some(Geometry {
                instance: "test-comp".into(),
                sequence: 1,
                lost: 0,
                locked: false,
                outputs,
                obstacles: vec![obstacle],
                covered_outputs: BTreeSet::from(["LEFT".into()]),
            })))
            .insert_resource(PreferenceStore::load(None))
            .init_resource::<ScenePointer>()
            .init_resource::<SceneBus>()
            .init_resource::<SceneControl>()
            .init_resource::<SceneTick>()
            .init_resource::<SceneFrameResults>()
            .init_resource::<CoveredOutputs>()
            .add_systems(Update, (reconcile, crate::boids::animate).chain())
            .add_systems(SceneAfterRender, submitted);
        (app, left, right)
    }

    fn update(app: &mut App, names: &[&str], results: &[(Entity, bool)]) {
        app.world_mut().resource_mut::<SceneTick>().0 = names.iter().map(|n| (*n).into()).collect();
        app.update();
        app.world_mut().resource_mut::<SceneFrameResults>().0 = results.iter().copied().collect();
        app.world_mut().run_schedule(SceneAfterRender);
    }

    fn suspended(app: &App, window: Entity) -> bool {
        app.world()
            .resource::<SceneControl>()
            .suspended_outputs
            .contains(&window)
    }

    #[test]
    fn quiet_bird_visibility_stays_clean_and_uncovering_still_propagates() {
        #[derive(Resource, Default)]
        struct Changes(Vec<Entity>);
        fn observe(
            birds: Query<Entity, (With<Paperbird>, Changed<Visibility>)>,
            mut changes: ResMut<Changes>,
        ) {
            changes.0 = birds.iter().collect();
        }
        let (mut app, left, right) = app();
        app.init_resource::<Changes>()
            .add_systems(PostUpdate, observe);
        let left_bird = app.world().resource::<Flocks>().0["LEFT"].entities[0];
        update(&mut app, &["LEFT", "RIGHT"], &[(left, true), (right, true)]);
        assert_eq!(
            app.world().get::<Visibility>(left_bird),
            Some(&Visibility::Hidden)
        );
        // Both the covered control wake and an ordinary animation frame must
        // leave unchanged Visibility components clean for Bevy propagation.
        update(&mut app, &[], &[]);
        assert!(app.world().resource::<Changes>().0.is_empty());
        update(&mut app, &["RIGHT"], &[(right, true)]);
        assert!(app.world().resource::<Changes>().0.is_empty());
        {
            let mut geometry = app.world_mut().resource_mut::<SceneGeometry>();
            let geometry = geometry.0.as_mut().unwrap();
            geometry.obstacles.clear();
            geometry.covered_outputs.clear();
        }
        update(&mut app, &["LEFT"], &[(left, true)]);
        assert_eq!(
            app.world().get::<Visibility>(left_bird),
            Some(&Visibility::Visible)
        );
        assert_eq!(app.world().resource::<Changes>().0, vec![left_bird]);
        assert!(!suspended(&app, left));
    }

    #[test]
    fn control_wake_clears_birds_but_requires_this_updates_successful_frame() {
        let (mut app, left, right) = app();
        update(&mut app, &[], &[(left, true)]);
        assert!(
            !suspended(&app, left),
            "stale success is not a clear acknowledgement"
        );
        let bird = app.world().resource::<Flocks>().0["LEFT"].entities[0];
        assert_eq!(
            *app.world().get::<Visibility>(bird).unwrap(),
            Visibility::Hidden
        );
        assert_eq!(app.world().resource::<SceneBus>().simulation_steps, 0);
        update(&mut app, &["LEFT"], &[]);
        assert!(!suspended(&app, left));
        update(&mut app, &["LEFT"], &[(left, false)]);
        assert!(!suspended(&app, left));
        update(&mut app, &["LEFT", "RIGHT"], &[(left, true), (right, true)]);
        assert!(suspended(&app, left));
        assert!(!suspended(&app, right));
        update(&mut app, &[], &[]);
        assert!(
            suspended(&app, left),
            "unchanged coverage must remain settled"
        );
    }

    #[test]
    fn uncover_resumes_without_catching_up_and_configuration_requires_a_new_clear() {
        let (mut app, left, _) = app();
        update(&mut app, &["LEFT"], &[(left, true)]);
        assert!(suspended(&app, left));
        app.world_mut()
            .resource_mut::<SceneViews>()
            .0
            .get_mut("LEFT")
            .unwrap()
            .configuration += 1;
        update(&mut app, &[], &[(left, true)]);
        assert!(!suspended(&app, left));
        update(&mut app, &["LEFT"], &[(left, true)]);
        assert!(suspended(&app, left));
        let ticks = app.world().resource::<Flocks>().0["LEFT"].flock.ticks;
        {
            let mut geometry = app.world_mut().resource_mut::<SceneGeometry>();
            let geometry = geometry.0.as_mut().unwrap();
            geometry.covered_outputs.clear();
            geometry.obstacles.clear();
        }
        update(&mut app, &[], &[]);
        assert!(!suspended(&app, left));
        update(&mut app, &["LEFT"], &[(left, true)]);
        let flocks = app.world().resource::<Flocks>();
        assert_eq!(flocks.0["LEFT"].flock.ticks, ticks);
        assert!(flocks.0["LEFT"].flock.birds()[0].visible);
    }

    #[test]
    fn pause_capture_and_replacement_cannot_reuse_an_old_clear() {
        let (mut app, left, _) = app();
        app.world_mut().resource_mut::<SceneControl>().paused = true;
        update(&mut app, &["LEFT"], &[(left, true)]);
        assert!(!suspended(&app, left));
        app.world_mut().resource_mut::<SceneControl>().paused = false;
        update(&mut app, &["LEFT"], &[(left, true)]);
        assert!(suspended(&app, left));
        app.insert_resource(CaptureOnce {
            path: String::new(),
            frames: 0,
            requested: false,
            window: None,
            accepted: None,
            attempts: 0,
            completed: false,
        });
        update(&mut app, &["LEFT"], &[(left, true)]);
        assert!(
            !suspended(&app, left),
            "capture must be allowed to complete"
        );
        app.world_mut().resource_mut::<CaptureOnce>().completed = true;
        update(&mut app, &["LEFT"], &[(left, true)]);
        assert!(suspended(&app, left));
        app.world_mut()
            .resource_mut::<Flocks>()
            .0
            .get_mut("LEFT")
            .unwrap()
            .seed += 1;
        update(&mut app, &[], &[(left, true)]);
        assert!(!suspended(&app, left));
        update(&mut app, &["LEFT"], &[(left, true)]);
        let replacement = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<SceneViews>()
            .0
            .get_mut("LEFT")
            .unwrap()
            .window = replacement;
        app.world_mut()
            .resource_mut::<Flocks>()
            .0
            .get_mut("LEFT")
            .unwrap()
            .window = replacement;
        update(&mut app, &[], &[(left, true)]);
        assert!(!suspended(&app, left));
        assert!(!suspended(&app, replacement));
        assert!(
            !app.world()
                .resource::<CoveredOutputs>()
                .0
                .contains_key(&left)
        );
    }
}
