//! In-process content sources: Bevy content rendered inside comp (a scene
//! plugin, not a Wayland client) that is measured like a client window.
//!
//! A plugin puts [`ContentSource`] on the root entity of its content and
//! writes [`ContentSourceFrame`] in every frame its content changes.
//! `ContentSource` is immutable, so every way its id can change (insert,
//! re-insert with another id, removal, despawn) runs a component hook, and
//! the registry follows the live components exactly.

use std::collections::{HashMap, VecDeque};

use bevy::{
    ecs::{lifecycle::HookContext, world::DeferredWorld},
    prelude::*,
    render::{Extract, ExtractSchedule, RenderApp},
};

use crate::protocol::{FramePresentationReporter, presentation::FrameSource};

/// Validated id: `[a-z0-9_-]{1,64}` (no `.`, it becomes a props path segment).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContentSourceId(String);

impl ContentSourceId {
    pub(crate) fn new(id: impl Into<String>) -> Result<Self, String> {
        let id = id.into();
        let valid = (1..=64).contains(&id.len())
            && id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            });
        if valid {
            Ok(Self(id))
        } else {
            Err(format!(
                "content source id {id:?} must match [a-z0-9_-]{{1,64}}"
            ))
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Put on the root entity of the plugin's rendered content. Immutable: to
/// change the id, insert a new value (the hooks re-register).
#[derive(Component, Clone, Debug)]
#[component(
    immutable,
    on_insert = content_source_inserted,
    on_remove = content_source_removed
)]
pub(crate) struct ContentSource {
    pub(crate) id: ContentSourceId,
    /// `None` = every output it is visible on. Reported as
    /// `sources.<id>.output`; the source is measured on the reporting
    /// output (nested has one). Re-inserting the same id with another
    /// output keeps the registration and its original output.
    pub(crate) output: Option<String>,
}

/// Written by the plugin in any frame where its content changed; comp moves
/// the cost fields out once per frame and carries them until a presented
/// frame report consumes them.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContentSourceFrame {
    /// +1 per content update (the "commit").
    pub(crate) revision: u64,
    /// GPU upload bytes since the last snapshot.
    pub(crate) upload_bytes: u64,
    /// Damaged area in physical px since the last snapshot.
    pub(crate) damage_px: u64,
    /// The injected `input_seq` the update answers, if known.
    pub(crate) consumed_input: Option<u64>,
}

impl ContentSourceFrame {
    /// Several updates between two snapshots keep the newest revision and
    /// add their costs, so no work is lost and skipped revisions count as
    /// discarded.
    #[allow(dead_code)]
    pub(crate) fn record(&mut self, revision: u64, upload_bytes: u64, damage_px: u64) {
        self.revision = self.revision.max(revision);
        self.upload_bytes = self.upload_bytes.saturating_add(upload_bytes);
        self.damage_px = self.damage_px.saturating_add(damage_px);
    }
}

/// Inserted on an entity whose `ContentSource` was refused (invalid or
/// duplicate id), so the plugin can see the refusal. A duplicate is
/// registered (and this marker removed) once the holder of its id goes.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentSourceRejected(pub(crate) String);

#[derive(Resource, Default)]
struct ContentSourceRegistry {
    by_id: HashMap<ContentSourceId, Entity>,
    by_entity: HashMap<Entity, ContentSourceId>,
    /// Refused duplicates, in arrival order, per id (never empty).
    waiting: HashMap<ContentSourceId, VecDeque<Entity>>,
    waiting_by_entity: HashMap<Entity, ContentSourceId>,
    /// Newest revision seen per registered source, for unregister's count.
    revisions: HashMap<ContentSourceId, u64>,
}

impl ContentSourceRegistry {
    fn remove_waiter(&mut self, id: &ContentSourceId, entity: Entity) {
        if let Some(queue) = self.waiting.get_mut(id) {
            queue.retain(|waiter| *waiter != entity);
            if queue.is_empty() {
                self.waiting.remove(id);
            }
        }
    }

