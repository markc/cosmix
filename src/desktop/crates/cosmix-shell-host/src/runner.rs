//! Bevy runner backed by one blocking calloop/Wayland event loop.

#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;
use std::time::Duration;

use bevy::app::{App, AppExit, PluginGroup, TerminalCtrlCHandlerPlugin};
use bevy::prelude::*;
use bevy::render::pipelined_rendering::PipelinedRenderingPlugin;
use bevy::time::Real;
use bevy::window::{ExitCondition, WindowPlugin};
use bevy::winit::WinitPlugin;
#[cfg(target_os = "linux")]
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, RegistrationToken};
use cosmix_shell::chrome::QuoinPanelMounts;
use cosmix_shell::core::{Edge, LogicalSize, OutputKey, ShellModel};
use cosmix_shell::runtime::{
    ShellCommand, ShellCommandKind, ShellFrameState, ShellRuntimePlugin, WakePolicy,
    replace_shell_model,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::delegate_compositor;
use smithay_client_toolkit::delegate_layer;
use smithay_client_toolkit::delegate_output;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::wlr_layer::{
    Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use crate::output::{
    OutputError, OutputRuntime, OutputRuntimeMap, SelectedOutput, insert_single_output,
    select_output,
};
use crate::planner::{OutputGeometry, ProtocolOp, plan_surface};
use crate::surface::{FractionalObjects, PanelSurface, SurfacePhase, SurfaceTag};

type ModelFactory = dyn Fn(OutputKey, LogicalSize) -> ShellModel + Send + Sync;

const ANIMATE_BACKSTOP: Duration = Duration::from_secs(1);
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONSECUTIVE_PAST_DEADLINES: u8 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayerCloseDecision {
    Retire,
    Exit,
}

fn layer_close_decision(output_live: bool, replacement_pending: bool) -> LayerCloseDecision {
    if output_live && !replacement_pending {
        LayerCloseDecision::Exit
    } else {
        LayerCloseDecision::Retire
    }
}

fn next_timer_deadline(
    policy: WakePolicy,
    animate_backstop: Option<Duration>,
    configure_deadlines: &[Duration],
) -> Option<Duration> {
    let policy_deadline = match policy {
        WakePolicy::Idle => None,
        WakePolicy::WakeAt(deadline) => Some(deadline),
        WakePolicy::Animate => animate_backstop,
    };
    policy_deadline
        .into_iter()
        .chain(configure_deadlines.iter().copied())
        .min()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PastDeadlineObservation {
    consecutive: u8,
    past: bool,
    stuck: bool,
}

fn observe_past_deadline(
    policy: WakePolicy,
    elapsed: Duration,
    consecutive: u8,
) -> PastDeadlineObservation {
    if matches!(policy, WakePolicy::WakeAt(deadline) if deadline <= elapsed) {
        let consecutive = consecutive.saturating_add(1);
        PastDeadlineObservation {
            consecutive,
            past: true,
            stuck: consecutive > MAX_CONSECUTIVE_PAST_DEADLINES,
        }
    } else {
        PastDeadlineObservation {
            consecutive: 0,
            past: false,
            stuck: false,
        }
    }
}

/// Startup policy supplied by the application while output identity and
/// geometry remain owned by SCTK discovery.
#[derive(Clone)]
pub struct LayerHostConfig {
    output_name: Option<String>,
    namespace: String,
    model_factory: Arc<ModelFactory>,
}

impl Debug for LayerHostConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LayerHostConfig")
            .field("output_name", &self.output_name)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl LayerHostConfig {
    pub fn new(
        output_name: Option<String>,
        model_factory: impl Fn(OutputKey, LogicalSize) -> ShellModel + Send + Sync + 'static,
    ) -> Self {
        Self {
            output_name,
            namespace: "dev.cosmix.quoin".to_owned(),
            model_factory: Arc::new(model_factory),
        }
    }
}

/// Four host-created UI roots, available before application Startup runs.
#[derive(Resource, Clone, Copy, Debug)]
pub struct LayerPanelMounts(pub QuoinPanelMounts);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayerHostError(String);

impl LayerHostError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Display for LayerHostError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for LayerHostError {}

/// Install the exact non-winit renderer stack and replace Bevy's runner with
/// the event-driven layer-shell loop. `LogPlugin` deliberately remains active.
pub fn configure_layer_host(app: &mut App, config: LayerHostConfig) -> &mut App {
    // Block termination signals before Bevy creates worker threads. Every
    // subsequently spawned thread inherits the mask, so calloop's signalfd is
    // the sole owner and the runner can log and drain a clean exit.
    #[cfg(target_os = "linux")]
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM]);
    app.add_plugins(
        DefaultPlugins
            .build()
            .disable::<WinitPlugin>()
            .disable::<PipelinedRenderingPlugin>()
            .disable::<TerminalCtrlCHandlerPlugin>()
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                close_when_requested: false,
                ..default()
            }),
    );
    app.set_runner(move |app| {
        run_layer_host(
            app,
            config,
            #[cfg(target_os = "linux")]
            signals,
        )
    })
}

