//! Input-free, output-bound Bevy scenes hosted on native background surfaces.
//!
//! The application owns scene content; this runner owns Wayland and rendering
//! lifetimes. No Quoin model, seat, input grab or polling render loop is used.

use crate::{
    background::{BackgroundPacer, configure_background},
    raw_handle::{RetainedWindow, retained_raw_handle},
    render_target::HostedRenderTarget,
    surface::{FractionalObjects, FrameCallbackData, SurfaceScalePlan, surface_scale_plan},
};
use bevy::{
    app::{TaskPoolOptions, TaskPoolPlugin, TerminalCtrlCHandlerPlugin},
    camera::visibility::RenderLayers,
    ecs::schedule::{ScheduleLabel, SingleThreadedExecutor},
    prelude::*,
    render::{
        ExtractSchedule, Render, RenderApp, pipelined_rendering::PipelinedRenderingPlugin,
        renderer::RenderDevice,
    },
    window::{ExitCondition, WindowCreated, WindowPlugin, WindowResized, WindowScaleFactorChanged},
    winit::WinitPlugin,
};
use calloop::{
    EventLoop,
    signals::{Signal, Signals},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, SurfaceData},
    delegate_layer, delegate_output, delegate_registry,
    globals::GlobalData,
    output::{OutputHandler, OutputState},
    reexports::calloop_wayland_source::WaylandSource,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_callback, wl_compositor, wl_output, wl_surface},
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

const MAX_OUTPUTS: usize = 16;
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(10);

/// A scene uses local logical coordinates, with its own render layer. Window
/// identity changes on output replacement, even if the output name is reused.
#[derive(Clone, Debug)]
pub struct SceneView {
    pub window: Entity,
    /// The host owns this entity, its Camera::is_active, render target and
    /// render layer. Scenes may set its transform, projection and effects;
    /// they must not replace or despawn it, or create an extra output camera.
    pub camera: Entity,
    pub logical_size: (u32, u32),
    pub origin: (i32, i32),
    pub scale: f64,
    pub render_layer: usize,
    /// Changes whenever the host stages a configure or scale plan.
    pub configuration: u64,
}

#[derive(Resource, Default)]
pub struct SceneViews(pub BTreeMap<String, SceneView>);

/// Outputs allowed to advance and render during this particular update.
/// Control-only wakes and teardown updates leave this empty.
#[derive(Resource, Default)]
pub struct SceneTick(pub Vec<String>);

/// Results of this update's attempted frames, available in SceneAfterRender.
#[derive(Resource, Default)]
pub struct SceneFrameResults(pub BTreeMap<Entity, bool>);

/// Process-lifetime counters. Update duration is wall time inside app.update,
/// including renderer waits, not CPU time or time waiting for frame callbacks.
#[derive(Resource, Default)]
pub struct SceneMetrics {
    pub updates: u64,
    pub update_ns: u64,
    pub max_update_ns: u64,
    pub submitted_frames: u64,
    pub failed_frames: u64,
    /// Successful submission-to-submission intervals, summed across outputs.
    /// Explicitly paused/suspended intervals are excluded. These measure
    /// client submission cadence, not physical display presentation.
    pub submission_intervals: u64,
    pub submission_interval_ns: u64,
    pub max_submission_interval_ns: u64,
    /// Disjoint buckets: <=20ms, <=40ms, <=60ms, >60ms.
    pub submission_interval_buckets: [u64; 4],
}

impl SceneMetrics {
    fn observe_submission_interval(&mut self, interval: Duration) {
        let ns = u64::try_from(interval.as_nanos()).unwrap_or(u64::MAX);
        self.submission_intervals = self.submission_intervals.saturating_add(1);
        self.submission_interval_ns = self.submission_interval_ns.saturating_add(ns);
        self.max_submission_interval_ns = self.max_submission_interval_ns.max(ns);
        let bucket = if interval <= Duration::from_millis(20) {
            0
        } else if interval <= Duration::from_millis(40) {
            1
        } else if interval <= Duration::from_millis(60) {
            2
        } else {
            3
        };
        self.submission_interval_buckets[bucket] =
            self.submission_interval_buckets[bucket].saturating_add(1);
    }
}

/// Runs on the owning thread immediately after the renderer has returned.
/// Applications can correlate optional GPU readbacks with real submissions.
#[derive(bevy::ecs::schedule::ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SceneAfterRender;

