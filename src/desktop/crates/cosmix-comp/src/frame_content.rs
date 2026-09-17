//! Which content each rendered frame contained, for `wp_presentation`.
//!
//! The render world records, per client surface, the newest commit whose
//! content the frame actually samples and whether the surface is shown,
//! plus the content sources. A backend attaches the record to a frame only
//! once it has proof that frame was presented, then reports it to the
//! protocol thread.
//!
//! "Shown" means: mapped and visible, on the output, and its texture is the
//! one this frame samples. Occlusion by other windows is not checked.
//!
//! Relies on (not enforced here): the nested backend renders every frame
//! (no idle skip), pipelined rendering is off (extract and render belong to
//! the same main-world update), and there is a single output whose logical
//! canvas starts at the origin.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::{
    asset::AssetId,
    prelude::*,
    render::{
        Extract, ExtractSchedule, Render, RenderApp, RenderSystems, render_asset::RenderAssets,
        texture::GpuImage,
    },
};
use cosmix_wgpu_dmabuf::{ImportProgress, ImportedDmabufImages};

use crate::{
    compositor_scene::{LogicalCanvasSize, SurfaceEntities},
    content_source::ExtractedContentSources,
    protocol::{
        SurfaceId,
        presentation::{FrameContent, FrameSurface},
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExtractedFrameSurface {
    id: SurfaceId,
    /// The newest commit the main world applied to this surface.
    commit: u64,
    visible: bool,
    image: AssetId<Image>,
    /// DMA-BUF surfaces: which import request carries which commit.
    dmabuf_requests: Option<VecDeque<(u64, u64)>>,
}

#[derive(Resource, Default)]
struct ExtractedFrameSurfaces(Vec<ExtractedFrameSurface>);

#[derive(Default)]
struct SurfaceMemory {
    /// The newest commit a frame actually sampled.
    matched: Option<u64>,
    /// The newest DMA-BUF commit already reported as refused.
    refused: Option<u64>,
}

#[derive(Resource, Default)]
struct FrameContentMemory(HashMap<SurfaceId, SurfaceMemory>);

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
            .init_resource::<FrameContentMemory>()
            .init_resource::<RenderFrameContent>()
            .add_systems(ExtractSchedule, extract_frame_surfaces)
            // Prepare runs after PrepareAssets, where Bevy prepares SHM
            // images and the DMA-BUF bridge installs imported ones.
            .add_systems(Render, resolve_frame_content.in_set(RenderSystems::Prepare));
    }
}

fn on_output(layout: &crate::protocol::SurfaceLayout, canvas: Vec2) -> bool {
    layout.width > 0.0
        && layout.height > 0.0
        && layout.x < canvas.x
        && layout.y < canvas.y
        && layout.x + layout.width > 0.0
        && layout.y + layout.height > 0.0
}

fn extract_frame_surfaces(
    entities: Extract<Res<SurfaceEntities>>,
    canvas: Extract<Option<Res<LogicalCanvasSize>>>,
    visibility: Extract<Query<&ViewVisibility>>,
    mut extracted: ResMut<ExtractedFrameSurfaces>,
) {
    extracted.0.clear();
    let canvas = canvas.as_ref().map(|canvas| canvas.0);
    for (id, surface) in &entities.surfaces {
        extracted.0.push(ExtractedFrameSurface {
            id: *id,
            commit: surface.applied_commit,
            visible: surface.layout.visible
                && canvas.is_none_or(|canvas| on_output(&surface.layout, canvas))
                && visibility
                    .get(surface.entity)
                    .is_ok_and(|visibility| visibility.get()),
            image: surface.image_id(),
            dmabuf_requests: surface.dmabuf_requests().cloned(),
        });
    }
}

/// Run the real extraction system against `main` (as the render app's
/// ExtractSchedule does) and return `(surface, visible, commit)`.
#[cfg(test)]
pub(crate) fn extract_for_test(main: &mut World) -> Vec<(SurfaceId, bool, u64)> {
    use bevy::ecs::system::RunSystemOnce;
    let mut render = World::new();
    render.init_resource::<ExtractedFrameSurfaces>();
    let mut main_world = bevy::render::MainWorld::default();
    *main_world = std::mem::take(main);
    render.insert_resource(main_world);
    render
        .run_system_once(extract_frame_surfaces)
        .expect("extraction runs");
    let mut main_world = render
        .remove_resource::<bevy::render::MainWorld>()
        .expect("main world is returned");
    *main = std::mem::take(&mut *main_world);
    let mut extracted = render
        .resource::<ExtractedFrameSurfaces>()
        .0
        .iter()
        .map(|surface| (surface.id, surface.visible, surface.commit))
        .collect::<Vec<_>>();
    extracted.sort_by_key(|(id, ..)| id.0);
    extracted
}

