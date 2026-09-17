//! In-process content sources: Bevy content rendered inside comp (a scene
//! plugin, not a Wayland client) that is measured like a client window.
//!
//! A plugin puts [`ContentSource`] on the root entity of its content and
//! writes [`ContentSourceFrame`] in every frame its content changes. The
//! registry is derived from live components, so a despawned entity or a
//! removed component unregisters by construction.

use std::collections::HashMap;

use bevy::{
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

/// Put on the root entity of the plugin's rendered content.
#[derive(Component, Clone, Debug)]
pub(crate) struct ContentSource {
    pub(crate) id: ContentSourceId,
    /// `None` = every output it is visible on.
    #[allow(dead_code)]
    pub(crate) output: Option<String>,
}

/// Written by the plugin in any frame where its content changed; the cost
/// fields are consumed (zeroed) once per frame when comp snapshots them.
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
/// duplicate id), so the plugin can see the refusal.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentSourceRejected(pub(crate) String);

#[derive(Resource, Default)]
struct ContentSourceRegistry {
    by_id: HashMap<ContentSourceId, Entity>,
    by_entity: HashMap<Entity, ContentSourceId>,
}

/// This frame's per-source snapshot, taken after visibility is computed.
#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentSourceSnapshot(pub(crate) Vec<FrameSource>);

/// Render-world copy of [`ContentSourceSnapshot`].
#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtractedContentSources(pub(crate) Vec<FrameSource>);

pub(crate) struct ContentSourcePlugin;

impl Plugin for ContentSourcePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContentSourceRegistry>()
            .init_resource::<ContentSourceSnapshot>()
            .add_systems(
                PostUpdate,
                (unregister_content_sources, register_content_sources).chain(),
            )
            .add_systems(Last, snapshot_content_sources);
        if let Some(render) = app.get_sub_app_mut(RenderApp) {
            render
                .init_resource::<ExtractedContentSources>()
                .add_systems(ExtractSchedule, extract_content_sources);
        }
    }
}

fn register_content_sources(
    mut commands: Commands,
    mut registry: ResMut<ContentSourceRegistry>,
    reporter: Option<Res<FramePresentationReporter>>,
    added: Query<(Entity, &ContentSource), Added<ContentSource>>,
) {
    for (entity, source) in &added {
        let refusal = match ContentSourceId::new(source.id.as_str()) {
            Err(error) => Some(error),
            Ok(id) => match registry.by_id.get(&id) {
                Some(holder) if *holder != entity => Some(format!(
                    "content source id {:?} is already registered",
                    id.as_str()
                )),
                _ => None,
            },
        };
        if let Some(reason) = refusal {
            warn!(%reason, "content source refused");
            commands
                .entity(entity)
                .insert(ContentSourceRejected(reason));
            continue;
        }
        let id = source.id.clone();
        registry.by_id.insert(id.clone(), entity);
        registry.by_entity.insert(entity, id.clone());
        if let Some(reporter) = &reporter {
            reporter.source_registered(id.as_str().to_string());
        }
    }
}

fn unregister_content_sources(
    mut registry: ResMut<ContentSourceRegistry>,
    reporter: Option<Res<FramePresentationReporter>>,
    mut removed: RemovedComponents<ContentSource>,
) {
    for entity in removed.read() {
        let Some(id) = registry.by_entity.remove(&entity) else {
            continue;
        };
        if registry.by_id.get(&id) == Some(&entity) {
            registry.by_id.remove(&id);
        }
        if let Some(reporter) = &reporter {
            reporter.source_unregistered(id.as_str().to_string());
        }
    }
}

fn snapshot_content_sources(
    registry: Res<ContentSourceRegistry>,
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
        snapshot.0.push(entry);
    }
    snapshot.0.sort_by(|left, right| left.id.cmp(&right.id));
}

fn extract_content_sources(
    snapshot: Extract<Res<ContentSourceSnapshot>>,
    mut extracted: ResMut<ExtractedContentSources>,
) {
    extracted.0.clone_from(&snapshot.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<ContentSourceRegistry>()
            .init_resource::<ContentSourceSnapshot>()
            .add_systems(
                PostUpdate,
                (unregister_content_sources, register_content_sources).chain(),
            )
            .add_systems(Last, snapshot_content_sources);
        app
    }

    fn source(id: &str) -> ContentSource {
        ContentSource {
            id: ContentSourceId(id.to_string()),
            output: None,
        }
    }

    fn registered(app: &App) -> Vec<String> {
        let mut ids = app
            .world()
            .resource::<ContentSourceRegistry>()
            .by_id
            .keys()
            .map(|id| id.as_str().to_string())
            .collect::<Vec<_>>();
        ids.sort();
        ids
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
        app.update();
        assert_eq!(registered(&app), ["scene"]);
        assert!(app.world().get::<ContentSourceRejected>(first).is_none());

        let duplicate = app.world_mut().spawn(source("scene")).id();
        let invalid = app.world_mut().spawn(source("Bad.Id")).id();
        app.update();
        assert!(
            app.world()
                .get::<ContentSourceRejected>(duplicate)
                .is_some()
        );
        assert!(app.world().get::<ContentSourceRejected>(invalid).is_some());
        assert_eq!(registered(&app), ["scene"]);

        // Removing the refused duplicate must not unregister the holder.
        app.world_mut().despawn(duplicate);
        app.update();
        assert_eq!(registered(&app), ["scene"]);

        app.world_mut().despawn(first);
        app.update();
        assert!(registered(&app).is_empty());

        let other = app.world_mut().spawn(source("other")).id();
        app.update();
        assert_eq!(registered(&app), ["other"]);
        app.world_mut().entity_mut(other).remove::<ContentSource>();
        app.update();
        assert!(registered(&app).is_empty());
    }

    #[test]
    fn costs_accumulate_until_the_snapshot_and_reset_after() {
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
