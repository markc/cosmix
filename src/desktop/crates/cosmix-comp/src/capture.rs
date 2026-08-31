//! Shared one-shot output capture service.
//!
//! Wire screencopy and the optional PNG policy both enter through this module.
//! The nested implementation uses Bevy's final-output redirection, which owns
//! a COPY_SRC texture, copies it to MAP_READ staging and blits it to the host
//! swapchain in the same render submission. Conversion and cropping stay off
//! the render and Wayland threads.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};

use bevy::{
    asset::RenderAssetUsages,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    render::view::screenshot::{Capturing, Screenshot, ScreenshotCaptured},
    window::{PrimaryWindow, RequestRedraw},
};
use smithay::reexports::calloop::channel::Sender as CalloopSender;

#[cfg(feature = "frame-capture")]
use bevy::camera::ManualTextureViewHandle;

use crate::{
    compositor_scene::CompositorSceneSet,
    protocol::{CaptureCompletionReporter, ClientSceneFeed},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CaptureId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureFormat {
    #[allow(dead_code)] // protocol seam for a future alpha-preserving source
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

#[derive(Clone, Debug)]
pub(crate) struct CaptureRequest {
    pub(crate) id: CaptureId,
    pub(crate) output_name: String,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    pub(crate) region: CaptureRegion,
    pub(crate) output_size: (u32, u32),
    pub(crate) format: CaptureFormat,
    pub(crate) with_damage: bool,
    pub(crate) cancellation: CaptureCancellation,
    pub(crate) reservation: CaptureReservationLease,
    /// Absolute deadline for this one admitted request. This is not a periodic
    /// timer; it bounds a single map/screenshot completion that never arrives.
    pub(crate) deadline: Instant,
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
    release_counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
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

/// A cloneable renderer-side ownership lease. The protocol byte reservation is
/// released only when the final queue/batch/worker/result holder drops it.
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
    fn counted(id: CaptureId, releases: Arc<std::sync::atomic::AtomicUsize>) -> Self {
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
    pub(crate) frame_token: u64,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: CaptureFormat,
    pub(crate) y_invert: bool,
    /// Packed little-endian ARGB/XRGB memory order: B, G, R, A/X.
    pub(crate) packed_bgra: Arc<Vec<u8>>,
    pub(crate) _reservation: CaptureReservationLease,
}

#[derive(Clone, Debug)]
pub(crate) struct CapturePresented {
    pub(crate) id: CaptureId,
    pub(crate) frame_token: u64,
    pub(crate) generation: u64,
    pub(crate) security_epoch: u64,
    pub(crate) seconds: u64,
    pub(crate) nanoseconds: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingCapturePresentation {
    pub(crate) id: CaptureId,
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

#[derive(Resource, Default)]
pub(crate) struct CaptureQueue(Vec<CaptureRequest>);

impl CaptureQueue {
    pub(crate) fn push(&mut self, request: CaptureRequest) {
        if !request.cancellation.is_cancelled() {
            self.0.push(request);
        }
    }
}

#[derive(Resource, Default)]
struct CaptureFrameTokens(u64);

#[derive(Resource, Default)]
struct DeferredDamageCaptures(Vec<CaptureRequest>);

/// Bevy accepts one screenshot entity for a normalised render target per app
/// frame. Protocol and PNG consumers share this latch so neither can create a
/// duplicate primary-window request that Bevy would discard without a
/// completion observer.
#[derive(Resource, Default)]
struct NestedScreenshotScheduled(bool);

/// Bevy owns a staging-buffer clone after extraction, independently of the
/// main-world Screenshot entity. Keep the protocol reservations alive until a
/// `ScreenshotCaptured` notification proves that Bevy's map task completed.
/// An extracted map which never resolves therefore remains charged and makes
/// later admission fail closed instead of accumulating unaccounted GPU work.
#[derive(Resource, Default)]
struct ScreenshotCompletionHolds {
    entries: BTreeMap<Entity, ScreenshotCompletionHold>,
    saturation_logged: bool,
}

struct ScreenshotCompletionHold {
    _leases: Vec<CaptureReservationLease>,
    extracted: bool,
}

impl ScreenshotCompletionHolds {
    fn saturated(&mut self) -> bool {
        if self.entries.len() < crate::protocol::MAX_IN_FLIGHT_CAPTURES {
            self.saturation_logged = false;
            return false;
        }
        if !self.saturation_logged {
            self.saturation_logged = true;
            tracing::warn!(
                limit = crate::protocol::MAX_IN_FLIGHT_CAPTURES,
                "screenshot completion limit saturated; capture admission is failing closed"
            );
        }
        true
    }

    fn track(&mut self, entity: Entity, requests: &[CaptureRequest]) {
        let previous = self.entries.insert(
            entity,
            ScreenshotCompletionHold {
                _leases: requests
                    .iter()
                    .map(|request| request.reservation.clone())
                    .collect(),
                extracted: false,
            },
        );
        debug_assert!(previous.is_none());
    }

    fn mark_extracted(&mut self, entity: Entity) {
        if let Some(hold) = self.entries.get_mut(&entity) {
            hold.extracted = true;
        }
    }

    fn remove_unextracted(&mut self, entity: Entity) {
        if self
            .entries
            .get(&entity)
            .is_some_and(|hold| !hold.extracted)
        {
            self.entries.remove(&entity);
        }
        self.reset_episode_if_below_limit();
    }

    fn completion_observed(&mut self, entity: Entity) {
        self.entries.remove(&entity);
        self.reset_episode_if_below_limit();
    }

    fn reset_episode_if_below_limit(&mut self) {
        if self.entries.len() < crate::protocol::MAX_IN_FLIGHT_CAPTURES {
            self.saturation_logged = false;
        }
    }
}

#[derive(Component)]
struct PendingCaptureBatch {
    frame_token: u64,
    requests: Vec<CaptureRequest>,
    #[cfg(feature = "frame-capture")]
    png_requests: Vec<PngCaptureAdmission>,
}

struct CaptureConversionJob {
    image: bevy::image::Image,
    frame_token: u64,
    requests: Vec<CaptureRequest>,
    #[cfg(feature = "frame-capture")]
    png_requests: Vec<PngCaptureAdmission>,
    reporter: Option<CaptureCompletionReporter>,
}

#[derive(Resource)]
struct CaptureConversionWorker(mpsc::SyncSender<CaptureConversionJob>);

impl Default for CaptureConversionWorker {
    fn default() -> Self {
        let (sender, receiver) = mpsc::sync_channel::<CaptureConversionJob>(8);
        std::thread::Builder::new()
            .name("cosmix-capture-convert".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    convert_capture_job(job);
                }
            })
            .expect("spawn bounded capture conversion worker");
        Self(sender)
    }
}

pub(crate) struct CaptureServicePlugin;

impl Plugin for CaptureServicePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CaptureQueue>()
            .init_resource::<CaptureFrameTokens>()
            .init_resource::<DeferredDamageCaptures>()
            .init_resource::<CapturePresentationPending>()
            .init_resource::<NestedScreenshotScheduled>()
            .init_resource::<ScreenshotCompletionHolds>()
            .init_resource::<CaptureConversionWorker>()
            .add_systems(
                First,
                (
                    reset_nested_screenshot_latch,
                    retain_extracted_screenshot_leases,
                    expire_nested_captures,
                    schedule_nested_capture,
                )
                    .chain()
                    .after(CompositorSceneSet),
            )
            .add_observer(complete_nested_capture);
        #[cfg(feature = "frame-capture")]
        app.init_resource::<PngCaptureService>();
    }
}

#[cfg(feature = "frame-capture")]
#[derive(Clone)]
pub(crate) enum PngCaptureTarget {
    Nested,
    TextureView(ManualTextureViewHandle),
}

#[cfg(feature = "frame-capture")]
#[derive(Clone)]
pub(crate) struct PngCaptureRequest {
    pub(crate) target: PngCaptureTarget,
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
        let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(requests.len()));
        let deadline = Instant::now()
            .checked_add(crate::protocol::CAPTURE_REQUEST_TIMEOUT)
            .unwrap_or_else(Instant::now);
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(requests.into_iter().map(|request| PngCaptureAdmission {
                request,
                deadline,
                cancellation: CaptureCancellation::default(),
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
    cancellation: CaptureCancellation,
    finished: Arc<AtomicBool>,
    batch_remaining: Arc<std::sync::atomic::AtomicUsize>,
    service: PngCaptureService,
}

#[cfg(feature = "frame-capture")]
impl PngCaptureAdmission {
    fn is_cancelled_or_expired(&self, now: Instant) -> bool {
        self.cancellation.is_cancelled() || self.deadline <= now
    }

    fn complete(&self) {
        self.cancellation.cancel();
        if !self.finished.swap(true, Ordering::AcqRel)
            && self.batch_remaining.fetch_sub(1, Ordering::AcqRel) == 1
        {
            self.service.batch_in_flight.store(false, Ordering::Release);
        }
    }
}

fn reset_nested_screenshot_latch(mut scheduled: ResMut<NestedScreenshotScheduled>) {
    scheduled.0 = false;
}

fn retain_extracted_screenshot_leases(
    mut holds: ResMut<ScreenshotCompletionHolds>,
    extracted: Query<Entity, (With<Screenshot>, With<Capturing>)>,
    screenshots: Query<(), With<Screenshot>>,
) {
    for entity in &extracted {
        holds.mark_extracted(entity);
    }
    holds
        .entries
        .retain(|entity, hold| hold.extracted || screenshots.get(*entity).is_ok());
    holds.reset_episode_if_below_limit();
}

#[allow(clippy::too_many_arguments)] // each argument is an independently scheduled ECS resource
fn schedule_nested_capture(
    mut commands: Commands,
    mut queue: ResMut<CaptureQueue>,
    mut deferred_damage: ResMut<DeferredDamageCaptures>,
    mut tokens: ResMut<CaptureFrameTokens>,
    presentations: Res<CapturePresentationPending>,
    windows: Query<(), With<PrimaryWindow>>,
    reporter: Option<Res<CaptureCompletionReporter>>,
    feed: Option<Res<ClientSceneFeed>>,
    mut nested_scheduled: ResMut<NestedScreenshotScheduled>,
    mut screenshot_holds: ResMut<ScreenshotCompletionHolds>,
    mut redraw: MessageWriter<RequestRedraw>,
    #[cfg(feature = "frame-capture")] png_service: Res<PngCaptureService>,
) {
    // `copy_with_damage` is armed first and becomes eligible only on the next
    // compositor update. S-1a reports that whole frame as damage; S-1b replaces
    // this one-update eligibility seam with the per-output damage journal.
    let mut requests = std::mem::take(&mut deferred_damage.0);
    for request in std::mem::take(&mut queue.0) {
        if request.with_damage {
            deferred_damage.0.push(request);
        } else {
            requests.push(request);
        }
    }
    #[cfg(feature = "frame-capture")]
    let mut png_requests = std::mem::take(
        &mut *png_service
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    #[cfg(feature = "frame-capture")]
    {
        let now = Instant::now();
        png_requests.retain(|request| {
            if request.is_cancelled_or_expired(now) {
                request.complete();
                false
            } else {
                true
            }
        });
    }
    if requests.is_empty() && {
        #[cfg(feature = "frame-capture")]
        {
            png_requests.is_empty()
        }
        #[cfg(not(feature = "frame-capture"))]
        {
            true
        }
    } {
        return;
    }
    let reporter = reporter
        .map(|reporter| reporter.clone())
        .or_else(|| feed.as_ref().map(|feed| feed.capture_completion_reporter()));
    requests.retain(|request| !request.cancellation.is_cancelled());
    if let Some(reporter) = &reporter {
        fail_expired_requests(&mut requests, Instant::now(), |request| {
            reporter.failed(request.id, request.generation, request.security_epoch);
        });
    } else {
        requests.clear();
    }
    if windows.is_empty() {
        for request in requests {
            if let Some(reporter) = &reporter {
                reporter.failed(request.id, request.generation, request.security_epoch);
            }
        }
        requests = Vec::new();
    }

    // Every nested consumer fans out from one redirected final-output snapshot.
    // Bevy silently drops duplicate screenshot entities for one normalised
    // render target, so wire and PNG requests must share this entity.
    let mut groups = BTreeMap::<String, Vec<CaptureRequest>>::new();
    for request in requests {
        groups
            .entry(request.output_name.clone())
            .or_default()
            .push(request);
    }
    #[cfg(feature = "frame-capture")]
    let mut nested_png = Vec::new();
    #[cfg(feature = "frame-capture")]
    let mut texture_png = Vec::new();
    #[cfg(feature = "frame-capture")]
    for request in png_requests {
        match request.request.target {
            PngCaptureTarget::Nested => nested_png.push(request),
            PngCaptureTarget::TextureView(handle) => texture_png.push((handle, request)),
        }
    }

    let mut nested_screenshot_spawned = false;
    for (_output_name, requests) in groups {
        let requests = requests
            .into_iter()
            .filter(|request| !request.cancellation.is_cancelled())
            .collect::<Vec<_>>();
        if requests.is_empty() {
            continue;
        }
        #[cfg(feature = "frame-capture")]
        let png_for_batch = std::mem::take(&mut nested_png);
        if screenshot_holds.saturated() {
            if let Some(reporter) = &reporter {
                for request in &requests {
                    reporter.failed(request.id, request.generation, request.security_epoch);
                }
            }
            #[cfg(feature = "frame-capture")]
            for request in png_for_batch {
                request.complete();
            }
            continue;
        }
        tokens.0 = tokens.0.wrapping_add(1).max(1);
        let frame_token = tokens.0;
        presentations.publish(requests.iter().map(|request| PendingCapturePresentation {
            id: request.id,
            frame_token,
            generation: request.generation,
            security_epoch: request.security_epoch,
            deadline: request.deadline,
        }));
        nested_scheduled.0 = true;
        nested_screenshot_spawned = true;
        write_nested_capture_redraw(&requests, &mut redraw);
        let entity = commands
            .spawn((
                Screenshot::primary_window(),
                PendingCaptureBatch {
                    frame_token,
                    requests: requests.clone(),
                    #[cfg(feature = "frame-capture")]
                    png_requests: png_for_batch,
                },
            ))
            .id();
        screenshot_holds.track(entity, &requests);
    }
    #[cfg(feature = "frame-capture")]
    if !nested_png.is_empty() && !nested_screenshot_spawned {
        if windows.is_empty() || screenshot_holds.saturated() {
            for request in nested_png {
                request.complete();
            }
        } else {
            tokens.0 = tokens.0.wrapping_add(1).max(1);
            nested_scheduled.0 = true;
            redraw.write(RequestRedraw);
            let entity = commands
                .spawn((
                    Screenshot::primary_window(),
                    PendingCaptureBatch {
                        frame_token: tokens.0,
                        requests: Vec::new(),
                        png_requests: nested_png,
                    },
                ))
                .id();
            screenshot_holds.track(entity, &[]);
        }
    }
    #[cfg(feature = "frame-capture")]
    for (handle, request) in texture_png {
        if screenshot_holds.saturated() {
            request.complete();
            continue;
        }
        tokens.0 = tokens.0.wrapping_add(1).max(1);
        let entity = commands
            .spawn((
                Screenshot::texture_view(handle),
                PendingCaptureBatch {
                    frame_token: tokens.0,
                    requests: Vec::new(),
                    png_requests: vec![request],
                },
            ))
            .id();
        screenshot_holds.track(entity, &[]);
    }
}

fn write_nested_capture_redraw(
    requests: &[CaptureRequest],
    redraw: &mut MessageWriter<RequestRedraw>,
) {
    if !requests.is_empty() {
        redraw.write(RequestRedraw);
    }
}

fn expire_nested_captures(
    mut commands: Commands,
    mut batches: Query<(Entity, &mut PendingCaptureBatch)>,
    feed: Option<Res<ClientSceneFeed>>,
    reporter: Option<Res<CaptureCompletionReporter>>,
    mut screenshot_holds: ResMut<ScreenshotCompletionHolds>,
) {
    let now = Instant::now();
    let reporter = reporter
        .map(|reporter| reporter.clone())
        .or_else(|| feed.as_ref().map(|feed| feed.capture_completion_reporter()));
    for (entity, mut batch) in &mut batches {
        batch
            .requests
            .retain(|request| !request.cancellation.is_cancelled());
        if let Some(reporter) = &reporter {
            fail_expired_requests(&mut batch.requests, now, |request| {
                reporter.failed(request.id, request.generation, request.security_epoch);
            });
        }
        #[cfg(feature = "frame-capture")]
        {
            batch.png_requests.retain(|request| {
                if request.is_cancelled_or_expired(now) {
                    request.complete();
                    false
                } else {
                    true
                }
            });
        }
        if batch.requests.is_empty() && {
            #[cfg(feature = "frame-capture")]
            {
                batch.png_requests.is_empty()
            }
            #[cfg(not(feature = "frame-capture"))]
            {
                true
            }
        } {
            screenshot_holds.remove_unextracted(entity);
            commands.entity(entity).despawn();
        }
    }
}

fn fail_expired_requests(
    requests: &mut Vec<CaptureRequest>,
    now: Instant,
    mut fail: impl FnMut(&CaptureRequest),
) {
    requests.retain(|request| {
        if request.deadline > now {
            true
        } else {
            fail(request);
            false
        }
    });
}

fn complete_nested_capture(
    captured: On<ScreenshotCaptured>,
    pending: Query<&PendingCaptureBatch>,
    feed: Option<Res<ClientSceneFeed>>,
    reporter: Option<Res<CaptureCompletionReporter>>,
    worker: Res<CaptureConversionWorker>,
    mut screenshot_holds: ResMut<ScreenshotCompletionHolds>,
) {
    screenshot_holds.completion_observed(captured.entity);
    let Ok(batch) = pending.get(captured.entity) else {
        return;
    };
    let requests = batch
        .requests
        .iter()
        .filter(|request| !request.cancellation.is_cancelled())
        .cloned()
        .collect::<Vec<_>>();
    let reporter = reporter
        .map(|reporter| reporter.clone())
        .or_else(|| feed.as_ref().map(|feed| feed.capture_completion_reporter()));
    #[cfg(feature = "frame-capture")]
    let png_requests = batch
        .png_requests
        .iter()
        .filter_map(|request| {
            if request.is_cancelled_or_expired(Instant::now()) {
                request.complete();
                None
            } else {
                Some(request.clone())
            }
        })
        .collect::<Vec<_>>();
    if requests.is_empty() && {
        #[cfg(feature = "frame-capture")]
        {
            png_requests.is_empty()
        }
        #[cfg(not(feature = "frame-capture"))]
        {
            true
        }
    } {
        return;
    }
    let job = CaptureConversionJob {
        image: captured.image.clone(),
        frame_token: batch.frame_token,
        requests,
        #[cfg(feature = "frame-capture")]
        png_requests,
        reporter,
    };
    if let Err(mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) =
        worker.0.try_send(job)
    {
        for request in job.requests {
            if let Some(reporter) = &job.reporter {
                reporter.failed(request.id, request.generation, request.security_epoch);
            }
        }
        #[cfg(feature = "frame-capture")]
        for request in job.png_requests {
            request.complete();
        }
    }
}

fn convert_capture_job(job: CaptureConversionJob) {
    let wire_cancelled = job
        .requests
        .iter()
        .all(|request| request.cancellation.is_cancelled());
    #[cfg(feature = "frame-capture")]
    let png_cancelled = job
        .png_requests
        .iter()
        .all(|request| request.cancellation.is_cancelled());
    #[cfg(not(feature = "frame-capture"))]
    let png_cancelled = true;
    if wire_cancelled && png_cancelled {
        #[cfg(feature = "frame-capture")]
        for request in job.png_requests {
            request.complete();
        }
        return;
    }
    let rgba = match job.image.try_into_dynamic() {
        Ok(image) => image.to_rgba8(),
        Err(_) => {
            for request in job.requests {
                if let Some(reporter) = &job.reporter {
                    reporter.failed(request.id, request.generation, request.security_epoch);
                }
            }
            #[cfg(feature = "frame-capture")]
            for request in job.png_requests {
                request.complete();
            }
            return;
        }
    };
    let source_size = (rgba.width(), rgba.height());
    for request in job.requests {
        if request.cancellation.is_cancelled() {
            continue;
        }
        if request.deadline <= Instant::now() {
            if let Some(reporter) = &job.reporter {
                reporter.failed(request.id, request.generation, request.security_epoch);
            }
            continue;
        }
        match convert_capture(rgba.as_raw(), source_size, &request) {
            Some(packed_bgra) => {
                if let Some(reporter) = &job.reporter {
                    publish_capture_pixels(&request, job.frame_token, packed_bgra, reporter);
                }
            }
            None => {
                if let Some(reporter) = &job.reporter {
                    reporter.failed(request.id, request.generation, request.security_epoch);
                }
            }
        }
    }
    #[cfg(feature = "frame-capture")]
    for request in job.png_requests {
        if request.is_cancelled_or_expired(Instant::now()) {
            request.complete();
            continue;
        }
        let Some(packed_bgra) = pack_png_bgra(rgba.as_raw(), source_size) else {
            request.complete();
            continue;
        };
        if request.cancellation.is_cancelled() {
            request.complete();
            continue;
        }
        let image = bevy::image::Image::new(
            Extent3d {
                width: source_size.0,
                height: source_size.1,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            packed_bgra,
            TextureFormat::Bgra8UnormSrgb,
            RenderAssetUsages::MAIN_WORLD,
        );
        let completion = request.clone();
        crate::frame_capture::save_capture_image(
            image,
            request.request.temporary_path.clone(),
            request.request.final_path.clone(),
            request.deadline,
            move || completion.complete(),
        );
    }
}

fn publish_capture_pixels(
    request: &CaptureRequest,
    frame_token: u64,
    packed_bgra: Vec<u8>,
    reporter: &CaptureCompletionReporter,
) {
    if request.cancellation.is_cancelled() {
        return;
    }
    reporter.pixels(CapturePixels {
        id: request.id,
        frame_token,
        generation: request.generation,
        security_epoch: request.security_epoch,
        width: request.region.width,
        height: request.region.height,
        format: request.format,
        y_invert: false,
        packed_bgra: Arc::new(packed_bgra),
        _reservation: request.reservation.clone(),
    });
}

#[cfg(feature = "frame-capture")]
fn pack_png_bgra(rgba: &[u8], source_size: (u32, u32)) -> Option<Vec<u8>> {
    let pixel_count = usize::try_from(source_size.0)
        .ok()?
        .checked_mul(usize::try_from(source_size.1).ok()?)?;
    let byte_count = pixel_count.checked_mul(4)?;
    if rgba.len() != byte_count {
        return None;
    }
    let mut packed = vec![0_u8; byte_count];
    for (source, destination) in rgba.chunks_exact(4).zip(packed.chunks_exact_mut(4)) {
        destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
    }
    Some(packed)
}

fn convert_capture(
    rgba: &[u8],
    source_size: (u32, u32),
    request: &CaptureRequest,
) -> Option<Vec<u8>> {
    if source_size != request.output_size {
        return None;
    }
    let region = request.region;
    let right = region.x.checked_add(region.width)?;
    let bottom = region.y.checked_add(region.height)?;
    if right > source_size.0 || bottom > source_size.1 {
        return None;
    }
    let row_bytes = usize::try_from(region.width).ok()?.checked_mul(4)?;
    let output_bytes = row_bytes.checked_mul(usize::try_from(region.height).ok()?)?;
    let source_stride = usize::try_from(source_size.0).ok()?.checked_mul(4)?;
    let mut packed = vec![0_u8; output_bytes];
    for row in 0..usize::try_from(region.height).ok()? {
        let source_start = usize::try_from(region.y)
            .ok()?
            .checked_add(row)?
            .checked_mul(source_stride)?
            .checked_add(usize::try_from(region.x).ok()?.checked_mul(4)?)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CaptureTestOutcome;

    fn request(
        format: CaptureFormat,
        region: CaptureRegion,
        output_size: (u32, u32),
    ) -> CaptureRequest {
        CaptureRequest {
            id: CaptureId(1),
            output_name: "cosmix-nested-0".into(),
            generation: 7,
            security_epoch: 11,
            region,
            output_size,
            format,
            with_damage: false,
            cancellation: CaptureCancellation::default(),
            reservation: CaptureReservationLease::detached(CaptureId(1)),
            deadline: Instant::now() + crate::protocol::CAPTURE_REQUEST_TIMEOUT,
        }
    }

    #[test]
    fn screencopy_s1a_05_probe_uses_real_xrgb_conversion_output() {
        let request = request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            (2, 1),
        );
        let pixels = convert_capture(&[1, 2, 3, 4, 10, 20, 30, 40], (2, 1), &request)
            .expect("probe-shaped capture converts");
        assert_eq!(pixels, vec![3, 2, 1, 0xff, 30, 20, 10, 0xff]);
    }

    #[test]
    fn screencopy_s1a_06_non_zero_offset_guard_contract() {
        let image = vec![9_u8; 32];
        let mut pool = vec![0xa5_u8; 16 + image.len() + 16];
        pool[16..16 + image.len()].copy_from_slice(&image);
        assert!(pool[..16].iter().all(|byte| *byte == 0xa5));
        assert!(pool[16 + image.len()..].iter().all(|byte| *byte == 0xa5));
    }

    #[test]
    fn screencopy_s1a_07_argb_xrgb_and_channel_conversion() {
        let region = CaptureRegion {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let xrgb = request(CaptureFormat::Xrgb8888, region, (1, 1));
        let argb = request(CaptureFormat::Argb8888, region, (1, 1));
        assert_eq!(
            convert_capture(&[1, 2, 3, 4], (1, 1), &xrgb),
            Some(vec![3, 2, 1, 255])
        );
        assert_eq!(
            convert_capture(&[1, 2, 3, 4], (1, 1), &argb),
            Some(vec![3, 2, 1, 4])
        );
    }

    #[test]
    fn screencopy_s1a_11_y_invert_is_metadata_not_row_mutation() {
        let rows = Arc::new(vec![1, 2, 3, 255, 4, 5, 6, 255]);
        let pixels = CapturePixels {
            id: CaptureId(1),
            frame_token: 1,
            generation: 1,
            security_epoch: 1,
            width: 1,
            height: 2,
            format: CaptureFormat::Xrgb8888,
            y_invert: true,
            packed_bgra: Arc::clone(&rows),
            _reservation: CaptureReservationLease::detached(CaptureId(1)),
        };
        assert!(pixels.y_invert);
        assert_eq!(&*pixels.packed_bgra, &*rows);
    }

    #[test]
    fn screencopy_s1a_15_same_variant_groups_one_snapshot() {
        let mut groups = BTreeMap::<String, Vec<CaptureRequest>>::new();
        for id in [1, 2] {
            let mut item = request(
                CaptureFormat::Xrgb8888,
                CaptureRegion {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                (1, 1),
            );
            item.id = CaptureId(id);
            groups
                .entry(item.output_name.clone())
                .or_default()
                .push(item);
        }
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.values().next().unwrap().len(), 2);
    }

    #[test]
    fn screencopy_s1a_16_crop_edges_have_no_pitch_skew() {
        let rgba = [
            1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255,
        ];
        let cropped = request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            },
            (3, 2),
        );
        assert_eq!(
            convert_capture(&rgba, (3, 2), &cropped),
            Some(vec![0, 0, 2, 255, 0, 0, 3, 255, 0, 0, 5, 255, 0, 0, 6, 255])
        );
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn screencopy_s1a_19_png_consumer_preserves_one_in_flight_latch() {
        let service = PngCaptureService::default();
        let target = PngCaptureRequest {
            target: PngCaptureTarget::Nested,
            final_path: "capture.png".into(),
            temporary_path: "capture.tmp".into(),
        };
        assert!(service.submit_batch(vec![target.clone()]));
        assert!(service.busy());
        assert!(!service.submit_batch(vec![target]));
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn png_screenshot_deadline_releases_the_batch_in_flight_latch() {
        let service = PngCaptureService::default();
        assert!(service.submit_batch(vec![PngCaptureRequest {
            target: PngCaptureTarget::Nested,
            final_path: "capture.png".into(),
            temporary_path: "capture.tmp".into(),
        }]));
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_plugins(CaptureServicePlugin)
            .insert_resource(service.clone());
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        app.update();
        let screenshot = app
            .world_mut()
            .query_filtered::<Entity, With<Screenshot>>()
            .single(app.world())
            .expect("PNG admission creates the shared Screenshot entity");
        app.world_mut()
            .entity_mut(screenshot)
            .get_mut::<PendingCaptureBatch>()
            .expect("Screenshot retains its capture consumers")
            .png_requests[0]
            .deadline = Instant::now();
        app.update();
        assert!(!service.busy());
        assert!(app.world().get_entity(screenshot).is_err());
    }

    #[test]
    fn extracted_deadline_retains_reservation_until_late_completion_exactly_once() {
        let releases = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_events, feed) = ClientSceneFeed::test_channel();
        let mut item = request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            (1, 1),
        );
        item.reservation = CaptureReservationLease::counted(item.id, Arc::clone(&releases));

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_plugins(CaptureServicePlugin)
            .insert_resource(feed.capture_completion_reporter());
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        app.world_mut().resource_mut::<CaptureQueue>().push(item);
        app.update();
        let screenshot = app
            .world_mut()
            .query_filtered::<Entity, With<Screenshot>>()
            .single(app.world())
            .expect("capture admission creates a Screenshot entity");
        app.world_mut().entity_mut(screenshot).insert(Capturing);
        app.world_mut()
            .entity_mut(screenshot)
            .get_mut::<PendingCaptureBatch>()
            .expect("Screenshot retains its wire request")
            .requests[0]
            .deadline = Instant::now();

        app.update();
        assert!(app.world().get_entity(screenshot).is_err());
        assert_eq!(releases.load(Ordering::Acquire), 0);

        app.world_mut().trigger(ScreenshotCaptured {
            image: bevy::image::Image::default(),
            entity: screenshot,
        });
        assert_eq!(releases.load(Ordering::Acquire), 1);
        app.world_mut().trigger(ScreenshotCaptured {
            image: bevy::image::Image::default(),
            entity: screenshot,
        });
        assert_eq!(releases.load(Ordering::Acquire), 1);
    }

    #[test]
    fn screenshot_completion_saturation_fails_new_wire_copy() {
        let (_events, feed) = ClientSceneFeed::test_channel();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_plugins(CaptureServicePlugin)
            .insert_resource(feed.capture_completion_reporter());
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        for raw in 1..=crate::protocol::MAX_IN_FLIGHT_CAPTURES {
            let entity = app.world_mut().spawn_empty().id();
            app.world_mut()
                .resource_mut::<ScreenshotCompletionHolds>()
                .entries
                .insert(
                    entity,
                    ScreenshotCompletionHold {
                        _leases: vec![CaptureReservationLease::detached(CaptureId(raw as u64))],
                        extracted: true,
                    },
                );
        }
        app.world_mut().resource_mut::<CaptureQueue>().push(request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            (1, 1),
        ));

        app.update();
        assert_eq!(
            feed.capture_outcomes_for_test(),
            vec![CaptureTestOutcome::Failed(CaptureId(1))]
        );
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<Screenshot>>()
                .iter(app.world())
                .count(),
            0
        );
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn png_and_wire_consumers_share_one_nested_screenshot_batch() {
        let service = PngCaptureService::default();
        assert!(service.submit_batch(vec![PngCaptureRequest {
            target: PngCaptureTarget::Nested,
            final_path: "capture.png".into(),
            temporary_path: "capture.tmp".into(),
        }]));
        let (_events, feed) = ClientSceneFeed::test_channel();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_plugins(CaptureServicePlugin)
            .insert_resource(service)
            .insert_resource(feed.capture_completion_reporter());
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        app.world_mut().resource_mut::<CaptureQueue>().push(request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            (1, 1),
        ));
        app.update();
        let mut screenshots = app
            .world_mut()
            .query_filtered::<&PendingCaptureBatch, With<Screenshot>>();
        let batches = screenshots.iter(app.world()).collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].requests.len(), 1);
        assert_eq!(batches[0].png_requests.len(), 1);
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn png_expiry_between_first_sweep_and_observer_completes_the_admission() {
        let service = PngCaptureService::default();
        assert!(service.submit_batch(vec![PngCaptureRequest {
            target: PngCaptureTarget::Nested,
            final_path: "capture.png".into(),
            temporary_path: "capture.tmp".into(),
        }]));
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_plugins(CaptureServicePlugin)
            .insert_resource(service.clone());
        app.world_mut().spawn((Window::default(), PrimaryWindow));
        app.update();
        let screenshot = app
            .world_mut()
            .query_filtered::<Entity, With<Screenshot>>()
            .single(app.world())
            .expect("PNG admission creates a Screenshot entity");
        app.world_mut()
            .entity_mut(screenshot)
            .get_mut::<PendingCaptureBatch>()
            .expect("Screenshot retains its PNG request")
            .png_requests[0]
            .deadline = Instant::now();

        app.world_mut().trigger(ScreenshotCaptured {
            image: bevy::image::Image::default(),
            entity: screenshot,
        });
        assert!(!service.busy());
    }

    #[test]
    fn plain_copy_requests_a_redraw_without_background_animation() {
        let item = request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            (1, 1),
        );
        let mut world = World::new();
        world.init_resource::<Messages<RequestRedraw>>();
        let mut state =
            bevy::ecs::system::SystemState::<MessageWriter<RequestRedraw>>::new(&mut world);
        write_nested_capture_redraw(
            &[item],
            &mut state
                .get_mut(&mut world)
                .expect("RequestRedraw messages are initialised"),
        );
        state.apply(&mut world);
        assert_eq!(
            world.resource::<Messages<RequestRedraw>>().len(),
            1,
            "production admission writes RequestRedraw independently of animation"
        );
    }

    #[test]
    fn capture_worker_missing_completion_expiry_and_shutdown_are_bounded() {
        let started = Instant::now();
        let worker = CaptureConversionWorker::default();
        let (_events, feed) = ClientSceneFeed::test_channel();
        let reporter = feed.capture_completion_reporter();
        let mut active = vec![request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            (1, 1),
        )];
        active[0].deadline = Instant::now();
        fail_expired_requests(&mut active, Instant::now(), |request| {
            reporter.failed(request.id, request.generation, request.security_epoch);
        });
        drop(worker);
        assert!(active.is_empty());
        assert_eq!(
            feed.capture_outcomes_for_test(),
            vec![CaptureTestOutcome::Failed(CaptureId(1))]
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn cancelled_ecs_request_spawns_no_screenshot_and_enters_no_worker_queue() {
        let item = request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            (1, 1),
        );
        item.cancellation.cancel();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_plugins(CaptureServicePlugin);
        app.world_mut().resource_mut::<CaptureQueue>().push(item);
        app.update();
        assert!(app.world().resource::<CaptureQueue>().0.is_empty());
        assert_eq!(
            app.world_mut()
                .query::<&Screenshot>()
                .iter(app.world())
                .count(),
            0
        );
    }

    #[test]
    fn cancelled_worker_result_is_dropped_before_pixel_publication() {
        let (_events, feed) = ClientSceneFeed::test_channel();
        let item = request(
            CaptureFormat::Xrgb8888,
            CaptureRegion {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            (1, 1),
        );
        let packed = convert_capture(&[1, 2, 3, 0xff], (1, 1), &item)
            .expect("worker packed pixels before cancellation arrived");
        item.cancellation.cancel();
        publish_capture_pixels(&item, 1, packed, &feed.capture_completion_reporter());
        assert!(feed.capture_outcomes_for_test().is_empty());
    }
}
