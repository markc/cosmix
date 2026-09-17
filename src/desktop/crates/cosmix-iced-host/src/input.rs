//! Pointer and window helpers taking plain numbers.

use iced_core::mouse::{self, Button, Interaction, ScrollDelta};
use iced_core::{Event, Point, window};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
const BTN_FORWARD: u32 = 0x115;
const BTN_BACK: u32 = 0x116;

/// Maps a Linux evdev button code (as sent by `wl_pointer.button`).
pub fn mouse_button(code: u32) -> Button {
    match code {
        BTN_LEFT => Button::Left,
        BTN_RIGHT => Button::Right,
        BTN_MIDDLE => Button::Middle,
        BTN_SIDE | BTN_BACK => Button::Back,
        BTN_EXTRA | BTN_FORWARD => Button::Forward,
        other => Button::Other(other.min(u32::from(u16::MAX)) as u16),
    }
}

pub fn button_event(code: u32, pressed: bool) -> Event {
    let button = mouse_button(code);
    Event::Mouse(if pressed {
        mouse::Event::ButtonPressed(button)
    } else {
        mouse::Event::ButtonReleased(button)
    })
}

/// Converts a physical pointer position into logical coordinates.
pub fn logical_position(x: f64, y: f64, scale_factor: f32) -> Point {
    let scale = f64::from(scale_factor);
    Point::new((x / scale) as f32, (y / scale) as f32)
}

/// A `wl_pointer.axis` value (logical px, positive = content moves up/left)
/// as an iced pixel scroll. iced's sign is the opposite.
pub fn wheel_pixels(horizontal: f64, vertical: f64) -> Event {
    Event::Mouse(mouse::Event::WheelScrolled {
        delta: ScrollDelta::Pixels {
            x: -horizontal as f32,
            y: -vertical as f32,
        },
    })
}

/// `wl_pointer.axis_value120` steps (120 per detent) as an iced line scroll.
pub fn wheel_value120(horizontal: i32, vertical: i32) -> Event {
    Event::Mouse(mouse::Event::WheelScrolled {
        delta: ScrollDelta::Lines {
            x: -(horizontal as f32) / 120.0,
            y: -(vertical as f32) / 120.0,
        },
    })
}

pub fn focus_event(focused: bool) -> Event {
    Event::Window(if focused {
        window::Event::Focused
    } else {
        window::Event::Unfocused
    })
}

/// The CSS / `wp_cursor_shape_v1` name for an interaction. `None` means hide
/// the cursor.
pub fn cursor_shape_name(interaction: Interaction) -> Option<&'static str> {
    Some(match interaction {
        Interaction::Hidden => return None,
        Interaction::None | Interaction::Idle => "default",
        Interaction::ContextMenu => "context-menu",
        Interaction::Help => "help",
        Interaction::Pointer => "pointer",
        Interaction::Progress => "progress",
        Interaction::Wait => "wait",
        Interaction::Cell => "cell",
        Interaction::Crosshair => "crosshair",
        Interaction::Text => "text",
        Interaction::Alias => "alias",
        Interaction::Copy => "copy",
        Interaction::Move => "move",
        Interaction::NoDrop => "no-drop",
        Interaction::NotAllowed => "not-allowed",
        Interaction::Grab => "grab",
        Interaction::Grabbing => "grabbing",
        Interaction::ResizingHorizontally => "ew-resize",
        Interaction::ResizingVertically => "ns-resize",
        Interaction::ResizingDiagonallyUp => "nesw-resize",
        Interaction::ResizingDiagonallyDown => "nwse-resize",
        Interaction::ResizingColumn => "col-resize",
        Interaction::ResizingRow => "row-resize",
        Interaction::AllScroll => "all-scroll",
        Interaction::ZoomIn => "zoom-in",
        Interaction::ZoomOut => "zoom-out",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_from_evdev() {
        assert_eq!(mouse_button(0x110), Button::Left);
        assert_eq!(mouse_button(0x111), Button::Right);
        assert_eq!(mouse_button(0x112), Button::Middle);
        assert_eq!(mouse_button(0x113), Button::Back);
        assert_eq!(mouse_button(0x114), Button::Forward);
        assert_eq!(mouse_button(0x117), Button::Other(0x117));
    }

    #[test]
    fn wheel_sign_is_inverted() {
        assert_eq!(
            wheel_value120(0, 120),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: ScrollDelta::Lines { x: 0.0, y: -1.0 }
            })
        );
        assert_eq!(
            wheel_pixels(2.0, -10.0),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: ScrollDelta::Pixels { x: -2.0, y: 10.0 }
            })
        );
    }

    #[test]
    fn physical_positions_scale_down() {
        assert_eq!(logical_position(25.0, 50.0, 2.5), Point::new(10.0, 20.0));
    }

    #[test]
    fn cursor_names() {
        assert_eq!(cursor_shape_name(Interaction::Pointer), Some("pointer"));
        assert_eq!(cursor_shape_name(Interaction::Hidden), None);
        assert_eq!(cursor_shape_name(Interaction::Idle), Some("default"));
    }
}