struct RunnerState {
    app: App,
    connection: Connection,
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    layer_shell: LayerShell,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    requested_output: Option<String>,
    namespace: String,
    model_factory: Arc<ModelFactory>,
    outputs: OutputRuntimeMap,
    selected_key: Option<OutputKey>,
    needs_update: bool,
    last_wake: WakePolicy,
    timer_token: Option<RegistrationToken>,
    timer_deadline: Option<Duration>,
    consecutive_past_deadlines: u8,
    exit_reason: Option<String>,
    abnormal_exit: bool,
    ready_logged: bool,
    replacement_needed: bool,
}

fn run_layer_host(
    mut app: App,
    config: LayerHostConfig,
    #[cfg(target_os = "linux")] signals: calloop::Result<Signals>,
) -> AppExit {
    let connection = match Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(error) => {
            app.finish();
            app.cleanup();
            tracing::info!("QUOIN_LAYER_HOST_EXIT reason=wayland-connect-failed-{error}");
            return AppExit::error();
        }
    };
    let (globals, mut event_queue) = match registry_queue_init(&connection) {
        Ok(values) => values,
        Err(error) => {
            app.finish();
            app.cleanup();
            tracing::info!("QUOIN_LAYER_HOST_EXIT reason=wayland-registry-failed-{error}");
            return AppExit::error();
        }
    };
    let qh = event_queue.handle();
    let compositor_state = match CompositorState::bind(&globals, &qh) {
        Ok(state) => state,
        Err(error) => return setup_error(app, format!("wl-compositor-unavailable-{error}")),
    };
    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(state) => state,
        Err(error) => return setup_error(app, format!("layer-shell-unavailable-{error}")),
    };
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let fractional_manager = globals.bind(&qh, 1..=1, GlobalData).ok();
    let viewporter = globals.bind(&qh, 1..=1, GlobalData).ok();
    let mut state = RunnerState {
        app,
        connection: connection.clone(),
        registry_state,
        compositor_state,
        output_state,
        layer_shell,
        fractional_manager,
        viewporter,
        requested_output: config.output_name.clone(),
        namespace: config.namespace.clone(),
        model_factory: config.model_factory.clone(),
        outputs: BTreeMap::new(),
        selected_key: None,
        needs_update: true,
        last_wake: WakePolicy::Idle,
        timer_token: None,
        timer_deadline: None,
        consecutive_past_deadlines: 0,
        exit_reason: None,
        abnormal_exit: false,
        ready_logged: false,
        replacement_needed: false,
    };

    // wl_output and xdg-output each use done boundaries. Two roundtrips make
    // the first complete OutputInfo observable without polling.
    for _ in 0..2 {
        if let Err(error) = event_queue.roundtrip(&mut state) {
            return state_setup_error(state, format!("output-discovery-failed-{error}"));
        }
    }
    let selected = match select_output(&state.output_state, config.output_name.as_deref()) {
        Ok(selected) => selected,
        Err(error) => return state_setup_error(state, output_reason(&error)),
    };
    let model = (config.model_factory)(selected.key.clone(), selected.logical_size);
    state.app.add_plugins(ShellRuntimePlugin::new(model));

    let panels = match state.create_panels(&qh, &selected, None) {
        Ok(panels) => panels,
        Err(error) => return state_setup_error(state, error.to_string()),
    };
    let mounts = PanelSurface::mounts(&panels);
    let selected_key = selected.key.clone();
    if let Err(error) = insert_single_output(
        &mut state.outputs,
        selected.key,
        OutputRuntime {
            wl_output: selected.wl_output,
            info: selected.info,
            logical_size: selected.logical_size,
            scale: selected.scale,
            panels,
        },
    ) {
        return state_setup_error(state, output_reason(&error));
    }
    state.selected_key = Some(selected_key);
    state.app.insert_resource(LayerPanelMounts(mounts));
    state.app.finish();
    state.app.cleanup();

    if state.app.is_plugin_added::<WinitPlugin>()
        || state.app.is_plugin_added::<PipelinedRenderingPlugin>()
        || state.app.is_plugin_added::<TerminalCtrlCHandlerPlugin>()
    {
        return state_exit(state, "forbidden-bevy-host-plugin-active", true);
    }

    let mut event_loop: EventLoop<RunnerState> = match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => return state_exit(state, &format!("calloop-create-failed-{error}"), true),
    };
    if let Err(error) = WaylandSource::new(connection, event_queue).insert(event_loop.handle()) {
        return state_exit(state, &format!("wayland-source-failed-{error}"), true);
    }
    #[cfg(target_os = "linux")]
    {
        let signals = match signals {
            Ok(signals) => signals,
            Err(error) => {
                return state_exit(state, &format!("signal-source-failed-{error}"), true);
            }
        };
        if let Err(error) = event_loop
            .handle()
            .insert_source(signals, |event, _, state| {
                state.exit_reason = Some(match event.signal() {
                    Signal::SIGINT => "signal-int".to_owned(),
                    Signal::SIGTERM => "signal-term".to_owned(),
                    signal => format!("signal-{signal:?}"),
                });
            })
        {
            return state_exit(state, &format!("signal-source-insert-failed-{error}"), true);
        }
    }

    let loop_handle = event_loop.handle();
    loop {
        if state.replacement_needed {
            let closed_exit_pending = state
                .exit_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("layer-surface-closed-"));
            state.replacement_needed = false;
            if let Err(error) = state.replace_selected_output(&qh) {
                state.abnormal_exit = true;
                state.exit_reason = Some(format!("output-replacement-failed-{error}"));
                continue;
            }
            if closed_exit_pending && state.selected_key.is_some() {
                state.exit_reason = None;
            }
        }
        if state.exit_reason.is_some() {
            break;
        }
        if state.needs_update {
            state.needs_update = false;
            state.app.update();
            if let Some(exit) = state.app.should_exit() {
                state.abnormal_exit = exit.is_error();
                state.exit_reason = Some(if exit.is_error() {
                    "bevy-app-error".to_owned()
                } else {
                    "bevy-app-exit".to_owned()
                });
                continue;
            }
            if let Err(error) = state.reconcile(&qh) {
                state.abnormal_exit = true;
                state.exit_reason = Some(format!("surface-plan-failed-{error}"));
                continue;
            }
            if let Err(error) = state.replace_wake_timer(&loop_handle) {
                state.abnormal_exit = true;
                state.exit_reason = Some(format!("wake-timer-failed-{error}"));
                continue;
            }
            if let Err(error) = state.connection.flush() {
                state.abnormal_exit = true;
                state.exit_reason = Some(format!("wayland-flush-failed-{error}"));
                continue;
            }
        }
        if state.needs_update || state.exit_reason.is_some() || state.replacement_needed {
            continue;
        }
        if let Err(error) = event_loop.dispatch(None, &mut state) {
            state.abnormal_exit = true;
            state.exit_reason = Some(format!("calloop-dispatch-failed-{error}"));
        }
    }

    let reason = state
        .exit_reason
        .clone()
        .unwrap_or_else(|| "clean".to_owned());
    let abnormal = state.abnormal_exit;
    state_exit(state, &reason, abnormal)
}