fn resolve_frame_content(
    extracted: Res<ExtractedFrameSurfaces>,
    sources: Option<Res<ExtractedContentSources>>,
    images: Option<Res<RenderAssets<GpuImage>>>,
    imports: Option<Res<ImportedDmabufImages>>,
    mut memory: ResMut<FrameContentMemory>,
    mut content: ResMut<RenderFrameContent>,
) {
    let surfaces = extracted
        .0
        .iter()
        .map(|surface| {
            let gpu_ready = images
                .as_ref()
                .is_some_and(|images| images.get(surface.image).is_some());
            let progress = surface
                .dmabuf_requests
                .as_ref()
                .and_then(|_| imports.as_ref())
                .and_then(|imports| imports.progress(surface.image));
            (surface.clone(), gpu_ready, progress)
        })
        .collect::<Vec<_>>();
    content.0 = frame_content(
        &surfaces,
        sources.map(|sources| sources.0.clone()).unwrap_or_default(),
        &mut memory.0,
    );
}

/// What the frame samples for one surface: `Some(commit)` when its texture
/// is known to carry that commit's buffer.
fn sampled_commit(
    surface: &ExtractedFrameSurface,
    gpu_ready: bool,
    progress: Option<ImportProgress>,
) -> Option<u64> {
    if !gpu_ready {
        // Bevy removes a GPU image before preparing its replacement, so an
        // unprepared SHM texture is not drawn at all.
        return None;
    }
    match &surface.dmabuf_requests {
        // SHM: a prepared image holds the newest extracted buffer.
        None => Some(surface.commit),
        // DMA-BUF: the bridge swaps images in place and keeps the previous
        // one while a replacement is pending or after it failed, so only
        // the installed request names what is sampled.
        Some(requests) => {
            let installed = progress?.installed?;
            requests
                .iter()
                .find(|(request, _)| *request == installed)
                .map(|(_, commit)| *commit)
        }
    }
}

/// A DMA-BUF commit whose import failed for good: nothing is pending and the
/// installed request is not the latest one.
fn refused_commit(
    surface: &ExtractedFrameSurface,
    progress: Option<ImportProgress>,
) -> Option<u64> {
    let requests = surface.dmabuf_requests.as_ref()?;
    let progress = progress?;
    if progress.pending || progress.installed == Some(progress.latest) {
        return None;
    }
    requests
        .iter()
        .find(|(request, _)| *request == progress.latest)
        .map(|(_, commit)| *commit)
}

