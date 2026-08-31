//! Renderer-owned one-shot output capture service.
//!
//! Wire screencopy and the optional PNG policy share one bounded readback lane.
//! The render world owns staging allocation, copy submission and `map_async`;
//! mapped bytes are normalised and cropped on the named worker. No Wayland
//! object, scanout slot or Bevy Screenshot entity crosses this boundary.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[cfg(any(test, feature = "frame-capture"))]
use std::sync::atomic::AtomicUsize;

#[cfg(feature = "frame-capture")]
use bevy::asset::RenderAssetUsages;
use bevy::{
    prelude::*,
    render::{
        Render, RenderApp,
        camera::ExtractedCamera,
        extract_component::{ExtractComponent, ExtractComponentPlugin},
        render_resource::{
            BindGroupEntry, BindGroupLayout, BindGroupLayoutEntry, BindingResource, BindingType,
            BlendState, Buffer, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
            CommandEncoder, CommandEncoderDescriptor, Extent3d, LoadOp, MapMode, MultisampleState,
            Operations, Origin3d, PipelineCompilationOptions, PipelineLayoutDescriptor,
            PrimitiveState, RawFragmentState, RawRenderPipelineDescriptor, RawVertexState,
            RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline, Sampler,
            SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor, ShaderSource,
            ShaderStages, StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout,
            TexelCopyTextureInfo, TextureAspect, TextureDescriptor, TextureDimension,
            TextureFormat, TextureSampleType, TextureUsages, TextureView, TextureViewDimension,
        },
        renderer::{RenderDevice, RenderQueue, render_system},
        view::ViewTarget,
    },
    window::RequestRedraw,
};
use smithay::reexports::calloop::channel::Sender as CalloopSender;

