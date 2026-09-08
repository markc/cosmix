//! Non-consuming observation of the compositor's GPU asset preparation.
//! Pipeline compilation, DMA-BUF ownership and presentation remain separate.

use std::collections::{BTreeMap, HashSet};

use bevy::{
    asset::AssetId,
    prelude::*,
    render::{
        ExtractSchedule, Render, RenderApp, RenderSystems,
        mesh::RenderMesh,
        render_asset::{AssetExtractionSystems, ExtractedAssets, RenderAsset, RenderAssets},
        texture::GpuImage,
    },
    sprite_render::{PreparedMaterial2d, SpriteMaterial},
};

/// Snapshot after preparation, not a certificate that a complete scene is ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AssetPreparationSnapshot {
    pub(crate) revision: Option<u64>,
    pub(crate) pending_preparations: usize,
    pub(crate) pending_removals: usize,
    pub(crate) tracked_types: usize,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    preparations: usize,
    removals: usize,
}

#[derive(Resource)]
pub(crate) struct AssetPreparationStatus {
    revision: Option<u64>,
    types: BTreeMap<&'static str, Counts>,
}

impl Default for AssetPreparationStatus {
    fn default() -> Self {
        Self {
            revision: Some(0),
            types: BTreeMap::new(),
        }
    }
}

impl AssetPreparationStatus {
    pub(crate) fn snapshot(&self) -> AssetPreparationSnapshot {
        AssetPreparationSnapshot {
            revision: self.revision,
            pending_preparations: self.types.values().map(|counts| counts.preparations).sum(),
            pending_removals: self.types.values().map(|counts| counts.removals).sum(),
            tracked_types: self.types.len(),
        }
    }
}

#[derive(Resource)]
struct PendingAssets<A: RenderAsset> {
    preparations: HashSet<AssetId<A::SourceAsset>>,
    removals: HashSet<AssetId<A::SourceAsset>>,
}

