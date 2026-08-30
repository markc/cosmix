//! Render-schedule seam for KMS output frames.
//!
//! A live source may block in [`acquire_output_frames`] while FIFO waits
//! for a swapchain image. That system runs only in Bevy's render schedule. The
//! Wayland frontend is owned by `cosmix-wayland`, so registry dispatch and
//! other protocol work do not wait for acquisition, rendering, or present.
//! This module does not open DRM nodes or create presentation surfaces.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    time::{Duration, Instant},
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use std::collections::BTreeSet;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::TrySendError,
};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

#[cfg(all(feature = "kms-live", not(test)))]
use std::thread;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use std::{sync::mpsc::SyncSender, thread::JoinHandle};

#[cfg(all(feature = "kms-live", not(test)))]
use std::os::fd::AsFd;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use bevy::ecs::system::RunSystemOnce;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use bevy::{
    app::AppExit,
    render::error_handler::{RenderError, RenderErrorPolicy},
};

#[cfg(all(feature = "kms-live", not(test)))]
use bevy::render::error_handler::RenderErrorHandler;

use bevy::{
    app::{App, Plugin, SubApp},
    camera::{
        Camera, CameraOutputMode, ClearColorConfig, ManualTextureViewHandle,
        NormalizedRenderTarget, RenderTarget,
    },
    prelude::{
        Camera2d, ClearColor, Component, Entity, IntoScheduleConfigs, Msaa, Name, Resource, World,
    },
    render::{
        Render, RenderApp, RenderSystems,
        camera::ExtractedCamera,
        render_resource::{CommandEncoderDescriptor, RenderPassDescriptor},
        renderer::{RenderDevice, RenderQueue, render_system},
        texture::{ManualTextureView, ManualTextureViews},
        view::{ViewTarget, prepare_view_attachments},
    },
};