fn setup_error(mut app: App, reason: String) -> AppExit {
    app.finish();
    app.cleanup();
    tracing::info!("QUOIN_LAYER_HOST_EXIT reason={reason}");
    AppExit::error()
}

fn state_setup_error(mut state: RunnerState, reason: String) -> AppExit {
    state.app.finish();
    state.app.cleanup();
    tracing::info!("QUOIN_LAYER_HOST_EXIT reason={reason}");
    AppExit::error()
}

fn state_exit(mut state: RunnerState, reason: &str, abnormal: bool) -> AppExit {
    state.timer_token = None;
    for output in state.outputs.values_mut() {
        for panel in &mut output.panels {
            panel.close(&mut state.app);
        }
    }
    if !state.outputs.is_empty() {
        state.app.update();
    }
    state.outputs.clear();
    tracing::info!("QUOIN_LAYER_HOST_EXIT reason={reason}");
    if abnormal {
        AppExit::error()
    } else {
        AppExit::Success
    }
}

fn output_reason(error: &OutputError) -> String {
    match error {
        OutputError::RequestedOutputUnavailable(name) => {
            format!("requested-output-unavailable-{name}")
        }
        OutputError::NoCompleteOutput => "no-complete-output".to_owned(),
        OutputError::MoreThanOneOutput => "v1-output-limit-exceeded".to_owned(),
    }
}

