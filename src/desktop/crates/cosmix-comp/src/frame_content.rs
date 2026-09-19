//! Which content each rendered frame contained, for `wp_presentation`.
//!
//! The render world records, per client surface, the newest commit whose
//! content the frame actually samples and whether the surface is shown,
//! plus the content sources. A backend attaches the record to a frame only
//! once it has proof that frame was presented, then reports it to the
//! protocol thread.
//!
//! "Shown" means: mapped and visible, on the output, and its texture is the
//! one this frame samples. Single-output reports exclude proven occlusion.
//! Multi-output presentation retains its existing shared-report semantics;
//! callback coverage is independently computed for every output.
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
        FramePresentationReporter, SurfaceId,
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
            .init_resource::<crate::occlusion::ExtractedCoverage>()
            .init_resource::<crate::occlusion::CoverageCache>()
            .add_systems(ExtractSchedule, crate::occlusion::extract)
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

/// A DMA-BUF commit whose import failed for good. `sampled` is the newest
/// commit the surface's frames sample (0 when none): every pending commit
/// in `(sampled, refused]` will never be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) id: SurfaceId,
    pub(crate) sampled: u64,
    pub(crate) refused: u64,
}

#[allow(clippy::too_many_arguments)]
fn resolve_frame_content(
    extracted: Res<ExtractedFrameSurfaces>,
    sources: Option<Res<ExtractedContentSources>>,
    images: Option<Res<RenderAssets<GpuImage>>>,
    imports: Option<Res<ImportedDmabufImages>>,
    reporter: Option<Res<FramePresentationReporter>>,
    mut memory: ResMut<FrameContentMemory>,
    mut content: ResMut<RenderFrameContent>,
    coverage: Res<crate::occlusion::ExtractedCoverage>,
    mut coverage_cache: ResMut<crate::occlusion::CoverageCache>,
    mut pipelines: Option<ResMut<bevy::render::render_resource::PipelineCache>>,
    assets: Option<Res<crate::render_asset_readiness::AssetPreparationStatus>>,
) {
    // One registry lock for every DMA-BUF surface in the frame.
    let dmabuf_images = extracted
        .0
        .iter()
        .filter(|surface| surface.dmabuf_requests.is_some())
        .map(|surface| surface.image)
        .collect::<Vec<_>>();
    let mut progress = match (&imports, dmabuf_images.is_empty()) {
        (Some(imports), false) => imports.progress_batch(&dmabuf_images).into_iter(),
        _ => Vec::new().into_iter(),
    };
    let surfaces = extracted
        .0
        .iter()
        .map(|surface| {
            let gpu_ready = images
                .as_ref()
                .is_some_and(|images| images.get(surface.image).is_some());
            let progress = if surface.dmabuf_requests.is_some() {
                progress.next().flatten()
            } else {
                None
            };
            (surface.clone(), gpu_ready, progress)
        })
        .collect::<Vec<_>>();
    if let Some(reporter) = reporter.as_ref() {
        let assets_ready = assets.as_ref().is_some_and(|assets| {
            let s = assets.snapshot();
            s.revision.is_some()
                && s.tracked_types > 0
                && s.pending_preparations == 0
                && s.pending_removals == 0
        });
        let ready = assets_ready
            && pipelines.as_mut().is_some_and(|pipelines| {
                let state = crate::render_pipeline_readiness::process_after_draw(pipelines);
                state.pipelines > 0
                    && state.pending == 0
                    && state.failed == 0
                    && !state.changed_after_draw
            });
        let sampled = surfaces
            .iter()
            .filter_map(|(surface, gpu, progress)| {
                ready
                    .then(|| sampled_commit(surface, *gpu, *progress))
                    .flatten()
                    .map(|seq| (surface.id, seq))
            })
            .collect();
        crate::occlusion::resolve(
            &coverage,
            &sampled,
            &reporter.occlusion,
            &mut coverage_cache,
        );
    }
    // Refusals do not wait for a presented frame: a frame that is never
    // reported must not strand them. They are marked delivered only once
    // sent (without a reporter nothing is advertised, so nothing waits).
    content.0 = frame_content(
        &surfaces,
        sources.map(|sources| sources.0.clone()).unwrap_or_default(),
        &mut memory.0,
        |refusal| {
            reporter.as_ref().is_some_and(|reporter| {
                reporter.commit_refused(refusal.id, Some(refusal.sampled), refusal.refused);
                true
            })
        },
    );
    if coverage_cache.result.revision == coverage.revision {
        apply_presentation_coverage(
            &mut content.0,
            &coverage_cache.result,
            // Include unproven outputs: absence of camera evidence cannot prove
            // this shared report covers only one output (0.61 conservative limit).
            coverage.scene.outputs.len(),
        );
    }
}

