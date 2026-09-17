//! Which content each rendered frame contained, for `wp_presentation`.
//!
//! The render world snapshots, per client surface, the newest commit whose
//! texture is prepared and whether the surface is visible, plus the content
//! sources. A backend attaches the snapshot to a frame only once it has
//! proof that frame was presented, then reports it to the protocol thread.

use std::collections::{HashMap, HashSet};

use bevy::{
    asset::AssetId,
    prelude::*,
    render::{
        Extract, ExtractSchedule, Render, RenderApp, RenderSystems, render_asset::RenderAssets,
        texture::GpuImage,
    },
};

use crate::{
    compositor_scene::SurfaceEntities,
    content_source::ExtractedContentSources,
    protocol::{
        SurfaceId,
        presentation::{FrameContent, FrameSurface},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExtractedFrameSurface {
    id: SurfaceId,
    commit: u64,
    visible: bool,
    image: AssetId<Image>,
}

#[derive(Resource, Default)]
struct ExtractedFrameSurfaces(Vec<ExtractedFrameSurface>);

/// The newest commit whose texture was prepared, per surface.
#[derive(Resource, Default)]
struct PreparedCommits(HashMap<SurfaceId, u64>);

/// This render frame's content, valid from `RenderSystems::Prepare` on.
#[derive(Resource, Default, Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderFrameContent(pub(crate) FrameContent);

pub(crate) struct FrameContentPlugin;

impl Plugin for FrameContentPlugin {
    fn build(&self, app: &mut App) {
        let Some(render) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render
            .init_resource::<ExtractedFrameSurfaces>()
            .init_resource::<PreparedCommits>()
            .init_resource::<RenderFrameContent>()
            .add_systems(ExtractSchedule, extract_frame_surfaces)
            .add_systems(Render, resolve_frame_content.in_set(RenderSystems::Prepare));
    }
}

fn extract_frame_surfaces(
    entities: Extract<Res<SurfaceEntities>>,
    visibility: Extract<Query<&ViewVisibility>>,
    mut extracted: ResMut<ExtractedFrameSurfaces>,
) {
    extracted.0.clear();
    for (id, surface) in &entities.surfaces {
        extracted.0.push(ExtractedFrameSurface {
            id: *id,
            commit: surface.applied_commit,
            visible: surface.layout.visible
                && visibility
                    .get(surface.entity)
                    .is_ok_and(|visibility| visibility.get()),
            image: surface.image_id(),
        });
    }
}

fn resolve_frame_content(
    extracted: Res<ExtractedFrameSurfaces>,
    sources: Option<Res<ExtractedContentSources>>,
    images: Option<Res<RenderAssets<GpuImage>>>,
    mut prepared: ResMut<PreparedCommits>,
    mut content: ResMut<RenderFrameContent>,
) {
    let surfaces = extracted
        .0
        .iter()
        .map(|surface| {
            let ready = images
                .as_ref()
                .is_some_and(|images| images.get(surface.image).is_some());
            (*surface, ready)
        })
        .collect::<Vec<_>>();
    content.0 = frame_content(
        &surfaces,
        sources.map(|sources| sources.0.clone()).unwrap_or_default(),
        &mut prepared.0,
    );
}

/// `ready` = the surface's texture is prepared this frame. Bevy removes a
/// GPU image before preparing its replacement, so an unprepared texture is
/// not drawn at all: such a surface is reported not shown, at the last
/// prepared commit, which leaves newer commits waiting and resolves nothing
/// already resolved.
fn frame_content(
    surfaces: &[(ExtractedFrameSurface, bool)],
    sources: Vec<crate::protocol::presentation::FrameSource>,
    prepared: &mut HashMap<SurfaceId, u64>,
) -> FrameContent {
    let live = surfaces
        .iter()
        .map(|(surface, _)| surface.id)
        .collect::<HashSet<_>>();
    prepared.retain(|id, _| live.contains(id));
    let mut frame = FrameContent {
        surfaces: Vec::with_capacity(surfaces.len()),
        sources,
    };
    for (surface, ready) in surfaces {
        if *ready {
            prepared.insert(surface.id, surface.commit);
        }
        frame.surfaces.push(if !surface.visible {
            // Hidden: nothing committed so far will be shown as committed.
            FrameSurface {
                id: surface.id,
                commit_seq: surface.commit,
                shown: false,
            }
        } else if *ready {
            FrameSurface {
                id: surface.id,
                commit_seq: surface.commit,
                shown: true,
            }
        } else {
            FrameSurface {
                id: surface.id,
                commit_seq: prepared.get(&surface.id).copied().unwrap_or(0),
                shown: false,
            }
        });
    }
    frame.surfaces.sort_by_key(|surface| surface.id.0);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(id: u64, commit: u64, visible: bool) -> ExtractedFrameSurface {
        ExtractedFrameSurface {
            id: SurfaceId(id),
            commit,
            visible,
            image: AssetId::default(),
        }
    }

    #[test]
    fn shown_needs_visibility_and_a_prepared_texture() {
        let mut prepared = HashMap::new();
        let frame = frame_content(
            &[
                (surface(1, 3, true), true),
                (surface(2, 5, false), true),
                (surface(3, 7, true), false),
            ],
            Vec::new(),
            &mut prepared,
        );
        assert_eq!(
            frame.surfaces,
            [
                FrameSurface {
                    id: SurfaceId(1),
                    commit_seq: 3,
                    shown: true
                },
                FrameSurface {
                    id: SurfaceId(2),
                    commit_seq: 5,
                    shown: false
                },
                FrameSurface {
                    id: SurfaceId(3),
                    commit_seq: 0,
                    shown: false
                },
            ]
        );
    }

    #[test]
    fn an_unprepared_update_reports_the_last_prepared_commit() {
        let mut prepared = HashMap::new();
        frame_content(&[(surface(1, 3, true), true)], Vec::new(), &mut prepared);
        let frame = frame_content(&[(surface(1, 4, true), false)], Vec::new(), &mut prepared);
        assert_eq!(
            frame.surfaces,
            [FrameSurface {
                id: SurfaceId(1),
                commit_seq: 3,
                shown: false
            }]
        );
        let frame = frame_content(&[(surface(1, 4, true), true)], Vec::new(), &mut prepared);
        assert_eq!(frame.surfaces[0].commit_seq, 4);
        assert!(frame.surfaces[0].shown);
        // A surface that left the scene forgets its prepared commit.
        frame_content(&[], Vec::new(), &mut prepared);
        assert!(prepared.is_empty());
    }
}
