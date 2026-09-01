//! Native SCTK pointer input translated into Bevy window input and shell holds.

use bevy::app::Update;
use bevy::ecs::message::MessageWriter;
use bevy::input::ButtonState;
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel};
use bevy::input::touch::TouchPhase;
use bevy::picking::events::PointerState;
use bevy::picking::pointer::{
    PointerAction, PointerId, PointerInput, PointerLocation, PointerPress,
};
use bevy::prelude::{
    App, Entity, IntoScheduleConfigs, Res, ResMut, Resource, Time, Vec2, Window, World,
};
use bevy::time::Real;
use bevy::window::{CursorEntered, CursorLeft, CursorMoved, WindowEvent};
use cosmix_shell::core::{CornerEvent, Edge, OutputKey, PanelInput};
use cosmix_shell::runtime::{ShellCommand, ShellCommandKind, ShellRuntimeSet};
use smithay_client_toolkit::seat::pointer::{
    AxisScroll, BTN_BACK, BTN_EXTRA, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE,
    PointerEvent, PointerEventKind,
};
use wayland_client::Proxy;
use wayland_client::protocol::wl_surface;

#[derive(Clone)]
pub(crate) struct SurfaceTarget {
    pub surface: wl_surface::WlSurface,
    pub window: Entity,
    pub edge: Edge,
    pub output_size: Vec2,
    pub thickness: f32,
    pub committed_margin: i32,
}

#[derive(Clone)]
struct Focus {
    surface_id: u32,
    window: Entity,
    edge: Edge,
    position: Vec2,
    output_position: Vec2,
}

#[derive(Resource, Default)]
pub(crate) struct StagedShellCommands(Vec<(OutputKey, ShellCommandKind)>);

pub(crate) fn configure_ingress(app: &mut App) {
    app.init_resource::<StagedShellCommands>().add_systems(
        Update,
        flush_staged_shell_commands.in_set(ShellRuntimeSet::Input),
    );
}

pub(crate) fn stage_shell_command(app: &mut App, output: OutputKey, kind: ShellCommandKind) {
    stage_shell_command_world(app.world_mut(), output, kind);
}

pub(crate) fn stage_shell_command_world(
    world: &mut World,
    output: OutputKey,
    kind: ShellCommandKind,
) {
    world
        .resource_mut::<StagedShellCommands>()
        .0
        .push((output, kind));
}

pub(crate) fn staged_shell_commands_pending(app: &App) -> bool {
    !app.world().resource::<StagedShellCommands>().0.is_empty()
}

fn shell_command_kind(kind: &ShellCommandKind) -> &'static str {
    match kind {
        ShellCommandKind::Geometry(_) => "geometry",
        ShellCommandKind::Corner(CornerEvent::Entered { .. }) => "corner-entered",
        ShellCommandKind::Corner(CornerEvent::Left { .. }) => "corner-left",
        ShellCommandKind::Panel { .. } => "panel",
        ShellCommandKind::Carousel { .. } => "carousel",
    }
}

fn flush_staged_shell_commands(
    time: Res<Time<Real>>,
    mut staged: ResMut<StagedShellCommands>,
    mut commands: MessageWriter<ShellCommand>,
) {
    let at = time.elapsed();
    for (output, kind) in staged.0.drain(..) {
        tracing::debug!(
            event = "quoin_staged_command_flushed",
            kind = shell_command_kind(&kind),
            output = output.as_str(),
            stamped_us = at.as_micros()
        );
        commands.write(ShellCommand { output, at, kind });
    }
}

#[derive(Default)]
pub(crate) struct PointerBridge {
    focus: Option<Focus>,
    pressed: Vec<MouseButton>,
    axis_active: bool,
    diagnostics: u64,
    last_output_position: Option<Vec2>,
}