struct PanelWaylandFactory<'a> {
    compositor_state: &'a CompositorState,
    layer_shell: &'a LayerShell,
    fractional_manager: Option<&'a WpFractionalScaleManagerV1>,
    viewporter: Option<&'a WpViewporter>,
    namespace: &'a str,
}

impl PanelWaylandFactory<'_> {
    fn create(
        &self,
        qh: &QueueHandle<RunnerState>,
        output: &wl_output::WlOutput,
        edge: Edge,
    ) -> (
        wl_surface::WlSurface,
        LayerSurface,
        Option<FractionalObjects>,
    ) {
        let wl_surface = self.compositor_state.create_surface(qh);
        let layer_surface = self.layer_shell.create_layer_surface(
            qh,
            wl_surface.clone(),
            Layer::Overlay,
            Some(self.namespace.to_owned()),
            Some(output),
        );
        let fractional = match (self.fractional_manager, self.viewporter) {
            (Some(manager), Some(viewporter)) => Some(FractionalObjects {
                scale: manager.get_fractional_scale(&wl_surface, qh, SurfaceTag { edge }),
                viewport: viewporter.get_viewport(&wl_surface, qh, GlobalData),
            }),
            _ => None,
        };
        (wl_surface, layer_surface, fractional)
    }
}

impl RunnerState {
    fn create_panels(
        &mut self,
        qh: &QueueHandle<Self>,
        selected: &SelectedOutput,
        retained_mounts: Option<QuoinPanelMounts>,
    ) -> Result<[PanelSurface; 4], LayerHostError> {
        let factory = PanelWaylandFactory {
            compositor_state: &self.compositor_state,
            layer_shell: &self.layer_shell,
            fractional_manager: self.fractional_manager.as_ref(),
            viewporter: self.viewporter.as_ref(),
            namespace: &self.namespace,
        };
        let mut panel_vec = Vec::with_capacity(Edge::ALL.len());
        for edge in Edge::ALL {
            let (wl_surface, layer_surface, fractional) =
                factory.create(qh, &selected.wl_output, edge);
            let panel = PanelSurface::from_wayland(
                &mut self.app,
                &self.connection,
                wl_surface,
                layer_surface,
                selected.logical_size,
                selected.scale,
                edge,
                fractional,
                retained_mounts.map(|mounts| mounts.get(edge)),
            )
            .map_err(|error| LayerHostError::new(format!("raw-handle-failed-{error}")))?;
            panel_vec.push(panel);
        }
        panel_vec
            .try_into()
            .map_err(|_| LayerHostError::new("panel construction count was not four"))
    }