use crate::{
    backend::CaptureSourceId,
    compositor_scene::CompositorSceneSet,
    protocol::{CaptureCompletionReporter, ClientSceneFeed},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CaptureId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureFormat {
    #[allow(dead_code)]
    Argb8888,
    Xrgb8888,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureRegion {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaptureLogicalRegion {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct CaptureRequest {
    pub(crate) id: CaptureId,
    pub(crate) source_id: CaptureSourceId,
    pub(crate) output_name: String,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    /// Displayed-orientation physical region.
    pub(crate) region: CaptureRegion,
    /// Global logical output rectangle advertised with this source.
    pub(crate) logical_rect: (i32, i32, u32, u32),
    /// Raw render target extent before output transform normalisation.
    pub(crate) source_storage_extent: (u32, u32),
    pub(crate) displayed_physical_extent: (u32, u32),
    pub(crate) scale120: u32,
    pub(crate) transform: smithay::utils::Transform,
    pub(crate) format: CaptureFormat,
    pub(crate) overlay_cursor: bool,
    pub(crate) cursor: Option<CaptureCursorSnapshot>,
    pub(crate) with_damage: bool,
    pub(crate) damage_baseline: Option<u64>,
    pub(crate) damage_revision: u64,
    pub(crate) damage: Vec<CaptureRegion>,
    pub(crate) cancellation: CaptureCancellation,
    pub(crate) reservation: CaptureReservationLease,
    pub(crate) deadline: Instant,
}

#[derive(Clone, Debug)]
pub(crate) struct CaptureCursorSnapshot {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Arc<Vec<u8>>,
    pub(crate) premultiplied: bool,
}

#[derive(Resource, Clone, Default)]
pub(crate) struct RetainedCaptureCursor(pub(crate) Option<CaptureCursorSnapshot>);

#[derive(Clone, Debug)]
pub(crate) struct CaptureDamageWatch {
    pub(crate) id: CaptureId,
    pub(crate) source_id: CaptureSourceId,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    pub(crate) region: CaptureRegion,
    pub(crate) logical_rect: (i32, i32, u32, u32),
    pub(crate) source_storage_extent: (u32, u32),
    pub(crate) displayed_physical_extent: (u32, u32),
    pub(crate) scale120: u32,
    pub(crate) transform: smithay::utils::Transform,
    pub(crate) overlay_cursor: bool,
    pub(crate) baseline: u64,
    pub(crate) cancellation: CaptureCancellation,
    pub(crate) deadline: Instant,
}

const MAX_DAMAGE_REVISIONS: usize = 128;
const MAX_DAMAGE_RECTS: usize = 64;

#[derive(Clone, Debug)]
struct DamageEntry {
    revision: u64,
    base: Vec<CaptureRegion>,
    cursor: Vec<CaptureRegion>,
}

#[derive(Clone, Debug)]
struct SourceDamageJournal {
    revision: u64,
    logical_rect: (i32, i32, u32, u32),
    storage_extent: (u32, u32),
    extent: (u32, u32),
    scale120: u32,
    transform: smithay::utils::Transform,
    entries: VecDeque<DamageEntry>,
}

#[derive(Resource, Clone, Default)]
pub(crate) struct OutputDamageJournal(Arc<Mutex<BTreeMap<CaptureSourceId, SourceDamageJournal>>>);

impl OutputDamageJournal {
    pub(crate) fn register(
        &self,
        source: CaptureSourceId,
        logical_rect: (i32, i32, u32, u32),
        storage_extent: (u32, u32),
        extent: (u32, u32),
        scale120: u32,
        transform: smithay::utils::Transform,
    ) {
        let mut journals = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let CaptureSourceId::Kms { key, generation } = &source {
            journals.retain(|candidate, _| {
                !matches!(candidate, CaptureSourceId::Kms { key: candidate_key, generation: candidate_generation }
                    if candidate_key == key && candidate_generation != generation)
            });
        }
        match journals.get_mut(&source) {
            Some(journal)
                if journal.logical_rect == logical_rect
                    && journal.storage_extent == storage_extent
                    && journal.extent == extent
                    && journal.scale120 == scale120
                    && journal.transform == transform => {}
            Some(journal) => {
                journal.logical_rect = logical_rect;
                journal.storage_extent = storage_extent;
                journal.extent = extent;
                journal.scale120 = scale120;
                journal.transform = transform;
                journal.revision = journal.revision.wrapping_add(1).max(1);
                journal.entries.clear();
                journal.entries.push_back(DamageEntry {
                    revision: journal.revision,
                    base: vec![full_region(extent)],
                    cursor: Vec::new(),
                });
            }
            None => {
                journals.insert(
                    source,
                    SourceDamageJournal {
                        revision: 0,
                        logical_rect,
                        storage_extent,
                        extent,
                        scale120,
                        transform,
                        entries: VecDeque::new(),
                    },
                );
            }
        }
    }

    pub(crate) fn retire_kms_sources(&self) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|source, _| !matches!(source, CaptureSourceId::Kms { .. }));
    }

    pub(crate) fn mark_nested_base_full(&self) {
        self.mark_sources(
            true,
            |source, _| matches!(source, CaptureSourceId::Nested { .. }),
            &[],
            true,
        );
    }

    pub(crate) fn mark_all_base_full(&self) {
        self.mark_all(true, None);
    }

    #[cfg(test)]
    pub(crate) fn mark_all_base_regions(&self, rectangles: &[CaptureRegion]) {
        self.mark_all_regions(true, rectangles);
    }

    #[cfg(test)]
    pub(crate) fn mark_all_cursor_regions(&self, rectangles: &[CaptureRegion]) {
        self.mark_all_regions(false, rectangles);
    }

    pub(crate) fn mark_base_logical_regions(&self, rectangles: &[CaptureLogicalRegion]) {
        self.mark_logical_regions(true, rectangles);
    }

    pub(crate) fn mark_cursor_logical_regions(&self, rectangles: &[CaptureLogicalRegion]) {
        self.mark_logical_regions(false, rectangles);
    }

    fn mark_logical_regions(&self, base: bool, rectangles: &[CaptureLogicalRegion]) {
        let mut journals = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for journal in journals.values_mut() {
            let (output_x, output_y, output_width, output_height) = journal.logical_rect;
            let output_left = output_x as f64;
            let output_top = output_y as f64;
            let output_right = output_left + f64::from(output_width);
            let output_bottom = output_top + f64::from(output_height);
            let mut damage = rectangles
                .iter()
                .filter_map(|rectangle| {
                    if rectangle.width <= 0.0 || rectangle.height <= 0.0 {
                        return None;
                    }
                    let left = f64::from(rectangle.x).max(output_left);
                    let top = f64::from(rectangle.y).max(output_top);
                    let right = f64::from(rectangle.x + rectangle.width).min(output_right);
                    let bottom = f64::from(rectangle.y + rectangle.height).min(output_bottom);
                    if right <= left || bottom <= top {
                        return None;
                    }
                    let project = |edge: f64| -> Option<u32> {
                        let scaled = edge * f64::from(journal.scale120) / 120.0;
                        let rounded = if scaled.is_sign_negative() {
                            (scaled - 0.5).ceil()
                        } else {
                            (scaled + 0.5).floor()
                        };
                        u32::try_from(rounded as i64).ok()
                    };
                    let left = project(left - output_left)?;
                    let top = project(top - output_top)?;
                    let right = project(right - output_left)?;
                    let bottom = project(bottom - output_top)?;
                    Some(CaptureRegion {
                        x: left,
                        y: top,
                        width: right.checked_sub(left)?,
                        height: bottom.checked_sub(top)?,
                    })
                })
                .filter_map(|rectangle| intersect_damage(rectangle, full_region(journal.extent)))
                .collect::<Vec<_>>();
            coalesce_damage(&mut damage, journal.extent);
            record_damage(journal, base, damage);
        }
    }

    fn mark_all(&self, base: bool, rectangle: Option<CaptureRegion>) {
        let rectangles = rectangle.into_iter().collect::<Vec<_>>();
        self.mark_all_inner(base, &rectangles, rectangle.is_none());
    }

    #[cfg(test)]
    fn mark_all_regions(&self, base: bool, rectangles: &[CaptureRegion]) {
        self.mark_all_inner(base, rectangles, false);
    }

    fn mark_all_inner(&self, base: bool, rectangles: &[CaptureRegion], full: bool) {
        self.mark_sources(base, |_, _| true, rectangles, full);
    }

    fn mark_sources(
        &self,
        base: bool,
        include: impl Fn(&CaptureSourceId, &SourceDamageJournal) -> bool,
        rectangles: &[CaptureRegion],
        full: bool,
    ) {
        let mut journals = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (source, journal) in journals.iter_mut() {
            if !include(source, journal) {
                continue;
            }
            let mut damage = if full {
                vec![full_region(journal.extent)]
            } else {
                rectangles
                    .iter()
                    .filter_map(|rectangle| {
                        intersect_damage(*rectangle, full_region(journal.storage_extent))
                    })
                    .filter_map(|rectangle| {
                        transform_damage_region(
                            rectangle,
                            journal.storage_extent,
                            journal.transform,
                        )
                    })
                    .filter_map(|rectangle| {
                        intersect_damage(rectangle, full_region(journal.extent))
                    })
                    .collect::<Vec<_>>()
            };
            coalesce_damage(&mut damage, journal.extent);
            record_damage(journal, base, damage);
        }
    }

    pub(crate) fn snapshot(
        &self,
        source: &CaptureSourceId,
        baseline: Option<u64>,
        include_cursor: bool,
        capture: CaptureRegion,
    ) -> (u64, Vec<CaptureRegion>) {
        let journals = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(journal) = journals.get(source) else {
            return (
                0,
                baseline
                    .is_none()
                    .then_some(full_region((capture.width, capture.height)))
                    .into_iter()
                    .collect(),
            );
        };
        if baseline.is_none() {
            return (
                journal.revision,
                vec![full_region((capture.width, capture.height))],
            );
        }
        let baseline = baseline.unwrap_or_default();
        let history_lost = journal
            .entries
            .front()
            .is_some_and(|entry| baseline < entry.revision.saturating_sub(1));
        if history_lost {
            return (
                journal.revision,
                vec![full_region((capture.width, capture.height))],
            );
        }
        let mut damage = Vec::new();
        for entry in journal
            .entries
            .iter()
            .filter(|entry| entry.revision > baseline)
        {
            for rectangle in entry.base.iter().chain(
                include_cursor
                    .then_some(&entry.cursor)
                    .into_iter()
                    .flatten(),
            ) {
                if let Some(intersection) = intersect_damage(*rectangle, capture) {
                    damage.push(intersection);
                }
            }
        }
        coalesce_damage(&mut damage, (capture.width, capture.height));
        (journal.revision, damage)
    }
}

fn record_damage(journal: &mut SourceDamageJournal, base: bool, damage: Vec<CaptureRegion>) {
    if damage.is_empty() {
        return;
    }
    journal.revision = journal.revision.wrapping_add(1).max(1);
    let entry = DamageEntry {
        revision: journal.revision,
        base: if base { damage.clone() } else { Vec::new() },
        cursor: if base { Vec::new() } else { damage },
    };
    journal.entries.push_back(entry);
    while journal.entries.len() > MAX_DAMAGE_REVISIONS {
        journal.entries.pop_front();
        if let Some(oldest) = journal.entries.front_mut() {
            oldest.base = vec![full_region(journal.extent)];
            oldest.cursor.clear();
        }
    }
}

fn full_region(extent: (u32, u32)) -> CaptureRegion {
    CaptureRegion {
        x: 0,
        y: 0,
        width: extent.0,
        height: extent.1,
    }
}

fn intersect_damage(damage: CaptureRegion, capture: CaptureRegion) -> Option<CaptureRegion> {
    let left = damage.x.max(capture.x);
    let top = damage.y.max(capture.y);
    let right = damage
        .x
        .checked_add(damage.width)?
        .min(capture.x.checked_add(capture.width)?);
    let bottom = damage
        .y
        .checked_add(damage.height)?
        .min(capture.y.checked_add(capture.height)?);
    (right > left && bottom > top).then_some(CaptureRegion {
        x: left - capture.x,
        y: top - capture.y,
        width: right - left,
        height: bottom - top,
    })
}

fn transform_damage_region(
    rectangle: CaptureRegion,
    storage: (u32, u32),
    transform: smithay::utils::Transform,
) -> Option<CaptureRegion> {
    let right = rectangle.x.checked_add(rectangle.width)?;
    let bottom = rectangle.y.checked_add(rectangle.height)?;
    if right > storage.0 || bottom > storage.1 {
        return None;
    }
    let (x, y, width, height) = match transform {
        smithay::utils::Transform::Normal => {
            (rectangle.x, rectangle.y, rectangle.width, rectangle.height)
        }
        smithay::utils::Transform::_90 => (
            storage.1.checked_sub(bottom)?,
            rectangle.x,
            rectangle.height,
            rectangle.width,
        ),
        smithay::utils::Transform::_180 => (
            storage.0.checked_sub(right)?,
            storage.1.checked_sub(bottom)?,
            rectangle.width,
            rectangle.height,
        ),
        smithay::utils::Transform::_270 => (
            rectangle.y,
            storage.0.checked_sub(right)?,
            rectangle.height,
            rectangle.width,
        ),
        smithay::utils::Transform::Flipped => (
            storage.0.checked_sub(right)?,
            rectangle.y,
            rectangle.width,
            rectangle.height,
        ),
        smithay::utils::Transform::Flipped90 => (
            storage.1.checked_sub(bottom)?,
            storage.0.checked_sub(right)?,
            rectangle.height,
            rectangle.width,
        ),
        smithay::utils::Transform::Flipped180 => (
            rectangle.x,
            storage.1.checked_sub(bottom)?,
            rectangle.width,
            rectangle.height,
        ),
        smithay::utils::Transform::Flipped270 => {
            (rectangle.y, rectangle.x, rectangle.height, rectangle.width)
        }
    };
    Some(CaptureRegion {
        x,
        y,
        width,
        height,
    })
}

fn coalesce_damage(damage: &mut Vec<CaptureRegion>, extent: (u32, u32)) {
    damage.sort_by_key(|rectangle| (rectangle.y, rectangle.x, rectangle.height, rectangle.width));
    damage.dedup();
    let mut index = 0;
    while index < damage.len() {
        let mut candidate = index + 1;
        while candidate < damage.len() {
            if let Some(union) = overlapping_union(damage[index], damage[candidate]) {
                damage[index] = union;
                damage.swap_remove(candidate);
                candidate = index + 1;
            } else {
                candidate += 1;
            }
        }
        index += 1;
    }
    damage.sort_by_key(|rectangle| (rectangle.y, rectangle.x, rectangle.height, rectangle.width));
    if damage.len() > MAX_DAMAGE_RECTS {
        damage.clear();
        damage.push(full_region(extent));
    }
}

fn overlapping_union(left: CaptureRegion, right: CaptureRegion) -> Option<CaptureRegion> {
    let left_right = left.x.checked_add(left.width)?;
    let left_bottom = left.y.checked_add(left.height)?;
    let right_right = right.x.checked_add(right.width)?;
    let right_bottom = right.y.checked_add(right.height)?;
    if left_right < right.x
        || right_right < left.x
        || left_bottom < right.y
        || right_bottom < left.y
    {
        return None;
    }
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let far_x = left_right.max(right_right);
    let far_y = left_bottom.max(right_bottom);
    Some(CaptureRegion {
        x,
        y,
        width: far_x - x,
        height: far_y - y,
    })
}

#[derive(Clone, Default)]
pub(crate) struct CaptureCancellation(Arc<AtomicBool>);

impl CaptureCancellation {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl fmt::Debug for CaptureCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

struct CaptureReservationInner {
    id: CaptureId,
    release: Option<CalloopSender<CaptureId>>,
    #[cfg(test)]
    release_counter: Option<Arc<AtomicUsize>>,
}

impl Drop for CaptureReservationInner {
    fn drop(&mut self) {
        if let Some(release) = &self.release {
            let _ = release.send(self.id);
        }
        #[cfg(test)]
        if let Some(releases) = &self.release_counter {
            releases.fetch_add(1, Ordering::AcqRel);
        }
    }
}

#[derive(Clone)]
pub(crate) struct CaptureReservationLease(Arc<CaptureReservationInner>);

impl CaptureReservationLease {
    pub(crate) fn new(id: CaptureId, release: CalloopSender<CaptureId>) -> Self {
        Self(Arc::new(CaptureReservationInner {
            id,
            release: Some(release),
            #[cfg(test)]
            release_counter: None,
        }))
    }

    #[cfg(test)]
    pub(crate) fn detached(id: CaptureId) -> Self {
        Self(Arc::new(CaptureReservationInner {
            id,
            release: None,
            release_counter: None,
        }))
    }

    #[cfg(test)]
    fn counted(id: CaptureId, releases: Arc<AtomicUsize>) -> Self {
        Self(Arc::new(CaptureReservationInner {
            id,
            release: None,
            release_counter: Some(releases),
        }))
    }
}

impl fmt::Debug for CaptureReservationLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureReservationLease")
            .field("id", &self.0.id)
            .field("holders", &Arc::strong_count(&self.0))
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CapturePixels {
    pub(crate) id: CaptureId,
    pub(crate) source_id: CaptureSourceId,
    pub(crate) frame_token: u64,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: CaptureFormat,
    pub(crate) y_invert: bool,
    pub(crate) damage_revision: u64,
    pub(crate) damage: Vec<CaptureRegion>,
    /// Packed little-endian ARGB/XRGB memory order: B, G, R, A/X.
    pub(crate) packed_bgra: Arc<Vec<u8>>,
    pub(crate) _reservation: CaptureReservationLease,
}

#[derive(Clone, Debug)]
pub(crate) struct CapturePresented {
    pub(crate) id: CaptureId,
    pub(crate) source_id: CaptureSourceId,
    pub(crate) frame_token: u64,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    pub(crate) seconds: u64,
    pub(crate) nanoseconds: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingCapturePresentation {
    pub(crate) id: CaptureId,
    pub(crate) source_id: CaptureSourceId,
    pub(crate) frame_token: u64,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    pub(crate) deadline: Instant,
}

#[derive(Resource, Clone, Default)]
pub(crate) struct CapturePresentationPending(Arc<Mutex<Vec<PendingCapturePresentation>>>);

impl CapturePresentationPending {
    pub(crate) fn publish(
        &self,
        presentations: impl IntoIterator<Item = PendingCapturePresentation>,
    ) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(presentations);
    }

    pub(crate) fn take(&self) -> Vec<PendingCapturePresentation> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_iter()
        .filter(|pending| pending.deadline > Instant::now())
        .collect()
    }
}

/// Stable source identity attached to a compositor output camera and extracted
/// to the render world. The KMS generation is part of the identity.
#[derive(Component, ExtractComponent, Clone, Debug, Eq, PartialEq)]
pub(crate) struct CaptureOutputSource {
    pub(crate) source_id: CaptureSourceId,
    pub(crate) output_name: String,
}

/// A cursor-only camera rendered into an independent transparent target. It
/// never shares Bevy's ping-pong main textures with the scene camera, so the
/// base output remains structurally available until capture has copied it.
#[derive(Component, ExtractComponent, Clone, Debug, Eq, PartialEq)]
pub(crate) struct CaptureCursorOverlaySource {
    pub(crate) source_id: CaptureSourceId,
}

#[derive(Resource, Default)]
pub(crate) struct CaptureQueue(Vec<CaptureRequest>);

impl CaptureQueue {
    pub(crate) fn push(&mut self, request: CaptureRequest) {
        if !request.cancellation.is_cancelled() {
            self.0.push(request);
        }
    }
}

#[derive(Resource, Clone, Default)]
struct CaptureReadbackBatch(Arc<Mutex<Vec<CaptureRequest>>>);

#[derive(Default)]
struct CaptureReadbackGroup {
    requests: Vec<CaptureRequest>,
    #[cfg(feature = "frame-capture")]
    png: Vec<PngCaptureAdmission>,
}

#[derive(Resource, Default)]
struct CaptureDamageWatches(Vec<CaptureRequest>);

#[derive(Resource, Default)]
pub(crate) struct CaptureDamageEligibilityWatches(pub(crate) Vec<CaptureDamageWatch>);

#[derive(Resource)]
struct CaptureRendererAvailable(bool);

#[derive(Resource, Clone, Default)]
struct CaptureFrameTokens(Arc<AtomicU64>);

impl CaptureFrameTokens {
    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel).wrapping_add(1).max(1)
    }
}

