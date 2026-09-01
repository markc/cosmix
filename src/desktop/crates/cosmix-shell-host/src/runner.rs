//! Bevy runner backed by one blocking calloop/Wayland event loop.

#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
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
use cosmix_shell::chrome::{QuoinCommittedMotionModes, QuoinPanelMounts};
use cosmix_shell::core::{Edge, LogicalSize, OutputKey, ShellModel};
use cosmix_shell::runtime::{
    ShellCommand, ShellCommandKind, ShellFrameState, ShellRuntimePlugin, WakePolicy,
    replace_shell_model,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, SurfaceData};
use smithay_client_toolkit::delegate_layer;
use smithay_client_toolkit::delegate_output;
use smithay_client_toolkit::delegate_pointer;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::delegate_seat;
use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_callback, wl_compositor, wl_output, wl_pointer, wl_seat, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use crate::corner_bus::{CornerBusHandle, CornerIngress, gate_ingress, start as start_corner_bus};
use crate::input::{PointerBridge, SurfaceTarget, configure_ingress, stage_shell_command};
use crate::output::{
    OutputError, OutputRuntime, OutputRuntimeMap, SelectedOutput, insert_single_output,
    select_output,
};
use crate::planner::{OutputGeometry, ProtocolOp, committed_edge_margin, plan_surface};
use crate::surface::{
    ApplyResult, FractionalObjects, FrameCallbackData, PanelSurface, SurfacePhase, SurfaceTag,
};

type ModelFactory = dyn Fn(OutputKey, LogicalSize) -> ShellModel + Send + Sync;

const ANIMATE_BACKSTOP: Duration = Duration::from_secs(1);
const ANIMATE_BACKSTOP_QUANTUM: Duration = Duration::from_millis(250);
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONSECUTIVE_PAST_DEADLINES: u8 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayerCloseDecision {
    Retire,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitAdvance {
    Retain,
    Pending,
    Advance,
    Repair,
}

fn commit_advance<E>(execution: Result<ApplyResult, E>, unmap: bool) -> Result<CommitAdvance, E> {
    execution.map(|result| match result {
        ApplyResult::Committed => CommitAdvance::Advance,
        ApplyResult::AwaitingConfigure => CommitAdvance::Pending,
        ApplyResult::Noop if unmap => CommitAdvance::Retain,
        ApplyResult::Noop => CommitAdvance::Repair,
    })
}

fn commit_advance_for_phase<E>(
    execution: Result<ApplyResult, E>,
    unmap: bool,
    awaiting_initial_configure: bool,
) -> Result<CommitAdvance, E> {
    if awaiting_initial_configure && !unmap {
        execution.map(|_| CommitAdvance::Pending)
    } else {
        commit_advance(execution, unmap)
    }
}

fn record_commit_advance(
    last_committed: &mut Option<cosmix_shell::runtime::PanelPresentation>,
    pending_committed: &mut Option<cosmix_shell::runtime::PanelPresentation>,
    next: &cosmix_shell::runtime::PanelPresentation,
    advance: CommitAdvance,
    unmap: bool,
) -> Option<cosmix_shell::core::PanelMode> {
    match advance {
        CommitAdvance::Advance => {
            *last_committed = Some(next.clone());
            *pending_committed = None;
            Some(next.mode)
        }
        CommitAdvance::Pending => {
            *pending_committed = Some(next.clone());
            None
        }
        CommitAdvance::Repair if last_committed.as_ref() == Some(next) && !unmap => Some(next.mode),
        CommitAdvance::Repair | CommitAdvance::Retain => None,
    }
}

fn update_committed_modes(
    modes: &mut QuoinCommittedMotionModes,
    committed: impl IntoIterator<Item = (Edge, cosmix_shell::core::PanelMode)>,
) -> bool {
    let mut changed = false;
    for (edge, mode) in committed {
        if modes.get(edge) != mode {
            modes.set(edge, mode);
            changed = true;
        }
    }
    changed
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
        WakePolicy::Idle | WakePolicy::Animate => None,
        WakePolicy::WakeAt(deadline) => Some(deadline),
    };
    policy_deadline
        .into_iter()
        .chain(animate_backstop)
        .chain(configure_deadlines.iter().copied())
        .min()
}