#[derive(Resource, Default)]
pub struct SceneControl {
    pub paused: bool,
    pub hidden: bool,
    pub fps_limit: Option<u32>,
    /// Per-window suspension after the application has submitted settled
    /// content. Global pause still takes priority. A new configuration must
    /// receive a fresh frame before this mask can suppress its presentation.
    pub suspended_outputs: BTreeSet<Entity>,
}

/// Optional one-shot application wake, independent of frame callbacks. A Bus
/// health deadline can therefore expire while an output is hidden or paused.
#[derive(Resource, Default)]
pub struct SceneUpdateDeadline(pub Option<Instant>);

#[derive(Resource, Clone)]
pub struct SceneWake(calloop::channel::SyncSender<()>);
impl SceneWake {
    pub fn wake(&self) {
        let _ = self.0.try_send(());
    }
    pub fn callback(&self) -> std::sync::Arc<dyn Fn() + Send + Sync> {
        let wake = self.clone();
        std::sync::Arc::new(move || wake.wake())
    }
}

/// Rendering pipeline selected when each output camera is first created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SceneCameraKind {
    #[default]
    TwoD,
    #[cfg(feature = "scene-3d")]
    ThreeD,
}

/// Client presentation policy for SceneHost windows only.
#[derive(Resource)]
struct ScenePresentMode(bevy::window::PresentMode);

fn scene_present_mode(value: Option<&str>) -> Result<bevy::window::PresentMode, &'static str> {
    match value {
        None | Some("fifo") => Ok(bevy::window::PresentMode::Fifo),
        Some("auto-no-vsync") => Ok(bevy::window::PresentMode::AutoNoVsync),
        _ => Err("COSMIX_SCENE_PRESENT_MODE must be fifo or auto-no-vsync"),
    }
}

/// Process-wide background identity and initial rendering policy.
#[derive(Clone, Debug)]
pub struct SceneHostConfig {
    pub fps: u32,
    pub namespace: String,
    pub title: String,
    pub camera_kind: SceneCameraKind,
}

impl Default for SceneHostConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            namespace: "cosmix-wallpaper".into(),
            title: "Cosmix wallpaper".into(),
            camera_kind: SceneCameraKind::TwoD,
        }
    }
}

/// Install before adding application plugins, so worker threads inherit the
/// blocked termination signals and native handles always drain on shutdown.
pub fn configure_scene_host(app: &mut App, fps: u32) -> Result<&mut App, &'static str> {
    configure_scene_host_with_config(app, SceneHostConfig { fps, ..default() })
}

/// Configure an input-free native background with its own surface identity.
/// Like configure_scene_host, call before adding application plugins.
/// `COSMIX_SCENE_PRESENT_MODE=fifo|auto-no-vsync` permits a client-side pacing
/// comparison. FIFO remains the default; native frame-callback gating remains
/// active in either mode. This does not change the compositor's scanout mode.
pub fn configure_scene_host_with_config(
    app: &mut App,
    config: SceneHostConfig,
) -> Result<&mut App, &'static str> {
    let present_mode = std::env::var("COSMIX_SCENE_PRESENT_MODE")
        .map(Some)
        .or_else(|error| match error {
            std::env::VarError::NotPresent => Ok(None),
            _ => Err("COSMIX_SCENE_PRESENT_MODE must be UTF-8"),
        })?;
    let present_mode = scene_present_mode(present_mode.as_deref())?;
    BackgroundPacer::new(Instant::now(), config.fps)?;
    if config.namespace.is_empty() || config.namespace.contains('\0') || config.title.contains('\0')
    {
        return Err("scene namespace must be nonempty and identity must not contain NUL");
    }
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM]);
    let (wake, wakes) = calloop::channel::sync_channel(1);
    app.add_plugins(
        DefaultPlugins
            .build()
            .disable::<WinitPlugin>()
            .disable::<PipelinedRenderingPlugin>()
            .disable::<TerminalCtrlCHandlerPlugin>()
            .set(TaskPoolPlugin {
                // One IO worker, one asynchronous worker and two compute
                // workers. This bounded scene does not need a pool sized to
                // every host core merely to prepare a few instanced meshes.
                task_pool_options: TaskPoolOptions::with_num_threads(4),
            })
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                close_when_requested: false,
                ..default()
            }),
    );
    app.insert_resource(ScenePresentMode(present_mode))
        .init_resource::<SceneViews>()
        .init_resource::<SceneTick>()
        .init_resource::<SceneFrameResults>()
        .init_resource::<SceneMetrics>()
        .init_schedule(SceneAfterRender)
        .init_resource::<SceneControl>()
        .init_resource::<SceneUpdateDeadline>()
        .insert_resource(SceneWake(wake));
    crate::presentation::configure(app);
    app.set_runner(move |mut app| {
        app.finish();
        app.cleanup();
        configure_scene_schedules(&mut app);
        crate::runner::frame_trace::install(
            &mut app,
            [
                "scene_main",
                "scene_extract",
                "scene_render",
                "scene_acquire_windows",
            ],
        );
        let result = run(app, config, wakes, signals);
        match result {
            Ok(exit) => exit,
            Err(error) => {
                tracing::error!(%error, "SCENE_HOST_EXIT");
                AppExit::error()
            }
        }
    });
    Ok(app)
}