struct CaptureReadbackJob {
    buffer: Buffer,
    mapped: Receiver<Result<(), String>>,
    device: RenderDevice,
    row_pitch: usize,
    source_extent: (u32, u32),
    source_format: TextureFormat,
    transform: smithay::utils::Transform,
    #[cfg(feature = "frame-capture")]
    displayed_extent: (u32, u32),
    frame_token: u64,
    requests: Vec<CaptureRequest>,
    #[cfg(feature = "frame-capture")]
    png_requests: Vec<PngCaptureAdmission>,
    reporter: Option<CaptureCompletionReporter>,
}

#[derive(Resource)]
struct CaptureCursorCompositePipeline {
    layout: BindGroupLayout,
    sampler: Sampler,
    pipelines: Vec<(TextureFormat, RenderPipeline)>,
}

impl FromWorld for CaptureCursorCompositePipeline {
    fn from_world(world: &mut World) -> Self {
        let device = world.resource::<RenderDevice>();
        let layout = device.create_bind_group_layout(
            "CosMix capture cursor overlay layout",
            &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        );
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("CosMix capture cursor overlay sampler"),
            ..Default::default()
        });
        let shader = device.create_and_validate_shader_module(ShaderModuleDescriptor {
            label: Some("CosMix capture cursor overlay shader"),
            source: ShaderSource::Wgsl(Cow::Borrowed(
                r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vertex(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var output: VertexOutput;
    let position = positions[vertex_index];
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.uv = vec2<f32>((position.x + 1.0) * 0.5, (1.0 - position.y) * 0.5);
    return output;
}

@group(0) @binding(0) var overlay_texture: texture_2d<f32>;
@group(0) @binding(1) var overlay_sampler: sampler;

@fragment
fn fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(overlay_texture, overlay_sampler, input.uv);
}
"#,
            )),
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("CosMix capture cursor overlay pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let compilation_options = PipelineCompilationOptions::default();
        let pipelines = [
            TextureFormat::Rgba8Unorm,
            TextureFormat::Rgba8UnormSrgb,
            TextureFormat::Bgra8Unorm,
            TextureFormat::Bgra8UnormSrgb,
        ]
        .into_iter()
        .map(|format| {
            let targets = [Some(ColorTargetState {
                format,
                blend: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: ColorWrites::ALL,
            })];
            let pipeline = device.create_render_pipeline(&RawRenderPipelineDescriptor {
                label: Some("CosMix capture cursor overlay pipeline"),
                layout: Some(&pipeline_layout),
                vertex: RawVertexState {
                    module: &shader,
                    entry_point: Some("vertex"),
                    compilation_options: compilation_options.clone(),
                    buffers: &[],
                },
                primitive: PrimitiveState::default(),
                depth_stencil: None,
                multisample: MultisampleState::default(),
                fragment: Some(RawFragmentState {
                    module: &shader,
                    entry_point: Some("fragment"),
                    compilation_options: compilation_options.clone(),
                    targets: &targets,
                }),
                multiview_mask: None,
                cache: None,
            });
            (format, pipeline)
        })
        .collect();
        Self {
            layout,
            sampler,
            pipelines,
        }
    }
}

impl CaptureCursorCompositePipeline {
    fn pipeline(&self, format: TextureFormat) -> Option<&RenderPipeline> {
        self.pipelines
            .iter()
            .find_map(|(candidate, pipeline)| (*candidate == format).then_some(pipeline))
    }
}

#[derive(Resource)]
struct CaptureReadbackWorker {
    sender: Option<SyncSender<CaptureReadbackJob>>,
    stop: Arc<AtomicBool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Default for CaptureReadbackWorker {
    fn default() -> Self {
        let (sender, receiver) = mpsc::sync_channel::<CaptureReadbackJob>(8);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("cosmix-capture-readback".into())
            .spawn(move || {
                let mut active = Vec::<JoinHandle<()>>::new();
                while !worker_stop.load(Ordering::Acquire) {
                    let mut index = 0;
                    while index < active.len() {
                        if active[index].is_finished() {
                            let completed = active.swap_remove(index);
                            let _ = completed.join();
                        } else {
                            index += 1;
                        }
                    }
                    if active.len() >= 8 {
                        std::thread::park_timeout(Duration::from_millis(1));
                        continue;
                    }
                    match receiver.recv_timeout(Duration::from_millis(1)) {
                        Ok(job) => {
                            let job_stop = Arc::clone(&worker_stop);
                            let thread = std::thread::Builder::new()
                                .name("cosmix-capture-map".into())
                                .spawn(move || complete_readback(job, &job_stop))
                                .expect("spawn bounded capture map task");
                            active.push(thread);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                for thread in active {
                    let _ = thread.join();
                }
            })
            .expect("spawn bounded capture readback worker");
        Self {
            sender: Some(sender),
            stop,
            join: Mutex::new(Some(join)),
        }
    }
}

impl Drop for CaptureReadbackWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender.take();
        if let Some(join) = self
            .join
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = join.join();
        }
    }
}

pub(crate) struct CaptureServicePlugin;

#[derive(SystemSet, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CaptureRenderSet;

impl Plugin for CaptureServicePlugin {
    fn build(&self, app: &mut App) {
        let renderer_available = app.get_sub_app(RenderApp).is_some();
        let batches = CaptureReadbackBatch::default();
        let tokens = CaptureFrameTokens::default();
        let reporter = CaptureReporterBridge::default();
        let damage = OutputDamageJournal::default();
        app.init_resource::<CaptureQueue>()
            .init_resource::<CaptureDamageWatches>()
            .init_resource::<CaptureDamageEligibilityWatches>()
            .init_resource::<CapturePresentationPending>()
            .init_resource::<RetainedCaptureCursor>()
            .insert_resource(CaptureRendererAvailable(renderer_available))
            .insert_resource(batches.clone())
            .insert_resource(tokens.clone())
            .insert_resource(reporter.clone())
            .insert_resource(damage.clone())
            .add_plugins(ExtractComponentPlugin::<CaptureOutputSource>::default())
            .add_plugins(ExtractComponentPlugin::<CaptureCursorOverlaySource>::default())
            .add_systems(
                First,
                (evaluate_damage_eligibility, schedule_capture_requests)
                    .chain()
                    .after(CompositorSceneSet),
            );
        #[cfg(feature = "frame-capture")]
        let png = PngCaptureService::default();
        #[cfg(feature = "frame-capture")]
        app.insert_resource(png.clone());

        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .insert_resource(batches)
                .insert_resource(tokens)
                .insert_resource(CaptureReadbackWorker::default())
                .insert_resource(reporter)
                .insert_resource(damage);
            #[cfg(feature = "frame-capture")]
            render_app.insert_resource(png);
            render_app.add_systems(
                Render,
                capture_output_frames
                    .in_set(CaptureRenderSet)
                    .after(render_system),
            );
        }
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.init_resource::<CaptureCursorCompositePipeline>();
        }
    }
}

fn evaluate_damage_eligibility(
    mut watches: ResMut<CaptureDamageEligibilityWatches>,
    damage: Res<OutputDamageJournal>,
    reporter: Res<CaptureReporterBridge>,
) {
    let Some(reporter) = reporter.reporter() else {
        return;
    };
    let now = Instant::now();
    watches.0.retain(|watch| {
        if watch.cancellation.is_cancelled() || watch.deadline <= now {
            return false;
        }
        damage.register(
            watch.source_id.clone(),
            watch.logical_rect,
            watch.source_storage_extent,
            watch.displayed_physical_extent,
            watch.scale120,
            watch.transform,
        );
        let (revision, rectangles) = damage.snapshot(
            &watch.source_id,
            Some(watch.baseline),
            watch.overlay_cursor,
            watch.region,
        );
        if rectangles.is_empty() {
            return true;
        }
        reporter.damage_eligible(
            watch.id,
            watch.generation,
            watch.security_epoch,
            revision,
            rectangles,
        );
        false
    });
}

#[derive(Resource, Clone, Default)]
pub(crate) struct CaptureReporterBridge(Arc<Mutex<Option<CaptureCompletionReporter>>>);

