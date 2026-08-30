//! Normal-window development host for Q-0.
//!
//! Window corners stand in for output corners. Borderless fullscreen makes
//! the absolute-pointer dwell path representative; the detector's physical
//! "continued push" branch remains explicitly synthetic-only as documented in
//! [`crate::core::corner`]. The host simulates exclusive zones by insetting a
//! central canvas and never requests a compositor exclusive zone.

use std::convert::Infallible;
use std::time::Duration;

use bevy::app::{App, Plugin, Update};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;
use bevy::ui::{UiRect, px};
use bevy::window::{
    CursorLeft, CursorMoved, MonitorSelection, RequestRedraw, WindowMode, WindowResized,
    WindowScaleFactorChanged,
};
use bevy::winit::{UpdateMode, WinitSettings};
use ctk::theme::tokens;

use crate::chrome::QuoinPanelMounts;
use crate::core::{
    Corner, CornerDetector, CornerDetectorConfig, CornerDetectorError, CornerDiagnostics,
    CornerEvent, Edge, LogicalPoint, LogicalSize, OutputKey, PointerSample,
};
use crate::host::ShellHost;
use crate::runtime::{
    HostGeometry, ShellCommand, ShellCommandKind, ShellFrame, ShellFrameState, ShellRuntimeSet,
    WakePolicy,
};

/// Finite idle heartbeat shared with the demo's initial settings.
///
/// `Duration::MAX` is forbidden: bevy_winit 0.19 `state.rs:711` uses
/// `Instant::checked_add`, and overflow leaves an earlier `WaitUntil` armed.
/// One wake per hour is a deliberate Q-0 deviation from the plan's §7
/// zero-wakeup doctrine; the real fix is an upstream indefinite wait or the
/// S-2 layer-shell host, which will not use bevy_winit's `Reactive` path.
pub const IDLE_WAIT: Duration = Duration::from_secs(3_600);

/// Host setup values. All units are logical pixels and real monotonic time.
#[derive(Clone, Debug)]
pub struct DevShellHostConfig {
    pub output: OutputKey,
    pub logical_size: LogicalSize,
    pub corner: CornerDetectorConfig,
}

/// Pure host-computed rectangle, exposed for deterministic layout tests.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DevRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Result of simulated exclusive-zone and corner-ownership layout.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DevHostLayout {
    pub panels: [DevRect; 4],
    pub canvas: DevRect,
}

/// Stateful implementation of the renderer-neutral host contract.
#[derive(Resource)]
pub struct DevShellHost {
    geometry: HostGeometry,
    window: Entity,
    mounts: QuoinPanelMounts,
    canvas: Entity,
    debug_text: Entity,
    glows: [Entity; 4],
    detector: CornerDetector,
    pointer: Option<LogicalPoint>,
    diagnostics: CornerDiagnostics,
    layout: DevHostLayout,
    frame_wake: WakePolicy,
    armed_wake: Option<ArmedWake>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArmedWake {
    Idle,
    Animate,
    Deadline(Duration),
    Expired(Duration),
}

impl ShellHost for DevShellHost {
    type Error = Infallible;
    type Mount = Entity;

    fn geometry(&self) -> &HostGeometry {
        &self.geometry
    }

    fn panel_mount(&self, edge: Edge) -> Self::Mount {
        self.mounts.get(edge)
    }

    fn apply(&mut self, frame: &ShellFrame) -> Result<(), Self::Error> {
        self.geometry = frame.geometry.clone();
        self.layout = layout_for(frame);
        Ok(())
    }

    fn set_wake_policy(&mut self, policy: WakePolicy) -> Result<(), Self::Error> {
        self.frame_wake = policy;
        Ok(())
    }
}

/// Host event bridge and reconciliation plugin.
pub struct DevShellHostPlugin;

impl Plugin for DevShellHostPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (resize_host, sample_corners, toggle_fullscreen)
                .chain()
                .in_set(ShellRuntimeSet::Input),
        )
        .add_systems(
            Update,
            (
                reconcile_host,
                apply_host_layout,
                update_debug_overlay,
                apply_wake_policy,
            )
                .chain()
                .in_set(ShellRuntimeSet::Host),
        );
    }
}