fn animate_backstop_deadline(requested_at: Duration) -> Duration {
    let deadline = requested_at.saturating_add(ANIMATE_BACKSTOP);
    let quantum_nanos = ANIMATE_BACKSTOP_QUANTUM.as_nanos();
    let remainder = deadline.as_nanos() % quantum_nanos;
    if remainder == 0 {
        deadline
    } else {
        deadline.saturating_add(Duration::from_nanos((quantum_nanos - remainder) as u64))
    }
}

fn oldest_frame_backstop_deadline(
    requested_at: impl IntoIterator<Item = Duration>,
) -> Option<Duration> {
    requested_at
        .into_iter()
        .min()
        .map(animate_backstop_deadline)
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
            stuck: consecutive >= MAX_CONSECUTIVE_PAST_DEADLINES,
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
    comp_service: String,
}

impl Debug for LayerHostConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LayerHostConfig")
            .field("output_name", &self.output_name)
            .field("namespace", &self.namespace)
            .field("comp_service", &self.comp_service)
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
            comp_service: "comp".to_owned(),
        }
    }

    pub fn with_comp_service(mut self, service: impl Into<String>) -> Self {
        self.comp_service = service.into();
        self
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
    configure_ingress(app);
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
    seat_state: SeatState,
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
    pointer_bridge: PointerBridge,
    pointer_seats: Vec<wl_seat::WlSeat>,
    active_pointer_seat: Option<wl_seat::WlSeat>,
    active_pointer: Option<wl_pointer::WlPointer>,
    corner_bus: Option<CornerBusHandle>,
    corner_engaged: BTreeSet<cosmix_shell::core::Corner>,
    corner_epoch: u64,
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
    let seat_state = SeatState::new(&globals, &qh);
    let fractional_manager = globals.bind(&qh, 1..=1, GlobalData).ok();
    let viewporter = globals.bind(&qh, 1..=1, GlobalData).ok();
    let mut state = RunnerState {
        app,
        connection: connection.clone(),
        registry_state,
        compositor_state,
        output_state,
        seat_state,
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
        pointer_bridge: PointerBridge::default(),
        pointer_seats: Vec::new(),
        active_pointer_seat: None,
        active_pointer: None,
        corner_bus: None,
        corner_engaged: BTreeSet::new(),
        corner_epoch: 0,
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
    state
        .app
        .insert_resource(QuoinCommittedMotionModes::hidden());
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
    let corner_bus = start_corner_bus(
        config.comp_service.clone(),
        state
            .selected_key
            .clone()
            .expect("selected output exists before corner ingress"),
    );
    let overflowed = corner_bus.overflowed.clone();
    let corner_epoch = corner_bus.epoch.clone();
    if let Err(error) =
        event_loop
            .handle()
            .insert_source(corner_bus.channel, move |event, _, state| match event {
                calloop::channel::Event::Msg(event) => {
                    let (reset, event) =
                        gate_ingress(event, &overflowed, &corner_epoch, &mut state.corner_epoch);
                    if reset {
                        state.apply_corner_ingress(CornerIngress::Reset {
                            epoch: state.corner_epoch,
                        });
                    }
                    if let Some(event) = event {
                        state.apply_corner_ingress(event);
                    }
                }
                calloop::channel::Event::Closed => {
                    state.apply_corner_ingress(CornerIngress::Reset {
                        epoch: state.corner_epoch,
                    })
                }
            })
    {
        return state_exit(
            state,
            &format!("corner-channel-insert-failed-{error}"),
            true,
        );
    }
    state.corner_bus = Some(corner_bus.handle);
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
    state.apply_corner_ingress(CornerIngress::Reset {
        epoch: state.corner_epoch,
    });
    if let Some(output) = state.selected_key.clone() {
        state.pointer_bridge.cleanup(&mut state.app, &output, None);
    }
    if let Some(pointer) = state.active_pointer.take() {
        pointer.release();
    }
    if let Some(corner_bus) = state.corner_bus.take()
        && !corner_bus.shutdown()
    {
        tracing::warn!(event = "quoin_corner_shutdown_timeout");
    }
    for output in state.outputs.values_mut() {
        for panel in &mut output.panels {
            panel.close(&mut state.app);
        }
    }
    if !state.outputs.is_empty() {
        state.app.update();
    }
    finish_closed_panels(
        state
            .outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut()),
    );
    state.outputs.clear();
    tracing::info!("QUOIN_LAYER_HOST_EXIT reason={reason}");
    if abnormal {
        AppExit::error()
    } else {
        AppExit::Success
    }
}

