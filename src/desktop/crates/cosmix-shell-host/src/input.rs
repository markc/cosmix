//! Native SCTK pointer input translated into Bevy window input and shell holds.

use std::time::Duration;

use bevy::input::ButtonState;
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel};
use bevy::input::touch::TouchPhase;
use bevy::prelude::{App, Entity, Vec2, Window};
use bevy::window::{CursorEntered, CursorLeft, CursorMoved, WindowEvent};
use cosmix_shell::core::{Edge, OutputKey, PanelInput};
use cosmix_shell::runtime::{ShellCommand, ShellCommandKind};
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
}

#[derive(Clone)]
struct Focus {
    surface_id: u32,
    window: Entity,
    edge: Edge,
    position: Vec2,
}

#[derive(Default)]
pub(crate) struct PointerBridge {
    focus: Option<Focus>,
    pressed: Vec<MouseButton>,
    axis_active: bool,
    diagnostics: u64,
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
                        target.surface.id().protocol_id(),
                        target.window,
                        target.edge,
                        position,
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
        surface_id: u32,
        window: Entity,
        edge: Edge,
        position: Vec2,
    ) {
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
        self.focus = Some(Focus {
            surface_id,
            window,
            edge,
            position,
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
    let at = app
        .world()
        .get_resource::<bevy::time::Time<bevy::time::Real>>()
        .map_or(Duration::ZERO, bevy::time::Time::elapsed);
    app.world_mut().write_message(ShellCommand {
        output: output.clone(),
        at,
        kind: ShellCommandKind::Panel { edge, input },
    });
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
    use bevy::ecs::message::Messages;

    fn pointer_app() -> (App, Entity, OutputKey) {
        let mut app = App::new();
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
    fn enter_order_cursor_scale_delta_and_window_attribution_are_exact() {
        let (mut app, window, output) = pointer_app();
        let mut bridge = PointerBridge::default();
        bridge.enter(
            &mut app,
            &output,
            7,
            window,
            Edge::Left,
            Vec2::new(20.0, 10.0),
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
            7,
            window,
            Edge::Top,
            Vec2::new(20.0, 10.0),
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
            7,
            window,
            Edge::Right,
            Vec2::new(10.0, 20.0),
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
    fn invalid_positions_and_unknown_wide_buttons_are_rejected() {
        let (app, window, _) = pointer_app();
        assert_eq!(valid_position(&app, window, (f64::NAN, 1.0)), None);
        assert_eq!(translate_button(u32::MAX), None);
        assert_eq!(translate_button(BTN_LEFT), Some(MouseButton::Left));
    }
}