/// Spawn the simulated screen, deterministic panel mounts and tuning overlay.
/// The returned mounts are the sole attachment information chrome receives.
pub fn spawn_dev_host(
    commands: &mut Commands,
    window: Entity,
    config: DevShellHostConfig,
) -> QuoinPanelMounts {
    let canvas = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::SURFACE),
            bevy::picking::Pickable::IGNORE,
            GlobalZIndex(0),
        ))
        .id();

    let mount = |commands: &mut Commands, z| {
        commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    overflow: Overflow::visible(),
                    ..default()
                },
                GlobalZIndex(z),
            ))
            .id()
    };
    // Top > right > bottom > left. The matching geometry calculation assigns
    // each shared corner to the same winner and shortens pinned neighbours.
    let left = mount(commands, 110);
    let bottom = mount(commands, 120);
    let right = mount(commands, 130);
    let top = mount(commands, 140);
    let mounts = QuoinPanelMounts::new(left, bottom, right, top);

    let deadzone = config.corner.deadzone_px() * 2.0;
    let glows = std::array::from_fn(|index| {
        let corner = Corner::ALL[index];
        commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    width: px(deadzone),
                    height: px(deadzone),
                    left: if matches!(corner, Corner::TopLeft | Corner::BottomLeft) {
                        px(0)
                    } else {
                        Val::Auto
                    },
                    right: if matches!(corner, Corner::TopRight | Corner::BottomRight) {
                        px(0)
                    } else {
                        Val::Auto
                    },
                    top: if matches!(corner, Corner::TopLeft | Corner::TopRight) {
                        px(0)
                    } else {
                        Val::Auto
                    },
                    bottom: if matches!(corner, Corner::BottomLeft | Corner::BottomRight) {
                        px(0)
                    } else {
                        Val::Auto
                    },
                    display: Display::None,
                    ..default()
                },
                bevy::feathers::theme::ThemeBackgroundColor(tokens::CONTROL_ACTIVE),
                bevy::picking::Pickable::IGNORE,
                GlobalZIndex(190),
            ))
            .id()
    });
    let debug_text = commands
        .spawn((
            Text::new("Quoin corner detector"),
            TextFont::from_font_size(12.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT_DIM),
            Node {
                position_type: PositionType::Absolute,
                left: px(48),
                top: px(10),
                padding: UiRect::all(px(7)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::MASTER_PANEL),
            bevy::picking::Pickable::IGNORE,
            GlobalZIndex(200),
        ))
        .id();

    let detector = CornerDetector::new(config.corner);
    let diagnostics = detector.diagnostics(Duration::ZERO);
    commands.insert_resource(DevShellHost {
        geometry: HostGeometry {
            output: config.output,
            logical_size: config.logical_size,
        },
        window,
        mounts,
        canvas,
        debug_text,
        glows,
        detector,
        pointer: None,
        diagnostics,
        layout: DevHostLayout::default(),
        frame_wake: WakePolicy::Idle,
        armed_wake: None,
    });
    mounts
}

fn resize_host(
    mut resized: MessageReader<WindowResized>,
    mut scale_changed: MessageReader<WindowScaleFactorChanged>,
    windows: Query<&Window>,
    mut host: ResMut<DevShellHost>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    let window = host.window;
    let mut event_size = None;
    for event in resized.read().filter(|event| event.window == window) {
        event_size = Some((event.width, event.height));
    }
    let scale_changed = scale_changed.read().any(|event| event.window == window);
    if event_size.is_none() && !scale_changed {
        return;
    }
    let Ok(window_state) = windows.get(window) else {
        return;
    };
    let (width, height) = event_size.unwrap_or((window_state.width(), window_state.height()));
    let Ok(size) = LogicalSize::new(width, height) else {
        return;
    };
    host.pointer = window_state
        .cursor_position()
        .map(|position| LogicalPoint::new(position.x, position.y));
    reseed_host_detector(&mut host, time.elapsed(), size, &mut commands);
}

