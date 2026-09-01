//! Bevy runner backed by one blocking calloop/Wayland event loop.

#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy::app::{App, AppExit, Last, PluginGroup, TerminalCtrlCHandlerPlugin};
use bevy::ecs::message::MessageReader;
use bevy::prelude::*;
use bevy::render::pipelined_rendering::PipelinedRenderingPlugin;
use bevy::time::{Real, TimeUpdateStrategy};
use bevy::window::{ExitCondition, RequestRedraw, WindowPlugin};
use bevy::winit::WinitPlugin;
#[cfg(target_os = "linux")]
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, RegistrationToken};
use cosmix_shell::chrome::{QuoinCommittedMotionModes, QuoinPanelMounts};
use cosmix_shell::core::{Edge, LogicalSize, OutputKey, PanelMode, ShellModel};
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
use crate::input::{
    PointerBridge, SurfaceTarget, configure_ingress, stage_shell_command,
    staged_shell_commands_pending,
};
use crate::output::{
    OutputError, OutputRuntime, OutputRuntimeMap, SelectedOutput, insert_single_output,
    select_output,
};
use crate::planner::{OutputGeometry, ProtocolOp, committed_edge_margin, plan_surface};
use crate::surface::{
    ApplyResult, FractionalObjects, FrameCallbackData, PanelSurface, SurfacePhase,
    SurfaceSizeError, SurfaceTag,
};

type ModelFactory = dyn Fn(OutputKey, LogicalSize) -> ShellModel + Send + Sync;

const ANIMATE_BACKSTOP: Duration = Duration::from_secs(1);
const ANIMATE_BACKSTOP_QUANTUM: Duration = Duration::from_millis(250);
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(10);
const EARLY_TIMER_REARM_GUARD: Duration = Duration::from_millis(1);
const MAX_CONSECUTIVE_PAST_DEADLINES: u8 = 16;
const MAX_PAST_DEADLINE_OBSERVATIONS: u8 = 64;

#[derive(Resource, Default)]
struct LayerHostUpdateWake(bool);

fn capture_layer_host_redraw(
    mut redraws: MessageReader<RequestRedraw>,
    mut wake: ResMut<LayerHostUpdateWake>,
) {
    if redraws.read().next().is_some() {
        wake.0 = true;
    }
}

fn take_layer_host_update_wake(app: &mut App) -> bool {
    std::mem::take(&mut app.world_mut().resource_mut::<LayerHostUpdateWake>().0)
}

fn run_layer_host_app_update(app: &mut App) -> bool {
    app.update();
    let redraw = take_layer_host_update_wake(app);
    let staged = staged_shell_commands_pending(app);
    let elapsed = app
        .world()
        .get_resource::<Time<Real>>()
        .map_or(Duration::ZERO, Time::elapsed);
    if let Some(frame) = app.world().get_resource::<ShellFrameState>() {
        match frame.0.wake {
            WakePolicy::WakeAt(deadline) => tracing::debug!(
                event = "quoin_wake_policy",
                policy = "wake-at",
                elapsed_us = elapsed.as_micros(),
                deadline_us = deadline.as_micros()
            ),
            WakePolicy::Animate => tracing::debug!(
                event = "quoin_wake_policy",
                policy = "animate",
                elapsed_us = elapsed.as_micros()
            ),
            WakePolicy::Idle => tracing::debug!(
                event = "quoin_wake_policy",
                policy = "none",
                elapsed_us = elapsed.as_micros()
            ),
        }
        if let Some(deadline) = frame.0.wake_deadline {
            tracing::debug!(
                event = "quoin_model_wake_deadline",
                result = "wake-at",
                elapsed_us = elapsed.as_micros(),
                deadline_us = deadline.as_micros()
            );
        } else {
            tracing::debug!(
                event = "quoin_model_wake_deadline",
                result = "none",
                elapsed_us = elapsed.as_micros()
            );
        }
    }
    if let Some(effects) = app
        .world()
        .get_resource::<cosmix_shell::runtime::ShellEffects>()
    {
        for effect in &effects.0 {
            tracing::debug!(
                event = "quoin_model_transition",
                edge = ?effect.edge,
                transition = ?effect.effect,
                elapsed_us = elapsed.as_micros()
            );
        }
    }
    redraw || staged
}

fn model_elapsed_at(time: &Time<Real>, instant: Instant) -> Duration {
    time.last_update().map_or(Duration::ZERO, |last_update| {
        time.elapsed()
            .saturating_add(instant.saturating_duration_since(last_update))
    })
}

fn update_sample_instant(app: &App, wake_timer: &WakeTimerState, now: Instant) -> Instant {
    let Some(time) = app.world().get_resource::<Time<Real>>() else {
        return now;
    };
    let elapsed = model_elapsed_at(time, now);
    let force_due = wake_timer.fired_deadline.is_some_and(|deadline| {
        wake_timer.early_rearmed_deadline == Some(deadline) && elapsed < deadline
    });
    if force_due {
        now.checked_add(
            wake_timer
                .fired_deadline
                .expect("force-due has a fired deadline")
                .saturating_sub(elapsed),
        )
        .unwrap_or(now)
    } else {
        now
    }
}

fn run_layer_host_app_update_at(app: &mut App, instant: Instant) -> bool {
    let expected_elapsed = app
        .world()
        .get_resource::<Time<Real>>()
        .map_or(Duration::ZERO, |time| model_elapsed_at(time, instant));
    app.insert_resource(TimeUpdateStrategy::ManualInstant(instant));
    let follow_up = run_layer_host_app_update(app);
    let actual_elapsed = app
        .world()
        .get_resource::<Time<Real>>()
        .map_or(Duration::ZERO, Time::elapsed);
    debug_assert_eq!(actual_elapsed, expected_elapsed);
    follow_up
}

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

trait ProtocolExecutor {
    fn execute(
        &mut self,
        panel: &mut PanelSurface,
        operations: &[ProtocolOp],
        elapsed: Duration,
    ) -> Result<ApplyResult, LayerHostError>;
}

struct WaylandProtocolExecutor;

impl ProtocolExecutor for WaylandProtocolExecutor {
    fn execute(
        &mut self,
        panel: &mut PanelSurface,
        operations: &[ProtocolOp],
        elapsed: Duration,
    ) -> Result<ApplyResult, LayerHostError> {
        Ok(panel.apply_protocol_ops(operations, elapsed))
    }
}

fn write_configured_motion_latch(app: &mut App, committed: Option<(Edge, PanelMode)>) -> bool {
    let Some((edge, mode)) = committed else {
        return false;
    };
    app.world_mut()
        .resource_mut::<QuoinCommittedMotionModes>()
        .set(edge, mode);
    true
}

fn handle_configure_success(
    app: &mut App,
    needs_update: &mut bool,
    committed: Option<(Edge, PanelMode)>,
) {
    write_configured_motion_latch(app, committed);
    *needs_update = true;
}

fn complete_configure_callback(
    app: &mut App,
    needs_update: &mut bool,
    abnormal_exit: &mut bool,
    exit_reason: &mut Option<String>,
    result: Result<Option<(Edge, PanelMode)>, SurfaceSizeError>,
) {
    match result {
        Ok(committed) => handle_configure_success(app, needs_update, committed),
        Err(error) => {
            *abnormal_exit = true;
            *exit_reason = Some(format!("configure-out-of-range-{}", error.reason_suffix()));
        }
    }
}