    fn replace_selected_output(&mut self, qh: &QueueHandle<Self>) -> Result<(), LayerHostError> {
        let mut retired = std::mem::take(&mut self.outputs)
            .into_values()
            .next()
            .ok_or_else(|| LayerHostError::new("removed output runtime was unavailable"))?;
        let retained_mounts = PanelSurface::mounts(&retired.panels);
        for panel in &mut retired.panels {
            panel.close(&mut self.app);
        }
        // Drain render extraction before any raw owner or protocol object is
        // dropped, exactly as for a normal unmap.
        self.app.update();
        for panel in retired.panels {
            panel.retire(&mut self.app);
        }

        let selected = match select_output(&self.output_state, self.requested_output.as_deref()) {
            Ok(selected) => selected,
            Err(OutputError::NoCompleteOutput | OutputError::RequestedOutputUnavailable(_)) => {
                self.selected_key = None;
                self.exit_reason = Some("selected-output-removed-no-replacement".to_owned());
                return Ok(());
            }
            Err(error) => return Err(LayerHostError::new(output_reason(&error))),
        };
        let model = (self.model_factory)(selected.key.clone(), selected.logical_size);
        replace_shell_model(self.app.world_mut(), model);
        let panels = self.create_panels(qh, &selected, Some(retained_mounts))?;
        let selected_key = selected.key.clone();
        insert_single_output(
            &mut self.outputs,
            selected.key,
            OutputRuntime {
                wl_output: selected.wl_output,
                info: selected.info,
                logical_size: selected.logical_size,
                scale: selected.scale,
                panels,
            },
        )
        .map_err(|error| LayerHostError::new(output_reason(&error)))?;
        self.selected_key = Some(selected_key);
        self.last_wake = WakePolicy::Idle;
        self.consecutive_past_deadlines = 0;
        self.needs_update = true;
        Ok(())
    }

    fn reconcile(&mut self, qh: &QueueHandle<Self>) -> Result<(), LayerHostError> {
        let elapsed = self
            .app
            .world()
            .get_resource::<Time<Real>>()
            .map_or(Duration::ZERO, Time::elapsed);
        let frame = self
            .app
            .world()
            .get_resource::<ShellFrameState>()
            .ok_or_else(|| LayerHostError::new("ShellFrameState is unavailable"))?
            .0
            .clone();
        let key = self
            .selected_key
            .as_ref()
            .ok_or_else(|| LayerHostError::new("selected output key is unavailable"))?;
        let RunnerState {
            app,
            connection,
            compositor_state,
            layer_shell,
            fractional_manager,
            viewporter,
            namespace,
            outputs,
            ..
        } = self;
        let output = outputs
            .get_mut(key)
            .ok_or_else(|| LayerHostError::new("selected output runtime is unavailable"))?;
        let geometry = OutputGeometry {
            width: output.logical_size.width(),
            height: output.logical_size.height(),
        };
        let factory = PanelWaylandFactory {
            compositor_state,
            layer_shell,
            fractional_manager: fractional_manager.as_ref(),
            viewporter: viewporter.as_ref(),
            namespace,
        };
        let mut unmaps = Vec::new();
        for edge in Edge::ALL {
            let panel = &mut output.panels[edge.index()];
            let next = frame.panel(edge);
            let operations = plan_surface(panel.last_committed.as_ref(), next, geometry)
                .map_err(|error| LayerHostError::new(error.to_string()))?;
            panel.last_committed = Some(next.clone());
            if !panel.has_wayland_objects() && operations.contains(&ProtocolOp::CreateSurface) {
                let (wl_surface, layer_surface, fractional) =
                    factory.create(qh, &output.wl_output, edge);
                panel
                    .install_wayland(connection, wl_surface, layer_surface, fractional)
                    .map_err(|error| LayerHostError::new(format!("raw-handle-failed-{error}")))?;
            }
            if operations.contains(&ProtocolOp::Unmap) {
                panel.begin_unmap(app);
                unmaps.push(edge);
            }
            panel.apply_protocol_ops(&operations, elapsed);
        }
        if !unmaps.is_empty() {
            // This non-pipelined update drains render extraction after raw
            // handle removal and before destroying the protocol objects.
            app.update();
            for edge in unmaps {
                output.panels[edge.index()].finish_unmap();
            }
        }
        self.last_wake = frame.wake;
        if frame.wake == WakePolicy::Animate {
            for panel in &mut output.panels {
                if panel.phase == SurfacePhase::Configured
                    && panel.wants_animation_callback()
                    && !panel.frame_pending
                {
                    panel.request_frame(qh, elapsed);
                    panel.commit_frame_request();
                }
            }
        }
        Ok(())
    }