fn finish_closed_panels<'a>(panels: impl IntoIterator<Item = &'a mut PanelSurface>) {
    for panel in panels {
        panel.finish_close();
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
                scale: Some(manager.get_fractional_scale(&wl_surface, qh, SurfaceTag { edge })),
                viewport: Some(viewporter.get_viewport(&wl_surface, qh, GlobalData)),
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
        self.apply_corner_ingress(CornerIngress::Reset {
            epoch: self.corner_epoch,
        });
        let mut retired = std::mem::take(&mut self.outputs)
            .into_values()
            .next()
            .ok_or_else(|| LayerHostError::new("removed output runtime was unavailable"))?;
        let retained_mounts = PanelSurface::mounts(&retired.panels);
        if let Some(output) = self.selected_key.clone() {
            self.pointer_bridge.cleanup(&mut self.app, &output, None);
        }
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
        self.selected_key = Some(selected_key.clone());
        if let Some(corner_bus) = self.corner_bus.as_ref() {
            self.corner_epoch = corner_bus.select_output(selected_key);
        }
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
        let mut committed_modes = Vec::new();
        for edge in Edge::ALL {
            let panel = &mut output.panels[edge.index()];
            let next = frame.panel(edge);
            if panel.phase == SurfacePhase::WaitingConfigure
                && panel.pending_committed.as_ref() == Some(next)
            {
                continue;
            }
            let effective_previous = panel
                .pending_committed
                .as_ref()
                .or(panel.last_committed.as_ref());
            let operations = plan_surface(effective_previous, next, geometry)
                .map_err(|error| LayerHostError::new(error.to_string()))?;
            if !panel.has_wayland_objects() && operations.contains(&ProtocolOp::CreateSurface) {
                let (wl_surface, layer_surface, fractional) =
                    factory.create(qh, &output.wl_output, edge);
                panel
                    .install_wayland(connection, wl_surface, layer_surface, fractional)
                    .map_err(|error| LayerHostError::new(format!("raw-handle-failed-{error}")))?;
            }
            if operations.contains(&ProtocolOp::Unmap) {
                self.pointer_bridge.cleanup(app, key, Some(panel.window));
                panel.begin_unmap(app);
                unmaps.push((edge, next.clone()));
            }
            let awaiting_initial_configure =
                panel.phase == SurfacePhase::WaitingConfigure && panel.pending_committed.is_some();
            let advance = commit_advance_for_phase::<std::convert::Infallible>(
                Ok(panel.apply_protocol_ops(&operations, elapsed)),
                operations.contains(&ProtocolOp::Unmap),
                awaiting_initial_configure,
            )
            .expect("infallible Wayland request executor");
            if let Some(mode) = record_commit_advance(
                &mut panel.last_committed,
                &mut panel.pending_committed,
                next,
                advance,
                operations.contains(&ProtocolOp::Unmap),
            ) {
                committed_modes.push((edge, mode));
            }
        }
        if !unmaps.is_empty() {
            // This non-pipelined update drains render extraction after raw
            // handle removal and before destroying the protocol objects.
            app.update();
            for (edge, next) in unmaps {
                output.panels[edge.index()].finish_unmap();
                output.panels[edge.index()].last_committed = Some(next.clone());
                committed_modes.push((edge, next.mode));
            }
        }
        if !committed_modes.is_empty() {
            let changed = {
                let mut modes = app.world_mut().resource_mut::<QuoinCommittedMotionModes>();
                update_committed_modes(&mut modes, committed_modes)
            };
            if changed {
                self.needs_update = true;
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
        let animate_backstop = oldest_frame_backstop_deadline(
            self.outputs
                .values()
                .flat_map(|output| output.panels.iter())
                .filter_map(PanelSurface::pending_frame_requested_at),
        );
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

        for panel in self
            .outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
        {
            panel.clear_overdue_frame(elapsed, ANIMATE_BACKSTOP);
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
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        unreachable!("frame callbacks use generation-tagged user data")
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
        let mut committed_mode = None;
        if let Some(panel) = outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
            .find(|panel| panel.matches_layer(layer))
        {
            // SCTK acknowledged the configure before invoking this callback.
            match panel.configure(app, qh, &configure, elapsed) {
                Ok(mode) => committed_mode = mode.map(|mode| (panel.edge, mode)),
                Err(error) => configure_error = Some(error),
            }
        }
        if let Some(error) = configure_error {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("configure-out-of-range-{}", error.reason_suffix()));
        } else {
            if let Some((edge, mode)) = committed_mode {
                app.world_mut()
                    .resource_mut::<QuoinCommittedMotionModes>()
                    .set(edge, mode);
            }
            self.needs_update = true;
        }
    }
}

impl ProvidesRegistryState for RunnerState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

impl SeatHandler for RunnerState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }

    fn new_capability(
        &mut self,
        _connection: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer {
            return;
        }
        if !self.pointer_seats.contains(&seat) {
            self.pointer_seats.push(seat);
        }
        self.promote_pointer(qh);
    }

    fn remove_capability(
        &mut self,
        _connection: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.remove_pointer_seat(qh, &seat);
        }
    }

    fn remove_seat(
        &mut self,
        _connection: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
    ) {
        self.remove_pointer_seat(qh, &seat);
    }
}