fn frame_content(
    surfaces: &[(ExtractedFrameSurface, bool, Option<ImportProgress>)],
    sources: Vec<crate::protocol::presentation::FrameSource>,
    memory: &mut HashMap<SurfaceId, SurfaceMemory>,
) -> FrameContent {
    let live = surfaces
        .iter()
        .map(|(surface, ..)| surface.id)
        .collect::<HashSet<_>>();
    memory.retain(|id, _| live.contains(id));
    let mut frame = FrameContent {
        surfaces: Vec::with_capacity(surfaces.len()),
        sources,
        refused: Vec::new(),
    };
    for (surface, gpu_ready, progress) in surfaces {
        let remembered = memory.entry(surface.id).or_default();
        if let Some(refused) = refused_commit(surface, *progress)
            && remembered.refused.is_none_or(|previous| previous < refused)
        {
            remembered.refused = Some(refused);
            frame.refused.push((surface.id, refused));
        }
        let sampled = sampled_commit(surface, *gpu_ready, *progress);
        if let Some(commit) = sampled {
            remembered.matched = Some(remembered.matched.map_or(commit, |old| old.max(commit)));
        }
        frame.surfaces.push(match (surface.visible, sampled) {
            (true, Some(commit)) => FrameSurface {
                id: surface.id,
                commit_seq: commit,
                shown: true,
            },
            // Hidden or off the output: nothing committed so far will be
            // shown as committed.
            (false, _) => FrameSurface {
                id: surface.id,
                commit_seq: surface.commit,
                shown: false,
            },
            // Visible but its newest content is not sampled yet: resolve
            // nothing new; newer commits keep waiting.
            (true, None) => FrameSurface {
                id: surface.id,
                commit_seq: remembered.matched.unwrap_or(0),
                shown: false,
            },
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
            dmabuf_requests: None,
        }
    }

    fn dmabuf(id: u64, commit: u64, requests: &[(u64, u64)]) -> ExtractedFrameSurface {
        ExtractedFrameSurface {
            dmabuf_requests: Some(requests.iter().copied().collect()),
            ..surface(id, commit, true)
        }
    }

    fn progress(latest: u64, installed: Option<u64>, pending: bool) -> Option<ImportProgress> {
        Some(ImportProgress {
            latest,
            installed,
            pending,
        })
    }

    fn shown(id: u64, commit_seq: u64, shown: bool) -> FrameSurface {
        FrameSurface {
            id: SurfaceId(id),
            commit_seq,
            shown,
        }
    }

    #[test]
    fn shown_needs_visibility_and_a_prepared_texture() {
        let mut memory = HashMap::new();
        let frame = frame_content(
            &[
                (surface(1, 3, true), true, None),
                (surface(2, 5, false), true, None),
                (surface(3, 7, true), false, None),
            ],
            Vec::new(),
            &mut memory,
        );
        assert_eq!(
            frame.surfaces,
            [shown(1, 3, true), shown(2, 5, false), shown(3, 0, false)]
        );
        assert!(frame.refused.is_empty());
    }

    #[test]
    fn an_unprepared_update_reports_the_last_sampled_commit() {
        let mut memory = HashMap::new();
        frame_content(
            &[(surface(1, 3, true), true, None)],
            Vec::new(),
            &mut memory,
        );
        let frame = frame_content(
            &[(surface(1, 4, true), false, None)],
            Vec::new(),
            &mut memory,
        );
        assert_eq!(frame.surfaces, [shown(1, 3, false)]);
        let frame = frame_content(
            &[(surface(1, 4, true), true, None)],
            Vec::new(),
            &mut memory,
        );
        assert_eq!(frame.surfaces, [shown(1, 4, true)]);
        // A surface that left the scene is forgotten.
        frame_content(&[], Vec::new(), &mut memory);
        assert!(memory.is_empty());
    }

    #[test]
    fn dmabuf_is_shown_only_with_its_own_request_installed() {
        let mut memory = HashMap::new();
        let requests = [(10, 1), (11, 2)];
        // Request 11 (commit 2) pending, request 10 (commit 1) still installed.
        let frame = frame_content(
            &[(dmabuf(1, 2, &requests), true, progress(11, Some(10), true))],
            Vec::new(),
            &mut memory,
        );
        assert_eq!(frame.surfaces, [shown(1, 1, true)]);
        // Nothing installed yet (first import pending): not shown, even
        // though the placeholder GpuImage exists.
        let frame = frame_content(
            &[(dmabuf(2, 1, &[(20, 1)]), true, progress(20, None, true))],
            Vec::new(),
            &mut memory,
        );
        assert_eq!(frame.surfaces, [shown(2, 0, false)]);
        // Installed: commit 2 is shown.
        let frame = frame_content(
            &[(dmabuf(1, 2, &requests), true, progress(11, Some(11), false))],
            Vec::new(),
            &mut memory,
        );
        assert_eq!(frame.surfaces, [shown(1, 2, true)]);
        assert!(frame.refused.is_empty());
    }

    #[test]
    fn a_failed_dmabuf_import_is_refused_once_and_never_shown() {
        let mut memory = HashMap::new();
        let requests = [(10, 1), (11, 2)];
        let failed = || (dmabuf(1, 2, &requests), true, progress(11, Some(10), false));
        let frame = frame_content(&[failed()], Vec::new(), &mut memory);
        assert_eq!(frame.surfaces, [shown(1, 1, true)]);
        assert_eq!(frame.refused, [(SurfaceId(1), 2)]);
        let frame = frame_content(&[failed()], Vec::new(), &mut memory);
        assert!(frame.refused.is_empty(), "reported once");
        // A failed first import leaves nothing installed: refused, not shown.
        let frame = frame_content(
            &[(dmabuf(2, 1, &[(20, 1)]), true, progress(20, None, false))],
            Vec::new(),
            &mut memory,
        );
        assert_eq!(frame.surfaces, [shown(2, 0, false)]);
        assert_eq!(frame.refused, [(SurfaceId(2), 1)]);
    }

    #[test]
    fn off_output_surfaces_are_not_on_the_output() {
        let layout = |x: f32, y: f32| crate::protocol::SurfaceLayout {
            x,
            y,
            width: 100.0,
            height: 50.0,
            z: Default::default(),
            source: None,
            parent: None,
            transform: crate::protocol::SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        };
        let canvas = Vec2::new(800.0, 600.0);
        assert!(on_output(&layout(0.0, 0.0), canvas));
        assert!(on_output(&layout(-99.0, 590.0), canvas), "one pixel in");
        assert!(!on_output(&layout(-100.0, 0.0), canvas));
        assert!(!on_output(&layout(800.0, 0.0), canvas));
        assert!(!on_output(&layout(0.0, 600.0), canvas));
        assert!(!on_output(&layout(10.0, -50.0), canvas));
    }
}