    fn replace_wake_timer(
        &mut self,
        loop_handle: &LoopHandle<'_, Self>,
    ) -> Result<(), LayerHostError> {
        let elapsed = self
            .app
            .world()
            .get_resource::<Time<Real>>()
            .map_or(Duration::ZERO, Time::elapsed);
        let observation =
            observe_past_deadline(self.last_wake, elapsed, self.consecutive_past_deadlines);
        self.consecutive_past_deadlines = observation.consecutive;
        if observation.past {
            self.needs_update = true;
        }
        if observation.stuck {
            if let Some(token) = self.timer_token.take() {
                loop_handle.remove(token);
            }
            self.timer_deadline = None;
            self.abnormal_exit = true;
            self.exit_reason = Some("wake-deadline-stuck".to_owned());
            return Ok(());
        }
        let effective_policy = if observation.past {
            WakePolicy::Idle
        } else {
            self.last_wake
        };
        let animate_backstop = (self.last_wake == WakePolicy::Animate)
            .then(|| elapsed.saturating_add(ANIMATE_BACKSTOP));
        let configure_deadlines = self
            .outputs
            .values()
            .flat_map(|output| output.panels.iter())
            .filter_map(|panel| {
                (panel.phase == SurfacePhase::WaitingConfigure)
                    .then_some(panel.waiting_configure_since)
                    .flatten()
                    .map(|started| started.saturating_add(CONFIGURE_TIMEOUT))
            })
            .collect::<Vec<_>>();
        let next_deadline =
            next_timer_deadline(effective_policy, animate_backstop, &configure_deadlines);
        if self.timer_deadline == next_deadline {
            return Ok(());
        }
        if let Some(token) = self.timer_token.take() {
            loop_handle.remove(token);
        }
        self.timer_deadline = next_deadline;
        if let Some(deadline) = next_deadline {
            let timer = Timer::from_duration(deadline.saturating_sub(elapsed));
            let token = loop_handle
                .insert_source(timer, |_, _, state| {
                    let fired_at = state.timer_deadline.take().unwrap_or_else(|| {
                        state
                            .app
                            .world()
                            .get_resource::<Time<Real>>()
                            .map_or(Duration::ZERO, Time::elapsed)
                    });
                    state.timer_token = None;
                    state.handle_wake_timer(fired_at);
                    TimeoutAction::Drop
                })
                .map_err(|error| LayerHostError::new(error.to_string()))?;
            self.timer_token = Some(token);
        }
        Ok(())
    }

    fn handle_wake_timer(&mut self, elapsed: Duration) {
        let configure_timeout = self
            .outputs
            .values()
            .flat_map(|output| output.panels.iter())
            .find(|panel| {
                panel.phase == SurfacePhase::WaitingConfigure
                    && panel
                        .waiting_configure_since
                        .is_some_and(|started| elapsed >= started.saturating_add(CONFIGURE_TIMEOUT))
            })
            .map(|panel| panel.edge);
        if let Some(edge) = configure_timeout {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("configure-timeout-{edge:?}"));
            return;
        }