    fn next_waiter(&mut self, id: &ContentSourceId) -> Option<Entity> {
        let queue = self.waiting.get_mut(id)?;
        let next = queue.pop_front();
        if queue.is_empty() {
            self.waiting.remove(id);
        }
        if let Some(next) = next {
            self.waiting_by_entity.remove(&next);
        }
        next
    }
}

/// This frame's per-source snapshot, taken after visibility is computed.
#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentSourceSnapshot(pub(crate) Vec<FrameSource>);

/// Render-world view of the sources: the latest revision and visibility,
/// with costs accumulated over every extracted frame since the last
/// presented report consumed them.
#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtractedContentSources(pub(crate) Vec<FrameSource>);

impl ExtractedContentSources {
    fn accumulate(&mut self, frame: &[FrameSource]) {
        let mut carried = std::mem::take(&mut self.0)
            .into_iter()
            .map(|source| (source.id.clone(), source))
            .collect::<HashMap<_, _>>();
        // Sources absent from this frame are gone (unregistered): drop them.
        self.0 = frame
            .iter()
            .map(|source| match carried.remove(&source.id) {
                Some(previous) => FrameSource {
                    upload_bytes: previous.upload_bytes.saturating_add(source.upload_bytes),
                    damage_px: previous.damage_px.saturating_add(source.damage_px),
                    // One slot: only the newest answered input is kept
                    // between reports.
                    consumed_input: source.consumed_input.or(previous.consumed_input),
                    revised_us: source.revised_us.or(previous.revised_us),
                    first_revised_us: previous.first_revised_us.or(source.first_revised_us),
                    ..source.clone()
                },
                None => source.clone(),
            })
            .collect();
    }

    /// A presented frame report took the accumulated costs. Every backend
    /// that reports frames must call this after sending a report (nested
    /// does in main.rs; kms-live must in Step 5), or the totals it reports
    /// keep growing until they saturate.
    pub(crate) fn consume(&mut self) {
        for source in &mut self.0 {
            source.upload_bytes = 0;
            source.damage_px = 0;
            source.consumed_input = None;
            source.revised_us = None;
            source.first_revised_us = None;
        }
    }
}

pub(crate) struct ContentSourcePlugin;

impl Plugin for ContentSourcePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContentSourceRegistry>()
            .init_resource::<ContentSourceSnapshot>()
            .add_systems(Last, snapshot_content_sources);
        if let Some(render) = app.get_sub_app_mut(RenderApp) {
            render
                .init_resource::<ExtractedContentSources>()
                .add_systems(ExtractSchedule, extract_content_sources);
        }
    }
}

fn register(world: &mut DeferredWorld, entity: Entity, id: ContentSourceId) {
    let reporter = world.get_resource::<FramePresentationReporter>().cloned();
    let output = world
        .get::<ContentSource>(entity)
        .and_then(|source| source.output.clone());
    let Some(mut registry) = world.get_resource_mut::<ContentSourceRegistry>() else {
        return;
    };
    registry.by_id.insert(id.clone(), entity);
    registry.by_entity.insert(entity, id.clone());
    registry.revisions.insert(id.clone(), 0);
    if let Some(reporter) = reporter {
        reporter.source_registered(id.as_str().to_string(), output);
    }
}

/// Take `entity` out of the registry (registered or waiting). A registered
/// id passes to the oldest waiter still carrying it.
fn forget_entity(world: &mut DeferredWorld, entity: Entity) {
    let reporter = world.get_resource::<FramePresentationReporter>().cloned();
    let Some(mut registry) = world.get_resource_mut::<ContentSourceRegistry>() else {
        return;
    };
    if let Some(id) = registry.waiting_by_entity.remove(&entity) {
        registry.remove_waiter(&id, entity);
    }
    let Some(id) = registry.by_entity.remove(&entity) else {
        return;
    };
    registry.by_id.remove(&id);
    let revision = registry.revisions.remove(&id).unwrap_or(0);
    let successor = registry.next_waiter(&id);
    if let Some(reporter) = &reporter {
        reporter.source_unregistered(id.as_str().to_string(), revision);
    }
    let Some(successor) = successor else {
        return;
    };
    world
        .commands()
        .entity(successor)
        .try_remove::<ContentSourceRejected>();
    register(world, successor, id);
}

