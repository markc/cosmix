//! Main-world asset wakeups, including work that arrives without protocol damage.
//! GPU preparation, pipeline settling and component changes are separate evidence.

use bevy::{
    app::MainScheduleOrder,
    asset::{Asset, AssetEvent},
    ecs::schedule::ScheduleLabel,
    prelude::*,
    shader::Shader,
    sprite_render::SpriteMaterial,
};

use crate::compositor_scene::SceneContentRevision;

/// Run after all standard Main schedules, including late typography/asset edits.
/// Future schedules after this one must either precede it or publish their own
/// demand before an idle decision is made.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RenderDemandObservation;

pub(crate) fn configure(app: &mut App) {
    app.init_schedule(RenderDemandObservation);
    // These small observers all share one revision writer. Running them on
    // one thread avoids worker wakeups for work that cannot run concurrently.
    app.edit_schedule(RenderDemandObservation, |schedule| {
        schedule.set_executor(bevy::ecs::schedule::SingleThreadedExecutor::new());
    });
    app.world_mut()
        .resource_mut::<MainScheduleOrder>()
        .insert_after(Last, RenderDemandObservation);
    install::<Image>(app);
    install::<Mesh>(app);
    install::<SpriteMaterial>(app);
    install::<ColorMaterial>(app);
    install::<crate::client_surface_material::ClientSurfaceMaterial>(app);
    install::<crate::chrome_frame_material::ChromeFrameMaterial>(app);
    install::<crate::shadow_material::ShadowMaterial>(app);
    install::<Shader>(app);
    install::<Font>(app);
    install::<TextureAtlasLayout>(app);
}

fn install<A: Asset>(app: &mut App) {
    // Headless/test configurations can omit entire asset families. Do not
    // initialise a competing asset store on behalf of an absent owning plugin.
    if app.world().contains_resource::<Assets<A>>() {
        app.add_systems(RenderDemandObservation, observe_resource::<A>);
    }
    if app.world().contains_resource::<Messages<AssetEvent<A>>>() {
        app.add_systems(RenderDemandObservation, observe_events::<A>);
    }
}

fn observe_resource<A: Asset>(assets: Res<Assets<A>>, mut revision: ResMut<SceneContentRevision>) {
    if assets.is_changed() {
        revision.advance();
    }
}