impl PointerBridge {
    pub fn frame(
        &mut self,
        app: &mut App,
        output: &OutputKey,
        targets: &[SurfaceTarget],
        events: &[PointerEvent],
    ) -> bool {
        let mut handled = false;
        for event in events {
            let Some(target) = targets
                .iter()
                .find(|target| target.surface == event.surface)
            else {
                continue;
            };
            match &event.kind {
                PointerEventKind::Enter { .. } => {
                    let Some(position) = valid_position(app, target.window, event.position) else {
                        self.reject("invalid-enter-position");
                        continue;
                    };
                    if self
                        .focus
                        .as_ref()
                        .is_some_and(|focus| focus.window != target.window)
                    {
                        self.cleanup(app, output, None);
                    }
                    self.enter(
                        app,
                        output,
                        (
                            target.surface.id().protocol_id(),
                            target.window,
                            target.edge,
                        ),
                        (position, output_position(target, position)),
                    );
                    handled = true;
                }
                PointerEventKind::Leave { .. } => {
                    if self
                        .focus
                        .as_ref()
                        .is_some_and(|focus| focus.surface_id == event.surface.id().protocol_id())
                    {
                        self.leave(app, output);
                        handled = true;
                    }
                }
                PointerEventKind::Motion { .. }
                | PointerEventKind::Press { .. }
                | PointerEventKind::Release { .. } => {
                    if self
                        .focus
                        .as_ref()
                        .is_none_or(|focus| focus.surface_id != event.surface.id().protocol_id())
                    {
                        continue;
                    }
                    let Some(position) = valid_position(app, target.window, event.position) else {
                        self.reject("invalid-event-position");
                        continue;
                    };
                    self.motion(app, target.window, position);
                    self.record_output_position(target, position);
                    match event.kind {
                        PointerEventKind::Press { button, .. } => {
                            if let Some(button) = translate_button(button) {
                                if !self.pressed.contains(&button) {
                                    self.pressed.push(button);
                                }
                                emit_window(
                                    app,
                                    MouseButtonInput {
                                        button,
                                        state: ButtonState::Pressed,
                                        window: target.window,
                                    },
                                );
                            } else {
                                self.reject("unrepresentable-button");
                            }
                        }
                        PointerEventKind::Release { button, .. } => {
                            if let Some(button) = translate_button(button) {
                                self.pressed.retain(|held| *held != button);
                                emit_window(
                                    app,
                                    MouseButtonInput {
                                        button,
                                        state: ButtonState::Released,
                                        window: target.window,
                                    },
                                );
                            } else {
                                self.reject("unrepresentable-button");
                            }
                        }
                        PointerEventKind::Motion { .. } => {}
                        _ => unreachable!(),
                    }
                    handled = true;
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    if self
                        .focus
                        .as_ref()
                        .is_none_or(|focus| focus.surface_id != event.surface.id().protocol_id())
                    {
                        continue;
                    }
                    let Some(position) = valid_position(app, target.window, event.position) else {
                        self.reject("invalid-axis-position");
                        continue;
                    };
                    self.record_output_position(target, position);
                    self.axis(app, target.window, *horizontal, *vertical);
                    handled = true;
                }
            }
        }
        handled
    }

    fn enter(
        &mut self,
        app: &mut App,
        output: &OutputKey,
        target: (u32, Entity, Edge),
        positions: (Vec2, Vec2),
    ) {
        let (surface_id, window, edge) = target;
        let (position, output_position) = positions;
        set_cursor(app, window, Some(position));
        emit_window(app, CursorEntered { window });
        emit_window(
            app,
            CursorMoved {
                window,
                position,
                delta: None,
            },
        );
        semantic(app, output, edge, PanelInput::PointerEntered);
        self.last_output_position = Some(output_position);
        self.focus = Some(Focus {
            surface_id,
            window,
            edge,
            position,
            output_position,
        });
    }

    pub fn cleanup(&mut self, app: &mut App, output: &OutputKey, window: Option<Entity>) -> bool {
        if window.is_some_and(|window| {
            self.focus
                .as_ref()
                .is_none_or(|focus| focus.window != window)
        }) {
            return false;
        }
        let Some(focus) = self.focus.clone() else {
            return false;
        };
        self.motion(app, focus.window, focus.position);
        if !self.pressed.is_empty() {
            cancel_picking_press(app);
        }
        self.release_state(app, focus.window);
        self.leave(app, output);
        true
    }

    fn release_state(&mut self, app: &mut App, window: Entity) {
        for button in self.pressed.drain(..) {
            emit_window(
                app,
                MouseButtonInput {
                    button,
                    state: ButtonState::Released,
                    window,
                },
            );
        }
        if self.axis_active {
            emit_window(
                app,
                MouseWheel {
                    unit: MouseScrollUnit::Pixel,
                    x: 0.0,
                    y: 0.0,
                    window,
                    phase: TouchPhase::Ended,
                },
            );
            self.axis_active = false;
        }
    }