fn content_source_inserted(mut world: DeferredWorld, context: HookContext) {
    let entity = context.entity;
    let Some(source) = world.get::<ContentSource>(entity).cloned() else {
        return;
    };
    let (registered, waiting) = world
        .get_resource::<ContentSourceRegistry>()
        .map(|registry| {
            (
                registry.by_entity.get(&entity).cloned(),
                registry.waiting_by_entity.get(&entity).cloned(),
            )
        })
        .unwrap_or_default();
    // Re-inserting the same id keeps the registration (and its stats) or
    // the place in the queue.
    if registered.as_ref() == Some(&source.id) || waiting.as_ref() == Some(&source.id) {
        return;
    }
    forget_entity(&mut world, entity);
    if let Err(reason) = ContentSourceId::new(source.id.as_str()) {
        // Invalid ids never become valid: refused, not queued.
        warn!(%reason, "content source refused");
        world
            .commands()
            .entity(entity)
            .try_insert(ContentSourceRejected(reason));
        return;
    }
    let holder = world
        .get_resource::<ContentSourceRegistry>()
        .and_then(|registry| registry.by_id.get(&source.id).copied());
    if holder.is_some() {
        let reason = format!(
            "content source id {:?} is already registered",
            source.id.as_str()
        );
        warn!(%reason, "content source refused");
        if let Some(mut registry) = world.get_resource_mut::<ContentSourceRegistry>() {
            registry
                .waiting
                .entry(source.id.clone())
                .or_default()
                .push_back(entity);
            registry.waiting_by_entity.insert(entity, source.id.clone());
        }
        world
            .commands()
            .entity(entity)
            .try_insert(ContentSourceRejected(reason));
        return;
    }
    world
        .commands()
        .entity(entity)
        .try_remove::<ContentSourceRejected>();
    register(&mut world, entity, source.id);
}

fn content_source_removed(mut world: DeferredWorld, context: HookContext) {
    forget_entity(&mut world, context.entity);
}

fn snapshot_content_sources(
    mut registry: ResMut<ContentSourceRegistry>,
    mut snapshot: ResMut<ContentSourceSnapshot>,
    mut sources: Query<(
        Entity,
        &ContentSource,
        Option<&mut ContentSourceFrame>,
        Option<&ViewVisibility>,
    )>,
) {
    snapshot.0.clear();
    for (entity, source, frame, visibility) in &mut sources {
        if registry.by_entity.get(&entity) != Some(&source.id) {
            continue;
        }
        let shown = visibility.is_some_and(|visibility| visibility.get());
        let known = registry.revisions.get(&source.id).copied().unwrap_or(0);
        let entry = match frame {
            Some(mut frame) => {
                // A new revision is stamped when comp first sees it.
                let revised_us = (frame.revision > known).then(crate::frame_trace::monotonic_us);
                let entry = FrameSource {
                    id: source.id.as_str().to_string(),
                    revision: frame.revision,
                    shown,
                    upload_bytes: frame.upload_bytes,
                    damage_px: frame.damage_px,
                    consumed_input: frame.consumed_input,
                    revised_us,
                    first_revised_us: revised_us,
                };
                // The costs move to the render world's accumulator; they are
                // not lost if this frame is never reported.
                if frame.upload_bytes != 0 || frame.damage_px != 0 || frame.consumed_input.is_some()
                {
                    frame.upload_bytes = 0;
                    frame.damage_px = 0;
                    frame.consumed_input = None;
                }
                entry
            }
            None => FrameSource {
                id: source.id.as_str().to_string(),
                shown,
                ..FrameSource::default()
            },
        };
        if let Some(revision) = registry.revisions.get_mut(&source.id) {
            *revision = (*revision).max(entry.revision);
        }
        snapshot.0.push(entry);
    }
    snapshot.0.sort_by(|left, right| left.id.cmp(&right.id));
}