fn sample_corners(
    mut moved: MessageReader<CursorMoved>,
    mut left: MessageReader<CursorLeft>,
    mut host: ResMut<DevShellHost>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    let now = time.elapsed();
    let mut sampled = false;
    let window = host.window;
    for event in moved.read().filter(|event| event.window == window) {
        let point = LogicalPoint::new(event.position.x, event.position.y);
        host.pointer = Some(point);
        emit_corner_sample(&mut host, now, point, &mut commands);
        sampled = true;
    }
    if !sampled
        && host
            .detector
            .next_deadline()
            .is_some_and(|deadline| now >= deadline)
        && let Some(point) = host.pointer
    {
        emit_corner_sample(&mut host, now, point, &mut commands);
    }
    if left.read().any(|event| event.window == window) {
        host.pointer = None;
        if let Ok(events) = host.detector.leave_output(now) {
            emit_corner_events(&host, now, events, &mut commands);
        }
    }
    host.diagnostics = host.detector.diagnostics(now);
}

fn emit_corner_sample(
    host: &mut DevShellHost,
    now: Duration,
    point: LogicalPoint,
    commands: &mut MessageWriter<ShellCommand>,
) {
    let sample = PointerSample::new(now, point, host.geometry.logical_size);
    if let Ok(events) = host.detector.sample(sample) {
        emit_corner_events(host, now, events, commands);
    }
}

fn emit_corner_events(
    host: &DevShellHost,
    now: Duration,
    events: Vec<CornerEvent>,
    commands: &mut MessageWriter<ShellCommand>,
) {
    for event in events {
        commands.write(ShellCommand {
            output: host.geometry.output.clone(),
            at: now,
            kind: ShellCommandKind::Corner(event),
        });
    }
}

fn toggle_fullscreen(
    keys: Res<ButtonInput<KeyCode>>,
    mut host: ResMut<DevShellHost>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
    mut windows: Query<&mut Window>,
) {
    if !keys.just_pressed(KeyCode::F11) {
        return;
    }
    let Ok(mut window) = windows.get_mut(host.window) else {
        return;
    };
    window.mode = if window.mode == WindowMode::Windowed {
        WindowMode::BorderlessFullscreen(MonitorSelection::Current)
    } else {
        WindowMode::Windowed
    };
    if let Ok(size) = LogicalSize::new(window.width(), window.height()) {
        host.pointer = window
            .cursor_position()
            .map(|position| LogicalPoint::new(position.x, position.y));
        reseed_host_detector(&mut host, time.elapsed(), size, &mut commands);
    }
}

fn reseed_host_detector(
    host: &mut DevShellHost,
    now: Duration,
    size: LogicalSize,
    commands: &mut MessageWriter<ShellCommand>,
) {
    if let Ok(events) = reset_detector_stream(&mut host.detector, &mut host.pointer, now, size) {
        host.geometry.logical_size = size;
        emit_corner_events(host, now, events, commands);
        commands.write(ShellCommand {
            output: host.geometry.output.clone(),
            at: now,
            kind: ShellCommandKind::Geometry(size),
        });
        host.diagnostics = host.detector.diagnostics(now);
    }
}

fn reset_detector_stream(
    detector: &mut CornerDetector,
    pointer: &mut Option<LogicalPoint>,
    now: Duration,
    size: LogicalSize,
) -> Result<Vec<CornerEvent>, CornerDetectorError> {
    let mut events = detector.leave_output(now)?;
    let Some(position) = *pointer else {
        return Ok(events);
    };
    if !size.contains(position) {
        *pointer = None;
        return Ok(events);
    }
    events.extend(detector.sample(PointerSample::new(now, position, size))?);
    Ok(events)
}