use bevy::camera::{OrthographicProjection, Projection, ScalingMode};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use bevy::{
    DefaultPlugins,
    app::{PluginGroup, PluginGroupBuilder, TerminalCtrlCHandlerPlugin},
    log::LogPlugin,
    render::pipelined_rendering::{PipelinedRenderingPlugin, RenderExtractApp},
    window::{ExitCondition, WindowPlugin},
    winit::WinitPlugin,
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use bevy::prelude::{
    Color, Commands, Quat, Query, Res, ResMut, Sprite, Time, Transform, Vec2, With,
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use cosmix_wgpu_dmabuf::{DmabufImportPlugin, ImportedDmabufImages};

#[cfg(all(feature = "kms-live", not(test)))]
use cosmix_wgpu_dmabuf::DmabufProbePlugin;

#[cfg(all(feature = "kms-live", not(test)))]
use bevy::render::render_resource as gpu;

#[cfg(all(feature = "kms-live", not(test)))]
use bevy::render::view::ExtractedView;

#[cfg(all(feature = "kms-live", not(test)))]
use bevy::prelude::Local;

#[cfg(all(feature = "kms-live", not(test)))]
use crate::compositor_scene::{DmabufOutputProbeSurfaces, install_dmabuf_output_probe};
#[cfg(all(feature = "kms-live", not(test)))]
use crate::decoration::DecorationStartup;
#[cfg(all(feature = "kms-live", not(test)))]
use crate::decoration_scene::init_chrome_font_cx;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use crate::{
    compositor_scene::{
        CompositorScenePlugin, DmabufOutputProbeSurface, SceneCursorMode, drain_protocol_events,
        set_compositor_logical_output_geometry,
    },
    protocol::ClientSceneFeed,
};

use super::{
    kms::{KmsRenderOperation, KmsRenderReply, OutputKey},
    worker::{
        KmsRenderInputSender, KmsRenderJoinOutcome, KmsRenderLifecycle, KmsRenderLifecycleState,
        KmsRenderPlatform, KmsRenderPlatformFailure, KmsRenderQuiescence,
        KmsRenderQuiescenceOutcome, KmsRenderRegistration, KmsRenderRegistrationDisposition,
        KmsRenderRelease, KmsRenderReleaseOutcome, KmsRenderWorker, KmsRenderWorkerEvent,
        KmsRenderWorkerFailure, KmsRenderWorkerStop, RegistrarEffect, RenderSource,
        RenderSourceRegistrar, RenderSourceRegistrarError,
    },
};

#[cfg(all(feature = "kms-live", not(test)))]
use super::atomic_presentation::{
    AtomicCancellation, AtomicPresenter, PendingFlipDrainOutcome, ProductionAtomicEventRouter,
    ProductionAtomicIo,
};

#[cfg(all(feature = "kms-live", not(test)))]
use super::scanout_pool::{RetainedScanoutBuffer, ScanoutPool, ScanoutPoolConfig};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use super::scanout_pool::ScanoutSlotId;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use super::worker::KmsRenderWorkerExit;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PresentDeadline(Option<Instant>);

impl PresentDeadline {
    /// Explicitly unbounded only for render paths which never submit KMS work.
    pub(crate) const fn unbounded_non_presenting() -> Self {
        Self(None)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn bounded(deadline: Instant) -> Self {
        Self(Some(deadline))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn instant(self) -> Option<Instant> {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum PresentOutcome {
    Displayed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) struct ResumePresentationPlan {
    pub(crate) classification: super::resume_scanout::ResumePresentationClassification,
    pub(crate) deadline: PresentDeadline,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) struct StagedResumeLease {
    pub(crate) lease: super::kms_live::MasterDrmLease,
    pub(crate) presentation: ResumePresentationPlan,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn seamless_resume_is_eligible(
    plan: Option<ResumePresentationPlan>,
    retained_buffer_exists: bool,
    selection_matches: bool,
) -> bool {
    retained_buffer_exists
        && selection_matches
        && matches!(
            plan,
            Some(ResumePresentationPlan {
                classification:
                    super::resume_scanout::ResumePresentationClassification::SeamlessPageFlip,
                ..
            })
        )
}

#[cfg(test)]
pub(crate) fn staged_resume_lease_for_test(
    lease: super::kms_live::MasterDrmLease,
) -> StagedResumeLease {
    StagedResumeLease {
        lease,
        presentation: ResumePresentationPlan {
            classification:
                super::resume_scanout::ResumePresentationClassification::ModesetRequired(
                    super::resume_scanout::ResumeModesetReason::NoUsableState,
                ),
            deadline: PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
        },
    }
}

pub(crate) trait PresentOutputOperation: Send + Sync + 'static {
    fn present(
        self: Box<Self>,
        deadline: PresentDeadline,
    ) -> Result<PresentOutcome, KmsRenderPlatformFailure>;
}

/// Adapter for infallible non-KMS test and nested presentation closures.
impl<F> PresentOutputOperation for F
where
    F: FnOnce() + Send + Sync + 'static,
{
    fn present(
        self: Box<Self>,
        _deadline: PresentDeadline,
    ) -> Result<PresentOutcome, KmsRenderPlatformFailure> {
        (*self)();
        Ok(PresentOutcome::Displayed)
    }
}

struct FalliblePresentOutput<F>(F);

impl<F> PresentOutputOperation for FalliblePresentOutput<F>
where
    F: FnOnce(PresentDeadline) -> Result<PresentOutcome, KmsRenderPlatformFailure>
        + Send
        + Sync
        + 'static,
{
    fn present(
        self: Box<Self>,
        deadline: PresentDeadline,
    ) -> Result<PresentOutcome, KmsRenderPlatformFailure> {
        let Self(present) = *self;
        present(deadline)
    }
}

pub(crate) type PresentOutputFrame = Box<dyn PresentOutputOperation>;

pub(crate) fn present_output_frame(
    present: PresentOutputFrame,
    deadline: PresentDeadline,
) -> Result<PresentOutcome, KmsRenderPlatformFailure> {
    present.present(deadline)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn fallible_present_output_frame<F>(present: F) -> PresentOutputFrame
where
    F: FnOnce(PresentDeadline) -> Result<PresentOutcome, KmsRenderPlatformFailure>
        + Send
        + Sync
        + 'static,
{
    Box::new(FalliblePresentOutput(present))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum CancelScope {
    Generation(u64),
    AllGenerations,
}

/// Called synchronously on an arbitrary caller thread after the stop flag's
/// Release-store and before the `Stop` enqueue, possibly more than once.
/// Implementations must be non-blocking, idempotent and duplicate-tolerant,
/// and take no lock the pump can hold. Atomic publication is deliberately
/// lock-free: cancellation may race a commit ioctl, so every ioctl outcome is
/// re-arbitrated and a successfully committed cancelled slot is held/drained.
/// Bounded cancellation wake-up outranks suppressing every racing commit.
///
/// Live production uses the atomic backend's generation-tagged eventfd writer.
#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct PresentationCancelHandle {
    cancel: Arc<dyn Fn(CancelScope) + Send + Sync + 'static>,
}

impl PresentationCancelHandle {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn noop_for_test() -> Self {
        Self {
            cancel: Arc::new(|_| {}),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn cancel(&self, scope: CancelScope) {
        (self.cancel)(scope);
    }

    #[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
    pub(crate) fn from_callback(cancel: impl Fn(CancelScope) + Send + Sync + 'static) -> Self {
        Self {
            cancel: Arc::new(cancel),
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(cancel: impl Fn(CancelScope) + Send + Sync + 'static) -> Self {
        Self::from_callback(cancel)
    }
}

pub(crate) struct AcquiredOutputFrame {
    pub(crate) view: ManualTextureView,
    pub(crate) present: PresentOutputFrame,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct UnpresentedFrameGuard {
    slot: ScanoutSlotId,
    abandon: Option<Box<dyn FnOnce(ScanoutSlotId) + Send + Sync + 'static>>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl UnpresentedFrameGuard {
    fn new(
        slot: ScanoutSlotId,
        abandon: impl FnOnce(ScanoutSlotId) + Send + Sync + 'static,
    ) -> Self {
        Self {
            slot,
            abandon: Some(Box::new(abandon)),
        }
    }

    fn disarm(&mut self) {
        self.abandon = None;
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Drop for UnpresentedFrameGuard {
    fn drop(&mut self) {
        if let Some(abandon) = self.abandon.take() {
            abandon(self.slot);
        }
    }
}

struct OutputFrameSource {
    generation: u64,
    handle: ManualTextureViewHandle,
    extent: (u32, u32),
    acquire: Box<
        dyn FnMut() -> Result<AcquiredOutputFrame, KmsRenderPlatformFailure>
            + Send
            + Sync
            + 'static,
    >,
    ready_generation: Option<u64>,
    current_ready_generation: Option<u64>,
    pending_present: Option<PresentOutputFrame>,
}

struct AcquiredOutputPresenter {
    key: OutputKey,
    generation: u64,
    present: PresentOutputFrame,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExtractedOutputView {
    key: OutputKey,
    generation: u64,
    handle: ManualTextureViewHandle,
    ready: bool,
    written: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderFrameEvent {
    FrameSubmitted { generation: u64, key: OutputKey },
    PresentationCancelled { generation: u64, key: OutputKey },
    TerminalFailure(KmsRenderWorkerFailure),
}

#[derive(bevy::prelude::Resource)]
pub(crate) struct KmsRenderTargets {
    sources: BTreeMap<OutputKey, OutputFrameSource>,
    lifecycle: Arc<KmsRenderLifecycle>,
    pending_quiescence: Vec<PendingRenderQuiescence>,
    worker_stop: Option<KmsRenderWorkerStop>,
    frame_events: Option<Sender<KmsRenderFrameEvent>>,
    present_deadline: PresentDeadline,
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    destructive_quiescence: Option<DestructiveQuiescenceLatch>,
}

impl KmsRenderTargets {
    fn new(present_deadline: PresentDeadline) -> Self {
        Self {
            sources: BTreeMap::new(),
            lifecycle: Arc::new(KmsRenderLifecycle::new()),
            pending_quiescence: Vec::new(),
            worker_stop: None,
            frame_events: None,
            present_deadline,
            #[cfg(any(all(feature = "kms-live", not(test)), test))]
            destructive_quiescence: None,
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct DestructiveQuiescenceIdentity {
    operation: KmsRenderOperation,
    generation: u64,
    key: Option<OutputKey>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl From<&KmsRenderQuiescence> for DestructiveQuiescenceIdentity {
    fn from(quiescence: &KmsRenderQuiescence) -> Self {
        Self {
            operation: quiescence.operation,
            generation: quiescence.generation,
            key: quiescence.key.clone(),
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Default)]
struct DestructiveQuiescenceLatch(Arc<Mutex<Option<DestructiveQuiescenceIdentity>>>);

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl DestructiveQuiescenceLatch {
    fn publish(&self, identity: DestructiveQuiescenceIdentity) -> Result<(), ()> {
        let mut published = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if published.is_some() {
            return Err(());
        }
        *published = Some(identity);
        Ok(())
    }

    fn take(&self) -> Option<DestructiveQuiescenceIdentity> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Installs the KMS render-world systems.
///
/// The plugin is inert until a source is registered. It is installed in the
/// nested renderer today so the same non-pipelined render schedule compiles and
/// runs continuously before a master-only source is added.
pub(crate) struct KmsRenderTargetPlugin;

impl Plugin for KmsRenderTargetPlugin {
    fn build(&self, app: &mut App) {
        install_kms_render_target(app, OfflineRenderPlatform);
    }
}

fn install_kms_render_target<T>(app: &mut App, platform: T)
where
    T: KmsRenderPlatform<Placeholder = KmsRenderPlaceholder>,
{
    let (event_sender, event_receiver) = mpsc::channel();
    let worker = KmsRenderWorker::spawn(platform, event_sender)
        .unwrap_or_else(|error| panic!("failed to start cosmix-kms-render: {error}"));
    let (frame_event_sender, frame_event_receiver) = mpsc::channel();
    wire_kms_render_target(
        app,
        &worker,
        event_receiver,
        frame_event_sender,
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        None,
    );
    drop(frame_event_receiver);
    app.insert_resource(KmsRenderWorkerResource(Mutex::new(Some(worker))));
}

fn wire_kms_render_target(
    app: &mut App,
    worker: &KmsRenderWorker<KmsRenderPlaceholder>,
    event_receiver: Receiver<KmsRenderWorkerEvent<KmsRenderPlaceholder>>,
    frame_events: Sender<KmsRenderFrameEvent>,
    #[cfg(any(all(feature = "kms-live", not(test)), test))] destructive_quiescence: Option<
        DestructiveQuiescenceLatch,
    >,
) {
    let (render_sender, render_receiver) = mpsc::channel();
    let (reply_sender, reply_receiver) = mpsc::channel();
    let release_sender = worker.release_sender();
    let registration_sender = worker.registration_sender();
    let quiescence_sender = worker.quiescence_sender();
    let worker_stop = worker.stop_handle();
    let render_lifecycle = worker_stop.render_lifecycle();
    app.insert_resource(KmsRegistrarInbox {
        receiver: Mutex::new(event_receiver),
        replies: reply_sender,
        registrar: RenderSourceRegistrar::default(),
        render: render_sender,
        releases: release_sender.clone(),
        registrations: registration_sender,
        worker_stop: worker_stop.clone(),
        terminal: None,
    })
    .insert_resource(KmsRegistrarReplies(Mutex::new(reply_receiver)))
    .init_resource::<KmsMainWorldOutputs>()
    .add_systems(bevy::app::First, apply_registrar_events);
    configure_render_app(
        app.sub_app_mut(RenderApp),
        render_receiver,
        release_sender,
        quiescence_sender,
        KmsRenderAppControl {
            lifecycle: render_lifecycle,
            worker_stop: Some(worker_stop),
            frame_events: Some(frame_events),
            #[cfg(any(all(feature = "kms-live", not(test)), test))]
            destructive_quiescence,
        },
    );
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct LiveKmsRenderInstallation {
    worker: KmsRenderWorker<KmsRenderPlaceholder>,
    render_world_dropped: super::worker::RenderWorldDropAcknowledger,
    #[cfg_attr(test, allow(dead_code))]
    frame_events: Receiver<KmsRenderFrameEvent>,
    destructive_quiescence: DestructiveQuiescenceLatch,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn install_live_kms_render_target<T>(
    app: &mut App,
    platform: T,
) -> Result<LiveKmsRenderInstallation, std::io::Error>
where
    T: KmsRenderPlatform<Placeholder = KmsRenderPlaceholder>,
{
    let (event_sender, event_receiver) = mpsc::channel();
    let (worker, render_world_dropped) = KmsRenderWorker::spawn_guarded(platform, event_sender)?;
    let (frame_event_sender, frame_events) = mpsc::channel();
    let destructive_quiescence = DestructiveQuiescenceLatch::default();
    wire_kms_render_target(
        app,
        &worker,
        event_receiver,
        frame_event_sender,
        Some(destructive_quiescence.clone()),
    );
    Ok(LiveKmsRenderInstallation {
        worker,
        render_world_dropped,
        frame_events,
        destructive_quiescence,
    })
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn live_headless_plugins() -> PluginGroupBuilder {
    configure_live_headless_plugins(DefaultPlugins.build())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn configure_live_headless_plugins(plugins: PluginGroupBuilder) -> PluginGroupBuilder {
    plugins
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
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn assert_non_pipelined_rendering(app: &App) -> Result<(), super::kms_live::KmsLiveError> {
    if app.is_plugin_added::<PipelinedRenderingPlugin>()
        || app.get_sub_app(RenderExtractApp).is_some()
    {
        return Err(super::kms_live::KmsLiveError::Setup(
            "PipelinedRenderingPlugin remained active after runtime disable".into(),
        ));
    }
    Ok(())
}

#[cfg(all(feature = "kms-live", not(test)))]
fn build_live_render_app(
    renderer: cosmix_wgpu_dmabuf::ManualVulkanRenderer,
    scene_mode: LiveSceneMode,
    decoration: DecorationStartup,
) -> Result<App, super::kms_live::KmsLiveError> {
    let mut app = App::new();
    init_chrome_font_cx(&mut app);
    app.insert_resource(decoration)
        .add_plugins(renderer.install_into(live_headless_plugins()));
    #[cfg(feature = "frame-capture")]
    crate::frame_capture::install_from_environment(&mut app)
        .map_err(|error| super::kms_live::KmsLiveError::Setup(error.to_string()))?;
    install_live_scene(&mut app, scene_mode);
    app.insert_resource(FirstLiveRenderError::default())
        .insert_resource(RenderErrorHandler(stop_live_rendering_after_first_error));
    assert_non_pipelined_rendering(&app)?;
    app.finish();
    app.cleanup();
    tracing::info!(?scene_mode, "live headless Bevy App built");
    Ok(app)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveSceneMode {
    FirstLight,
    ClientContent,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn install_live_scene(app: &mut App, scene_mode: LiveSceneMode) {
    match scene_mode {
        LiveSceneMode::FirstLight => {
            app.add_plugins(FirstLightScenePlugin);
        }
        LiveSceneMode::ClientContent => {
            app.insert_resource(ClearColor(Color::srgb(0.010, 0.018, 0.055)))
                .add_plugins((
                    DmabufImportPlugin,
                    CompositorScenePlugin::new(1, 1, SceneCursorMode::SoftwareCursor),
                ));
            #[cfg(all(feature = "kms-live", not(test)))]
            configure_live_dmabuf_debug(app);
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn configure_live_dmabuf_debug(app: &mut App) {
    let no_cache = live_debug_switch("COSMIX_DMABUF_NO_CACHE");
    let probe = live_debug_switch("COSMIX_DMABUF_PROBE");
    if !no_cache && !probe {
        return;
    }

    app.world()
        .resource::<ImportedDmabufImages>()
        .configure_debug(no_cache, probe);
    if probe {
        app.add_plugins(DmabufProbePlugin);
        install_dmabuf_output_probe(app);
        app.sub_app_mut(RenderApp).add_systems(
            Render,
            probe_dmabuf_output
                .after(clear_unwritten_output_frames)
                .before(present_output_frames),
        );
    }
    tracing::info!(no_cache, probe, "kms-live DMA-BUF instrumentation enabled");
}

#[cfg(all(feature = "kms-live", not(test)))]
fn live_debug_switch(name: &'static str) -> bool {
    match std::env::var_os(name) {
        None => false,
        Some(value) if live_instrumentation_switch_enabled(Some(value.as_os_str())) => true,
        Some(value) => {
            tracing::warn!(
                switch = name,
                value = ?value,
                "ignoring kms-live instrumentation switch because its value is not exactly 1"
            );
            false
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn live_instrumentation_switch_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value == "1")
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn prepare_live_scene_start(
    app: &mut App,
    scene_mode: LiveSceneMode,
    scene_feed: Option<ClientSceneFeed>,
    logical_extent: (u32, u32),
    output_scale: super::kms::OutputScale120,
) -> Result<(), super::kms_live::KmsLiveError> {
    match (scene_mode, scene_feed) {
        (LiveSceneMode::FirstLight, None) => Ok(()),
        (LiveSceneMode::ClientContent, Some(scene_feed)) => {
            app.insert_resource(scene_feed);
            set_compositor_logical_output_geometry(
                app.world_mut(),
                logical_extent.0,
                logical_extent.1,
                output_scale,
            );
            Ok(())
        }
        (LiveSceneMode::FirstLight, Some(_)) => Err(super::kms_live::KmsLiveError::Setup(
            "first-light App received a client scene feed".into(),
        )),
        (LiveSceneMode::ClientContent, None) => Err(super::kms_live::KmsLiveError::Setup(
            "client-content App started without its client scene feed".into(),
        )),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct FirstLightScenePlugin;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Plugin for FirstLightScenePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(Color::srgb(0.010, 0.018, 0.055)))
            .add_systems(bevy::app::Startup, spawn_first_light_scene)
            .add_systems(bevy::app::Update, animate_first_light_scene);
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Component)]
struct FirstLightRectangle;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn spawn_first_light_scene(mut commands: Commands) {
    commands.spawn((
        FirstLightRectangle,
        Sprite::from_color(Color::srgb(0.10, 0.62, 0.78), Vec2::new(480.0, 280.0)),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn animate_first_light_scene(
    time: Res<Time>,
    mut clear: ResMut<ClearColor>,
    mut rectangle: Query<&mut Transform, With<FirstLightRectangle>>,
) {
    let elapsed = time.elapsed_secs();
    let blue = 0.052 + 0.018 * (elapsed * 0.19).sin();
    clear.0 = Color::srgb(0.008, 0.016, blue);
    for mut transform in &mut rectangle {
        transform.translation.x = (elapsed * 0.31).sin() * 280.0;
        transform.translation.y = (elapsed * 0.47 + 0.8).sin() * 140.0;
        transform.rotation = Quat::from_rotation_z(elapsed * 0.22);
    }
}

struct KmsRenderPlaceholder {
    extent: (u32, u32),
    logical_extent: (u32, u32),
    view: Option<ManualTextureView>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn selected_logical_extent(output: &super::kms::SelectedOutput) -> (u32, u32) {
    (
        u32::try_from(output.logical_rect.width)
            .expect("admitted output has a positive logical width"),
        u32::try_from(output.logical_rect.height)
            .expect("admitted output has a positive logical height"),
    )
}

pub(crate) fn logical_output_projection(logical_extent: (u32, u32)) -> Projection {
    Projection::Orthographic(OrthographicProjection {
        scaling_mode: ScalingMode::Fixed {
            width: logical_extent.0 as f32,
            height: logical_extent.1 as f32,
        },
        ..OrthographicProjection::default_2d()
    })
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) struct LiveRenderEngine {
    app: Option<App>,
    render_world_dropped: Option<super::worker::RenderWorldDropAcknowledger>,
    worker: Option<KmsRenderWorker<KmsRenderPlaceholder>>,
    frame_events: Receiver<KmsRenderFrameEvent>,
    destructive_quiescence: DestructiveQuiescenceLatch,
    update_gate: LiveRenderUpdateGate,
    terminal_updates_stopped: Arc<AtomicBool>,
    expected_destructive_quiescence: Vec<DestructiveQuiescenceIdentity>,
    output: OutputKey,
    generation: u64,
    transition_generation: u64,
    output_ready: bool,
    #[cfg_attr(test, allow(dead_code))]
    resume_leases: LiveResumeLeaseSlot,
    topology_client: Option<crate::protocol::KmsTopologyClient>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(test, allow(dead_code))]
enum LiveRenderUpdateGate {
    Open,
    AwaitingDestructiveReply(DestructiveQuiescenceIdentity),
    Paused { generation: u64 },
    AwaitingReplacement { generation: u64, key: OutputKey },
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Resource, Default)]
struct FirstLiveRenderError(Option<String>);

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn stop_live_rendering_after_first_error(
    error: &RenderError,
    main_world: &mut World,
    _render_world: &mut World,
) -> RenderErrorPolicy {
    let detail = format!("{:?} RenderError: {}", error.ty, error.description);
    let first_occurrence = {
        let mut first = main_world.resource_mut::<FirstLiveRenderError>();
        if first.0.is_none() {
            first.0 = Some(detail);
            true
        } else {
            false
        }
    };
    if first_occurrence {
        main_world.write_message(AppExit::error());
    }
    RenderErrorPolicy::StopRendering
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn update_live_app(app: &mut App) -> Result<(), super::kms_live::KmsLiveError> {
    // Production invariant: every full live update routes through this
    // function so renderer exit remains observable at the pump boundary.
    app.update();
    let Some(exit) = app.should_exit() else {
        return Ok(());
    };
    let detail = app
        .world()
        .get_resource::<FirstLiveRenderError>()
        .and_then(|first| first.0.as_deref());
    let reason = match (exit, detail) {
        (AppExit::Error(code), Some(detail)) => {
            format!("live renderer exited with code {code}: {detail}")
        }
        (AppExit::Error(code), None) => format!("live renderer exited with code {code}"),
        (AppExit::Success, Some(detail)) => format!("live renderer exited: {detail}"),
        (AppExit::Success, None) => "live renderer requested a successful exit".into(),
    };
    Err(super::kms_live::KmsLiveError::Setup(reason))
}

#[cfg(test)]
pub(crate) type LiveRenderAdapter = LiveRenderEngine;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveOutputRegistration {
    Pending,
    Ready,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Debug)]
pub(crate) enum PumpReply {
    Started(Result<(), super::kms_live::KmsLiveError>),
    Registration(Result<LiveOutputRegistration, super::kms_live::KmsLiveError>),
    Updated(Result<Vec<KmsRenderFrameEvent>, super::kms_live::KmsLiveError>),
    #[allow(dead_code)]
    TransitionBegun {
        generation: u64,
        result: Result<(), super::kms_live::KmsLiveError>,
    },
    #[allow(dead_code)]
    ResumeLeaseStaged {
        generation: u64,
        result: Result<(), super::kms_live::KmsLiveError>,
    },
    #[allow(dead_code)]
    TransitionUpdated {
        generation: u64,
        result: Result<Vec<KmsRenderReply>, super::kms_live::KmsLiveError>,
    },
    SceneDrained {
        generation: u64,
        result: Result<(), super::kms_live::KmsLiveError>,
    },
    Exited,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
trait LivePumpUpdater {
    fn update_for_pump(
        &mut self,
    ) -> Result<Vec<KmsRenderFrameEvent>, super::kms_live::KmsLiveError>;
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LivePumpUpdater for LiveRenderEngine {
    fn update_for_pump(
        &mut self,
    ) -> Result<Vec<KmsRenderFrameEvent>, super::kms_live::KmsLiveError> {
        self.update()?;
        Ok(self.drain_frame_events())
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn live_pump_update_reply(current: &mut impl LivePumpUpdater) -> (PumpReply, bool) {
    let result = current.update_for_pump();
    let failed = result.is_err();
    (PumpReply::Updated(result), failed)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveRegistrarDrain {
    output_ready: bool,
    replies_drained: usize,
    transition_replies: Vec<KmsRenderReply>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveRenderEngine {
    fn full_update_allowed(&self) -> bool {
        self.update_gate == LiveRenderUpdateGate::Open
            && !self.terminal_updates_stopped.load(Ordering::Acquire)
    }

    fn stop_terminal_updates(&self) {
        // The pump stop publisher and both full-update entry points share this
        // Release/Acquire frontier. An update which observes `false` is already
        // executing on the pump thread and must finish before shutdown can drop
        // the worlds and acknowledge worker teardown. An admitted command which
        // observes `true` is restricted to registrar polling.
        self.terminal_updates_stopped.store(true, Ordering::Release);
    }

    fn observe_destructive_quiescence(&mut self) -> Result<(), super::kms_live::KmsLiveError> {
        let Some(published) = self.destructive_quiescence.take() else {
            return Ok(());
        };
        let Some(expected) = self.expected_destructive_quiescence.first() else {
            return Err(super::kms_live::KmsLiveError::Setup(format!(
                "kms-live-unexpected-destructive-quiescence: {published:?}"
            )));
        };
        if *expected != published {
            return Err(super::kms_live::KmsLiveError::Setup(format!(
                "kms-live-destructive-quiescence-generation-mismatch: expected {expected:?}, received {published:?}"
            )));
        }
        if self.update_gate != LiveRenderUpdateGate::Open {
            return Err(super::kms_live::KmsLiveError::Setup(format!(
                "kms-live-destructive-quiescence-gate-closed: received {published:?} while {:?}",
                self.update_gate
            )));
        }
        self.expected_destructive_quiescence.remove(0);
        self.update_gate = LiveRenderUpdateGate::AwaitingDestructiveReply(published);
        Ok(())
    }

    fn replacement_installed(world: &World, generation: u64, key: &OutputKey) -> bool {
        world
            .resource::<KmsMainWorldOutputs>()
            .0
            .get(key)
            .is_some_and(|output| output.generation == generation)
    }

    fn replacement_failed(replies: &[KmsRenderReply], generation: u64, key: &OutputKey) -> bool {
        replies.iter().any(|reply| {
            matches!(reply, KmsRenderReply::OutputFailed {
                generation: failed_generation,
                key: failed_key,
                ..
            } if *failed_generation == generation && failed_key == key)
        })
    }

    fn advance_update_gate(
        &mut self,
        replies: &[KmsRenderReply],
        replacement_installed: bool,
    ) -> bool {
        match self.update_gate.clone() {
            LiveRenderUpdateGate::AwaitingDestructiveReply(identity) => match identity.operation {
                KmsRenderOperation::Suspend
                    if replies.iter().any(|reply| {
                        matches!(reply, KmsRenderReply::Suspended { generation }
                            if *generation == identity.generation)
                    }) =>
                {
                    self.update_gate = LiveRenderUpdateGate::Paused {
                        generation: identity.generation,
                    };
                    false
                }
                KmsRenderOperation::RemoveOutput
                    if replies.iter().any(|reply| {
                        matches!(reply, KmsRenderReply::OutputRemoved { generation, key }
                            if *generation == identity.generation
                                && Some(key) == identity.key.as_ref())
                    }) =>
                {
                    self.update_gate = LiveRenderUpdateGate::Open;
                    false
                }
                KmsRenderOperation::ChangeOutput if replacement_installed => {
                    self.update_gate = LiveRenderUpdateGate::Open;
                    true
                }
                KmsRenderOperation::ChangeOutput
                    if identity.key.as_ref().is_some_and(|key| {
                        Self::replacement_failed(replies, identity.generation, key)
                    }) =>
                {
                    // Quiescence already removed every derived reference to the old target, and
                    // OutputFailed proves that no replacement SourceReady was installed. A full
                    // update is therefore safe for the coordinator's follow-up topology action.
                    self.update_gate = LiveRenderUpdateGate::Open;
                    false
                }
                _ => false,
            },
            LiveRenderUpdateGate::AwaitingReplacement { .. } if replacement_installed => {
                self.update_gate = LiveRenderUpdateGate::Open;
                true
            }
            LiveRenderUpdateGate::AwaitingReplacement { generation, key }
                if Self::replacement_failed(replies, generation, &key) =>
            {
                // The pause Clear already removed the old surface from both worlds, while a
                // failed AddOutput installed no SourceReady-derived state. Reopening is safe and
                // lets the existing rollback Suspend execute its render-world Clear/quiescence.
                self.update_gate = LiveRenderUpdateGate::Open;
                false
            }
            LiveRenderUpdateGate::Open
            | LiveRenderUpdateGate::Paused { .. }
            | LiveRenderUpdateGate::AwaitingReplacement { .. } => false,
        }
    }

    fn run_registrar_only(app: &mut App) -> Result<(), super::kms_live::KmsLiveError> {
        app.world_mut()
            .run_system_once(apply_registrar_events)
            .map_err(|error| {
                super::kms_live::KmsLiveError::Setup(format!(
                    "live render transition registrar update failed: {error}"
                ))
            })
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveRenderStartControl {
    target_pairing: super::kms_live::LiveTargetPairingLedger,
    terminal_updates_stopped: Arc<AtomicBool>,
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveRenderEngine {
    fn drain_scene(&mut self) -> Result<(), super::kms_live::KmsLiveError> {
        let app = self
            .app
            .as_mut()
            .expect("live Bevy app exists while draining client scene state");
        drain_live_client_scene(app);
        Ok(())
    }

    // Each parameter is a distinct piece of live-session state the render
    // thread needs at startup; grouping them into a struct would just move
    // the same eight fields one level out without reducing what the caller
    // has to assemble.
    #[allow(clippy::too_many_arguments)]
    fn start(
        mut app: App,
        backend: LivePreparedBackend,
        drm_device: u64,
        lease: super::kms_live::MasterDrmLease,
        output: super::kms::SelectedOutput,
        initial_commands: Vec<super::kms::KmsRenderCommand>,
        topology_client: crate::protocol::KmsTopologyClient,
        control: LiveRenderStartControl,
    ) -> Result<Self, super::kms_live::KmsLiveError> {
        let generation = initial_commands
            .iter()
            .find_map(|command| match command {
                super::kms::KmsRenderCommand::AddOutput {
                    generation,
                    output: added,
                } if added.key == output.key => Some(*generation),
                _ => None,
            })
            .ok_or_else(|| {
                super::kms_live::KmsLiveError::Setup(
                    "protocol topology emitted no initial live output command".into(),
                )
            })?;
        let resume_leases = Arc::new(Mutex::new(GenerationLeaseSlot::default()));
        let LivePreparedBackend {
            bridge,
            cancellation,
        } = backend;
        let ownership = LiveRenderPlatformOwnership(LiveAtomicOwnership {
            targets: BTreeMap::new(),
            retained_buffers: BTreeMap::new(),
            fail_closed_ownership_islands: FailClosedAtomicOwnershipIslands::default(),
            gpu_retirement: bridge.retirement_adapter(),
            bridge,
            cancellation,
            lease: Some(lease),
            resume_leases: Arc::clone(&resume_leases),
            target_generation: generation,
            target_pairing: control.target_pairing,
            drm_device,
            event_router: None,
            resume_presentation: None,
        });
        let platform = LiveRenderPlatform {
            ownership: Some(ownership),
        };
        let LiveKmsRenderInstallation {
            worker,
            render_world_dropped,
            frame_events,
            destructive_quiescence,
        } = install_live_kms_render_target(&mut app, platform)
            .map_err(|error| super::kms_live::KmsLiveError::Setup(error.to_string()))?;
        let adapter = Self {
            app: Some(app),
            render_world_dropped: Some(render_world_dropped),
            worker: Some(worker),
            frame_events,
            destructive_quiescence,
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped: control.terminal_updates_stopped,
            expected_destructive_quiescence: Vec::new(),
            output: output.key.clone(),
            generation,
            transition_generation: generation,
            output_ready: false,
            resume_leases,
            topology_client: Some(topology_client),
        };
        for command in initial_commands {
            if let Err(error) = adapter
                .worker
                .as_ref()
                .expect("live render worker exists after installation")
                .send(command)
            {
                return Err(fail_live_adapter_start(
                    adapter,
                    super::kms_live::KmsLiveError::Setup(error.to_string()),
                ));
            }
        }
        Ok(adapter)
    }

    pub(crate) fn shutdown(mut self) -> Result<(), super::kms_live::KmsLiveError> {
        self.shutdown_inner()
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveRenderEngine {
    pub(crate) fn stage_resume_lease(
        &self,
        generation: u64,
        resume: StagedResumeLease,
    ) -> Result<(), super::kms_live::KmsLiveError> {
        let mut staged = self.resume_leases.lock().map_err(|_| {
            super::kms_live::KmsLiveError::Setup("live resume-lease slot was poisoned".into())
        })?;
        if self.transition_generation.checked_add(1) != Some(generation) {
            let code = if generation <= self.transition_generation {
                "kms-live-stale-generation"
            } else {
                "kms-live-generation-gap"
            };
            return Err(super::kms_live::KmsLiveError::Setup(format!(
                "{code}: resume lease generation {generation} does not immediately follow {}",
                self.transition_generation
            )));
        }
        staged.stage(generation, resume).map_err(|code| {
            super::kms_live::KmsLiveError::Setup(format!(
                "{code}: the live resume-lease slot was already occupied while staging generation {generation}"
            ))
        })
    }

    pub(crate) fn poll_output_registration(
        &mut self,
    ) -> Result<LiveOutputRegistration, super::kms_live::KmsLiveError> {
        if self.output_ready {
            return Ok(LiveOutputRegistration::Ready);
        }
        let output = self.output.clone();
        let app = self
            .app
            .as_mut()
            .expect("live Bevy app exists during output registration");
        app.world_mut()
            .run_system_once(apply_registrar_events)
            .map_err(|error| {
                super::kms_live::KmsLiveError::Setup(format!(
                    "live render registration update failed: {error}"
                ))
            })?;
        let drained = drain_live_registrar_replies(
            app.world(),
            self.generation,
            &output,
            self.output_ready,
            self.topology_client.as_ref(),
            true,
        )?;
        let _ = drained.replies_drained;
        self.output_ready = drained.output_ready;
        if app
            .world()
            .resource::<KmsRegistrarInbox>()
            .registrar
            .is_terminal()
        {
            let phase = if self.output_ready {
                "after the output became ready"
            } else {
                "before the output became ready"
            };
            return Err(super::kms_live::KmsLiveError::Setup(format!(
                "live render registrar stopped {phase}"
            )));
        }
        Ok(if self.output_ready {
            LiveOutputRegistration::Ready
        } else {
            LiveOutputRegistration::Pending
        })
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn update(&mut self) -> Result<(), super::kms_live::KmsLiveError> {
        let output = self.output.clone();
        let full_update = self.full_update_allowed();
        {
            let app = self
                .app
                .as_mut()
                .expect("live Bevy app exists while pumping");
            if full_update {
                #[cfg(all(feature = "kms-live", not(test)))]
                update_live_app(app)?;
                #[cfg(not(all(feature = "kms-live", not(test))))]
                update_live_app(app)?;
            } else {
                Self::run_registrar_only(app)?;
            }
        }
        if full_update {
            self.observe_destructive_quiescence()?;
        }
        let app = self
            .app
            .as_mut()
            .expect("live Bevy app exists while pumping");
        let drained = drain_live_registrar_replies(
            app.world(),
            self.generation,
            &output,
            self.output_ready,
            self.topology_client.as_ref(),
            true,
        )?;
        let _ = drained.replies_drained;
        self.output_ready = drained.output_ready;
        if app
            .world()
            .resource::<KmsRegistrarInbox>()
            .registrar
            .is_terminal()
        {
            return Err(super::kms_live::KmsLiveError::Setup(
                "live render registrar stopped while frames were being pumped".into(),
            ));
        }
        Ok(())
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn drain_frame_events(&mut self) -> Vec<KmsRenderFrameEvent> {
        let mut events = Vec::new();
        loop {
            match self.frame_events.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return events,
            }
        }
    }

    #[allow(dead_code)]
    fn begin_transition(
        &mut self,
        commands: Vec<super::kms::KmsRenderCommand>,
    ) -> Result<(), super::kms_live::KmsLiveError> {
        validate_transition_generations(self.transition_generation, &commands)?;
        let destructive_commands = commands
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    super::kms::KmsRenderCommand::Suspend { .. }
                        | super::kms::KmsRenderCommand::ChangeOutput { .. }
                        | super::kms::KmsRenderCommand::RemoveOutput { .. }
                )
            })
            .count();
        if destructive_commands > 1 {
            return Err(super::kms_live::KmsLiveError::Setup(format!(
                "kms-live-multiple-destructive-commands: transition contains {destructive_commands} destructive boundaries"
            )));
        }
        validate_staged_resume_lease(&self.resume_leases, &commands)?;
        if matches!(
            self.update_gate,
            LiveRenderUpdateGate::AwaitingDestructiveReply(_)
        ) {
            return Err(super::kms_live::KmsLiveError::Setup(
                "kms-live-destructive-transition-overlap: the preceding destructive reply is still pending"
                    .into(),
            ));
        }
        let replacement = commands.iter().find_map(|command| match command {
            super::kms::KmsRenderCommand::AddOutput { generation, output }
            | super::kms::KmsRenderCommand::ChangeOutput { generation, output } => {
                Some((*generation, output.key.clone()))
            }
            _ => None,
        });
        let paused_replacement = if matches!(self.update_gate, LiveRenderUpdateGate::Paused { .. })
        {
            if !commands
                .iter()
                .any(|command| matches!(command, super::kms::KmsRenderCommand::Resume { .. }))
            {
                return Err(super::kms_live::KmsLiveError::Setup(
                    "kms-live-paused-transition-missing-resume: paused rendering requires Resume before replacement installation"
                        .into(),
                ));
            }
            let Some(replacement) = replacement else {
                return Err(super::kms_live::KmsLiveError::Setup(
                    "kms-live-resume-replacement-missing: paused rendering can reopen only after a replacement output is installed"
                        .into(),
                ));
            };
            Some(replacement)
        } else {
            None
        };
        if let Some((generation, key)) = paused_replacement {
            self.update_gate = LiveRenderUpdateGate::AwaitingReplacement { generation, key };
        }
        if commands
            .iter()
            .any(|command| matches!(command, super::kms::KmsRenderCommand::Suspend { .. }))
        {
            pause_virtual_time(
                self.app
                    .as_mut()
                    .expect("live Bevy app exists when suspend begins")
                    .world_mut(),
            );
        }
        for command in commands {
            let command_generation = kms_render_command_generation(&command);
            let destructive_identity = match &command {
                super::kms::KmsRenderCommand::Suspend { generation } => {
                    Some(DestructiveQuiescenceIdentity {
                        operation: KmsRenderOperation::Suspend,
                        generation: *generation,
                        key: None,
                    })
                }
                super::kms::KmsRenderCommand::ChangeOutput { generation, output } => {
                    Some(DestructiveQuiescenceIdentity {
                        operation: KmsRenderOperation::ChangeOutput,
                        generation: *generation,
                        key: Some(output.key.clone()),
                    })
                }
                super::kms::KmsRenderCommand::RemoveOutput { generation, key } => {
                    Some(DestructiveQuiescenceIdentity {
                        operation: KmsRenderOperation::RemoveOutput,
                        generation: *generation,
                        key: Some(key.clone()),
                    })
                }
                super::kms::KmsRenderCommand::Resume { .. }
                | super::kms::KmsRenderCommand::AddOutput { .. } => None,
            };
            match &command {
                super::kms::KmsRenderCommand::Suspend { .. } => self.output_ready = false,
                super::kms::KmsRenderCommand::AddOutput { generation, output }
                | super::kms::KmsRenderCommand::ChangeOutput { generation, output }
                    if output.key == self.output =>
                {
                    self.generation = *generation;
                    self.output_ready = false;
                }
                super::kms::KmsRenderCommand::Resume { .. }
                | super::kms::KmsRenderCommand::RemoveOutput { .. }
                | super::kms::KmsRenderCommand::AddOutput { .. }
                | super::kms::KmsRenderCommand::ChangeOutput { .. } => {}
            }
            self.worker
                .as_ref()
                .expect("live render worker exists during transition")
                .send(command)
                .map_err(|error| super::kms_live::KmsLiveError::Setup(error.to_string()))?;
            if let Some(identity) = destructive_identity {
                self.expected_destructive_quiescence.push(identity);
            }
            self.transition_generation = command_generation;
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn transition_update(&mut self) -> Result<Vec<KmsRenderReply>, super::kms_live::KmsLiveError> {
        let output = self.output.clone();
        let full_update = self.full_update_allowed();
        {
            let app = self
                .app
                .as_mut()
                .expect("live Bevy app exists during lifecycle transition");
            if full_update {
                #[cfg(all(feature = "kms-live", not(test)))]
                update_live_app(app)?;
                #[cfg(not(all(feature = "kms-live", not(test))))]
                update_live_app(app)?;
            } else {
                Self::run_registrar_only(app)?;
            }
        }
        if full_update {
            self.observe_destructive_quiescence()?;
        }
        let mut drained = drain_live_registrar_replies(
            self.app
                .as_ref()
                .expect("live Bevy app exists during lifecycle transition")
                .world(),
            self.generation,
            &output,
            self.output_ready,
            self.topology_client.as_ref(),
            false,
        )?;
        self.output_ready = drained.output_ready;
        let replacement_installed = match &self.update_gate {
            LiveRenderUpdateGate::AwaitingDestructiveReply(identity)
                if identity.operation == KmsRenderOperation::ChangeOutput =>
            {
                identity.key.as_ref().is_some_and(|key| {
                    Self::replacement_installed(
                        self.app
                            .as_ref()
                            .expect("live Bevy app exists during lifecycle transition")
                            .world(),
                        identity.generation,
                        key,
                    )
                })
            }
            LiveRenderUpdateGate::AwaitingReplacement { generation, key } => {
                Self::replacement_installed(
                    self.app
                        .as_ref()
                        .expect("live Bevy app exists during lifecycle transition")
                        .world(),
                    *generation,
                    key,
                )
            }
            _ => false,
        };
        let reopen_after_install =
            self.advance_update_gate(&drained.transition_replies, replacement_installed);
        // SourceReady may reopen the transition gate during a registrar-only
        // terminal turn. Recheck the independent terminal frontier before the
        // replacement's first full update; bookkeeping may complete after stop,
        // but no newly installed target may start rendering.
        if reopen_after_install && self.full_update_allowed() {
            let app = self
                .app
                .as_mut()
                .expect("live Bevy app exists after replacement installation");
            #[cfg(all(feature = "kms-live", not(test)))]
            update_live_app(app)?;
            #[cfg(not(all(feature = "kms-live", not(test))))]
            update_live_app(app)?;
            self.observe_destructive_quiescence()?;
            let after_install = drain_live_registrar_replies(
                self.app
                    .as_ref()
                    .expect("live Bevy app exists after replacement installation")
                    .world(),
                self.generation,
                &output,
                self.output_ready,
                self.topology_client.as_ref(),
                false,
            )?;
            self.output_ready = after_install.output_ready;
            drained
                .transition_replies
                .extend(after_install.transition_replies);
        }
        let app = self
            .app
            .as_mut()
            .expect("live Bevy app exists during lifecycle transition");
        unpause_virtual_time_after_output_ready(
            app.world_mut(),
            &drained.transition_replies,
            self.generation,
            &output,
        );
        Ok(drained.transition_replies)
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) fn drain_live_client_scene(app: &mut App) {
    if app.world().contains_resource::<ClientSceneFeed>() {
        drain_protocol_events(app.world_mut());
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn pause_virtual_time(world: &mut World) {
    world
        .resource_mut::<bevy::prelude::Time<bevy::time::Virtual>>()
        .pause();
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn unpause_virtual_time_after_output_ready(
    world: &mut World,
    replies: &[KmsRenderReply],
    generation: u64,
    output: &OutputKey,
) -> bool {
    let ready = replies.iter().any(|reply| {
        matches!(
            reply,
            KmsRenderReply::OutputReady {
                generation: ready_generation,
                key,
            } if *ready_generation == generation && key == output
        )
    });
    if ready {
        world
            .resource_mut::<bevy::prelude::Time<bevy::time::Virtual>>()
            .unpause();
    }
    ready
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn kms_render_command_generation(command: &super::kms::KmsRenderCommand) -> u64 {
    match command {
        super::kms::KmsRenderCommand::Suspend { generation }
        | super::kms::KmsRenderCommand::Resume { generation }
        | super::kms::KmsRenderCommand::AddOutput { generation, .. }
        | super::kms::KmsRenderCommand::ChangeOutput { generation, .. }
        | super::kms::KmsRenderCommand::RemoveOutput { generation, .. } => *generation,
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn validate_transition_generations(
    current: u64,
    commands: &[super::kms::KmsRenderCommand],
) -> Result<u64, super::kms_live::KmsLiveError> {
    if commands.is_empty() {
        return Err(super::kms_live::KmsLiveError::Setup(
            "kms-live-empty-transition: a render transition must contain a command".into(),
        ));
    }
    let mut latest = current;
    for command in commands {
        let generation = kms_render_command_generation(command);
        if latest.checked_add(1) != Some(generation) {
            let code = if generation <= latest {
                "kms-live-stale-generation"
            } else {
                "kms-live-generation-gap"
            };
            return Err(super::kms_live::KmsLiveError::Setup(format!(
                "{code}: transition generation {generation} does not immediately follow {latest}"
            )));
        }
        latest = generation;
    }
    Ok(latest)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn validate_staged_resume_lease<T>(
    slot: &Arc<Mutex<GenerationLeaseSlot<T>>>,
    commands: &[super::kms::KmsRenderCommand],
) -> Result<(), super::kms_live::KmsLiveError> {
    let mut resume_generations = commands.iter().filter_map(|command| match command {
        super::kms::KmsRenderCommand::Resume { generation } => Some(*generation),
        _ => None,
    });
    let resume_generation = resume_generations.next();
    if let Some(second) = resume_generations.next() {
        return Err(super::kms_live::KmsLiveError::Setup(format!(
            "kms-live-resume-lease-batch-ambiguous: a transition cannot contain a second Resume generation {second}"
        )));
    }
    let staged_generation = slot
        .lock()
        .map_err(|_| {
            super::kms_live::KmsLiveError::Setup("live resume-lease slot was poisoned".into())
        })?
        .generation();
    match (resume_generation, staged_generation) {
        (Some(expected), Some(staged)) if expected == staged => Ok(()),
        (Some(expected), Some(staged)) => Err(super::kms_live::KmsLiveError::Setup(format!(
            "kms-live-resume-lease-batch-generation-mismatch: staged generation {staged} does not match Resume generation {expected}"
        ))),
        (Some(expected), None) => Err(super::kms_live::KmsLiveError::Setup(format!(
            "kms-live-resume-lease-batch-missing: Resume generation {expected} has no staged authority"
        ))),
        (None, Some(staged)) => Err(super::kms_live::KmsLiveError::Setup(format!(
            "kms-live-resume-lease-batch-unexpected: staged generation {staged} has no Resume command"
        ))),
        (None, None) => Ok(()),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn drain_live_registrar_replies(
    world: &World,
    expected_generation: u64,
    output: &OutputKey,
    mut output_ready: bool,
    topology_client: Option<&crate::protocol::KmsTopologyClient>,
    fail_expected_output: bool,
) -> Result<LiveRegistrarDrain, super::kms_live::KmsLiveError> {
    let replies = drain_registrar_replies(world)
        .map_err(|error| super::kms_live::KmsLiveError::Setup(format!("{error:?}")))?;
    let replies_drained = replies.len();
    let mut transition_replies = Vec::new();
    for reply in replies {
        if let Some(client) = topology_client {
            client
                .submit_render_reply(reply.clone())
                .map_err(super::kms_live::KmsLiveError::Setup)?;
        }
        match &reply {
            KmsRenderReply::OutputReady { generation, key }
                if *generation == expected_generation && *key == *output =>
            {
                output_ready = true;
            }
            KmsRenderReply::OutputFailed {
                generation,
                key,
                reason,
            } if fail_expected_output && *generation == expected_generation && *key == *output => {
                return Err(super::kms_live::KmsLiveError::Setup(format!(
                    "live output failed: {reason}"
                )));
            }
            KmsRenderReply::WorkerFailed { code, reason, .. } => {
                return Err(super::kms_live::KmsLiveError::Setup(format!(
                    "live render worker failed: {code}: {reason}"
                )));
            }
            KmsRenderReply::FrameSubmitted { .. }
            | KmsRenderReply::Suspended { .. }
            | KmsRenderReply::OutputReady { .. }
            | KmsRenderReply::OutputFailed { .. }
            | KmsRenderReply::OutputRemoved { .. } => {}
        }
        if matches!(
            reply,
            KmsRenderReply::Suspended { .. }
                | KmsRenderReply::OutputReady { .. }
                | KmsRenderReply::OutputFailed { .. }
                | KmsRenderReply::OutputRemoved { .. }
        ) {
            transition_replies.push(reply);
        }
    }
    Ok(LiveRegistrarDrain {
        output_ready,
        replies_drained,
        transition_replies,
    })
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn nominal_refresh_interval(refresh_millihz: u32) -> Duration {
    let nanos = 1_000_000_000_000_u64 / u64::from(refresh_millihz.max(1));
    Duration::from_nanos(nanos).clamp(Duration::from_millis(4), Duration::from_millis(50))
}

/// Coupled to wgpu-core 29.0.4's private `present::FRAME_TIMEOUT_MS` value.
/// wgpu-hal 29.0.4 `vulkan/swapchain/native.rs::acquire` applies that timeout
/// afresh to the previous-submission fence, `vkAcquireNextImageKHR`, and the
/// post-acquire fence.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const WGPU_SURFACE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(any(all(feature = "kms-live", not(test)), test))]
const WGPU_SURFACE_ACQUIRE_BOUNDED_WAITS: u32 = 3;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
const LIVE_PUMP_QUIESCE_MARGIN: Duration = Duration::from_millis(250);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const ATOMIC_PRESENT_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) const ATOMIC_MODESET_TIMEOUT: Duration = Duration::from_millis(1_500);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const SEAMLESS_RESUME_MINIMUM_BUDGET: Duration = Duration::from_millis(750);

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn atomic_present_deadline(after_gpu_completion: Instant, allow_modeset: bool) -> PresentDeadline {
    let timeout = if allow_modeset {
        // HDMI/DP link retraining can take several hundred milliseconds. Keep
        // this bounded but distinct from a steady-state vblank/pageflip wait.
        ATOMIC_MODESET_TIMEOUT
    } else {
        ATOMIC_PRESENT_TIMEOUT
    };
    PresentDeadline::bounded(after_gpu_completion + timeout)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn seamless_resume_has_minimum_budget(now: Instant, overall_deadline: Instant) -> bool {
    overall_deadline
        .checked_duration_since(now)
        .is_some_and(|remaining| remaining >= SEAMLESS_RESUME_MINIMUM_BUDGET)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn atomic_admission_deadline(now: Instant, staged_deadline: Option<Instant>) -> Option<Instant> {
    let local_deadline = now.checked_add(ATOMIC_PRESENT_TIMEOUT)?;
    match staged_deadline {
        Some(deadline) if deadline <= now => None,
        Some(deadline) => Some(local_deadline.min(deadline)),
        None => Some(local_deadline),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn optional_atomic_resume_stage_deadline(
    now: Instant,
    overall_deadline: Instant,
    reserved_following_stages: u32,
) -> Option<Instant> {
    let reserve = ATOMIC_PRESENT_TIMEOUT.saturating_mul(reserved_following_stages);
    let latest_stage_deadline = overall_deadline.checked_sub(reserve)?;
    if latest_stage_deadline <= now {
        return None;
    }
    Some(
        now.checked_add(ATOMIC_PRESENT_TIMEOUT)
            .unwrap_or(latest_stage_deadline)
            .min(latest_stage_deadline),
    )
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn live_pump_quiesce_timeout(nominal_refresh_interval: Duration) -> Duration {
    WGPU_SURFACE_ACQUIRE_TIMEOUT
        .saturating_mul(WGPU_SURFACE_ACQUIRE_BOUNDED_WAITS)
        .saturating_add(nominal_refresh_interval.saturating_mul(2))
        .saturating_add(LIVE_PUMP_QUIESCE_MARGIN)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveRenderEngine {
    fn shutdown_inner(&mut self) -> Result<(), super::kms_live::KmsLiveError> {
        // Direct shutdown (including the render-error route) may not have come
        // through LiveRenderPump::begin_stop. Publish the same terminal frontier
        // before any action which can let the worker destroy a render target.
        self.stop_terminal_updates();
        if let Some(imports) = self
            .app
            .as_ref()
            .and_then(|app| app.world().get_resource::<ImportedDmabufImages>())
        {
            // stop_protocol runs after adapter shutdown, so the protocol thread
            // is still able to publish wl_buffer.release here. App destruction
            // has no local -> FOREIGN barrier; suppress every drop callback
            // before either Bevy world starts dropping and strand the final
            // uses fail-closed until the session disconnects its clients.
            imports.begin_terminal_teardown();
        }
        if let Some(worker) = self.worker.as_ref() {
            let stop = worker.stop_handle();
            stop.begin_shutdown();
            stop.wake();
        }
        drop(self.app.take());
        if let Some(acknowledgement) = self.render_world_dropped.take() {
            acknowledgement.acknowledge();
        }
        let outcome = self
            .worker
            .take()
            .expect("live render worker exists until shutdown")
            .finish(Duration::from_secs(30));
        live_worker_shutdown_result(outcome)
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn live_worker_shutdown_result(
    outcome: KmsRenderJoinOutcome,
) -> Result<(), super::kms_live::KmsLiveError> {
    match outcome {
        KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled) => Ok(()),
        outcome => Err(super::kms_live::KmsLiveError::Setup(format!(
            "live render shutdown failed: {outcome:?}"
        ))),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Drop for LiveRenderEngine {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.shutdown_inner();
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn fail_live_adapter_start(
    mut adapter: LiveRenderEngine,
    error: super::kms_live::KmsLiveError,
) -> super::kms_live::KmsLiveError {
    let worker = adapter.shutdown_inner();
    super::kms_live::combine_live_results(Err(error), worker)
        .expect_err("adapter-start cleanup retains the original startup failure")
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(test, allow(dead_code))]
enum PumpCommand {
    Start {
        lease: super::kms_live::MasterDrmLease,
        output: Box<super::kms::SelectedOutput>,
        initial_commands: Vec<super::kms::KmsRenderCommand>,
        topology_client: crate::protocol::KmsTopologyClient,
        scene_feed: Option<Box<ClientSceneFeed>>,
    },
    PollRegistration,
    Update,
    #[allow(dead_code)]
    BeginTransition(Vec<super::kms::KmsRenderCommand>),
    #[allow(dead_code)]
    StageResumeLease {
        generation: u64,
        resume: StagedResumeLease,
    },
    #[allow(dead_code)]
    TransitionUpdate {
        generation: u64,
    },
    DrainScene {
        generation: u64,
    },
    Stop,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveRenderPumpState {
    Running,
    Joined,
    Detached,
}

#[cfg(test)]
struct TestPumpTransitionProbe {
    reached: SyncSender<LiveRenderPumpState>,
    resume: Receiver<()>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct BoundedPumpJoin {
    completion: Receiver<Result<(), super::kms_live::KmsLiveError>>,
    thread: Option<JoinHandle<()>>,
    state: LiveRenderPumpState,
    #[cfg(test)]
    transition_probe: Option<TestPumpTransitionProbe>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl BoundedPumpJoin {
    fn finish(&mut self, timeout: Duration) -> Result<(), super::kms_live::KmsLiveError> {
        let started = std::time::Instant::now();
        let result = match self.completion.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(self.detach_timeout(timeout)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(super::kms_live::KmsLiveError::Setup(
                "live render pump completion channel closed".into(),
            )),
        };
        let Some(thread) = self.thread.take() else {
            return result;
        };
        while !thread.is_finished() {
            if started.elapsed() >= timeout {
                drop(thread);
                self.set_state(LiveRenderPumpState::Detached);
                return Err(super::kms_live::KmsLiveError::PumpDetached(
                    "live render pump reported completion without exiting within its deadline"
                        .into(),
                ));
            }
            std::thread::park_timeout(Duration::from_millis(1));
        }
        let joined = thread.join();
        self.set_state(LiveRenderPumpState::Joined);
        joined.map_err(|_| {
            super::kms_live::KmsLiveError::Setup("live render pump thread panicked".into())
        })?;
        result
    }

    fn detach_timeout(&mut self, timeout: Duration) -> super::kms_live::KmsLiveError {
        drop(self.thread.take());
        self.set_state(LiveRenderPumpState::Detached);
        super::kms_live::KmsLiveError::PumpDetached(format!(
            "live render pump did not quiesce within {}ms",
            timeout.as_millis()
        ))
    }

    fn set_state(&mut self, state: LiveRenderPumpState) {
        self.state = state;
        #[cfg(test)]
        if let Some(probe) = self.transition_probe.as_ref() {
            probe
                .reached
                .send(state)
                .expect("live pump transition observer remains connected");
            probe
                .resume
                .recv()
                .expect("live pump transition observer resumes shutdown");
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Drop for BoundedPumpJoin {
    fn drop(&mut self) {
        drop(self.thread.take());
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) struct LiveRenderPump {
    commands: SyncSender<PumpCommand>,
    stop: Arc<AtomicBool>,
    presentation_cancel: PresentationCancelHandle,
    join: BoundedPumpJoin,
    nominal_refresh_interval: Duration,
    #[cfg(test)]
    quiesce_timeout_override: Option<Duration>,
}

#[cfg(test)]
pub(crate) struct TestLiveRenderPumpBarrier {
    commands: Receiver<PumpCommand>,
    completion_release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    exit_release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    completion_sent: Receiver<()>,
    thread_exited: Receiver<()>,
    transition: Receiver<LiveRenderPumpState>,
    resume_transition: SyncSender<()>,
}

#[cfg(test)]
impl TestLiveRenderPumpBarrier {
    pub(crate) fn wait_for_stop(&self) {
        assert!(matches!(
            self.commands.recv_timeout(Duration::from_secs(1)),
            Ok(PumpCommand::Stop)
        ));
    }

    pub(crate) fn release_completion_and_wait(&self) {
        Self::release(&self.completion_release);
        self.completion_sent
            .recv_timeout(Duration::from_secs(1))
            .expect("live pump reports completion while its thread remains held");
    }

    pub(crate) fn release_thread_exit(&self) {
        Self::release(&self.exit_release);
    }

    pub(crate) fn assert_thread_still_running(&self) {
        assert!(matches!(
            self.thread_exited.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    pub(crate) fn wait_for_joined_transition(&self) {
        self.wait_for_transition(LiveRenderPumpState::Joined);
    }

    pub(crate) fn wait_for_detached_transition(&self) {
        self.wait_for_transition(LiveRenderPumpState::Detached);
    }

    pub(crate) fn resume_transition(&self) {
        self.resume_transition
            .send(())
            .expect("resume the observed live pump transition");
    }

    pub(crate) fn release_all_and_wait(self) {
        Self::release(&self.completion_release);
        Self::release(&self.exit_release);
        self.thread_exited
            .recv_timeout(Duration::from_secs(1))
            .expect("live pump test thread exits after release");
    }

    fn wait_for_transition(&self, expected: LiveRenderPumpState) {
        assert_eq!(
            self.transition
                .recv_timeout(Duration::from_secs(1))
                .expect("live pump reaches its terminal transition"),
            expected
        );
    }

    fn release(barrier: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
        let (released, wake) = &**barrier;
        *released.lock().expect("live pump test barrier lock") = true;
        wake.notify_all();
    }
}

#[cfg(test)]
fn wait_for_test_barrier(barrier: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
    let (released, wake) = &**barrier;
    let mut released = released.lock().expect("live pump test barrier lock");
    while !*released {
        released = wake.wait(released).expect("live pump test barrier wait");
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) struct PreparedLiveRenderPump {
    pub(crate) pump: LiveRenderPump,
    pub(crate) output_selector: PreparedLiveOutputSelector,
    pub(crate) protocol_wiring: crate::protocol::WaylandGpuWiring,
}

#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) struct PreparedLiveOutputSelector(
    pub(crate) cosmix_wgpu_dmabuf::ScanoutImportCapabilities,
);

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiveRenderPreparationStatus {
    Pending,
    Ready,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparationSendStatus {
    Delivered,
    Cancelled,
}

#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) struct LiveRenderPumpPreparation {
    commands: SyncSender<PumpCommand>,
    stop: Arc<AtomicBool>,
    join: BoundedPumpJoin,
    preparation: Receiver<Result<PumpPreparation, super::kms_live::KmsLiveError>>,
    prepared: Option<PumpPreparation>,
    presentation_cancel: PresentationCancelHandle,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct PumpPreparation {
    output_selector: PreparedLiveOutputSelector,
    protocol_wiring: crate::protocol::WaylandGpuWiring,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LivePreparedBackend {
    bridge: cosmix_wgpu_dmabuf::ScanoutRenderBridge,
    cancellation: Arc<AtomicCancellation>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveRenderScene {
    mode: LiveSceneMode,
    decoration: DecorationStartup,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveRenderPump {
    #[cfg(all(feature = "kms-live", not(test)))]
    pub(crate) fn begin_prepare(
        drm_device: u64,
        _presentation_backend: super::kms::PresentationBackend,
        coordinator: Sender<super::kms_live::LiveCoordinatorEvent>,
        target_pairing: super::kms_live::LiveTargetPairingLedger,
        scene_mode: LiveSceneMode,
        decoration: DecorationStartup,
    ) -> Result<LiveRenderPumpPreparation, super::kms_live::KmsLiveError> {
        let (commands, command_receiver) = mpsc::sync_channel(1);
        let (preparation_sender, preparation_receiver) = mpsc::sync_channel(0);
        let (completion_sender, completion) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let atomic_cancellation = AtomicCancellation::new().map_err(|error| {
            super::kms_live::KmsLiveError::Setup(format!(
                "kms-live-atomic-cancellation-eventfd-failed: {error}"
            ))
        })?;
        let presentation_cancel = atomic_cancellation.handle();
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("cosmix-kms-pump".into())
            .spawn(move || {
                let result = run_live_render_pump(
                    drm_device,
                    command_receiver,
                    coordinator,
                    thread_stop,
                    preparation_sender,
                    target_pairing,
                    LiveRenderScene {
                        mode: scene_mode,
                        decoration,
                    },
                    atomic_cancellation,
                );
                let _ = completion_sender.send(result);
            })
            .map_err(|error| {
                super::kms_live::KmsLiveError::Setup(format!(
                    "live render pump thread failed: {error}"
                ))
            })?;
        Ok(LiveRenderPumpPreparation {
            commands,
            stop,
            join: BoundedPumpJoin {
                completion,
                thread: Some(thread),
                state: LiveRenderPumpState::Running,
            },
            preparation: preparation_receiver,
            prepared: None,
            presentation_cancel,
        })
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    pub(crate) fn start(
        &mut self,
        lease: super::kms_live::MasterDrmLease,
        output: super::kms::SelectedOutput,
        initial_commands: Vec<super::kms::KmsRenderCommand>,
        topology_client: crate::protocol::KmsTopologyClient,
        scene_feed: Option<ClientSceneFeed>,
    ) -> Result<(), super::kms_live::KmsLiveError> {
        self.nominal_refresh_interval =
            nominal_refresh_interval(output.display.mode.refresh_millihz);
        self.send_command(PumpCommand::Start {
            lease,
            output: Box::new(output),
            initial_commands,
            topology_client,
            scene_feed: scene_feed.map(Box::new),
        })
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    pub(crate) fn poll_registration(&self) -> Result<(), super::kms_live::KmsLiveError> {
        self.send_command(PumpCommand::PollRegistration)
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    pub(crate) fn update(&self) -> Result<(), super::kms_live::KmsLiveError> {
        self.send_command(PumpCommand::Update)
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    #[allow(dead_code)]
    pub(crate) fn begin_transition(
        &self,
        commands: Vec<super::kms::KmsRenderCommand>,
    ) -> Result<(), super::kms_live::KmsLiveError> {
        self.send_command(PumpCommand::BeginTransition(commands))
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    #[allow(dead_code)]
    pub(crate) fn stage_resume_lease(
        &self,
        generation: u64,
        resume: StagedResumeLease,
    ) -> Result<(), super::kms_live::KmsLiveError> {
        self.send_command(PumpCommand::StageResumeLease { generation, resume })
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    #[allow(dead_code)]
    pub(crate) fn transition_update(
        &self,
        generation: u64,
    ) -> Result<(), super::kms_live::KmsLiveError> {
        self.send_command(PumpCommand::TransitionUpdate { generation })
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    pub(crate) fn drain_scene(&self, generation: u64) -> Result<(), super::kms_live::KmsLiveError> {
        self.send_command(PumpCommand::DrainScene { generation })
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    pub(crate) fn nominal_refresh_interval(&self) -> Duration {
        self.nominal_refresh_interval
    }

    pub(crate) fn begin_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.presentation_cancel.cancel(CancelScope::AllGenerations);
        match self.commands.try_send(PumpCommand::Stop) {
            Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub(crate) fn cancel_generation_presentations(&self, generation: u64) {
        self.presentation_cancel
            .cancel(CancelScope::Generation(generation));
    }

    pub(crate) fn shutdown(mut self) -> Result<(), super::kms_live::KmsLiveError> {
        self.begin_stop();
        let timeout = {
            #[cfg(test)]
            {
                self.quiesce_timeout_override
                    .unwrap_or_else(|| live_pump_quiesce_timeout(self.nominal_refresh_interval))
            }
            #[cfg(not(test))]
            {
                live_pump_quiesce_timeout(self.nominal_refresh_interval)
            }
        };
        self.join.finish(timeout)
    }

    #[cfg(test)]
    pub(crate) fn blocked_for_test(quiesce_timeout: Duration) -> (Self, TestLiveRenderPumpBarrier) {
        Self::blocked_for_test_with_cancel(
            quiesce_timeout,
            PresentationCancelHandle::noop_for_test(),
        )
    }

    #[cfg(test)]
    pub(crate) fn blocked_for_test_with_cancel(
        quiesce_timeout: Duration,
        presentation_cancel: PresentationCancelHandle,
    ) -> (Self, TestLiveRenderPumpBarrier) {
        let (commands, command_receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let completion_release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let thread_completion_release = Arc::clone(&completion_release);
        let exit_release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let thread_exit_release = Arc::clone(&exit_release);
        let (entered_sender, entered) = mpsc::sync_channel(1);
        let (completion_sender, completion) = mpsc::sync_channel(1);
        let (completion_sent_sender, completion_sent) = mpsc::sync_channel(1);
        let (thread_exited_sender, thread_exited) = mpsc::sync_channel(1);
        let (transition_sender, transition) = mpsc::sync_channel(0);
        let (resume_transition, transition_resume) = mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            entered_sender.send(()).expect("announce blocked pump");
            wait_for_test_barrier(&thread_completion_release);
            let _ = completion_sender.send(Ok(()));
            let _ = completion_sent_sender.send(());
            wait_for_test_barrier(&thread_exit_release);
            let _ = thread_exited_sender.send(());
        });
        entered
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked pump reaches its barrier");
        (
            Self {
                commands,
                stop,
                presentation_cancel,
                join: BoundedPumpJoin {
                    completion,
                    thread: Some(thread),
                    state: LiveRenderPumpState::Running,
                    transition_probe: Some(TestPumpTransitionProbe {
                        reached: transition_sender,
                        resume: transition_resume,
                    }),
                },
                nominal_refresh_interval: Duration::from_millis(16),
                quiesce_timeout_override: Some(quiesce_timeout),
            },
            TestLiveRenderPumpBarrier {
                commands: command_receiver,
                completion_release,
                exit_release,
                completion_sent,
                thread_exited,
                transition,
                resume_transition,
            },
        )
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    fn send_command(&self, command: PumpCommand) -> Result<(), super::kms_live::KmsLiveError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => super::kms_live::KmsLiveError::Setup(
                    "live render pump already has an outstanding command".into(),
                ),
                TrySendError::Disconnected(_) => {
                    super::kms_live::KmsLiveError::Setup("live render pump stopped".into())
                }
            })
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveRenderPumpPreparation {
    pub(crate) fn wait_slice(
        &mut self,
        timeout: Duration,
    ) -> Result<LiveRenderPreparationStatus, super::kms_live::KmsLiveError> {
        if self.prepared.is_some() {
            return Ok(LiveRenderPreparationStatus::Ready);
        }
        match self.preparation.recv_timeout(timeout) {
            Ok(Ok(prepared)) => {
                self.prepared = Some(prepared);
                Ok(LiveRenderPreparationStatus::Ready)
            }
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(LiveRenderPreparationStatus::Pending),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(super::kms_live::KmsLiveError::Setup(
                "live render pump stopped during preparation".into(),
            )),
        }
    }

    pub(crate) fn finish(mut self) -> PreparedLiveRenderPump {
        let preparation = self
            .prepared
            .take()
            .expect("live render pump preparation finishes only after readiness");
        PreparedLiveRenderPump {
            pump: LiveRenderPump {
                commands: self.commands,
                stop: self.stop,
                presentation_cancel: self.presentation_cancel,
                join: self.join,
                nominal_refresh_interval: Duration::from_millis(16),
            },
            output_selector: preparation.output_selector,
            protocol_wiring: preparation.protocol_wiring,
        }
    }

    pub(crate) fn abort(mut self) -> Result<(), super::kms_live::KmsLiveError> {
        // Preparation now owns the same eventfd contract as the running pump:
        // publish stop first, then wake any blocked presentation, then enqueue
        // Stop. `abort_live_pump_preparation` repeats the idempotent store.
        self.stop.store(true, Ordering::Release);
        self.presentation_cancel.cancel(CancelScope::AllGenerations);
        abort_live_pump_preparation(
            self.stop.as_ref(),
            self.preparation,
            &self.commands,
            &mut self.join,
        )
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn abort_live_pump_preparation<T>(
    stop: &std::sync::atomic::AtomicBool,
    preparation: Receiver<T>,
    commands: &SyncSender<PumpCommand>,
    join: &mut BoundedPumpJoin,
) -> Result<(), super::kms_live::KmsLiveError> {
    stop.store(true, std::sync::atomic::Ordering::Release);
    drop(preparation);
    match commands.try_send(PumpCommand::Stop) {
        Ok(()) | Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => {}
    }
    join.finish(
        Duration::from_millis(16)
            .saturating_mul(2)
            .saturating_add(LIVE_PUMP_QUIESCE_MARGIN),
    )
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Drop for LiveRenderPump {
    fn drop(&mut self) {
        if self.join.state == LiveRenderPumpState::Running {
            self.begin_stop();
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn send_live_pump_preparation<T>(
    preparation: &SyncSender<T>,
    value: T,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<PreparationSendStatus, super::kms_live::KmsLiveError> {
    match preparation.send(value) {
        Ok(()) => Ok(PreparationSendStatus::Delivered),
        Err(_) if stop.load(std::sync::atomic::Ordering::Acquire) => {
            Ok(PreparationSendStatus::Cancelled)
        }
        Err(_) => Err(super::kms_live::KmsLiveError::Setup(
            "live coordinator stopped during render preparation".into(),
        )),
    }
}

// Same as LiveRenderPump::start above: eight distinct channels/handles the
// pump thread owns for its whole lifetime, not fields a struct would make
// fewer.
#[allow(clippy::too_many_arguments)]
#[cfg(all(feature = "kms-live", not(test)))]
fn run_live_render_pump(
    drm_device: u64,
    commands: Receiver<PumpCommand>,
    coordinator: Sender<super::kms_live::LiveCoordinatorEvent>,
    stop: Arc<AtomicBool>,
    preparation: SyncSender<Result<PumpPreparation, super::kms_live::KmsLiveError>>,
    target_pairing: super::kms_live::LiveTargetPairingLedger,
    scene: LiveRenderScene,
    atomic_cancellation: Arc<AtomicCancellation>,
) -> Result<(), super::kms_live::KmsLiveError> {
    let renderer = match cosmix_wgpu_dmabuf::ManualVulkanRenderer::new_for_drm_offscreen(drm_device)
    {
        Ok(renderer) => renderer,
        Err(error) => {
            let detail = error.to_string();
            let _ = preparation.send(Err(super::kms_live::KmsLiveError::Setup(detail.clone())));
            return Err(super::kms_live::KmsLiveError::Setup(detail));
        }
    };
    let scanout_bridge = renderer
        .scanout_render_bridge()
        .expect("live renderer retains its DRM identity");
    // The Rung-2 direct-display comparison sidecar retired with its backend.
    let output_selector = PreparedLiveOutputSelector(scanout_bridge.capabilities());
    let backend = LivePreparedBackend {
        bridge: scanout_bridge,
        cancellation: atomic_cancellation,
    };
    let protocol_wiring = crate::protocol::WaylandGpuWiring {
        dmabuf_capabilities: Some(renderer.capabilities().clone()),
        dmabuf_validator: Some(Box::new(renderer.dmabuf_validator())),
        retirement_adapter: Some(Box::new(renderer.retirement_adapter())),
    };
    let scene_mode = scene.mode;
    let app = match build_live_render_app(renderer, scene_mode, scene.decoration) {
        Ok(app) => app,
        Err(error) => {
            let detail = error.to_string();
            let _ = preparation.send(Err(super::kms_live::KmsLiveError::Setup(detail.clone())));
            return Err(super::kms_live::KmsLiveError::Setup(detail));
        }
    };
    if send_live_pump_preparation(
        &preparation,
        Ok(PumpPreparation {
            output_selector,
            protocol_wiring,
        }),
        &stop,
    )? == PreparationSendStatus::Cancelled
    {
        return Ok(());
    }

    let mut backend = Some(backend);
    let mut engine: Option<LiveRenderEngine> = None;
    let mut app = Some(app);
    loop {
        let Some(command) = receive_live_pump_command(&commands, &stop)? else {
            break;
        };
        match command {
            PumpCommand::Start {
                lease,
                output,
                initial_commands,
                topology_client,
                scene_feed,
            } => {
                let mut starting_app = app.take().expect("live render App starts once");
                let logical_extent = (
                    u32::try_from(output.logical_rect.width)
                        .expect("admitted output has a positive logical width"),
                    u32::try_from(output.logical_rect.height)
                        .expect("admitted output has a positive logical height"),
                );
                if let Err(error) = prepare_live_scene_start(
                    &mut starting_app,
                    scene_mode,
                    scene_feed.map(|feed| *feed),
                    logical_extent,
                    output.output_scale,
                ) {
                    send_pump_reply(&coordinator, PumpReply::Started(Err(error)))?;
                    return Ok(());
                }
                let result = LiveRenderEngine::start(
                    starting_app,
                    backend.take().expect("live render backend starts once"),
                    drm_device,
                    lease,
                    *output,
                    initial_commands,
                    topology_client,
                    LiveRenderStartControl {
                        target_pairing: target_pairing.clone(),
                        terminal_updates_stopped: Arc::clone(&stop),
                    },
                );
                match result {
                    Ok(started) => {
                        engine = Some(started);
                        send_pump_reply(&coordinator, PumpReply::Started(Ok(())))?;
                    }
                    Err(error) => {
                        send_pump_reply(&coordinator, PumpReply::Started(Err(error)))?;
                        return Ok(());
                    }
                }
            }
            PumpCommand::PollRegistration => {
                let result = engine
                    .as_mut()
                    .expect("live render engine starts before registration")
                    .poll_output_registration();
                let failed = result.is_err();
                send_pump_reply(&coordinator, PumpReply::Registration(result))?;
                if failed {
                    break;
                }
            }
            PumpCommand::Update => {
                let current = engine
                    .as_mut()
                    .expect("live render engine starts before updates");
                let (reply, failed) = live_pump_update_reply(current);
                send_pump_reply(&coordinator, reply)?;
                if failed {
                    break;
                }
            }
            PumpCommand::BeginTransition(commands) => {
                let generation = commands
                    .last()
                    .map(kms_render_command_generation)
                    .unwrap_or_default();
                let result = engine
                    .as_mut()
                    .expect("live render engine starts before transitions")
                    .begin_transition(commands);
                let failed = result.is_err();
                send_pump_reply(
                    &coordinator,
                    PumpReply::TransitionBegun { generation, result },
                )?;
                if failed {
                    break;
                }
            }
            PumpCommand::StageResumeLease { generation, resume } => {
                let result = engine
                    .as_ref()
                    .expect("live render engine starts before resume staging")
                    .stage_resume_lease(generation, resume);
                let failed = result.is_err();
                send_pump_reply(
                    &coordinator,
                    PumpReply::ResumeLeaseStaged { generation, result },
                )?;
                if failed {
                    break;
                }
            }
            PumpCommand::TransitionUpdate { generation } => {
                let current = engine
                    .as_mut()
                    .expect("live render engine starts before transition updates");
                let result = if generation == current.transition_generation {
                    current.transition_update()
                } else {
                    Err(super::kms_live::KmsLiveError::Setup(format!(
                        "kms-live-stale-generation: transition update generation {generation} does not match {}",
                        current.transition_generation
                    )))
                };
                let failed = result.is_err();
                send_pump_reply(
                    &coordinator,
                    PumpReply::TransitionUpdated { generation, result },
                )?;
                if failed {
                    break;
                }
            }
            PumpCommand::DrainScene { generation } => {
                let current = engine
                    .as_mut()
                    .expect("live render engine starts before scene drains");
                let result = if generation == current.transition_generation {
                    current.drain_scene()
                } else {
                    Err(super::kms_live::KmsLiveError::Setup(format!(
                        "kms-live-stale-generation: scene drain generation {generation} does not match {}",
                        current.transition_generation
                    )))
                };
                let failed = result.is_err();
                send_pump_reply(&coordinator, PumpReply::SceneDrained { generation, result })?;
                if failed {
                    break;
                }
            }
            PumpCommand::Stop => break,
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
    }
    let result = match engine {
        Some(engine) => engine.shutdown(),
        None => Ok(()),
    };
    let _ = coordinator.send(super::kms_live::LiveCoordinatorEvent::Pump(
        PumpReply::Exited,
    ));
    result
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn receive_live_pump_command(
    commands: &Receiver<PumpCommand>,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<Option<PumpCommand>, super::kms_live::KmsLiveError> {
    let command = commands.recv().map_err(|_| {
        super::kms_live::KmsLiveError::Setup("live render pump command channel closed".into())
    })?;
    Ok((!stop.load(std::sync::atomic::Ordering::Acquire)).then_some(command))
}

#[cfg(all(feature = "kms-live", not(test)))]
fn send_pump_reply(
    coordinator: &Sender<super::kms_live::LiveCoordinatorEvent>,
    reply: PumpReply,
) -> Result<(), super::kms_live::KmsLiveError> {
    coordinator
        .send(super::kms_live::LiveCoordinatorEvent::Pump(reply))
        .map_err(|_| super::kms_live::KmsLiveError::Setup("live coordinator stopped".into()))
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveRenderPlatform {
    ownership: Option<LiveRenderPlatformOwnership>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveRenderPlatformOwnership(LiveAtomicOwnership);

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn drop_live_ownership_fail_closed<T, E>(
    ownership: &mut Option<T>,
    retire_submitted_work: impl FnOnce(&mut T) -> Result<(), E>,
    destroy_after_retirement: impl FnOnce(&mut T) -> Result<(), E>,
    report: impl FnOnce(&E),
) {
    let result = retire_submitted_work(
        ownership
            .as_mut()
            .expect("live platform ownership exists until fallback teardown"),
    );
    let result = match result {
        Ok(()) => destroy_after_retirement(
            ownership
                .as_mut()
                .expect("live platform ownership exists after proved retirement"),
        ),
        Err(failure) => {
            // A failed retirement wait is not proof that the GPU released its
            // surfaces. Leak the complete platform ownership island fail-closed
            // instead of letting automatic field destruction tear them down.
            let ownership = ownership
                .take()
                .expect("live platform ownership exists when retirement is unproven");
            std::mem::forget(ownership);
            Err(failure)
        }
    };
    if let Err(failure) = &result {
        report(failure);
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveAtomicOwnership {
    targets: BTreeMap<OutputKey, LiveAtomicTarget>,
    retained_buffers: BTreeMap<OutputKey, RetainedAtomicBuffer>,
    fail_closed_ownership_islands: FailClosedAtomicOwnershipIslands,
    bridge: cosmix_wgpu_dmabuf::ScanoutRenderBridge,
    cancellation: Arc<AtomicCancellation>,
    lease: Option<super::kms_live::MasterDrmLease>,
    resume_leases: LiveResumeLeaseSlot,
    target_generation: u64,
    target_pairing: super::kms_live::LiveTargetPairingLedger,
    gpu_retirement: cosmix_wgpu_dmabuf::WgpuWaitForSubmittedWork,
    drm_device: u64,
    event_router: Option<Arc<ProductionAtomicEventRouter>>,
    resume_presentation: Option<ResumePresentationPlan>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveAtomicTarget {
    generation: u64,
    state: Arc<Mutex<LiveAtomicTargetState>>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveAtomicTargetState {
    pool: ScanoutPool,
    presenter: AtomicPresenter<ProductionAtomicIo>,
    telemetry: AtomicPresentTelemetry,
    unpresented_rendering: BTreeSet<ScanoutSlotId>,
    retained_handoff: Option<RetainedAtomicHandoff>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct RetainedAtomicBuffer {
    buffer: RetainedScanoutBuffer,
    _ledger: RetainedBufferLedgerGuard,
}

#[cfg(all(feature = "kms-live", not(test)))]
impl RetainedAtomicBuffer {
    fn new(
        generation: u64,
        buffer: RetainedScanoutBuffer,
        pairing: super::kms_live::LiveTargetPairingLedger,
    ) -> Self {
        Self {
            buffer,
            _ledger: RetainedBufferLedgerGuard::new(generation, pairing),
        }
    }

    fn mark_pending_handoff(&mut self) {
        self._ledger.mark_pending_handoff();
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct RetainedBufferLedgerGuard {
    generation: u64,
    pairing: super::kms_live::LiveTargetPairingLedger,
    pending_handoff: bool,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl RetainedBufferLedgerGuard {
    fn new(generation: u64, pairing: super::kms_live::LiveTargetPairingLedger) -> Self {
        pairing.record_retained_created(generation);
        Self {
            generation,
            pairing,
            pending_handoff: false,
        }
    }

    fn mark_pending_handoff(&mut self) {
        if !self.pending_handoff {
            self.pairing
                .record_retained_handoff_started(self.generation);
            self.pending_handoff = true;
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Drop for RetainedBufferLedgerGuard {
    fn drop(&mut self) {
        self.pairing
            .record_retained_released(self.generation, self.pending_handoff);
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
struct RetainedAtomicHandoff {
    slot: ScanoutSlotId,
    _retained: RetainedAtomicBuffer,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn settle_unpresented_after_retirement<E>(
    slots: &mut BTreeSet<ScanoutSlotId>,
    mut settle: impl FnMut(ScanoutSlotId) -> Result<(), E>,
) -> Result<(), E> {
    let mut pending = std::mem::take(slots).into_iter();
    while let Some(slot) = pending.next() {
        if let Err(error) = settle(slot) {
            slots.insert(slot);
            slots.extend(pending);
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(all(feature = "kms-live", not(test)))]
fn unpresented_atomic_frame_guard(
    state: Arc<Mutex<LiveAtomicTargetState>>,
    slot: ScanoutSlotId,
) -> UnpresentedFrameGuard {
    UnpresentedFrameGuard::new(slot, move |slot| match state.lock() {
        Ok(mut state) => {
            state.unpresented_rendering.insert(slot);
        }
        Err(_) => tracing::error!(
            code = "kms-live-atomic-unpresented-slot-ledger-poisoned",
            slot = slot.0,
            "could not record an abandoned atomic Rendering slot; fail-closed teardown will retain it"
        ),
    })
}

#[cfg(all(feature = "kms-live", not(test)))]
#[derive(Default)]
struct FailClosedAtomicOwnershipIslands {
    retained: Vec<Arc<Mutex<LiveAtomicTargetState>>>,
}

#[cfg(all(feature = "kms-live", not(test)))]
impl FailClosedAtomicOwnershipIslands {
    fn retain(&mut self, island: Arc<Mutex<LiveAtomicTargetState>>) {
        self.retained.push(island);
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl Drop for FailClosedAtomicOwnershipIslands {
    fn drop(&mut self) {
        // These islands reached a state where explicit disable/RmFB could not
        // be proved. Preserve a permanent strong owner rather than allowing a
        // later incidental last-drop to close duplicated fds or GBM storage.
        for island in std::mem::take(&mut self.retained) {
            let _ = Arc::into_raw(island);
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
struct AtomicPresentTelemetry {
    frames: u64,
    wall_total: Duration,
    cpu_total: Duration,
    interval_started: Instant,
}

#[cfg(all(feature = "kms-live", not(test)))]
impl AtomicPresentTelemetry {
    fn new() -> Self {
        Self {
            frames: 0,
            wall_total: Duration::ZERO,
            cpu_total: Duration::ZERO,
            interval_started: Instant::now(),
        }
    }

    fn observe(&mut self, wall: Duration, cpu: Duration) {
        self.frames = self.frames.saturating_add(1);
        self.wall_total = self.wall_total.saturating_add(wall);
        self.cpu_total = self.cpu_total.saturating_add(cpu);
        if self.interval_started.elapsed() >= Duration::from_secs(1) {
            tracing::info!(
                atomic_frames = self.frames,
                atomic_frame_wall_us = self.wall_total.as_micros(),
                atomic_present_cpu_us = self.cpu_total.as_micros(),
                "live KMS atomic presentation telemetry"
            );
            self.frames = 0;
            self.wall_total = Duration::ZERO;
            self.cpu_total = Duration::ZERO;
            self.interval_started = Instant::now();
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn thread_cpu_time() -> Duration {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(value.tv_sec.max(0) as u64, value.tv_nsec.max(0) as u32)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
type LiveResumeLeaseSlot = Arc<Mutex<GenerationLeaseSlot<StagedResumeLease>>>;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct GenerationLeaseSlot<T> {
    lease: Option<(u64, T)>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl<T> Default for GenerationLeaseSlot<T> {
    fn default() -> Self {
        Self { lease: None }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl<T> GenerationLeaseSlot<T> {
    fn generation(&self) -> Option<u64> {
        self.lease.as_ref().map(|(generation, _)| *generation)
    }

    fn stage(&mut self, generation: u64, lease: T) -> Result<(), &'static str> {
        if self.lease.is_some() {
            return Err("kms-live-resume-lease-duplicate");
        }
        self.lease = Some((generation, lease));
        Ok(())
    }

    fn take(&mut self, generation: u64) -> Result<T, &'static str> {
        match self.lease.as_ref() {
            Some((staged_generation, _)) if *staged_generation == generation => self
                .lease
                .take()
                .map(|(_, lease)| lease)
                .ok_or("kms-live-resume-lease-missing"),
            Some(_) => Err("kms-live-resume-lease-generation-mismatch"),
            None => Err("kms-live-resume-lease-missing"),
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveAtomicOwnership {
    fn allocate_fresh_pool(
        &self,
        lease: &super::kms_live::MasterDrmLease,
        selection: super::kms::AtomicOutputSelection,
        staged_deadline: Option<Instant>,
        phase: &'static str,
    ) -> Result<ScanoutPool, KmsRenderPlatformFailure> {
        let config = ScanoutPoolConfig::two_slot();
        let allocated = match staged_deadline {
            Some(deadline) => ScanoutPool::allocate_with_staged_deadline(
                lease.fd.as_fd(),
                self.drm_device,
                selection,
                config,
                &self.bridge,
                deadline,
            ),
            None => ScanoutPool::allocate(
                lease.fd.as_fd(),
                self.drm_device,
                selection,
                config,
                &self.bridge,
            ),
        };
        allocated.map_err(|error| atomic_platform_failure(phase, error))
    }

    fn reap_retained_buffers_except(
        &mut self,
        mut keep: impl FnMut(&OutputKey) -> bool,
        phase: &'static str,
    ) {
        let stale = self
            .retained_buffers
            .keys()
            .filter(|key| !keep(key))
            .cloned()
            .collect::<Vec<_>>();
        for key in stale {
            if self.retained_buffers.remove(&key).is_some() {
                tracing::info!(
                    code = "kms-live-atomic-retained-output-vanished",
                    phase,
                    connector = %key.connector_name,
                    "reaped retained scanout storage for an output absent from the authoritative target set"
                );
            }
        }
    }

    fn source(
        &mut self,
        output: &super::kms::SelectedOutput,
    ) -> Result<RenderSource<KmsRenderPlaceholder>, KmsRenderPlatformFailure> {
        if self.targets.contains_key(&output.key) {
            return Err(KmsRenderPlatformFailure::new(
                "kms-live-target-already-exists",
                format!(
                    "atomic target {} is already active",
                    output.key.connector_name
                ),
            ));
        }
        let selection = output.display;
        // Modifier tie-break policy remains format rank -> non-linear -> raw
        // modifier. Its Rung-3 revisit data comes from this selected tuple plus
        // the one-second atomic frame-wall/CPU telemetry emitted below during
        // Mark's live self-switch and external-switch gate.
        tracing::info!(
            format = format_args!("{:#010x}", selection.format),
            modifier = format_args!("{:#018x}", selection.modifier),
            connector_id = selection.connector_id,
            crtc_id = selection.crtc_id,
            plane_id = selection.primary_plane_id,
            "live KMS atomic scanout selection"
        );
        // AddOutput/ChangeOutput identifies the sole authoritative target for
        // this generation. Retained keys not matching it represent connectors
        // which vanished while paused and must not pin plane fds indefinitely.
        self.reap_retained_buffers_except(|key| key == &output.key, "resume-source");
        let resume_plan = self.resume_presentation.take();
        // Consume the staged deadline before any optional retained-buffer
        // import, pool allocation or presenter admission. No seamless substep
        // may mint a fresh budget after this point.
        let staged_resume_deadline = match resume_plan {
            Some(plan) => Some(plan.deadline.instant().ok_or_else(|| {
                KmsRenderPlatformFailure::terminal(
                    "kms-live-atomic-seamless-unbounded",
                    "staged atomic resume has no absolute deadline",
                )
            })?),
            None => None,
        };
        let lease = self.lease.as_ref().ok_or_else(|| {
            KmsRenderPlatformFailure::new(
                "kms-live-authority-unavailable",
                "atomic source creation requires an active master lease",
            )
        })?;
        self.cancellation.arm_generation(self.target_generation);
        let event_router = match self.event_router.as_ref() {
            Some(router) => Arc::clone(router),
            None => {
                let fd = lease.fd.as_fd().try_clone_to_owned().map_err(|error| {
                    atomic_platform_failure("event-router fd duplication", error)
                })?;
                let router = ProductionAtomicEventRouter::new(fd)
                    .map_err(|error| atomic_platform_failure("event-router creation", error))?;
                self.event_router = Some(Arc::clone(&router));
                router
            }
        };
        let mut retained = self.retained_buffers.remove(&output.key);
        let classifier_eligible = seamless_resume_is_eligible(
            resume_plan,
            retained.is_some(),
            retained
                .as_ref()
                .is_some_and(|retained| retained.buffer.selection() == selection),
        );
        let seamless_budget_available = staged_resume_deadline
            .is_some_and(|deadline| seamless_resume_has_minimum_budget(Instant::now(), deadline));
        let seamless_eligible = classifier_eligible && seamless_budget_available;
        if classifier_eligible && !seamless_budget_available {
            tracing::info!(
                minimum_budget_ms = SEAMLESS_RESUME_MINIMUM_BUDGET.as_millis(),
                "discarding retained scanout storage; staged resume lacks the minimum attempt + drain + fallback reserve"
            );
        }
        if retained.is_some() && !seamless_eligible {
            tracing::info!(
                classification = ?resume_plan.map(|plan| plan.classification),
                "discarding retained scanout storage; resume requires a full modeset"
            );
            drop(retained.take());
        }

        let mut retained_candidate = None;
        let mut pool = if let Some(retained_buffer) = retained.as_ref() {
            match ScanoutPool::allocate_with_retained(
                lease.fd.as_fd(),
                self.drm_device,
                selection,
                ScanoutPoolConfig::new(3)
                    .expect("three slots are within the bounded scanout-pool configuration"),
                &self.bridge,
                &retained_buffer.buffer,
                staged_resume_deadline
                    .expect("eligible seamless resume has a staged absolute deadline"),
            ) {
                Ok((pool, slot)) => {
                    retained_candidate = Some(slot);
                    pool
                }
                Err(error) => {
                    tracing::warn!(
                        cause = %error,
                        "retained scanout import failed; falling back to a fresh full-modeset pool"
                    );
                    drop(retained.take());
                    self.allocate_fresh_pool(
                        lease,
                        selection,
                        staged_resume_deadline,
                        "fallback pool allocation",
                    )?
                }
            }
        } else {
            self.allocate_fresh_pool(lease, selection, staged_resume_deadline, "pool allocation")?
        };

        let admission_deadline = atomic_admission_deadline(Instant::now(), staged_resume_deadline)
            .ok_or_else(|| {
                KmsRenderPlatformFailure::terminal(
                    "kms-live-atomic-resume-deadline-expired",
                    "staged resume deadline expired before atomic presenter admission",
                )
            })?;
        let mut presenter = match production_presenter_for_pool(
            &pool,
            Arc::clone(&self.cancellation),
            Arc::clone(&event_router),
            self.target_generation,
            admission_deadline,
        ) {
            Ok(presenter) => presenter,
            Err(error) if retained_candidate.is_some() && error.retain_pool => {
                tracing::error!(
                    code = error.code,
                    detail = %error.detail,
                    "retained atomic admission cleanup was unproved; terminating with the surrendered presenter and matching GBM pool deliberately retained"
                );
                // AtomicPresenter::production has already fail-closed the
                // matching presenter/DRM ownership. The caller only retains
                // the pool half, so no complete ownership island can be built
                // here; terminal process teardown is the only honest outcome.
                std::mem::forget(pool);
                return Err(KmsRenderPlatformFailure::terminal(error.code, error.detail));
            }
            Err(error) if retained_candidate.is_some() => {
                tracing::warn!(
                    code = error.code,
                    detail = %error.detail,
                    "retained scanout framebuffer/admission setup failed; falling back to a fresh full-modeset pool"
                );
                drop(retained.take());
                retained_candidate = None;
                pool = self.allocate_fresh_pool(
                    lease,
                    selection,
                    staged_resume_deadline,
                    "fallback pool allocation",
                )?;
                let fallback_admission_deadline = atomic_admission_deadline(
                    Instant::now(),
                    staged_resume_deadline,
                )
                .ok_or_else(|| {
                    KmsRenderPlatformFailure::terminal(
                        "kms-live-atomic-resume-deadline-expired",
                        "staged resume deadline expired before fallback atomic presenter admission",
                    )
                })?;
                match production_presenter_for_pool(
                    &pool,
                    Arc::clone(&self.cancellation),
                    Arc::clone(&event_router),
                    self.target_generation,
                    fallback_admission_deadline,
                ) {
                    Ok(presenter) => presenter,
                    Err(error) => {
                        if error.retain_pool {
                            tracing::error!(
                                code = error.code,
                                detail = %error.detail,
                                "fallback atomic admission cleanup was unproved; deliberately leaking the matching GBM pool"
                            );
                            std::mem::forget(pool);
                        }
                        return Err(KmsRenderPlatformFailure::terminal(error.code, error.detail));
                    }
                }
            }
            Err(error) => {
                if error.retain_pool {
                    tracing::error!(
                        code = error.code,
                        detail = %error.detail,
                        "atomic admission cleanup was unproved; deliberately leaking the matching GBM pool"
                    );
                    std::mem::forget(pool);
                }
                return Err(KmsRenderPlatformFailure::terminal(error.code, error.detail));
            }
        };

        if let Some(slot) = retained_candidate {
            pool.queue_retained_candidate(slot)
                .map_err(|error| atomic_platform_failure("retained slot queue", error))?;
        }
        // Acquire the first fresh rendering target before the optional live
        // flip. Once retained content is displayed, no fallible camera-target
        // acquisition may remain outside the installed ownership island.
        let first_slot = pool
            .begin_rendering()
            .map_err(|error| atomic_platform_failure("initial slot acquisition", error))?;
        let first_view = pool
            .manual_view(first_slot)
            .map_err(|error| atomic_platform_failure("initial target view", error))?;

        let mut retained_handoff = None;
        if let (Some(slot), Some(_plan), Some(mut retained_buffer)) =
            (retained_candidate, resume_plan, retained.take())
        {
            let overall_resume_deadline = staged_resume_deadline
                .expect("retained resume plan was validated before optional allocation");
            let seamless_deadline =
                optional_atomic_resume_stage_deadline(Instant::now(), overall_resume_deadline, 2)
                    .map(PresentDeadline::bounded);
            let outcome = seamless_deadline.map(|deadline| {
                presenter.present_retained_seamless(slot, self.target_generation, deadline)
            });
            match outcome {
                None => {
                    pool.abandon_uncommitted_retained_candidate(slot)
                        .map_err(|error| {
                            atomic_platform_failure("deadline-skipped retained slot discard", error)
                        })?;
                    drop(retained_buffer);
                    tracing::warn!(
                        "seamless resume was skipped because its attempt and drain could not preserve a full atomic-present fallback budget"
                    );
                }
                Some(Ok(PresentOutcome::Displayed)) => {
                    pool.display_queued(slot).map_err(|error| {
                        atomic_platform_failure("retained pageflip completion", error)
                    })?;
                    retained_buffer.mark_pending_handoff();
                    retained_handoff = Some(RetainedAtomicHandoff {
                        slot,
                        _retained: retained_buffer,
                    });
                    tracing::info!(
                        generation = self.target_generation,
                        slot = slot.0,
                        "kms-live seamless resume displayed retained scanout storage"
                    );
                }
                Some(Ok(PresentOutcome::Cancelled)) => {
                    if presenter.has_pending_commit() {
                        pool.cancel(slot).map_err(|error| {
                            atomic_platform_failure("cancelled retained slot hold", error)
                        })?;
                    } else {
                        pool.abandon_uncommitted_retained_candidate(slot)
                            .map_err(|error| {
                                atomic_platform_failure("cancelled retained slot discard", error)
                            })?;
                    }
                    drop(retained_buffer);
                    tracing::warn!(
                        "seamless resume was cancelled; retaining full-modeset policy for the next rendered frame"
                    );
                }
                Some(Err(error)) => {
                    let mut retained_buffer = Some(retained_buffer);
                    if presenter.has_pending_commit() {
                        let drain = optional_atomic_resume_stage_deadline(
                            Instant::now(),
                            overall_resume_deadline,
                            1,
                        )
                        .map_or(
                            Ok(PendingFlipDrainOutcome::Deadline),
                            |deadline| {
                                presenter.drain_pending_flip(self.target_generation, deadline)
                            },
                        );
                        match drain {
                            Ok(PendingFlipDrainOutcome::Drained) => {
                                pool.display_queued(slot).map_err(|display| {
                                    atomic_platform_failure(
                                        "late retained pageflip completion",
                                        display,
                                    )
                                })?;
                                retained_buffer
                                    .as_mut()
                                    .expect("late retained flip still owns its guard")
                                    .mark_pending_handoff();
                                retained_handoff = Some(RetainedAtomicHandoff {
                                    slot,
                                    _retained: retained_buffer
                                        .take()
                                        .expect("late retained flip still owns its guard"),
                                });
                                tracing::warn!(
                                    code = error.code,
                                    detail = %error.detail,
                                    "seamless pageflip completed during bounded fallback drain; retaining it as the resumed front buffer"
                                );
                            }
                            Ok(
                                PendingFlipDrainOutcome::Cancelled
                                | PendingFlipDrainOutcome::Deadline,
                            ) => {
                                pool.cancel(slot).map_err(|hold| {
                                    atomic_platform_failure("failed retained slot hold", hold)
                                })?;
                            }
                            Err(drain) => {
                                pool.cancel(slot).map_err(|hold| {
                                    atomic_platform_failure("failed retained slot hold", hold)
                                })?;
                                tracing::warn!(
                                    cause = %drain,
                                    "pending seamless flip could not be decoded during fallback drain"
                                );
                            }
                        }
                    } else {
                        pool.abandon_uncommitted_retained_candidate(slot)
                            .map_err(|discard| {
                                atomic_platform_failure("failed retained slot discard", discard)
                            })?;
                    }
                    if let Some(retained_buffer) = retained_buffer {
                        drop(retained_buffer);
                        tracing::warn!(
                            code = error.code,
                            detail = %error.detail,
                            "seamless resume attempt failed; falling back to the normal full-modeset first frame"
                        );
                    }
                }
            }
        }

        // The slot is already Rendering: keep the path to its guard below
        // infallible, or explicitly settle it before adding any new failure.
        // The TEST_ONLY probe has already proved this exact slot/request. Keep
        // the mutable binding explicit so no later refactor can omit it.
        debug_assert!(presenter.framebuffer(first_slot).is_some());
        let state = Arc::new(Mutex::new(LiveAtomicTargetState {
            pool,
            presenter,
            telemetry: AtomicPresentTelemetry::new(),
            unpresented_rendering: BTreeSet::new(),
            retained_handoff,
        }));
        let first_guard = unpresented_atomic_frame_guard(Arc::clone(&state), first_slot);
        let first = Arc::new(Mutex::new(Some((
            first_slot,
            first_view.clone(),
            first_guard,
        ))));
        let acquisition_state = Arc::clone(&state);
        let acquisition_first = Arc::clone(&first);
        let generation = self.target_generation;
        self.target_pairing.record_created(generation);
        self.targets
            .insert(output.key.clone(), LiveAtomicTarget { generation, state });
        Ok(RenderSource {
            placeholder: KmsRenderPlaceholder {
                extent: manual_texture_view_extent(&first_view),
                logical_extent: selected_logical_extent(output),
                view: Some(first_view),
            },
            acquire: Box::new(move || {
                let (slot, view, unpresented_guard) = if let Some(first) = acquisition_first
                    .lock()
                    .map_err(|_| {
                        KmsRenderPlatformFailure::terminal(
                            "kms-live-atomic-initial-frame-poisoned",
                            "initial atomic frame was poisoned",
                        )
                    })?
                    .take()
                {
                    first
                } else {
                    let mut state = acquisition_state.lock().map_err(|_| {
                        KmsRenderPlatformFailure::terminal(
                            "kms-live-atomic-target-poisoned",
                            "atomic target was poisoned during frame acquisition",
                        )
                    })?;
                    let slot = state
                        .pool
                        .begin_rendering()
                        .map_err(|error| atomic_platform_failure("begin rendering", error))?;
                    let view = state
                        .pool
                        .manual_view(slot)
                        .map_err(|error| atomic_platform_failure("manual view", error))?;
                    // Guard creation must remain the next infallible step after
                    // acquiring a Rendering slot and its view.
                    let guard =
                        unpresented_atomic_frame_guard(Arc::clone(&acquisition_state), slot);
                    (slot, view, guard)
                };
                let presentation_state = Arc::clone(&acquisition_state);
                Ok(AcquiredOutputFrame {
                    view,
                    present: fallible_present_output_frame(move |_present_deadline| {
                        let mut unpresented_guard = unpresented_guard;
                        let wall_started = Instant::now();
                        let cpu_started = thread_cpu_time();
                        let mut state = presentation_state.lock().map_err(|_| {
                            KmsRenderPlatformFailure::terminal(
                                "kms-live-atomic-target-poisoned",
                                "atomic target lock was poisoned during presentation",
                            )
                        })?;
                        // Rung 6 (scanout explicit fencing via IN_FENCE_FD)
                        // was SKIPPED: this bounded implicit GPU-completion
                        // wait measured ~0.3 ms/frame present CPU on Arc/MTL
                        // (banked gate telemetry, 2026-08-17), so the plan's
                        // conditional trigger never fired. Wayland
                        // linux-drm-syncobj client sync is separate and IS
                        // implemented.
                        if let Err(error) = state.pool.prove_rendering_complete(slot) {
                            // The pool already moved a timed-out Rendering
                            // slot to HeldUntilSuspend; do not record it for a
                            // second post-retirement transition.
                            unpresented_guard.disarm();
                            return Err(atomic_platform_failure("GPU completion", error));
                        }
                        // GPU retirement has its own bounded wait. Start the
                        // page-flip budget only after that proof completes so
                        // one deadline can never consume the other.
                        let allow_modeset = state.presenter.next_present_allows_modeset();
                        let deadline = atomic_present_deadline(Instant::now(), allow_modeset);
                        state
                            .pool
                            .queue(slot)
                            .map_err(|error| atomic_platform_failure("slot queue", error))?;
                        unpresented_guard.disarm();
                        let outcome = state.presenter.present(slot, generation, deadline);
                        match &outcome {
                            Ok(PresentOutcome::Displayed) => {
                                let old_front =
                                    state.pool.display_queued(slot).map_err(|error| {
                                        atomic_platform_failure("pageflip completion", error)
                                    })?;
                                if state
                                    .retained_handoff
                                    .as_ref()
                                    .is_some_and(|handoff| Some(handoff.slot) == old_front)
                                {
                                    // The retained image remained Front until
                                    // this freshly rendered flip completed.
                                    // Dropping the guard records its one and
                                    // only retained-ledger release.
                                    drop(state.retained_handoff.take());
                                }
                            }
                            Ok(PresentOutcome::Cancelled) | Err(_) => {
                                state.pool.cancel(slot).map_err(|error| {
                                    atomic_platform_failure("cancelled slot hold", error)
                                })?;
                            }
                        }
                        state.telemetry.observe(
                            wall_started.elapsed(),
                            thread_cpu_time().saturating_sub(cpu_started),
                        );
                        outcome
                    }),
                })
            }),
        })
    }

    fn destroy_targets(
        &mut self,
        key: Option<&OutputKey>,
        retain_displayed_buffer: bool,
    ) -> Result<(), KmsRenderPlatformFailure> {
        if key.is_none() && retain_displayed_buffer {
            let active_keys = self.targets.keys().cloned().collect::<BTreeSet<_>>();
            self.reap_retained_buffers_except(
                |retained_key| active_keys.contains(retained_key),
                "suspend-authoritative-target-set",
            );
        }
        let targets = if let Some(key) = key {
            self.targets
                .remove(key)
                .map(|target| (key.clone(), target))
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            std::mem::take(&mut self.targets)
                .into_iter()
                .collect::<Vec<_>>()
        };
        let mut first = None;
        for (key, target) in targets {
            let released = match Arc::try_unwrap(target.state) {
                Ok(state) => Ok(state),
                Err(retained) => {
                    let owners = Arc::strong_count(&retained);
                    tracing::error!(
                        code = "kms-live-atomic-referenced-ownership-island-retained",
                        owners,
                        "atomic target remained referenced after render quiescence; retaining its complete ownership island permanently"
                    );
                    self.fail_closed_ownership_islands.retain(retained);
                    Err(KmsRenderPlatformFailure::terminal(
                        "kms-live-target-still-referenced",
                        format!("atomic target retained {owners} owners after render quiescence"),
                    ))
                }
            }
            .and_then(|state| {
                    let mut state = match state.into_inner() {
                        Ok(state) => state,
                        Err(poisoned) => {
                            let state = poisoned.into_inner();
                            std::mem::forget(state);
                            return Err(KmsRenderPlatformFailure::terminal(
                            "kms-live-atomic-target-poisoned",
                                "atomic target lock was poisoned during teardown; ownership island was deliberately retained",
                            ));
                        }
                    };
                    let retained = if retain_displayed_buffer {
                        match state.pool.retain_front_buffer() {
                            Ok(retained) => retained,
                            Err(error) => {
                                tracing::warn!(
                                    cause = %error,
                                    connector = %key.connector_name,
                                    "last displayed scanout storage could not be retained; resume will use a full modeset"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let drain_timeout = state.presenter.pending_commit_teardown_timeout(
                        ATOMIC_PRESENT_TIMEOUT,
                        ATOMIC_MODESET_TIMEOUT,
                    );
                    let deadline = Instant::now() + drain_timeout;
                    let cleanup = (|| {
                        // A cancelled NONBLOCK commit still owns a kernel event
                        // and Queued storage. Drain it first within the same
                        // absolute teardown budget; if it remains in flight,
                        // disable handles EBUSY by waiting and retrying within
                        // that unchanged deadline.
                        let _ = state
                            .presenter
                            .drain_pending_flip_for_teardown(deadline)
                            .map_err(|error| atomic_platform_failure("pending-flip drain", error))?;
                        // An authority-intact self-switch proves this disable,
                        // so resume observes an inactive CRTC and conservatively
                        // chooses a full modeset. Seamless retention is expected
                        // only after external revocation prevents the disable.
                        let authority_revoked = match state.presenter.disable_nonblocking(deadline)
                        {
                            Ok(()) => {
                                // The proved disable removes kernel scanout
                                // use before RmFB. Change only the slot ledger;
                                // buffer storage remains owned through RmFB.
                                state.pool.mark_disabled().map_err(|error| {
                                    atomic_platform_failure("disabled slot ledger", error)
                                })?;
                                false
                            }
                            Err(error) if error.authority_was_revoked() => {
                                // Revocation means this file description can no
                                // longer mutate KMS state; the kernel has ended
                                // this master's scanout lifetime. RmFB is still
                                // attempted to release object IDs, and the full
                                // island is retained if the kernel refuses it.
                                true
                            }
                            Err(error) => {
                                return Err(atomic_platform_failure(
                                    "non-blocking disable",
                                    error,
                                ));
                            }
                        };
                        let slot_states = state.pool.slot_state_view();
                        state
                            .presenter
                            .remove_framebuffers(&slot_states, authority_revoked)
                            .map_err(|error| atomic_platform_failure("framebuffer removal", error))?;
                        state
                            .presenter
                            .destroy_mode_blob()
                            .map_err(|error| atomic_platform_failure("mode-blob removal", error))?;
                        // RmFB must precede dropping held GBM storage. Render
                        // quiescence and bounded GPU retirement already removed
                        // every Vulkan/wgpu owner.
                        state.pool.release_after_suspend();
                        Ok(())
                    })();
                    if let Err(error) = cleanup {
                        tracing::error!(
                            code = "kms-live-atomic-ownership-island-retained",
                            detail = %error.detail,
                            "atomic teardown was unproved; deliberately leaking the complete DRM/GBM/Vulkan ownership island"
                        );
                        std::mem::forget(state);
                        return Err(error);
                    }
                    Ok(retained)
                });
            if released.is_ok() {
                self.target_pairing.record_released(target.generation);
            }
            if let Ok(Some(buffer)) = released.as_ref() {
                let retained = RetainedAtomicBuffer::new(
                    target.generation,
                    buffer.clone(),
                    self.target_pairing.clone(),
                );
                drop(self.retained_buffers.insert(key.clone(), retained));
            }
            if let Err(error) = released
                && first.is_none()
            {
                first = Some(error);
            }
        }
        first.map_or(Ok(()), Err)
    }

    fn retire_submitted_work(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        cosmix_wgpu_dmabuf::WaitForSubmittedWork::wait_for_submitted_work(
            &mut self.gpu_retirement,
            cosmix_wgpu_dmabuf::RETIREMENT_WAIT_TIMEOUT,
        )
        .map_err(|error| atomic_platform_failure("submitted-work retirement", error))?;

        // Render-world quiescence has dropped every acquire/present closure and
        // the global wait above proves their GPU use retired. Only now may an
        // unpresented Rendering slot leave that state; it remains held and is
        // never recycled before suspend destroys/replaces its storage.
        for target in self.targets.values() {
            let mut state = target.state.lock().map_err(|_| {
                KmsRenderPlatformFailure::terminal(
                    "kms-live-atomic-target-poisoned",
                    "atomic target lock was poisoned while settling unpresented frames",
                )
            })?;
            let mut unpresented = std::mem::take(&mut state.unpresented_rendering);
            if let Err(error) = settle_unpresented_after_retirement(&mut unpresented, |slot| {
                state.pool.settle_unpresented_after_retirement(slot)
            }) {
                state.unpresented_rendering.extend(unpresented);
                return Err(atomic_platform_failure(
                    "unpresented frame settlement",
                    error,
                ));
            }
        }
        Ok(())
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn atomic_platform_failure(
    phase: &'static str,
    error: impl std::fmt::Display,
) -> KmsRenderPlatformFailure {
    KmsRenderPlatformFailure::terminal(
        "kms-live-atomic-presentation-failed",
        format!("atomic {phase} failed: {error}"),
    )
}

#[cfg(all(feature = "kms-live", not(test)))]
fn production_presenter_for_pool(
    pool: &ScanoutPool,
    cancellation: Arc<AtomicCancellation>,
    event_router: Arc<ProductionAtomicEventRouter>,
    generation: u64,
    admission_deadline: Instant,
) -> Result<
    AtomicPresenter<ProductionAtomicIo>,
    super::atomic_presentation::AtomicPresenterSetupError,
> {
    let presenter_fd = pool.duplicate_drm_fd().map_err(|error| {
        super::atomic_presentation::AtomicPresenterSetupError::external(
            "kms-live-atomic-presenter-fd-duplication-failed",
            error.to_string(),
        )
    })?;
    AtomicPresenter::production(
        presenter_fd,
        pool,
        cancellation,
        event_router,
        generation,
        admission_deadline,
    )
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveRenderPlatformOwnership {
    fn retire_submitted_work(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        self.0.retire_submitted_work()
    }

    fn destroy_live_targets_after_quiescence(
        &mut self,
        key: Option<&OutputKey>,
    ) -> Result<(), KmsRenderPlatformFailure> {
        self.0.destroy_targets(key, false)
    }

    fn source(
        &mut self,
        output: &super::kms::SelectedOutput,
    ) -> Result<RenderSource<KmsRenderPlaceholder>, KmsRenderPlatformFailure> {
        self.0.source(output)
    }

    fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        let released = self.0.destroy_targets(None, true);
        drop(self.0.event_router.take());
        drop(self.0.lease.take());
        released
    }

    fn resume(&mut self, generation: u64) -> Result<(), KmsRenderPlatformFailure> {
        let lease = &mut self.0.lease;
        let resume_leases = &self.0.resume_leases;
        let target_generation = &mut self.0.target_generation;
        if lease.is_some() {
            return Err(KmsRenderPlatformFailure::new(
                "kms-live-resume-while-active",
                "resume cannot replace an active master lease",
            ));
        }
        let output_generation = generation.checked_add(1).ok_or_else(|| {
            KmsRenderPlatformFailure::terminal(
                "kms-live-generation-exhausted",
                "resume generation cannot advance to an output generation",
            )
        })?;
        let staged = resume_leases
            .lock()
            .map_err(|_| {
                KmsRenderPlatformFailure::new(
                    "kms-live-resume-slot-poisoned",
                    "live resume-lease slot was poisoned",
                )
            })?
            .take(generation)
            .map_err(|code| {
                KmsRenderPlatformFailure::new(
                    code,
                    format!("the resume-lease slot cannot supply generation {generation}"),
                )
            })?;
        *lease = Some(staged.lease);
        *target_generation = output_generation;
        self.0.resume_presentation = Some(staged.presentation);
        Ok(())
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl Drop for LiveRenderPlatform {
    fn drop(&mut self) {
        // Belt-and-braces for every caught and uncaught construction exit. The
        // guarded worker reaches Drop only after RenderWorldDropped.
        drop_live_ownership_fail_closed(
            &mut self.ownership,
            LiveRenderPlatformOwnership::retire_submitted_work,
            |ownership| ownership.destroy_live_targets_after_quiescence(None),
            |failure| {
                tracing::error!(code = failure.code, detail = %failure.detail, "live KMS fallback teardown failed");
            },
        );
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl KmsRenderPlatform for LiveRenderPlatform {
    type Placeholder = KmsRenderPlaceholder;

    fn retire_submitted_work(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        self.ownership
            .as_mut()
            .expect("live platform ownership exists while the worker is active")
            .retire_submitted_work()
    }

    fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        self.ownership
            .as_mut()
            .expect("live platform ownership exists while suspending")
            .suspend()
    }

    fn resume(&mut self, generation: u64) -> Result<(), KmsRenderPlatformFailure> {
        self.ownership
            .as_mut()
            .expect("live platform ownership exists while resuming")
            .resume(generation)
    }

    fn add_output(
        &mut self,
        output: &super::kms::SelectedOutput,
    ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
        self.ownership
            .as_mut()
            .expect("live platform ownership exists while adding an output")
            .source(output)
    }

    fn change_output(
        &mut self,
        output: &super::kms::SelectedOutput,
    ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
        let ownership = self
            .ownership
            .as_mut()
            .expect("live platform ownership exists while changing an output");
        ownership.destroy_live_targets_after_quiescence(Some(&output.key))?;
        ownership.source(output)
    }

    fn remove_output(&mut self, key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
        self.ownership
            .as_mut()
            .expect("live platform ownership exists while removing an output")
            .destroy_live_targets_after_quiescence(Some(key))
    }

    fn teardown(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        let ownership = self
            .ownership
            .as_mut()
            .expect("live platform ownership exists during explicit teardown");
        ownership.retire_submitted_work()?;
        ownership.destroy_live_targets_after_quiescence(None)
    }
}

impl super::worker::PlaceholderExtent for KmsRenderPlaceholder {
    fn extent(&self) -> (u32, u32) {
        self.view
            .as_ref()
            .map(manual_texture_view_extent)
            .unwrap_or(self.extent)
    }
}

fn manual_texture_view_extent(view: &ManualTextureView) -> (u32, u32) {
    (view.size.x, view.size.y)
}

struct OfflineRenderPlatform;

impl KmsRenderPlatform for OfflineRenderPlatform {
    type Placeholder = KmsRenderPlaceholder;

    fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        Ok(())
    }

    fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
        Ok(())
    }

    fn add_output(
        &mut self,
        _output: &super::kms::SelectedOutput,
    ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
        Err(offline_platform_failure())
    }

    fn change_output(
        &mut self,
        _output: &super::kms::SelectedOutput,
    ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
        Err(offline_platform_failure())
    }

    fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
        Err(offline_platform_failure())
    }
}

fn offline_platform_failure() -> KmsRenderPlatformFailure {
    KmsRenderPlatformFailure::new(
        "kms-live-adapter-unavailable",
        "D-2a has no authority-changing render platform",
    )
}

#[derive(Resource)]
struct KmsRenderWorkerResource(Mutex<Option<KmsRenderWorker<KmsRenderPlaceholder>>>);

impl Drop for KmsRenderWorkerResource {
    /// This Bevy resource owns only the offline platform. Live DRM platforms
    /// use `spawn_guarded` and acknowledge the separately owned render world
    /// before the worker can destroy platform resources.
    fn drop(&mut self) {
        let worker = match self.0.get_mut() {
            Ok(worker) => worker,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(worker) = worker.take() else {
            return;
        };
        // Thirty seconds is a diagnostic grace period for slow driver teardown;
        // finish still joins an overdue thread rather than detaching it.
        match worker.finish(Duration::from_secs(30)) {
            KmsRenderJoinOutcome::Panicked => {
                tracing::error!("cosmix-kms-render panicked during shutdown");
            }
            KmsRenderJoinOutcome::TimedOut => {
                tracing::error!("cosmix-kms-render did not stop before the shutdown deadline");
            }
            KmsRenderJoinOutcome::Exited(_) => {}
        }
    }
}

struct KmsRenderAppControl {
    lifecycle: Arc<KmsRenderLifecycle>,
    worker_stop: Option<KmsRenderWorkerStop>,
    frame_events: Option<Sender<KmsRenderFrameEvent>>,
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    destructive_quiescence: Option<DestructiveQuiescenceLatch>,
}

fn configure_render_app(
    render_app: &mut SubApp,
    render_commands: Receiver<RenderWorldCommand>,
    releases: KmsRenderInputSender<KmsRenderRelease>,
    quiescences: KmsRenderInputSender<KmsRenderQuiescence>,
    control: KmsRenderAppControl,
) {
    let KmsRenderAppControl {
        lifecycle,
        worker_stop,
        frame_events,
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        destructive_quiescence,
    } = control;
    let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
    targets.lifecycle = lifecycle;
    targets.worker_stop = worker_stop;
    targets.frame_events = frame_events;
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    {
        targets.destructive_quiescence = destructive_quiescence;
    }
    render_app
        .insert_resource(targets)
        .insert_resource(KmsRenderCommands {
            commands: Mutex::new(render_commands),
            releases,
            quiescences,
        })
        .add_systems(
            Render,
            (
                apply_render_world_commands,
                refresh_output_readiness,
                acquire_output_frames,
            )
                .chain()
                .before(prepare_view_attachments),
        )
        .add_systems(
            Render,
            clear_unwritten_output_frames
                .after(render_system)
                .before(present_output_frames),
        )
        .add_systems(
            Render,
            present_output_frames.after(clear_unwritten_output_frames),
        )
        .add_systems(
            Render,
            complete_render_quiescence
                .in_set(RenderSystems::PostCleanup)
                .after(present_output_frames),
        );
}

fn refresh_output_readiness(
    mut targets: bevy::prelude::ResMut<KmsRenderTargets>,
    views: bevy::prelude::Query<(&ExtractedCamera, &ViewTarget)>,
) {
    for source in targets.sources.values_mut() {
        source.current_ready_generation = views
            .iter()
            .any(|(camera, _)| {
                camera.target == Some(NormalizedRenderTarget::TextureView(source.handle))
            })
            .then_some(source.generation);
    }
}

fn acquire_output_frames(
    mut targets: bevy::prelude::ResMut<KmsRenderTargets>,
    mut views: bevy::prelude::ResMut<ManualTextureViews>,
) {
    match targets.lifecycle.state() {
        KmsRenderLifecycleState::Active => {}
        KmsRenderLifecycleState::Terminating | KmsRenderLifecycleState::Terminated => {
            drain_render_resources(RenderDrainScope::All, &mut targets, &mut views);
            return;
        }
        KmsRenderLifecycleState::Quiescing
        | KmsRenderLifecycleState::Suspended
        | KmsRenderLifecycleState::Resuming => return,
    }
    let mut results = Vec::new();
    let mut terminal_failure = None;
    let frame_events = targets.frame_events.clone();
    for (key, source) in &mut targets.sources {
        // A matching active camera and ViewTarget must survive one complete
        // render update before the source may take ownership of a FIFO image.
        // Once acquired, retain that sole image until a later update proves it
        // was written; wgpu-hal's native Vulkan discard path cannot return an
        // unpresented image to the swapchain.
        if source.ready_generation != Some(source.generation)
            || source.current_ready_generation != Some(source.generation)
            || source.pending_present.is_some()
        {
            continue;
        }
        let result = (source.acquire)();
        match result {
            Ok(frame) => {
                let actual = manual_texture_view_extent(&frame.view);
                if actual != source.extent {
                    terminal_failure = Some(KmsRenderWorkerFailure {
                        operation: KmsRenderOperation::Worker,
                        generation: source.generation,
                        key: Some(key.clone()),
                        failure: KmsRenderPlatformFailure::new(
                            "kms-frame-view-size-mismatch",
                            format!(
                                "expected {}x{}, got {}x{}",
                                source.extent.0, source.extent.1, actual.0, actual.1
                            ),
                        ),
                    });
                    break;
                }
                results.push((key.clone(), source.handle, frame.view, frame.present));
            }
            Err(error) => {
                terminal_failure = Some(KmsRenderWorkerFailure {
                    operation: KmsRenderOperation::Worker,
                    generation: source.generation,
                    key: Some(key.clone()),
                    failure: error,
                });
                break;
            }
        }
    }

    if let Some(failure) = terminal_failure {
        if let Some(worker_stop) = &targets.worker_stop {
            worker_stop.begin_render_path_failure(failure.clone());
            worker_stop.wake();
        }
        if let Some(frame_events) = &frame_events {
            let _ = frame_events.send(KmsRenderFrameEvent::TerminalFailure(failure));
        }
        drain_render_resources(RenderDrainScope::All, &mut targets, &mut views);
        return;
    }

    for (key, handle, view, present) in results {
        let pending_present = &mut targets
            .sources
            .get_mut(&key)
            .expect("acquired source remains installed through this update")
            .pending_present;
        views.insert(handle, view);
        *pending_present = Some(present);
    }
}

#[derive(Component)]
struct KmsOutputCamera;

#[derive(Resource, Default)]
struct KmsMainWorldOutputs(BTreeMap<OutputKey, MainWorldOutput>);

#[derive(Clone, Copy)]
struct MainWorldOutput {
    entity: Entity,
    handle: ManualTextureViewHandle,
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    generation: u64,
}

#[derive(Resource)]
pub(crate) struct KmsRegistrarInbox {
    receiver: Mutex<Receiver<KmsRenderWorkerEvent<KmsRenderPlaceholder>>>,
    replies: Sender<KmsRenderReply>,
    registrar: RenderSourceRegistrar<KmsRenderPlaceholder>,
    render: Sender<RenderWorldCommand>,
    releases: KmsRenderInputSender<KmsRenderRelease>,
    registrations: KmsRenderInputSender<KmsRenderRegistration>,
    worker_stop: KmsRenderWorkerStop,
    terminal: Option<KmsRegistrarChannelError>,
}

#[derive(Resource)]
pub(crate) struct KmsRegistrarReplies(Mutex<Receiver<KmsRenderReply>>);

impl KmsRegistrarReplies {
    pub(crate) fn drain(&self) -> Result<Vec<KmsRenderReply>, KmsRegistrarChannelError> {
        let receiver = self
            .0
            .lock()
            .map_err(|_| KmsRegistrarChannelError::ReplyReceiverPoisoned)?;
        let mut replies = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(reply) => replies.push(reply),
                Err(TryRecvError::Empty) => return Ok(replies),
                Err(TryRecvError::Disconnected) => {
                    return Err(KmsRegistrarChannelError::ReplyChannelDisconnected);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KmsRegistrarChannelError {
    EventReceiverPoisoned,
    EventChannelDisconnected,
    ReplyReceiverPoisoned,
    ReplyChannelDisconnected,
    RegistrationChannelDisconnected,
    RenderChannelDisconnected,
    WorkerPoisoned,
    WorkerUnavailable,
    WorkerStopped,
    WorkerCommandDisconnected,
}

pub(crate) fn drain_registrar_replies(
    world: &World,
) -> Result<Vec<KmsRenderReply>, KmsRegistrarChannelError> {
    world.resource::<KmsRegistrarReplies>().drain()
}

pub(crate) fn send_render_commands(
    world: &World,
    commands: Vec<super::kms::KmsRenderCommand>,
) -> Result<(), KmsRegistrarChannelError> {
    let worker = world
        .resource::<KmsRenderWorkerResource>()
        .0
        .lock()
        .map_err(|_| KmsRegistrarChannelError::WorkerPoisoned)?;
    let worker = worker
        .as_ref()
        .ok_or(KmsRegistrarChannelError::WorkerUnavailable)?;
    for command in commands {
        worker.send(command).map_err(|error| match error {
            super::worker::KmsRenderSendError::WorkerStopped => {
                KmsRegistrarChannelError::WorkerStopped
            }
            super::worker::KmsRenderSendError::CommandChannelDisconnected => {
                KmsRegistrarChannelError::WorkerCommandDisconnected
            }
        })?;
    }
    Ok(())
}

enum RenderWorldCommand {
    Install {
        generation: u64,
        key: OutputKey,
        handle: ManualTextureViewHandle,
        extent: (u32, u32),
        acquire: Box<
            dyn FnMut() -> Result<AcquiredOutputFrame, KmsRenderPlatformFailure>
                + Send
                + Sync
                + 'static,
        >,
    },
    Deactivate {
        operation: KmsRenderOperation,
        generation: u64,
        key: OutputKey,
    },
    Remove {
        generation: u64,
        key: OutputKey,
    },
    Clear {
        generation: u64,
    },
    Terminate,
}

#[derive(Resource)]
struct KmsRenderCommands {
    commands: Mutex<Receiver<RenderWorldCommand>>,
    releases: KmsRenderInputSender<KmsRenderRelease>,
    quiescences: KmsRenderInputSender<KmsRenderQuiescence>,
}

impl Drop for KmsRenderCommands {
    fn drop(&mut self) {
        let command_receiver = match self.commands.get_mut() {
            Ok(receiver) => receiver,
            Err(poisoned) => poisoned.into_inner(),
        };
        while let Ok(command) = command_receiver.try_recv() {
            if let Some(release) =
                render_world_command_release(&command, KmsRenderReleaseOutcome::Aborted)
            {
                // Teardown may race worker teardown; a disconnected channel is already terminal.
                let _ = self.releases.send(release);
            }
            if let Some(quiescence) =
                render_world_command_quiescence(&command, KmsRenderQuiescenceOutcome::Aborted)
            {
                let _ = self.quiescences.send(quiescence);
            }
        }
    }
}

fn apply_registrar_events(world: &mut World) {
    loop {
        let received = {
            let inbox = world.resource::<KmsRegistrarInbox>();
            if inbox.terminal == Some(KmsRegistrarChannelError::EventChannelDisconnected) {
                return;
            }
            match inbox.receiver.lock() {
                Ok(receiver) => Ok(receiver.try_recv()),
                Err(_) => Err(KmsRegistrarChannelError::EventReceiverPoisoned),
            }
        };
        let event = match received {
            Ok(Ok(event)) => event,
            Ok(Err(TryRecvError::Empty)) => return,
            Ok(Err(TryRecvError::Disconnected)) => {
                let already_terminal = world
                    .resource::<KmsRegistrarInbox>()
                    .registrar
                    .is_terminal();
                let failure = terminate_registrar(
                    world,
                    KmsRegistrarChannelError::EventChannelDisconnected,
                    KmsRenderOperation::Worker,
                    0,
                    None,
                );
                if !already_terminal
                    && world
                        .resource::<KmsRegistrarInbox>()
                        .replies
                        .send(worker_failure_reply(&failure))
                        .is_err()
                {
                    tracing::error!("KMS registrar reply receiver disconnected");
                }
                tracing::error!(
                    reason = ?KmsRegistrarChannelError::EventChannelDisconnected,
                    "KMS render-worker event channel disconnected"
                );
                return;
            }
            Err(error) => {
                terminate_registrar(world, error, KmsRenderOperation::Worker, 0, None);
                tracing::error!(?error, "KMS registrar event receiver was poisoned");
                return;
            }
        };
        if let KmsRenderWorkerEvent::WorkerStopped(exit) = &event {
            tracing::error!(?exit, "KMS render worker stopped unexpectedly");
        }
        let update = {
            let mut inbox = world.resource_mut::<KmsRegistrarInbox>();
            inbox.registrar.apply(event)
        };
        let mut update = match update {
            Ok(update) => update,
            Err(error) => {
                report_registrar_error(&error);
                let registration = error.rejected_registration();
                let operation = world
                    .resource::<KmsRegistrarInbox>()
                    .registrar
                    .expected_operation(registration.generation, &registration.key)
                    .unwrap_or(KmsRenderOperation::Worker);
                if send_registration(world, registration.clone()).is_err() {
                    terminate_registrar(
                        world,
                        KmsRegistrarChannelError::RegistrationChannelDisconnected,
                        operation,
                        registration.generation,
                        Some(registration.key),
                    );
                    tracing::error!("KMS worker registration receiver disconnected");
                    return;
                }
                let failure_reply = error.failure_reply();
                if world
                    .resource::<KmsRegistrarInbox>()
                    .replies
                    .send(failure_reply)
                    .is_err()
                {
                    terminate_registrar(
                        world,
                        KmsRegistrarChannelError::ReplyChannelDisconnected,
                        operation,
                        registration.generation,
                        Some(registration.key),
                    );
                    tracing::error!("KMS registrar reply receiver disconnected");
                    return;
                }
                continue;
            }
        };
        let update_identity = update.identity.as_ref().map(|identity| {
            (
                identity.operation,
                identity.generation,
                identity.key.clone(),
            )
        });
        for effect in update.effects.drain(..) {
            let (operation, generation, key) = registrar_effect_identity(&effect);
            let release = registrar_effect_release(&effect, KmsRenderReleaseOutcome::Aborted);
            if apply_main_world_effect(world, effect).is_err() {
                terminate_registrar(
                    world,
                    KmsRegistrarChannelError::RenderChannelDisconnected,
                    operation,
                    generation,
                    key.clone(),
                );
                if let Some(mut registration) = update.registration.take() {
                    registration.disposition = KmsRenderRegistrationDisposition::Rejected;
                    let _ = send_registration(world, registration);
                }
                if let Some(release) = release
                    && world
                        .resource::<KmsRegistrarInbox>()
                        .releases
                        .send(release)
                        .is_err()
                {
                    tracing::error!("KMS worker release receiver disconnected");
                }
                tracing::error!(
                    reason = ?KmsRegistrarChannelError::RenderChannelDisconnected,
                    "KMS render-world command receiver disconnected"
                );
                return;
            }
        }
        if let Some(registration) = update.registration
            && send_registration(world, registration.clone()).is_err()
        {
            let (operation, generation, key) = update_identity.unwrap_or((
                KmsRenderOperation::Worker,
                registration.generation,
                Some(registration.key.clone()),
            ));
            let _rollback_handle = world
                .resource_mut::<KmsRegistrarInbox>()
                .registrar
                .rollback_registration(&registration);
            terminate_registrar(
                world,
                KmsRegistrarChannelError::RegistrationChannelDisconnected,
                operation,
                generation,
                key,
            );
            tracing::error!("KMS worker registration receiver disconnected");
            return;
        }
        if let Some(reply) = update.reply {
            let (operation, generation, key) = update_identity
                .expect("every registrar reply carries its authoritative event identity");
            let sent = world.resource::<KmsRegistrarInbox>().replies.send(reply);
            if sent.is_err() {
                terminate_registrar(
                    world,
                    KmsRegistrarChannelError::ReplyChannelDisconnected,
                    operation,
                    generation,
                    key,
                );
                tracing::error!("KMS registrar reply receiver disconnected");
                return;
            }
        }
    }
}

fn terminate_registrar(
    world: &mut World,
    error: KmsRegistrarChannelError,
    operation: KmsRenderOperation,
    generation: u64,
    key: Option<OutputKey>,
) -> KmsRenderWorkerFailure {
    let failure = KmsRenderWorkerFailure {
        operation,
        generation,
        key,
        failure: registrar_channel_failure(error),
    };
    let terminal_effect = {
        let mut inbox = world.resource_mut::<KmsRegistrarInbox>();
        let terminal_effect = inbox.registrar.transition_terminal();
        inbox.terminal = Some(error);
        inbox.worker_stop.begin_render_path_failure(failure.clone());
        inbox.worker_stop.wake();
        terminal_effect
    };
    if let Some(effect) = terminal_effect {
        apply_main_world_effect(world, effect)
            .expect("terminal cleanup handles a disconnected render command receiver");
    }
    failure
}

fn registrar_channel_failure(error: KmsRegistrarChannelError) -> KmsRenderPlatformFailure {
    let (code, detail) = match error {
        KmsRegistrarChannelError::EventReceiverPoisoned => (
            "render-worker-event-receiver-poisoned",
            "KMS render-worker event receiver was poisoned",
        ),
        KmsRegistrarChannelError::EventChannelDisconnected => (
            "render-worker-event-channel-disconnected",
            "KMS render-worker event channel disconnected",
        ),
        KmsRegistrarChannelError::ReplyReceiverPoisoned => (
            "registrar-reply-receiver-poisoned",
            "KMS registrar reply receiver was poisoned",
        ),
        KmsRegistrarChannelError::ReplyChannelDisconnected => (
            "registrar-reply-channel-disconnected",
            "KMS registrar reply channel disconnected",
        ),
        KmsRegistrarChannelError::RegistrationChannelDisconnected => (
            "render-worker-registration-channel-disconnected",
            "KMS render-worker registration channel disconnected",
        ),
        KmsRegistrarChannelError::RenderChannelDisconnected => (
            "render-world-command-disconnected",
            "KMS render-world command receiver disconnected",
        ),
        KmsRegistrarChannelError::WorkerPoisoned => (
            "render-worker-poisoned",
            "KMS render-worker mutex was poisoned",
        ),
        KmsRegistrarChannelError::WorkerUnavailable => (
            "render-worker-unavailable",
            "KMS render worker was unavailable",
        ),
        KmsRegistrarChannelError::WorkerStopped => {
            ("render-worker-stopped", "KMS render worker had stopped")
        }
        KmsRegistrarChannelError::WorkerCommandDisconnected => (
            "render-worker-command-disconnected",
            "KMS render-worker command channel disconnected",
        ),
    };
    KmsRenderPlatformFailure::new(code, detail)
}

fn worker_failure_reply(failure: &KmsRenderWorkerFailure) -> KmsRenderReply {
    KmsRenderReply::WorkerFailed {
        operation: failure.operation,
        generation: failure.generation,
        key: failure.key.clone(),
        code: failure.failure.code,
        reason: failure.failure.detail.clone(),
    }
}

fn registrar_effect_release(
    effect: &RegistrarEffect<KmsRenderPlaceholder>,
    outcome: KmsRenderReleaseOutcome,
) -> Option<KmsRenderRelease> {
    match effect {
        RegistrarEffect::Deactivate {
            operation,
            generation,
            key,
            ..
        } => Some(KmsRenderRelease {
            operation: *operation,
            generation: *generation,
            key: Some(key.clone()),
            outcome,
        }),
        RegistrarEffect::Clear { generation } => Some(KmsRenderRelease {
            operation: KmsRenderOperation::Suspend,
            generation: *generation,
            key: None,
            outcome,
        }),
        RegistrarEffect::Install { .. }
        | RegistrarEffect::Remove { .. }
        | RegistrarEffect::Terminate => None,
    }
}

fn render_world_command_release(
    command: &RenderWorldCommand,
    outcome: KmsRenderReleaseOutcome,
) -> Option<KmsRenderRelease> {
    match command {
        RenderWorldCommand::Deactivate {
            operation,
            generation,
            key,
        } => Some(KmsRenderRelease {
            operation: *operation,
            generation: *generation,
            key: Some(key.clone()),
            outcome,
        }),
        RenderWorldCommand::Clear { generation } => Some(KmsRenderRelease {
            operation: KmsRenderOperation::Suspend,
            generation: *generation,
            key: None,
            outcome,
        }),
        RenderWorldCommand::Install { .. }
        | RenderWorldCommand::Remove { .. }
        | RenderWorldCommand::Terminate => None,
    }
}

fn render_world_command_quiescence(
    command: &RenderWorldCommand,
    outcome: KmsRenderQuiescenceOutcome,
) -> Option<KmsRenderQuiescence> {
    render_world_command_release(command, KmsRenderReleaseOutcome::Granted).map(|release| {
        KmsRenderQuiescence {
            operation: release.operation,
            generation: release.generation,
            key: release.key,
            outcome,
        }
    })
}

fn registrar_effect_identity(
    effect: &RegistrarEffect<KmsRenderPlaceholder>,
) -> (KmsRenderOperation, u64, Option<OutputKey>) {
    match effect {
        RegistrarEffect::Install {
            operation,
            generation,
            key,
            ..
        }
        | RegistrarEffect::Deactivate {
            operation,
            generation,
            key,
            ..
        }
        | RegistrarEffect::Remove {
            operation,
            generation,
            key,
            ..
        } => (*operation, *generation, Some(key.clone())),
        RegistrarEffect::Clear { generation } => (KmsRenderOperation::Suspend, *generation, None),
        RegistrarEffect::Terminate => (KmsRenderOperation::Worker, 0, None),
    }
}

fn send_registration(
    world: &World,
    registration: KmsRenderRegistration,
) -> Result<(), KmsRegistrarChannelError> {
    world
        .resource::<KmsRegistrarInbox>()
        .registrations
        .send(registration)
        .map_err(|_| KmsRegistrarChannelError::RegistrationChannelDisconnected)
}

fn report_registrar_error(error: &RenderSourceRegistrarError) {
    tracing::error!(?error, "KMS render-source registration refused");
}

/// `Deactivate` grants release after the render source is detached, not after every cloned
/// texture view is dropped. Main-sub-app-before-`RenderApp` teardown can likewise leave views
/// alive after platform destruction; the live adapter must resolve both before holding DRM state.
fn apply_main_world_effect(
    world: &mut World,
    effect: RegistrarEffect<KmsRenderPlaceholder>,
) -> Result<(), KmsRegistrarChannelError> {
    match effect {
        RegistrarEffect::Install {
            operation: _,
            generation,
            key,
            handle,
            placeholder,
            acquire,
        } => {
            let extent = super::worker::PlaceholderExtent::extent(&placeholder);
            let logical_extent = placeholder.logical_extent;
            world
                .resource::<KmsRegistrarInbox>()
                .render
                .send(RenderWorldCommand::Install {
                    generation,
                    key: key.clone(),
                    handle,
                    extent,
                    acquire,
                })
                .map_err(|_| KmsRegistrarChannelError::RenderChannelDisconnected)?;
            let Some(placeholder) = placeholder.view else {
                #[cfg(test)]
                {
                    return Ok(());
                }
                #[cfg(not(test))]
                unreachable!("only manual KMS placeholders exist in production");
            };
            world
                .resource_mut::<ManualTextureViews>()
                .insert(handle, placeholder);
            let existing = world.resource::<KmsMainWorldOutputs>().0.get(&key).copied();
            // Bevy 0.19 reports scale 1.0 for every TextureView target and has
            // no supported camera-level override. Decoration Text2d therefore
            // rasterises at RendererOutputScale120 and inversely scales its
            // Transform, keeping logical layout while producing physical-size
            // KMS glyph atlas entries.
            let entity = if let Some(existing) = existing {
                let mut entity = world.entity_mut(existing.entity);
                entity.insert((
                    Camera {
                        is_active: true,
                        ..Default::default()
                    },
                    logical_output_projection(logical_extent),
                    RenderTarget::TextureView(handle),
                    KmsOutputCamera,
                    Msaa::Off,
                ));
                #[cfg(feature = "frame-capture")]
                entity.insert(crate::frame_capture::FrameCaptureTarget::new(
                    &key.connector_name,
                ));
                existing.entity
            } else {
                world
                    .spawn((
                        Name::new(format!(
                            "KMS output camera {}:{}",
                            key.device, key.connector_name
                        )),
                        Camera2d,
                        Camera::default(),
                        logical_output_projection(logical_extent),
                        RenderTarget::TextureView(handle),
                        KmsOutputCamera,
                        Msaa::Off,
                        #[cfg(feature = "frame-capture")]
                        crate::frame_capture::FrameCaptureTarget::new(&key.connector_name),
                    ))
                    .id()
            };
            world.resource_mut::<KmsMainWorldOutputs>().0.insert(
                key.clone(),
                MainWorldOutput {
                    entity,
                    handle,
                    #[cfg(any(all(feature = "kms-live", not(test)), test))]
                    generation,
                },
            );
            Ok(())
        }
        RegistrarEffect::Deactivate {
            operation,
            generation,
            key,
            handle,
        } => {
            world
                .resource::<KmsRegistrarInbox>()
                .render
                .send(RenderWorldCommand::Deactivate {
                    operation,
                    generation,
                    key: key.clone(),
                })
                .map_err(|_| KmsRegistrarChannelError::RenderChannelDisconnected)?;
            if let Some(output) = world.resource::<KmsMainWorldOutputs>().0.get(&key).copied()
                && let Some(mut camera) = world.get_mut::<Camera>(output.entity)
            {
                camera.is_active = false;
            }
            if let Some(handle) = handle {
                debug_assert_eq!(
                    world
                        .resource::<ManualTextureViews>()
                        .get(&handle)
                        .map(|_| handle),
                    Some(handle)
                );
            }
            Ok(())
        }
        RegistrarEffect::Remove {
            operation: _,
            generation,
            key,
            handle,
        } => {
            world
                .resource::<KmsRegistrarInbox>()
                .render
                .send(RenderWorldCommand::Remove {
                    generation,
                    key: key.clone(),
                })
                .map_err(|_| KmsRegistrarChannelError::RenderChannelDisconnected)?;
            remove_main_world_source(world, &key, handle);
            Ok(())
        }
        RegistrarEffect::Clear { generation } => {
            world
                .resource::<KmsRegistrarInbox>()
                .render
                .send(RenderWorldCommand::Clear { generation })
                .map_err(|_| KmsRegistrarChannelError::RenderChannelDisconnected)?;
            let outputs = std::mem::take(&mut world.resource_mut::<KmsMainWorldOutputs>().0);
            for (_, output) in outputs {
                world
                    .resource_mut::<ManualTextureViews>()
                    .remove(&output.handle);
                if let Ok(entity) = world.get_entity_mut(output.entity) {
                    entity.despawn();
                }
            }
            Ok(())
        }
        RegistrarEffect::Terminate => {
            if world
                .resource::<KmsRegistrarInbox>()
                .render
                .send(RenderWorldCommand::Terminate)
                .is_err()
            {
                // Every producer of `Terminate` latches the worker's shared terminal barrier
                // before this effect. Acquisition and presentation check that barrier, so a
                // disconnected command receiver cannot leave a live platform callback.
                tracing::error!(
                    "KMS render-world command receiver disconnected during terminal cleanup"
                );
            }
            clear_main_world_sources(world);
            Ok(())
        }
    }
}

fn remove_main_world_source(world: &mut World, key: &OutputKey, handle: ManualTextureViewHandle) {
    world.resource_mut::<ManualTextureViews>().remove(&handle);
    if let Some(output) = world.resource_mut::<KmsMainWorldOutputs>().0.remove(key)
        && let Ok(entity) = world.get_entity_mut(output.entity)
    {
        entity.despawn();
    }
}

fn clear_main_world_sources(world: &mut World) {
    let outputs = world
        .resource::<KmsMainWorldOutputs>()
        .0
        .iter()
        .map(|(key, output)| (key.clone(), output.handle))
        .collect::<Vec<_>>();
    for (key, handle) in outputs {
        remove_main_world_source(world, &key, handle);
    }
    world.resource_mut::<ManualTextureViews>().clear();
}

fn apply_render_world_commands(
    commands: bevy::prelude::Res<KmsRenderCommands>,
    mut targets: bevy::prelude::ResMut<KmsRenderTargets>,
    mut views: bevy::prelude::ResMut<ManualTextureViews>,
) {
    let Ok(command_receiver) = commands.commands.lock() else {
        tracing::error!("KMS render command receiver was poisoned");
        return;
    };
    loop {
        match command_receiver.try_recv() {
            Ok(RenderWorldCommand::Install {
                generation,
                key,
                handle,
                extent,
                acquire,
            }) => {
                if targets
                    .sources
                    .get(&key)
                    .is_some_and(|source| source.generation > generation)
                {
                    continue;
                }
                targets.sources.insert(
                    key,
                    OutputFrameSource {
                        generation,
                        handle,
                        extent,
                        acquire,
                        ready_generation: None,
                        current_ready_generation: None,
                        pending_present: None,
                    },
                );
            }
            Ok(RenderWorldCommand::Deactivate {
                operation,
                generation,
                key,
            }) => {
                if commands
                    .releases
                    .send(KmsRenderRelease {
                        operation,
                        generation,
                        key: Some(key.clone()),
                        outcome: KmsRenderReleaseOutcome::Granted,
                    })
                    .is_err()
                {
                    tracing::error!("KMS worker release receiver disconnected");
                    return;
                }
                targets.pending_quiescence.push(PendingRenderQuiescence {
                    drain: PendingRenderDrain::OutputThrough {
                        key: key.clone(),
                        generation,
                    },
                    acknowledgement: KmsRenderQuiescence {
                        operation,
                        generation,
                        key: Some(key),
                        outcome: KmsRenderQuiescenceOutcome::Quiesced,
                    },
                });
            }
            Ok(RenderWorldCommand::Remove { generation, key }) => {
                drain_render_resources(
                    RenderDrainScope::OutputThrough {
                        key: &key,
                        generation,
                    },
                    &mut targets,
                    &mut views,
                );
            }
            Ok(RenderWorldCommand::Clear { generation }) => {
                if commands
                    .releases
                    .send(KmsRenderRelease {
                        operation: KmsRenderOperation::Suspend,
                        generation,
                        key: None,
                        outcome: KmsRenderReleaseOutcome::Granted,
                    })
                    .is_err()
                {
                    tracing::error!("KMS worker release receiver disconnected");
                    return;
                }
                targets.pending_quiescence.push(PendingRenderQuiescence {
                    drain: PendingRenderDrain::AllThrough(generation),
                    acknowledgement: KmsRenderQuiescence {
                        operation: KmsRenderOperation::Suspend,
                        generation,
                        key: None,
                        outcome: KmsRenderQuiescenceOutcome::Quiesced,
                    },
                });
            }
            Ok(RenderWorldCommand::Terminate) => {
                drain_render_resources(RenderDrainScope::All, &mut targets, &mut views);
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

fn present_output_frames(
    mut targets: bevy::prelude::ResMut<KmsRenderTargets>,
    views: bevy::prelude::Query<(&ExtractedCamera, &ViewTarget)>,
) {
    if targets.lifecycle.state() != KmsRenderLifecycleState::Active {
        return;
    }

    let extracted = targets
        .sources
        .iter()
        .map(|(key, source)| {
            let target = views.iter().find_map(|(camera, target)| {
                (camera.target == Some(NormalizedRenderTarget::TextureView(source.handle)))
                    .then_some(target)
            });
            ExtractedOutputView {
                key: key.clone(),
                generation: source.generation,
                handle: source.handle,
                ready: target.is_some(),
                written: target.is_some_and(ViewTarget::needs_present),
            }
        })
        .collect::<Vec<_>>();
    present_selected_output_frames(&mut targets, &extracted);
}

/// Bevy's final output pass is demand-driven: if no render node consumes an
/// empty camera's output attachment, `ViewTarget::needs_present()` remains
/// false even though the camera is configured to clear. KMS cannot abandon
/// that already-acquired FIFO image, so consume the real attachment with the
/// camera's clear operation before consulting the truthful presentation gate.
#[cfg(test)]
#[derive(Resource, Clone)]
struct FallbackClearProbe {
    clears: Arc<AtomicUsize>,
    suppress_upscaling: Arc<AtomicBool>,
}

fn clear_unwritten_output_frames(
    targets: bevy::prelude::Res<KmsRenderTargets>,
    clear_color_global: Option<bevy::prelude::Res<ClearColor>>,
    views: bevy::prelude::Query<(&ExtractedCamera, &ViewTarget)>,
    render_device: Option<bevy::prelude::Res<RenderDevice>>,
    render_queue: Option<bevy::prelude::Res<RenderQueue>>,
    #[cfg(test)] fallback_probe: Option<bevy::prelude::Res<FallbackClearProbe>>,
) {
    if targets.lifecycle.state() != KmsRenderLifecycleState::Active {
        return;
    }
    let (Some(clear_color_global), Some(render_device), Some(render_queue)) =
        (clear_color_global, render_device, render_queue)
    else {
        return;
    };

    let mut encoder = None;
    for source in targets.sources.values() {
        let Some((camera, target)) = views.iter().find(|(camera, _)| {
            camera.target == Some(NormalizedRenderTarget::TextureView(source.handle))
        }) else {
            continue;
        };
        if target.needs_present() {
            continue;
        }

        let clear_color = match camera.output_mode {
            CameraOutputMode::Write { clear_color, .. } => match clear_color {
                ClearColorConfig::Default => clear_color_global.0.to_linear(),
                ClearColorConfig::Custom(color) => color.to_linear(),
                ClearColorConfig::None => continue,
            },
            CameraOutputMode::Skip => continue,
        };
        let Some(attachment) = target.out_texture_color_attachment(Some(clear_color)) else {
            continue;
        };
        let encoder = encoder.get_or_insert_with(|| {
            render_device.create_command_encoder(&CommandEncoderDescriptor {
                label: Some("KMS unwritten-output clear encoder"),
            })
        });
        let _clear_pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("KMS unwritten-output clear pass"),
            color_attachments: &[Some(attachment)],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        #[cfg(test)]
        if let Some(probe) = &fallback_probe {
            probe.clears.fetch_add(1, Ordering::SeqCst);
        }
    }

    if let Some(encoder) = encoder {
        render_queue.submit([encoder.finish()]);
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DmabufOutputProbeRegion {
    rect: (u32, u32, u32, u32),
    x: u32,
    rows: [u32; 3],
    width: u32,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn project_dmabuf_output_probe_region(
    surface: DmabufOutputProbeSurface,
    canvas: Vec2,
    clip_from_world: bevy::math::Mat4,
    viewport: bevy::math::UVec4,
) -> Option<DmabufOutputProbeRegion> {
    if surface.width <= 0.0 || surface.height <= 0.0 || viewport.z == 0 || viewport.w == 0 {
        return None;
    }
    let world_left = surface.x - canvas.x / 2.0;
    let world_right = world_left + surface.width;
    let world_top = canvas.y / 2.0 - surface.y;
    let world_bottom = world_top - surface.height;
    let project = |x: f32, y: f32| {
        let ndc = clip_from_world.project_point3(bevy::math::Vec3::new(x, y, 0.0));
        let pixel = Vec2::new(
            viewport.x as f32 + (ndc.x + 1.0) * 0.5 * viewport.z as f32,
            viewport.y as f32 + (1.0 - ndc.y) * 0.5 * viewport.w as f32,
        );
        pixel.is_finite().then_some(pixel)
    };
    let projected = [
        project(world_left, world_top)?,
        project(world_right, world_top)?,
        project(world_left, world_bottom)?,
        project(world_right, world_bottom)?,
    ];
    let min_x = projected
        .iter()
        .map(|point| point.x)
        .fold(f32::INFINITY, f32::min);
    let max_x = projected
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = projected
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min);
    let max_y = projected
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max);
    let viewport_right = viewport.x.saturating_add(viewport.z);
    let viewport_bottom = viewport.y.saturating_add(viewport.w);
    let left = (min_x.floor().max(viewport.x as f32) as u32).min(viewport_right);
    let right = (max_x.ceil().max(viewport.x as f32) as u32).min(viewport_right);
    let top = (min_y.floor().max(viewport.y as f32) as u32).min(viewport_bottom);
    let bottom = (max_y.ceil().max(viewport.y as f32) as u32).min(viewport_bottom);
    if right <= left || bottom <= top {
        return None;
    }

    let rect_width = right - left;
    let rect_height = bottom - top;
    let inside_left = left + u32::from(rect_width > 2);
    let inside_right = right - u32::from(rect_width > 2);
    let inside_width = inside_right - inside_left;
    let width = inside_width.min(256);
    let x = inside_left + (inside_width - width) / 2;
    let top_row = top + u32::from(rect_height > 2);
    let bottom_row = bottom - 1 - u32::from(rect_height > 2);
    Some(DmabufOutputProbeRegion {
        rect: (left, top, right, bottom),
        x,
        rows: [top_row, top + rect_height / 2, bottom_row],
        width,
    })
}

#[cfg(all(feature = "kms-live", not(test)))]
#[derive(Debug, Eq, PartialEq)]
struct DmabufOutputProbeSummary {
    nonzero_bytes: [usize; 3],
    checksum: u64,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn dmabuf_output_probe_due(rendered_frames: &mut u64, import_requested: bool) -> bool {
    *rendered_frames = rendered_frames.wrapping_add(1);
    import_requested || rendered_frames.is_multiple_of(60)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn readback_dmabuf_output_probe(
    device: &RenderDevice,
    queue: &RenderQueue,
    texture: &gpu::TextureView,
    region: DmabufOutputProbeRegion,
) -> Result<DmabufOutputProbeSummary, String> {
    const BYTES_PER_PIXEL: u32 = 4;
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let row_bytes = region.width.saturating_mul(BYTES_PER_PIXEL);
    let padded_row_bytes = RenderDevice::align_copy_bytes_per_row(row_bytes as usize);
    let buffer_size = u64::try_from(padded_row_bytes.saturating_mul(region.rows.len()))
        .map_err(|_| "output-probe buffer size is not representable".to_string())?;
    let buffer = device.create_buffer(&gpu::BufferDescriptor {
        label: Some("DMA-BUF physical-output probe readback"),
        size: buffer_size,
        usage: gpu::BufferUsages::COPY_DST | gpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&gpu::CommandEncoderDescriptor {
        label: Some("DMA-BUF physical-output probe copy"),
    });
    for (index, row) in region.rows.into_iter().enumerate() {
        encoder.copy_texture_to_buffer(
            gpu::TexelCopyTextureInfo {
                texture: texture.texture(),
                mip_level: 0,
                origin: gpu::Origin3d {
                    x: region.x,
                    y: row,
                    z: 0,
                },
                aspect: gpu::TextureAspect::All,
            },
            gpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: gpu::TexelCopyBufferLayout {
                    offset: u64::try_from(index.saturating_mul(padded_row_bytes))
                        .map_err(|_| "output-probe row offset is not representable".to_string())?,
                    bytes_per_row: Some(
                        u32::try_from(padded_row_bytes).map_err(|_| {
                            "output-probe row pitch is not representable".to_string()
                        })?,
                    ),
                    rows_per_image: Some(1),
                },
            },
            gpu::Extent3d {
                width: region.width,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
    }
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (mapped_sender, mapped_receiver) = std::sync::mpsc::sync_channel(1);
    slice.map_async(gpu::MapMode::Read, move |result| {
        let _ = mapped_sender.send(result.map_err(|error| error.to_string()));
    });
    device
        .poll(gpu::PollType::wait_indefinitely())
        .map_err(|error| error.to_string())?;
    mapped_receiver
        .recv()
        .map_err(|_| "output-probe mapping callback was dropped".to_string())??;

    let data = slice.get_mapped_range();
    let row_bytes = usize::try_from(row_bytes)
        .map_err(|_| "output-probe row width is not representable".to_string())?;
    let mut summary = DmabufOutputProbeSummary {
        nonzero_bytes: [0; 3],
        checksum: FNV_OFFSET,
    };
    for index in 0..summary.nonzero_bytes.len() {
        let start = index.saturating_mul(padded_row_bytes);
        let row = data
            .get(start..start.saturating_add(row_bytes))
            .ok_or_else(|| {
                "output-probe mapped range is shorter than requested rows".to_string()
            })?;
        summary.nonzero_bytes[index] = row.iter().filter(|byte| **byte != 0).count();
        for byte in row {
            summary.checksum ^= u64::from(*byte);
            summary.checksum = summary.checksum.wrapping_mul(FNV_PRIME);
        }
    }
    drop(data);
    buffer.unmap();
    Ok(summary)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn probe_dmabuf_output(
    imports: Res<ImportedDmabufImages>,
    surfaces: Res<DmabufOutputProbeSurfaces>,
    views: Query<(&ExtractedCamera, &ExtractedView, &ViewTarget)>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    mut rendered_frames: Local<u64>,
) {
    if !views
        .iter()
        .any(|(camera, _, target)| camera.target.is_some() && target.needs_present())
    {
        return;
    }
    if !dmabuf_output_probe_due(&mut rendered_frames, imports.take_output_probe_request()) {
        return;
    }
    if surfaces.surfaces.is_empty() {
        tracing::info!(
            stage = "output",
            "DMA-BUF GPU probe found no visible sprite rect"
        );
        return;
    }

    for (camera, view, target) in &views {
        if camera.target.is_none() || !target.needs_present() {
            continue;
        }
        let Some(output_view) = target.out_texture() else {
            continue;
        };
        let Some(format) = target.out_texture_view_format() else {
            continue;
        };
        if !matches!(
            format,
            gpu::TextureFormat::Rgba8Unorm
                | gpu::TextureFormat::Rgba8UnormSrgb
                | gpu::TextureFormat::Bgra8Unorm
                | gpu::TextureFormat::Bgra8UnormSrgb
                | gpu::TextureFormat::Rgb10a2Unorm
        ) {
            tracing::error!(
                stage = "output",
                ?format,
                "DMA-BUF GPU probe cannot read this output format"
            );
            continue;
        }
        let clip_from_world = view
            .clip_from_world
            .unwrap_or_else(|| view.clip_from_view * view.world_from_view.to_matrix().inverse());
        for surface in &surfaces.surfaces {
            let Some(region) = project_dmabuf_output_probe_region(
                *surface,
                surfaces.canvas,
                clip_from_world,
                view.viewport,
            ) else {
                continue;
            };
            match readback_dmabuf_output_probe(&device, &queue, output_view, region) {
                Ok(summary) => tracing::info!(
                    stage = "output",
                    surface_id = surface.surface_id.0,
                    expected_rect = ?(surface.x, surface.y, surface.width, surface.height),
                    physical_rect = ?region.rect,
                    sample_x = region.x,
                    rows = ?region.rows,
                    width = region.width,
                    ?format,
                    material_alpha_mode = if surface.opaque { "Opaque" } else { "Blend" },
                    material_blend = if surface.opaque { "disabled" } else { "standard-alpha" },
                    nonzero_bytes = ?summary.nonzero_bytes,
                    checksum = format_args!("{:016x}", summary.checksum),
                    "DMA-BUF GPU probe"
                ),
                Err(probe_error) => tracing::error!(
                    stage = "output",
                    surface_id = surface.surface_id.0,
                    physical_rect = ?region.rect,
                    %probe_error,
                    "DMA-BUF GPU probe failed"
                ),
            }
        }
    }
}

fn present_selected_output_frames(
    targets: &mut KmsRenderTargets,
    extracted: &[ExtractedOutputView],
) {
    let presenters = select_written_presenters(&mut targets.sources, extracted);
    let frame_events = targets.frame_events.clone();
    for presenter in presenters {
        let event = match present_output_frame(presenter.present, targets.present_deadline) {
            Ok(PresentOutcome::Displayed) => Some(KmsRenderFrameEvent::FrameSubmitted {
                generation: presenter.generation,
                key: presenter.key,
            }),
            Ok(PresentOutcome::Cancelled) => Some(KmsRenderFrameEvent::PresentationCancelled {
                generation: presenter.generation,
                key: presenter.key,
            }),
            Err(failure) => Some(KmsRenderFrameEvent::TerminalFailure(
                KmsRenderWorkerFailure {
                    operation: KmsRenderOperation::Worker,
                    generation: presenter.generation,
                    key: Some(presenter.key),
                    failure,
                },
            )),
        };
        let terminal = matches!(event, Some(KmsRenderFrameEvent::TerminalFailure(_)));
        let reply_carried_authority_failure = matches!(
            event.as_ref(),
            Some(KmsRenderFrameEvent::TerminalFailure(failure))
                if failure.failure.atomic_commit_authority_errno().is_some()
        );
        if let Some(KmsRenderFrameEvent::TerminalFailure(failure)) = event.as_ref()
            && !reply_carried_authority_failure
            && let Some(worker_stop) = &targets.worker_stop
        {
            worker_stop.begin_render_path_failure(failure.clone());
            worker_stop.wake();
        }
        if let (Some(frame_events), Some(event)) = (&frame_events, event) {
            let _ = frame_events.send(event);
        }
        if terminal {
            break;
        }
    }
}

fn select_written_presenters(
    sources: &mut BTreeMap<OutputKey, OutputFrameSource>,
    views: &[ExtractedOutputView],
) -> Vec<AcquiredOutputPresenter> {
    let mut presenters = Vec::new();
    for (key, source) in sources {
        let Some(view) = views.iter().find(|view| {
            view.ready
                && view.key == *key
                && view.generation == source.generation
                && view.handle == source.handle
        }) else {
            continue;
        };
        source.ready_generation = Some(source.generation);
        if view.written
            && let Some(present) = source.pending_present.take()
        {
            presenters.push(AcquiredOutputPresenter {
                key: key.clone(),
                generation: source.generation,
                present,
            });
        }
    }
    presenters
}

fn complete_render_quiescence(
    commands: bevy::prelude::Res<KmsRenderCommands>,
    mut targets: bevy::prelude::ResMut<KmsRenderTargets>,
    mut views: bevy::prelude::ResMut<ManualTextureViews>,
) {
    let pending = targets.pending_quiescence.drain(..).collect::<Vec<_>>();
    for quiescence in pending {
        match &quiescence.drain {
            PendingRenderDrain::OutputThrough { key, generation } => drain_render_resources(
                RenderDrainScope::OutputThrough {
                    key,
                    generation: *generation,
                },
                &mut targets,
                &mut views,
            ),
            PendingRenderDrain::AllThrough(generation) => drain_render_resources(
                RenderDrainScope::AllThrough(*generation),
                &mut targets,
                &mut views,
            ),
        }
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        {
            let identity = DestructiveQuiescenceIdentity::from(&quiescence.acknowledgement);
            if targets
                .destructive_quiescence
                .as_ref()
                .is_some_and(|latch| latch.publish(identity.clone()).is_err())
            {
                tracing::error!(
                    ?identity,
                    "destructive render quiescence was published while an earlier generation remained latched"
                );
                return;
            }
        }
        if commands
            .quiescences
            .send(quiescence.acknowledgement)
            .is_err()
        {
            tracing::error!("KMS worker quiescence receiver disconnected");
            return;
        }
    }
}

struct PendingRenderQuiescence {
    drain: PendingRenderDrain,
    acknowledgement: KmsRenderQuiescence,
}

enum PendingRenderDrain {
    OutputThrough { key: OutputKey, generation: u64 },
    AllThrough(u64),
}

enum RenderDrainScope<'a> {
    OutputThrough { key: &'a OutputKey, generation: u64 },
    AllThrough(u64),
    All,
}

/// The sole render-world resource drain. Its five callers cover terminal
/// acquire, deactivate, remove, suspend-clear and terminal teardown.
fn drain_render_resources(
    scope: RenderDrainScope<'_>,
    targets: &mut KmsRenderTargets,
    views: &mut ManualTextureViews,
) {
    let removed_handles = targets
        .sources
        .iter()
        .filter_map(|(key, source)| {
            let remove = match &scope {
                RenderDrainScope::OutputThrough {
                    key: target,
                    generation,
                } => key == *target && source.generation <= *generation,
                RenderDrainScope::AllThrough(generation) => source.generation <= *generation,
                RenderDrainScope::All => true,
            };
            remove.then_some((key.clone(), source.handle))
        })
        .collect::<Vec<_>>();
    for (key, handle) in removed_handles {
        targets.sources.remove(&key);
        views.remove(&handle);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        collections::HashMap,
        fs::File,
        future::Future,
        os::fd::AsRawFd,
        os::unix::net::UnixStream,
        pin::pin,
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::{Context, Poll, Waker},
        thread,
        time::{Duration, Instant},
    };

    use bevy::{
        app::{First, SubApp},
        camera::CameraProjection,
        core_pipeline::upscaling::ViewUpscalingPipeline,
        ecs::{
            query::With,
            schedule::{NodeId, ScheduleGraph, graph::Direction},
            system::{IntoSystem, System},
        },
        render::{
            RenderPlugin,
            renderer::{RenderAdapter, RenderAdapterInfo, RenderInstance, WgpuWrapper},
            settings::RenderCreation,
        },
        time::{TimePlugin, TimeUpdateStrategy},
        window::PrimaryWindow,
    };
    use cosmix_wgpu_dmabuf::{DmabufBufferId, DmabufDescriptor, DmabufPlane, DmabufRelease};
    use smithay::backend::allocator::{Fourcc, Modifier};

    use super::*;
    use crate::{
        backend::kms::{
            AtomicOutputSelection, ConnectorMode, KmsRenderCommand, LogicalRect, SelectedOutput,
        },
        backend::worker::KmsRenderWorkerExit,
        protocol::{
            ProtocolEvent, ShmFrame, SurfaceFrame, SurfaceId, SurfaceLayout, SurfaceTransform,
            WaylandRuntime,
        },
    };

    pub(crate) fn live_client_scene_app_for_test(feed: ClientSceneFeed, extent: (u32, u32)) -> App {
        let mut app = App::new();
        app.init_resource::<bevy::asset::Assets<bevy::image::Image>>();
        install_live_scene(&mut app, LiveSceneMode::ClientContent);
        prepare_live_scene_start(
            &mut app,
            LiveSceneMode::ClientContent,
            Some(feed),
            extent,
            crate::backend::kms::OutputScale120::ONE,
        )
        .expect("offline client scene has its feed before the first update");
        app
    }

    #[test]
    fn dmabuf_output_probe_projects_three_rows_inside_the_sprite_rect() {
        let surface = DmabufOutputProbeSurface {
            surface_id: SurfaceId(7),
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 12.0,
            opaque: true,
        };
        let projection =
            bevy::math::Mat4::orthographic_rh(-50.0, 50.0, -40.0, 40.0, -1000.0, 1000.0);

        let region = project_dmabuf_output_probe_region(
            surface,
            Vec2::new(100.0, 80.0),
            projection,
            bevy::math::UVec4::new(0, 0, 200, 160),
        )
        .expect("the sprite intersects the physical target");

        assert_eq!(region.rect, (20, 40, 80, 64));
        assert_eq!(region.x, 21);
        assert_eq!(region.width, 58);
        assert_eq!(region.rows, [41, 52, 62]);

        let mut rendered_frames = 0;
        for frame in 1..=121 {
            let import_requested = frame == 4;
            assert_eq!(
                dmabuf_output_probe_due(&mut rendered_frames, import_requested),
                matches!(frame, 4 | 60 | 120),
                "unexpected output-probe decision on rendered frame {frame}",
            );
        }
    }

    fn poll_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("noop wgpu future unexpectedly remained pending"),
        }
    }

    fn noop_render_plugin() -> (RenderPlugin, wgpu::Device) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::NOOP,
            backend_options: wgpu::BackendOptions {
                noop: wgpu::NoopBackendOptions { enable: true },
                ..Default::default()
            },
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = poll_ready(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("noop adapter exists");
        let adapter_info = adapter.get_info();
        let (device, queue) = poll_ready(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("cosmix-comp frame-driver noop device"),
            ..Default::default()
        }))
        .expect("noop device exists");
        let render_creation = RenderCreation::manual(
            device.clone().into(),
            RenderQueue(Arc::new(WgpuWrapper::new(queue))),
            RenderAdapterInfo(WgpuWrapper::new(adapter_info)),
            RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
            RenderInstance(Arc::new(WgpuWrapper::new(instance))),
        );
        (
            RenderPlugin {
                render_creation,
                synchronous_pipeline_compilation: true,
                ..Default::default()
            },
            device,
        )
    }

    fn frame_driver_view(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        label: &'static str,
    ) -> ManualTextureView {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        ManualTextureView::with_default_format(
            texture
                .create_view(&wgpu::TextureViewDescriptor::default())
                .into(),
            bevy::prelude::UVec2::new(width, height),
        )
    }

    struct FrameDriverPlatform {
        device: wgpu::Device,
        presented: Arc<AtomicUsize>,
        destructive_release: Option<Receiver<()>>,
        terminal_teardown: Option<HeldTerminalTeardown>,
        destroyed: Option<Arc<AtomicBool>>,
        add_failures: Arc<AtomicUsize>,
    }

    struct HeldTerminalTeardown {
        entered: SyncSender<()>,
        release: Receiver<()>,
    }

    impl FrameDriverPlatform {
        fn source(&self, output: &SelectedOutput) -> RenderSource<KmsRenderPlaceholder> {
            let width = output.display.mode.width;
            let height = output.display.mode.height;
            let device = self.device.clone();
            let presented = Arc::clone(&self.presented);
            RenderSource {
                placeholder: KmsRenderPlaceholder {
                    extent: (width, height),
                    logical_extent: selected_logical_extent(output),
                    view: Some(frame_driver_view(
                        &self.device,
                        width,
                        height,
                        "frame-driver placeholder",
                    )),
                },
                acquire: Box::new(move || {
                    let presented = Arc::clone(&presented);
                    Ok(AcquiredOutputFrame {
                        view: frame_driver_view(
                            &device,
                            width,
                            height,
                            "frame-driver acquired output",
                        ),
                        present: Box::new(move || {
                            presented.fetch_add(1, Ordering::SeqCst);
                        }),
                    })
                }),
            }
        }
    }

    impl KmsRenderPlatform for FrameDriverPlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            if let Some(release) = self.destructive_release.take() {
                release.recv_timeout(Duration::from_secs(30)).map_err(|_| {
                    KmsRenderPlatformFailure::terminal(
                        "held-destroy-release-timeout",
                        "test did not release the held destructive worker call",
                    )
                })?;
            }
            if let Some(destroyed) = &self.destroyed {
                destroyed.store(true, Ordering::SeqCst);
            }
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            if self
                .add_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(KmsRenderPlatformFailure::new(
                    "kms-live-test-retryable-add",
                    "injected retryable AddOutput failure",
                ));
            }
            Ok(self.source(output))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(self.source(output))
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn teardown(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            if let Some(held) = self.terminal_teardown.take() {
                if let Some(destroyed) = &self.destroyed {
                    destroyed.store(true, Ordering::SeqCst);
                }
                held.entered.send(()).map_err(|_| {
                    KmsRenderPlatformFailure::terminal(
                        "held-terminal-teardown-observer-lost",
                        "test teardown observer stopped before destruction",
                    )
                })?;
                held.release
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|_| {
                        KmsRenderPlatformFailure::terminal(
                            "held-terminal-teardown-release-timeout",
                            "test did not release the held terminal worker teardown",
                        )
                    })?;
            }
            Ok(())
        }
    }

    type ClientContentFrameDriver = (
        LiveRenderAdapter,
        SyncSender<Vec<ProtocolEvent>>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
    );

    fn client_content_frame_driver() -> ClientContentFrameDriver {
        client_content_frame_driver_inner(false, None, None, None, None, None)
    }

    fn decorated_client_content_frame_driver() -> ClientContentFrameDriver {
        client_content_frame_driver_inner(true, None, None, None, None, None)
    }

    fn client_content_frame_driver_inner(
        server_side_decoration: bool,
        destructive_release: Option<Receiver<()>>,
        destroyed: Option<Arc<AtomicBool>>,
        post_destroy_updates: Option<Arc<AtomicUsize>>,
        add_failures: Option<Arc<AtomicUsize>>,
        terminal_teardown: Option<HeldTerminalTeardown>,
    ) -> ClientContentFrameDriver {
        let (scene_events, feed) = ClientSceneFeed::test_channel();
        let (render_plugin, device) = noop_render_plugin();
        let mut app = App::new();
        if server_side_decoration {
            app.insert_resource(crate::decoration::DecorationStartup::resolve(
                true,
                cosmix_deco::ChromeStyle::Mac,
            ));
        }
        app.add_plugins(configure_live_headless_plugins(DefaultPlugins.build()).set(render_plugin));
        install_live_scene(&mut app, LiveSceneMode::ClientContent);
        let terminal_updates_stopped = Arc::new(AtomicBool::new(false));
        if let (Some(destroyed), Some(post_destroy_updates)) =
            (destroyed.clone(), post_destroy_updates)
        {
            let terminal_updates_stopped = Arc::clone(&terminal_updates_stopped);
            app.add_systems(bevy::app::Update, move || {
                if destroyed.load(Ordering::SeqCst)
                    || terminal_updates_stopped.load(Ordering::Acquire)
                {
                    post_destroy_updates.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        app.sub_app_mut(RenderApp).add_systems(
            Render,
            suppress_frame_driver_upscaling
                .after(RenderSystems::Prepare)
                .before(render_system),
        );
        let fallback_clears = Arc::new(AtomicUsize::new(0));
        let suppress_upscaling = Arc::new(AtomicBool::new(true));
        app.sub_app_mut(RenderApp)
            .insert_resource(FallbackClearProbe {
                clears: Arc::clone(&fallback_clears),
                suppress_upscaling: Arc::clone(&suppress_upscaling),
            });
        prepare_live_scene_start(
            &mut app,
            LiveSceneMode::ClientContent,
            Some(feed),
            (320, 240),
            crate::backend::kms::OutputScale120::ONE,
        )
        .expect("frame-driver client scene starts");
        app.finish();
        app.cleanup();

        let presented = Arc::new(AtomicUsize::new(0));
        let output = blocked_output();
        let LiveKmsRenderInstallation {
            worker,
            render_world_dropped,
            frame_events,
            destructive_quiescence,
        } = install_live_kms_render_target(
            &mut app,
            FrameDriverPlatform {
                device,
                presented: Arc::clone(&presented),
                destructive_release,
                terminal_teardown,
                destroyed,
                add_failures: add_failures.unwrap_or_default(),
            },
        )
        .expect("frame-driver render target installs");
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 1,
                output: output.clone(),
            })
            .expect("frame-driver output registration is queued");
        let mut adapter = LiveRenderAdapter {
            app: Some(app),
            render_world_dropped: Some(render_world_dropped),
            worker: Some(worker),
            frame_events,
            destructive_quiescence,
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped,
            expected_destructive_quiescence: Vec::new(),
            output: output.key,
            generation: 1,
            transition_generation: 1,
            output_ready: false,
            resume_leases: Arc::new(Mutex::new(GenerationLeaseSlot::default())),
            topology_client: None,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while adapter
            .poll_output_registration()
            .expect("frame-driver registration remains healthy")
            != LiveOutputRegistration::Ready
        {
            assert!(
                Instant::now() < deadline,
                "frame-driver output registration timed out"
            );
            thread::yield_now();
        }
        (
            adapter,
            scene_events,
            presented,
            fallback_clears,
            suppress_upscaling,
        )
    }

    fn suppress_frame_driver_upscaling(world: &mut World) {
        // Model the live empty-scene failure at its Bevy boundary: no graph
        // node consumes the prepared final attachment, while the active KMS
        // camera and its configured clear remain intact.
        if !world
            .resource::<FallbackClearProbe>()
            .suppress_upscaling
            .load(Ordering::SeqCst)
        {
            return;
        }
        let views = world
            .query_filtered::<Entity, With<ViewUpscalingPipeline>>()
            .iter(world)
            .collect::<Vec<_>>();
        for view in views {
            world.entity_mut(view).remove::<ViewUpscalingPipeline>();
        }
    }

    fn submitted_frames(events: &[KmsRenderFrameEvent]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, KmsRenderFrameEvent::FrameSubmitted { .. }))
            .count()
    }

    fn reach_steady_frame_submission(adapter: &mut LiveRenderAdapter) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            adapter
                .update()
                .expect("frame-driver update remains healthy");
            if submitted_frames(&adapter.drain_frame_events()) == 1 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "frame-driver emitted no initial submitted frame"
            );
            thread::yield_now();
        }
    }

    fn assert_next_update_submits(adapter: &mut LiveRenderAdapter) {
        adapter
            .update()
            .expect("frame-driver update remains healthy");
        assert_eq!(
            submitted_frames(&adapter.drain_frame_events()),
            1,
            "every steady active-output update submits exactly one written frame"
        );
    }

    fn populated_surface(id: SurfaceId) -> ProtocolEvent {
        ProtocolEvent::SurfaceUpserted {
            id,
            scene: crate::protocol::SurfaceSceneSnapshot {
                layout: SurfaceLayout {
                    x: 24.0,
                    y: 18.0,
                    width: 64.0,
                    height: 48.0,
                    z: 1.0,
                    source: None,
                    parent: None,
                    transform: SurfaceTransform::Normal,
                    visible: true,
                    toplevel: None,
                },
                kind: crate::protocol::SceneSurfaceKind::Toplevel,
                title: None,
            },
            frame: SurfaceFrame::Shm(ShmFrame {
                width: 2,
                height: 2,
                opaque: true,
                rgba: Arc::new(vec![0x40; 16]),
            }),
        }
    }

    fn decorated_surface(id: SurfaceId) -> ProtocolEvent {
        let mut event = populated_surface(id);
        let ProtocolEvent::SurfaceUpserted { scene, .. } = &mut event else {
            unreachable!("populated_surface always returns an upsert")
        };
        scene.layout.toplevel = Some(crate::protocol::ToplevelSceneState {
            decoration: crate::protocol::SceneDecorationMode::ServerSide,
            focused: true,
            committed_maximized: false,
            window_geometry: crate::protocol::SceneWindowGeometry {
                x: 0.0,
                y: 0.0,
                width: scene.layout.width,
                height: scene.layout.height,
            },
            chrome_pointer: crate::protocol::ChromePointerSceneState::default(),
        });
        event
    }

    #[derive(Clone, Default)]
    struct AcquireBarrier(Arc<(Mutex<AcquireBarrierState>, Condvar)>);

    #[derive(Default)]
    struct AcquireBarrierState {
        entered: bool,
        released: bool,
    }

    struct DroppableTargetProbe(Arc<AtomicBool>);

    impl Drop for DroppableTargetProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct ForcedDoubleRetirementFailure {
        ownership: Option<DroppableTargetProbe>,
        retirement_attempts: Arc<AtomicUsize>,
        fallback_reports: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ForcedDoubleRetirementFailure {
        fn retire_submitted_work(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            self.retirement_attempts.fetch_add(1, Ordering::SeqCst);
            Err(KmsRenderPlatformFailure::terminal(
                "kms-live-surface-retirement-unproven",
                "forced retirement failure",
            ))
        }

        fn teardown(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            self.retire_submitted_work()
        }
    }

    impl Drop for ForcedDoubleRetirementFailure {
        fn drop(&mut self) {
            let retirement_attempts = Arc::clone(&self.retirement_attempts);
            let fallback_reports = Arc::clone(&self.fallback_reports);
            drop_live_ownership_fail_closed(
                &mut self.ownership,
                move |_| {
                    retirement_attempts.fetch_add(1, Ordering::SeqCst);
                    Err(KmsRenderPlatformFailure::terminal(
                        "kms-live-surface-retirement-unproven",
                        "forced fallback retirement failure",
                    ))
                },
                |_| panic!("unproven retirement must not destroy live targets"),
                move |failure| {
                    fallback_reports
                        .lock()
                        .expect("fallback report probe")
                        .push(failure.code);
                },
            );
        }
    }

    #[test]
    fn forced_double_retirement_failure_leaks_targets_and_reports_error() {
        let target_dropped = Arc::new(AtomicBool::new(false));
        let retirement_attempts = Arc::new(AtomicUsize::new(0));
        let fallback_reports = Arc::new(Mutex::new(Vec::new()));
        let mut platform = ForcedDoubleRetirementFailure {
            ownership: Some(DroppableTargetProbe(Arc::clone(&target_dropped))),
            retirement_attempts: Arc::clone(&retirement_attempts),
            fallback_reports: Arc::clone(&fallback_reports),
        };

        let failure = platform
            .teardown()
            .expect_err("explicit teardown reports the first retirement failure");
        assert_eq!(failure.code, "kms-live-surface-retirement-unproven");
        drop(platform);

        assert_eq!(retirement_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            *fallback_reports.lock().expect("fallback reports"),
            vec!["kms-live-surface-retirement-unproven"]
        );
        assert!(
            !target_dropped.load(Ordering::SeqCst),
            "unfenced targets must remain leaked until process exit"
        );
    }

    #[test]
    fn resume_lease_slot_consumes_only_the_exact_generation() {
        let mut slot = GenerationLeaseSlot::default();
        let lease: std::os::fd::OwnedFd = UnixStream::pair().expect("lease pipe").0.into();
        let raw = lease.as_raw_fd();
        slot.stage(4, lease).expect("stage generation four");
        assert_eq!(
            slot.take(3).expect_err("stale generation rejected"),
            "kms-live-resume-lease-generation-mismatch"
        );
        assert_eq!(
            slot.take(4).expect("exact generation consumes").as_raw_fd(),
            raw
        );
    }

    #[test]
    fn resume_lease_slot_refuses_to_retain_two_authorities() {
        let mut slot = GenerationLeaseSlot::<std::os::fd::OwnedFd>::default();
        slot.stage(4, UnixStream::pair().expect("first lease pipe").0.into())
            .expect("stage generation four");
        assert_eq!(
            slot.stage(5, UnixStream::pair().expect("second lease pipe").0.into(),)
                .expect_err("a second authority cannot displace the first"),
            "kms-live-resume-lease-duplicate"
        );
        drop(slot.take(4).expect("the first authority remains staged"));
    }

    #[test]
    fn staged_resume_lease_fixture_carries_authority_and_fallback_plan() {
        let staged = staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
            fd: UnixStream::pair().expect("staged lease pipe").0.into(),
        });

        assert!(staged.lease.fd.as_raw_fd() >= 0);
        assert!(matches!(
            staged.presentation.classification,
            super::super::resume_scanout::ResumePresentationClassification::ModesetRequired(
                super::super::resume_scanout::ResumeModesetReason::NoUsableState
            )
        ));
        assert!(staged.presentation.deadline.instant().is_some());
    }

    #[test]
    fn resume_lease_staging_refuses_a_generation_gap_without_occupying_the_slot() {
        let output = blocked_output();
        let resume_leases = Arc::new(Mutex::new(GenerationLeaseSlot::default()));
        let adapter = LiveRenderAdapter {
            app: None,
            render_world_dropped: None,
            worker: None,
            frame_events: mpsc::channel().1,
            destructive_quiescence: DestructiveQuiescenceLatch::default(),
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped: Arc::new(AtomicBool::new(false)),
            expected_destructive_quiescence: Vec::new(),
            output: output.key,
            generation: 2,
            transition_generation: 2,
            output_ready: false,
            resume_leases: Arc::clone(&resume_leases),
            topology_client: None,
        };

        let error = adapter
            .stage_resume_lease(
                4,
                staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
                    fd: UnixStream::pair().expect("gap lease pipe").0.into(),
                }),
            )
            .expect_err("a generation gap must be refused before occupying the slot");
        assert!(
            error.to_string().starts_with("kms-live-generation-gap:"),
            "the gap needs a distinct diagnostic: {error}"
        );
        adapter
            .stage_resume_lease(
                3,
                staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
                    fd: UnixStream::pair().expect("next lease pipe").0.into(),
                }),
            )
            .expect("the exact next generation can still occupy the slot");
        assert_eq!(
            resume_leases
                .lock()
                .expect("resume lease slot")
                .generation(),
            Some(3)
        );
    }

    #[test]
    fn pump_transition_batch_requires_contiguous_generations() {
        let output = blocked_output();
        let commands = vec![
            KmsRenderCommand::Resume { generation: 2 },
            KmsRenderCommand::AddOutput {
                generation: 3,
                output,
            },
        ];
        assert_eq!(
            validate_transition_generations(1, &commands).expect("contiguous transition"),
            3
        );
        let gap = vec![
            KmsRenderCommand::Suspend { generation: 3 },
            KmsRenderCommand::Resume { generation: 4 },
        ];
        assert!(
            validate_transition_generations(1, &gap)
                .expect_err("a transition beginning after the next generation is rejected")
                .to_string()
                .starts_with("kms-live-generation-gap:")
        );
        let stale = vec![
            KmsRenderCommand::Suspend { generation: 2 },
            KmsRenderCommand::Resume { generation: 2 },
        ];
        assert!(
            validate_transition_generations(1, &stale)
                .expect_err("duplicate generation rejected")
                .to_string()
                .starts_with("kms-live-stale-generation:")
        );
        assert!(
            validate_transition_generations(1, &[])
                .expect_err("empty transition rejected")
                .to_string()
                .starts_with("kms-live-empty-transition:")
        );
    }

    fn gate_test_adapter() -> LiveRenderAdapter {
        let output = blocked_output();
        LiveRenderAdapter {
            app: None,
            render_world_dropped: None,
            worker: None,
            frame_events: mpsc::channel().1,
            destructive_quiescence: DestructiveQuiescenceLatch::default(),
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped: Arc::new(AtomicBool::new(false)),
            expected_destructive_quiescence: Vec::new(),
            output: output.key,
            generation: 1,
            transition_generation: 1,
            output_ready: false,
            resume_leases: Arc::new(Mutex::new(GenerationLeaseSlot::default())),
            topology_client: None,
        }
    }

    #[test]
    fn destructive_quiescence_latch_requires_exact_generation_identity() {
        let mut adapter = gate_test_adapter();
        let expected = DestructiveQuiescenceIdentity {
            operation: KmsRenderOperation::ChangeOutput,
            generation: 8,
            key: Some(adapter.output.clone()),
        };
        adapter
            .expected_destructive_quiescence
            .push(expected.clone());
        adapter
            .destructive_quiescence
            .publish(DestructiveQuiescenceIdentity {
                generation: 7,
                ..expected.clone()
            })
            .expect("stale identity publishes for validation");
        let error = adapter
            .observe_destructive_quiescence()
            .expect_err("stale quiescence cannot close the current generation");
        assert!(
            error
                .to_string()
                .starts_with("kms-live-destructive-quiescence-generation-mismatch:")
        );
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);

        adapter
            .destructive_quiescence
            .publish(expected.clone())
            .expect("exact identity publishes");
        adapter
            .observe_destructive_quiescence()
            .expect("exact identity closes the gate");
        assert_eq!(
            adapter.update_gate,
            LiveRenderUpdateGate::AwaitingDestructiveReply(expected)
        );
    }

    #[test]
    fn change_and_remove_keep_full_updates_closed_until_their_safe_boundary() {
        let mut adapter = gate_test_adapter();
        let key = adapter.output.clone();
        let change = DestructiveQuiescenceIdentity {
            operation: KmsRenderOperation::ChangeOutput,
            generation: 2,
            key: Some(key.clone()),
        };
        adapter.update_gate = LiveRenderUpdateGate::AwaitingDestructiveReply(change.clone());
        assert!(!adapter.advance_update_gate(&[], false));
        assert_eq!(
            adapter.update_gate,
            LiveRenderUpdateGate::AwaitingDestructiveReply(change)
        );
        assert!(adapter.advance_update_gate(&[], true));
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);

        let remove = DestructiveQuiescenceIdentity {
            operation: KmsRenderOperation::RemoveOutput,
            generation: 3,
            key: Some(key.clone()),
        };
        adapter.update_gate = LiveRenderUpdateGate::AwaitingDestructiveReply(remove.clone());
        assert!(!adapter.advance_update_gate(
            &[KmsRenderReply::OutputRemoved {
                generation: 2,
                key: key.clone(),
            }],
            false,
        ));
        assert_eq!(
            adapter.update_gate,
            LiveRenderUpdateGate::AwaitingDestructiveReply(remove)
        );
        assert!(!adapter.advance_update_gate(
            &[KmsRenderReply::OutputRemoved { generation: 3, key }],
            false,
        ));
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);
    }

    #[test]
    fn multiple_destructive_commands_are_refused_before_worker_ingress() {
        let mut adapter = gate_test_adapter();
        let error = adapter
            .begin_transition(vec![
                KmsRenderCommand::Suspend { generation: 2 },
                KmsRenderCommand::RemoveOutput {
                    generation: 3,
                    key: adapter.output.clone(),
                },
            ])
            .expect_err("one transition cannot carry two destructive boundaries");

        assert!(
            error
                .to_string()
                .starts_with("kms-live-multiple-destructive-commands:"),
            "the refusal needs a stable typed diagnostic: {error}"
        );
        assert_eq!(adapter.transition_generation, 1);
        assert!(adapter.expected_destructive_quiescence.is_empty());
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);
    }

    #[test]
    fn resume_batch_refuses_a_lease_staged_with_the_output_ready_generation() {
        let output = blocked_output();
        let resume_leases = Arc::new(Mutex::new(GenerationLeaseSlot::default()));
        resume_leases
            .lock()
            .expect("resume lease slot")
            .stage(
                4,
                staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
                    fd: UnixStream::pair().expect("resume lease pipe").0.into(),
                }),
            )
            .expect("stage output-ready generation");
        let mut adapter = LiveRenderAdapter {
            app: None,
            render_world_dropped: None,
            worker: None,
            frame_events: mpsc::channel().1,
            destructive_quiescence: DestructiveQuiescenceLatch::default(),
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped: Arc::new(AtomicBool::new(false)),
            expected_destructive_quiescence: Vec::new(),
            output: output.key.clone(),
            generation: 2,
            transition_generation: 2,
            output_ready: false,
            resume_leases,
            topology_client: None,
        };

        let error = adapter
            .begin_transition(vec![
                KmsRenderCommand::Resume { generation: 3 },
                KmsRenderCommand::AddOutput {
                    generation: 4,
                    output,
                },
            ])
            .expect_err("the staged authority must match the batch Resume command");
        assert!(
            error
                .to_string()
                .starts_with("kms-live-resume-lease-batch-generation-mismatch:"),
            "the mismatch needs a distinct diagnostic: {error}"
        );
        assert_eq!(adapter.transition_generation, 2);
    }

    #[test]
    fn live_shutdown_reports_a_worker_failure_exit() {
        let outcome = KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::TeardownFailed {
            prior: Box::new(KmsRenderWorkerExit::Cancelled),
            failure: KmsRenderPlatformFailure::new(
                "injected-teardown-failure",
                "worker teardown failed",
            ),
        });
        let error = live_worker_shutdown_result(outcome)
            .expect_err("a guarded worker failure is not a successful live shutdown");
        assert!(error.to_string().contains("injected-teardown-failure"));
    }

    #[test]
    fn guarded_live_app_drops_both_worlds_before_platform_teardown() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new();
        app.insert_resource(WorldDropProbe {
            event: "main-world",
            events: Arc::clone(&events),
        });
        let mut render_app = SubApp::new();
        render_app.init_schedule(Render);
        render_app.insert_resource(WorldDropProbe {
            event: "render-world",
            events: Arc::clone(&events),
        });
        app.insert_sub_app(RenderApp, render_app);
        let LiveKmsRenderInstallation {
            worker,
            render_world_dropped,
            frame_events,
            destructive_quiescence,
        } = install_live_kms_render_target(
            &mut app,
            DropOrderPlatform {
                events: Arc::clone(&events),
            },
        )
        .expect("guarded live installer starts");
        let output = blocked_output();
        let mut adapter = LiveRenderAdapter {
            app: Some(app),
            render_world_dropped: Some(render_world_dropped),
            worker: Some(worker),
            frame_events,
            destructive_quiescence,
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped: Arc::new(AtomicBool::new(false)),
            expected_destructive_quiescence: Vec::new(),
            output: output.key,
            generation: 1,
            transition_generation: 1,
            output_ready: false,
            resume_leases: Arc::new(Mutex::new(GenerationLeaseSlot::default())),
            topology_client: None,
        };

        adapter
            .shutdown_inner()
            .expect("guarded live app shuts down");

        let events = events.lock().expect("drop-order log");
        let platform = events
            .iter()
            .position(|event| *event == "platform")
            .expect("platform teardown recorded");
        for world in ["main-world", "render-world"] {
            let dropped = events
                .iter()
                .position(|event| *event == world)
                .expect("world drop recorded");
            assert!(dropped < platform, "{world} must drop before the platform");
        }
    }
    #[test]
    fn pump_shutdown_suppresses_protocol_release_for_displayed_cacheable_dmabuf() {
        let (protocol_release, protocol_side) = mpsc::channel();
        let mut app = App::new();
        app.init_resource::<bevy::asset::Assets<bevy::image::Image>>()
            .init_resource::<ImportedDmabufImages>();
        let importer = app.world().resource::<ImportedDmabufImages>().clone();
        let image = importer
            .import(
                &mut app
                    .world_mut()
                    .resource_mut::<bevy::asset::Assets<bevy::image::Image>>(),
                DmabufBufferId(73),
                true,
                DmabufDescriptor {
                    width: 8,
                    height: 8,
                    fourcc: Fourcc::Argb8888 as u32,
                    modifier: u64::from(Modifier::Linear),
                    planes: vec![DmabufPlane {
                        fd: File::open("/dev/null")
                            .expect("/dev/null is available")
                            .into(),
                        offset: 0,
                        stride: 32,
                    }],
                },
                DmabufRelease::Explicit(Box::new(move || {
                    protocol_release
                        .send(73_u64)
                        .expect("protocol-side release receiver remains live");
                })),
            )
            .expect("cacheable DMA-BUF use is registered");
        app.world_mut().spawn(Sprite::from_image(image));
        drop(importer);

        let mut render_app = SubApp::new();
        render_app.init_schedule(Render);
        app.insert_sub_app(RenderApp, render_app);
        let events = Arc::new(Mutex::new(Vec::new()));
        let output = blocked_output();
        let LiveKmsRenderInstallation {
            worker,
            render_world_dropped,
            frame_events,
            destructive_quiescence,
        } = install_live_kms_render_target(
            &mut app,
            DropOrderPlatform {
                events: Arc::clone(&events),
            },
        )
        .expect("guarded live installer starts");
        let mut adapter = LiveRenderAdapter {
            app: Some(app),
            render_world_dropped: Some(render_world_dropped),
            worker: Some(worker),
            frame_events,
            destructive_quiescence,
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped: Arc::new(AtomicBool::new(false)),
            expected_destructive_quiescence: Vec::new(),
            output: output.key,
            generation: 1,
            transition_generation: 1,
            output_ready: false,
            resume_leases: Arc::new(Mutex::new(GenerationLeaseSlot::default())),
            topology_client: None,
        };

        adapter
            .shutdown_inner()
            .expect("pump shutdown completes with a displayed DMA-BUF");

        assert!(
            matches!(
                protocol_side.try_recv(),
                Err(TryRecvError::Empty | TryRecvError::Disconnected)
            ),
            "App drop must not reach the protocol release seam during teardown"
        );
    }

    #[test]
    fn headless_live_plugin_set_has_no_window_or_pipelined_runner() {
        struct HeadlessPluginProbe;

        impl PluginGroup for HeadlessPluginProbe {
            fn build(self) -> PluginGroupBuilder {
                PluginGroupBuilder::start::<Self>()
                    .add(LogPlugin::default())
                    .add(WindowPlugin::default())
                    .add(WinitPlugin::default())
                    .add(PipelinedRenderingPlugin)
                    .add(TerminalCtrlCHandlerPlugin)
            }
        }

        drop(live_headless_plugins());
        let mut app = App::new();
        app.add_plugins(configure_live_headless_plugins(HeadlessPluginProbe.build()));

        assert!(app.is_plugin_added::<WindowPlugin>());
        assert!(!app.is_plugin_added::<LogPlugin>());
        assert!(!app.is_plugin_added::<WinitPlugin>());
        assert!(!app.is_plugin_added::<PipelinedRenderingPlugin>());
        assert!(!app.is_plugin_added::<TerminalCtrlCHandlerPlugin>());
        assert_non_pipelined_rendering(&app).expect("headless render scheduling");
        assert_eq!(
            app.world_mut()
                .query::<&PrimaryWindow>()
                .iter(app.world())
                .count(),
            0
        );
    }

    #[test]
    fn live_scene_modes_install_exactly_one_first_light_or_client_content_stack() {
        let mut first_light = App::new();
        install_live_scene(&mut first_light, LiveSceneMode::FirstLight);
        assert!(first_light.is_plugin_added::<FirstLightScenePlugin>());
        assert!(!first_light.is_plugin_added::<CompositorScenePlugin>());
        assert!(!first_light.is_plugin_added::<DmabufImportPlugin>());

        let mut client_content = App::new();
        install_live_scene(&mut client_content, LiveSceneMode::ClientContent);
        assert!(!client_content.is_plugin_added::<FirstLightScenePlugin>());
        assert!(client_content.is_plugin_added::<CompositorScenePlugin>());
        assert!(client_content.is_plugin_added::<DmabufImportPlugin>());

        let (_, feed) = ClientSceneFeed::test_channel();
        prepare_live_scene_start(
            &mut client_content,
            LiveSceneMode::ClientContent,
            Some(feed),
            (1920, 1080),
            crate::backend::kms::OutputScale120::ONE,
        )
        .expect("client scene feed transfers before the first update");
        assert!(
            client_content
                .world()
                .contains_resource::<ClientSceneFeed>()
        );
    }

    #[test]
    fn visible_cursor_empty_scene_uses_the_normal_pass_and_submits() {
        let (mut adapter, _scene_events, presented, fallback_clears, suppress_upscaling) =
            client_content_frame_driver();
        suppress_upscaling.store(false, Ordering::SeqCst);
        reach_steady_frame_submission(&mut adapter);
        let before = presented.load(Ordering::SeqCst);
        let fallback_before = fallback_clears.load(Ordering::SeqCst);

        for _ in 0..4 {
            assert_next_update_submits(&mut adapter);
        }

        assert_eq!(presented.load(Ordering::SeqCst) - before, 4);
        assert_eq!(
            fallback_clears.load(Ordering::SeqCst) - fallback_before,
            0,
            "visible default cursor writes the empty scene through the normal sprite pass"
        );
        adapter
            .shutdown_inner()
            .expect("empty-scene frame driver shuts down");
    }

    #[test]
    fn hidden_cursor_empty_scene_uses_the_clear_fallback_and_still_submits() {
        let (mut adapter, scene_events, presented, fallback_clears, _suppress_upscaling) =
            client_content_frame_driver();
        reach_steady_frame_submission(&mut adapter);
        scene_events
            .send(vec![ProtocolEvent::CursorUpdated {
                image: crate::protocol::CursorImage::Hidden,
            }])
            .expect("hidden cursor reaches the scene");
        let presented_before = presented.load(Ordering::SeqCst);
        let fallback_before = fallback_clears.load(Ordering::SeqCst);

        for _ in 0..4 {
            assert_next_update_submits(&mut adapter);
        }

        assert_eq!(presented.load(Ordering::SeqCst) - presented_before, 4);
        assert_eq!(
            fallback_clears.load(Ordering::SeqCst) - fallback_before,
            4,
            "hidden cursor leaves no drawable scene content, so the existing clear fallback owns it"
        );
        adapter
            .shutdown_inner()
            .expect("hidden-cursor frame driver shuts down");
    }

    #[test]
    fn submissions_continue_after_all_client_surfaces_are_destroyed() {
        let (mut adapter, scene_events, presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver();
        reach_steady_frame_submission(&mut adapter);
        let baseline_images = adapter
            .app
            .as_ref()
            .expect("frame-driver App")
            .world()
            .resource::<bevy::asset::Assets<bevy::image::Image>>()
            .len();
        let id = SurfaceId(1);
        scene_events
            .send(vec![populated_surface(id)])
            .expect("client surface reaches the scene");
        assert_next_update_submits(&mut adapter);
        assert!(
            adapter
                .app
                .as_ref()
                .expect("frame-driver App")
                .world()
                .resource::<bevy::asset::Assets<bevy::image::Image>>()
                .len()
                > baseline_images,
            "the populated client surface owns a scene image"
        );

        scene_events
            .send(vec![ProtocolEvent::SurfaceDestroyed { id }])
            .expect("quit-all surface removal reaches the scene");
        let before_empty_updates = presented.load(Ordering::SeqCst);
        for _ in 0..4 {
            assert_next_update_submits(&mut adapter);
        }

        let app = adapter.app.as_mut().expect("frame-driver App");
        assert_eq!(
            app.world()
                .resource::<bevy::asset::Assets<bevy::image::Image>>()
                .len(),
            baseline_images,
            "quit-all removes the last client image"
        );
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<KmsOutputCamera>>()
                .iter(app.world())
                .count(),
            1,
            "surface despawn leaves the live output camera intact"
        );
        assert_eq!(presented.load(Ordering::SeqCst) - before_empty_updates, 4);
        adapter
            .shutdown_inner()
            .expect("quit-all frame driver shuts down");
    }

    #[test]
    fn live_headless_renderer_compiles_chrome_frame_material() {
        let (mut adapter, scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            decorated_client_content_frame_driver();
        reach_steady_frame_submission(&mut adapter);
        scene_events
            .send(vec![decorated_surface(SurfaceId(1))])
            .expect("decorated client surface reaches the scene");

        for _ in 0..4 {
            assert_next_update_submits(&mut adapter);
        }

        let app = adapter.app.as_mut().expect("frame-driver App");
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<crate::decoration_scene::DecoChromeFrame>>()
                .iter(app.world())
                .count(),
            1,
            "the live render app reaches the ChromeFrameMaterial path"
        );
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<crate::decoration_scene::DecoShadow>>()
                .iter(app.world())
                .count(),
            1,
            "the live render app reaches the ShadowMaterial path"
        );
        adapter
            .shutdown_inner()
            .expect("decorated frame driver shuts down");
    }

    #[test]
    fn suspend_completes_after_all_client_surfaces_are_destroyed() {
        let (mut adapter, scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver();
        reach_steady_frame_submission(&mut adapter);
        let id = SurfaceId(2);
        scene_events
            .send(vec![populated_surface(id)])
            .expect("client surface reaches the scene");
        assert_next_update_submits(&mut adapter);
        scene_events
            .send(vec![ProtocolEvent::SurfaceDestroyed { id }])
            .expect("quit-all surface removal reaches the scene");
        assert_next_update_submits(&mut adapter);

        adapter
            .begin_transition(vec![KmsRenderCommand::Suspend { generation: 2 }])
            .expect("suspend begins after quit-all");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let replies = adapter
                .transition_update()
                .expect("quit-all suspend transition remains healthy");
            if replies
                .iter()
                .any(|reply| matches!(reply, KmsRenderReply::Suspended { generation: 2 }))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "quit-all suspend transition timed out"
            );
            thread::yield_now();
        }
        adapter
            .shutdown_inner()
            .expect("suspended frame driver shuts down");
    }

    #[test]
    fn held_worker_destroy_race_runs_only_registrar_polling_after_quiescence() {
        let (release_destroy, held_destroy) = mpsc::sync_channel(0);
        let destroyed = Arc::new(AtomicBool::new(false));
        let post_destroy_updates = Arc::new(AtomicUsize::new(0));
        let (mut adapter, _scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver_inner(
                false,
                Some(held_destroy),
                Some(Arc::clone(&destroyed)),
                Some(Arc::clone(&post_destroy_updates)),
                None,
                None,
            );
        reach_steady_frame_submission(&mut adapter);
        adapter
            .begin_transition(vec![KmsRenderCommand::Suspend { generation: 2 }])
            .expect("held suspend begins");

        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(
            adapter.update_gate,
            LiveRenderUpdateGate::AwaitingDestructiveReply(DestructiveQuiescenceIdentity {
                operation: KmsRenderOperation::Suspend,
                generation: 2,
                key: None,
            })
        ) {
            assert!(
                adapter
                    .transition_update()
                    .expect("quiescence publication remains healthy")
                    .is_empty()
            );
            assert!(Instant::now() < deadline, "quiescence latch timed out");
            thread::yield_now();
        }
        assert!(
            !destroyed.load(Ordering::SeqCst),
            "the full-update gate must close before the worker may destroy"
        );

        release_destroy.send(()).expect("release held destruction");
        while !destroyed.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "held destruction timed out");
            thread::yield_now();
        }
        loop {
            let replies = adapter
                .transition_update()
                .expect("registrar-only suspended delivery remains healthy");
            if replies
                .iter()
                .any(|reply| matches!(reply, KmsRenderReply::Suspended { generation: 2 }))
            {
                break;
            }
            assert!(Instant::now() < deadline, "Suspended delivery timed out");
            thread::yield_now();
        }
        assert_eq!(
            post_destroy_updates.load(Ordering::SeqCst),
            0,
            "no full App::update may run after destructive quiescence"
        );
        assert_eq!(
            adapter.update_gate,
            LiveRenderUpdateGate::Paused { generation: 2 }
        );
        adapter
            .shutdown_inner()
            .expect("held-race frame driver shuts down");
    }

    #[test]
    fn orderly_shutdown_stop_frontier_blocks_admitted_update_before_held_teardown() {
        let (teardown_entered_sender, teardown_entered) = mpsc::sync_channel(0);
        let (release_teardown, held_teardown) = mpsc::sync_channel(0);
        let destroyed = Arc::new(AtomicBool::new(false));
        let post_stop_updates = Arc::new(AtomicUsize::new(0));
        let (mut adapter, _scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver_inner(
                false,
                None,
                Some(Arc::clone(&destroyed)),
                Some(Arc::clone(&post_stop_updates)),
                None,
                Some(HeldTerminalTeardown {
                    entered: teardown_entered_sender,
                    release: held_teardown,
                }),
            );
        reach_steady_frame_submission(&mut adapter);

        // Model an Update already admitted by receive_live_pump_command when
        // LiveRenderPump::begin_stop publishes the terminal frontier.
        adapter.stop_terminal_updates();
        adapter
            .update()
            .expect("an admitted terminal update is reduced to registrar polling");
        assert_eq!(
            post_stop_updates.load(Ordering::SeqCst),
            0,
            "no full App::update may cross the terminal stop frontier"
        );

        let observed_destroyed = Arc::clone(&destroyed);
        let observed_updates = Arc::clone(&post_stop_updates);
        let observer = thread::spawn(move || {
            teardown_entered
                .recv_timeout(Duration::from_secs(30))
                .expect("terminal platform teardown begins");
            assert!(
                observed_destroyed.load(Ordering::SeqCst),
                "the fake platform marks destruction before holding teardown"
            );
            assert_eq!(
                observed_updates.load(Ordering::SeqCst),
                0,
                "no instrumented full update runs while destruction is held"
            );
            release_teardown
                .send(())
                .expect("release held terminal teardown");
        });
        adapter
            .shutdown_inner()
            .expect("orderly shutdown completes after held terminal teardown");
        observer.join().expect("teardown observer remains healthy");
        assert_eq!(post_stop_updates.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn terminal_stop_blocks_post_install_update_while_awaiting_replacement() {
        let destroyed = Arc::new(AtomicBool::new(false));
        let post_stop_updates = Arc::new(AtomicUsize::new(0));
        let (mut adapter, _scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver_inner(
                false,
                None,
                Some(Arc::clone(&destroyed)),
                Some(Arc::clone(&post_stop_updates)),
                None,
                None,
            );
        reach_steady_frame_submission(&mut adapter);
        let deadline = Instant::now() + Duration::from_secs(30);

        adapter
            .begin_transition(vec![KmsRenderCommand::Suspend { generation: 2 }])
            .expect("terminal replacement test pause begins");
        loop {
            let replies = adapter
                .transition_update()
                .expect("terminal replacement test pause remains healthy");
            if replies
                .iter()
                .any(|reply| matches!(reply, KmsRenderReply::Suspended { generation: 2 }))
            {
                break;
            }
            assert!(Instant::now() < deadline, "test pause timed out");
            thread::yield_now();
        }
        assert!(destroyed.load(Ordering::SeqCst));
        assert_eq!(post_stop_updates.load(Ordering::SeqCst), 0);

        adapter
            .stage_resume_lease(
                3,
                staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
                    fd: UnixStream::pair()
                        .expect("terminal replacement lease pipe")
                        .0
                        .into(),
                }),
            )
            .expect("terminal replacement authority stages");
        let output = blocked_output();
        let key = output.key.clone();
        adapter
            .begin_transition(vec![
                KmsRenderCommand::Resume { generation: 3 },
                KmsRenderCommand::AddOutput {
                    generation: 4,
                    output,
                },
            ])
            .expect("terminal replacement begins");
        assert!(matches!(
            adapter.update_gate,
            LiveRenderUpdateGate::AwaitingReplacement { generation: 4, key: ref waiting }
                if *waiting == key
        ));

        adapter.stop_terminal_updates();
        loop {
            adapter
                .transition_update()
                .expect("terminal replacement bookkeeping remains healthy");
            if LiveRenderEngine::replacement_installed(
                adapter
                    .app
                    .as_ref()
                    .expect("terminal replacement test owns App")
                    .world(),
                4,
                &key,
            ) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "terminal replacement installation timed out"
            );
            thread::yield_now();
        }
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);
        adapter
            .transition_update()
            .expect("an open transition gate remains terminally registrar-only");
        assert_eq!(
            post_stop_updates.load(Ordering::SeqCst),
            0,
            "replacement installation cannot bypass terminal stop"
        );
        adapter
            .shutdown_inner()
            .expect("terminal replacement test shuts down");
    }

    #[test]
    fn resume_reopens_full_updates_only_after_replacement_install() {
        let (release_destroy, held_destroy) = mpsc::sync_channel(0);
        let destroyed = Arc::new(AtomicBool::new(false));
        let (mut adapter, _scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver_inner(
                false,
                Some(held_destroy),
                Some(Arc::clone(&destroyed)),
                None,
                None,
                None,
            );
        reach_steady_frame_submission(&mut adapter);
        adapter
            .begin_transition(vec![KmsRenderCommand::Suspend { generation: 2 }])
            .expect("resume test suspend begins");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !matches!(
            adapter.update_gate,
            LiveRenderUpdateGate::AwaitingDestructiveReply(_)
        ) {
            adapter
                .transition_update()
                .expect("resume test reaches destructive quiescence");
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        release_destroy
            .send(())
            .expect("release resume-test destroy");
        while adapter.update_gate != (LiveRenderUpdateGate::Paused { generation: 2 }) {
            adapter
                .transition_update()
                .expect("resume test reaches Paused");
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }

        let key = adapter.output.clone();
        let wrong_generation_updates = Arc::new(AtomicUsize::new(0));
        let replacement_updates = Arc::new(AtomicUsize::new(0));
        let wrong_probe = Arc::clone(&wrong_generation_updates);
        let replacement_probe = Arc::clone(&replacement_updates);
        let probe_key = key.clone();
        adapter
            .app
            .as_mut()
            .expect("resume test owns App")
            .add_systems(
                bevy::app::Update,
                move |outputs: Res<KmsMainWorldOutputs>| {
                    if outputs
                        .0
                        .get(&probe_key)
                        .is_some_and(|output| output.generation == 4)
                    {
                        replacement_probe.fetch_add(1, Ordering::SeqCst);
                    } else {
                        wrong_probe.fetch_add(1, Ordering::SeqCst);
                    }
                },
            );
        adapter
            .stage_resume_lease(
                3,
                staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
                    fd: UnixStream::pair().expect("resume lease pipe").0.into(),
                }),
            )
            .expect("resume authority stages");
        let output = blocked_output();
        adapter
            .begin_transition(vec![
                KmsRenderCommand::Resume { generation: 3 },
                KmsRenderCommand::AddOutput {
                    generation: 4,
                    output,
                },
            ])
            .expect("resume and replacement begin");
        assert!(matches!(
            adapter.update_gate,
            LiveRenderUpdateGate::AwaitingReplacement { generation: 4, key: ref waiting }
                if *waiting == key
        ));
        loop {
            let replies = adapter
                .transition_update()
                .expect("resume replacement transition remains healthy");
            if replies.iter().any(|reply| {
                matches!(reply, KmsRenderReply::OutputReady { generation: 4, key: ready }
                    if *ready == key)
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "replacement readiness timed out");
            thread::yield_now();
        }
        assert_eq!(wrong_generation_updates.load(Ordering::SeqCst), 0);
        assert!(replacement_updates.load(Ordering::SeqCst) > 0);
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);
        adapter
            .shutdown_inner()
            .expect("resumed frame driver shuts down");
    }

    #[test]
    fn retryable_resume_add_failure_rolls_back_and_second_attempt_reopens_gate() {
        let add_failures = Arc::new(AtomicUsize::new(0));
        let (mut adapter, _scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver_inner(
                false,
                None,
                None,
                None,
                Some(Arc::clone(&add_failures)),
                None,
            );
        reach_steady_frame_submission(&mut adapter);
        let deadline = Instant::now() + Duration::from_secs(30);

        adapter
            .begin_transition(vec![KmsRenderCommand::Suspend { generation: 2 }])
            .expect("retry test pause begins");
        loop {
            let replies = adapter
                .transition_update()
                .expect("retry test pause remains healthy");
            if replies
                .iter()
                .any(|reply| matches!(reply, KmsRenderReply::Suspended { generation: 2 }))
            {
                break;
            }
            assert!(Instant::now() < deadline, "initial pause timed out");
            thread::yield_now();
        }
        assert_eq!(
            adapter.update_gate,
            LiveRenderUpdateGate::Paused { generation: 2 }
        );

        add_failures.store(1, Ordering::SeqCst);
        adapter
            .stage_resume_lease(
                3,
                staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
                    fd: UnixStream::pair().expect("first retry lease pipe").0.into(),
                }),
            )
            .expect("first retry authority stages");
        adapter
            .begin_transition(vec![
                KmsRenderCommand::Resume { generation: 3 },
                KmsRenderCommand::AddOutput {
                    generation: 4,
                    output: blocked_output(),
                },
            ])
            .expect("first resume attempt begins");
        loop {
            let replies = adapter
                .transition_update()
                .expect("retryable AddOutput failure remains non-terminal");
            if replies.iter().any(|reply| {
                matches!(reply, KmsRenderReply::OutputFailed {
                    generation: 4,
                    reason,
                    ..
                } if reason.contains("kms-live-test-retryable-add"))
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "retryable AddOutput failure timed out"
            );
            thread::yield_now();
        }
        assert_eq!(
            adapter.update_gate,
            LiveRenderUpdateGate::Open,
            "the empty failed-replacement state must admit rollback Suspend"
        );
        drop(
            adapter
                .resume_leases
                .lock()
                .expect("retry lease slot")
                .take(3)
                .expect("test platform leaves the first staged lease for explicit cleanup"),
        );

        adapter
            .begin_transition(vec![KmsRenderCommand::Suspend { generation: 5 }])
            .expect("rollback Suspend begins");
        loop {
            let replies = adapter
                .transition_update()
                .expect("rollback Suspend remains healthy");
            if replies
                .iter()
                .any(|reply| matches!(reply, KmsRenderReply::Suspended { generation: 5 }))
            {
                break;
            }
            assert!(Instant::now() < deadline, "rollback Suspend timed out");
            thread::yield_now();
        }
        assert_eq!(
            adapter.update_gate,
            LiveRenderUpdateGate::Paused { generation: 5 }
        );

        adapter
            .stage_resume_lease(
                6,
                staged_resume_lease_for_test(super::super::kms_live::MasterDrmLease {
                    fd: UnixStream::pair()
                        .expect("second retry lease pipe")
                        .0
                        .into(),
                }),
            )
            .expect("second retry authority stages");
        adapter
            .begin_transition(vec![
                KmsRenderCommand::Resume { generation: 6 },
                KmsRenderCommand::AddOutput {
                    generation: 7,
                    output: blocked_output(),
                },
            ])
            .expect("second resume attempt begins");
        loop {
            let replies = adapter
                .transition_update()
                .expect("second resume attempt remains healthy");
            if replies
                .iter()
                .any(|reply| matches!(reply, KmsRenderReply::OutputReady { generation: 7, .. }))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "second resume readiness timed out"
            );
            thread::yield_now();
        }
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);
        adapter
            .shutdown_inner()
            .expect("retry frame driver shuts down");
    }

    #[test]
    fn registered_kms_output_has_one_non_multisampled_camera() {
        let mut app = App::new();
        app.world_mut()
            .insert_resource(ManualTextureViews::default());
        let mut render_app = SubApp::new();
        render_app.init_schedule(Render);
        render_app.insert_resource(ManualTextureViews::default());
        app.insert_sub_app(RenderApp, render_app);
        let output = blocked_output();
        let LiveKmsRenderInstallation {
            worker,
            render_world_dropped,
            frame_events,
            destructive_quiescence,
        } = install_live_kms_render_target(
            &mut app,
            PanicAfterRegisteredPlatform {
                panic_connector_id: u32::MAX,
                acquire_called: Arc::new(AtomicBool::new(false)),
            },
        )
        .expect("guarded live installer starts");
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 1,
                output: output.clone(),
            })
            .expect("queue output registration");
        let mut adapter = LiveRenderAdapter {
            app: Some(app),
            render_world_dropped: Some(render_world_dropped),
            worker: Some(worker),
            frame_events,
            destructive_quiescence,
            update_gate: LiveRenderUpdateGate::Open,
            terminal_updates_stopped: Arc::new(AtomicBool::new(false)),
            expected_destructive_quiescence: Vec::new(),
            output: output.key,
            generation: 1,
            transition_generation: 1,
            output_ready: false,
            resume_leases: Arc::new(Mutex::new(GenerationLeaseSlot::default())),
            topology_client: None,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match adapter
                .poll_output_registration()
                .expect("registration remains healthy")
            {
                LiveOutputRegistration::Ready => break,
                LiveOutputRegistration::Pending => {
                    assert!(Instant::now() < deadline, "output registration timed out");
                    thread::park_timeout(Duration::from_millis(1));
                }
            }
        }

        let app = adapter.app.as_mut().expect("adapter owns the App");
        let cameras = app
            .world_mut()
            .query_filtered::<(&Msaa, &Projection), With<KmsOutputCamera>>()
            .iter(app.world())
            .collect::<Vec<_>>();
        assert_eq!(cameras.len(), 1);
        assert_eq!(*cameras[0].0, Msaa::Off);
        let Projection::Orthographic(projection) = cameras[0].1 else {
            panic!("KMS output camera must use an orthographic projection");
        };
        assert!(matches!(
            projection.scaling_mode,
            ScalingMode::Fixed {
                width: 320.0,
                height: 240.0
            }
        ));
        #[cfg(feature = "frame-capture")]
        {
            let tagged_targets = app
                .world_mut()
                .query_filtered::<
                    (&RenderTarget, &crate::frame_capture::FrameCaptureTarget),
                    With<KmsOutputCamera>,
                >()
                .iter(app.world())
                .collect::<Vec<_>>();
            assert_eq!(tagged_targets.len(), 1);
            assert!(matches!(tagged_targets[0].0, RenderTarget::TextureView(_)));
            assert_eq!(tagged_targets[0].1.name(), "Blocked-1");
        }
        adapter.shutdown_inner().expect("guarded app shuts down");
    }

    #[test]
    fn fixed_logical_projection_maps_output_corners_to_the_physical_target_corners() {
        let Projection::Orthographic(mut projection) = logical_output_projection((1536, 864))
        else {
            panic!("logical output projection is orthographic");
        };
        let baseline = OrthographicProjection::default_2d();
        assert_eq!(
            (projection.near, projection.far),
            (baseline.near, baseline.far)
        );
        projection.update(3840.0, 2160.0);
        assert_eq!(
            projection.area,
            bevy::math::Rect::new(-768.0, -432.0, 768.0, 432.0)
        );

        let clip_from_view = projection.get_clip_from_view();
        for (logical, physical) in [
            (Vec2::new(-768.0, 432.0), Vec2::new(0.0, 0.0)),
            (Vec2::new(768.0, 432.0), Vec2::new(3840.0, 0.0)),
            (Vec2::new(-768.0, -432.0), Vec2::new(0.0, 2160.0)),
            (Vec2::new(768.0, -432.0), Vec2::new(3840.0, 2160.0)),
        ] {
            let clip = clip_from_view.project_point3(logical.extend(0.0));
            let mapped = Vec2::new((clip.x + 1.0) * 0.5 * 3840.0, (1.0 - clip.y) * 0.5 * 2160.0);
            assert!(
                mapped.distance(physical) < 0.001,
                "logical corner {logical:?} mapped to {mapped:?}, not {physical:?}"
            );
        }
    }

    #[test]
    fn scale_one_fixed_projection_is_bit_identical_to_the_legacy_window_projection() {
        let mut legacy = OrthographicProjection::default_2d();
        legacy.update(1920.0, 1080.0);
        let Projection::Orthographic(mut fixed) = logical_output_projection((1920, 1080)) else {
            panic!("logical output projection is orthographic");
        };
        fixed.update(1920.0, 1080.0);
        assert_eq!(fixed.area, legacy.area);
        assert_eq!(fixed.get_clip_from_view(), legacy.get_clip_from_view());
        assert_eq!((fixed.near, fixed.far), (legacy.near, legacy.far));
    }

    #[test]
    fn worker_failure_reply_after_output_ready_is_terminal_and_drained() {
        let output = blocked_output();
        let (replies, receiver) = mpsc::channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarReplies(Mutex::new(receiver)));
        replies
            .send(KmsRenderReply::FrameSubmitted {
                generation: 1,
                key: output.key.clone(),
            })
            .unwrap();
        replies
            .send(KmsRenderReply::WorkerFailed {
                operation: KmsRenderOperation::Worker,
                generation: 2,
                key: Some(output.key.clone()),
                code: "injected-post-ready-worker-failure",
                reason: "worker failed after readiness".into(),
            })
            .unwrap();

        let error = drain_live_registrar_replies(&world, 1, &output.key, true, None, true)
            .expect_err("post-ready worker failure is terminal");

        assert!(
            error
                .to_string()
                .contains("injected-post-ready-worker-failure")
        );
        assert!(drain_registrar_replies(&world).unwrap().is_empty());
    }

    #[test]
    fn live_frame_submitted_replies_are_fully_drained() {
        let output = blocked_output();
        let (replies, receiver) = mpsc::channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarReplies(Mutex::new(receiver)));
        for generation in 1..=10_000 {
            replies
                .send(KmsRenderReply::FrameSubmitted {
                    generation,
                    key: output.key.clone(),
                })
                .unwrap();
        }

        let drained = drain_live_registrar_replies(&world, 1, &output.key, true, None, true)
            .expect("frame-submitted replies are disposable");

        assert!(drained.output_ready);
        assert_eq!(drained.replies_drained, 10_000);
        assert!(drain_registrar_replies(&world).unwrap().is_empty());
    }

    #[test]
    fn nominal_refresh_backoff_is_clamped() {
        assert_eq!(
            nominal_refresh_interval(1_000_000),
            Duration::from_millis(4)
        );
        assert_eq!(nominal_refresh_interval(1_000), Duration::from_millis(50));
        assert_eq!(
            nominal_refresh_interval(60_000),
            Duration::from_nanos(16_666_666)
        );
    }

    #[test]
    fn live_pump_quiescence_covers_the_bounded_surface_acquire() {
        let nominal = nominal_refresh_interval(60_000);
        assert_eq!(
            live_pump_quiesce_timeout(nominal),
            WGPU_SURFACE_ACQUIRE_TIMEOUT.saturating_mul(WGPU_SURFACE_ACQUIRE_BOUNDED_WAITS)
                + nominal.saturating_mul(2)
                + LIVE_PUMP_QUIESCE_MARGIN
        );
        assert_eq!(live_pump_quiesce_timeout(nominal).as_millis(), 3_283);
    }

    #[test]
    fn forced_render_error_sends_one_terminal_reply_and_stops_updates() {
        struct ForcedRenderErrorPump {
            app: App,
        }

        impl LivePumpUpdater for ForcedRenderErrorPump {
            fn update_for_pump(
                &mut self,
            ) -> Result<Vec<KmsRenderFrameEvent>, crate::backend::kms_live::KmsLiveError>
            {
                update_live_app(&mut self.app)?;
                Ok(Vec::new())
            }
        }

        let updates = Arc::new(AtomicUsize::new(0));
        let update_probe = Arc::clone(&updates);
        let mut app = App::new();
        app.insert_resource(FirstLiveRenderError::default())
            .add_systems(bevy::app::Update, move || {
                update_probe.fetch_add(1, Ordering::SeqCst);
            });
        let mut render_world = World::new();
        let first = RenderError {
            ty: bevy::render::error_handler::ErrorType::Validation,
            description: "forced first render failure detail".into(),
            source: None,
        };
        let repeated = RenderError {
            ty: bevy::render::error_handler::ErrorType::Internal,
            description: "later render failure must not replace the first".into(),
            source: None,
        };
        assert!(matches!(
            stop_live_rendering_after_first_error(&first, app.world_mut(), &mut render_world),
            RenderErrorPolicy::StopRendering
        ));
        stop_live_rendering_after_first_error(&repeated, app.world_mut(), &mut render_world);
        let mut pump = ForcedRenderErrorPump { app };
        let mut replies = Vec::new();
        for _queued_update in 0..2 {
            let (reply, terminal) = live_pump_update_reply(&mut pump);
            replies.push(reply);
            if terminal {
                break;
            }
        }

        assert_eq!(replies.len(), 1, "only the terminal reply is emitted");
        let detail = match &replies[0] {
            PumpReply::Updated(Err(error)) => error.to_string(),
            reply => panic!("forced render error returned {reply:?}"),
        };
        assert!(detail.contains("forced first render failure detail"));
        assert!(!detail.contains("later render failure"));
        assert_eq!(updates.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn render_error_before_destructive_quiescence_stays_fatal() {
        let (mut adapter, _scene_events, _presented, _fallback_clears, _suppress_upscaling) =
            client_content_frame_driver();
        reach_steady_frame_submission(&mut adapter);
        adapter
            .begin_transition(vec![KmsRenderCommand::Suspend { generation: 2 }])
            .expect("fatality test suspend begins");
        let mut render_world = World::new();
        adapter
            .app
            .as_mut()
            .expect("fatality test owns App")
            .insert_resource(FirstLiveRenderError::default());
        let failure = RenderError {
            ty: bevy::render::error_handler::ErrorType::Validation,
            description: "forced pre-quiescence render failure".into(),
            source: None,
        };
        assert!(matches!(
            stop_live_rendering_after_first_error(
                &failure,
                adapter
                    .app
                    .as_mut()
                    .expect("fatality test owns App")
                    .world_mut(),
                &mut render_world,
            ),
            RenderErrorPolicy::StopRendering
        ));
        let error = adapter
            .transition_update()
            .expect_err("a render failure before quiescence must remain fatal");
        assert!(
            error
                .to_string()
                .contains("forced pre-quiescence render failure")
        );
        assert_eq!(adapter.update_gate, LiveRenderUpdateGate::Open);
        adapter
            .shutdown_inner()
            .expect("fatality-test frame driver shuts down");
    }

    #[test]
    fn manual_placeholder_extent_comes_from_the_actual_view() {
        let output = blocked_output();
        let placeholder = KmsRenderPlaceholder {
            extent: (output.display.mode.width, output.display.mode.height),
            logical_extent: selected_logical_extent(&output),
            view: Some(noop_manual_view(640, 480, "wrong-sized placeholder")),
        };
        let mut registrar = RenderSourceRegistrar::default();
        registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                super::super::kms::KmsRenderCommand::AddOutput {
                    generation: 1,
                    output: output.clone(),
                },
            ))
            .expect("add output accepted");

        let error = match registrar.apply(KmsRenderWorkerEvent::SourceReady {
            generation: 1,
            output: output.clone(),
            source: RenderSource {
                placeholder,
                acquire: Box::new(|| Err("unused".into())),
            },
        }) {
            Ok(_) => panic!("the actual placeholder extent must be checked"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            RenderSourceRegistrarError::PlaceholderSizeMismatch {
                generation: 1,
                key: output.key,
                expected: (320, 240),
                actual: (640, 480),
            }
        );
    }

    #[test]
    fn later_frame_extent_mismatch_is_a_terminal_render_failure() {
        let output = blocked_output();
        let key = output.key.clone();
        let (worker_events, _worker_event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            worker_events,
        )
        .expect("render worker starts");
        let worker_stop = worker.stop_handle();
        let lifecycle = worker_stop.render_lifecycle();
        let (frame_event_sender, frame_events) = mpsc::channel();
        let wrong_view = noop_manual_view(640, 480, "wrong-sized later frame");
        let handle = ManualTextureViewHandle(81);
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.lifecycle = Arc::clone(&lifecycle);
        targets.worker_stop = Some(worker_stop);
        targets.frame_events = Some(frame_event_sender);
        targets.sources.insert(
            key.clone(),
            OutputFrameSource {
                generation: 9,
                handle,
                extent: (320, 240),
                acquire: Box::new(move || {
                    Ok(AcquiredOutputFrame {
                        view: wrong_view.clone(),
                        present: Box::new(|| panic!("wrong-sized frame was presented")),
                    })
                }),
                ready_generation: Some(9),
                current_ready_generation: Some(9),
                pending_present: None,
            },
        );
        let mut views = ManualTextureViews::default();
        views.insert(
            handle,
            noop_manual_view(320, 240, "registered frame extent"),
        );
        let mut world = World::new();
        world.insert_resource(targets);
        world.insert_resource(views);

        world.run_system_once(acquire_output_frames).unwrap();

        assert!(matches!(
            lifecycle.state(),
            KmsRenderLifecycleState::Terminating | KmsRenderLifecycleState::Terminated
        ));
        assert!(
            !world
                .resource::<KmsRenderTargets>()
                .sources
                .contains_key(&key)
        );
        assert!(!world.resource::<ManualTextureViews>().contains_key(&handle));
        let failure = match frame_events
            .recv_timeout(Duration::from_secs(2))
            .expect("terminal frame event")
        {
            KmsRenderFrameEvent::TerminalFailure(failure) => failure,
            event => panic!("unexpected frame event: {event:?}"),
        };
        assert_eq!(failure.failure.code, "kms-frame-view-size-mismatch");
        assert_eq!(failure.generation, 9);
        assert_eq!(failure.key, Some(key));
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderPathDisconnected(exit))
                if exit.failure.code == "kms-frame-view-size-mismatch"
        ));
    }

    #[test]
    fn frame_submitted_event_is_emitted_only_after_the_present_call_runs() {
        let key = blocked_output().key;
        let handle = ManualTextureViewHandle(82);
        let presented = Arc::new(AtomicBool::new(false));
        let presented_probe = Arc::clone(&presented);
        let view = noop_manual_view(320, 240, "present event frame");
        let (frame_event_sender, frame_events) = mpsc::channel();
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.frame_events = Some(frame_event_sender);
        targets.sources.insert(
            key.clone(),
            OutputFrameSource {
                generation: 10,
                handle,
                extent: (320, 240),
                acquire: Box::new(move || {
                    Ok(AcquiredOutputFrame {
                        view: view.clone(),
                        present: Box::new({
                            let presented_probe = Arc::clone(&presented_probe);
                            move || presented_probe.store(true, Ordering::SeqCst)
                        }),
                    })
                }),
                ready_generation: Some(10),
                current_ready_generation: Some(10),
                pending_present: None,
            },
        );
        let mut world = World::new();
        world.insert_resource(targets);
        world.insert_resource(ManualTextureViews::default());

        world.run_system_once(acquire_output_frames).unwrap();
        assert!(!presented.load(Ordering::SeqCst));
        assert!(matches!(frame_events.try_recv(), Err(TryRecvError::Empty)));
        present_selected_output_frames(
            &mut world.resource_mut::<KmsRenderTargets>(),
            &[ExtractedOutputView {
                key: key.clone(),
                generation: 10,
                handle,
                ready: true,
                written: true,
            }],
        );

        assert!(presented.load(Ordering::SeqCst));
        assert_eq!(
            frame_events.recv_timeout(Duration::from_secs(1)).unwrap(),
            KmsRenderFrameEvent::FrameSubmitted {
                generation: 10,
                key,
            }
        );
    }

    #[test]
    fn cancelled_present_maps_to_a_typed_updated_marker() {
        let key = blocked_output().key;
        let handle = ManualTextureViewHandle(83);
        let deadline_instant = Instant::now() + Duration::from_secs(1);
        let deadline = PresentDeadline::bounded(deadline_instant);
        let observed_deadline = Arc::new(Mutex::new(None));
        let present_deadline = Arc::clone(&observed_deadline);
        let view = noop_manual_view(320, 240, "cancelled present frame");
        let (frame_event_sender, frame_events) = mpsc::channel();
        let mut targets = KmsRenderTargets::new(deadline);
        targets.frame_events = Some(frame_event_sender);
        targets.sources.insert(
            key.clone(),
            OutputFrameSource {
                generation: 11,
                handle,
                extent: (320, 240),
                acquire: Box::new(move || {
                    Ok(AcquiredOutputFrame {
                        view: view.clone(),
                        present: fallible_present_output_frame({
                            let present_deadline = Arc::clone(&present_deadline);
                            move |deadline| {
                                *present_deadline.lock().expect("deadline probe lock") =
                                    Some(deadline);
                                Ok(PresentOutcome::Cancelled)
                            }
                        }),
                    })
                }),
                ready_generation: Some(11),
                current_ready_generation: Some(11),
                pending_present: None,
            },
        );
        let mut world = World::new();
        world.insert_resource(targets);
        world.insert_resource(ManualTextureViews::default());

        world.run_system_once(acquire_output_frames).unwrap();
        present_selected_output_frames(
            &mut world.resource_mut::<KmsRenderTargets>(),
            &[ExtractedOutputView {
                key,
                generation: 11,
                handle,
                ready: true,
                written: true,
            }],
        );

        assert_eq!(
            *observed_deadline.lock().expect("deadline probe lock"),
            Some(deadline)
        );
        assert_eq!(deadline.instant(), Some(deadline_instant));

        struct DrainedPresentEvents(Receiver<KmsRenderFrameEvent>);

        impl LivePumpUpdater for DrainedPresentEvents {
            fn update_for_pump(
                &mut self,
            ) -> Result<Vec<KmsRenderFrameEvent>, crate::backend::kms_live::KmsLiveError>
            {
                Ok(self.0.try_iter().collect())
            }
        }

        let (reply, failed) = live_pump_update_reply(&mut DrainedPresentEvents(frame_events));
        assert!(!failed);
        assert!(matches!(
            reply,
            PumpReply::Updated(Ok(events))
                if matches!(events.as_slice(), [KmsRenderFrameEvent::PresentationCancelled {
                    generation: 11,
                    key: cancelled_key,
                }] if *cancelled_key == blocked_output().key)
        ));
    }

    #[test]
    fn authority_class_present_failure_is_reply_carried_without_stopping_worker() {
        let output = blocked_output();
        let key = output.key.clone();
        let (worker_events, _worker_event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            worker_events,
        )
        .expect("render worker starts");
        let worker_stop = worker.stop_handle();
        let lifecycle = worker_stop.render_lifecycle();
        let (frame_event_sender, frame_events) = mpsc::channel();
        let handle = ManualTextureViewHandle(84);
        let view = noop_manual_view(320, 240, "authority failure frame");
        let mut targets = KmsRenderTargets::new(PresentDeadline::bounded(
            Instant::now() + Duration::from_secs(1),
        ));
        targets.lifecycle = Arc::clone(&lifecycle);
        targets.worker_stop = Some(worker_stop.clone());
        targets.frame_events = Some(frame_event_sender);
        targets.sources.insert(
            key.clone(),
            OutputFrameSource {
                generation: 61,
                handle,
                extent: (320, 240),
                acquire: Box::new(move || {
                    Ok(AcquiredOutputFrame {
                        view: view.clone(),
                        present: fallible_present_output_frame(move |_| {
                            Err(KmsRenderPlatformFailure::terminal(
                                "kms-live-atomic-commit-hard-rejection",
                                "atomic commit ioctl failed with errno 13: Permission denied",
                            ))
                        }),
                    })
                }),
                ready_generation: Some(61),
                current_ready_generation: Some(61),
                pending_present: None,
            },
        );
        let mut world = World::new();
        world.insert_resource(targets);
        world.insert_resource(ManualTextureViews::default());

        world.run_system_once(acquire_output_frames).unwrap();
        present_selected_output_frames(
            &mut world.resource_mut::<KmsRenderTargets>(),
            &[ExtractedOutputView {
                key: key.clone(),
                generation: 61,
                handle,
                ready: true,
                written: true,
            }],
        );

        let failure = match frame_events
            .recv_timeout(Duration::from_secs(1))
            .expect("authority failure is reply-carried")
        {
            KmsRenderFrameEvent::TerminalFailure(failure) => failure,
            event => panic!("unexpected frame event: {event:?}"),
        };
        assert_eq!(
            failure.failure.atomic_commit_authority_errno(),
            Some(libc::EACCES)
        );
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Active);
        assert!(
            world
                .resource::<KmsRenderTargets>()
                .sources
                .contains_key(&key),
            "the source survives until the pause Suspend destroys it"
        );

        worker_stop.begin_shutdown();
        worker_stop.wake();
        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );
    }

    #[test]
    fn noop_cancel_handle_is_inert_and_fake_records_scope() {
        PresentationCancelHandle::noop_for_test().cancel(CancelScope::AllGenerations);

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::clone(&recorded);
        let handle = PresentationCancelHandle::fake(move |scope| {
            probe.lock().expect("cancel probe lock").push(scope);
        });
        handle.cancel(CancelScope::Generation(19));

        assert_eq!(
            *recorded.lock().expect("cancel probe lock"),
            [CancelScope::Generation(19)]
        );
    }

    #[test]
    fn steady_state_atomic_flip_keeps_its_250ms_deadline_after_gpu_completion() {
        let gpu_wait_started = Instant::now();
        let gpu_completed = gpu_wait_started + Duration::from_millis(240);
        let deadline = atomic_present_deadline(gpu_completed, false)
            .instant()
            .expect("atomic flip deadline is bounded");
        assert_eq!(
            deadline.duration_since(gpu_completed),
            ATOMIC_PRESENT_TIMEOUT
        );
        assert_eq!(
            deadline.duration_since(gpu_wait_started),
            Duration::from_millis(240) + ATOMIC_PRESENT_TIMEOUT
        );
    }

    #[test]
    fn modeset_carrying_atomic_commit_gets_the_named_modeset_deadline() {
        let gpu_completed = Instant::now();
        let deadline = atomic_present_deadline(gpu_completed, true)
            .instant()
            .expect("atomic modeset deadline is bounded");

        assert_eq!(
            deadline.duration_since(gpu_completed),
            ATOMIC_MODESET_TIMEOUT
        );
        assert!(ATOMIC_MODESET_TIMEOUT > ATOMIC_PRESENT_TIMEOUT);
    }

    #[test]
    fn seamless_failure_drain_and_fallback_fit_original_stage_budget() {
        let started = Instant::now();
        let overall = started + ATOMIC_PRESENT_TIMEOUT.saturating_mul(3);
        assert_eq!(
            SEAMLESS_RESUME_MINIMUM_BUDGET,
            ATOMIC_PRESENT_TIMEOUT.saturating_mul(3)
        );
        assert!(seamless_resume_has_minimum_budget(started, overall));
        assert!(!seamless_resume_has_minimum_budget(
            started,
            started + Duration::from_millis(300)
        ));

        // Start the fake clock before retained GBM/Vulkan import and presenter
        // admission. Both consume the original stage rather than creating a
        // fresh deadline.
        let after_retained_import = started + Duration::from_millis(50);
        let admission_deadline = atomic_admission_deadline(after_retained_import, Some(overall))
            .expect("admission begins inside the staged deadline");
        assert_eq!(
            admission_deadline.duration_since(after_retained_import),
            ATOMIC_PRESENT_TIMEOUT
        );
        let late_admission = overall - Duration::from_millis(100);
        assert_eq!(
            atomic_admission_deadline(late_admission, Some(overall)),
            Some(overall),
            "presenter admission is min(local timeout, staged deadline)"
        );

        // Advance through successful retained admission before attempting the
        // optional same-mode flip.
        let after_admission = started + Duration::from_millis(100);

        let seamless_deadline = optional_atomic_resume_stage_deadline(after_admission, overall, 2)
            .expect("three-stage budget admits seamless attempt");
        assert_eq!(
            seamless_deadline,
            started + ATOMIC_PRESENT_TIMEOUT,
            "pre-attempt work consumes the seamless slice rather than shifting it"
        );

        // Advance the fake clock to the end of the failed seamless attempt.
        let after_seamless_failure = seamless_deadline;
        let drain_deadline =
            optional_atomic_resume_stage_deadline(after_seamless_failure, overall, 1)
                .expect("one drain slice still preserves fallback");
        assert_eq!(
            drain_deadline.duration_since(after_seamless_failure),
            ATOMIC_PRESENT_TIMEOUT
        );

        // Advance again: the remaining stage is an untouched fallback slice.
        let after_drain = drain_deadline;
        assert_eq!(overall.duration_since(after_drain), ATOMIC_PRESENT_TIMEOUT);
        assert!(optional_atomic_resume_stage_deadline(after_drain, overall, 1).is_none());
    }

    #[test]
    fn quiescence_dropped_unpresented_closure_settles_its_slot_after_retirement() {
        let recorded = Arc::new(Mutex::new(BTreeSet::new()));
        let drop_record = Arc::clone(&recorded);
        let guard = UnpresentedFrameGuard::new(ScanoutSlotId(1), move |slot| {
            drop_record
                .lock()
                .expect("unpresented drop ledger")
                .insert(slot);
        });
        let unpresented = fallible_present_output_frame(move |_| {
            let mut guard = guard;
            guard.disarm();
            Ok(PresentOutcome::Displayed)
        });

        // Render-world quiescence drops this closure without invoking it.
        drop(unpresented);
        let mut pending = std::mem::take(&mut *recorded.lock().expect("drop ledger"));
        assert_eq!(pending, BTreeSet::from([ScanoutSlotId(1)]));

        let mut settled = Vec::new();
        settle_unpresented_after_retirement(&mut pending, |slot| {
            settled.push(slot);
            Ok::<_, ()>(())
        })
        .expect("global retirement permits settlement");
        assert!(pending.is_empty());
        assert_eq!(settled, [ScanoutSlotId(1)]);
    }

    fn assert_retained_ledger_guard_releases_once(dispose: impl FnOnce(RetainedBufferLedgerGuard)) {
        let ledger = super::super::kms_live::LiveTargetPairingLedger::default();
        let guard = RetainedBufferLedgerGuard::new(17, ledger.clone());
        assert_eq!(
            ledger.retained_snapshot(17),
            super::super::kms_live::LiveRetainedBufferPairingCounts {
                created: 1,
                released: 0,
                pending_handoffs: 0,
            }
        );
        dispose(guard);
        assert_eq!(
            ledger.retained_snapshot(17),
            super::super::kms_live::LiveRetainedBufferPairingCounts {
                created: 1,
                released: 1,
                pending_handoffs: 0,
            }
        );
    }

    #[test]
    fn seamless_consumed_retained_buffer_releases_ledger_exactly_once() {
        let ledger = super::super::kms_live::LiveTargetPairingLedger::default();
        let mut guard = RetainedBufferLedgerGuard::new(17, ledger.clone());
        guard.mark_pending_handoff();
        assert_eq!(
            ledger.retained_snapshot(17),
            super::super::kms_live::LiveRetainedBufferPairingCounts {
                created: 1,
                released: 0,
                pending_handoffs: 1,
            }
        );
        {
            let mut handoff = Some(guard);
            drop(handoff.take());
            drop(handoff);
        }
        assert_eq!(
            ledger.retained_snapshot(17),
            super::super::kms_live::LiveRetainedBufferPairingCounts {
                created: 1,
                released: 1,
                pending_handoffs: 0,
            }
        );
    }

    #[test]
    fn modeset_fallback_discards_retained_buffer_exactly_once() {
        assert_retained_ledger_guard_releases_once(drop);
    }

    #[test]
    fn paused_teardown_discards_retained_buffer_exactly_once() {
        assert_retained_ledger_guard_releases_once(|guard| {
            let ownership = vec![guard];
            drop(ownership);
        });
    }

    #[test]
    fn vanished_retained_output_releases_its_ledger_entry_once() {
        let ledger = super::super::kms_live::LiveTargetPairingLedger::default();
        let mut retained = BTreeMap::from([
            (
                "HDMI-A-1",
                RetainedBufferLedgerGuard::new(17, ledger.clone()),
            ),
            ("DP-1", RetainedBufferLedgerGuard::new(19, ledger.clone())),
        ]);

        retained.retain(|connector, _| *connector == "DP-1");
        assert_eq!(ledger.retained_snapshot(17).released, 1);
        assert_eq!(ledger.retained_snapshot(19).released, 0);
        drop(retained);
        assert_eq!(ledger.retained_snapshot(19).released, 1);
    }

    #[test]
    fn every_resume_classifier_reason_keeps_full_modeset_policy() {
        use super::super::resume_scanout::{ResumeModesetReason, ResumePresentationClassification};

        for reason in [
            ResumeModesetReason::GenerationMismatch,
            ResumeModesetReason::InactiveCrtc,
            ResumeModesetReason::RouteMismatch,
            ResumeModesetReason::ModeMismatch,
            ResumeModesetReason::PlaneGeometryOrFormatMismatch,
            ResumeModesetReason::NoUsableState,
        ] {
            assert!(!seamless_resume_is_eligible(
                Some(ResumePresentationPlan {
                    classification: ResumePresentationClassification::ModesetRequired(reason),
                    deadline: PresentDeadline::bounded(Instant::now() + Duration::from_secs(1),),
                }),
                true,
                true,
            ));
        }
        assert!(seamless_resume_is_eligible(
            Some(ResumePresentationPlan {
                classification: ResumePresentationClassification::SeamlessPageFlip,
                deadline: PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            }),
            true,
            true,
        ));
        assert!(!seamless_resume_is_eligible(
            Some(ResumePresentationPlan {
                classification: ResumePresentationClassification::SeamlessPageFlip,
                deadline: PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            }),
            true,
            false,
        ));
    }

    #[test]
    fn unwritten_fifo_image_is_never_reacquired_dropped_or_presented_before_a_fresh_write() {
        #[derive(Default)]
        struct SwapchainOracle {
            acquired: AtomicUsize,
            written: AtomicUsize,
            presented: AtomicUsize,
            abandoned: AtomicUsize,
        }

        struct OutstandingImage {
            oracle: Arc<SwapchainOracle>,
            presented: AtomicBool,
        }

        impl OutstandingImage {
            fn mark_presented(&self) {
                self.presented.store(true, Ordering::SeqCst);
            }
        }

        impl Drop for OutstandingImage {
            fn drop(&mut self) {
                if !self.presented.load(Ordering::SeqCst) {
                    self.oracle.abandoned.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        let key = blocked_output().key;
        let handle = ManualTextureViewHandle(101);
        let oracle = Arc::new(SwapchainOracle::default());
        let acquire_oracle = Arc::clone(&oracle);
        let view = noop_manual_view(320, 240, "held FIFO frame");
        let (frame_event_sender, frame_events) = mpsc::channel();
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.frame_events = Some(frame_event_sender);
        targets.sources.insert(
            key.clone(),
            OutputFrameSource {
                generation: 8,
                handle,
                extent: (320, 240),
                acquire: Box::new(move || {
                    acquire_oracle.acquired.fetch_add(1, Ordering::SeqCst);
                    let outstanding = OutstandingImage {
                        oracle: Arc::clone(&acquire_oracle),
                        presented: AtomicBool::new(false),
                    };
                    let present_oracle = Arc::clone(&acquire_oracle);
                    Ok(AcquiredOutputFrame {
                        view: view.clone(),
                        present: Box::new(move || {
                            assert!(
                                present_oracle.written.load(Ordering::SeqCst)
                                    > present_oracle.presented.load(Ordering::SeqCst),
                                "an acquired image was presented before a fresh write"
                            );
                            outstanding.mark_presented();
                            present_oracle.presented.fetch_add(1, Ordering::SeqCst);
                        }),
                    })
                }),
                ready_generation: None,
                current_ready_generation: None,
                pending_present: None,
            },
        );
        let mut world = World::new();
        world.insert_resource(targets);
        world.insert_resource(ManualTextureViews::default());

        for _ in 0..3 {
            world.run_system_once(acquire_output_frames).unwrap();
        }
        assert_eq!(oracle.acquired.load(Ordering::SeqCst), 0);

        present_selected_output_frames(
            &mut world.resource_mut::<KmsRenderTargets>(),
            &[ExtractedOutputView {
                key: key.clone(),
                generation: 7,
                handle,
                ready: true,
                written: true,
            }],
        );
        world.run_system_once(acquire_output_frames).unwrap();
        assert_eq!(oracle.acquired.load(Ordering::SeqCst), 0);

        present_selected_output_frames(
            &mut world.resource_mut::<KmsRenderTargets>(),
            &[ExtractedOutputView {
                key: key.clone(),
                generation: 8,
                handle,
                ready: true,
                written: false,
            }],
        );
        world.run_system_once(acquire_output_frames).unwrap();
        assert_eq!(oracle.acquired.load(Ordering::SeqCst), 0);
        world
            .resource_mut::<KmsRenderTargets>()
            .sources
            .get_mut(&key)
            .expect("current source")
            .current_ready_generation = Some(8);
        world.run_system_once(acquire_output_frames).unwrap();
        assert_eq!(oracle.acquired.load(Ordering::SeqCst), 1);

        for _ in 0..3 {
            present_selected_output_frames(
                &mut world.resource_mut::<KmsRenderTargets>(),
                &[ExtractedOutputView {
                    key: key.clone(),
                    generation: 8,
                    handle,
                    ready: true,
                    written: false,
                }],
            );
            world.run_system_once(acquire_output_frames).unwrap();
        }
        assert_eq!(oracle.acquired.load(Ordering::SeqCst), 1);
        assert_eq!(oracle.abandoned.load(Ordering::SeqCst), 0);
        assert_eq!(oracle.presented.load(Ordering::SeqCst), 0);

        oracle.written.fetch_add(1, Ordering::SeqCst);
        present_selected_output_frames(
            &mut world.resource_mut::<KmsRenderTargets>(),
            &[ExtractedOutputView {
                key: key.clone(),
                generation: 8,
                handle,
                ready: true,
                written: true,
            }],
        );

        assert_eq!(oracle.acquired.load(Ordering::SeqCst), 1);
        assert_eq!(oracle.written.load(Ordering::SeqCst), 1);
        assert_eq!(oracle.presented.load(Ordering::SeqCst), 1);
        assert_eq!(oracle.abandoned.load(Ordering::SeqCst), 0);
        assert_eq!(
            frame_events.recv_timeout(Duration::from_secs(1)).unwrap(),
            KmsRenderFrameEvent::FrameSubmitted { generation: 8, key }
        );
    }

    impl AcquireBarrier {
        fn enter_and_wait(&self) {
            let (state, wake) = &*self.0;
            let mut state = state.lock().expect("acquire barrier lock");
            state.entered = true;
            wake.notify_all();
            while !state.released {
                state = wake.wait(state).expect("acquire barrier wait");
            }
        }

        fn wait_until_entered(&self, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            let (state, wake) = &*self.0;
            let mut state = state.lock().expect("acquire barrier lock");
            while !state.entered {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "fake acquire did not enter before deadline"
                );
                let (next, result) = wake
                    .wait_timeout(state, remaining)
                    .expect("acquire barrier wait");
                state = next;
                assert!(
                    !result.timed_out() || state.entered,
                    "fake acquire did not enter before deadline"
                );
            }
        }

        fn release(&self) {
            let (state, wake) = &*self.0;
            state.lock().expect("acquire barrier lock").released = true;
            wake.notify_all();
        }
    }

    struct BlockingAcquirePlatform {
        barrier: AcquireBarrier,
    }

    struct BlockingPresentPlatform {
        barrier: AcquireBarrier,
    }

    struct BlockingTeardownPlatform {
        barrier: AcquireBarrier,
    }

    struct DropOrderPlatform {
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    #[derive(Resource)]
    struct WorldDropProbe {
        event: &'static str,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Drop for WorldDropProbe {
        fn drop(&mut self) {
            self.events.lock().expect("drop-order log").push(self.event);
        }
    }

    type HandoffOperationLog = Arc<Mutex<Vec<(KmsRenderOperation, Option<OutputKey>)>>>;

    struct HandoffPlatform {
        add_called: Arc<AtomicBool>,
        operations: HandoffOperationLog,
    }

    struct PanicAfterRegisteredPlatform {
        panic_connector_id: u32,
        acquire_called: Arc<AtomicBool>,
    }

    struct FailingResumeAfterRegisteredPlatform {
        acquire_called: Arc<AtomicBool>,
    }

    impl HandoffPlatform {
        fn record(&self, operation: KmsRenderOperation, key: Option<OutputKey>) {
            self.operations
                .lock()
                .expect("handoff operation log")
                .push((operation, key));
        }

        fn source(output: &SelectedOutput) -> RenderSource<KmsRenderPlaceholder> {
            RenderSource {
                placeholder: KmsRenderPlaceholder {
                    extent: (output.display.mode.width, output.display.mode.height),
                    logical_extent: selected_logical_extent(output),
                    view: None,
                },
                acquire: Box::new(|| Err("handoff test has no frame".into())),
            }
        }

        fn source_with_noop_view(output: &SelectedOutput) -> RenderSource<KmsRenderPlaceholder> {
            let (device, _queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
            let size = wgpu::Extent3d {
                width: output.display.mode.width,
                height: output.display.mode.height,
                depth_or_array_layers: 1,
            };
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("KMS main-world cleanup test texture"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            RenderSource {
                placeholder: KmsRenderPlaceholder {
                    extent: (size.width, size.height),
                    logical_extent: selected_logical_extent(output),
                    view: Some(ManualTextureView::with_default_format(
                        texture
                            .create_view(&wgpu::TextureViewDescriptor::default())
                            .into(),
                        bevy::prelude::UVec2::new(size.width, size.height),
                    )),
                },
                acquire: Box::new(|| Err("noop cleanup test has no frame".into())),
            }
        }

        fn source_with_noop_view_and_probe(
            output: &SelectedOutput,
            acquire_called: Arc<AtomicBool>,
        ) -> RenderSource<KmsRenderPlaceholder> {
            let mut source = Self::source_with_noop_view(output);
            source.acquire = Box::new(move || {
                acquire_called.store(true, Ordering::SeqCst);
                Err("instrumented terminal test source has no frame".into())
            });
            source
        }
    }

    impl KmsRenderPlatform for HandoffPlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::Suspend, None);
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::Resume, None);
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.add_called.store(true, Ordering::SeqCst);
            self.record(KmsRenderOperation::AddOutput, Some(output.key.clone()));
            Ok(Self::source(output))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::ChangeOutput, Some(output.key.clone()));
            Ok(Self::source(output))
        }

        fn remove_output(&mut self, key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::RemoveOutput, Some(key.clone()));
            Ok(())
        }
    }

    impl KmsRenderPlatform for PanicAfterRegisteredPlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            if output.connector_id == self.panic_connector_id {
                panic!("injected KMS render-worker panic");
            }
            Ok(HandoffPlatform::source_with_noop_view_and_probe(
                output,
                Arc::clone(&self.acquire_called),
            ))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.add_output(output)
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
    }

    impl KmsRenderPlatform for FailingResumeAfterRegisteredPlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Err(KmsRenderPlatformFailure::new(
                "injected-resume-failure",
                "injected failure after a source was registered",
            ))
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(HandoffPlatform::source_with_noop_view_and_probe(
                output,
                Arc::clone(&self.acquire_called),
            ))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.add_output(output)
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
    }

    impl KmsRenderPlatform for BlockingAcquirePlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            let barrier = self.barrier.clone();
            Ok(RenderSource {
                placeholder: KmsRenderPlaceholder {
                    extent: (output.display.mode.width, output.display.mode.height),
                    logical_extent: selected_logical_extent(output),
                    view: None,
                },
                acquire: Box::new(move || {
                    barrier.enter_and_wait();
                    Err("blocked fake frame released".into())
                }),
            })
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.add_output(output)
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
    }

    impl KmsRenderPlatform for BlockingPresentPlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            let mut source = HandoffPlatform::source_with_noop_view(output);
            let view = source.placeholder.view.as_ref().expect("noop view").clone();
            let barrier = self.barrier.clone();
            source.acquire = Box::new(move || {
                let present_barrier = barrier.clone();
                Ok(AcquiredOutputFrame {
                    view: view.clone(),
                    present: Box::new(move || present_barrier.enter_and_wait()),
                })
            });
            Ok(source)
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.add_output(output)
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
    }

    impl KmsRenderPlatform for BlockingTeardownPlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(HandoffPlatform::source(output))
        }
        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(HandoffPlatform::source(output))
        }
        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
        fn teardown(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            self.barrier.enter_and_wait();
            Ok(())
        }
    }

    impl KmsRenderPlatform for DropOrderPlatform {
        type Placeholder = KmsRenderPlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(HandoffPlatform::source(output))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(HandoffPlatform::source(output))
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn teardown(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            self.events.lock().expect("drop-order log").push("platform");
            Ok(())
        }
    }

    fn blocked_output() -> SelectedOutput {
        let mode = ConnectorMode {
            width: 320,
            height: 240,
            refresh_millihz: 60_000,
            preferred: true,
            clock_khz: 1,
            hsync: (1, 1, 1),
            vsync: (1, 1, 1),
            hskew: 0,
            vscan: 0,
            flags: 0,
        };
        SelectedOutput {
            key: OutputKey {
                device: 226,
                connector_name: "Blocked-1".into(),
            },
            connector_id: 91,
            connector_mode: mode,
            display: AtomicOutputSelection {
                connector_id: 91,
                crtc_id: 151,
                primary_plane_id: 31,
                mode,
                format: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
            },
            output_scale: crate::backend::kms::OutputScale120::ONE,
            logical_rect: LogicalRect {
                x: 0,
                y: 0,
                width: 320,
                height: 240,
            },
        }
    }

    fn noop_manual_view(width: u32, height: u32, label: &'static str) -> ManualTextureView {
        let (device, _queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        ManualTextureView::with_default_format(
            texture
                .create_view(&wgpu::TextureViewDescriptor::default())
                .into(),
            bevy::prelude::UVec2::new(width, height),
        )
    }

    fn retain_presenter(world: &mut World) -> Arc<AtomicBool> {
        let present_called = Arc::new(AtomicBool::new(false));
        let present_flag = Arc::clone(&present_called);
        let mut targets = world.resource_mut::<KmsRenderTargets>();
        let source = targets
            .sources
            .values_mut()
            .next()
            .expect("retained presenter has a registered source");
        source.pending_present = Some(Box::new(move || {
            present_flag.store(true, Ordering::SeqCst);
        }));
        present_called
    }

    fn assert_terminal_barrier_suppresses_retained_callbacks(
        world: &mut World,
        key: &OutputKey,
        acquire_called: &AtomicBool,
        present_called: &AtomicBool,
    ) {
        assert!(
            world
                .resource::<KmsRenderTargets>()
                .sources
                .contains_key(key),
            "the disconnected terminal command path must leave the fail-safe something to guard"
        );
        world
            .run_system_once(present_output_frames)
            .expect("terminal presentation guard runs");
        assert!(
            !present_called.load(Ordering::SeqCst),
            "the shared terminal barrier must suppress a retained presenter"
        );
        world
            .run_system_once(acquire_output_frames)
            .expect("terminal acquisition guard runs");
        assert!(
            !acquire_called.load(Ordering::SeqCst),
            "the shared terminal barrier must suppress a retained acquisition callback"
        );
        assert!(
            !world
                .resource::<KmsRenderTargets>()
                .sources
                .contains_key(key),
            "the terminal acquisition guard must discard retained sources"
        );
    }

    pub(crate) fn while_real_app_acquire_is_blocked(
        runtime: WaylandRuntime,
        probe: impl FnOnce() + Send + 'static,
    ) {
        let barrier = AcquireBarrier::default();
        while_real_app_render_is_blocked(
            runtime,
            BlockingAcquirePlatform {
                barrier: barrier.clone(),
            },
            barrier,
            false,
            probe,
        );
    }

    pub(crate) fn while_real_app_present_is_blocked(
        runtime: WaylandRuntime,
        probe: impl FnOnce() + Send + 'static,
    ) {
        let barrier = AcquireBarrier::default();
        while_real_app_render_is_blocked(
            runtime,
            BlockingPresentPlatform {
                barrier: barrier.clone(),
            },
            barrier,
            true,
            probe,
        );
    }

    fn while_real_app_render_is_blocked<T>(
        runtime: WaylandRuntime,
        platform: T,
        barrier: AcquireBarrier,
        present_acquired_as_written: bool,
        probe: impl FnOnce() + Send + 'static,
    ) where
        T: KmsRenderPlatform<Placeholder = KmsRenderPlaceholder>,
    {
        let mut app = App::new();
        let mut render_app = SubApp::new();
        render_app.init_schedule(Render);
        render_app.add_systems(Render, prepare_view_attachments.run_if(|| false));
        render_app.add_systems(Render, render_system.run_if(|| false));
        render_app
            .world_mut()
            .insert_resource(ManualTextureViews::default());
        app.insert_sub_app(RenderApp, render_app);
        app.world_mut()
            .insert_resource(ManualTextureViews::default());
        app.insert_resource(runtime);
        install_kms_render_target(&mut app, platform);
        app.sub_app_mut(RenderApp).add_systems(
            Render,
            mark_all_sources_ready_for_test
                .after(refresh_output_readiness)
                .before(acquire_output_frames),
        );
        if present_acquired_as_written {
            app.sub_app_mut(RenderApp).add_systems(
                Render,
                present_all_acquired_for_test
                    .after(acquire_output_frames)
                    .before(present_output_frames),
            );
        }

        let output = blocked_output();
        let key = output.key.clone();
        send_render_commands(
            app.world(),
            vec![KmsRenderCommand::AddOutput {
                generation: 1,
                output,
            }],
        )
        .expect("queue blocked output through the real worker");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            app.world_mut().run_schedule(First);
            if app
                .world()
                .resource::<KmsRegistrarInbox>()
                .registrar
                .registered(&key)
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "source did not traverse the registrar before deadline"
            );
            thread::yield_now();
        }

        let probe_barrier = barrier.clone();
        let client = thread::Builder::new()
            .name("cosmix-kms-render-client-test".into())
            .spawn(move || {
                probe_barrier.wait_until_entered(Duration::from_secs(2));
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(probe));
                probe_barrier.release();
                result
            })
            .expect("blocked-acquire client test thread starts");

        app.sub_app_mut(RenderApp).world_mut().run_schedule(Render);
        let result = client.join().expect("blocked-acquire client thread exits");
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn mark_all_sources_ready_for_test(mut targets: bevy::prelude::ResMut<KmsRenderTargets>) {
        for source in targets.sources.values_mut() {
            source.ready_generation = Some(source.generation);
            source.current_ready_generation = Some(source.generation);
        }
    }

    fn present_all_acquired_for_test(mut targets: bevy::prelude::ResMut<KmsRenderTargets>) {
        let extracted = targets
            .sources
            .iter()
            .map(|(key, source)| ExtractedOutputView {
                key: key.clone(),
                generation: source.generation,
                handle: source.handle,
                ready: true,
                written: true,
            })
            .collect::<Vec<_>>();
        present_selected_output_frames(&mut targets, &extracted);
    }

    pub(crate) fn while_worker_teardown_is_blocked(
        runtime: WaylandRuntime,
        probe: impl FnOnce() + Send + 'static,
    ) {
        let barrier = AcquireBarrier::default();
        let (events, _receiver) = mpsc::channel();
        let (worker, render_world_dropped) = KmsRenderWorker::spawn_guarded(
            BlockingTeardownPlatform {
                barrier: barrier.clone(),
            },
            events,
        )
        .expect("guarded teardown worker starts");
        render_world_dropped.acknowledge();
        let join = thread::spawn(move || worker.finish(Duration::from_secs(30)));
        barrier.wait_until_entered(Duration::from_secs(2));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(probe));
        barrier.release();
        assert!(matches!(
            join.join().expect("teardown worker joins"),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        ));
        drop(runtime);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn system_node<Out, Marker>(
        graph: &ScheduleGraph,
        system: impl IntoSystem<(), Out, Marker>,
    ) -> NodeId {
        let system_type = IntoSystem::into_system(system).system_type();
        graph
            .systems
            .iter()
            .find(|(_, candidate, _)| candidate.system_type() == system_type)
            .map(|(key, _, _)| NodeId::System(key))
            .expect("system is present in the render schedule")
    }

    fn has_configured_ordering(graph: &ScheduleGraph, before: NodeId, after: NodeId) -> bool {
        let dependency = graph.dependency().graph();
        let hierarchy = graph.hierarchy().graph();
        dependency.contains_edge(before, after)
            || hierarchy
                .neighbors_directed(before, Direction::Incoming)
                .any(|set| dependency.contains_edge(set, after))
            || hierarchy
                .neighbors_directed(after, Direction::Incoming)
                .any(|set| dependency.contains_edge(before, set))
    }

    #[test]
    fn acquired_frame_replaces_the_stable_handle_and_is_retained_until_present() {
        let handle = ManualTextureViewHandle(91);
        let mut views = HashMap::from([(handle, "placeholder")]);
        let presented = Arc::new(AtomicBool::new(false));
        let present_flag = Arc::clone(&presented);
        views.insert(handle, "acquired");
        let mut retained = Some(
            Box::new(move || present_flag.store(true, Ordering::SeqCst)) as PresentOutputFrame
        );

        assert_eq!(views, HashMap::from([(handle, "acquired")]));
        assert!(retained.is_some());
        assert!(!presented.load(Ordering::SeqCst));
        assert_eq!(
            present_output_frame(
                retained.take().expect("retained presenter"),
                PresentDeadline::unbounded_non_presenting(),
            )
            .expect("infallible test presenter succeeds"),
            PresentOutcome::Displayed
        );
        assert!(presented.load(Ordering::SeqCst));
    }

    #[test]
    fn render_schedule_has_direct_acquire_clear_and_present_ordering_edges() {
        let mut render_app = SubApp::new();
        render_app.init_schedule(Render);
        render_app.add_systems(Render, prepare_view_attachments);
        render_app.add_systems(Render, render_system.run_if(|| false));
        let (_, render_commands) = mpsc::channel();
        let (releases, _) = KmsRenderInputSender::test_channel();
        let (quiescences, _) = KmsRenderInputSender::test_channel();
        configure_render_app(
            &mut render_app,
            render_commands,
            releases,
            quiescences,
            KmsRenderAppControl {
                lifecycle: Arc::new(KmsRenderLifecycle::new()),
                worker_stop: None,
                frame_events: None,
                destructive_quiescence: None,
            },
        );

        let schedule = render_app.get_schedule(Render).expect("render schedule");
        let graph = schedule.graph();
        assert!(
            has_configured_ordering(
                graph,
                system_node(graph, acquire_output_frames),
                system_node(graph, prepare_view_attachments),
            ),
            "acquisition must directly precede Bevy attachment preparation",
        );
        assert!(
            has_configured_ordering(
                graph,
                system_node(graph, render_system),
                system_node(graph, clear_unwritten_output_frames),
            ),
            "the fallback clear must have a direct dependency on Bevy's render system",
        );
        assert!(
            has_configured_ordering(
                graph,
                system_node(graph, clear_unwritten_output_frames),
                system_node(graph, present_output_frames),
            ),
            "presentation must have a direct dependency on the fallback clear",
        );
        assert!(
            has_configured_ordering(
                graph,
                system_node(graph, present_output_frames),
                system_node(graph, complete_render_quiescence),
            ),
            "quiescence cleanup must have a direct dependency on presentation",
        );
    }

    #[test]
    fn suspend_drain_removes_only_output_render_resources() {
        let key = blocked_output().key;
        let output_handle = ManualTextureViewHandle(111);
        let retained_handle = ManualTextureViewHandle(112);
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.sources.insert(
            key.clone(),
            OutputFrameSource {
                generation: 12,
                handle: output_handle,
                extent: (320, 240),
                acquire: Box::new(|| panic!("drained source acquired")),
                ready_generation: Some(12),
                current_ready_generation: Some(12),
                pending_present: Some(Box::new(|| panic!("drained output was presented"))),
            },
        );
        let mut views = ManualTextureViews::default();
        views.insert(
            output_handle,
            noop_manual_view(320, 240, "suspended output"),
        );
        views.insert(
            retained_handle,
            noop_manual_view(64, 64, "non-output retained view"),
        );

        drain_render_resources(RenderDrainScope::AllThrough(12), &mut targets, &mut views);

        assert!(targets.sources.is_empty());
        assert!(!views.contains_key(&output_handle));
        assert!(
            views.contains_key(&retained_handle),
            "suspend must not drain non-output image resources"
        );
    }

    #[test]
    fn virtual_time_has_no_elapsed_leap_across_a_long_pause() {
        let mut app = App::new();
        app.add_plugins(TimePlugin)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                16,
            )));
        app.update();
        let before_pause = app
            .world()
            .resource::<bevy::prelude::Time<bevy::time::Virtual>>()
            .elapsed();

        pause_virtual_time(app.world_mut());
        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::from_secs(6 * 60 * 60));
        app.update();
        {
            let paused = app
                .world()
                .resource::<bevy::prelude::Time<bevy::time::Virtual>>();
            assert_eq!(paused.delta(), Duration::ZERO);
            assert_eq!(paused.elapsed(), before_pause);
        }

        let output = blocked_output().key;
        assert!(!unpause_virtual_time_after_output_ready(
            app.world_mut(),
            &[KmsRenderReply::OutputReady {
                generation: 13,
                key: output.clone(),
            }],
            14,
            &output,
        ));
        assert!(
            app.world()
                .resource::<bevy::prelude::Time<bevy::time::Virtual>>()
                .is_paused(),
            "a stale OutputReady must not resume virtual time"
        );
        assert!(unpause_virtual_time_after_output_ready(
            app.world_mut(),
            &[KmsRenderReply::OutputReady {
                generation: 14,
                key: output.clone(),
            }],
            14,
            &output,
        ));
        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::from_millis(16));
        app.update();
        let resumed = app
            .world()
            .resource::<bevy::prelude::Time<bevy::time::Virtual>>();
        assert_eq!(resumed.delta(), Duration::from_millis(16));
        assert_eq!(resumed.elapsed() - before_pause, Duration::from_millis(16));
    }

    #[test]
    fn quiescence_publishes_closed_gate_before_worker_acknowledgement() {
        let key = OutputKey {
            device: 226,
            connector_name: "Virtual-1".into(),
        };
        let (command_sender, command_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = KmsRenderInputSender::test_channel();
        let (quiescence_sender, quiescence_receiver) = KmsRenderInputSender::test_channel();
        command_sender
            .send(RenderWorldCommand::Deactivate {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 19,
                key: key.clone(),
            })
            .expect("render command receiver");

        let mut world = bevy::prelude::World::new();
        world.insert_resource(KmsRenderCommands {
            commands: Mutex::new(command_receiver),
            releases: release_sender,
            quiescences: quiescence_sender,
        });
        let mut output = blocked_output();
        output.key = key.clone();
        let source = HandoffPlatform::source_with_noop_view(&output);
        let handle = ManualTextureViewHandle(19);
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.destructive_quiescence = Some(DestructiveQuiescenceLatch::default());
        targets.sources.insert(
            key.clone(),
            OutputFrameSource {
                generation: 18,
                handle,
                extent: (output.display.mode.width, output.display.mode.height),
                acquire: source.acquire,
                ready_generation: Some(18),
                current_ready_generation: Some(18),
                pending_present: None,
            },
        );
        let mut views = ManualTextureViews::default();
        views.insert(handle, source.placeholder.view.expect("noop view"));
        world.insert_resource(targets);
        world.insert_resource(views);
        world.run_system_once(apply_render_world_commands).unwrap();

        assert_eq!(
            release_receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("worker transition release"),
            KmsRenderRelease {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 19,
                key: Some(key),
                outcome: KmsRenderReleaseOutcome::Granted,
            }
        );
        assert!(
            world.resource::<ManualTextureViews>().contains_key(&handle),
            "the acquired view must survive until Bevy has finished this render update"
        );
        assert_eq!(
            quiescence_receiver.recv_timeout(Duration::ZERO),
            Err("timed out"),
            "quiescence is a distinct late-render proof, not part of Granted"
        );
        world.run_system_once(complete_render_quiescence).unwrap();
        assert!(
            !world.resource::<ManualTextureViews>().contains_key(&handle),
            "PostCleanup drops the acquired view before proving render quiescence"
        );
        assert_eq!(
            world
                .resource::<KmsRenderTargets>()
                .destructive_quiescence
                .as_ref()
                .expect("live-style quiescence latch installed")
                .take(),
            Some(DestructiveQuiescenceIdentity {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 19,
                key: Some(OutputKey {
                    device: 226,
                    connector_name: "Virtual-1".into(),
                }),
            }),
            "PostCleanup must publish the generation gate before the worker consumes Quiesced"
        );
        assert_eq!(
            quiescence_receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("render resources quiesced"),
            KmsRenderQuiescence {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 19,
                key: Some(OutputKey {
                    device: 226,
                    connector_name: "Virtual-1".into(),
                }),
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            }
        );
    }

    #[test]
    fn dropping_queued_render_deactivation_stops_worker_and_fails_successor() {
        let first = blocked_output();
        let mut second = blocked_output();
        second.key.connector_name = "Blocked-2".into();
        let second_key = second.key.clone();
        let add_called = Arc::new(AtomicBool::new(false));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (event_sender, event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::clone(&add_called),
                operations: Arc::clone(&operations),
            },
            event_sender,
        )
        .expect("worker starts");
        worker
            .send(KmsRenderCommand::RemoveOutput {
                generation: 20,
                key: first.key.clone(),
            })
            .expect("queue removal");
        assert!(matches!(
            event_receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::RemoveOutput { generation: 20, .. }
            ))
        ));
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 21,
                output: second,
            })
            .expect("queue successor add");

        let (command_sender, command_receiver) = mpsc::channel();
        command_sender
            .send(RenderWorldCommand::Deactivate {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 20,
                key: first.key.clone(),
            })
            .expect("queue render deactivation");
        drop(KmsRenderCommands {
            commands: Mutex::new(command_receiver),
            releases: worker.release_sender(),
            quiescences: worker.quiescence_sender(),
        });

        assert!(matches!(
            event_receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 20,
                failure: KmsRenderPlatformFailure {
                    code: "render-world-command-aborted",
                    ..
                },
                ..
            }))
        ));
        assert!(matches!(
            event_receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                operation: KmsRenderOperation::AddOutput,
                generation: 21,
                key: Some(key),
                failure: KmsRenderPlatformFailure {
                    code: "render-worker-stopped-before-command",
                    ..
                },
            })) if key == second_key
        ));
        assert!(!add_called.load(Ordering::SeqCst));
        assert!(
            !operations
                .lock()
                .expect("handoff operation log")
                .contains(&(KmsRenderOperation::RemoveOutput, Some(first.key.clone()))),
            "dropping the queued deactivation must abort the matching platform removal"
        );
        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderWorldHandoffAborted {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 20,
                key: Some(first.key),
            })
        );
    }

    #[test]
    fn terminal_render_path_failure_refuses_later_send_and_fails_buffered_successor() {
        let first = blocked_output();
        let mut second = blocked_output();
        second.key.connector_name = "Blocked-2".into();
        let second_key = second.key.clone();
        let add_called = Arc::new(AtomicBool::new(false));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (event_sender, event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::clone(&add_called),
                operations: Arc::clone(&operations),
            },
            event_sender,
        )
        .expect("real worker starts");
        let releases = worker.release_sender();
        let registrations = worker.registration_sender();
        let worker_stop = worker.stop_handle();
        worker
            .send(KmsRenderCommand::RemoveOutput {
                generation: 10,
                key: first.key.clone(),
            })
            .expect("queue failed handoff command");
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 11,
                output: second,
            })
            .expect("queue successor command");
        let (reply_sender, reply_receiver) = mpsc::channel();
        let (render_sender, render_receiver) = mpsc::channel();
        drop(render_receiver);
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender,
            releases,
            registrations,
            worker_stop,
            terminal: None,
        });
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut replies = Vec::new();
        while !(replies
            .iter()
            .any(|reply| matches!(reply, KmsRenderReply::WorkerFailed { generation: 10, .. }))
            && replies
                .iter()
                .any(|reply| matches!(reply, KmsRenderReply::WorkerFailed { generation: 11, .. })))
            && Instant::now() < deadline
        {
            apply_registrar_events(&mut world);
            replies.extend(reply_receiver.try_iter());
            thread::yield_now();
        }
        assert!(
            !add_called.load(Ordering::SeqCst),
            "terminal handoff failure must stop before the buffered successor reaches the platform"
        );
        assert!(matches!(
            replies.as_slice(),
            [
                KmsRenderReply::WorkerFailed {
                    operation: KmsRenderOperation::RemoveOutput,
                    generation: 10,
                    key: Some(first_key),
                    code: "render-world-command-disconnected",
                    ..
                },
                KmsRenderReply::WorkerFailed {
                    operation: KmsRenderOperation::AddOutput,
                    generation: 11,
                    key: Some(buffered_key),
                    code: "render-worker-stopped-before-command",
                    ..
                }
            ] if *first_key == first.key && *buffered_key == second_key
        ));
        assert!(
            !operations
                .lock()
                .expect("handoff operation log")
                .contains(&(KmsRenderOperation::RemoveOutput, Some(first.key.clone()),)),
            "the terminally failed removal must not reach the platform"
        );
        assert_eq!(
            worker.send(KmsRenderCommand::AddOutput {
                generation: 12,
                output: blocked_output(),
            }),
            Err(super::super::worker::KmsRenderSendError::WorkerStopped)
        );
        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderPathDisconnected(
                KmsRenderWorkerFailure {
                    operation: KmsRenderOperation::RemoveOutput,
                    generation: 10,
                    key: Some(first.key),
                    failure: KmsRenderPlatformFailure::new(
                        "render-world-command-disconnected",
                        "KMS render-world command receiver disconnected",
                    ),
                }
            ))
        );

        // The worker's terminal event must latch before its sender disconnects;
        // otherwise the next pump would republish the same stop as a fresh failure.
        apply_registrar_events(&mut world);
        replies.extend(reply_receiver.try_iter());
        assert_eq!(
            replies.len(),
            2,
            "a latched terminal registrar must not republish on channel disconnect"
        );
    }

    #[test]
    fn render_path_failure_ignores_already_queued_successor_source() {
        let first = blocked_output();
        let mut successor = blocked_output();
        successor.key.connector_name = "Blocked-2".into();
        successor.connector_id = 92;

        let mut registrar = RenderSourceRegistrar::default();
        registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 1,
                    output: first.clone(),
                },
            ))
            .expect("expect first output");
        let registration = registrar
            .apply(KmsRenderWorkerEvent::SourceReady {
                generation: 1,
                output: first.clone(),
                source: HandoffPlatform::source(&first),
            })
            .expect("register first output");
        assert!(matches!(
            registration.effects.as_slice(),
            [RegistrarEffect::Install { generation: 1, key, .. }] if *key == first.key
        ));
        let removal = registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::RemoveOutput {
                    generation: 2,
                    key: first.key.clone(),
                },
            ))
            .expect("expect first output removal");
        assert!(matches!(
            removal.effects.as_slice(),
            [RegistrarEffect::Deactivate {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 2,
                key,
                ..
            }] if *key == first.key
        ));

        // This is the serial worker's reachable order when a completed removal is
        // immediately followed by an add: removal does not wait for registration,
        // while the successor add stops only after publishing SourceReady.
        let (event_sender, event_receiver) = mpsc::channel();
        event_sender
            .send(KmsRenderWorkerEvent::Reply(KmsRenderReply::OutputRemoved {
                generation: 2,
                key: first.key.clone(),
            }))
            .expect("queue completed removal");
        event_sender
            .send(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 3,
                    output: successor.clone(),
                },
            ))
            .expect("queue accepted successor");
        event_sender
            .send(KmsRenderWorkerEvent::SourceReady {
                generation: 3,
                output: successor.clone(),
                source: HandoffPlatform::source(&successor),
            })
            .expect("queue successor source");

        let (worker_event_sender, _worker_event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            worker_event_sender,
        )
        .expect("worker starts");
        let worker_stop = worker.stop_handle();
        let (reply_sender, _reply_receiver) = mpsc::channel();
        let (render_sender, render_receiver) = mpsc::channel();
        drop(render_receiver);
        let (releases, _release_receiver) = KmsRenderInputSender::test_channel();
        let (registrations, _registration_receiver) = KmsRenderInputSender::test_channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar,
            render: render_sender,
            releases,
            registrations,
            worker_stop,
            terminal: None,
        });
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());

        apply_registrar_events(&mut world);

        {
            let inbox = world.resource::<KmsRegistrarInbox>();
            assert_eq!(
                inbox.terminal,
                Some(KmsRegistrarChannelError::RenderChannelDisconnected)
            );
        }

        apply_registrar_events(&mut world);

        {
            let inbox = world.resource::<KmsRegistrarInbox>();
            assert_eq!(inbox.registrar.expected_generation(&successor.key), None);
            assert!(inbox.registrar.registered(&successor.key).is_none());
            assert!(inbox.registrar.is_terminal());
            assert!(matches!(
                inbox.receiver.lock().expect("event receiver").try_recv(),
                Err(TryRecvError::Empty)
            ));
        }
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderPathDisconnected(
                KmsRenderWorkerFailure {
                    operation: KmsRenderOperation::RemoveOutput,
                    generation: 2,
                    ..
                }
            ))
        ));
    }

    fn assert_effectless_output_failure_disconnect_identity(
        command: KmsRenderCommand,
        operation: KmsRenderOperation,
        generation: u64,
        key: OutputKey,
    ) {
        let (event_sender, event_receiver) = mpsc::channel();
        event_sender
            .send(KmsRenderWorkerEvent::CommandAccepted(command))
            .expect("queue accepted operation");
        event_sender
            .send(KmsRenderWorkerEvent::Reply(KmsRenderReply::OutputFailed {
                generation,
                key: key.clone(),
                reason: "injected platform failure".into(),
            }))
            .expect("queue effectless output failure");

        let (worker_event_sender, _worker_event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            worker_event_sender,
        )
        .expect("worker starts");
        let worker_stop = worker.stop_handle();
        let (reply_sender, reply_receiver) = mpsc::channel();
        drop(reply_receiver);
        let (render_sender, _render_receiver) = mpsc::channel();
        let (releases, _release_receiver) = KmsRenderInputSender::test_channel();
        let (registrations, _registration_receiver) = KmsRenderInputSender::test_channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender,
            releases,
            registrations,
            worker_stop,
            terminal: None,
        });
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());

        apply_registrar_events(&mut world);

        assert_eq!(
            world.resource::<KmsRegistrarInbox>().terminal,
            Some(KmsRegistrarChannelError::ReplyChannelDisconnected)
        );
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderPathDisconnected(
                KmsRenderWorkerFailure {
                    operation: failed_operation,
                    generation: failed_generation,
                    key: Some(failed_key),
                    failure: KmsRenderPlatformFailure {
                        code: "registrar-reply-channel-disconnected",
                        ..
                    },
                }
            )) if failed_operation == operation
                && failed_generation == generation
                && failed_key == key
        ));
    }

    #[test]
    fn disconnected_effectless_failed_add_keeps_add_identity() {
        let output = blocked_output();
        assert_effectless_output_failure_disconnect_identity(
            KmsRenderCommand::AddOutput {
                generation: 31,
                output: output.clone(),
            },
            KmsRenderOperation::AddOutput,
            31,
            output.key,
        );
    }

    #[test]
    fn disconnected_effectless_failed_remove_keeps_remove_identity() {
        let output = blocked_output();
        assert_effectless_output_failure_disconnect_identity(
            KmsRenderCommand::RemoveOutput {
                generation: 32,
                key: output.key.clone(),
            },
            KmsRenderOperation::RemoveOutput,
            32,
            output.key,
        );
    }

    #[test]
    fn registrar_error_reply_disconnect_keeps_expected_add_identity() {
        let output = blocked_output();
        let (event_sender, event_receiver) = mpsc::channel();
        event_sender
            .send(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 33,
                    output: output.clone(),
                },
            ))
            .expect("queue accepted add");
        event_sender
            .send(KmsRenderWorkerEvent::SourceReady {
                generation: 33,
                output: output.clone(),
                source: RenderSource {
                    placeholder: KmsRenderPlaceholder {
                        extent: (1, 1),
                        logical_extent: (1, 1),
                        view: None,
                    },
                    acquire: Box::new(|| Err("unused mismatched source".into())),
                },
            })
            .expect("queue mismatched source");

        let (worker_event_sender, _worker_event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            worker_event_sender,
        )
        .expect("worker starts");
        let worker_stop = worker.stop_handle();
        let (reply_sender, reply_receiver) = mpsc::channel();
        drop(reply_receiver);
        let (render_sender, _render_receiver) = mpsc::channel();
        let (releases, _release_receiver) = KmsRenderInputSender::test_channel();
        let (registrations, registration_receiver) = KmsRenderInputSender::test_channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender,
            releases,
            registrations,
            worker_stop,
            terminal: None,
        });
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());

        apply_registrar_events(&mut world);

        assert_eq!(
            registration_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("mismatched source is rejected"),
            KmsRenderRegistration {
                generation: 33,
                key: output.key.clone(),
                disposition: KmsRenderRegistrationDisposition::Rejected,
            }
        );
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderPathDisconnected(
                KmsRenderWorkerFailure {
                    operation: KmsRenderOperation::AddOutput,
                    generation: 33,
                    key: Some(key),
                    failure: KmsRenderPlatformFailure {
                        code: "registrar-reply-channel-disconnected",
                        ..
                    },
                }
            )) if key == output.key
        ));
    }

    #[test]
    fn registration_send_failure_terminates_without_immediate_platform_rollback() {
        let output = blocked_output();
        let (event_sender, event_receiver) = mpsc::channel();
        event_sender
            .send(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 41,
                    output: output.clone(),
                },
            ))
            .expect("queue accepted output");
        event_sender
            .send(KmsRenderWorkerEvent::SourceReady {
                generation: 41,
                output: output.clone(),
                source: HandoffPlatform::source_with_noop_view(&output),
            })
            .expect("queue ready source");

        let (worker_event_sender, _worker_event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            worker_event_sender,
        )
        .expect("worker starts");
        let worker_stop = worker.stop_handle();
        let (reply_sender, _reply_receiver) = mpsc::channel();
        let (render_sender, render_receiver) = mpsc::channel();
        let (releases, _release_receiver) = KmsRenderInputSender::test_channel();
        let (registrations, registration_receiver) = KmsRenderInputSender::test_channel();
        drop(registration_receiver);
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender,
            releases,
            registrations,
            worker_stop,
            terminal: None,
        });
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());
        let handle = ManualTextureViewHandle(1);
        let seeded_view = HandoffPlatform::source_with_noop_view(&output)
            .placeholder
            .view
            .expect("noop source carries a manual texture view");
        world
            .resource_mut::<ManualTextureViews>()
            .insert(handle, seeded_view);
        let entity = world
            .spawn((
                Camera::default(),
                RenderTarget::TextureView(handle),
                KmsOutputCamera,
            ))
            .id();
        world.resource_mut::<KmsMainWorldOutputs>().0.insert(
            output.key.clone(),
            MainWorldOutput {
                entity,
                handle,
                generation: 1,
            },
        );
        assert!(world.resource::<ManualTextureViews>().contains_key(&handle));
        assert!(world.get_entity(entity).is_ok());

        apply_registrar_events(&mut world);

        {
            let inbox = world.resource::<KmsRegistrarInbox>();
            assert_eq!(
                inbox.terminal,
                Some(KmsRegistrarChannelError::RegistrationChannelDisconnected)
            );
            assert!(inbox.registrar.is_terminal());
            assert_eq!(inbox.registrar.expected_generation(&output.key), None);
            assert!(inbox.registrar.registered(&output.key).is_none());
        }
        assert!(!world.resource::<ManualTextureViews>().contains_key(&handle));
        assert!(
            !world
                .resource::<KmsMainWorldOutputs>()
                .0
                .contains_key(&output.key)
        );
        assert!(world.get_entity(entity).is_err());
        assert!(matches!(
            render_receiver.recv_timeout(Duration::from_secs(2)),
            Ok(RenderWorldCommand::Install {
                generation: 41,
                key,
                ..
            }) if key == output.key
        ));
        assert!(matches!(
            render_receiver.recv_timeout(Duration::from_secs(2)),
            Ok(RenderWorldCommand::Terminate)
        ));
        assert!(matches!(
            render_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert_eq!(
            worker.send(KmsRenderCommand::Resume { generation: 42 }),
            Err(super::super::worker::KmsRenderSendError::WorkerStopped)
        );
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderPathDisconnected(
                KmsRenderWorkerFailure {
                    operation: KmsRenderOperation::AddOutput,
                    generation: 41,
                    key: Some(key),
                    failure: KmsRenderPlatformFailure {
                        code: "render-worker-registration-channel-disconnected",
                        ..
                    },
                }
            )) if key == output.key
        ));
    }

    #[test]
    fn begin_render_path_failure_latches_barrier_before_terminal_delivery() {
        let output = blocked_output();
        let acquire_called = Arc::new(AtomicBool::new(false));
        let (event_sender, event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            event_sender,
        )
        .expect("worker starts");
        let releases = worker.release_sender();
        let registrations = worker.registration_sender();
        let worker_stop = worker.stop_handle();
        let render_lifecycle = worker_stop.render_lifecycle();
        let quiescences = worker.quiescence_sender();
        let (reply_sender, _reply_receiver) = mpsc::channel();
        let (render_sender, render_receiver) = mpsc::channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender.clone(),
            releases: releases.clone(),
            registrations,
            worker_stop: worker_stop.clone(),
            terminal: None,
        });
        world.insert_resource(KmsRenderCommands {
            commands: Mutex::new(render_receiver),
            releases,
            quiescences,
        });
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.lifecycle = render_lifecycle;
        world.insert_resource(targets);
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());
        let acquire_flag = Arc::clone(&acquire_called);
        render_sender
            .send(RenderWorldCommand::Install {
                generation: 60,
                key: output.key.clone(),
                handle: ManualTextureViewHandle(60),
                extent: (output.display.mode.width, output.display.mode.height),
                acquire: Box::new(move || {
                    acquire_flag.store(true, Ordering::SeqCst);
                    Err("retained render-path-failure source has no frame".into())
                }),
            })
            .expect("queue retained source");
        world
            .run_system_once(apply_render_world_commands)
            .expect("install retained source");
        world
            .remove_resource::<KmsRenderCommands>()
            .expect("disconnect terminal commands while retaining render targets");
        let present_called = retain_presenter(&mut world);
        let failure = KmsRenderWorkerFailure {
            operation: KmsRenderOperation::RemoveOutput,
            generation: 61,
            key: Some(output.key.clone()),
            failure: KmsRenderPlatformFailure::new(
                "render-world-command-disconnected",
                "injected render-path failure",
            ),
        };

        // `terminate_registrar` performs this transition and barrier store before `wake`.
        // Delay only the wake so `finalize_worker_exit` cannot mask a missing store here.
        let terminal_effect = world
            .resource_mut::<KmsRegistrarInbox>()
            .registrar
            .transition_terminal()
            .expect("first terminal transition produces cleanup");
        worker_stop.begin_render_path_failure(failure.clone());
        // Any delayed Resume/ChangeOutput/RemoveOutput completion calls the
        // same `active()` transition. It must be unable to resurrect callbacks
        // after the terminal store above.
        worker_stop.render_lifecycle().attempt_active_for_test();
        apply_main_world_effect(&mut world, terminal_effect)
            .expect("terminal cleanup tolerates a disconnected command receiver");

        assert_terminal_barrier_suppresses_retained_callbacks(
            &mut world,
            &output.key,
            &acquire_called,
            &present_called,
        );
        worker_stop.wake();
        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderPathDisconnected(failure))
        );
    }

    #[test]
    fn finalized_worker_exit_suppresses_retained_callbacks_without_terminal_delivery() {
        let output = blocked_output();
        let acquire_called = Arc::new(AtomicBool::new(false));
        let (event_sender, event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FailingResumeAfterRegisteredPlatform {
                acquire_called: Arc::clone(&acquire_called),
            },
            event_sender,
        )
        .expect("worker starts");
        let releases = worker.release_sender();
        let registrations = worker.registration_sender();
        let worker_stop = worker.stop_handle();
        let render_lifecycle = worker_stop.render_lifecycle();
        let quiescences = worker.quiescence_sender();
        let (reply_sender, reply_receiver) = mpsc::channel();
        let (render_sender, render_receiver) = mpsc::channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender,
            releases: releases.clone(),
            registrations,
            worker_stop,
            terminal: None,
        });
        world.insert_resource(KmsRenderCommands {
            commands: Mutex::new(render_receiver),
            releases,
            quiescences,
        });
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.lifecycle = render_lifecycle;
        world.insert_resource(targets);
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());

        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 70,
                output: output.clone(),
            })
            .expect("queue registered output");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut replies = Vec::new();
        while !replies
            .iter()
            .any(|reply| matches!(reply, KmsRenderReply::OutputReady { generation: 70, .. }))
            && Instant::now() < deadline
        {
            apply_registrar_events(&mut world);
            world
                .run_system_once(apply_render_world_commands)
                .expect("apply render-world install");
            replies.extend(reply_receiver.try_iter());
            thread::yield_now();
        }
        assert!(matches!(
            replies.as_slice(),
            [KmsRenderReply::OutputReady { generation: 70, key }] if *key == output.key
        ));
        world
            .remove_resource::<KmsRenderCommands>()
            .expect("disconnect terminal commands while retaining render targets");
        let present_called = retain_presenter(&mut world);

        worker
            .send(KmsRenderCommand::Resume { generation: 71 })
            .expect("queue failing resume");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !replies.iter().any(|reply| {
            matches!(
                reply,
                KmsRenderReply::WorkerFailed {
                    operation: KmsRenderOperation::Resume,
                    generation: 71,
                    code: "injected-resume-failure",
                    ..
                }
            )
        }) && Instant::now() < deadline
        {
            apply_registrar_events(&mut world);
            replies.extend(reply_receiver.try_iter());
            thread::yield_now();
        }
        assert!(matches!(
            replies.as_slice(),
            [
                KmsRenderReply::OutputReady { generation: 70, .. },
                KmsRenderReply::WorkerFailed {
                    operation: KmsRenderOperation::Resume,
                    generation: 71,
                    key: None,
                    code: "injected-resume-failure",
                    ..
                }
            ]
        ));
        assert_terminal_barrier_suppresses_retained_callbacks(
            &mut world,
            &output.key,
            &acquire_called,
            &present_called,
        );
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::PlatformFailed(
                KmsRenderWorkerFailure {
                    operation: KmsRenderOperation::Resume,
                    generation: 71,
                    key: None,
                    failure: KmsRenderPlatformFailure {
                        code: "injected-resume-failure",
                        ..
                    },
                }
            ))
        ));
    }

    #[test]
    fn panicking_worker_with_registered_source_publishes_and_forwards_terminal_failure() {
        let first = blocked_output();
        let mut panicking = blocked_output();
        panicking.key.connector_name = "Panicking-2".into();
        panicking.connector_id = 92;
        let panicking_key = panicking.key.clone();
        let acquire_called = Arc::new(AtomicBool::new(false));
        let (event_sender, event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            PanicAfterRegisteredPlatform {
                panic_connector_id: panicking.connector_id,
                acquire_called: Arc::clone(&acquire_called),
            },
            event_sender,
        )
        .expect("worker starts");
        let releases = worker.release_sender();
        let registrations = worker.registration_sender();
        let worker_stop = worker.stop_handle();
        let render_lifecycle = worker_stop.render_lifecycle();
        let quiescences = worker.quiescence_sender();
        let (reply_sender, reply_receiver) = mpsc::channel();
        let (render_sender, render_receiver) = mpsc::channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender,
            releases: releases.clone(),
            registrations,
            worker_stop,
            terminal: None,
        });
        world.insert_resource(KmsRenderCommands {
            commands: Mutex::new(render_receiver),
            releases,
            quiescences,
        });
        let mut targets = KmsRenderTargets::new(PresentDeadline::unbounded_non_presenting());
        targets.lifecycle = render_lifecycle;
        world.insert_resource(targets);
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());

        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 1,
                output: first.clone(),
            })
            .expect("queue registered output");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut replies = Vec::new();
        while !replies
            .iter()
            .any(|reply| matches!(reply, KmsRenderReply::OutputReady { generation: 1, .. }))
            && Instant::now() < deadline
        {
            apply_registrar_events(&mut world);
            world
                .run_system_once(apply_render_world_commands)
                .expect("apply render-world install");
            replies.extend(reply_receiver.try_iter());
            thread::yield_now();
        }
        assert!(matches!(
            replies.as_slice(),
            [KmsRenderReply::OutputReady {
                generation: 1,
                key,
            }] if *key == first.key
        ));
        assert!(
            world
                .resource::<KmsRenderTargets>()
                .sources
                .contains_key(&first.key),
            "the registered source must retain its instrumented event sender"
        );
        let first_output = world
            .resource::<KmsMainWorldOutputs>()
            .0
            .get(&first.key)
            .copied()
            .expect("registered output has main-world state");
        assert!(
            world
                .resource::<ManualTextureViews>()
                .contains_key(&first_output.handle),
            "registered output has a manual texture view"
        );
        world
            .remove_resource::<KmsRenderCommands>()
            .expect("disconnect terminal commands while retaining render targets");
        let present_called = retain_presenter(&mut world);

        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 2,
                output: panicking,
            })
            .expect("queue panicking output");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !replies.iter().any(|reply| {
            matches!(
                reply,
                KmsRenderReply::WorkerFailed {
                    generation: 2,
                    code: "render-worker-panicked",
                    ..
                }
            )
        }) && Instant::now() < deadline
        {
            apply_registrar_events(&mut world);
            replies.extend(reply_receiver.try_iter());
            thread::yield_now();
        }
        assert!(matches!(
            replies.as_slice(),
            [
                KmsRenderReply::OutputReady { generation: 1, .. },
                KmsRenderReply::WorkerFailed {
                    operation: KmsRenderOperation::AddOutput,
                    generation: 2,
                    key: Some(key),
                    code: "render-worker-panicked",
                    ..
                }
            ] if *key == panicking_key
        ));
        assert_terminal_barrier_suppresses_retained_callbacks(
            &mut world,
            &first.key,
            &acquire_called,
            &present_called,
        );
        assert!(
            !world
                .resource::<KmsMainWorldOutputs>()
                .0
                .contains_key(&first.key),
            "terminal failure must remove the registered main-world output"
        );
        assert!(
            !world
                .resource::<ManualTextureViews>()
                .contains_key(&first_output.handle),
            "terminal failure must remove the registered manual texture view"
        );
        assert!(world.get_entity(first_output.entity).is_err());
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Panicked(_))
        ));
        assert!(matches!(
            world
                .resource::<KmsRegistrarInbox>()
                .receiver
                .lock()
                .expect("event receiver")
                .try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn disconnected_worker_event_channel_is_reported_as_terminal() {
        let (event_sender, event_receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            HandoffPlatform {
                add_called: Arc::new(AtomicBool::new(false)),
                operations: Arc::new(Mutex::new(Vec::new())),
            },
            event_sender,
        )
        .expect("worker starts");
        let worker_stop = worker.stop_handle();
        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );

        let (reply_sender, reply_receiver) = mpsc::channel();
        let (render_sender, _render_receiver) = mpsc::channel();
        let (releases, _release_receiver) = KmsRenderInputSender::test_channel();
        let (registrations, _registration_receiver) = KmsRenderInputSender::test_channel();
        let mut world = World::new();
        world.insert_resource(KmsRegistrarInbox {
            receiver: Mutex::new(event_receiver),
            replies: reply_sender,
            registrar: RenderSourceRegistrar::default(),
            render: render_sender,
            releases,
            registrations,
            worker_stop,
            terminal: None,
        });
        world.init_resource::<KmsMainWorldOutputs>();
        world.insert_resource(ManualTextureViews::default());

        apply_registrar_events(&mut world);

        assert_eq!(
            world.resource::<KmsRegistrarInbox>().terminal,
            Some(KmsRegistrarChannelError::EventChannelDisconnected)
        );
        assert!(matches!(
            reply_receiver.try_recv(),
            Ok(KmsRenderReply::WorkerFailed {
                operation: KmsRenderOperation::Worker,
                generation: 0,
                key: None,
                code: "render-worker-event-channel-disconnected",
                ..
            })
        ));
    }

    #[test]
    fn wedged_pump_join_detaches_within_its_deadline() {
        let barrier = AcquireBarrier::default();
        let thread_barrier = barrier.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let thread_finished = Arc::clone(&finished);
        let (completion_sender, completion) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let (lock, wake) = &*thread_barrier.0;
            let mut state = lock.lock().expect("barrier state");
            state.entered = true;
            wake.notify_all();
            while !state.released {
                state = wake.wait(state).expect("barrier wait");
            }
            thread_finished.store(true, Ordering::Release);
            let _ = completion_sender.send(Ok(()));
        });
        {
            let (lock, wake) = &*barrier.0;
            let mut state = lock.lock().expect("barrier state");
            while !state.entered {
                state = wake.wait(state).expect("barrier wait");
            }
        }
        let mut join = BoundedPumpJoin {
            completion,
            thread: Some(thread),
            state: LiveRenderPumpState::Running,
            transition_probe: None,
        };
        let started = Instant::now();
        let error = join
            .finish(Duration::from_millis(20))
            .expect_err("wedged update is detached");
        assert!(matches!(
            error,
            crate::backend::kms_live::KmsLiveError::PumpDetached(_)
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(join.state, LiveRenderPumpState::Detached);
        assert!(!finished.load(Ordering::Acquire));

        let (lock, wake) = &*barrier.0;
        lock.lock().expect("barrier state").released = true;
        wake.notify_all();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn begin_stop_invokes_cancel_for_all_generations() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::clone(&recorded);
        let cancel = PresentationCancelHandle::fake(move |scope| {
            probe.lock().expect("cancel probe lock").push(scope);
        });
        let (pump, barrier) =
            LiveRenderPump::blocked_for_test_with_cancel(Duration::from_millis(20), cancel);

        pump.begin_stop();

        assert_eq!(
            *recorded.lock().expect("cancel probe lock"),
            [CancelScope::AllGenerations]
        );
        barrier.wait_for_stop();
        barrier.release_all_and_wait();
    }

    #[test]
    fn pause_cancellation_invokes_cancel_for_the_active_generation() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::clone(&recorded);
        let cancel = PresentationCancelHandle::fake(move |scope| {
            probe.lock().expect("cancel probe lock").push(scope);
        });
        let (pump, barrier) =
            LiveRenderPump::blocked_for_test_with_cancel(Duration::from_millis(20), cancel);

        pump.cancel_generation_presentations(73);

        assert_eq!(
            *recorded.lock().expect("cancel probe lock"),
            [CancelScope::Generation(73)]
        );
        pump.begin_stop();
        barrier.wait_for_stop();
        barrier.release_all_and_wait();
    }

    #[test]
    fn capacity_one_mailbox_does_not_block_presentation_cancellation() {
        let (cancelled, cancellation_observed) = mpsc::sync_channel(1);
        let cancel = PresentationCancelHandle::fake(move |scope| {
            if scope == CancelScope::Generation(73) {
                cancelled
                    .send(scope)
                    .expect("cancellation observer remains connected");
            }
        });
        let (pump, barrier) =
            LiveRenderPump::blocked_for_test_with_cancel(Duration::from_millis(20), cancel);
        pump.commands
            .try_send(PumpCommand::Update)
            .expect("the capacity-one command mailbox is occupied");

        pump.cancel_generation_presentations(73);

        assert_eq!(
            cancellation_observed
                .recv_timeout(Duration::from_millis(100))
                .expect("cancellation bypasses the full pump mailbox"),
            CancelScope::Generation(73)
        );
        assert!(matches!(
            barrier.commands.recv_timeout(Duration::from_secs(1)),
            Ok(PumpCommand::Update)
        ));
        pump.begin_stop();
        barrier.wait_for_stop();
        barrier.release_all_and_wait();
    }

    #[test]
    fn preparation_cancellation_wakes_a_blocked_readiness_send_cleanly() {
        let (preparation_sender, preparation_receiver) = mpsc::sync_channel(0);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            entered_sender.send(()).expect("announce readiness send");
            let result = send_live_pump_preparation(&preparation_sender, (), thread_stop.as_ref())
                .and_then(|status| match status {
                    PreparationSendStatus::Cancelled => Ok(()),
                    PreparationSendStatus::Delivered => {
                        Err(crate::backend::kms_live::KmsLiveError::Setup(
                            "readiness unexpectedly reached a cancelled coordinator".into(),
                        ))
                    }
                });
            completion_sender
                .send(result)
                .expect("report readiness-send outcome");
        });
        entered_receiver
            .recv()
            .expect("readiness sender reached the cancellation seam");

        let (commands, _command_receiver) = mpsc::sync_channel(1);
        let mut join = BoundedPumpJoin {
            completion: completion_receiver,
            thread: Some(thread),
            state: LiveRenderPumpState::Running,
            transition_probe: None,
        };
        abort_live_pump_preparation(stop.as_ref(), preparation_receiver, &commands, &mut join)
            .expect("cancelled readiness send joins instead of detaching");
        assert_eq!(join.state, LiveRenderPumpState::Joined);
    }

    #[test]
    fn latched_stop_prevents_a_queued_pump_command_from_executing() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PumpCommand::Update)
            .expect("queue one pump command");
        let stop = AtomicBool::new(true);
        assert!(
            receive_live_pump_command(&receiver, &stop)
                .expect("receive observes the stop latch")
                .is_none()
        );
    }

    #[test]
    fn bounded_pump_join_drop_never_waits_for_a_wedged_thread() {
        let barrier = AcquireBarrier::default();
        let thread_barrier = barrier.clone();
        let (_completion_sender, completion) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let (lock, wake) = &*thread_barrier.0;
            let mut state = lock.lock().expect("barrier state");
            state.entered = true;
            wake.notify_all();
            while !state.released {
                state = wake.wait(state).expect("barrier wait");
            }
        });
        {
            let (lock, wake) = &*barrier.0;
            let mut state = lock.lock().expect("barrier state");
            while !state.entered {
                state = wake.wait(state).expect("barrier wait");
            }
        }
        let join = BoundedPumpJoin {
            completion,
            thread: Some(thread),
            state: LiveRenderPumpState::Running,
            transition_probe: None,
        };
        let started = Instant::now();
        drop(join);
        assert!(started.elapsed() < Duration::from_millis(100));

        let (lock, wake) = &*barrier.0;
        lock.lock().expect("barrier state").released = true;
        wake.notify_all();
    }
}
