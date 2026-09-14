use crate::{Core, Modifiers, Painter, View, terminal::MouseModifiers};
use bevy::{ecs::system::SystemParam, input::mouse::MouseScrollUnit, prelude::*};

#[derive(Component, Default)]
struct MouseState {
    held: [bool; 3],
    wheel: f32,
    motion: Option<(u16, u16, u8)>,
}

#[derive(SystemParam)]
struct Input<'w, 's> {
    core: Res<'w, Core>,
    view: Res<'w, View>,
    painter: Res<'w, Painter>,
    modifiers: Res<'w, Modifiers>,
    nodes: Query<
        'w,
        's,
        (
            &'static ComputedNode,
            &'static UiGlobalTransform,
            &'static ComputedUiTargetCamera,
        ),
    >,
    cameras: Query<'w, 's, &'static Camera>,
    states: Query<'w, 's, &'static mut MouseState>,
}

fn button(button: PointerButton) -> usize {
    match button {
        PointerButton::Primary => 0,
        PointerButton::Middle => 1,
        PointerButton::Secondary => 2,
    }
}

// UI transforms and sizes are viewport-relative physical pixels. Pointer
// locations are window-logical pixels. Match Bevy's UI picking conversion,
// then invert the IMAGE transform: its origin includes the pane border.
fn cell(
    position: Vec2,
    scale: f32,
    viewport: Vec2,
    node: &ComputedNode,
    transform: &UiGlobalTransform,
    cell_size: Vec2,
    grid: (u16, u16),
) -> (u16, u16) {
    let physical = position * scale - viewport;
    let relative = (transform.affine().inverse().transform_point2(physical) + node.size() / 2.0)
        * node.inverse_scale_factor();
    (
        (relative.x / cell_size.x)
            .floor()
            .clamp(0.0, f32::from(grid.0.saturating_sub(1))) as u16,
        (relative.y / cell_size.y)
            .floor()
            .clamp(0.0, f32::from(grid.1.saturating_sub(1))) as u16,
    )
}

enum Action {
    Button(usize, bool),
    Motion(u8),
    Wheel(f32, MouseScrollUnit),
}

impl Input<'_, '_> {
    fn report(
        &mut self,
        container: Entity,
        image: Entity,
        id: u64,
        position: Vec2,
        action: Action,
    ) -> bool {
        let Some(pane) = self.view.pane_views.iter().find(|pane| pane.id == id) else {
            return false;
        };
        let Ok((node, transform, target)) = self.nodes.get(image) else {
            return false;
        };
        let Some(camera) = target
            .get()
            .and_then(|entity| self.cameras.get(entity).ok())
        else {
            return false;
        };
        let painter = self.painter.0.lock().unwrap();
        let (col, row) = cell(
            position,
            camera.target_scaling_factor().unwrap_or(1.0),
            camera
                .physical_viewport_rect()
                .map_or(Vec2::ZERO, |rect| rect.min.as_vec2()),
            node,
            transform,
            Vec2::new(painter.logical_width(), painter.logical_height()),
            (pane.cols, pane.rows),
        );
        let mods = MouseModifiers {
            shift: self.modifiers.shift(),
            alt: self.modifiers.0[4] || self.modifiers.0[5],
            ctrl: self.modifiers.ctrl(),
        };
        let Ok(mut state) = self.states.get_mut(container) else {
            return false;
        };
        let tabs = self.core.0.lock().unwrap();
        let Some(terminal) = tabs.pane_by_id(id) else {
            return false;
        };
        tabs.user_activity();
        let terminal = terminal.lock().unwrap();
        match action {
            Action::Button(button, pressed) => {
                // Release and DragEnd can both arrive; only report once.
                if !pressed && !state.held[button] {
                    return false;
                }
                state.held[button] = pressed;
                state.motion = None;
                terminal.mouse_button(col, row, button as u8, pressed, mods)
            }
            Action::Motion(button) => {
                // Drag targets the original pane even outside its bounds. Move
                // handles bare motion only, avoiding duplicate drag reports.
                if button == 3 && state.held.iter().any(|held| *held) {
                    return false;
                }
                let motion = (col, row, button);
                if state.motion == Some(motion) {
                    return false;
                }
                let sent = terminal.mouse_motion(col, row, button, mods);
                if sent {
                    state.motion = Some(motion);
                }
                sent
            }
            Action::Wheel(y, unit) => {
                state.wheel += match unit {
                    MouseScrollUnit::Line => y,
                    MouseScrollUnit::Pixel => y / painter.logical_height(),
                };
                let lines = state.wheel.trunc() as i32;
                state.wheel -= lines as f32;
                if terminal.mouse_scroll(col, row, lines, mods) {
                    true
                } else {
                    terminal.scroll_wheel(lines, mods);
                    false
                }
            }
        }
    }
}