fn reconcile_host(frame: Res<ShellFrameState>, mut host: ResMut<DevShellHost>) {
    let _ = host.apply(&frame.0);
    let _ = host.set_wake_policy(frame.0.wake);
}

fn apply_host_layout(host: Res<DevShellHost>, mut nodes: Query<&mut Node>) {
    if let Ok(mut canvas) = nodes.get_mut(host.canvas) {
        apply_rect(&mut canvas, host.layout.canvas);
    }
    for edge in Edge::ALL {
        if let Ok(mut node) = nodes.get_mut(host.panel_mount(edge)) {
            apply_rect(&mut node, host.layout.panels[edge.index()]);
        }
    }
}

fn apply_rect(node: &mut Node, rect: DevRect) {
    node.left = px(rect.x);
    node.top = px(rect.y);
    node.width = px(rect.width.max(0.0));
    node.height = px(rect.height.max(0.0));
}

fn update_debug_overlay(
    host: Res<DevShellHost>,
    mut texts: Query<&mut Text>,
    mut nodes: Query<&mut Node>,
) {
    if let Ok(mut text) = texts.get_mut(host.debug_text) {
        let diagnostics = host.diagnostics;
        text.0 = format!(
            "deadzone {:.0}px · dwell {}/{}ms · gate {:.0}px/s\ncandidate {:?} · engaged {:?} · velocity {}",
            host.detector.config().deadzone_px(),
            diagnostics.dwell_elapsed.as_millis(),
            host.detector.config().dwell().as_millis(),
            host.detector.config().velocity_max_px_s(),
            diagnostics.candidate,
            diagnostics.engaged,
            diagnostics
                .last_motion_speed_px_s
                .map(|speed| format!("{speed:.0}px/s"))
                .unwrap_or_else(|| "—".to_owned()),
        );
    }
    let active = host.diagnostics.candidate.or(host.diagnostics.engaged);
    for (index, entity) in host.glows.into_iter().enumerate() {
        if let Ok(mut node) = nodes.get_mut(entity) {
            node.display = if active == Some(Corner::ALL[index]) {
                Display::Flex
            } else {
                Display::None
            };
        }
    }
}

fn apply_wake_policy(
    mut host: ResMut<DevShellHost>,
    time: Res<Time<Real>>,
    mut settings: ResMut<WinitSettings>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let now = time.elapsed();
    let frame_deadline = match host.frame_wake {
        WakePolicy::WakeAt(deadline) => Some(deadline),
        WakePolicy::Idle | WakePolicy::Animate => None,
    };
    let deadline = frame_deadline
        .into_iter()
        .chain(host.detector.next_deadline())
        .min();
    let desired = desired_wake(host.frame_wake, deadline, now);
    let request_redraw = apply_wake_arm(&mut host.armed_wake, desired, now, &mut settings);
    if request_redraw {
        redraw.write(RequestRedraw);
    }
}

fn apply_wake_arm(
    armed: &mut Option<ArmedWake>,
    desired: ArmedWake,
    now: Duration,
    settings: &mut WinitSettings,
) -> bool {
    if !arm_if_changed(armed, desired) {
        return false;
    }
    match desired {
        ArmedWake::Animate => {
            settings.focused_mode = UpdateMode::Continuous;
            settings.unfocused_mode = UpdateMode::Continuous;
        }
        ArmedWake::Deadline(deadline) => {
            let wait = deadline.saturating_sub(now);
            // Keep both modes identical while a deadline is armed. In
            // bevy_winit 0.19 state.rs:659, a focus-driven effective-mode
            // change resets the scheduled tick and re-arms the full wait.
            // Low-power both ways: raw device motion must not wake an
            // unfocused window; every input the shell reacts to arrives as
            // a window event.
            let mode = UpdateMode::reactive_low_power(wait);
            settings.focused_mode = mode;
            settings.unfocused_mode = mode;
        }
        ArmedWake::Idle => {
            settings.focused_mode = UpdateMode::reactive(IDLE_WAIT);
            settings.unfocused_mode = UpdateMode::reactive_low_power(IDLE_WAIT);
        }
        ArmedWake::Expired(_) => {
            let mode = UpdateMode::reactive(IDLE_WAIT);
            settings.focused_mode = mode;
            settings.unfocused_mode = mode;
            return true;
        }
    }
    false
}