    fn motion(&mut self, app: &mut App, window: Entity, position: Vec2) {
        let delta = self
            .focus
            .as_ref()
            .filter(|focus| focus.window == window)
            .map(|focus| position - focus.position);
        set_cursor(app, window, Some(position));
        emit_window(
            app,
            CursorMoved {
                window,
                position,
                delta,
            },
        );
        if let Some(focus) = self.focus.as_mut() {
            focus.position = position;
        }
    }

    fn record_output_position(&mut self, target: &SurfaceTarget, local: Vec2) {
        let position = output_position(target, local);
        self.last_output_position = Some(position);
        if let Some(focus) = self.focus.as_mut() {
            focus.output_position = position;
        }
    }

    pub(crate) const fn last_output_position(&self) -> Option<Vec2> {
        self.last_output_position
    }

    fn leave(&mut self, app: &mut App, output: &OutputKey) {
        let Some(focus) = self.focus.take() else {
            return;
        };
        self.release_state(app, focus.window);
        set_cursor(app, focus.window, None);
        emit_window(
            app,
            CursorLeft {
                window: focus.window,
            },
        );
        semantic(app, output, focus.edge, PanelInput::PointerLeft);
    }

    fn axis(
        &mut self,
        app: &mut App,
        window: Entity,
        horizontal: AxisScroll,
        vertical: AxisScroll,
    ) {
        let stopped = horizontal.stop || vertical.stop;
        let phase = if stopped {
            TouchPhase::Ended
        } else if self.axis_active {
            TouchPhase::Moved
        } else {
            TouchPhase::Started
        };
        let discrete = horizontal.discrete != 0 || vertical.discrete != 0;
        emit_window(
            app,
            MouseWheel {
                unit: if discrete {
                    MouseScrollUnit::Line
                } else {
                    MouseScrollUnit::Pixel
                },
                x: -(if discrete {
                    horizontal.discrete as f32
                } else {
                    horizontal.absolute as f32
                }),
                y: -(if discrete {
                    vertical.discrete as f32
                } else {
                    vertical.absolute as f32
                }),
                window,
                phase,
            },
        );
        self.axis_active = !stopped;
    }

    fn reject(&mut self, reason: &'static str) {
        self.diagnostics = self.diagnostics.saturating_add(1);
        if self.diagnostics.is_power_of_two() {
            tracing::warn!(
                event = "quoin_pointer_event_rejected",
                count = self.diagnostics,
                reason
            );
        }
    }
}

fn cancel_picking_press(app: &mut App) {
    let location = {
        let world = app.world_mut();
        let mut pointers = world.query::<(&PointerId, &PointerLocation)>();
        pointers
            .iter(world)
            .find_map(|(id, location)| (*id == PointerId::Mouse).then(|| location.location.clone()))
            .flatten()
    };
    if let Some(location) = location
        && let Some(mut messages) = app
            .world_mut()
            .get_resource_mut::<bevy::ecs::message::Messages<PointerInput>>()
    {
        messages.write(PointerInput::new(
            PointerId::Mouse,
            location,
            PointerAction::Cancel,
        ));
    }
    if let Some(mut state) = app.world_mut().get_resource_mut::<PointerState>() {
        state.clear(PointerId::Mouse);
    }
    let world = app.world_mut();
    let mut pointers = world.query::<(&PointerId, &mut PointerPress)>();
    for (id, mut press) in pointers.iter_mut(world) {
        if *id == PointerId::Mouse {
            *press = PointerPress::default();
        }
    }
}

fn valid_position(app: &App, window: Entity, position: (f64, f64)) -> Option<Vec2> {
    if !position.0.is_finite() || !position.1.is_finite() {
        return None;
    }
    let window = app.world().get::<Window>(window)?;
    Some(Vec2::new(
        (position.0 as f32).clamp(0.0, window.resolution.width()),
        (position.1 as f32).clamp(0.0, window.resolution.height()),
    ))
}

fn set_cursor(app: &mut App, window: Entity, position: Option<Vec2>) {
    if let Some(mut window) = app.world_mut().get_mut::<Window>(window) {
        window.set_cursor_position(position);
    }
}

fn emit_window<T>(app: &mut App, event: T)
where
    T: bevy::prelude::Message + Clone,
    WindowEvent: From<T>,
{
    app.world_mut().write_message(event.clone());
    app.world_mut().write_message(WindowEvent::from(event));
}