fn layer_close_decision(output_live: bool, replacement_pending: bool) -> LayerCloseDecision {
    if output_live && !replacement_pending {
        LayerCloseDecision::Exit
    } else {
        LayerCloseDecision::Retire
    }
}

trait PointerRelease {
    fn protocol_version(&self) -> u32;
    fn release(self);
}

impl PointerRelease for wl_pointer::WlPointer {
    fn protocol_version(&self) -> u32 {
        self.version()
    }

    fn release(self) {
        wl_pointer::WlPointer::release(&self);
    }
}

fn release_pointer(pointer: impl PointerRelease) {
    if pointer.protocol_version() >= 3 {
        pointer.release();
    }
}

fn next_timer_deadline(
    policy: WakePolicy,
    model_deadline: Option<Duration>,
    animate_backstop: Option<Duration>,
    configure_deadlines: &[Duration],
) -> Option<Duration> {
    let policy_deadline = match policy {
        WakePolicy::Idle | WakePolicy::Animate => None,
        WakePolicy::WakeAt(deadline) => Some(deadline),
    };
    let model_deadline = (policy != WakePolicy::Idle)
        .then_some(model_deadline)
        .flatten();
    let animate_backstop = (policy == WakePolicy::Animate)
        .then_some(animate_backstop)
        .flatten();
    policy_deadline
        .into_iter()
        .chain(model_deadline)
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

fn wake_timer_delay(deadline: Duration, elapsed: Duration, early_rearm: bool) -> Duration {
    let remaining = deadline.saturating_sub(elapsed);
    if early_rearm && !remaining.is_zero() {
        remaining.saturating_add(EARLY_TIMER_REARM_GUARD)
    } else {
        remaining
    }
}

fn observe_wake_timer_fire(elapsed: Duration, deadline: Duration) -> Option<Duration> {
    let early = elapsed < deadline;
    tracing::debug!(
        event = "quoin_wake_timer_fired",
        elapsed_us = elapsed.as_micros(),
        deadline_us = deadline.as_micros(),
        early
    );
    early.then_some(deadline)
}

trait WakeTimerTarget {
    fn clear_wake_timer_registration(&mut self);
    fn queue_wake_timer_fire(&mut self, deadline: Duration);
}

fn wake_timer_source_fired<S: WakeTimerTarget>(state: &mut S, deadline: Duration) {
    state.clear_wake_timer_registration();
    state.queue_wake_timer_fire(deadline);
}

#[derive(Default)]
struct WakeTimerState {
    token: Option<RegistrationToken>,
    deadline: Option<Duration>,
    fired_deadline: Option<Duration>,
    pending_early_rearm: Option<Duration>,
    early_rearmed_deadline: Option<Duration>,
    observed_past_deadline: Option<Duration>,
    consecutive_past_deadlines: u8,
    total_past_deadline_observations: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedWake {
    None,
    Early(Duration),
    Due(Duration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PastDeadline {
    NotPast,
    DueNow { first: bool },
    Stuck,
}

impl WakeTimerState {
    fn observe_queued_fire(&mut self, elapsed: Duration) -> QueuedWake {
        let Some(deadline) = self.fired_deadline.take() else {
            return QueuedWake::None;
        };
        if observe_wake_timer_fire(elapsed, deadline).is_some() {
            debug_assert_ne!(self.early_rearmed_deadline, Some(deadline));
            self.pending_early_rearm = Some(deadline);
            QueuedWake::Early(deadline)
        } else {
            QueuedWake::Due(deadline)
        }
    }

    fn observe_next_deadline(
        &mut self,
        next_deadline: Option<Duration>,
        elapsed: Duration,
        model_progressed: bool,
    ) -> PastDeadline {
        if next_deadline.is_none_or(|deadline| deadline > elapsed) {
            self.observed_past_deadline = None;
            self.consecutive_past_deadlines = 0;
            self.total_past_deadline_observations = 0;
            return PastDeadline::NotPast;
        }
        let deadline = next_deadline.expect("past deadline was checked above");
        let same_deadline = self.observed_past_deadline == Some(deadline);
        if !same_deadline {
            self.observed_past_deadline = Some(deadline);
            self.total_past_deadline_observations = 0;
        }
        if model_progressed || !same_deadline {
            self.consecutive_past_deadlines = 0;
        }
        self.consecutive_past_deadlines = self.consecutive_past_deadlines.saturating_add(1);
        self.total_past_deadline_observations =
            self.total_past_deadline_observations.saturating_add(1);
        if self.consecutive_past_deadlines >= MAX_CONSECUTIVE_PAST_DEADLINES
            || self.total_past_deadline_observations >= MAX_PAST_DEADLINE_OBSERVATIONS
        {
            PastDeadline::Stuck
        } else {
            PastDeadline::DueNow {
                first: self.total_past_deadline_observations == 1,
            }
        }
    }
}

fn replace_wake_timer_source<S: WakeTimerTarget + 'static>(
    loop_handle: &LoopHandle<'_, S>,
    timer: &mut WakeTimerState,
    next_deadline: Option<Duration>,
    elapsed: Duration,
) -> Result<(), LayerHostError> {
    if next_deadline.is_none() {
        if let Some(token) = timer.token.take() {
            loop_handle.remove(token);
        }
        timer.deadline = None;
        timer.pending_early_rearm = None;
        timer.early_rearmed_deadline = None;
        return Ok(());
    }
    if timer.deadline == next_deadline {
        return Ok(());
    }
    if let Some(token) = timer.token.take() {
        loop_handle.remove(token);
    }
    timer.early_rearmed_deadline = None;
    timer.deadline = next_deadline;
    let deadline = next_deadline.expect("no deadline returned above");
    let rearmed = timer.pending_early_rearm.take() == Some(deadline);
    if rearmed {
        timer.early_rearmed_deadline = Some(deadline);
    }
    let delay = wake_timer_delay(deadline, elapsed, rearmed);
    tracing::debug!(
        event = if rearmed {
            "quoin_wake_timer_rearmed"
        } else {
            "quoin_wake_timer_armed"
        },
        deadline_us = deadline.as_micros(),
        elapsed_us = elapsed.as_micros(),
        delay_us = delay.as_micros()
    );
    let token = loop_handle
        .insert_source(Timer::from_duration(delay), move |_, _, state| {
            wake_timer_source_fired(state, deadline);
            TimeoutAction::Drop
        })
        .map_err(|error| LayerHostError::new(error.to_string()))?;
    timer.token = Some(token);
    Ok(())
}

fn reset_wake_timer_for_output_replacement<S>(
    loop_handle: &LoopHandle<'_, S>,
    timer: &mut WakeTimerState,
) {
    if let Some(token) = timer.token.take() {
        loop_handle.remove(token);
    }
    *timer = WakeTimerState::default();
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
    app.init_resource::<LayerHostUpdateWake>()
        .add_systems(Last, capture_layer_host_redraw);
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
    last_wake_deadline: Option<Duration>,
    wake_timer: WakeTimerState,
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

impl WakeTimerTarget for RunnerState {
    fn clear_wake_timer_registration(&mut self) {
        self.wake_timer.deadline = None;
        self.wake_timer.token = None;
    }

    fn queue_wake_timer_fire(&mut self, deadline: Duration) {
        self.wake_timer.fired_deadline = Some(deadline);
        self.needs_update = true;
    }
}

impl RunnerState {
    #[allow(clippy::too_many_arguments)]
    fn reconcile<E: ProtocolExecutor>(
        app: &mut App,
        pointer_bridge: &mut PointerBridge,
        output_key: &OutputKey,
        panels: &mut [PanelSurface; 4],
        output_size: LogicalSize,
        frame: &cosmix_shell::runtime::ShellFrame,
        elapsed: Duration,
        executor: &mut E,
    ) -> Result<bool, LayerHostError> {
        let geometry = OutputGeometry {
            width: output_size.width(),
            height: output_size.height(),
        };
        let mut unmaps = Vec::new();
        let mut committed_modes = Vec::new();
        for edge in Edge::ALL {
            let panel = &mut panels[edge.index()];
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
            if operations.contains(&ProtocolOp::Unmap) {
                pointer_bridge.cleanup(app, output_key, Some(panel.window));
                panel.begin_unmap(app);
                unmaps.push((edge, next.clone()));
            }
            let awaiting_initial_configure =
                panel.phase == SurfacePhase::WaitingConfigure && panel.pending_committed.is_some();
            let advance = commit_advance_for_phase(
                executor.execute(panel, &operations, elapsed),
                operations.contains(&ProtocolOp::Unmap),
                awaiting_initial_configure,
            )?;
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
            // This non-pipelined update drains render extraction after raw handle
            // removal and before destroying the protocol objects.
            app.update();
            for (edge, next) in unmaps {
                panels[edge.index()].finish_unmap();
                panels[edge.index()].last_committed = Some(next.clone());
                committed_modes.push((edge, next.mode));
            }
        }
        let mut latch_changed = false;
        if !committed_modes.is_empty() {
            let changed = {
                let mut modes = app.world_mut().resource_mut::<QuoinCommittedMotionModes>();
                update_committed_modes(&mut modes, committed_modes)
            };
            latch_changed = changed;
        }
        Ok(latch_changed)
    }
}

trait RunnerIteration<Context> {
    fn begin_update(&mut self, now: Instant);
    fn app_exit(&mut self) -> Option<AppExit>;
    fn record_app_exit(&mut self, exit: AppExit);
    fn reconcile_iteration(&mut self, context: &Context) -> Result<(), LayerHostError>;
    fn replace_timer_iteration(&mut self, context: &Context) -> Result<(), LayerHostError>;
    fn flush_iteration(&mut self, context: &Context) -> Result<(), LayerHostError>;
    fn fail_iteration(&mut self, stage: &'static str, error: LayerHostError);
}

fn run_update_iteration<State, Context>(state: &mut State, context: &Context, now: Instant)
where
    State: RunnerIteration<Context>,
{
    state.begin_update(now);
    if let Some(exit) = state.app_exit() {
        state.record_app_exit(exit);
        return;
    }
    if let Err(error) = state.reconcile_iteration(context) {
        state.fail_iteration("surface-plan", error);
        return;
    }
    if let Err(error) = state.replace_timer_iteration(context) {
        state.fail_iteration("wake-timer", error);
        return;
    }
    if let Err(error) = state.flush_iteration(context) {
        state.fail_iteration("wayland-flush", error);
    }
}

impl<'loop_handle>
    RunnerIteration<(
        &QueueHandle<RunnerState>,
        &LoopHandle<'loop_handle, RunnerState>,
    )> for RunnerState
{
    fn begin_update(&mut self, now: Instant) {
        self.needs_update = false;
        let sample = update_sample_instant(&self.app, &self.wake_timer, now);
        if run_layer_host_app_update_at(&mut self.app, sample) {
            self.needs_update = true;
        }
        let elapsed = self
            .app
            .world()
            .get_resource::<Time<Real>>()
            .map_or(Duration::ZERO, Time::elapsed);
        self.handle_queued_wake_timer(elapsed);
    }

    fn app_exit(&mut self) -> Option<AppExit> {
        self.app.should_exit()
    }

    fn record_app_exit(&mut self, exit: AppExit) {
        self.abnormal_exit = exit.is_error();
        self.exit_reason = Some(if exit.is_error() {
            "bevy-app-error".to_owned()
        } else {
            "bevy-app-exit".to_owned()
        });
    }

    fn reconcile_iteration(
        &mut self,
        context: &(
            &QueueHandle<RunnerState>,
            &LoopHandle<'loop_handle, RunnerState>,
        ),
    ) -> Result<(), LayerHostError> {
        self.reconcile_live(context.0)
    }

    fn replace_timer_iteration(
        &mut self,
        context: &(
            &QueueHandle<RunnerState>,
            &LoopHandle<'loop_handle, RunnerState>,
        ),
    ) -> Result<(), LayerHostError> {
        self.replace_wake_timer(context.1)
    }

    fn flush_iteration(
        &mut self,
        _context: &(
            &QueueHandle<RunnerState>,
            &LoopHandle<'loop_handle, RunnerState>,
        ),
    ) -> Result<(), LayerHostError> {
        self.connection
            .flush()
            .map_err(|error| LayerHostError::new(error.to_string()))
    }

    fn fail_iteration(&mut self, stage: &'static str, error: LayerHostError) {
        self.abnormal_exit = true;
        self.exit_reason = Some(format!("{stage}-failed-{error}"));
    }
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
        last_wake_deadline: None,
        wake_timer: WakeTimerState::default(),
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
            if let Err(error) = state.replace_selected_output(&qh, &loop_handle) {
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
            // TODO(slice-2.1): replace the headless mirror with an injected production-loop
            // harness if Wayland and calloop dependencies become constructible in tests.
            run_update_iteration(&mut state, &(&qh, &loop_handle), Instant::now());
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
    state.wake_timer.token = None;
    state.apply_corner_ingress(CornerIngress::Reset {
        epoch: state.corner_epoch,
    });
    if let Some(output) = state.selected_key.clone() {
        state.pointer_bridge.cleanup(&mut state.app, &output, None);
    }
    if let Some(pointer) = state.active_pointer.take() {
        release_pointer(pointer);
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

    fn replace_selected_output(
        &mut self,
        qh: &QueueHandle<Self>,
        loop_handle: &LoopHandle<'_, Self>,
    ) -> Result<(), LayerHostError> {
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
        self.last_wake_deadline = None;
        reset_wake_timer_for_output_replacement(loop_handle, &mut self.wake_timer);
        self.needs_update = true;
        Ok(())
    }

    fn reconcile_live(&mut self, qh: &QueueHandle<Self>) -> Result<(), LayerHostError> {
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
            pointer_bridge,
            needs_update,
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
        for edge in Edge::ALL {
            let panel = &mut output.panels[edge.index()];
            let next = frame.panel(edge);
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
        }
        if Self::reconcile(
            app,
            pointer_bridge,
            key,
            &mut output.panels,
            output.logical_size,
            &frame,
            elapsed,
            &mut WaylandProtocolExecutor,
        )? {
            *needs_update = true;
        }
        self.last_wake = frame.wake;
        self.last_wake_deadline = frame.wake_deadline;
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
        let next_deadline = next_timer_deadline(
            self.last_wake,
            self.last_wake_deadline,
            animate_backstop,
            &configure_deadlines,
        );
        let model_progressed = self
            .app
            .world()
            .get_resource::<cosmix_shell::runtime::ShellEffects>()
            .is_some_and(|effects| !effects.0.is_empty());
        let past = self
            .wake_timer
            .observe_next_deadline(next_deadline, elapsed, model_progressed);
        match past {
            PastDeadline::DueNow { .. } | PastDeadline::Stuck => {
                let deadline = next_deadline.expect("past observation has a deadline");
                if matches!(past, PastDeadline::DueNow { first: true }) {
                    tracing::warn!(
                        event = "quoin_wake_deadline_unconsumed",
                        elapsed_us = elapsed.as_micros(),
                        deadline_us = deadline.as_micros()
                    );
                }
                if let Some(token) = self.wake_timer.token.take() {
                    loop_handle.remove(token);
                }
                self.wake_timer.deadline = None;
                self.wake_timer.pending_early_rearm = None;
                self.handle_due_wake_timer(elapsed);
                if past == PastDeadline::Stuck {
                    tracing::error!(
                        event = "quoin_wake_deadline_stuck",
                        elapsed_us = elapsed.as_micros(),
                        deadline_us = deadline.as_micros(),
                        consecutive = self.wake_timer.consecutive_past_deadlines,
                        total = self.wake_timer.total_past_deadline_observations
                    );
                    self.abnormal_exit = true;
                    self.exit_reason = Some("wake-deadline-stuck".to_owned());
                }
                return Ok(());
            }
            PastDeadline::NotPast => {}
        }
        replace_wake_timer_source(loop_handle, &mut self.wake_timer, next_deadline, elapsed)
    }

    fn handle_queued_wake_timer(&mut self, elapsed: Duration) {
        if matches!(
            self.wake_timer.observe_queued_fire(elapsed),
            QueuedWake::Due(_)
        ) {
            self.handle_due_wake_timer(elapsed);
        }
    }

    fn handle_due_wake_timer(&mut self, elapsed: Duration) {
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
            // TODO(slice-2.1): quarantining or recreating a panel after the existing
            // fatal configure timeout needs an explicit lifecycle design.
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
        let RunnerState {
            app,
            outputs,
            needs_update,
            abnormal_exit,
            exit_reason,
            ..
        } = self;
        let result = if let Some(panel) = outputs
            .values_mut()
            .flat_map(|output| output.panels.iter_mut())
            .find(|panel| panel.matches_layer(layer))
        {
            // SCTK acknowledged the configure before invoking this callback.
            match panel.configure(app, qh, &configure, elapsed) {
                Ok(mode) => Ok(mode.map(|mode| (panel.edge, mode))),
                Err(error) => Err(error),
            }
        } else {
            Ok(None)
        };
        complete_configure_callback(app, needs_update, abnormal_exit, exit_reason, result);
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
        match &ingress {
            CornerIngress::Event {
                output,
                epoch,
                event,
            } => tracing::debug!(
                event = "quoin_corner_ingress_applied",
                kind = match event {
                    cosmix_shell::core::CornerEvent::Entered { .. } => "entered",
                    cosmix_shell::core::CornerEvent::Left { .. } => "left",
                },
                output = output.as_str(),
                epoch
            ),
            CornerIngress::Reset { epoch } => tracing::debug!(
                event = "quoin_corner_ingress_applied",
                kind = "reset",
                output = self.selected_key.as_ref().map_or("none", OutputKey::as_str),
                epoch
            ),
            CornerIngress::Disabled { epoch } => tracing::debug!(
                event = "quoin_corner_ingress_applied",
                kind = "disabled",
                output = self.selected_key.as_ref().map_or("none", OutputKey::as_str),
                epoch
            ),
        }
        self.corner_epoch = self.corner_epoch.max(ingress.epoch());
        if apply_corner_ingress_to_app(
            &mut self.app,
            self.selected_key.as_ref(),
            &mut self.corner_engaged,
            ingress,
        ) {
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
            release_pointer(pointer);
        }
        self.active_pointer_seat = None;
        self.promote_pointer(qh);
    }
}

pub(crate) fn apply_corner_ingress_to_app(
    app: &mut App,
    selected_key: Option<&OutputKey>,
    corner_engaged: &mut BTreeSet<cosmix_shell::core::Corner>,
    ingress: CornerIngress,
) -> bool {
    let resets = matches!(
        &ingress,
        CornerIngress::Reset { .. } | CornerIngress::Disabled { .. }
    );
    let events = match ingress {
        CornerIngress::Event { output, event, .. } if selected_key == Some(&output) => {
            let corner = event.corner();
            let changed = match event {
                cosmix_shell::core::CornerEvent::Entered { .. } => corner_engaged.insert(corner),
                cosmix_shell::core::CornerEvent::Left { .. } => corner_engaged.remove(&corner),
            };
            changed.then_some(event).into_iter().collect::<Vec<_>>()
        }
        CornerIngress::Event { .. } => Vec::new(),
        CornerIngress::Reset { .. } | CornerIngress::Disabled { .. } => corner_engaged
            .iter()
            .copied()
            .map(|corner| cosmix_shell::core::CornerEvent::Left { corner })
            .collect::<Vec<_>>(),
    };
    if resets {
        corner_engaged.clear();
    }
    let Some(output) = selected_key.cloned() else {
        return false;
    };
    let changed = !events.is_empty();
    for event in events {
        stage_shell_command(app, output.clone(), ShellCommandKind::Corner(event));
    }
    changed
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
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use bevy::MinimalPlugins;
    use bevy::time::TimeUpdateStrategy;
    use cosmix_shell::core::{
        ConcealReason, Corner, CornerEvent, CornerTrigger, PanelEffect, PanelMode,
    };
    use cosmix_shell::runtime::{ShellEffects, ShellRuntimePlugin};

    use crate::surface::frame_request_overdue;

    struct FakePointer {
        version: u32,
        releases: Arc<Mutex<u32>>,
    }

    struct ScriptedExecutor {
        results: VecDeque<Result<ApplyResult, &'static str>>,
        operations: Vec<(Edge, Vec<ProtocolOp>)>,
    }

    enum TestRunnerIngress {
        Corner(CornerEvent),
    }

    struct TestRunnerState {
        app: App,
        output: OutputKey,
        needs_update: bool,
        wake_timer: WakeTimerState,
        fresh_elapsed: Duration,
        last_timer_delay: Option<Duration>,
        last_timer_rearmed: bool,
        animate_requested_at: Option<Duration>,
        abnormal_exit: bool,
        exit_reason: Option<String>,
        iteration_steps: Vec<&'static str>,
    }

    impl WakeTimerTarget for TestRunnerState {
        fn clear_wake_timer_registration(&mut self) {
            self.wake_timer.deadline = None;
            self.wake_timer.token = None;
        }

        fn queue_wake_timer_fire(&mut self, deadline: Duration) {
            self.wake_timer.fired_deadline = Some(deadline);
            self.needs_update = true;
        }
    }

    impl<'loop_handle> RunnerIteration<LoopHandle<'loop_handle, TestRunnerState>> for TestRunnerState {
        fn begin_update(&mut self, _now: Instant) {
            // Mirrors the production RunnerState::begin_update timer/update ordering.
            self.iteration_steps.push("update");
            self.needs_update = false;
            let model_elapsed = self.app.world().resource::<Time<Real>>().elapsed();
            if self.fresh_elapsed > model_elapsed {
                *self.app.world_mut().resource_mut::<TimeUpdateStrategy>() =
                    TimeUpdateStrategy::ManualDuration(self.fresh_elapsed - model_elapsed);
            }
            if let Some(deadline) = self.wake_timer.fired_deadline
                && self.wake_timer.early_rearmed_deadline == Some(deadline)
                && self.fresh_elapsed < deadline
            {
                *self.app.world_mut().resource_mut::<TimeUpdateStrategy>() =
                    TimeUpdateStrategy::ManualDuration(deadline - model_elapsed);
                self.fresh_elapsed = deadline;
            }
            if run_layer_host_app_update(&mut self.app) {
                self.needs_update = true;
            }
            let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
            self.fresh_elapsed = self.fresh_elapsed.max(elapsed);
            if matches!(
                self.wake_timer.observe_queued_fire(self.fresh_elapsed),
                QueuedWake::Due(_)
            ) {
                self.needs_update = true;
            }
        }

        fn app_exit(&mut self) -> Option<AppExit> {
            self.iteration_steps.push("should-exit");
            self.app.should_exit()
        }

        fn record_app_exit(&mut self, exit: AppExit) {
            self.abnormal_exit = exit.is_error();
            self.exit_reason = Some("test-app-exit".to_owned());
        }

        fn reconcile_iteration(
            &mut self,
            _handle: &LoopHandle<'loop_handle, TestRunnerState>,
        ) -> Result<(), LayerHostError> {
            self.iteration_steps.push("reconcile");
            Ok(())
        }

        fn replace_timer_iteration(
            &mut self,
            handle: &LoopHandle<'loop_handle, TestRunnerState>,
        ) -> Result<(), LayerHostError> {
            // Mirrors the production RunnerState::replace_wake_timer decision path.
            self.iteration_steps.push("replace-timer");
            let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
            let frame = &self.app.world().resource::<ShellFrameState>().0;
            let animate_backstop = self.animate_requested_at.map(animate_backstop_deadline);
            let next_deadline =
                next_timer_deadline(frame.wake, frame.wake_deadline, animate_backstop, &[]);
            let past = self.wake_timer.observe_next_deadline(
                next_deadline,
                self.fresh_elapsed,
                !self.app.world().resource::<ShellEffects>().0.is_empty(),
            );
            if past != PastDeadline::NotPast {
                if let Some(token) = self.wake_timer.token.take() {
                    handle.remove(token);
                }
                self.wake_timer.deadline = None;
                self.wake_timer.pending_early_rearm = None;
                self.animate_requested_at = None;
                self.needs_update = true;
                if past == PastDeadline::Stuck {
                    self.abnormal_exit = true;
                    self.exit_reason = Some("wake-deadline-stuck".to_owned());
                }
                return Ok(());
            }
            if self.wake_timer.deadline != next_deadline
                && let Some(deadline) = next_deadline
            {
                self.last_timer_rearmed = self.wake_timer.pending_early_rearm == Some(deadline);
                self.last_timer_delay =
                    Some(wake_timer_delay(deadline, elapsed, self.last_timer_rearmed));
            }
            replace_wake_timer_source(handle, &mut self.wake_timer, next_deadline, elapsed)
        }

        fn flush_iteration(
            &mut self,
            _handle: &LoopHandle<'loop_handle, TestRunnerState>,
        ) -> Result<(), LayerHostError> {
            self.iteration_steps.push("flush");
            Ok(())
        }

        fn fail_iteration(&mut self, stage: &'static str, error: LayerHostError) {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("{stage}-failed-{error}"));
        }
    }

    fn drive_test_runner_update(
        state: &mut TestRunnerState,
        handle: &LoopHandle<'_, TestRunnerState>,
    ) {
        assert!(state.needs_update);
        run_update_iteration(state, handle, Instant::now());
    }

    fn force_test_timer_one_quantum_early(
        state: &mut TestRunnerState,
        handle: &LoopHandle<'_, TestRunnerState>,
    ) {
        let deadline = state.wake_timer.deadline.expect("grace timer is armed");
        if let Some(token) = state.wake_timer.token.take() {
            handle.remove(token);
        }
        state.wake_timer.deadline = Some(deadline);
        state.wake_timer.token = Some(
            handle
                .insert_source(Timer::immediate(), move |_, _, state| {
                    wake_timer_source_fired(state, deadline);
                    TimeoutAction::Drop
                })
                .unwrap(),
        );
    }

    type TeardownSequences = [Arc<Mutex<Vec<&'static str>>>; 4];

    #[derive(Resource)]
    struct LateActivation {
        output: OutputKey,
        armed: bool,
    }

    #[derive(Resource)]
    struct LateStagedCommand {
        output: OutputKey,
        armed: bool,
    }

    fn emit_late_activation(
        time: Res<Time<Real>>,
        mut activation: ResMut<LateActivation>,
        mut commands: MessageWriter<ShellCommand>,
        mut redraw: MessageWriter<RequestRedraw>,
    ) {
        if !activation.armed {
            return;
        }
        activation.armed = false;
        commands.write(ShellCommand {
            output: activation.output.clone(),
            at: time.elapsed(),
            kind: ShellCommandKind::Panel {
                edge: Edge::Left,
                input: cosmix_shell::core::PanelInput::Pin,
            },
        });
        redraw.write(RequestRedraw);
    }

    fn emit_late_staged_command(world: &mut World) {
        let output = {
            let mut late = world.resource_mut::<LateStagedCommand>();
            if !late.armed {
                return;
            }
            late.armed = false;
            late.output.clone()
        };
        crate::input::stage_shell_command_world(
            world,
            output,
            ShellCommandKind::Panel {
                edge: Edge::Left,
                input: cosmix_shell::core::PanelInput::Pin,
            },
        );
    }

    fn refuse_to_consume_wake_deadline(mut frame: ResMut<ShellFrameState>) {
        let deadline = Duration::from_millis(1);
        frame.0.wake = WakePolicy::WakeAt(deadline);
        frame.0.wake_deadline = Some(deadline);
    }

    impl ProtocolExecutor for ScriptedExecutor {
        fn execute(
            &mut self,
            panel: &mut PanelSurface,
            operations: &[ProtocolOp],
            _elapsed: Duration,
        ) -> Result<ApplyResult, LayerHostError> {
            if operations.is_empty() {
                return Ok(ApplyResult::Noop);
            }
            self.operations.push((panel.edge, operations.to_vec()));
            self.results
                .pop_front()
                .unwrap_or(Ok(ApplyResult::Noop))
                .map_err(LayerHostError::new)
        }
    }

    fn reconciliation_panels(
        app: &mut App,
        phase: SurfacePhase,
    ) -> ([PanelSurface; 4], TeardownSequences) {
        let sequences = std::array::from_fn(|_| Arc::new(Mutex::new(Vec::new())));
        let panels = std::array::from_fn(|index| {
            let mut panel = PanelSurface::test_double(app, phase, Arc::clone(&sequences[index]));
            panel.edge = Edge::ALL[index];
            panel
        });
        (panels, sequences)
    }

    impl PointerRelease for FakePointer {
        fn protocol_version(&self) -> u32 {
            self.version
        }

        fn release(self) {
            *self.releases.lock().unwrap() += 1;
        }
    }

    #[test]
    fn pointer_release_is_sent_only_when_the_advertised_version_supports_it() {
        let releases = Arc::new(Mutex::new(0));
        release_pointer(FakePointer {
            version: 2,
            releases: Arc::clone(&releases),
        });
        assert_eq!(*releases.lock().unwrap(), 0);

        release_pointer(FakePointer {
            version: 3,
            releases: Arc::clone(&releases),
        });
        assert_eq!(*releases.lock().unwrap(), 1);
    }

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
    fn late_pointer_activation_gets_exactly_one_host_owned_follow_up_update() {
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
        app.add_message::<RequestRedraw>()
            .init_resource::<LayerHostUpdateWake>()
            .insert_resource(LateActivation {
                output,
                armed: true,
            })
            .add_systems(Last, capture_layer_host_redraw)
            .add_systems(
                Update,
                emit_late_activation.in_set(cosmix_shell::runtime::ShellRuntimeSet::Host),
            );

        assert!(
            run_layer_host_app_update(&mut app),
            "the production loop must request one host follow-up"
        );
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Hidden,
            "the activation command was produced after this update's Model set"
        );

        assert!(!run_layer_host_app_update(&mut app));
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Pinned,
            "the single follow-up consumes the activation command"
        );
    }

    #[test]
    fn command_staged_after_model_forces_one_production_loop_follow_up() {
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
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .add_message::<RequestRedraw>()
            .init_resource::<LayerHostUpdateWake>()
            .insert_resource(LateStagedCommand {
                output,
                armed: true,
            })
            .add_systems(
                Update,
                emit_late_staged_command.in_set(cosmix_shell::runtime::ShellRuntimeSet::Host),
            );

        assert!(run_layer_host_app_update(&mut app));
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Hidden
        );
        assert!(!run_layer_host_app_update(&mut app));
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Pinned
        );
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
    fn production_reconciliation_commits_configure_latch_rejects_failure_and_unmaps() {
        let output = OutputKey::new("DP-1").unwrap();
        let mut model = ShellModel::new(
            output.clone(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let hidden_frame = cosmix_shell::runtime::ShellFrame::from_model(&model);
        model
            .panel_input(
                Edge::Left,
                Duration::ZERO,
                cosmix_shell::core::PanelInput::Reveal,
            )
            .unwrap();
        let revealed_frame = cosmix_shell::runtime::ShellFrame::from_model(&model);

        let mut app = App::new();
        app.insert_resource(QuoinCommittedMotionModes::hidden());
        let (mut panels, sequences) = reconciliation_panels(&mut app, SurfacePhase::Configured);
        for edge in Edge::ALL {
            panels[edge.index()].last_committed = Some(hidden_frame.panel(edge).clone());
        }
        let mut pointer = PointerBridge::default();
        let mut deferred = ScriptedExecutor {
            results: VecDeque::from([Ok(ApplyResult::AwaitingConfigure)]),
            operations: Vec::new(),
        };
        assert!(
            !RunnerState::reconcile(
                &mut app,
                &mut pointer,
                &output,
                &mut panels,
                LogicalSize::new(1_000.0, 800.0).unwrap(),
                &revealed_frame,
                Duration::ZERO,
                &mut deferred,
            )
            .unwrap()
        );
        assert_eq!(
            panels[Edge::Left.index()].pending_committed.as_ref(),
            Some(revealed_frame.panel(Edge::Left))
        );
        assert_eq!(
            app.world()
                .resource::<QuoinCommittedMotionModes>()
                .get(Edge::Left),
            PanelMode::Hidden
        );

        let panel = &mut panels[Edge::Left.index()];
        panel.phase = SurfacePhase::Configured;
        let configured = panel
            .commit_pending_configure()
            .map(|mode| (Edge::Left, mode));
        let mut configure_wake = false;
        let mut configure_abnormal = false;
        let mut configure_exit_reason = None;
        complete_configure_callback(
            &mut app,
            &mut configure_wake,
            &mut configure_abnormal,
            &mut configure_exit_reason,
            Ok(configured),
        );
        assert!(
            configure_wake,
            "deleting the production configure completion call loses this wake"
        );
        assert!(!configure_abnormal);
        assert_eq!(configure_exit_reason, None);
        assert_eq!(
            app.world()
                .resource::<QuoinCommittedMotionModes>()
                .get(Edge::Left),
            PanelMode::Revealed,
            "deleting the configured-latch resource write leaves this Hidden"
        );

        model
            .panel_input(
                Edge::Left,
                Duration::from_millis(1),
                cosmix_shell::core::PanelInput::Pin,
            )
            .unwrap();
        let pinned_frame = cosmix_shell::runtime::ShellFrame::from_model(&model);
        let mut failing = ScriptedExecutor {
            results: VecDeque::from([Err("executor failed")]),
            operations: Vec::new(),
        };
        assert!(
            RunnerState::reconcile(
                &mut app,
                &mut pointer,
                &output,
                &mut panels,
                LogicalSize::new(1_000.0, 800.0).unwrap(),
                &pinned_frame,
                Duration::from_millis(1),
                &mut failing,
            )
            .is_err()
        );
        assert_eq!(
            panels[Edge::Left.index()].last_committed.as_ref(),
            Some(revealed_frame.panel(Edge::Left))
        );
        assert_eq!(
            app.world()
                .resource::<QuoinCommittedMotionModes>()
                .get(Edge::Left),
            PanelMode::Revealed
        );

        let mut unmapping = ScriptedExecutor {
            results: VecDeque::from([Ok(ApplyResult::Noop)]),
            operations: Vec::new(),
        };
        assert!(
            RunnerState::reconcile(
                &mut app,
                &mut pointer,
                &output,
                &mut panels,
                LogicalSize::new(1_000.0, 800.0).unwrap(),
                &hidden_frame,
                Duration::from_millis(2),
                &mut unmapping,
            )
            .unwrap()
        );
        assert_eq!(panels[Edge::Left.index()].phase, SurfacePhase::Unmapped);
        assert_eq!(
            app.world()
                .resource::<QuoinCommittedMotionModes>()
                .get(Edge::Left),
            PanelMode::Hidden
        );
        assert_eq!(sequences[Edge::Left.index()].lock().unwrap().len(), 4);
        assert!(unmapping.operations[0].1.contains(&ProtocolOp::Unmap));
    }

    #[test]
    fn pending_first_map_whose_grace_expires_unmaps_through_reconciliation() {
        let output = OutputKey::new("DP-1").unwrap();
        let mut model = ShellModel::new(
            output.clone(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        model
            .panel_input(
                Edge::Left,
                Duration::ZERO,
                cosmix_shell::core::PanelInput::CornerEntered,
            )
            .unwrap();
        let pending_reveal = cosmix_shell::runtime::ShellFrame::from_model(&model);
        model
            .panel_input(
                Edge::Left,
                Duration::from_millis(1),
                cosmix_shell::core::PanelInput::CornerLeft,
            )
            .unwrap();
        model.tick(Duration::from_millis(1_001)).unwrap();
        let expired = cosmix_shell::runtime::ShellFrame::from_model(&model);
        assert!(!expired.panel(Edge::Left).mapped);

        let mut app = App::new();
        app.insert_resource(QuoinCommittedMotionModes::hidden());
        let (mut panels, sequences) =
            reconciliation_panels(&mut app, SurfacePhase::WaitingConfigure);
        panels[Edge::Left.index()].pending_committed =
            Some(pending_reveal.panel(Edge::Left).clone());
        for edge in [Edge::Bottom, Edge::Right, Edge::Top] {
            panels[edge.index()].last_committed = Some(expired.panel(edge).clone());
        }
        let mut executor = ScriptedExecutor {
            results: VecDeque::from([Ok(ApplyResult::Noop)]),
            operations: Vec::new(),
        };

        RunnerState::reconcile(
            &mut app,
            &mut PointerBridge::default(),
            &output,
            &mut panels,
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            &expired,
            Duration::from_millis(1_001),
            &mut executor,
        )
        .unwrap();

        assert_eq!(panels[Edge::Left.index()].phase, SurfacePhase::Unmapped);
        assert!(panels[Edge::Left.index()].pending_committed.is_none());
        assert_eq!(
            panels[Edge::Left.index()].last_committed.as_ref(),
            Some(expired.panel(Edge::Left))
        );
        assert_eq!(sequences[Edge::Left.index()].lock().unwrap().len(), 4);
        assert!(executor.operations[0].1.contains(&ProtocolOp::Unmap));
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
        assert_eq!(
            next_timer_deadline(left.wake, left.wake_deadline, None, &[]),
            Some(deadline)
        );

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
    fn production_calloop_rearms_an_early_grace_timer_and_conceals_fifty_times() {
        for iteration in 0..50 {
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
            app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
                .add_message::<RequestRedraw>()
                .init_resource::<LayerHostUpdateWake>()
                .add_systems(Last, capture_layer_host_redraw)
                .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
            let mut state = TestRunnerState {
                app,
                output: output.clone(),
                needs_update: false,
                wake_timer: WakeTimerState::default(),
                fresh_elapsed: Duration::ZERO,
                last_timer_delay: None,
                last_timer_rearmed: false,
                animate_requested_at: None,
                abnormal_exit: false,
                exit_reason: None,
                iteration_steps: Vec::new(),
            };
            let mut event_loop: EventLoop<TestRunnerState> = EventLoop::try_new().unwrap();
            let handle = event_loop.handle();
            let (sender, channel) = calloop::channel::sync_channel(4);
            handle
                .insert_source(channel, |event, _, state| {
                    if let calloop::channel::Event::Msg(TestRunnerIngress::Corner(event)) = event {
                        stage_shell_command(
                            &mut state.app,
                            state.output.clone(),
                            ShellCommandKind::Corner(event),
                        );
                        state.needs_update = true;
                    }
                })
                .unwrap();

            sender
                .try_send(TestRunnerIngress::Corner(CornerEvent::Entered {
                    corner: Corner::TopLeft,
                    dwell: Duration::from_millis(200),
                    trigger: CornerTrigger::Compositor,
                }))
                .unwrap();
            event_loop.dispatch(None, &mut state).unwrap();
            if iteration == 0 {
                state.animate_requested_at = Some(Duration::ZERO);
                state.fresh_elapsed = Duration::from_millis(1_600);
            }
            drive_test_runner_update(&mut state, &handle);

            if iteration == 0 {
                assert_eq!(
                    state.iteration_steps,
                    [
                        "update",
                        "should-exit",
                        "reconcile",
                        "replace-timer",
                        "flush"
                    ],
                    "deleting or reordering a production update-iteration phase breaks this"
                );
                assert!(state.needs_update, "a past backstop is due-now");
                assert!(!state.abnormal_exit, "a slow first map is recoverable");
                assert_eq!(state.exit_reason, None);
                drive_test_runner_update(&mut state, &handle);
            }

            let revealed = state
                .app
                .world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .clone();
            assert!(revealed.mapped, "iteration {iteration}: enter must map");

            *state.app.world_mut().resource_mut::<TimeUpdateStrategy>() =
                TimeUpdateStrategy::ManualDuration(Duration::ZERO);
            sender
                .try_send(TestRunnerIngress::Corner(CornerEvent::Left {
                    corner: Corner::TopLeft,
                }))
                .unwrap();
            event_loop.dispatch(None, &mut state).unwrap();
            drive_test_runner_update(&mut state, &handle);
            let deadline = state.wake_timer.deadline.expect("left arms grace timer");
            let now = state.app.world().resource::<Time<Real>>().elapsed();
            let before = deadline - Duration::from_millis(1);
            *state.app.world_mut().resource_mut::<TimeUpdateStrategy>() =
                TimeUpdateStrategy::ManualDuration(before - now);
            state.fresh_elapsed = before;
            state.needs_update = true;
            drive_test_runner_update(&mut state, &handle);

            force_test_timer_one_quantum_early(&mut state, &handle);
            event_loop.dispatch(None, &mut state).unwrap();
            *state.app.world_mut().resource_mut::<TimeUpdateStrategy>() =
                TimeUpdateStrategy::ManualDuration(Duration::ZERO);
            drive_test_runner_update(&mut state, &handle);
            assert_eq!(
                state
                    .app
                    .world()
                    .resource::<ShellFrameState>()
                    .0
                    .panel(Edge::Left)
                    .mode,
                PanelMode::Revealed,
                "iteration {iteration}: early timer must not conceal"
            );
            assert_eq!(state.wake_timer.deadline, Some(deadline));
            assert!(state.wake_timer.token.is_some(), "early timer must re-arm");
            assert!(state.last_timer_rearmed);
            assert_eq!(state.last_timer_delay, Some(Duration::from_millis(2)));

            force_test_timer_one_quantum_early(&mut state, &handle);
            event_loop.dispatch(None, &mut state).unwrap();
            drive_test_runner_update(&mut state, &handle);
            assert!(
                state.wake_timer.token.is_none(),
                "iteration {iteration}: the second early firing is due-now, never a third arm"
            );
            let conceal_effects = state.app.world().resource::<ShellEffects>().0.clone();
            let after_motion = deadline + Duration::from_millis(201);
            state.fresh_elapsed = after_motion;
            drive_test_runner_update(&mut state, &handle);
            let frame = &state.app.world().resource::<ShellFrameState>().0;
            let concealed = frame.panel(Edge::Left);
            assert_eq!(concealed.mode, PanelMode::Hidden);
            assert!(!concealed.mapped, "iteration {iteration}: panel must unmap");
            assert_eq!(
                conceal_effects,
                [cosmix_shell::runtime::ShellEffect {
                    edge: Edge::Left,
                    effect: PanelEffect::Conceal {
                        reason: ConcealReason::CornerLeft,
                    },
                }]
            );
            assert_eq!(
                plan_surface(
                    Some(&revealed),
                    concealed,
                    OutputGeometry {
                        width: 1_000.0,
                        height: 800.0,
                    },
                )
                .unwrap(),
                [ProtocolOp::Unmap]
            );
        }
    }

    #[test]
    fn timer_selection_uses_the_earliest_bounded_one_shot() {
        let seconds = Duration::from_secs;
        assert_eq!(
            next_timer_deadline(
                WakePolicy::Animate,
                None,
                Some(seconds(6)),
                &[seconds(7), seconds(12)]
            ),
            Some(seconds(6))
        );
        assert_eq!(
            next_timer_deadline(
                WakePolicy::WakeAt(seconds(8)),
                None,
                None,
                &[seconds(7), seconds(12)]
            ),
            Some(seconds(7))
        );
        assert_eq!(
            next_timer_deadline(WakePolicy::Idle, None, None, &[seconds(10)]),
            Some(seconds(10))
        );
    }

    #[test]
    fn output_replacement_removes_the_armed_timer_source() {
        let mut state = TestRunnerState {
            app: App::new(),
            output: OutputKey::new("DP-1").unwrap(),
            needs_update: false,
            wake_timer: WakeTimerState::default(),
            fresh_elapsed: Duration::ZERO,
            last_timer_delay: None,
            last_timer_rearmed: false,
            animate_requested_at: None,
            abnormal_exit: false,
            exit_reason: None,
            iteration_steps: Vec::new(),
        };
        let mut event_loop: EventLoop<TestRunnerState> = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let deadline = Duration::from_millis(10);
        state.wake_timer.deadline = Some(deadline);
        state.wake_timer.token = Some(
            handle
                .insert_source(Timer::immediate(), move |_, _, state| {
                    wake_timer_source_fired(state, deadline);
                    TimeoutAction::Drop
                })
                .unwrap(),
        );

        reset_wake_timer_for_output_replacement(&handle, &mut state.wake_timer);
        event_loop
            .dispatch(Some(Duration::ZERO), &mut state)
            .unwrap();

        assert!(state.wake_timer.token.is_none());
        assert!(state.wake_timer.fired_deadline.is_none());
        assert!(
            !state.needs_update,
            "the retired source must not wake fresh state"
        );
    }

    #[test]
    fn cancel_after_early_fire_forgets_rearm_state_for_the_same_deadline() {
        let mut state = TestRunnerState {
            app: App::new(),
            output: OutputKey::new("DP-1").unwrap(),
            needs_update: false,
            wake_timer: WakeTimerState::default(),
            fresh_elapsed: Duration::ZERO,
            last_timer_delay: None,
            last_timer_rearmed: false,
            animate_requested_at: None,
            abnormal_exit: false,
            exit_reason: None,
            iteration_steps: Vec::new(),
        };
        let event_loop: EventLoop<TestRunnerState> = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let deadline = Duration::from_millis(100);
        state.wake_timer.pending_early_rearm = Some(deadline);

        replace_wake_timer_source(&handle, &mut state.wake_timer, None, Duration::ZERO).unwrap();
        assert!(state.wake_timer.pending_early_rearm.is_none());
        assert!(state.wake_timer.early_rearmed_deadline.is_none());

        replace_wake_timer_source(
            &handle,
            &mut state.wake_timer,
            Some(deadline),
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(state.wake_timer.token.is_some());
        assert_eq!(state.wake_timer.deadline, Some(deadline));
        assert!(
            state.wake_timer.early_rearmed_deadline.is_none(),
            "the same quantised value is a fresh first arm after cancellation"
        );
        reset_wake_timer_for_output_replacement(&handle, &mut state.wake_timer);
    }

    #[test]
    fn same_past_deadline_with_model_progress_trips_the_total_cap() {
        let mut timer = WakeTimerState::default();
        let deadline = Duration::from_millis(1);
        let elapsed = Duration::from_secs(1);

        for observation in 1..=MAX_PAST_DEADLINE_OBSERVATIONS {
            let expected = if observation == MAX_PAST_DEADLINE_OBSERVATIONS {
                PastDeadline::Stuck
            } else {
                PastDeadline::DueNow {
                    first: observation == 1,
                }
            };
            assert_eq!(
                timer.observe_next_deadline(Some(deadline), elapsed, true),
                expected,
                "model progress must not reset observation {observation} for the same deadline"
            );
        }

        assert_eq!(timer.consecutive_past_deadlines, 1);
        assert_eq!(
            timer.total_past_deadline_observations,
            MAX_PAST_DEADLINE_OBSERVATIONS
        );
    }

    #[test]
    fn advancing_past_deadlines_with_progress_are_not_a_stuck_loop() {
        let mut timer = WakeTimerState::default();
        let elapsed = Duration::from_secs(1);

        for millis in 1..=20 {
            assert_eq!(
                timer.observe_next_deadline(Some(Duration::from_millis(millis)), elapsed, true,),
                PastDeadline::DueNow { first: true },
                "advancing past deadline {millis} must reset the stuck count"
            );
        }

        assert_eq!(timer.consecutive_past_deadlines, 1);
        assert_eq!(
            timer.observed_past_deadline,
            Some(Duration::from_millis(20))
        );
        assert_eq!(timer.total_past_deadline_observations, 1);
    }

    #[test]
    fn advancing_deadline_resets_the_total_observation_count() {
        let mut timer = WakeTimerState::default();
        let elapsed = Duration::from_secs(1);
        let deadline_count = u64::from(MAX_PAST_DEADLINE_OBSERVATIONS) + 20;

        for millis in 1..=deadline_count {
            assert_eq!(
                timer.observe_next_deadline(Some(Duration::from_millis(millis)), elapsed, false,),
                PastDeadline::DueNow { first: true },
                "distinct past deadline {millis} must start a fresh total"
            );
        }

        assert_eq!(timer.consecutive_past_deadlines, 1);
        assert_eq!(timer.total_past_deadline_observations, 1);
    }

    #[test]
    fn never_consumed_deadline_trips_the_bounded_abnormal_exit() {
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
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .add_message::<RequestRedraw>()
            .init_resource::<LayerHostUpdateWake>()
            .add_systems(Last, refuse_to_consume_wake_deadline)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
        let mut state = TestRunnerState {
            app,
            output,
            needs_update: true,
            wake_timer: WakeTimerState::default(),
            fresh_elapsed: Duration::from_millis(10),
            last_timer_delay: None,
            last_timer_rearmed: false,
            animate_requested_at: None,
            abnormal_exit: false,
            exit_reason: None,
            iteration_steps: Vec::new(),
        };
        let event_loop: EventLoop<TestRunnerState> = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();

        for attempt in 1..=MAX_CONSECUTIVE_PAST_DEADLINES {
            drive_test_runner_update(&mut state, &handle);
            if attempt < MAX_CONSECUTIVE_PAST_DEADLINES {
                assert!(
                    !state.abnormal_exit,
                    "attempt {attempt} remains bounded recovery"
                );
            }
        }
        assert!(state.abnormal_exit);
        assert_eq!(state.exit_reason.as_deref(), Some("wake-deadline-stuck"));
        assert_eq!(
            state.wake_timer.consecutive_past_deadlines,
            MAX_CONSECUTIVE_PAST_DEADLINES
        );
        assert_eq!(
            state.wake_timer.total_past_deadline_observations,
            MAX_CONSECUTIVE_PAST_DEADLINES
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
    fn leaving_animate_does_not_resurrect_a_stale_frame_backstop() {
        let backstop = oldest_frame_backstop_deadline([Duration::from_secs(5)]);
        assert_eq!(
            next_timer_deadline(
                WakePolicy::Idle,
                Some(Duration::from_secs(4)),
                backstop,
                &[]
            ),
            None
        );
    }

    #[test]
    fn idle_without_pending_frames_or_configures_arms_nothing() {
        assert_eq!(next_timer_deadline(WakePolicy::Idle, None, None, &[]), None);
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
    fn early_timer_rearm_uses_the_real_remaining_time_with_one_guard() {
        let deadline = Duration::from_millis(800);
        let elapsed = deadline - Duration::from_millis(1);
        assert_eq!(
            wake_timer_delay(deadline, elapsed, true),
            Duration::from_millis(2)
        );
        assert_eq!(
            wake_timer_delay(deadline, elapsed, false),
            Duration::from_millis(1)
        );
        assert_eq!(wake_timer_delay(deadline, deadline, true), Duration::ZERO);
    }

    #[test]
    fn production_clock_sample_is_the_exact_model_elapsed() {
        let output = OutputKey::new("DP-1").unwrap();
        let model = ShellModel::new(
            output,
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        configure_ingress(&mut app);
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .add_message::<RequestRedraw>()
            .init_resource::<LayerHostUpdateWake>();
        let first = Instant::now();
        run_layer_host_app_update_at(&mut app, first);
        assert_eq!(
            app.world().resource::<Time<Real>>().elapsed(),
            Duration::ZERO
        );

        run_layer_host_app_update_at(&mut app, first + Duration::from_millis(1_600));
        assert_eq!(
            app.world().resource::<Time<Real>>().elapsed(),
            Duration::from_millis(1_600),
            "the timer decision and model update consume one sampled clock value"
        );
    }
}