fn desired_wake(policy: WakePolicy, deadline: Option<Duration>, now: Duration) -> ArmedWake {
    if policy == WakePolicy::Animate {
        return ArmedWake::Animate;
    }
    match deadline {
        Some(deadline) if deadline <= now => ArmedWake::Expired(deadline),
        Some(deadline) => ArmedWake::Deadline(deadline),
        None => ArmedWake::Idle,
    }
}

fn arm_if_changed(armed: &mut Option<ArmedWake>, desired: ArmedWake) -> bool {
    if *armed == Some(desired) {
        return false;
    }
    *armed = Some(desired);
    true
}

/// Compute deterministic corner ownership and pinned-adjacent reductions.
pub fn layout_for(frame: &ShellFrame) -> DevHostLayout {
    let width = frame.geometry.logical_size.width();
    let height = frame.geometry.logical_size.height();
    let pinned = |edge: Edge| frame.panel(edge).exclusive_zone_px;
    let left_inset = pinned(Edge::Left).min(width);
    let right_inset = pinned(Edge::Right).min((width - left_inset).max(0.0));
    let top_inset = pinned(Edge::Top).min(height);
    let bottom_inset = pinned(Edge::Bottom).min((height - top_inset).max(0.0));
    let thickness = |edge: Edge| frame.panel(edge).thickness_px;

    let mut panels = [DevRect::default(); 4];
    panels[Edge::Top.index()] = DevRect {
        x: 0.0,
        y: 0.0,
        width,
        height: thickness(Edge::Top),
    };
    panels[Edge::Right.index()] = DevRect {
        x: (width - thickness(Edge::Right)).max(0.0),
        y: top_inset,
        width: thickness(Edge::Right),
        height: (height - top_inset).max(0.0),
    };
    panels[Edge::Bottom.index()] = DevRect {
        x: 0.0,
        y: (height - thickness(Edge::Bottom)).max(0.0),
        width: (width - right_inset).max(0.0),
        height: thickness(Edge::Bottom),
    };
    panels[Edge::Left.index()] = DevRect {
        x: 0.0,
        y: top_inset,
        width: thickness(Edge::Left),
        height: (height - top_inset - bottom_inset).max(0.0),
    };
    DevHostLayout {
        panels,
        canvas: DevRect {
            x: left_inset,
            y: top_inset,
            width: (width - left_inset - right_inset).max(0.0),
            height: (height - top_inset - bottom_inset).max(0.0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Carousel, CornerTrigger, PanelInput, ShellModel};
    use std::time::Instant;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn pending_absolute_deadline_is_armed_only_once() {
        let deadline = ms(1_000);
        let mut armed = None;
        let first = desired_wake(WakePolicy::Idle, Some(deadline), ms(100));
        assert_eq!(first, ArmedWake::Deadline(deadline));
        assert!(arm_if_changed(&mut armed, first));

        let same_absolute_deadline = desired_wake(WakePolicy::Idle, Some(deadline), ms(600));
        assert_eq!(same_absolute_deadline, ArmedWake::Deadline(deadline));
        assert!(!arm_if_changed(&mut armed, same_absolute_deadline));
    }

    #[test]
    fn deadline_to_idle_replaces_stale_wait_with_finite_heartbeat() {
        let mut armed = None;
        let mut settings = WinitSettings::continuous();
        assert!(!apply_wake_arm(
            &mut armed,
            ArmedWake::Deadline(ms(1_000)),
            ms(100),
            &mut settings,
        ));
        assert_eq!(update_wait(settings.focused_mode), ms(900));

        assert!(!apply_wake_arm(
            &mut armed,
            ArmedWake::Idle,
            ms(200),
            &mut settings,
        ));
        assert_eq!(update_wait(settings.focused_mode), IDLE_WAIT);
        assert_ne!(IDLE_WAIT, Duration::MAX);
        assert!(Instant::now().checked_add(IDLE_WAIT).is_some());
    }

    #[test]
    fn armed_deadline_uses_identical_modes_across_focus_changes() {
        let mut armed = None;
        let mut settings = WinitSettings::continuous();
        assert!(!apply_wake_arm(
            &mut armed,
            ArmedWake::Deadline(ms(1_000)),
            ms(700),
            &mut settings,
        ));

        assert_eq!(settings.focused_mode, settings.unfocused_mode);
        assert_eq!(update_wait(settings.focused_mode), ms(300));
        match settings.focused_mode {
            UpdateMode::Reactive {
                react_to_device_events,
                ..
            } => assert!(!react_to_device_events),
            UpdateMode::Continuous => panic!("expected reactive Winit mode"),
        }
    }

    fn update_wait(mode: UpdateMode) -> Duration {
        match mode {
            UpdateMode::Reactive { wait, .. } => wait,
            UpdateMode::Continuous => panic!("expected reactive Winit mode"),
        }
    }

    #[test]
    fn expired_deadline_requests_at_most_one_rearm() {
        let deadline = ms(100);
        let mut armed = None;
        let expired = desired_wake(WakePolicy::Idle, Some(deadline), ms(101));
        assert_eq!(expired, ArmedWake::Expired(deadline));
        assert!(arm_if_changed(&mut armed, expired));
        assert!(!arm_if_changed(&mut armed, expired));
    }

    #[test]
    fn bounds_reset_ends_engaged_stream_and_drops_outside_pointer() {
        let old_size = LogicalSize::new(100.0, 100.0).unwrap();
        let mut detector =
            CornerDetector::new(CornerDetectorConfig::new(12.0, ms(200), 1_500.0).unwrap());
        let old_corner = LogicalPoint::new(99.0, 99.0);
        detector
            .sample(PointerSample::new(ms(0), old_corner, old_size))
            .unwrap();
        assert_eq!(
            detector
                .sample(PointerSample::new(ms(200), old_corner, old_size))
                .unwrap(),
            vec![CornerEvent::Entered {
                corner: Corner::BottomRight,
                dwell: ms(200),
                trigger: CornerTrigger::Dwell,
            }]
        );

        let mut pointer = Some(old_corner);
        let events = reset_detector_stream(
            &mut detector,
            &mut pointer,
            ms(201),
            LogicalSize::new(50.0, 50.0).unwrap(),
        )
        .unwrap();
        assert_eq!(
            events,
            vec![CornerEvent::Left {
                corner: Corner::BottomRight,
            }]
        );
        assert_eq!(pointer, None);
        assert_eq!(detector.engaged_corner(), None);
    }

    #[test]
    fn pinned_neighbours_reduce_lower_priority_edge_lengths() {
        let size = LogicalSize::new(1000.0, 800.0).unwrap();
        let mut model = ShellModel::new(
            OutputKey::new("dev").unwrap(),
            size,
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        for edge in Edge::ALL {
            model.set_carousel(edge, Carousel::new(["page"]).unwrap());
        }
        model
            .panel_input(Edge::Top, Duration::ZERO, PanelInput::Pin)
            .unwrap();
        model
            .panel_input(Edge::Right, Duration::ZERO, PanelInput::Pin)
            .unwrap();
        let frame = ShellFrame::from_model(&model);
        let layout = layout_for(&frame);
        assert_eq!(layout.panels[Edge::Top.index()].width, 1000.0);
        assert_eq!(
            layout.panels[Edge::Right.index()].y,
            frame.panel(Edge::Top).thickness_px
        );
        assert_eq!(
            layout.panels[Edge::Bottom.index()].width,
            1000.0 - frame.panel(Edge::Right).thickness_px
        );
        assert_eq!(layout.canvas.x, 0.0);
        assert_eq!(layout.canvas.y, frame.panel(Edge::Top).thickness_px);
    }
}