impl CaptureReporterBridge {
    pub(crate) fn reporter(&self) -> Option<CaptureCompletionReporter> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[allow(clippy::too_many_arguments)]
fn schedule_capture_requests(
    mut queue: ResMut<CaptureQueue>,
    mut watches: ResMut<CaptureDamageWatches>,
    batches: Res<CaptureReadbackBatch>,
    damage: Res<OutputDamageJournal>,
    renderer: Res<CaptureRendererAvailable>,
    reporter: Option<Res<CaptureCompletionReporter>>,
    feed: Option<Res<ClientSceneFeed>>,
    bridge: Res<CaptureReporterBridge>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let reporter = reporter
        .map(|reporter| reporter.clone())
        .or_else(|| feed.as_ref().map(|feed| feed.capture_completion_reporter()));
    *bridge
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = reporter.clone();
    let now = Instant::now();
    watches.0.extend(std::mem::take(&mut queue.0));
    let mut admitted = Vec::new();
    let mut waiting = Vec::new();
    for mut request in std::mem::take(&mut watches.0) {
        if request.cancellation.is_cancelled() {
            continue;
        }
        if request.deadline <= now {
            if let Some(reporter) = &reporter {
                reporter.failed(request.id, request.generation, request.security_epoch);
            }
            continue;
        }
        if !renderer.0 {
            if let Some(reporter) = &reporter {
                reporter.failed(request.id, request.generation, request.security_epoch);
            }
            continue;
        }
        damage.register(
            request.source_id.clone(),
            request.logical_rect,
            request.source_storage_extent,
            request.displayed_physical_extent,
            request.scale120,
            request.transform,
        );
        let (revision, rectangles) = damage.snapshot(
            &request.source_id,
            request.damage_baseline,
            request.overlay_cursor,
            request.region,
        );
        request.damage_revision = revision;
        request.damage = if request.with_damage {
            rectangles
        } else {
            Vec::new()
        };
        if request.with_damage && request.damage.is_empty() {
            waiting.push(request);
            continue;
        }
        admitted.push(request);
    }
    watches.0 = waiting;
    if admitted.is_empty() {
        return;
    }
    if admitted
        .iter()
        .any(|request| matches!(request.source_id, CaptureSourceId::Nested { .. }))
    {
        redraw.write(RequestRedraw);
    }
    batches
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .extend(admitted);
}

fn encode_cursor_overlay(
    encoder: &mut CommandEncoder,
    device: &RenderDevice,
    composite: &CaptureCursorCompositePipeline,
    overlay: &TextureView,
    destination: &TextureView,
    destination_format: TextureFormat,
) -> bool {
    let Some(pipeline) = composite.pipeline(destination_format) else {
        return false;
    };
    let bind_group = device.create_bind_group(
        "CosMix capture cursor overlay bind group",
        &composite.layout,
        &[
            BindGroupEntry {
                binding: 0,
                resource: BindingResource::TextureView(overlay),
            },
            BindGroupEntry {
                binding: 1,
                resource: BindingResource::Sampler(&composite.sampler),
            },
        ],
    );
    let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
        label: Some("CosMix capture cursor overlay composite"),
        color_attachments: &[Some(RenderPassColorAttachment {
            view: destination,
            depth_slice: None,
            resolve_target: None,
            ops: Operations {
                load: LoadOp::Load,
                store: StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, &bind_group, &[]);
    pass.draw(0..3, 0..1);
    true
}

#[allow(clippy::too_many_arguments)]
fn capture_output_frames(
    batches: Res<CaptureReadbackBatch>,
    tokens: Res<CaptureFrameTokens>,
    worker: Res<CaptureReadbackWorker>,
    reporter: Res<CaptureReporterBridge>,
    pending_nested: Option<Res<CapturePresentationPending>>,
    mut kms_targets: Option<ResMut<crate::backend::render::KmsRenderTargets>>,
    damage: Res<OutputDamageJournal>,
    #[cfg(feature = "frame-capture")] png_service: Res<PngCaptureService>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    composite: Res<CaptureCursorCompositePipeline>,
    views: Query<(&ExtractedCamera, &ViewTarget, Option<&CaptureOutputSource>)>,
    overlay_views: Query<(&ViewTarget, &CaptureCursorOverlaySource)>,
) {
    let reporter = reporter.reporter();
    let requests = std::mem::take(
        &mut *batches
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    let mut groups = BTreeMap::<(CaptureSourceId, bool), CaptureReadbackGroup>::new();
    for request in requests {
        groups
            .entry((request.source_id.clone(), request.overlay_cursor))
            .or_default()
            .requests
            .push(request);
    }

    #[cfg(feature = "frame-capture")]
    for admission in std::mem::take(
        &mut *png_service
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    ) {
        let source = match &admission.request.target {
            PngCaptureTarget::Nested => views.iter().find_map(|(_, _, source)| {
                source.and_then(|source| {
                    matches!(source.source_id, CaptureSourceId::Nested { .. })
                        .then(|| (source.source_id.clone(), false))
                })
            }),
            PngCaptureTarget::Kms { source_id, .. } => Some((source_id.clone(), true)),
        };
        if let Some(source) = source {
            groups.entry(source).or_default().png.push(admission);
        } else {
            admission.complete();
        }
    }
    let mut kms_composited = BTreeSet::new();

    for ((source_id, cursor_inclusive), mut group) in groups {
        let Some((_camera, target, _)) = views.iter().find(|(_, _, source)| {
            source.is_some_and(|source| {
                source.source_id == source_id
                    && group
                        .requests
                        .iter()
                        .all(|request| request.output_name == source.output_name)
                    && {
                        #[cfg(feature = "frame-capture")]
                        {
                            group
                                .png
                                .iter()
                                .all(|request| match &request.request.target {
                                    PngCaptureTarget::Nested => true,
                                    PngCaptureTarget::Kms { output_name, .. } => {
                                        output_name == &source.output_name
                                    }
                                })
                        }
                        #[cfg(not(feature = "frame-capture"))]
                        {
                            true
                        }
                    }
            })
        }) else {
            fail_readback_group(&group, reporter.as_ref());
            continue;
        };
        let frame_token = match &source_id {
            CaptureSourceId::Nested { .. } => Some(tokens.next()),
            CaptureSourceId::Kms { .. } => kms_targets
                .as_deref()
                .and_then(|targets| targets.capture_frame_token(&source_id)),
        };
        let Some(frame_token) = frame_token else {
            fail_readback_group(&group, reporter.as_ref());
            continue;
        };
        for request in &mut group.requests {
            let (revision, rectangles) = damage.snapshot(
                &request.source_id,
                request.damage_baseline,
                request.overlay_cursor,
                request.region,
            );
            request.damage_revision = revision;
            if request.with_damage {
                request.damage = rectangles;
            }
        }
        // The scene camera's final output is the cursor-free source for both
        // nested and KMS. Cursor cameras render to independent transparent
        // targets and therefore cannot clear or swap this texture.
        let Some(scene_texture) = target.out_texture() else {
            fail_readback_group(&group, reporter.as_ref());
            continue;
        };
        let texture_size = scene_texture.texture().size();
        let texture_size = (texture_size.width, texture_size.height);
        let Some(source_format) = target.out_texture_view_format() else {
            fail_readback_group(&group, reporter.as_ref());
            continue;
        };
        let source_extent = group
            .requests
            .first()
            .map_or(texture_size, |request| request.source_storage_extent);
        let transform = group
            .requests
            .first()
            .map_or(smithay::utils::Transform::Normal, |request| {
                request.transform
            });
        #[cfg(feature = "frame-capture")]
        let displayed_extent = group
            .requests
            .first()
            .map_or(source_extent, |request| request.displayed_physical_extent);
        if !matches!(
            source_format,
            TextureFormat::Rgba8Unorm
                | TextureFormat::Rgba8UnormSrgb
                | TextureFormat::Bgra8Unorm
                | TextureFormat::Bgra8UnormSrgb
        ) {
            fail_readback_group(&group, reporter.as_ref());
            continue;
        }
        let Some(row_bytes) = usize::try_from(source_extent.0)
            .ok()
            .and_then(|width| width.checked_mul(4))
        else {
            fail_readback_group(&group, reporter.as_ref());
            continue;
        };
        let row_pitch = RenderDevice::align_copy_bytes_per_row(row_bytes);
        let Some(buffer_size) = row_pitch.checked_mul(source_extent.1 as usize) else {
            fail_readback_group(&group, reporter.as_ref());
            continue;
        };
        let buffer = device.create_buffer(&BufferDescriptor {
            label: Some("CosMix capture readback staging"),
            size: buffer_size as u64,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("CosMix capture readback encoder"),
        });
        let destination = TexelCopyBufferInfo {
            buffer: &buffer,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_pitch as u32),
                rows_per_image: Some(source_extent.1),
            },
        };
        let copy_extent = Extent3d {
            width: source_extent.0,
            height: source_extent.1,
            depth_or_array_layers: 1,
        };
        let scene_copy = TexelCopyTextureInfo {
            texture: scene_texture.texture(),
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        };
        if cursor_inclusive {
            let Some((overlay_target, _)) = overlay_views
                .iter()
                .find(|(_, overlay)| overlay.source_id == source_id)
            else {
                fail_readback_group(&group, reporter.as_ref());
                continue;
            };
            let Some(overlay_texture) = overlay_target.out_texture() else {
                fail_readback_group(&group, reporter.as_ref());
                continue;
            };
            match &source_id {
                CaptureSourceId::Kms { .. } => {
                    let _written = target.out_texture_color_attachment(None);
                    if !encode_cursor_overlay(
                        &mut encoder,
                        &device,
                        &composite,
                        overlay_texture,
                        scene_texture,
                        source_format,
                    ) {
                        fail_readback_group(&group, reporter.as_ref());
                        continue;
                    }
                    kms_composited.insert(source_id.clone());
                    encoder.copy_texture_to_buffer(scene_copy, destination, copy_extent);
                }
                CaptureSourceId::Nested { .. } => {
                    // The host swapchain already contains the cursor-free scene.
                    // Compose only into this capture-owned intermediate so the
                    // host compositor still draws exactly one cursor.
                    let composed = device.create_texture(&TextureDescriptor {
                        label: Some("CosMix nested cursor-inclusive capture"),
                        size: copy_extent,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: TextureDimension::D2,
                        format: source_format,
                        usage: TextureUsages::COPY_DST
                            | TextureUsages::COPY_SRC
                            | TextureUsages::RENDER_ATTACHMENT,
                        view_formats: &[],
                    });
                    let composed_view = composed.create_view(&Default::default());
                    encoder.copy_texture_to_texture(
                        scene_copy,
                        composed.as_image_copy(),
                        copy_extent,
                    );
                    if !encode_cursor_overlay(
                        &mut encoder,
                        &device,
                        &composite,
                        overlay_texture,
                        &composed_view,
                        source_format,
                    ) {
                        fail_readback_group(&group, reporter.as_ref());
                        continue;
                    }
                    encoder.copy_texture_to_buffer(
                        composed.as_image_copy(),
                        destination,
                        copy_extent,
                    );
                }
            }
            // Cursor pixels now came from the production GPU overlay camera;
            // the worker must not apply a second CPU cursor.
            for request in &mut group.requests {
                request.cursor = None;
            }
            #[cfg(feature = "frame-capture")]
            for request in &mut group.png {
                request.request.cursor = None;
            }
        } else {
            encoder.copy_texture_to_buffer(scene_copy, destination, copy_extent);
        }
        queue.submit([encoder.finish()]);
        let (mapped_tx, mapped) = mpsc::sync_channel(1);
        buffer.slice(..).map_async(MapMode::Read, move |result| {
            let _ = mapped_tx.try_send(result.map_err(|error| error.to_string()));
        });
        let presentations = group
            .requests
            .iter()
            .map(|request| PendingCapturePresentation {
                id: request.id,
                source_id: request.source_id.clone(),
                frame_token,
                generation: request.generation,
                security_epoch: request.security_epoch,
                deadline: request.deadline,
            })
            .collect::<Vec<_>>();
        let presentation_bound = group.requests.is_empty()
            || match &source_id {
                CaptureSourceId::Nested { .. } => pending_nested.as_ref().is_some_and(|pending| {
                    pending.publish(presentations);
                    true
                }),
                CaptureSourceId::Kms { .. } => kms_targets.as_deref_mut().is_some_and(|targets| {
                    targets.bind_capture_presentations(&source_id, presentations, reporter.clone())
                }),
            };
        if !presentation_bound {
            fail_readback_group(&group, reporter.as_ref());
            buffer.unmap();
            continue;
        }
        let job = CaptureReadbackJob {
            buffer,
            mapped,
            device: device.clone(),
            row_pitch,
            source_extent,
            source_format,
            transform,
            #[cfg(feature = "frame-capture")]
            displayed_extent,
            frame_token,
            requests: group.requests,
            #[cfg(feature = "frame-capture")]
            png_requests: group.png,
            reporter: reporter.clone(),
        };
        let Some(sender) = worker.sender.as_ref() else {
            fail_requests(&job.requests, job.reporter.as_ref());
            #[cfg(feature = "frame-capture")]
            fail_png(&job.png_requests);
            continue;
        };
        let _ = try_submit_readback(sender, job);
    }

    // KMS scan-out always includes the production cursor, even when nobody
    // requested an inclusive readback. Base copies above have already been
    // submitted, so this final pass cannot contaminate them.
    for (_, target, source) in &views {
        let Some(source) = source else {
            continue;
        };
        if !matches!(source.source_id, CaptureSourceId::Kms { .. })
            || kms_composited.contains(&source.source_id)
        {
            continue;
        }
        let Some(destination) = target.out_texture() else {
            continue;
        };
        let Some(format) = target.out_texture_view_format() else {
            continue;
        };
        let Some((overlay, _)) = overlay_views
            .iter()
            .find(|(_, overlay)| overlay.source_id == source.source_id)
        else {
            continue;
        };
        let Some(overlay) = overlay.out_texture() else {
            continue;
        };
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("CosMix KMS cursor overlay encoder"),
        });
        let _written = target.out_texture_color_attachment(None);
        if encode_cursor_overlay(
            &mut encoder,
            &device,
            &composite,
            overlay,
            destination,
            format,
        ) {
            queue.submit([encoder.finish()]);
        }
    }
}

fn try_submit_readback(sender: &SyncSender<CaptureReadbackJob>, job: CaptureReadbackJob) -> bool {
    match sender.try_send(job) {
        Ok(()) => true,
        Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => {
            job.buffer.unmap();
            fail_requests(&job.requests, job.reporter.as_ref());
            #[cfg(feature = "frame-capture")]
            fail_png(&job.png_requests);
            false
        }
    }
}

fn fail_readback_group(group: &CaptureReadbackGroup, reporter: Option<&CaptureCompletionReporter>) {
    fail_requests(&group.requests, reporter);
    #[cfg(feature = "frame-capture")]
    fail_png(&group.png);
}

#[cfg(feature = "frame-capture")]
fn fail_png(requests: &[PngCaptureAdmission]) {
    for request in requests {
        request.complete();
    }
}

fn complete_readback(mut job: CaptureReadbackJob, stop: &AtomicBool) {
    let mapped = loop {
        if stop.load(Ordering::Acquire) {
            job.buffer.unmap();
            return;
        }
        let now = Instant::now();
        job.requests.retain(|request| {
            if request.cancellation.is_cancelled() {
                return false;
            }
            if request.deadline <= now {
                if let Some(reporter) = &job.reporter {
                    reporter.failed(request.id, request.generation, request.security_epoch);
                }
                return false;
            }
            true
        });
        #[cfg(feature = "frame-capture")]
        job.png_requests.retain(|request| {
            if request.deadline <= now {
                request.complete();
                false
            } else {
                true
            }
        });
        let earliest_deadline = job
            .requests
            .iter()
            .map(|request| request.deadline)
            .chain({
                #[cfg(feature = "frame-capture")]
                {
                    job.png_requests.iter().map(|request| request.deadline)
                }
                #[cfg(not(feature = "frame-capture"))]
                {
                    std::iter::empty()
                }
            })
            .min();
        let Some(earliest_deadline) = earliest_deadline else {
            job.buffer.unmap();
            return;
        };
        if job
            .device
            .poll(bevy::render::render_resource::PollType::Poll)
            .is_err()
        {
            fail_requests(&job.requests, job.reporter.as_ref());
            #[cfg(feature = "frame-capture")]
            fail_png(&job.png_requests);
            job.buffer.unmap();
            return;
        }
        match job.mapped.try_recv() {
            Ok(Ok(())) => break true,
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => break false,
            Err(TryRecvError::Empty) => std::thread::park_timeout(
                earliest_deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(1)),
            ),
        }
    };
    if !mapped {
        fail_requests(&job.requests, job.reporter.as_ref());
        #[cfg(feature = "frame-capture")]
        fail_png(&job.png_requests);
        job.buffer.unmap();
        return;
    }
    let mapped = job.buffer.slice(..).get_mapped_range();
    let Some(rgba) = normalise_mapped_output(
        &mapped,
        job.row_pitch,
        job.source_extent,
        job.source_format,
        job.transform,
    ) else {
        drop(mapped);
        job.buffer.unmap();
        fail_requests(&job.requests, job.reporter.as_ref());
        #[cfg(feature = "frame-capture")]
        fail_png(&job.png_requests);
        return;
    };
    drop(mapped);
    job.buffer.unmap();
    for request in &job.requests {
        if request.cancellation.is_cancelled() {
            continue;
        }
        if request.deadline <= Instant::now() {
            if let Some(reporter) = &job.reporter {
                reporter.failed(request.id, request.generation, request.security_epoch);
            }
            continue;
        }
        let mut composed;
        let pixels = if request.overlay_cursor {
            composed = rgba.clone();
            if let Some(cursor) = &request.cursor {
                overlay_cursor(&mut composed, request.displayed_physical_extent, cursor);
            }
            &composed
        } else {
            &rgba
        };
        let Some(packed_bgra) = convert_capture(pixels, request) else {
            if let Some(reporter) = &job.reporter {
                reporter.failed(request.id, request.generation, request.security_epoch);
            }
            continue;
        };
        if let Some(reporter) = &job.reporter {
            reporter.pixels(CapturePixels {
                id: request.id,
                source_id: request.source_id.clone(),
                frame_token: job.frame_token,
                generation: request.generation,
                security_epoch: request.security_epoch,
                width: request.region.width,
                height: request.region.height,
                format: request.format,
                y_invert: false,
                damage_revision: request.damage_revision,
                damage: request.damage.clone(),
                packed_bgra: Arc::new(packed_bgra),
                _reservation: request.reservation.clone(),
            });
        }
    }
    #[cfg(feature = "frame-capture")]
    for request in job.png_requests {
        let mut inclusive;
        let pixels = if let Some(cursor) = &request.request.cursor {
            inclusive = rgba.clone();
            overlay_cursor(&mut inclusive, job.displayed_extent, cursor);
            &inclusive
        } else {
            &rgba
        };
        publish_png(pixels, job.displayed_extent, request);
    }
}