impl<A: RenderAsset> Default for PendingAssets<A> {
    fn default() -> Self {
        Self {
            preparations: HashSet::new(),
            removals: HashSet::new(),
        }
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AssetReadinessSet;

/// Install after the owning material/render plugins have built their resources.
/// Disabled material plugins do not have a preparation queue to observe.
pub(crate) fn configure(app: &mut App) {
    let Some(render) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render.init_resource::<AssetPreparationStatus>();
    install::<GpuImage>(render);
    install::<RenderMesh>(render);
    install::<PreparedMaterial2d<SpriteMaterial>>(render);
    install::<PreparedMaterial2d<ColorMaterial>>(render);
    install::<PreparedMaterial2d<crate::client_surface_material::ClientSurfaceMaterial>>(render);
    install::<PreparedMaterial2d<crate::chrome_frame_material::ChromeFrameMaterial>>(render);
    install::<PreparedMaterial2d<crate::shadow_material::ShadowMaterial>>(render);
}

fn install<A: RenderAsset>(render: &mut bevy::app::SubApp) {
    if !render.world().contains_resource::<ExtractedAssets<A>>()
        || !render.world().contains_resource::<RenderAssets<A>>()
    {
        return;
    }
    render
        .world_mut()
        .resource_mut::<AssetPreparationStatus>()
        .types
        .insert(std::any::type_name::<A>(), Counts::default());
    render
        .init_resource::<PendingAssets<A>>()
        .add_systems(
            ExtractSchedule,
            observe_extracted::<A>.after(AssetExtractionSystems),
        )
        .add_systems(
            Render,
            reconcile_prepared::<A>
                .in_set(AssetReadinessSet)
                .after(RenderSystems::PrepareAssets)
                .before(RenderSystems::Prepare),
        );
}

fn observe_extracted<A: RenderAsset>(
    extracted: Res<ExtractedAssets<A>>,
    mut pending: ResMut<PendingAssets<A>>,
    mut status: ResMut<AssetPreparationStatus>,
) {
    if !extracted.extracted.is_empty() || !extracted.removed.is_empty() {
        status.revision = status.revision.and_then(|revision| revision.checked_add(1));
    }
    for id in &extracted.removed {
        pending.preparations.remove(id);
        pending.removals.insert(*id);
    }
    // Preparation processes removals before newly extracted values. A new
    // extraction of the same ID therefore supersedes a pending removal.
    for (id, _) in &extracted.extracted {
        pending.removals.remove(id);
        pending.preparations.insert(*id);
    }
}

fn reconcile_prepared<A: RenderAsset>(
    prepared: Res<RenderAssets<A>>,
    mut pending: ResMut<PendingAssets<A>>,
    mut status: ResMut<AssetPreparationStatus>,
) {
    // Bevy removes the previous GPU value before attempting its replacement.
    // RetryNextUpdate leaves it absent until preparation actually succeeds.
    pending
        .preparations
        .retain(|id| prepared.get(*id).is_none());
    pending.removals.retain(|id| prepared.get(*id).is_some());
    status.types.insert(
        std::any::type_name::<A>(),
        Counts {
            preparations: pending.preparations.len(),
            removals: pending.removals.len(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        ecs::system::{RunSystemOnce, SystemParamItem},
        render::render_asset::{
            PrepareAssetError, PrepareNextFrameAssets, RenderAssetBytesPerFrameLimiter,
            prepare_assets,
        },
    };

    #[derive(Asset, TypePath, Clone)]
    struct TestAsset {
        retries: usize,
        value: u32,
    }

    struct Prepared(u32);

    impl RenderAsset for Prepared {
        type SourceAsset = TestAsset;
        type Param = ();
        fn prepare_asset(
            mut source: TestAsset,
            _: AssetId<TestAsset>,
            _: &mut SystemParamItem<()>,
            _: Option<&Self>,
        ) -> Result<Self, PrepareAssetError<TestAsset>> {
            if source.retries > 0 {
                source.retries -= 1;
                Err(PrepareAssetError::RetryNextUpdate(source))
            } else {
                Ok(Self(source.value))
            }
        }
    }

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<ExtractedAssets<Prepared>>();
        world.init_resource::<RenderAssets<Prepared>>();
        world.init_resource::<PrepareNextFrameAssets<Prepared>>();
        world.init_resource::<RenderAssetBytesPerFrameLimiter>();
        world.init_resource::<PendingAssets<Prepared>>();
        world.init_resource::<AssetPreparationStatus>();
        world
    }

    fn extract(world: &mut World, id: AssetId<TestAsset>, retries: usize, value: u32) {
        let mut extracted = world.resource_mut::<ExtractedAssets<Prepared>>();
        extracted.extracted.push((id, TestAsset { retries, value }));
        extracted.added.insert(id);
    }

    fn prepare(world: &mut World) -> AssetPreparationSnapshot {
        world
            .run_system_once(observe_extracted::<Prepared>)
            .unwrap();
        world.run_system_once(prepare_assets::<Prepared>).unwrap();
        world
            .run_system_once(reconcile_prepared::<Prepared>)
            .unwrap();
        world
            .resource_mut::<ExtractedAssets<Prepared>>()
            .added
            .clear();
        world.resource::<AssetPreparationStatus>().snapshot()
    }

    #[test]
    fn real_extraction_schedule_observes_before_preparation_and_tracks_unused_not_removed() {
        use bevy::{
            ecs::schedule::ScheduleLabel,
            render::{extract_plugin::ExtractPlugin, render_asset::RenderAssetPlugin},
        };
        let mut app = App::new();
        app.add_plugins(ExtractPlugin::default())
            .init_resource::<Assets<TestAsset>>()
            .add_message::<AssetEvent<TestAsset>>()
            .add_plugins(RenderAssetPlugin::<Prepared>::default());
        let render = app.get_sub_app_mut(RenderApp).unwrap();
        render
            .init_resource::<RenderAssetBytesPerFrameLimiter>()
            .init_resource::<AssetPreparationStatus>();
        install::<Prepared>(render);
        render.update_schedule = Some(Render.intern());
        let handle = app
            .world_mut()
            .resource_mut::<Assets<TestAsset>>()
            .add(TestAsset {
                retries: 1,
                value: 7,
            });
        let id = handle.id();
        app.world_mut()
            .write_message(AssetEvent::<TestAsset>::Added { id });
        app.update();
        let snapshot = |app: &App| {
            app.get_sub_app(RenderApp)
                .unwrap()
                .world()
                .resource::<AssetPreparationStatus>()
                .snapshot()
        };
        assert_eq!(snapshot(&app).tracked_types, 1);
        assert_eq!(snapshot(&app).pending_preparations, 1);
        let revision = snapshot(&app).revision;
        app.update();
        assert_eq!(snapshot(&app).pending_preparations, 0);
        assert_eq!(snapshot(&app).revision, revision);
        assert_eq!(
            app.get_sub_app(RenderApp)
                .unwrap()
                .world()
                .resource::<RenderAssets<Prepared>>()
                .get(id)
                .unwrap()
                .0,
            7
        );

        app.world_mut()
            .resource_mut::<Assets<TestAsset>>()
            .remove(id);
        app.world_mut()
            .write_message(AssetEvent::<TestAsset>::Removed { id });
        app.update();
        assert_eq!(snapshot(&app).revision, revision);
        assert!(
            app.get_sub_app(RenderApp)
                .unwrap()
                .world()
                .resource::<RenderAssets<Prepared>>()
                .get(id)
                .is_some()
        );
        app.world_mut()
            .write_message(AssetEvent::<TestAsset>::Unused { id });
        app.update();
        assert!(snapshot(&app).revision > revision);
        assert_eq!(snapshot(&app).pending_removals, 0);
        assert!(
            app.get_sub_app(RenderApp)
                .unwrap()
                .world()
                .resource::<RenderAssets<Prepared>>()
                .get(id)
                .is_none()
        );
    }

    #[test]
    fn retries_remain_pending_until_actual_preparation_and_superseding_content_wins() {
        let mut world = world();
        let id = AssetId::default();
        world
            .resource_mut::<RenderAssets<Prepared>>()
            .insert(id, Prepared(1));
        extract(&mut world, id, 2, 2);
        let first = prepare(&mut world);
        assert_eq!(first.pending_preparations, 1);
        assert!(world.resource::<RenderAssets<Prepared>>().get(id).is_none());
        let second = prepare(&mut world);
        assert_eq!(second.pending_preparations, 1);
        assert_eq!(
            second.revision, first.revision,
            "a retry is not new extraction"
        );
        // Replace the queued retry before it succeeds; the old value must not
        // make a newer pending extraction look ready.
        extract(&mut world, id, 1, 3);
        assert_eq!(prepare(&mut world).pending_preparations, 1);
        assert_eq!(prepare(&mut world).pending_preparations, 0);
        assert_eq!(
            world
                .resource::<RenderAssets<Prepared>>()
                .get(id)
                .unwrap()
                .0,
            3
        );
    }

    #[test]
    fn observation_does_not_drain_extraction_and_removal_cancels_retry() {
        let mut world = world();
        let id = AssetId::default();
        extract(&mut world, id, 3, 1);
        world
            .run_system_once(observe_extracted::<Prepared>)
            .unwrap();
        assert_eq!(
            world
                .resource::<ExtractedAssets<Prepared>>()
                .extracted
                .len(),
            1
        );
        // Use the real preparation system, including its private retry queue.
        world.run_system_once(prepare_assets::<Prepared>).unwrap();
        world
            .resource_mut::<ExtractedAssets<Prepared>>()
            .added
            .clear();
        world
            .resource_mut::<ExtractedAssets<Prepared>>()
            .removed
            .insert(id);
        let removed = prepare(&mut world);
        assert_eq!(removed.pending_preparations, 0);
        assert_eq!(removed.pending_removals, 0);
        for _ in 0..4 {
            assert_eq!(prepare(&mut world).pending_preparations, 0);
        }
        assert!(world.resource::<RenderAssets<Prepared>>().get(id).is_none());
    }

    #[test]
    fn removal_is_pending_until_gpu_entry_disappears_and_overflow_stays_unknown() {
        let mut world = world();
        let id = AssetId::default();
        world
            .resource_mut::<RenderAssets<Prepared>>()
            .insert(id, Prepared(1));
        world
            .resource_mut::<ExtractedAssets<Prepared>>()
            .removed
            .insert(id);
        world.resource_mut::<AssetPreparationStatus>().revision = Some(u64::MAX);
        world
            .run_system_once(observe_extracted::<Prepared>)
            .unwrap();
        world
            .run_system_once(reconcile_prepared::<Prepared>)
            .unwrap();
        assert_eq!(
            world
                .resource::<AssetPreparationStatus>()
                .snapshot()
                .pending_removals,
            1
        );
        assert_eq!(
            world
                .resource::<AssetPreparationStatus>()
                .snapshot()
                .revision,
            None
        );
        let settled = prepare(&mut world);
        assert_eq!(settled.pending_removals, 0);
        assert_eq!(settled.revision, None);
    }
}