impl PointerHandler for RunnerState {
    fn pointer_frame(
        &mut self,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        if events.is_empty() || self.active_pointer.as_ref() != Some(pointer) {
            return;
        }
        self.needs_update = true;
        let targets = self
            .outputs
            .values()
            .flat_map(|output| output.panels.iter().map(move |panel| (output, panel)))
            .filter_map(|(output, panel)| {
                let presentation = panel.last_committed.as_ref()?;
                panel.wayland_surface().map(|surface| SurfaceTarget {
                    surface,
                    window: panel.window,
                    edge: panel.edge,
                    output_size: Vec2::new(
                        output.logical_size.width(),
                        output.logical_size.height(),
                    ),
                    thickness: presentation.thickness_px,
                    committed_margin: committed_edge_margin(presentation),
                })
            })
            .collect::<Vec<_>>();
        let Some(output) = self.selected_key.as_ref() else {
            return;
        };
        if self
            .pointer_bridge
            .frame(&mut self.app, output, &targets, events)
            && let Some(position) = self.pointer_bridge.last_output_position()
        {
            tracing::trace!(
                event = "quoin_pointer_output_position",
                x = position.x,
                y = position.y
            );
        }
    }
}

impl RunnerState {
    fn apply_corner_ingress(&mut self, ingress: CornerIngress) {
        self.corner_epoch = self.corner_epoch.max(ingress.epoch());
        let resets = matches!(
            &ingress,
            CornerIngress::Reset { .. } | CornerIngress::Disabled { .. }
        );
        let events = match ingress {
            CornerIngress::Event { output, event, .. }
                if self.selected_key.as_ref() == Some(&output) =>
            {
                let corner = event.corner();
                let changed = match event {
                    cosmix_shell::core::CornerEvent::Entered { .. } => {
                        self.corner_engaged.insert(corner)
                    }
                    cosmix_shell::core::CornerEvent::Left { .. } => {
                        self.corner_engaged.remove(&corner)
                    }
                };
                changed.then_some(event).into_iter().collect::<Vec<_>>()
            }
            CornerIngress::Event { .. } => Vec::new(),
            CornerIngress::Reset { .. } | CornerIngress::Disabled { .. } => self
                .corner_engaged
                .iter()
                .copied()
                .map(|corner| cosmix_shell::core::CornerEvent::Left { corner })
                .collect::<Vec<_>>(),
        };
        if resets {
            self.corner_engaged.clear();
        }
        let Some(output) = self.selected_key.clone() else {
            return;
        };
        for event in events {
            stage_shell_command(
                &mut self.app,
                output.clone(),
                ShellCommandKind::Corner(event),
            );
            self.needs_update = true;
        }
    }