fn observe_events<A: Asset>(
    mut events: MessageReader<AssetEvent<A>>,
    mut revision: ResMut<SceneContentRevision>,
) {
    // Consume only our reader's cursor, never the renderer's messages. Do this
    // even when resource change detection already says dirty. A Last edit can
    // precede its published AssetEvent by one Main turn: both must wake so that
    // the renderer eventually extracts the event-driven change as well.
    let has_events = !events.is_empty();
    events.clear();
    if has_events {
        revision.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::{AssetApp, AssetPlugin};
    use bevy::ecs::message::MessageCursor;

    #[derive(Asset, TypePath)]
    struct TestAsset(u32);

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<TestAsset>()
            .init_resource::<SceneContentRevision>();
        configure(&mut app);
        install::<TestAsset>(&mut app);
        app
    }

    fn revision(app: &App) -> u64 {
        app.world().resource::<SceneContentRevision>().0.unwrap()
    }

    #[test]
    fn quiet_main_turns_do_not_wake_and_asset_events_keep_independent_readers() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<TestAsset>>()
            .add(TestAsset(1));
        let mut reader = MessageCursor::<AssetEvent<TestAsset>>::default();
        app.update();
        let settled = revision(&app);
        assert!(
            reader
                .read(app.world().resource::<Messages<AssetEvent<TestAsset>>>())
                .any(|event| matches!(event, AssetEvent::Added { id } if *id == handle.id()))
        );
        for _ in 0..8 {
            assert_eq!(
                app.world()
                    .resource::<Assets<TestAsset>>()
                    .get(&handle)
                    .unwrap()
                    .0,
                1
            );
            app.update();
            assert_eq!(revision(&app), settled);
        }
        app.world_mut()
            .resource_mut::<Assets<TestAsset>>()
            .get_mut(&handle)
            .unwrap()
            .0 = 2;
        app.update();
        assert!(revision(&app) > settled);
        assert!(
            reader
                .read(app.world().resource::<Messages<AssetEvent<TestAsset>>>())
                .any(|event| matches!(event, AssetEvent::Modified { id } if *id == handle.id()))
        );
        let changed = revision(&app);
        app.update();
        assert_eq!(revision(&app), changed);
        app.world_mut()
            .resource_mut::<Assets<TestAsset>>()
            .remove(&handle);
        app.update();
        assert!(revision(&app) > changed);
    }

    #[derive(Resource)]
    struct LateEdit(Option<Handle<TestAsset>>);

    fn edit_in_last(mut edit: ResMut<LateEdit>, mut assets: ResMut<Assets<TestAsset>>) {
        if let Some(handle) = edit.0.take() {
            assets.get_mut(&handle).unwrap().0 += 1;
        }
    }

    #[test]
    fn late_edit_and_its_delayed_event_each_wake_without_render_updates() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<TestAsset>>()
            .add(TestAsset(0));
        app.insert_resource(LateEdit(None))
            .add_systems(Last, edit_in_last);
        app.update();
        app.update();
        let baseline = revision(&app);
        app.world_mut().resource_mut::<LateEdit>().0 = Some(handle.clone());
        app.main_mut().run_default_schedule();
        let direct_edit = revision(&app);
        assert!(direct_edit > baseline);
        app.world_mut().clear_trackers();
        // No extraction between these Main turns. Publication still creates a
        // newer sticky demand, which cannot be acknowledged by the earlier draw.
        // This checks the watermark only: a future idle policy MUST extract on
        // each event-bearing turn, before Bevy expires its extraction messages.
        app.main_mut().run_default_schedule();
        let published = revision(&app);
        assert!(published > direct_edit);
        app.world_mut().clear_trackers();
        for _ in 0..8 {
            app.main_mut().run_default_schedule();
            app.world_mut().clear_trackers();
            assert_eq!(revision(&app), published);
        }
    }

    #[test]
    fn independently_published_events_wake_even_without_asset_resource_changes() {
        let mut app = app();
        app.update();
        app.update();
        let baseline = revision(&app);
        app.world_mut()
            .write_message(AssetEvent::<TestAsset>::LoadedWithDependencies {
                id: bevy::asset::AssetId::default(),
            });
        app.update();
        assert!(revision(&app) > baseline);
        let published = revision(&app);
        app.update();
        assert_eq!(revision(&app), published);
    }

    #[test]
    fn bare_asset_storage_is_observed_without_registering_or_replacing_it() {
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .init_resource::<SceneContentRevision>();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::default());
        configure(&mut app);
        assert!(
            !app.world()
                .contains_resource::<Messages<AssetEvent<Image>>>()
        );
        assert!(app.world().resource::<Assets<Image>>().contains(&handle));
        app.update();
        let initial = revision(&app);
        app.update();
        assert_eq!(revision(&app), initial);
        app.world_mut()
            .resource_mut::<Assets<Image>>()
            .remove(&handle);
        app.update();
        assert!(revision(&app) > initial);
        app.world_mut().resource_mut::<SceneContentRevision>().0 = Some(u64::MAX);
        app.world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::default());
        app.update();
        assert_eq!(app.world().resource::<SceneContentRevision>().0, None);
        app.update();
        assert_eq!(app.world().resource::<SceneContentRevision>().0, None);
    }

    #[test]
    fn last_handle_drop_wakes_and_preserves_unused_event_for_extraction() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<TestAsset>>()
            .add(TestAsset(0));
        let id = handle.id();
        app.update();
        app.update();
        let settled = revision(&app);
        let mut reader = app
            .world()
            .resource::<Messages<AssetEvent<TestAsset>>>()
            .get_cursor_current();
        drop(handle);
        app.update();
        assert!(revision(&app) > settled);
        assert!(!app.world().resource::<Assets<TestAsset>>().contains(id));
        assert!(
            reader
                .read(app.world().resource::<Messages<AssetEvent<TestAsset>>>())
                .any(|event| matches!(event, AssetEvent::Unused { id: unused } if *unused == id))
        );
        let removed = revision(&app);
        app.update();
        assert_eq!(revision(&app), removed);
    }
}