fn extract_content_sources(
    snapshot: Extract<Res<ContentSourceSnapshot>>,
    mut extracted: ResMut<ExtractedContentSources>,
) {
    extracted.accumulate(&snapshot.0);
}

/// Gate G1s (`--features content-source-probe`, nested only): a quad whose
/// colour changes every frame, registered as content source `probe` with
/// fake costs. It writes `COSMIX_CONTENT_SOURCE_PROBE_REVISIONS` revisions
/// (default 300), logs its own totals, holds still for four seconds so the
/// last revision and the carried costs are reported, then despawns itself.
#[cfg(feature = "content-source-probe")]
pub(crate) mod probe {
    use super::*;

    const DEFAULT_REVISIONS: u64 = 300;
    const HOLD_SECS: f64 = 4.0;
    const UPLOAD_BYTES: u64 = 4_096;
    const DAMAGE_PX: u64 = 96 * 96;

    #[derive(Component)]
    struct ContentSourceProbe {
        revisions: u64,
        written: u64,
        upload_bytes: u64,
        damage_px: u64,
        done_at: Option<f64>,
    }

    pub(crate) struct ContentSourceProbePlugin;

    impl Plugin for ContentSourceProbePlugin {
        fn build(&self, app: &mut App) {
            app.add_systems(Startup, spawn_probe)
                .add_systems(Update, drive_probe);
        }
    }

    fn spawn_probe(mut commands: Commands) {
        let revisions = std::env::var("COSMIX_CONTENT_SOURCE_PROBE_REVISIONS")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(DEFAULT_REVISIONS);
        let Ok(id) = ContentSourceId::new("probe") else {
            return;
        };
        commands.spawn((
            Name::new("content-source probe"),
            ContentSource { id, output: None },
            ContentSourceFrame::default(),
            ContentSourceProbe {
                revisions,
                written: 0,
                upload_bytes: 0,
                damage_px: 0,
                done_at: None,
            },
            Sprite::from_color(Color::WHITE, Vec2::splat(96.0)),
            Transform::from_xyz(-200.0, 150.0, 0.5),
        ));
        info!(revisions, "COSMIX_CONTENT_SOURCE_PROBE START");
    }