    fn promote_pointer(&mut self, qh: &QueueHandle<Self>) {
        if self.active_pointer.is_some() {
            return;
        }
        for seat in self.pointer_seats.clone() {
            if let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) {
                self.active_pointer_seat = Some(seat);
                self.active_pointer = Some(pointer);
                break;
            }
        }
    }

    fn remove_pointer_seat(&mut self, qh: &QueueHandle<Self>, seat: &wl_seat::WlSeat) {
        self.pointer_seats.retain(|candidate| candidate != seat);
        if self.active_pointer_seat.as_ref() != Some(seat) {
            return;
        }
        if let Some(output) = self.selected_key.clone()
            && self.pointer_bridge.cleanup(&mut self.app, &output, None)
        {
            self.needs_update = true;
        }
        if let Some(pointer) = self.active_pointer.take() {
            pointer.release();
        }
        self.active_pointer_seat = None;
        self.promote_pointer(qh);
    }
}

delegate_seat!(RunnerState);
delegate_pointer!(RunnerState);

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

impl Dispatch<wl_callback::WlCallback, FrameCallbackData> for RunnerState {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: <wl_callback::WlCallback as Proxy>::Event,
        data: &FrameCallbackData,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let wl_callback::Event::Done { .. } = event else {
            unreachable!("wl_callback has only the done event")
        };
        let Some(panel) = state.panel_for_surface_mut(&data.surface) else {
            tracing::trace!(
                callback_generation = data.generation,
                "ignoring frame callback for retired surface"
            );
            return;
        };
        if panel.frame_done(data.generation) {
            state.needs_update = true;
            state.check_ready();
        }
    }
}