/// Small background scenes spend more work coordinating the parallel ECS
/// executor than running their systems. Serial schedule dispatch still permits
/// asset tasks and explicitly parallel render preparation, and keeps GPU work
/// on the same hardware renderer. Configure after plugin finish so extraction
/// and rendering schedules exist, before any native handles are exposed.
fn configure_scene_schedules(app: &mut App) {
    for label in [
        First.intern(),
        PreUpdate.intern(),
        Update.intern(),
        PostUpdate.intern(),
        Last.intern(),
    ] {
        app.edit_schedule(label, |schedule| {
            schedule.set_executor(SingleThreadedExecutor::new());
        });
    }
    if let Some(render) = app.get_sub_app_mut(RenderApp) {
        render.edit_schedule(Render, |schedule| {
            schedule.set_executor(SingleThreadedExecutor::new());
        });
        render.edit_schedule(ExtractSchedule, |schedule| {
            schedule.set_executor(SingleThreadedExecutor::new());
            // Replacing an executor resets this flag. Extraction must defer
            // commands to Render's ExtractCommands set, as ExtractPlugin does.
            schedule.set_apply_final_deferred(false);
        });
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use bevy::{app::SubApp, ecs::schedule::ScheduleBuildSettings};

    #[derive(Resource)]
    struct Extracted;

    #[test]
    fn revoked_output_does_not_disable_other_outputs_or_reappear_on_update() {
        let mut revoked = RevokedOutputs(Vec::new());
        revoked.revoke(10);
        revoked.revoke(10);
        for _ in 0..3 {
            revoked.retain_present(&[10, 20]);
            let admitted: Vec<_> = [10, 20]
                .into_iter()
                .filter(|output| !revoked.contains(output))
                .collect();
            assert_eq!(admitted, [20]);
            assert_eq!(revoked.0.len(), 1);
        }
    }

    #[test]
    fn output_replacement_and_removal_forget_only_the_old_revocation() {
        let mut revoked = RevokedOutputs(Vec::new());
        revoked.revoke(10);
        revoked.revoke(20);
        // A new protocol object is eligible even if the connector name is
        // reused before a reconcile sees the intermediate unplugged state.
        revoked.retain_present(&[11, 20]);
        assert!(!revoked.contains(&11));
        assert!(revoked.contains(&20));
        assert!(!revoked.contains(&10));
        revoked.retain_present(&[]);
        assert!(revoked.0.is_empty());
    }

    #[test]
    fn serial_dispatch_preserves_extraction_command_barrier() {
        let mut app = App::new();
        let mut render = SubApp::new();
        let mut extract = Schedule::new(ExtractSchedule);
        extract.set_build_settings(ScheduleBuildSettings {
            auto_insert_apply_deferred: false,
            ..default()
        });
        extract.set_apply_final_deferred(false);
        extract.add_systems(|mut commands: Commands| {
            commands.insert_resource(Extracted);
        });
        render.add_schedule(extract);
        app.insert_sub_app(RenderApp, render);
        configure_scene_schedules(&mut app);
        app.sub_app_mut(RenderApp).world_mut().schedule_scope(
            ExtractSchedule,
            |world, schedule| {
                schedule.run(world);
                assert!(
                    !world.contains_resource::<Extracted>(),
                    "extract must not apply commands"
                );
                schedule.apply_deferred(world);
                assert!(world.contains_resource::<Extracted>());
            },
        );
    }
}

struct SceneSurface {
    // Explicit drop after render drain; protocol children precede their parent.
    fractional: Option<FractionalObjects>,
    layer: LayerSurface,
    _owner: RetainedWindow,
    handle: bevy::window::RawHandleWrapper,
    output: wl_output::WlOutput,
    target: HostedRenderTarget,
    logical: (u32, u32),
    origin: (i32, i32),
    integer_scale: i32,
    fractional_scale: Option<f64>,
    slot: usize,
    configured: bool,
    pending_plan: Option<SurfaceScalePlan>,
    configuration: u64,
    failed_submissions: u8,
    last_submission: Option<Instant>,
    configure_deadline: Instant,
    pacer: BackgroundPacer,
}

impl SceneSurface {
    fn apply_size(&mut self, _app: &mut App, max_texture: u32) -> Result<(), String> {
        let plan = surface_scale_plan(
            self.logical,
            self.integer_scale,
            self.fractional_scale.or_else(|| {
                self.fractional
                    .as_ref()
                    .map(|_| f64::from(self.integer_scale))
            }),
            max_texture,
        )
        .map_err(|e| format!("invalid background size: {e:?}"))?;
        // Configures may arrive while paused or waiting for a callback. Do
        // not expose a raw handle or resize the GPU swapchain in a control-
        // only update: Bevy otherwise makes an untracked initial presentation,
        // and Mesa's next FIFO present can block waiting for an inactive VT.
        self.pending_plan = Some(plan);
        self.configuration = self
            .configuration
            .checked_add(1)
            .ok_or("background configuration generation exhausted")?;
        self.configured = true;
        Ok(())
    }

    fn prepare_frame(&mut self, app: &mut App) {
        let Some(plan) = self.pending_plan.take() else {
            return;
        };
        if self.layer.wl_surface().version() >= 3 {
            self.layer.wl_surface().set_buffer_scale(plan.buffer_scale);
        }
        if let (Some(objects), Some((width, height))) =
            (&self.fractional, plan.viewport_destination)
        {
            objects
                .viewport
                .as_ref()
                .expect("viewport lives with fractional scale")
                .set_destination(width, height);
        }
        let mut window = app
            .world_mut()
            .get_mut::<Window>(self.target.window)
            .expect("host window");
        window
            .resolution
            .set_scale_factor_override(Some(plan.scale_factor as f32));
        window
            .resolution
            .set_physical_resolution(plan.physical_size.0, plan.physical_size.1);
        if app
            .world()
            .get::<bevy::window::RawHandleWrapper>(self.target.window)
            .is_none()
        {
            app.world_mut()
                .entity_mut(self.target.window)
                .insert(self.handle.clone());
            app.world_mut().write_message(WindowCreated {
                window: self.target.window,
            });
        }
        app.world_mut().write_message(WindowResized {
            window: self.target.window,
            width: self.logical.0 as f32,
            height: self.logical.1 as f32,
        });
        app.world_mut().write_message(WindowScaleFactorChanged {
            window: self.target.window,
            scale_factor: plan.scale_factor,
        });
    }

    fn view(&self) -> SceneView {
        SceneView {
            window: self.target.window,
            camera: self.target.camera,
            logical_size: self.logical,
            origin: self.origin,
            scale: self
                .fractional_scale
                .unwrap_or(f64::from(self.integer_scale)),
            render_layer: self.slot,
            configuration: self.configuration,
        }
    }
}

// A closed layer must not be recreated on the same wl_output. Track protocol
// identity, not the connector name: unplug/replug may reuse that name.
struct RevokedOutputs<T>(Vec<T>);

impl<T: PartialEq> RevokedOutputs<T> {
    fn revoke(&mut self, output: T) {
        if !self.0.contains(&output) {
            self.0.push(output);
        }
    }

    fn retain_present(&mut self, outputs: &[T]) {
        self.0.retain(|output| outputs.contains(output));
    }

    fn contains(&self, output: &T) -> bool {
        self.0.contains(output)
    }
}

struct State {
    app: App,
    connection: Connection,
    registry: RegistryState,
    compositor: CompositorState,
    outputs: OutputState,
    layer_shell: LayerShell,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    surfaces: BTreeMap<String, SceneSurface>,
    revoked_outputs: RevokedOutputs<wl_output::WlOutput>,
    outputs_dirty: bool,
    needs_update: bool,
    stop: bool,
    error: Option<String>,
    max_texture: u32,
    fps: u32,
    config: SceneHostConfig,
}

fn run(
    app: App,
    config: SceneHostConfig,
    wakes: calloop::channel::Channel<()>,
    signals: Result<Signals, calloop::Error>,
) -> Result<AppExit, String> {
    let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init(&connection).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let max_texture = app
        .get_sub_app(RenderApp)
        .and_then(|a| a.world().get_resource::<RenderDevice>())
        .map(|d| d.limits().max_texture_dimension_2d)
        .filter(|v| *v > 0)
        .ok_or("render device unavailable")?;
    let mut state = State {
        app,
        connection: connection.clone(),
        registry: RegistryState::new(&globals),
        compositor,
        outputs: OutputState::new(&globals, &qh),
        layer_shell,
        fractional_manager: globals.bind(&qh, 1..=1, ()).ok(),
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        surfaces: BTreeMap::new(),
        revoked_outputs: RevokedOutputs(Vec::new()),
        outputs_dirty: true,
        needs_update: true,
        stop: false,
        error: None,
        max_texture,
        fps: config.fps,
        config,
    };
    for _ in 0..2 {
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    }
    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(wakes, |event, _, state| {
            if matches!(event, calloop::channel::Event::Msg(())) {
                state.needs_update = true;
            }
        })
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(signals.map_err(|e| e.to_string())?, |_, _, state| {
            state.stop = true
        })
        .map_err(|e| e.to_string())?;
    WaylandSource::new(connection, queue)
        .insert(event_loop.handle())
        .map_err(|e| e.to_string())?;
    // Once raw handles can be installed, every return goes through the drain.
    let result = state.drive(&mut event_loop, &qh);
    state.disable_cameras();
    state.app.world_mut().resource_mut::<SceneViews>().0.clear();
    for surface in state.surfaces.values() {
        surface.target.detach(&mut state.app);
    }
    state.update_scene();
    state.surfaces.clear();
    tracing::info!("SCENE_HOST_STOPPED render_handles_drained=true");
    result
}

impl State {
    fn update_scene(&mut self) {
        let started = Instant::now();
        {
            let _trace = crate::runner::frame_trace::span("scene_app_update", 0);
            self.app.update();
        }
        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let mut metrics = self.app.world_mut().resource_mut::<SceneMetrics>();
        metrics.updates = metrics.updates.saturating_add(1);
        metrics.update_ns = metrics.update_ns.saturating_add(elapsed);
        metrics.max_update_ns = metrics.max_update_ns.max(elapsed);
    }

    fn disable_cameras(&mut self) {
        self.app.world_mut().resource_mut::<SceneTick>().0.clear();
        for surface in self.surfaces.values() {
            self.app
                .world_mut()
                .get_mut::<Camera>(surface.target.camera)
                .expect("host camera")
                .is_active = false;
        }
    }

    fn reconcile_outputs(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.outputs_dirty = false;
        let mut live = BTreeMap::new();
        let outputs: Vec<_> = self.outputs.outputs().collect();
        self.revoked_outputs.retain_present(&outputs);
        for output in outputs {
            if self.revoked_outputs.contains(&output) {
                continue;
            }
            let Some(info) = self.outputs.info(&output) else {
                continue;
            };
            let (Some(name), Some((width, height))) = (info.name, info.logical_size) else {
                continue;
            };
            if width <= 0 || height <= 0 || info.scale_factor <= 0 {
                continue;
            }
            if live
                .insert(
                    name,
                    (
                        output,
                        (width as u32, height as u32),
                        info.logical_position.unwrap_or((0, 0)),
                        info.scale_factor,
                    ),
                )
                .is_some()
            {
                return Err("duplicate output names".into());
            }
        }
        if live.len() > MAX_OUTPUTS {
            return Err("background supports at most 16 outputs".into());
        }
        if self.app.world().resource::<SceneControl>().hidden {
            live.clear();
        }
        let removed: Vec<_> = self
            .surfaces
            .iter()
            .filter(|(name, s)| live.get(*name).is_none_or(|v| v.0 != s.output))
            .map(|(name, _)| name.clone())
            .collect();
        if !removed.is_empty() {
            self.disable_cameras();
            for name in &removed {
                self.surfaces[name].target.detach(&mut self.app);
                self.app
                    .world_mut()
                    .resource_mut::<SceneViews>()
                    .0
                    .remove(name);
            }
            self.update_scene(); // extraction barrier before any native object drops
            for name in removed {
                let surface = self.surfaces.remove(&name).expect("retired output");
                self.app.world_mut().despawn(surface.target.camera);
                self.app.world_mut().despawn(surface.target.window);
                drop(surface);
            }
        }
        for (name, (output, logical, origin, scale)) in live {
            if let Some(surface) = self.surfaces.get_mut(&name) {
                surface.origin = origin;
                surface.integer_scale = scale;
                // Configure owns surface dimensions. Output geometry is only
                // the fallback when a configure leaves an axis unspecified.
                if surface.configured {
                    surface.apply_size(&mut self.app, self.max_texture)?;
                }
                continue;
            }
            let slot = (1..=MAX_OUTPUTS)
                .find(|slot| self.surfaces.values().all(|s| s.slot != *slot))
                .expect("bounded output slots");
            let wl_surface = self.compositor.create_surface(qh);
            let layer = self.layer_shell.create_layer_surface(
                qh,
                wl_surface.clone(),
                Layer::Background,
                Some(self.config.namespace.as_str()),
                Some(&output),
            );
            let fractional = match (&self.fractional_manager, &self.viewporter) {
                (Some(manager), Some(viewporter)) => Some(FractionalObjects {
                    scale: Some(manager.get_fractional_scale(&wl_surface, qh, GlobalData)),
                    viewport: Some(viewporter.get_viewport(&wl_surface, qh, ())),
                }),
                _ => None,
            };
            let (owner, handle) = retained_raw_handle(self.connection.clone(), wl_surface)
                .map_err(|e| e.to_string())?;
            configure_background(&layer, &self.compositor).map_err(|e| e.to_string())?;
            let target = HostedRenderTarget::spawn_with_camera(
                &mut self.app,
                format!("{} — {name}", self.config.title),
                self.config.camera_kind,
            );
            let present_mode = self.app.world().resource::<ScenePresentMode>().0;
            self.app
                .world_mut()
                .get_mut::<Window>(target.window)
                .expect("host window")
                .present_mode = present_mode;
            self.app
                .world_mut()
                .entity_mut(target.camera)
                .insert(RenderLayers::layer(slot));
            self.surfaces.insert(
                name,
                SceneSurface {
                    fractional,
                    layer,
                    _owner: owner,
                    handle,
                    output,
                    target,
                    logical,
                    origin,
                    integer_scale: scale,
                    fractional_scale: None,
                    slot,
                    configured: false,
                    pending_plan: None,
                    configuration: 0,
                    failed_submissions: 0,
                    last_submission: None,
                    configure_deadline: Instant::now() + CONFIGURE_TIMEOUT,
                    pacer: BackgroundPacer::new(Instant::now(), self.fps).expect("validated fps"),
                },
            );
        }
        self.needs_update = true;
        Ok(())
    }

    fn drive(
        &mut self,
        event_loop: &mut EventLoop<Self>,
        qh: &QueueHandle<Self>,
    ) -> Result<AppExit, String> {
        loop {
            if self
                .app
                .world()
                .resource::<SceneUpdateDeadline>()
                .0
                .is_some_and(|d| d <= Instant::now())
            {
                self.app.world_mut().resource_mut::<SceneUpdateDeadline>().0 = None;
                self.needs_update = true;
            }
            if let Some(error) = self.error.take() {
                return Err(error);
            }
            if let Some(exit) = self.app.should_exit() {
                return Ok(exit);
            }
            if self.stop {
                return Ok(AppExit::Success);
            }
            let hidden = self.app.world().resource::<SceneControl>().hidden;
            if self.outputs_dirty {
                self.reconcile_outputs(qh)?;
            }
            self.disable_cameras();
            let paused = self.app.world().resource::<SceneControl>().paused;
            let suspended = self
                .app
                .world()
                .resource::<SceneControl>()
                .suspended_outputs
                .clone();
            let now = Instant::now();
            if let Some(fps) = self.app.world().resource::<SceneControl>().fps_limit
                && (1..=60).contains(&fps)
                && fps != self.fps
            {
                self.fps = fps;
                for surface in self.surfaces.values_mut() {
                    surface.pacer.set_rate(now, fps).expect("validated rate");
                }
            }
            let mut deadline: Option<Instant> = None;
            let mut rendering = Vec::new();
            let mut views = BTreeMap::new();
            for (name, surface) in &mut self.surfaces {
                if !surface.configured {
                    if now >= surface.configure_deadline {
                        return Err(format!("background configure timed out: {name}"));
                    }
                    deadline = Some(deadline.map_or(surface.configure_deadline, |d| {
                        d.min(surface.configure_deadline)
                    }));
                    continue;
                }
                views.insert(name.clone(), surface.view());
                let output_paused = paused
                    || (suspended.contains(&surface.target.window)
                        && surface.pending_plan.is_none()
                        && surface.failed_submissions == 0);
                surface.pacer.set_paused(output_paused);
                if output_paused {
                    surface.last_submission = None;
                }
                if let Some(generation) = surface.pacer.begin_frame(now) {
                    surface.prepare_frame(&mut self.app);
                    let wl_surface = surface.layer.wl_surface();
                    let callback = wl_surface.frame(
                        qh,
                        FrameCallbackData {
                            surface: wl_surface.clone(),
                            generation,
                        },
                    );
                    crate::runner::frame_trace::point(
                        "scene_frame_requested",
                        u64::from(wl_surface.id().protocol_id()),
                        u64::from(callback.id().protocol_id()),
                    );
                    crate::runner::frame_trace::point(
                        "host_window_wayland",
                        surface.target.window.to_bits(),
                        u64::from(wl_surface.id().protocol_id()),
                    );
                    self.app
                        .world_mut()
                        .get_mut::<Camera>(surface.target.camera)
                        .expect("host camera")
                        .is_active = true;
                    rendering.push((name.clone(), generation));
                }
                if let Some(d) = surface.pacer.deadline() {
                    deadline = Some(deadline.map_or(d, |old| old.min(d)));
                }
            }
            if self.needs_update || !rendering.is_empty() {
                self.needs_update = false;
                self.app.world_mut().resource_mut::<SceneViews>().0 = views;
                self.app.world_mut().resource_mut::<SceneTick>().0 =
                    rendering.iter().map(|(name, _)| name.clone()).collect();
                self.update_scene();
                // Persist presentation changes before any failed-frame retry
                // can skip the bottom-of-loop comparison. The next iteration
                // must retire/recreate roles even if no buffer was submitted.
                self.outputs_dirty |= hidden != self.app.world().resource::<SceneControl>().hidden;
                let submitted = &self
                    .app
                    .sub_app(RenderApp)
                    .world()
                    .resource::<crate::presentation::SubmittedWindows>()
                    .0;
                let mut retry = false;
                let mut terminal_error = None;
                let mut results = BTreeMap::new();
                let submitted_at = Instant::now();
                let mut intervals = Vec::new();
                for (name, generation) in rendering {
                    let surface = self.surfaces.get_mut(&name).expect("rendered output");
                    let success = submitted.contains(&surface.target.window);
                    results.insert(surface.target.window, success);
                    if success {
                        surface.failed_submissions = 0;
                        if let Some(last) = surface.last_submission.replace(submitted_at) {
                            intervals.push(submitted_at.saturating_duration_since(last));
                        }
                    } else {
                        surface.failed_submissions += 1;
                        if surface.failed_submissions >= 3 {
                            terminal_error.get_or_insert_with(|| {
                                format!("background failed to submit three buffers: {name}")
                            });
                        }
                        surface.pacer.frame_not_submitted(generation);
                        retry = true;
                    }
                }
                {
                    let mut metrics = self.app.world_mut().resource_mut::<SceneMetrics>();
                    for interval in intervals {
                        metrics.observe_submission_interval(interval);
                    }
                    for submitted in results.values() {
                        if *submitted {
                            metrics.submitted_frames = metrics.submitted_frames.saturating_add(1);
                        } else {
                            metrics.failed_frames = metrics.failed_frames.saturating_add(1);
                        }
                    }
                }
                self.app.world_mut().resource_mut::<SceneFrameResults>().0 = results;
                if let Some(error) = terminal_error {
                    return Err(error);
                }
                self.app.world_mut().run_schedule(SceneAfterRender);
                {
                    let _trace = crate::runner::frame_trace::span("scene_wayland_flush", 0);
                    self.connection.flush().map_err(|e| e.to_string())?;
                }
                if retry {
                    continue;
                }
            }
            if let Some(d) = self.app.world().resource::<SceneUpdateDeadline>().0 {
                deadline = Some(deadline.map_or(d, |old| old.min(d)));
            }
            let timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if hidden != self.app.world().resource::<SceneControl>().hidden {
                self.outputs_dirty = true;
                continue;
            }
            if self.app.should_exit().is_some()
                || paused != self.app.world().resource::<SceneControl>().paused
                || suspended
                    != self
                        .app
                        .world()
                        .resource::<SceneControl>()
                        .suspended_outputs
            {
                continue;
            }
            let _trace = crate::runner::frame_trace::span("scene_dispatch_wait", 0);
            event_loop
                .dispatch(timeout, self)
                .map_err(|e| e.to_string())?;
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.outputs_dirty = true;
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.outputs_dirty = true;
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.outputs_dirty = true;
    }
}
impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(surface) = self.surfaces.values().find(|s| s.layer == *layer) {
            self.revoked_outputs.revoke(surface.output.clone());
            // Retire through reconcile_outputs' render extraction barrier.
            // Other outputs, and the Bus control service, remain alive.
            self.outputs_dirty = true;
        }
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(surface) = self.surfaces.values_mut().find(|s| s.layer == *layer) {
            let fallback = self
                .outputs
                .info(&surface.output)
                .and_then(|i| i.logical_size)
                .unwrap_or((surface.logical.0 as i32, surface.logical.1 as i32));
            surface.logical = (
                if configure.new_size.0 == 0 {
                    fallback.0.max(1) as u32
                } else {
                    configure.new_size.0
                },
                if configure.new_size.1 == 0 {
                    fallback.1.max(1) as u32
                } else {
                    configure.new_size.1
                },
            );
            if let Err(error) = surface.apply_size(&mut self.app, self.max_texture) {
                self.error = Some(error);
            }
            self.needs_update = true;
        }
    }
}
impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        wl_surface: &wl_surface::WlSurface,
        scale: i32,
    ) {
        if let Some(surface) = self
            .surfaces
            .values_mut()
            .find(|s| s.layer.wl_surface() == wl_surface)
        {
            surface.integer_scale = scale;
            if surface.configured
                && let Err(error) = surface.apply_size(&mut self.app, self.max_texture)
            {
                self.error = Some(error);
            }
            self.needs_update = true;
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl Dispatch<wl_callback::WlCallback, FrameCallbackData> for State {
    fn event(
        state: &mut Self,
        callback: &wl_callback::WlCallback,
        _: wl_callback::Event,
        data: &FrameCallbackData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        crate::runner::frame_trace::point(
            "scene_frame_received",
            u64::from(data.surface.id().protocol_id()),
            u64::from(callback.id().protocol_id()),
        );
        if let Some(surface) = state
            .surfaces
            .values_mut()
            .find(|s| s.layer.wl_surface() == &data.surface)
        {
            surface.pacer.frame_done(data.generation);
        }
    }
}
impl Dispatch<WpFractionalScaleV1, GlobalData> for State {
    fn event(
        state: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(surface) = state
                .surfaces
                .values_mut()
                .find(|s| s.fractional.as_ref().and_then(|f| f.scale.as_ref()) == Some(proxy))
        {
            surface.fractional_scale = Some(f64::from(scale) / 120.0);
            if surface.configured
                && let Err(error) = surface.apply_size(&mut state.app, state.max_texture)
            {
                state.error = Some(error);
            }
            state.needs_update = true;
        }
    }
}
wayland_client::delegate_noop!(State: ignore WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(State: ignore WpViewporter);
wayland_client::delegate_noop!(State: ignore WpViewport);
wayland_client::delegate_dispatch!(State: [wl_compositor::WlCompositor: GlobalData] => CompositorState);
wayland_client::delegate_dispatch!(State: [wl_surface::WlSurface: SurfaceData] => CompositorState);
delegate_output!(State);
delegate_layer!(State);
delegate_registry!(State);

#[cfg(test)]
mod present_mode_tests {
    #[test]
    fn explicit_present_mode_is_bounded_and_defaults_to_fifo() {
        use bevy::window::PresentMode;
        assert_eq!(super::scene_present_mode(None), Ok(PresentMode::Fifo));
        assert_eq!(
            super::scene_present_mode(Some("fifo")),
            Ok(PresentMode::Fifo)
        );
        assert_eq!(
            super::scene_present_mode(Some("auto-no-vsync")),
            Ok(PresentMode::AutoNoVsync)
        );
        assert!(super::scene_present_mode(Some("immediate")).is_err());
        assert!(super::scene_present_mode(Some("")).is_err());
    }
}