fn semantic(app: &mut App, output: &OutputKey, edge: Edge, input: PanelInput) {
    stage_shell_command(app, output.clone(), ShellCommandKind::Panel { edge, input });
}

fn output_position(target: &SurfaceTarget, local: Vec2) -> Vec2 {
    output_logical_position(
        target.edge,
        local,
        target.output_size,
        target.thickness,
        target.committed_margin,
    )
}

fn translate_button(button: u32) -> Option<MouseButton> {
    Some(match button {
        BTN_LEFT => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_SIDE | BTN_BACK => MouseButton::Back,
        BTN_EXTRA | BTN_FORWARD => MouseButton::Forward,
        other => MouseButton::Other(u16::try_from(other).ok()?),
    })
}

/// Convert a surface-local logical point to output-logical coordinates.
pub fn output_logical_position(
    edge: Edge,
    local: Vec2,
    output: Vec2,
    thickness: f32,
    committed_margin: i32,
) -> Vec2 {
    let margin = committed_margin as f32;
    match edge {
        Edge::Left => Vec2::new(margin + local.x, local.y),
        Edge::Right => Vec2::new(output.x - thickness - margin + local.x, local.y),
        Edge::Top => Vec2::new(local.x, margin + local.y),
        Edge::Bottom => Vec2::new(local.x, output.y - thickness - margin + local.y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::MinimalPlugins;
    use bevy::camera::RenderTarget;
    use bevy::ecs::message::Messages;
    use bevy::ecs::observer::On;
    use bevy::picking::Pickable;
    use bevy::picking::PickingSettings;
    use bevy::picking::backend::HitData;
    use bevy::picking::events::{
        Cancel, Click, Drag, DragDrop, DragEnd, DragEnter, DragLeave, DragOver, DragStart, Enter,
        Leave, Move, Out, Over, Pointer, Press, Release, Scroll, pointer_events,
    };
    use bevy::picking::hover::{HoverMap, PreviousHoverMap};
    use bevy::picking::input::mouse_pick_events;
    use bevy::picking::pointer::{Location, PointerMap, update_pointer_map};
    use bevy::time::TimeUpdateStrategy;
    use bevy::ui_widgets::{Activate, Button, ButtonPlugin};
    use bevy::window::WindowRef;
    use cosmix_shell::core::{
        Corner, CornerEvent, CornerTrigger, LogicalSize, PanelMode, ShellModel,
    };
    use cosmix_shell::runtime::{ShellFrameState, ShellRuntimePlugin, WakePolicy};
    use std::time::Duration;

    fn pointer_app() -> (App, Entity, OutputKey) {
        let mut app = App::new();
        app.init_resource::<StagedShellCommands>();
        app.add_message::<CursorEntered>()
            .add_message::<CursorMoved>()
            .add_message::<CursorLeft>()
            .add_message::<MouseButtonInput>()
            .add_message::<MouseWheel>()
            .add_message::<WindowEvent>()
            .add_message::<ShellCommand>();
        let window = app
            .world_mut()
            .spawn(Window {
                resolution: bevy::window::WindowResolution::new(200, 100)
                    .with_scale_factor_override(2.0),
                ..Default::default()
            })
            .id();
        (app, window, OutputKey::new("DP-1").unwrap())
    }

    #[derive(Resource, Default)]
    struct ActivationCount(u32);

    fn count_activation(_activation: On<Activate>, mut count: ResMut<ActivationCount>) {
        count.0 += 1;
    }

    fn picking_app() -> (App, Entity, Entity, OutputKey) {
        let (mut app, window, output) = pointer_app();
        app.add_plugins(ButtonPlugin)
            .init_resource::<PickingSettings>()
            .init_resource::<PointerState>()
            .init_resource::<PointerMap>()
            .init_resource::<HoverMap>()
            .init_resource::<PreviousHoverMap>()
            .init_resource::<ActivationCount>()
            .add_message::<PointerInput>()
            .add_message::<Pointer<Cancel>>()
            .add_message::<Pointer<Click>>()
            .add_message::<Pointer<Press>>()
            .add_message::<Pointer<DragDrop>>()
            .add_message::<Pointer<DragEnd>>()
            .add_message::<Pointer<DragEnter>>()
            .add_message::<Pointer<Drag>>()
            .add_message::<Pointer<DragLeave>>()
            .add_message::<Pointer<DragOver>>()
            .add_message::<Pointer<DragStart>>()
            .add_message::<Pointer<Scroll>>()
            .add_message::<Pointer<Move>>()
            .add_message::<Pointer<Out>>()
            .add_message::<Pointer<Over>>()
            .add_message::<Pointer<Leave>>()
            .add_message::<Pointer<Enter>>()
            .add_message::<Pointer<Release>>()
            .add_observer(count_activation);
        app.finish();
        app.cleanup();

        let target = RenderTarget::Window(WindowRef::Entity(window))
            .normalize(None)
            .unwrap();
        let location = Location {
            target,
            position: Vec2::new(12.0, 8.0),
        };
        app.world_mut().spawn((
            PointerId::Mouse,
            PointerLocation::new(location),
            PointerPress::default(),
        ));
        app.world_mut()
            .run_system_cached(update_pointer_map)
            .unwrap();
        let button = app.world_mut().spawn((Button, Pickable::default())).id();
        let hit = HitData::new(button, 0.0, None, None);
        app.world_mut()
            .resource_mut::<HoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(button, hit.clone());
        app.world_mut()
            .resource_mut::<PreviousHoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(button, hit);
        (app, window, button, output)
    }

    #[test]
    fn output_logical_conversion_covers_every_edge_and_margin_sign() {
        let local = Vec2::new(5.0, 7.0);
        let output = Vec2::new(1_000.0, 800.0);
        assert_eq!(
            output_logical_position(Edge::Left, local, output, 100.0, -20),
            Vec2::new(-15.0, 7.0)
        );
        assert_eq!(
            output_logical_position(Edge::Top, local, output, 100.0, 0),
            Vec2::new(5.0, 7.0)
        );
        assert_eq!(
            output_logical_position(Edge::Right, local, output, 100.0, -20),
            Vec2::new(925.0, 7.0)
        );
        assert_eq!(
            output_logical_position(Edge::Bottom, local, output, 100.0, 0),
            Vec2::new(5.0, 707.0)
        );
    }

    #[test]
    fn staged_corner_times_start_after_idle_and_left_gets_a_fresh_grace() {
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
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs(1)));

        app.update();
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
        let entered = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left);
        assert_eq!(entered.mode, PanelMode::Revealed);
        assert_eq!(
            entered.visible_fraction, 0.0,
            "late enter starts its animation now"
        );

        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Revealed,
            "an active corner hold survives more than one grace interval"
        );
        *app.world_mut().resource_mut::<TimeUpdateStrategy>() =
            TimeUpdateStrategy::ManualDuration(Duration::ZERO);
        stage_shell_command(
            &mut app,
            output,
            ShellCommandKind::Corner(CornerEvent::Left {
                corner: Corner::TopLeft,
            }),
        );
        app.update();
        let now = app.world().resource::<Time<Real>>().elapsed();
        let frame = &app.world().resource::<ShellFrameState>().0;
        let left = frame.panel(Edge::Left);
        assert_eq!(left.mode, PanelMode::Revealed);
        assert_eq!(
            frame.wake,
            WakePolicy::WakeAt(now + Duration::from_millis(800))
        );
    }

    #[test]
    fn enter_order_cursor_scale_delta_and_window_attribution_are_exact() {
        let (mut app, window, output) = pointer_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Left),
            (Vec2::new(20.0, 10.0), Vec2::new(20.0, 10.0)),
        );
        bridge.motion(&mut app, window, Vec2::new(24.0, 13.0));
        let events = app
            .world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .drain()
            .collect::<Vec<_>>();
        assert!(
            matches!(events[0], WindowEvent::CursorEntered(CursorEntered { window: event_window }) if event_window == window)
        );
        assert!(
            matches!(events[1], WindowEvent::CursorMoved(CursorMoved { window: event_window, position, delta: None }) if event_window == window && position == Vec2::new(20.0, 10.0))
        );
        assert!(
            matches!(events[2], WindowEvent::CursorMoved(CursorMoved { window: event_window, position, delta: Some(delta) }) if event_window == window && position == Vec2::new(24.0, 13.0) && delta == Vec2::new(4.0, 3.0))
        );
        assert_eq!(
            app.world()
                .get::<Window>(window)
                .unwrap()
                .physical_cursor_position(),
            Some(Vec2::new(48.0, 26.0))
        );
    }

    #[test]
    fn wheel_sign_unit_phase_and_cleanup_releases_then_leaves() {
        let (mut app, window, output) = pointer_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Top),
            (Vec2::new(20.0, 10.0), Vec2::new(20.0, 10.0)),
        );
        bridge.axis(
            &mut app,
            window,
            AxisScroll {
                discrete: 2,
                ..Default::default()
            },
            AxisScroll {
                discrete: -3,
                ..Default::default()
            },
        );
        bridge
            .pressed
            .extend([MouseButton::Left, MouseButton::Back]);
        assert!(bridge.cleanup(&mut app, &output, Some(window)));
        let events = app
            .world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .drain()
            .collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(event, WindowEvent::MouseWheel(MouseWheel { unit: MouseScrollUnit::Line, x, y, phase: TouchPhase::Started, .. }) if *x == -2.0 && *y == 3.0)));
        let releases = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    WindowEvent::MouseButtonInput(MouseButtonInput {
                        state: ButtonState::Released,
                        ..
                    })
                )
            })
            .unwrap();
        let left = events
            .iter()
            .position(|event| matches!(event, WindowEvent::CursorLeft(_)))
            .unwrap();
        assert!(releases < left);
        assert!(bridge.focus.is_none());
        assert!(bridge.pressed.is_empty());
        assert_eq!(
            app.world()
                .get::<Window>(window)
                .unwrap()
                .physical_cursor_position(),
            None
        );
    }

    #[test]
    fn native_leave_releases_buttons_and_axis_before_later_teardown() {
        let (mut app, window, output) = pointer_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Right),
            (Vec2::new(10.0, 20.0), Vec2::new(10.0, 20.0)),
        );
        bridge.pressed.push(MouseButton::Left);
        bridge.axis_active = true;
        bridge.leave(&mut app, &output);

        let events = app
            .world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .drain()
            .collect::<Vec<_>>();
        let release = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    WindowEvent::MouseButtonInput(MouseButtonInput {
                        state: ButtonState::Released,
                        ..
                    })
                )
            })
            .unwrap();
        let axis_end = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    WindowEvent::MouseWheel(MouseWheel {
                        phase: TouchPhase::Ended,
                        ..
                    })
                )
            })
            .unwrap();
        let left = events
            .iter()
            .position(|event| matches!(event, WindowEvent::CursorLeft(_)))
            .unwrap();
        assert!(release < left);
        assert!(axis_end < left);
        assert!(bridge.pressed.is_empty());
        assert!(!bridge.axis_active);
        assert!(!bridge.cleanup(&mut app, &output, None));
    }

    #[test]
    fn teardown_cancels_a_pressed_control_without_activating_it() {
        let (mut app, window, button, output) = picking_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            (7, window, Edge::Left),
            (Vec2::new(12.0, 8.0), Vec2::new(12.0, 8.0)),
        );
        app.world_mut()
            .resource_mut::<Messages<WindowEvent>>()
            .clear();

        emit_window(
            &mut app,
            MouseButtonInput {
                button: MouseButton::Left,
                state: ButtonState::Pressed,
                window,
            },
        );
        app.world_mut()
            .run_system_cached(mouse_pick_events)
            .unwrap();
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();
        app.world_mut().run_system_cached(pointer_events).unwrap();
        app.world_mut().flush();
        assert!(app.world().entity(button).contains::<bevy::ui::Pressed>());

        bridge.pressed.push(MouseButton::Left);
        assert!(bridge.cleanup(&mut app, &output, Some(window)));
        app.world_mut()
            .run_system_cached(mouse_pick_events)
            .unwrap();
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();
        app.world_mut().run_system_cached(pointer_events).unwrap();
        app.world_mut().flush();

        assert_eq!(app.world().resource::<ActivationCount>().0, 0);
        assert!(!app.world().entity(button).contains::<bevy::ui::Pressed>());
        let mut pointers = app.world_mut().query::<(&PointerId, &PointerPress)>();
        let press = pointers
            .iter(app.world())
            .find_map(|(id, press)| (*id == PointerId::Mouse).then_some(press))
            .unwrap();
        assert!(!press.is_any_pressed());
    }

    #[test]
    fn invalid_positions_and_unknown_wide_buttons_are_rejected() {
        let (app, window, _) = pointer_app();
        assert_eq!(valid_position(&app, window, (f64::NAN, 1.0)), None);
        assert_eq!(translate_button(u32::MAX), None);
        assert_eq!(translate_button(BTN_LEFT), Some(MouseButton::Left));
    }
}