wayland_client::delegate_dispatch!(RunnerState: [wl_compositor::WlCompositor: GlobalData] => CompositorState);
wayland_client::delegate_dispatch!(RunnerState: [wl_surface::WlSurface: SurfaceData] => CompositorState);
delegate_output!(RunnerState);
delegate_layer!(RunnerState);
delegate_registry!(RunnerState);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use bevy::MinimalPlugins;
    use bevy::time::TimeUpdateStrategy;
    use cosmix_shell::core::{
        ConcealReason, Corner, CornerEvent, CornerTrigger, PanelEffect, PanelMode,
    };
    use cosmix_shell::runtime::{ShellEffects, ShellRuntimePlugin};

    use crate::surface::frame_request_overdue;

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
    fn executor_failure_and_deferred_configure_cannot_advance_committed_state() {
        assert_eq!(
            commit_advance::<&str>(Ok(ApplyResult::AwaitingConfigure), false),
            Ok(CommitAdvance::Pending)
        );
        assert_eq!(
            commit_advance(Err("executor failed"), false),
            Err("executor failed")
        );
        assert_eq!(
            commit_advance::<&str>(Ok(ApplyResult::Noop), true),
            Ok(CommitAdvance::Retain)
        );
        assert_eq!(
            commit_advance_for_phase::<&str>(Ok(ApplyResult::Committed), false, true),
            Ok(CommitAdvance::Pending),
            "property commits before the first configure remain pending"
        );
        assert_eq!(
            commit_advance_for_phase::<&str>(Ok(ApplyResult::Noop), true, true),
            Ok(CommitAdvance::Retain),
            "cancelling a pending first map must still take the unmap path"
        );
    }

    #[test]
    fn stable_committed_presentation_schedules_zero_further_updates() {
        let mut modes = QuoinCommittedMotionModes::hidden();
        assert!(!update_committed_modes(
            &mut modes,
            [(Edge::Left, PanelMode::Hidden)]
        ));
        assert_eq!(modes.get(Edge::Left), PanelMode::Hidden);
    }

    #[test]
    fn committed_owner_seam_covers_deferred_failure_commit_and_unmap() {
        let model = ShellModel::new(
            OutputKey::new("DP-1").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let hidden = cosmix_shell::runtime::ShellFrame::from_model(&model)
            .panel(Edge::Left)
            .clone();
        let mut revealed = hidden.clone();
        revealed.mode = PanelMode::Revealed;
        revealed.mapped = true;

        let mut last = None;
        let mut pending = None;
        let committed = record_commit_advance(
            &mut last,
            &mut pending,
            &revealed,
            commit_advance::<&str>(Ok(ApplyResult::Committed), false).unwrap(),
            false,
        )
        .unwrap();
        let mut modes = QuoinCommittedMotionModes::hidden();
        assert!(update_committed_modes(
            &mut modes,
            [(Edge::Left, committed)]
        ));
        assert_eq!(modes.get(Edge::Left), PanelMode::Revealed);

        last = None;
        pending = None;
        assert_eq!(
            record_commit_advance(
                &mut last,
                &mut pending,
                &revealed,
                commit_advance::<&str>(Ok(ApplyResult::AwaitingConfigure), false).unwrap(),
                false,
            ),
            None
        );
        assert_eq!(pending.as_ref(), Some(&revealed));

        assert!(commit_advance(Err::<ApplyResult, _>("commit failed"), false).is_err());
        assert!(last.is_none(), "failure must not fabricate a commit");
        assert_eq!(pending.as_ref(), Some(&revealed));

        let operations = plan_surface(
            pending.as_ref().or(last.as_ref()),
            &hidden,
            OutputGeometry {
                width: 1_000.0,
                height: 800.0,
            },
        )
        .unwrap();
        assert_eq!(operations, [ProtocolOp::Unmap]);

        assert!(pending.take().is_some());
        last = Some(hidden.clone());
        modes = QuoinCommittedMotionModes::hidden();
        assert!(!update_committed_modes(
            &mut modes,
            [(Edge::Left, hidden.mode)]
        ));
        assert_eq!(last.as_ref(), Some(&hidden));
    }

    #[test]
    fn production_ingress_timer_and_pending_map_reconcile_corner_left_without_frames() {
        let output = OutputKey::new("DP-1").unwrap();
        let model = ShellModel::new(
            output.clone(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        configure_ingress(&mut app);
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs(1)));

        stage_shell_command(
            &mut app,
            output.clone(),
            ShellCommandKind::Corner(CornerEvent::Entered {
                corner: Corner::TopLeft,
                dwell: Duration::from_millis(200),
                trigger: CornerTrigger::Compositor,
            }),
        );
        app.update();
        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::from_millis(200));
        app.update();
        let revealed = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left)
            .clone();
        assert!(revealed.mapped);

        let mut last = None;
        let mut pending = None;
        record_commit_advance(
            &mut last,
            &mut pending,
            &revealed,
            CommitAdvance::Pending,
            false,
        );

        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::from_millis(10));
        stage_shell_command(
            &mut app,
            output,
            ShellCommandKind::Corner(CornerEvent::Left {
                corner: Corner::TopLeft,
            }),
        );
        app.update();
        let left = app.world().resource::<ShellFrameState>().0.clone();
        let deadline = match left.wake {
            WakePolicy::WakeAt(deadline) => deadline,
            policy => panic!("left must arm one calloop deadline, got {policy:?}"),
        };
        assert_eq!(deadline, Duration::from_millis(1_010));
        assert_eq!(next_timer_deadline(left.wake, None, &[]), Some(deadline));

        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::from_millis(800));
        app.update();
        assert_eq!(
            app.world().resource::<ShellEffects>().0,
            [cosmix_shell::runtime::ShellEffect {
                edge: Edge::Left,
                effect: PanelEffect::Conceal {
                    reason: ConcealReason::CornerLeft,
                },
            }]
        );

        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::from_millis(200));
        app.update();
        let concealed = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left)
            .clone();
        assert!(!concealed.mapped);
        assert_eq!(
            plan_surface(
                pending.as_ref().or(last.as_ref()),
                &concealed,
                OutputGeometry {
                    width: 1_000.0,
                    height: 800.0,
                },
            )
            .unwrap(),
            [ProtocolOp::Unmap]
        );
    }

    #[test]
    fn timer_selection_uses_the_earliest_bounded_one_shot() {
        let seconds = Duration::from_secs;
        assert_eq!(
            next_timer_deadline(
                WakePolicy::WakeAt(seconds(8)),
                Some(seconds(6)),
                &[seconds(7), seconds(12)]
            ),
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
    fn animate_backstop_quantises_request_deadlines() {
        let seconds = Duration::from_secs;
        let first = animate_backstop_deadline(seconds(5) + Duration::from_millis(10));
        let second = animate_backstop_deadline(seconds(5) + Duration::from_millis(20));
        assert_eq!(first, seconds(6) + Duration::from_millis(250));
        assert_eq!(first, second);
    }

    #[test]
    fn oldest_stalled_frame_controls_backstop_and_expires_before_timely_frame() {
        let seconds = Duration::from_secs;
        let stalled = seconds(5) + Duration::from_millis(10);
        let timely = seconds(5) + Duration::from_millis(800);
        let deadline = oldest_frame_backstop_deadline([stalled, timely])
            .expect("pending requests arm a deadline");

        assert_eq!(deadline, seconds(6) + Duration::from_millis(250));
        assert_eq!(
            oldest_frame_backstop_deadline([stalled, seconds(6)]),
            Some(deadline),
            "a newer timely request must not push the stalled request's deadline"
        );
        assert!(frame_request_overdue(
            Some(stalled),
            deadline,
            ANIMATE_BACKSTOP
        ));
        assert!(!frame_request_overdue(
            Some(timely),
            deadline,
            ANIMATE_BACKSTOP
        ));
    }

    #[test]
    fn leaving_animate_keeps_a_pending_frame_backstop_armed() {
        let backstop = oldest_frame_backstop_deadline([Duration::from_secs(5)]);
        assert_eq!(
            next_timer_deadline(WakePolicy::Idle, backstop, &[]),
            backstop
        );
    }

    #[test]
    fn idle_without_pending_frames_or_configures_arms_nothing() {
        assert_eq!(next_timer_deadline(WakePolicy::Idle, None, &[]), None);
    }

    #[test]
    fn state_exit_close_path_destroys_each_holder_once() {
        let mut app = App::new();
        let first = Arc::new(Mutex::new(Vec::new()));
        let second = Arc::new(Mutex::new(Vec::new()));
        let mut panels = [
            PanelSurface::test_double(&mut app, SurfacePhase::Closed, Arc::clone(&first)),
            PanelSurface::test_double(&mut app, SurfacePhase::Closed, Arc::clone(&second)),
        ];

        finish_closed_panels(panels.iter_mut());
        finish_closed_panels(panels.iter_mut());

        let expected = ["viewport", "fractional", "layer", "owner"];
        assert_eq!(*first.lock().unwrap(), expected);
        assert_eq!(*second.lock().unwrap(), expected);
    }

    #[test]
    fn past_deadline_counter_is_bounded_and_resets() {
        let elapsed = Duration::from_secs(10);
        let past = WakePolicy::WakeAt(elapsed);
        let before_limit = observe_past_deadline(past, elapsed, 62);
        assert_eq!(
            before_limit,
            PastDeadlineObservation {
                consecutive: 63,
                past: true,
                stuck: false,
            }
        );
        assert_eq!(
            observe_past_deadline(past, elapsed, before_limit.consecutive),
            PastDeadlineObservation {
                consecutive: 64,
                past: true,
                stuck: true,
            }
        );
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