pub(crate) fn apply_presentation_coverage(
    content: &mut FrameContent,
    coverage: &crate::occlusion::CoverageSnapshot,
    outputs: usize,
) {
    if outputs != 1 {
        return;
    }
    for surface in &mut content.surfaces {
        if coverage.content.get(&surface.id) == Some(&crate::occlusion::TreeVisibility::Occluded) {
            surface.shown = false;
            surface.waiting = false;
        }
    }
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
        // Relies on the bridge's `installed` matching the GpuImage: if Bevy
        // re-prepared the uninitialised placeholder over an imported view
        // (see `set_client_image_linear` in compositor_scene.rs) while the
        // registry still says Applied, this reports the import as sampled.
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
/// installed request is not the latest one. A failed re-upload of the
/// content already installed (a rebuilt upsert) refuses nothing: that
/// commit is still on screen.
fn refused_commit(
    surface: &ExtractedFrameSurface,
    progress: Option<ImportProgress>,
) -> Option<u64> {
    let requests = surface.dmabuf_requests.as_ref()?;
    let progress = progress?;
    if progress.pending || progress.installed == Some(progress.latest) {
        return None;
    }
    let commit_of = |wanted: u64| {
        requests
            .iter()
            .find(|(request, _)| *request == wanted)
            .map(|(_, commit)| *commit)
    };
    let refused = commit_of(progress.latest)?;
    let installed = progress.installed.and_then(commit_of);
    installed
        .is_none_or(|installed| refused > installed)
        .then_some(refused)
}

fn frame_content(
    surfaces: &[(ExtractedFrameSurface, bool, Option<ImportProgress>)],
    sources: Vec<crate::protocol::presentation::FrameSource>,
    memory: &mut HashMap<SurfaceId, SurfaceMemory>,
    mut deliver_refusal: impl FnMut(Refusal) -> bool,
) -> FrameContent {
    let live = surfaces
        .iter()
        .map(|(surface, ..)| surface.id)
        .collect::<HashSet<_>>();
    memory.retain(|id, _| live.contains(id));
    let mut frame = FrameContent {
        surfaces: Vec::with_capacity(surfaces.len()),
        sources,
    };
    for (surface, gpu_ready, progress) in surfaces {
        let remembered = memory.entry(surface.id).or_default();
        let sampled = sampled_commit(surface, *gpu_ready, *progress);
        if let Some(commit) = sampled {
            remembered.matched = Some(remembered.matched.map_or(commit, |old| old.max(commit)));
        }
        if let Some(refused) = refused_commit(surface, *progress)
            && remembered.refused.is_none_or(|previous| previous < refused)
            && deliver_refusal(Refusal {
                id: surface.id,
                sampled: remembered.matched.unwrap_or(0),
                refused,
            })
        {
            remembered.refused = Some(refused);
        }
        frame.surfaces.push(match (surface.visible, sampled) {
            (true, Some(commit)) => FrameSurface {
                id: surface.id,
                commit_seq: commit,
                shown: true,
                waiting: false,
            },
            // Hidden or off the output: nothing committed so far will be
            // shown as committed.
            (false, _) => FrameSurface {
                id: surface.id,
                commit_seq: surface.commit,
                shown: false,
                waiting: false,
            },
            // Visible but its newest content is not sampled yet (an SHM
            // texture still preparing, a DMA-BUF request not installed or
            // aged out of the history): resolve nothing new; newer commits
            // keep waiting, and the stats see a stall, not a hide.
            (true, None) => FrameSurface {
                id: surface.id,
                commit_seq: remembered.matched.unwrap_or(0),
                shown: false,
                waiting: true,
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

    /// Visible, newest content not sampled yet.
    fn waiting(id: u64, commit_seq: u64) -> FrameSurface {
        FrameSurface {
            waiting: true,
            ..shown(id, commit_seq, false)
        }
    }

    fn shown(id: u64, commit_seq: u64, shown: bool) -> FrameSurface {
        FrameSurface {
            waiting: false,
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
            |_| true,
        );
        assert_eq!(
            frame.surfaces,
            [shown(1, 3, true), shown(2, 5, false), waiting(3, 0)]
        );
    }

    #[test]
    fn an_unprepared_update_reports_the_last_sampled_commit() {
        let mut memory = HashMap::new();
        frame_content(
            &[(surface(1, 3, true), true, None)],
            Vec::new(),
            &mut memory,
            |_| true,
        );
        let frame = frame_content(
            &[(surface(1, 4, true), false, None)],
            Vec::new(),
            &mut memory,
            |_| true,
        );
        assert_eq!(frame.surfaces, [waiting(1, 3)]);
        let frame = frame_content(
            &[(surface(1, 4, true), true, None)],
            Vec::new(),
            &mut memory,
            |_| true,
        );
        assert_eq!(frame.surfaces, [shown(1, 4, true)]);
        // A surface that left the scene is forgotten.
        frame_content(&[], Vec::new(), &mut memory, |_| true);
        assert!(memory.is_empty());
    }

    /// G2: a rebuilt upsert re-uploads the installed commit under a new
    /// request; if that import fails, the commit is still on screen.
    #[test]
    fn a_failed_re_upload_of_installed_content_refuses_nothing() {
        let mut memory = HashMap::new();
        let requests = [(10, 4), (11, 4)];
        let mut sent = Vec::new();
        let frame = frame_content(
            &[(dmabuf(1, 4, &requests), true, progress(11, Some(10), false))],
            Vec::new(),
            &mut memory,
            |refusal| {
                sent.push(refusal);
                true
            },
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(frame.surfaces, [shown(1, 4, true)]);
        // A newer commit that fails is still refused.
        let requests = [(10, 4), (11, 4), (12, 5)];
        frame_content(
            &[(dmabuf(1, 5, &requests), true, progress(12, Some(10), false))],
            Vec::new(),
            &mut memory,
            |refusal| {
                sent.push(refusal);
                true
            },
        );
        assert_eq!(sent, [refusal(1, 4, 5)]);
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
            |_| true,
        );
        assert_eq!(frame.surfaces, [shown(1, 1, true)]);
        // Nothing installed yet (first import pending): not shown, even
        // though the placeholder GpuImage exists.
        let frame = frame_content(
            &[(dmabuf(2, 1, &[(20, 1)]), true, progress(20, None, true))],
            Vec::new(),
            &mut memory,
            |_| true,
        );
        assert_eq!(frame.surfaces, [waiting(2, 0)]);
        // Installed: commit 2 is shown.
        let frame = frame_content(
            &[(dmabuf(1, 2, &requests), true, progress(11, Some(11), false))],
            Vec::new(),
            &mut memory,
            |_| true,
        );
        assert_eq!(frame.surfaces, [shown(1, 2, true)]);
    }

    fn refusal(id: u64, sampled: u64, refused: u64) -> Refusal {
        Refusal {
            id: SurfaceId(id),
            sampled,
            refused,
        }
    }

    #[test]
    fn a_failed_dmabuf_import_is_refused_once_and_never_shown() {
        let mut memory = HashMap::new();
        let requests = [(10, 1), (11, 2), (12, 3)];
        // Request 12 (commit 3) failed; 10 (commit 1) is still installed and
        // 11 (commit 2) was superseded while pending: (1, 3] is refused.
        let failed = || (dmabuf(1, 3, &requests), true, progress(12, Some(10), false));
        let mut sent = Vec::new();
        let frame = frame_content(&[failed()], Vec::new(), &mut memory, |refusal| {
            sent.push(refusal);
            true
        });
        assert_eq!(frame.surfaces, [shown(1, 1, true)]);
        assert_eq!(sent, [refusal(1, 1, 3)]);
        frame_content(&[failed()], Vec::new(), &mut memory, |refusal| {
            sent.push(refusal);
            true
        });
        assert_eq!(sent.len(), 1, "delivered once");
        // A failed first import leaves nothing installed: refused, not shown.
        let frame = frame_content(
            &[(dmabuf(2, 1, &[(20, 1)]), true, progress(20, None, false))],
            Vec::new(),
            &mut memory,
            |refusal| {
                sent.push(refusal);
                true
            },
        );
        assert_eq!(frame.surfaces, [waiting(2, 0)]);
        assert_eq!(sent.last(), Some(&refusal(2, 0, 1)));
    }

    /// NEW-1: a refusal that could not be delivered is offered again on the
    /// next frame, whatever happened to the first frame's report.
    #[test]
    fn an_undelivered_refusal_is_offered_again() {
        let mut memory = HashMap::new();
        let failed = (
            dmabuf(1, 2, &[(10, 1), (11, 2)]),
            true,
            progress(11, Some(10), false),
        );
        let mut offered = 0;
        frame_content(
            std::slice::from_ref(&failed),
            Vec::new(),
            &mut memory,
            |_| {
                offered += 1;
                false
            },
        );
        let mut sent = Vec::new();
        frame_content(
            std::slice::from_ref(&failed),
            Vec::new(),
            &mut memory,
            |refusal| {
                sent.push(refusal);
                true
            },
        );
        assert_eq!(offered, 1);
        assert_eq!(sent, [refusal(1, 1, 2)]);
        frame_content(&[failed], Vec::new(), &mut memory, |_| {
            panic!("delivered already")
        });
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