        if self.last_wake == WakePolicy::Animate {
            for panel in self
                .outputs
                .values_mut()
                .flat_map(|output| output.panels.iter_mut())
            {
                panel.clear_overdue_frame(elapsed, ANIMATE_BACKSTOP);
            }
        }
        self.needs_update = true;
    }

    fn panel_for_surface_mut(
        &mut self,
        surface: &wl_surface::WlSurface,
    ) -> Option<&mut PanelSurface> {
        self.outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
            .find(|panel| panel.matches_surface(surface))
    }

    fn check_ready(&mut self) {
        if self.ready_logged {
            return;
        }
        let configured = self
            .outputs
            .values()
            .flat_map(|output| output.panels.iter())
            .filter(|panel| panel.phase == SurfacePhase::Configured)
            .count();
        let presented = self
            .outputs
            .values()
            .flat_map(|output| output.panels.iter())
            .filter(|panel| panel.presented)
            .count();
        if configured == 4 && presented == 4 {
            tracing::info!("QUOIN_LAYER_HOST_READY configured=4 presented=4");
            self.ready_logged = true;
        }
    }

    fn refresh_output(&mut self, output: &wl_output::WlOutput) {
        let Some(info) = self.output_state.info(output) else {
            return;
        };
        let Some((width, height)) = info.logical_size else {
            return;
        };
        let Ok(logical_size) = LogicalSize::new(width as f32, height as f32) else {
            return;
        };
        let scale = info.scale_factor.max(1);
        let Some(runtime) = self
            .outputs
            .values_mut()
            .find(|runtime| runtime.wl_output == *output)
        else {
            return;
        };
        runtime.info = info;
        runtime.logical_size = logical_size;
        runtime.scale = scale;
        for panel in &mut runtime.panels {
            if let Err(error) = panel.update_output_metrics(&mut self.app, logical_size, scale) {
                self.abnormal_exit = true;
                self.exit_reason =
                    Some(format!("configure-out-of-range-{}", error.reason_suffix()));
                return;
            }
        }
        if let Some(key) = &self.selected_key {
            let elapsed = self
                .app
                .world()
                .get_resource::<Time<Real>>()
                .map_or(Duration::ZERO, Time::elapsed);
            self.app.world_mut().write_message(ShellCommand {
                output: key.clone(),
                at: elapsed,
                kind: ShellCommandKind::Geometry(logical_size),
            });
        }
        self.needs_update = true;
    }
}

impl CompositorHandler for RunnerState {
    fn scale_factor_changed(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale: i32,
    ) {
        let output_size = panel_output_size(&self.outputs, surface);
        let RunnerState { app, outputs, .. } = self;
        let size_error = outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
            .find(|panel| panel.matches_surface(surface))
            .and_then(|panel| panel.update_output_metrics(app, output_size, scale).err());
        if let Some(error) = size_error {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("configure-out-of-range-{}", error.reason_suffix()));
        }
        self.needs_update = true;
    }

    fn transform_changed(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if let Some(panel) = self.panel_for_surface_mut(surface) {
            panel.frame_done();
        }
        if self.last_wake == WakePolicy::Animate {
            self.needs_update = true;
        }
        self.check_ready();
    }

    fn surface_enter(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

fn panel_output_size(outputs: &OutputRuntimeMap, surface: &wl_surface::WlSurface) -> LogicalSize {
    outputs
        .values()
        .find(|output| {
            output
                .panels
                .iter()
                .any(|panel| panel.matches_surface(surface))
        })
        .map_or_else(
            || LogicalSize::new(1.0, 1.0).expect("static fallback geometry is valid"),
            |output| output.logical_size,
        )
}

impl OutputHandler for RunnerState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.refresh_output(&output);
    }

    fn output_destroyed(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self
            .outputs
            .values()
            .any(|runtime| runtime.wl_output == output)
        {
            self.replacement_needed = true;
        }
    }
}

impl LayerShellHandler for RunnerState {
    fn closed(&mut self, _connection: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let Some((panel_output, edge)) = self.outputs.values().find_map(|output| {
            output
                .panels
                .iter()
                .find(|panel| panel.matches_layer(layer))
                .map(|panel| (output.wl_output.clone(), panel.edge))
        }) else {
            return;
        };
        let output_live = self
            .output_state
            .outputs()
            .any(|output| output == panel_output);
        let decision = layer_close_decision(output_live, self.replacement_needed);
        let RunnerState { app, outputs, .. } = self;
        if let Some(panel) = outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
            .find(|panel| panel.matches_layer(layer))
        {
            panel.close(app);
            match decision {
                LayerCloseDecision::Retire => self.replacement_needed = true,
                LayerCloseDecision::Exit => {
                    self.exit_reason = Some(format!("layer-surface-closed-{edge:?}"));
                }
            }
        }
    }

