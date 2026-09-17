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
    on_discard = content_source_discarded
)]
pub(crate) struct ContentSource {
    pub(crate) id: ContentSourceId,
    /// `None` = every output it is visible on. Per-output accounting is
    /// Step 6; today every source is measured on the one reporting output.
    #[allow(dead_code)]
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
    /// Refused duplicates, in arrival order, per id.
    waiting: HashMap<ContentSourceId, VecDeque<Entity>>,
    /// Newest revision seen per registered source, for unregister's count.
    revisions: HashMap<ContentSourceId, u64>,
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
                    consumed_input: source.consumed_input.or(previous.consumed_input),
                    ..source.clone()
                },
                None => source.clone(),
            })
            .collect();
    }

    /// A presented frame report took the accumulated costs.
    pub(crate) fn consume(&mut self) {
        for source in &mut self.0 {
            source.upload_bytes = 0;
            source.damage_px = 0;
            source.consumed_input = None;
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
    let Some(mut registry) = world.get_resource_mut::<ContentSourceRegistry>() else {
        return;
    };
    registry.by_id.insert(id.clone(), entity);
    registry.by_entity.insert(entity, id.clone());
    registry.revisions.insert(id.clone(), 0);
    drop(registry);
    if let Some(reporter) = reporter {
        reporter.source_registered(id.as_str().to_string());
    }
}

fn content_source_inserted(mut world: DeferredWorld, context: HookContext) {
    let entity = context.entity;
    let Some(source) = world.get::<ContentSource>(entity).cloned() else {
        return;
    };
    let refusal = match ContentSourceId::new(source.id.as_str()) {
        Err(error) => Some(error),
        Ok(id) => world
            .get_resource::<ContentSourceRegistry>()
            .and_then(|registry| registry.by_id.get(&id).copied())
            .filter(|holder| *holder != entity)
            .map(|_| format!("content source id {:?} is already registered", id.as_str())),
    };
    if let Some(reason) = refusal {
        warn!(%reason, "content source refused");
        if let Some(mut registry) = world.get_resource_mut::<ContentSourceRegistry>() {
            registry
                .waiting
                .entry(source.id.clone())
                .or_default()
                .push_back(entity);
        }
        world
            .commands()
            .entity(entity)
            .insert(ContentSourceRejected(reason));
        return;
    }
    world
        .commands()
        .entity(entity)
        .remove::<ContentSourceRejected>();
    register(&mut world, entity, source.id);
}

fn content_source_discarded(mut world: DeferredWorld, context: HookContext) {
    let entity = context.entity;
    let Some(source) = world.get::<ContentSource>(entity).cloned() else {
        return;
    };
    let reporter = world.get_resource::<FramePresentationReporter>().cloned();
    let Some(mut registry) = world.get_resource_mut::<ContentSourceRegistry>() else {
        return;
    };
    if let Some(waiting) = registry.waiting.get_mut(&source.id) {
        waiting.retain(|waiter| *waiter != entity);
    }
    if registry.by_entity.get(&entity) != Some(&source.id) {
        return;
    }
    registry.by_entity.remove(&entity);
    registry.by_id.remove(&source.id);
    let revision = registry.revisions.remove(&source.id).unwrap_or(0);
    // The next refused duplicate that still carries this id takes over.
    let successor = registry
        .waiting
        .get_mut(&source.id)
        .and_then(|waiting| waiting.pop_front());
    if registry
        .waiting
        .get(&source.id)
        .is_some_and(VecDeque::is_empty)
    {
        registry.waiting.remove(&source.id);
    }
    drop(registry);
    if let Some(reporter) = &reporter {
        reporter.source_unregistered(source.id.as_str().to_string(), revision);
    }
    if let Some(successor) = successor
        && world
            .get::<ContentSource>(successor)
            .is_some_and(|candidate| candidate.id == source.id)
    {
        world
            .commands()
            .entity(successor)
            .remove::<ContentSourceRejected>();
        register(&mut world, successor, source.id);
    }
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
        let entry = match frame {
            Some(mut frame) => {
                let entry = FrameSource {
                    id: source.id.as_str().to_string(),
                    revision: frame.revision,
                    shown,
                    upload_bytes: frame.upload_bytes,
                    damage_px: frame.damage_px,
                    consumed_input: frame.consumed_input,
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
        assert!(registry.by_entity.is_empty() && registry.waiting.len() <= 1);
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
    }

    #[test]
    fn render_side_costs_carry_until_a_report_consumes_them() {
        let frame = |upload: u64, input: Option<u64>| FrameSource {
            id: "scene".into(),
            revision: 3,
            shown: true,
            upload_bytes: upload,
            damage_px: upload / 2,
            consumed_input: input,
        };
        let mut extracted = ExtractedContentSources::default();
        extracted.accumulate(&[frame(10, Some(7))]);
        // An unreported frame: costs add up, the input mark survives.
        extracted.accumulate(&[frame(4, None)]);
        assert_eq!(extracted.0[0].upload_bytes, 14);
        assert_eq!(extracted.0[0].damage_px, 7);
        assert_eq!(extracted.0[0].consumed_input, Some(7));
        extracted.consume();
        assert_eq!(extracted.0[0].upload_bytes, 0);
        assert_eq!(extracted.0[0].consumed_input, None);
        extracted.accumulate(&[frame(2, None)]);
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