    fn drive_probe(
        time: Res<Time>,
        mut commands: Commands,
        mut probes: Query<(
            Entity,
            &mut ContentSourceProbe,
            &mut ContentSourceFrame,
            &mut Sprite,
        )>,
    ) {
        let now = time.elapsed_secs_f64();
        for (entity, mut probe, mut frame, mut sprite) in &mut probes {
            if probe.written < probe.revisions {
                probe.written += 1;
                let upload = UPLOAD_BYTES + probe.written % 7;
                frame.record(probe.written, upload, DAMAGE_PX);
                probe.upload_bytes += upload;
                probe.damage_px += DAMAGE_PX;
                sprite.color = Color::hsl((probe.written * 37 % 360) as f32, 0.8, 0.5);
                if probe.written == probe.revisions {
                    probe.done_at = Some(now);
                    info!(
                        "COSMIX_CONTENT_SOURCE_PROBE DONE revisions={} upload_bytes={} damage_px={}",
                        probe.written, probe.upload_bytes, probe.damage_px
                    );
                }
            } else if probe.done_at.is_some_and(|done| now - done >= HOLD_SECS) {
                commands.entity(entity).despawn();
                info!("COSMIX_CONTENT_SOURCE_PROBE DESPAWNED");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<ContentSourceRegistry>()
            .init_resource::<ContentSourceSnapshot>()
            .add_systems(Last, snapshot_content_sources);
        app
    }

    fn source(id: &str) -> ContentSource {
        ContentSource {
            id: ContentSourceId(id.to_string()),
            output: None,
        }
    }

    fn registered(app: &App) -> Vec<(String, Entity)> {
        let mut ids = app
            .world()
            .resource::<ContentSourceRegistry>()
            .by_id
            .iter()
            .map(|(id, entity)| (id.as_str().to_string(), *entity))
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    fn rejected(app: &App, entity: Entity) -> bool {
        app.world().get::<ContentSourceRejected>(entity).is_some()
    }

    #[test]
    fn ids_are_validated() {
        for valid in ["a", "scene-iced_0", &"x".repeat(64)] {
            assert!(ContentSourceId::new(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "Scene", "a.b", "a b", &"x".repeat(65)] {
            assert!(ContentSourceId::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn registers_once_refuses_duplicates_and_unregisters_on_despawn_or_removal() {
        let mut app = app();
        let first = app.world_mut().spawn(source("scene")).id();
        app.update();
        assert_eq!(registered(&app), [("scene".to_string(), first)]);
        assert!(!rejected(&app, first));

        let duplicate = app.world_mut().spawn(source("scene")).id();
        let invalid = app.world_mut().spawn(source("Bad.Id")).id();
        app.update();
        assert!(rejected(&app, duplicate));
        assert!(rejected(&app, invalid));
        assert_eq!(registered(&app), [("scene".to_string(), first)]);

        // Removing a refused duplicate must not unregister the holder.
        let spare = app.world_mut().spawn(source("scene")).id();
        app.world_mut().despawn(spare);
        app.update();
        assert_eq!(registered(&app), [("scene".to_string(), first)]);

        // The holder goes: the waiting duplicate takes over the id.
        app.world_mut().despawn(first);
        app.update();
        assert_eq!(registered(&app), [("scene".to_string(), duplicate)]);
        assert!(!rejected(&app, duplicate));

        app.world_mut()
            .entity_mut(duplicate)
            .remove::<ContentSource>();
        app.update();
        assert!(registered(&app).is_empty());
        let registry = app.world().resource::<ContentSourceRegistry>();
        assert!(registry.by_entity.is_empty());
        assert!(
            registry.waiting.is_empty() && registry.waiting_by_entity.is_empty(),
            "invalid ids are not queued and empty queues are dropped"
        );
    }

    #[test]
    fn re_inserting_the_same_id_keeps_the_registration() {
        let mut app = app();
        let holder = app.world_mut().spawn(source("scene")).id();
        let waiter = app.world_mut().spawn(source("scene")).id();
        app.update();
        app.world_mut()
            .resource_mut::<ContentSourceRegistry>()
            .revisions
            .insert(ContentSourceId("scene".into()), 9);
        // Same id on the holder: no unregister, no hand-over, stats kept.
        app.world_mut().entity_mut(holder).insert(source("scene"));
        app.update();
        assert_eq!(registered(&app), [("scene".to_string(), holder)]);
        assert!(rejected(&app, waiter));
        assert_eq!(
            app.world()
                .resource::<ContentSourceRegistry>()
                .revisions
                .get(&ContentSourceId("scene".into())),
            Some(&9)
        );
        // Same id on the waiter: it keeps its place.
        app.world_mut().entity_mut(waiter).insert(source("scene"));
        app.update();
        assert!(rejected(&app, waiter));
        // The waiter moves to another id: it leaves the queue and registers.
        app.world_mut().entity_mut(waiter).insert(source("other"));
        app.update();
        assert!(!rejected(&app, waiter));
        let registry = app.world().resource::<ContentSourceRegistry>();
        assert!(registry.waiting.is_empty());
        assert_eq!(
            registered(&app),
            [("other".to_string(), waiter), ("scene".to_string(), holder)]
        );
    }

    #[test]
    fn changing_the_id_re_registers() {
        let mut app = app();
        let entity = app.world_mut().spawn(source("before")).id();
        app.update();
        assert_eq!(registered(&app), [("before".to_string(), entity)]);
        app.world_mut().entity_mut(entity).insert(source("after"));
        app.update();
        assert_eq!(registered(&app), [("after".to_string(), entity)]);
        // Re-inserting the same id keeps one registration.
        app.world_mut().entity_mut(entity).insert(source("after"));
        app.update();
        assert_eq!(registered(&app), [("after".to_string(), entity)]);
    }

    #[test]
    fn costs_move_out_of_the_component_each_snapshot() {
        let mut app = app();
        let entity = app
            .world_mut()
            .spawn((source("scene"), ContentSourceFrame::default()))
            .id();
        app.update();
        {
            let mut frame = app
                .world_mut()
                .get_mut::<ContentSourceFrame>(entity)
                .unwrap();
            frame.record(1, 100, 10);
            frame.record(2, 50, 5);
        }
        app.update();
        let snapshot = app.world().resource::<ContentSourceSnapshot>().0.clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            (
                snapshot[0].revision,
                snapshot[0].upload_bytes,
                snapshot[0].damage_px
            ),
            (2, 150, 15)
        );
        assert!(
            snapshot[0].revised_us.is_some(),
            "a new revision is stamped"
        );
        // No visibility component in this minimal app: not shown.
        assert!(!snapshot[0].shown);
        let frame = *app.world().get::<ContentSourceFrame>(entity).unwrap();
        assert_eq!(
            (frame.revision, frame.upload_bytes, frame.damage_px),
            (2, 0, 0)
        );
        assert_eq!(
            app.world()
                .resource::<ContentSourceRegistry>()
                .revisions
                .get(&ContentSourceId("scene".into())),
            Some(&2)
        );

        app.update();
        let snapshot = app.world().resource::<ContentSourceSnapshot>().0.clone();
        assert_eq!(
            (
                snapshot[0].revision,
                snapshot[0].upload_bytes,
                snapshot[0].damage_px
            ),
            (2, 0, 0),
            "an unchanged source is still listed with zero cost"
        );
        assert_eq!(
            snapshot[0].revised_us, None,
            "an old revision is not restamped"
        );
    }

    #[test]
    fn render_side_costs_carry_until_a_report_consumes_them() {
        let frame = |upload: u64, input: Option<u64>, revised: Option<u64>| FrameSource {
            id: "scene".into(),
            revision: 3,
            shown: true,
            upload_bytes: upload,
            damage_px: upload / 2,
            consumed_input: input,
            revised_us: revised,
            first_revised_us: revised,
        };
        let mut extracted = ExtractedContentSources::default();
        extracted.accumulate(&[frame(10, Some(7), Some(100))]);
        // An unreported frame: costs add up, the input mark survives, the
        // oldest and newest revision stamps are both kept.
        extracted.accumulate(&[frame(4, None, Some(200))]);
        assert_eq!(extracted.0[0].upload_bytes, 14);
        assert_eq!(extracted.0[0].damage_px, 7);
        assert_eq!(extracted.0[0].consumed_input, Some(7));
        assert_eq!(
            (extracted.0[0].first_revised_us, extracted.0[0].revised_us),
            (Some(100), Some(200))
        );
        extracted.accumulate(&[frame(0, None, None)]);
        assert_eq!(extracted.0[0].revised_us, Some(200));
        extracted.consume();
        assert_eq!(extracted.0[0].upload_bytes, 0);
        assert_eq!(extracted.0[0].consumed_input, None);
        assert_eq!(extracted.0[0].first_revised_us, None);
        extracted.accumulate(&[frame(2, None, None)]);
        assert_eq!(extracted.0[0].upload_bytes, 2);
        // A source that left the scene is dropped.
        extracted.accumulate(&[]);
        assert!(extracted.0.is_empty());
    }

    #[test]
    fn a_visible_entity_is_shown() {
        let mut app = app();
        let entity = app
            .world_mut()
            .spawn((
                source("scene"),
                ContentSourceFrame::default(),
                ViewVisibility::default(),
            ))
            .id();
        app.update();
        app.update();
        assert!(!app.world().resource::<ContentSourceSnapshot>().0[0].shown);
        *app.world_mut().get_mut::<ViewVisibility>(entity).unwrap() = ViewVisibility::VISIBLE;
        app.update();
        // A frame's visibility is only valid until the next visibility pass;
        // with no pass in this app the flag survives into the snapshot.
        assert!(app.world().resource::<ContentSourceSnapshot>().0[0].shown);
    }
}