pub(super) fn observe(commands: &mut Commands, container: Entity, image: Entity, id: u64) {
    commands
        .entity(container)
        .insert(MouseState::default())
        .observe(move |mut event: On<Pointer<Press>>, mut input: Input| {
            if input.report(
                container,
                image,
                id,
                event.pointer_location.position,
                Action::Button(button(event.button), true),
            ) {
                event.propagate(false);
            }
        })
        .observe(move |mut event: On<Pointer<Release>>, mut input: Input| {
            if input.report(
                container,
                image,
                id,
                event.pointer_location.position,
                Action::Button(button(event.button), false),
            ) {
                event.propagate(false);
            }
        })
        .observe(move |mut event: On<Pointer<Move>>, mut input: Input| {
            if input.report(
                container,
                image,
                id,
                event.pointer_location.position,
                Action::Motion(3),
            ) {
                event.propagate(false);
            }
        })
        .observe(move |mut event: On<Pointer<Drag>>, mut input: Input| {
            if input.report(
                container,
                image,
                id,
                event.pointer_location.position,
                Action::Motion(button(event.button) as u8),
            ) {
                event.propagate(false);
            }
        })
        .observe(move |mut event: On<Pointer<DragEnd>>, mut input: Input| {
            if input.report(
                container,
                image,
                id,
                event.pointer_location.position,
                Action::Button(button(event.button), false),
            ) {
                event.propagate(false);
            }
        })
        .observe(move |mut event: On<Pointer<Scroll>>, mut input: Input| {
            if input.report(
                container,
                image,
                id,
                event.pointer_location.position,
                Action::Wheel(event.y, event.unit),
            ) {
                event.propagate(false);
            }
        })
        .observe(
            move |_: On<Pointer<Cancel>>, mut states: Query<&mut MouseState>| {
                if let Ok(mut state) = states.get_mut(container) {
                    *state = MouseState::default();
                }
            },
        );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::Affine2;

    #[test]
    fn mouse_cells_respect_pane_offsets_borders_scale_and_viewport() {
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
            // A second pane below a menu, with a resolved physical border.
            let top_left = Vec2::new(403.0, 47.0);
            let viewport = Vec2::new(20.0, 30.0);
            let physical_cell = Vec2::new(9.0, 18.0);
            let node = ComputedNode {
                size: physical_cell * Vec2::new(80.0, 24.0),
                inverse_scale_factor: 1.0 / scale,
                ..default()
            };
            let transform =
                UiGlobalTransform::from(Affine2::from_translation(top_left + node.size() / 2.0));
            for (relative, expected) in [
                (Vec2::ZERO, (0, 0)),
                (Vec2::new(8.9, 17.9), (0, 0)),
                (Vec2::new(9.1, 18.1), (1, 1)),
                (Vec2::new(91.0, 91.0), (10, 5)),
                (Vec2::splat(-100.0), (0, 0)),
                (Vec2::splat(10000.0), (79, 23)),
            ] {
                let pointer = (viewport + top_left + relative) / scale;
                assert_eq!(
                    cell(
                        pointer,
                        scale,
                        viewport,
                        &node,
                        &transform,
                        physical_cell / scale,
                        (80, 24)
                    ),
                    expected,
                    "scale={scale}, relative={relative}"
                );
            }
        }
    }
}