fn overlay_cursor(target: &mut [u8], extent: (u32, u32), cursor: &CaptureCursorSnapshot) {
    if cursor.width == 0 || cursor.height == 0 {
        return;
    }
    for dy in 0..cursor.height {
        let y = cursor.y.saturating_add(dy as i32);
        if y < 0 || y >= extent.1 as i32 {
            continue;
        }
        for dx in 0..cursor.width {
            let x = cursor.x.saturating_add(dx as i32);
            if x < 0 || x >= extent.0 as i32 {
                continue;
            }
            let source = ((dy * cursor.width + dx) * 4) as usize;
            let destination = ((y as u32 * extent.0 + x as u32) * 4) as usize;
            let Some(source_pixel) = cursor.rgba.get(source..source + 4) else {
                continue;
            };
            let Some(destination_pixel) = target.get_mut(destination..destination + 4) else {
                continue;
            };
            let alpha = u32::from(source_pixel[3]);
            let inverse = 255 - alpha;
            for channel in 0..3 {
                let source = if cursor.premultiplied {
                    u32::from(source_pixel[channel])
                } else {
                    (u32::from(source_pixel[channel]) * alpha + 127) / 255
                };
                destination_pixel[channel] = (source
                    + (u32::from(destination_pixel[channel]) * inverse + 127) / 255)
                    .min(255) as u8;
            }
            destination_pixel[3] = 255;
        }
    }
}

fn fail_requests(requests: &[CaptureRequest], reporter: Option<&CaptureCompletionReporter>) {
    let Some(reporter) = reporter else {
        return;
    };
    for request in requests {
        if !request.cancellation.is_cancelled() {
            reporter.failed(request.id, request.generation, request.security_epoch);
        }
    }
}