    fn configure(
        &mut self,
        _connection: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let elapsed = self
            .app
            .world()
            .get_resource::<Time<Real>>()
            .map_or(Duration::ZERO, Time::elapsed);
        let RunnerState { app, outputs, .. } = self;
        let mut configure_error = None;
        if let Some(panel) = outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
            .find(|panel| panel.matches_layer(layer))
        {
            // SCTK acknowledged the configure before invoking this callback.
            configure_error = panel.configure(app, qh, &configure, elapsed).err();
        }
        if let Some(error) = configure_error {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("configure-out-of-range-{}", error.reason_suffix()));
        } else {
            self.needs_update = true;
        }
    }
}

impl ProvidesRegistryState for RunnerState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

impl Dispatch<WpFractionalScaleManagerV1, GlobalData> for RunnerState {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _data: &GlobalData,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleV1, SurfaceTag> for RunnerState {
    fn event(
        state: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: <WpFractionalScaleV1 as Proxy>::Event,
        data: &SurfaceTag,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        let RunnerState { app, outputs, .. } = state;
        if let Some(panel) = outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
            .find(|panel| panel.edge == data.edge && panel.matches_fractional_scale(proxy))
        {
            if let Err(error) = panel.set_fractional_scale(app, scale) {
                state.abnormal_exit = true;
                state.exit_reason =
                    Some(format!("configure-out-of-range-{}", error.reason_suffix()));
            } else {
                state.needs_update = true;
            }
        }
    }
}

impl Dispatch<WpViewporter, GlobalData> for RunnerState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as Proxy>::Event,
        _data: &GlobalData,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, GlobalData> for RunnerState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as Proxy>::Event,
        _data: &GlobalData,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(RunnerState);
delegate_output!(RunnerState);
delegate_layer!(RunnerState);
delegate_registry!(RunnerState);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_close_only_exits_for_a_live_output_without_replacement() {
        assert_eq!(layer_close_decision(true, false), LayerCloseDecision::Exit);
        assert_eq!(
            layer_close_decision(false, false),
            LayerCloseDecision::Retire
        );
        assert_eq!(layer_close_decision(true, true), LayerCloseDecision::Retire);
        assert_eq!(
            layer_close_decision(false, true),
            LayerCloseDecision::Retire
        );
    }

    #[test]
    fn timer_selection_uses_the_earliest_bounded_one_shot() {
        let seconds = Duration::from_secs;
        assert_eq!(next_timer_deadline(WakePolicy::Idle, None, &[]), None);
        assert_eq!(
            next_timer_deadline(WakePolicy::Animate, Some(seconds(6)), &[]),
            Some(seconds(6))
        );
        assert_eq!(
            next_timer_deadline(
                WakePolicy::WakeAt(seconds(8)),
                None,
                &[seconds(7), seconds(12)]
            ),
            Some(seconds(7))
        );
        assert_eq!(
            next_timer_deadline(WakePolicy::Idle, None, &[seconds(10)]),
            Some(seconds(10))
        );
    }

    #[test]
    fn past_deadline_counter_is_bounded_and_resets() {
        let elapsed = Duration::from_secs(10);
        let past = WakePolicy::WakeAt(elapsed);
        let at_limit = observe_past_deadline(past, elapsed, 63);
        assert_eq!(
            at_limit,
            PastDeadlineObservation {
                consecutive: 64,
                past: true,
                stuck: false,
            }
        );
        assert!(observe_past_deadline(past, elapsed, at_limit.consecutive).stuck);
        assert_eq!(
            observe_past_deadline(
                WakePolicy::WakeAt(elapsed + Duration::from_secs(1)),
                elapsed,
                42
            ),
            PastDeadlineObservation {
                consecutive: 0,
                past: false,
                stuck: false,
            }
        );
        assert_eq!(
            observe_past_deadline(WakePolicy::Animate, elapsed, 42).consecutive,
            0
        );
    }
}