fn normalise_mapped_output(
    mapped: &[u8],
    row_pitch: usize,
    source_extent: (u32, u32),
    source_format: TextureFormat,
    transform: smithay::utils::Transform,
) -> Option<Vec<u8>> {
    let (width, height) = source_extent;
    let displayed = if matches!(
        transform,
        smithay::utils::Transform::_90
            | smithay::utils::Transform::_270
            | smithay::utils::Transform::Flipped90
            | smithay::utils::Transform::Flipped270
    ) {
        (height, width)
    } else {
        (width, height)
    };
    let byte_count = usize::try_from(displayed.0)
        .ok()?
        .checked_mul(displayed.1 as usize)?
        .checked_mul(4)?;
    let mut rgba = vec![0_u8; byte_count];
    for y in 0..height {
        for x in 0..width {
            let source = (y as usize)
                .checked_mul(row_pitch)?
                .checked_add(x as usize * 4)?;
            let pixel = mapped.get(source..source + 4)?;
            let (r, g, b, a) = match source_format {
                TextureFormat::Rgba8Unorm | TextureFormat::Rgba8UnormSrgb => {
                    (pixel[0], pixel[1], pixel[2], pixel[3])
                }
                TextureFormat::Bgra8Unorm | TextureFormat::Bgra8UnormSrgb => {
                    (pixel[2], pixel[1], pixel[0], pixel[3])
                }
                _ => return None,
            };
            let (dx, dy) = transform_pixel(transform, x, y, width, height)?;
            let destination = (dy as usize)
                .checked_mul(displayed.0 as usize)?
                .checked_add(dx as usize)?
                .checked_mul(4)?;
            rgba[destination..destination + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    Some(rgba)
}

pub(crate) fn transform_pixel(
    transform: smithay::utils::Transform,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Option<(u32, u32)> {
    Some(match transform {
        smithay::utils::Transform::Normal => (x, y),
        smithay::utils::Transform::_90 => (height.checked_sub(y + 1)?, x),
        smithay::utils::Transform::_180 => (width.checked_sub(x + 1)?, height.checked_sub(y + 1)?),
        smithay::utils::Transform::_270 => (y, width.checked_sub(x + 1)?),
        smithay::utils::Transform::Flipped => (width.checked_sub(x + 1)?, y),
        smithay::utils::Transform::Flipped90 => {
            (height.checked_sub(y + 1)?, width.checked_sub(x + 1)?)
        }
        smithay::utils::Transform::Flipped180 => (x, height.checked_sub(y + 1)?),
        smithay::utils::Transform::Flipped270 => (y, x),
    })
}

fn convert_capture(rgba: &[u8], request: &CaptureRequest) -> Option<Vec<u8>> {
    let source_size = request.displayed_physical_extent;
    let region = request.region;
    let right = region.x.checked_add(region.width)?;
    let bottom = region.y.checked_add(region.height)?;
    if right > source_size.0 || bottom > source_size.1 {
        return None;
    }
    let row_bytes = usize::try_from(region.width).ok()?.checked_mul(4)?;
    let output_bytes = row_bytes.checked_mul(region.height as usize)?;
    let source_stride = usize::try_from(source_size.0).ok()?.checked_mul(4)?;
    let mut packed = vec![0_u8; output_bytes];
    for row in 0..region.height as usize {
        let source_start = (region.y as usize)
            .checked_add(row)?
            .checked_mul(source_stride)?
            .checked_add(region.x as usize * 4)?;
        let source = rgba.get(source_start..source_start.checked_add(row_bytes)?)?;
        let destination = &mut packed[row * row_bytes..(row + 1) * row_bytes];
        for (pixel, converted) in source.chunks_exact(4).zip(destination.chunks_exact_mut(4)) {
            let alpha = match request.format {
                CaptureFormat::Argb8888 => pixel[3],
                CaptureFormat::Xrgb8888 => 0xff,
            };
            converted.copy_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
        }
    }
    Some(packed)
}

#[cfg(feature = "frame-capture")]
#[derive(Clone)]
pub(crate) enum PngCaptureTarget {
    Nested,
    Kms {
        source_id: CaptureSourceId,
        output_name: String,
    },
}

#[cfg(feature = "frame-capture")]
#[derive(Clone)]
pub(crate) struct PngCaptureRequest {
    pub(crate) target: PngCaptureTarget,
    pub(crate) cursor: Option<CaptureCursorSnapshot>,
    pub(crate) final_path: std::path::PathBuf,
    pub(crate) temporary_path: std::path::PathBuf,
}

#[cfg(feature = "frame-capture")]
#[derive(Resource, Clone, Default)]
pub(crate) struct PngCaptureService {
    queue: Arc<Mutex<Vec<PngCaptureAdmission>>>,
    batch_in_flight: Arc<AtomicBool>,
}

#[cfg(feature = "frame-capture")]
impl PngCaptureService {
    pub(crate) fn busy(&self) -> bool {
        self.batch_in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn submit_batch(&self, requests: Vec<PngCaptureRequest>) -> bool {
        if requests.is_empty()
            || self
                .batch_in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        let remaining = Arc::new(AtomicUsize::new(requests.len()));
        let deadline = Instant::now()
            .checked_add(crate::protocol::CAPTURE_REQUEST_TIMEOUT)
            .unwrap_or_else(Instant::now);
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(requests.into_iter().map(|request| PngCaptureAdmission {
                request,
                deadline,
                finished: Arc::new(AtomicBool::new(false)),
                batch_remaining: Arc::clone(&remaining),
                service: self.clone(),
            }));
        true
    }
}

#[cfg(feature = "frame-capture")]
#[derive(Clone)]
struct PngCaptureAdmission {
    request: PngCaptureRequest,
    deadline: Instant,
    finished: Arc<AtomicBool>,
    batch_remaining: Arc<AtomicUsize>,
    service: PngCaptureService,
}

#[cfg(feature = "frame-capture")]
impl PngCaptureAdmission {
    fn complete(&self) {
        if !self.finished.swap(true, Ordering::AcqRel)
            && self.batch_remaining.fetch_sub(1, Ordering::AcqRel) == 1
        {
            self.service.batch_in_flight.store(false, Ordering::Release);
        }
    }
}

#[cfg(feature = "frame-capture")]
fn publish_png(rgba: &[u8], size: (u32, u32), request: PngCaptureAdmission) {
    if request.deadline <= Instant::now() {
        request.complete();
        return;
    }
    let mut bgra = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    let image = bevy::image::Image::new(
        Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        bgra,
        TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD,
    );
    let completion = request.clone();
    crate::frame_capture::save_capture_image(
        image,
        request.request.temporary_path,
        request.request.final_path,
        request.deadline,
        move || completion.complete(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use bevy::{
        DefaultPlugins,
        app::{PluginGroup, TerminalCtrlCHandlerPlugin},
        log::LogPlugin,
        render::{
            RenderPlugin,
            pipelined_rendering::PipelinedRenderingPlugin,
            render_asset::RenderAssets,
            renderer::{RenderAdapter, RenderAdapterInfo, RenderInstance, WgpuWrapper},
            settings::RenderCreation,
            texture::GpuImage,
        },
        window::{ExitCondition, WindowPlugin},
        winit::WinitPlugin,
    };
    use cosmix_wgpu_dmabuf::DmabufImportPlugin;
    use smithay::reexports::wayland_server::backend::ObjectId;

    fn test_render_device() -> (wgpu::Device, RenderDevice) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            bevy::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            }))
            .expect("capture worker tests require a real Vulkan adapter");
        let (device, _) =
            bevy::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("capture worker test device");
        let render_device = RenderDevice::from(device.clone());
        (device, render_device)
    }

    fn request(transform: smithay::utils::Transform) -> CaptureRequest {
        CaptureRequest {
            id: CaptureId(1),
            source_id: CaptureSourceId::Nested {
                output_name: "cosmix-nested-0".into(),
            },
            output_name: "cosmix-nested-0".into(),
            generation: 1,
            security_epoch: 1,
            region: CaptureRegion {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            logical_rect: (0, 0, 2, 1),
            source_storage_extent: (2, 1),
            displayed_physical_extent: (2, 1),
            scale120: 120,
            transform,
            format: CaptureFormat::Xrgb8888,
            overlay_cursor: false,
            cursor: None,
            with_damage: false,
            damage_baseline: None,
            damage_revision: 3,
            damage: Vec::new(),
            cancellation: CaptureCancellation::default(),
            reservation: CaptureReservationLease::detached(CaptureId(1)),
            deadline: Instant::now() + crate::protocol::CAPTURE_REQUEST_TIMEOUT,
        }
    }

    #[test]
    fn renderer_owned_readback_preserves_xrgb_channel_order() {
        let request = request(smithay::utils::Transform::Normal);
        assert_eq!(
            convert_capture(&[1, 2, 3, 4, 10, 20, 30, 40], &request),
            Some(vec![3, 2, 1, 255, 30, 20, 10, 255])
        );
    }

    #[test]
    fn screencopy_s1a_16_crop_edges_have_no_pitch_skew() {
        let raw = [
            1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 0xee, 0xee, 0xee, 0xee, 4, 0, 0, 255, 5, 0,
            0, 255, 6, 0, 0, 255, 0xdd, 0xdd, 0xdd, 0xdd,
        ];
        let normalised = normalise_mapped_output(
            &raw,
            16,
            (3, 2),
            TextureFormat::Rgba8Unorm,
            smithay::utils::Transform::Normal,
        )
        .expect("padded GPU rows normalise");
        let mut cropped = request(smithay::utils::Transform::Normal);
        cropped.region = CaptureRegion {
            x: 1,
            y: 0,
            width: 2,
            height: 2,
        };
        cropped.logical_rect = (0, 0, 3, 2);
        cropped.source_storage_extent = (3, 2);
        cropped.displayed_physical_extent = (3, 2);
        assert_eq!(
            convert_capture(&normalised, &cropped),
            Some(vec![0, 0, 2, 255, 0, 0, 3, 255, 0, 0, 5, 255, 0, 0, 6, 255])
        );
    }

    #[test]
    fn plain_copy_requests_a_redraw_without_background_animation() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_plugins(CaptureServicePlugin);
        app.world_mut().resource_mut::<CaptureRendererAvailable>().0 = true;
        app.world_mut()
            .resource_mut::<CaptureQueue>()
            .push(request(smithay::utils::Transform::Normal));
        app.update();
        assert_eq!(app.world().resource::<Messages<RequestRedraw>>().len(), 1);
        assert_eq!(
            app.world()
                .resource::<CaptureReadbackBatch>()
                .0
                .lock()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn capture_worker_shutdown_is_bounded() {
        let started = Instant::now();
        drop(CaptureReadbackWorker::default());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_while_map_is_in_flight_drops_pixels_and_releases_once() {
        let (device, render_device) = test_render_device();
        let buffer = render_device.create_buffer(&BufferDescriptor {
            label: Some("cancelled in-flight capture map"),
            size: 4,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let (mapped_tx, mapped) = mpsc::sync_channel(1);
        buffer.slice(..).map_async(MapMode::Read, move |result| {
            let _ = mapped_tx.try_send(result.map_err(|error| error.to_string()));
        });
        let releases = Arc::new(AtomicUsize::new(0));
        let mut item = request(smithay::utils::Transform::Normal);
        item.source_storage_extent = (1, 1);
        item.displayed_physical_extent = (1, 1);
        item.logical_rect = (0, 0, 1, 1);
        item.region = full_region((1, 1));
        item.reservation = CaptureReservationLease::counted(item.id, Arc::clone(&releases));
        let cancellation = item.cancellation.clone();
        let (_events, feed) = ClientSceneFeed::test_channel();
        cancellation.cancel();
        complete_readback(
            CaptureReadbackJob {
                buffer,
                mapped,
                device: render_device,
                row_pitch: 4,
                source_extent: (1, 1),
                source_format: TextureFormat::Rgba8Unorm,
                transform: smithay::utils::Transform::Normal,
                #[cfg(feature = "frame-capture")]
                displayed_extent: (1, 1),
                frame_token: 1,
                requests: vec![item],
                #[cfg(feature = "frame-capture")]
                png_requests: Vec::new(),
                reporter: Some(feed.capture_completion_reporter()),
            },
            &AtomicBool::new(false),
        );
        device.poll(wgpu::PollType::Poll).unwrap();
        assert_eq!(releases.load(Ordering::Acquire), 1);
    }

    #[test]
    fn screenshot_completion_saturation_fails_new_wire_copy() {
        let (_device, render_device) = test_render_device();
        let buffer = render_device.create_buffer(&BufferDescriptor {
            label: Some("saturated capture queue"),
            size: 4,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let (mapped_tx, mapped) = mpsc::sync_channel(1);
        buffer.slice(..).map_async(MapMode::Read, move |result| {
            let _ = mapped_tx.try_send(result.map_err(|error| error.to_string()));
        });
        let (_receiver_guard, sender) = {
            let (sender, receiver) = mpsc::sync_channel::<CaptureReadbackJob>(0);
            (receiver, sender)
        };
        let (_events, feed) = ClientSceneFeed::test_channel();
        let reporter = feed.capture_completion_reporter();
        assert!(!try_submit_readback(
            &sender,
            CaptureReadbackJob {
                buffer,
                mapped,
                device: render_device,
                row_pitch: 4,
                source_extent: (1, 1),
                source_format: TextureFormat::Rgba8Unorm,
                transform: smithay::utils::Transform::Normal,
                #[cfg(feature = "frame-capture")]
                displayed_extent: (1, 1),
                frame_token: 1,
                requests: vec![request(smithay::utils::Transform::Normal)],
                #[cfg(feature = "frame-capture")]
                png_requests: Vec::new(),
                reporter: Some(reporter),
            }
        ));
        assert_eq!(
            feed.capture_outcomes_for_test(),
            vec![crate::protocol::CaptureTestOutcome::Failed(CaptureId(1))]
        );
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn png_and_wire_consumers_share_one_renderer_batch() {
        let service = PngCaptureService::default();
        assert!(service.submit_batch(vec![PngCaptureRequest {
            target: PngCaptureTarget::Nested,
            cursor: None,
            final_path: "capture.png".into(),
            temporary_path: "capture.tmp".into(),
        }]));
        let wire = request(smithay::utils::Transform::Normal);
        let png = service.queue.lock().unwrap().pop().unwrap();
        let mut groups = BTreeMap::<(CaptureSourceId, bool), CaptureReadbackGroup>::new();
        groups
            .entry((wire.source_id.clone(), wire.overlay_cursor))
            .or_default()
            .requests
            .push(wire);
        groups
            .entry((
                CaptureSourceId::Nested {
                    output_name: "cosmix-nested-0".into(),
                },
                false,
            ))
            .or_default()
            .png
            .push(png);
        assert_eq!(groups.len(), 1);
        let batch = groups.values().next().unwrap();
        assert_eq!(batch.requests.len(), 1);
        assert_eq!(batch.png.len(), 1);
    }

    #[test]
    fn all_wayland_transforms_normalise_corner_pixels_top_down() {
        let raw = [1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255];
        for (transform, expected) in [
            (smithay::utils::Transform::Normal, [1, 2, 3, 4]),
            (smithay::utils::Transform::_90, [3, 1, 4, 2]),
            (smithay::utils::Transform::_180, [4, 3, 2, 1]),
            (smithay::utils::Transform::_270, [2, 4, 1, 3]),
            (smithay::utils::Transform::Flipped, [2, 1, 4, 3]),
            (smithay::utils::Transform::Flipped90, [4, 2, 3, 1]),
            (smithay::utils::Transform::Flipped180, [3, 4, 1, 2]),
            (smithay::utils::Transform::Flipped270, [1, 3, 2, 4]),
        ] {
            let normalised =
                normalise_mapped_output(&raw, 8, (2, 2), TextureFormat::Rgba8Unorm, transform)
                    .expect("four-corner fixture normalises");
            assert_eq!(normalised.len(), raw.len());
            assert_eq!(
                normalised.iter().step_by(4).copied().collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn cancelled_map_releases_the_reservation_once() {
        let releases = Arc::new(AtomicUsize::new(0));
        let lease = CaptureReservationLease::counted(CaptureId(7), Arc::clone(&releases));
        let cancellation = CaptureCancellation::default();
        cancellation.cancel();
        drop(lease);
        assert_eq!(releases.load(Ordering::Acquire), 1);
    }

    #[test]
    fn damage_metadata_stays_attached_to_the_pixel_token() {
        let mut request = request(smithay::utils::Transform::Normal);
        request.with_damage = true;
        request.damage = vec![CaptureRegion {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        }];
        assert_eq!(request.damage_revision, 3);
        assert_eq!(request.damage[0].x, 1);
    }

    #[test]
    fn bounded_damage_journal_separates_cursor_and_manager_baselines() {
        let journal = OutputDamageJournal::default();
        let source = CaptureSourceId::Nested {
            output_name: "cosmix-nested-0".into(),
        };
        journal.register(
            source.clone(),
            (0, 0, 64, 48),
            (64, 48),
            (64, 48),
            120,
            smithay::utils::Transform::Normal,
        );
        let region = CaptureRegion {
            x: 8,
            y: 6,
            width: 20,
            height: 12,
        };
        assert_eq!(
            journal.snapshot(&source, None, false, region),
            (0, vec![full_region((20, 12))])
        );
        journal.mark_all_cursor_regions(&[full_region((64, 48))]);
        assert!(
            journal
                .snapshot(&source, Some(0), false, region)
                .1
                .is_empty()
        );
        assert_eq!(
            journal.snapshot(&source, Some(0), true, region).1,
            vec![full_region((20, 12))]
        );
        let cursor_revision = journal.snapshot(&source, Some(0), true, region).0;
        journal.mark_all_base_full();
        let (revision, damage) = journal.snapshot(&source, Some(cursor_revision), false, region);
        assert!(revision > cursor_revision);
        assert_eq!(damage, vec![full_region((20, 12))]);
    }

    #[test]
    fn damage_journal_coalesces_and_clips_projected_old_and_new_bounds() {
        let journal = OutputDamageJournal::default();
        let source = CaptureSourceId::Nested {
            output_name: "cosmix-nested-0".into(),
        };
        journal.register(
            source.clone(),
            (0, 0, 20, 12),
            (20, 12),
            (20, 12),
            120,
            smithay::utils::Transform::Normal,
        );
        journal.mark_all_base_regions(&[
            CaptureRegion {
                x: 2,
                y: 3,
                width: 8,
                height: 5,
            },
            CaptureRegion {
                x: 8,
                y: 4,
                width: 20,
                height: 4,
            },
        ]);
        let (revision, damage) = journal.snapshot(
            &source,
            Some(0),
            false,
            CaptureRegion {
                x: 5,
                y: 2,
                width: 10,
                height: 8,
            },
        );
        assert_eq!(revision, 1);
        assert_eq!(
            damage,
            vec![CaptureRegion {
                x: 0,
                y: 1,
                width: 10,
                height: 5,
            }]
        );
    }

    #[test]
    fn damage_journal_transforms_storage_rectangles_into_displayed_orientation() {
        let journal = OutputDamageJournal::default();
        let source = CaptureSourceId::Kms {
            key: crate::backend::kms::OutputKey {
                device: 17,
                connector_name: "DP-17".into(),
            },
            generation: 9,
        };
        journal.register(
            source.clone(),
            (0, 0, 12, 20),
            (20, 12),
            (12, 20),
            120,
            smithay::utils::Transform::_90,
        );
        journal.mark_all_base_regions(&[CaptureRegion {
            x: 2,
            y: 3,
            width: 8,
            height: 5,
        }]);
        assert_eq!(
            journal
                .snapshot(&source, Some(0), false, full_region((12, 20)))
                .1,
            vec![CaptureRegion {
                x: 4,
                y: 2,
                width: 5,
                height: 8,
            }]
        );
    }

    #[test]
    fn output_local_damage_on_a_never_wakes_b() {
        let journal = OutputDamageJournal::default();
        let source = |connector: &str| CaptureSourceId::Kms {
            key: crate::backend::kms::OutputKey {
                device: 17,
                connector_name: connector.into(),
            },
            generation: 1,
        };
        let a = source("A");
        let b = source("B");
        journal.register(
            a.clone(),
            (0, 0, 100, 100),
            (100, 100),
            (100, 100),
            120,
            smithay::utils::Transform::Normal,
        );
        journal.register(
            b.clone(),
            (100, 0, 100, 100),
            (100, 100),
            (100, 100),
            120,
            smithay::utils::Transform::Normal,
        );
        journal.mark_base_logical_regions(&[CaptureLogicalRegion {
            x: 10.0,
            y: 12.0,
            width: 8.0,
            height: 6.0,
        }]);
        assert!(
            !journal
                .snapshot(&a, Some(0), false, full_region((100, 100)))
                .1
                .is_empty()
        );
        assert!(
            journal
                .snapshot(&b, Some(0), false, full_region((100, 100)))
                .1
                .is_empty()
        );

        journal.mark_cursor_logical_regions(&[CaptureLogicalRegion {
            x: 120.0,
            y: 20.0,
            width: 4.0,
            height: 5.0,
        }]);
        assert!(
            journal
                .snapshot(&a, Some(1), true, full_region((100, 100)))
                .1
                .is_empty()
        );
        assert_eq!(
            journal
                .snapshot(&b, Some(0), true, full_region((100, 100)))
                .1,
            vec![CaptureRegion {
                x: 20,
                y: 20,
                width: 4,
                height: 5,
            }]
        );
    }

    #[test]
    fn damage_journal_prunes_replaced_generations_and_pause_retirement() {
        let journal = OutputDamageJournal::default();
        let key = crate::backend::kms::OutputKey {
            device: 17,
            connector_name: "A".into(),
        };
        for generation in 1..=64 {
            journal.register(
                CaptureSourceId::Kms {
                    key: key.clone(),
                    generation,
                },
                (0, 0, 100, 100),
                (100, 100),
                (100, 100),
                120,
                smithay::utils::Transform::Normal,
            );
            assert_eq!(journal.0.lock().unwrap().len(), 1);
        }
        journal.retire_kms_sources();
        assert!(journal.0.lock().unwrap().is_empty());
    }

    #[test]
    fn surface_upsert_relayout_and_unmap_each_wake_damage_waiters() {
        let journal = OutputDamageJournal::default();
        let source = CaptureSourceId::Nested {
            output_name: "cosmix-nested-0".into(),
        };
        journal.register(
            source.clone(),
            (0, 0, 100, 100),
            (100, 100),
            (100, 100),
            120,
            smithay::utils::Transform::Normal,
        );
        let mut baseline = 0;
        for bounds in [
            CaptureLogicalRegion {
                x: 4.0,
                y: 5.0,
                width: 10.0,
                height: 12.0,
            },
            CaptureLogicalRegion {
                x: 20.0,
                y: 5.0,
                width: 10.0,
                height: 12.0,
            },
            CaptureLogicalRegion {
                x: 20.0,
                y: 5.0,
                width: 10.0,
                height: 12.0,
            },
        ] {
            journal.mark_base_logical_regions(&[bounds]);
            let (revision, damage) =
                journal.snapshot(&source, Some(baseline), false, full_region((100, 100)));
            assert!(!damage.is_empty());
            assert!(revision > baseline);
            baseline = revision;
        }
    }

    #[test]
    fn cursor_motion_only_wakes_inclusive_not_base_capture() {
        let journal = OutputDamageJournal::default();
        let source = CaptureSourceId::Nested {
            output_name: "cosmix-nested-0".into(),
        };
        journal.register(
            source.clone(),
            (0, 0, 100, 100),
            (100, 100),
            (100, 100),
            120,
            smithay::utils::Transform::Normal,
        );
        journal.mark_cursor_logical_regions(&[
            CaptureLogicalRegion {
                x: 2.0,
                y: 3.0,
                width: 8.0,
                height: 9.0,
            },
            CaptureLogicalRegion {
                x: 12.0,
                y: 13.0,
                width: 8.0,
                height: 9.0,
            },
        ]);
        assert!(
            journal
                .snapshot(&source, Some(0), false, full_region((100, 100)))
                .1
                .is_empty()
        );
        assert!(
            !journal
                .snapshot(&source, Some(0), true, full_region((100, 100)))
                .1
                .is_empty()
        );
    }

    #[test]
    fn output_resize_forces_full_damage() {
        let journal = OutputDamageJournal::default();
        let source = CaptureSourceId::Nested {
            output_name: "cosmix-nested-0".into(),
        };
        journal.register(
            source.clone(),
            (0, 0, 100, 100),
            (100, 100),
            (100, 100),
            120,
            smithay::utils::Transform::Normal,
        );
        journal.register(
            source.clone(),
            (0, 0, 120, 80),
            (120, 80),
            (120, 80),
            120,
            smithay::utils::Transform::Normal,
        );
        assert_eq!(
            journal
                .snapshot(&source, Some(0), false, full_region((120, 80)))
                .1,
            vec![full_region((120, 80))]
        );
    }

    #[test]
    fn two_manager_damage_baselines_advance_independently() {
        let journal = OutputDamageJournal::default();
        let source = CaptureSourceId::Nested {
            output_name: "cosmix-nested-0".into(),
        };
        journal.register(
            source.clone(),
            (0, 0, 100, 100),
            (100, 100),
            (100, 100),
            120,
            smithay::utils::Transform::Normal,
        );
        journal.mark_base_logical_regions(&[CaptureLogicalRegion {
            x: 1.0,
            y: 1.0,
            width: 2.0,
            height: 2.0,
        }]);
        let manager_a = journal
            .snapshot(&source, Some(0), false, full_region((100, 100)))
            .0;
        let manager_b = 0;
        journal.mark_base_logical_regions(&[CaptureLogicalRegion {
            x: 10.0,
            y: 10.0,
            width: 2.0,
            height: 2.0,
        }]);
        let a_damage = journal
            .snapshot(&source, Some(manager_a), false, full_region((100, 100)))
            .1;
        let b_damage = journal
            .snapshot(&source, Some(manager_b), false, full_region((100, 100)))
            .1;
        assert_eq!(
            a_damage,
            vec![CaptureRegion {
                x: 10,
                y: 10,
                width: 2,
                height: 2
            }]
        );
        assert!(b_damage.iter().any(|damage| damage.x == 1));
        assert!(b_damage.iter().any(|damage| damage.x == 10));
    }

    #[test]
    fn cursor_overlay_preserves_hotspot_clipping_and_premultiplied_alpha() {
        let mut target = vec![10_u8, 20, 30, 255, 10, 20, 30, 255];
        let cursor = CaptureCursorSnapshot {
            x: -1,
            y: 0,
            width: 2,
            height: 1,
            rgba: Arc::new(vec![255, 0, 0, 255, 0, 64, 0, 128]),
            premultiplied: true,
        };
        overlay_cursor(&mut target, (2, 1), &cursor);
        assert_eq!(target, vec![5, 74, 15, 255, 10, 20, 30, 255]);
    }

    #[test]
    fn pixel_equivalence_uses_a_real_render_attachment_and_raw_readback() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            bevy::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            }))
            .expect("pixel-equivalence gate requires a real Vulkan adapter");
        let adapter_info = adapter.get_info();
        assert_eq!(adapter_info.backend, wgpu::Backend::Vulkan);
        let (device, queue) =
            bevy::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("capture pixel-equivalence device"),
                ..Default::default()
            }))
            .expect("pixel-equivalence gate opens its Vulkan device");
        let render_device = RenderDevice::from(device.clone());
        let mut world = World::new();
        world.insert_resource(render_device.clone());
        let composite = CaptureCursorCompositePipeline::from_world(&mut world);

        const WIDTH: u32 = 64;
        const HEIGHT: u32 = 2;
        const CURSOR_X: u32 = 7;
        let render_creation = RenderCreation::manual(
            device.clone().into(),
            RenderQueue(Arc::new(WgpuWrapper::new(queue.clone()))),
            RenderAdapterInfo(WgpuWrapper::new(adapter_info)),
            RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
            RenderInstance(Arc::new(WgpuWrapper::new(instance))),
        );
        let (cursor_events, cursor_feed) = ClientSceneFeed::test_channel();
        cursor_feed.set_cursor_position_for_test(crate::protocol::CursorPositionSnapshot {
            x: 8.0,
            y: 0.0,
            revision: 1,
        });
        let mut cursor_app = App::new();
        cursor_app
            .add_plugins(
                DefaultPlugins
                    .build()
                    .disable::<LogPlugin>()
                    .disable::<WinitPlugin>()
                    .disable::<PipelinedRenderingPlugin>()
                    .disable::<TerminalCtrlCHandlerPlugin>()
                    .set(WindowPlugin {
                        primary_window: None,
                        exit_condition: ExitCondition::DontExit,
                        close_when_requested: false,
                        ..Default::default()
                    })
                    .set(RenderPlugin {
                        render_creation,
                        synchronous_pipeline_compilation: true,
                        ..Default::default()
                    }),
            )
            .insert_resource(cursor_feed)
            .add_plugins((
                DmabufImportPlugin,
                crate::compositor_scene::CompositorScenePlugin::new(
                    WIDTH,
                    HEIGHT,
                    crate::compositor_scene::SceneCursorMode::HostCursor,
                ),
            ));
        cursor_app.finish();
        cursor_app.cleanup();
        cursor_events
            .try_send(vec![crate::protocol::ProtocolEvent::CursorUpdated {
                image: crate::protocol::CursorImage::Surface {
                    id: ObjectId::null(),
                    hotspot: (1, 0),
                    presentation: crate::protocol::CursorPresentation {
                        width: 1.0,
                        height: 1.0,
                        source: None,
                        transform: crate::protocol::SurfaceTransform::Normal,
                    },
                    frame: Some(crate::protocol::SurfaceFrame::Shm(
                        crate::protocol::ShmFrame {
                            width: 1,
                            height: 1,
                            opaque: true,
                            rgba: Arc::new(vec![255, 0, 0, 255]),
                        },
                    )),
                },
            }])
            .expect("production cursor update enters the render app");
        for _ in 0..12 {
            cursor_app.update();
        }
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("production cursor overlay render completes");
        let overlay_handle = cursor_app
            .world()
            .resource::<crate::compositor_scene::NestedCursorOverlay>()
            .image
            .clone();
        let overlay = cursor_app
            .sub_app(RenderApp)
            .world()
            .resource::<RenderAssets<GpuImage>>()
            .get(&overlay_handle)
            .expect("production cursor overlay image is prepared")
            .texture
            .clone();
        let extent = wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        };
        for format in [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8Unorm,
        ] {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("capture pixel-equivalence render target"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let cursor_offset = (CURSOR_X * 4) as usize;
            let staging = |label| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: u64::from(WIDTH * HEIGHT * 4),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                })
            };
            let base_buffer = staging("capture base raw readback");
            let inclusive_buffer = staging("capture inclusive raw readback");
            fn copy_to(buffer: &wgpu::Buffer) -> wgpu::TexelCopyBufferInfo<'_> {
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(WIDTH * 4),
                        rows_per_image: Some(HEIGHT),
                    },
                }
            }
            let mut encoder = render_device.create_command_encoder(&CommandEncoderDescriptor {
                label: Some("capture production-order equivalence encoder"),
            });
            {
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("capture deterministic base scene"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 10.0 / 255.0,
                                g: 20.0 / 255.0,
                                b: 30.0 / 255.0,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            // This is the production structural order: base readback first,
            // then the production overlay pass, then inclusive readback.
            encoder.copy_texture_to_buffer(texture.as_image_copy(), copy_to(&base_buffer), extent);
            let overlay_view = overlay.create_view(&wgpu::TextureViewDescriptor::default());
            let destination_view =
                TextureView::from(texture.create_view(&wgpu::TextureViewDescriptor::default()));
            assert!(encode_cursor_overlay(
                &mut encoder,
                &render_device,
                &composite,
                &overlay_view,
                &destination_view,
                format,
            ));
            encoder.copy_texture_to_buffer(
                texture.as_image_copy(),
                copy_to(&inclusive_buffer),
                extent,
            );
            queue.submit([encoder.finish()]);

            let map = |buffer: &wgpu::Buffer| {
                let (sender, receiver) = mpsc::sync_channel(1);
                buffer
                    .slice(..)
                    .map_async(wgpu::MapMode::Read, move |result| {
                        sender.send(result).unwrap();
                    });
                device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                receiver.recv().unwrap().unwrap();
                let bytes = buffer.slice(..).get_mapped_range().to_vec();
                buffer.unmap();
                bytes
            };
            let base = map(&base_buffer);
            let inclusive = map(&inclusive_buffer);
            let base_pixel = match format {
                wgpu::TextureFormat::Rgba8Unorm => [10, 20, 30, 255],
                wgpu::TextureFormat::Bgra8Unorm => [30, 20, 10, 255],
                _ => unreachable!(),
            };
            assert!(base.chunks_exact(4).all(|pixel| pixel == base_pixel));
            let mut reference = base.clone();
            let inclusive_pixel = match format {
                wgpu::TextureFormat::Rgba8Unorm => [255, 0, 0, 255],
                wgpu::TextureFormat::Bgra8Unorm => [0, 0, 255, 255],
                _ => unreachable!(),
            };
            reference[cursor_offset..cursor_offset + 4].copy_from_slice(&inclusive_pixel);
            assert_eq!(
                inclusive, reference,
                "production overlay must be byte-exact"
            );

            // Mutant self-checks prove the gate goes red for the two mistakes it
            // is meant to catch rather than merely comparing a fixture to itself.
            let mut channel_swapped = reference.clone();
            channel_swapped[cursor_offset] = inclusive_pixel[2];
            channel_swapped[cursor_offset + 2] = inclusive_pixel[0];
            assert_ne!(inclusive, channel_swapped, "channel-swap mutant must fail");
            let mut shifted_hotspot = base.clone();
            shifted_hotspot[cursor_offset + 4..cursor_offset + 8].copy_from_slice(&inclusive_pixel);
            assert_ne!(inclusive, shifted_hotspot, "hotspot-shift mutant must fail");
            assert_eq!(
                inclusive.iter().map(|byte| u64::from(*byte)).sum::<u64>(),
                40_515
            );
        }
    }
}
