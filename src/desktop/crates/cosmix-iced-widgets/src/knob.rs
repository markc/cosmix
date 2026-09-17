//! Rotary pan knob.
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced::{Element, Event, Length, Point, Rectangle, Size, keyboard};

use crate::AudioStyle;
use crate::audio_style::quad;
use crate::fader::{Drag, PointerState, press_is_double};

/// Vertical pointer travel, in logical pixels, for the whole -1..=1 range.
const TRAVEL: f32 = 150.0;
/// Indicator sweep either side of the top, in radians (135 degrees).
const SWEEP: f32 = 0.75 * std::f32::consts::PI;

/// A controlled pan knob with a value in -1 (left) ..= 1 (right).
///
/// Dragging up increases the value; Shift drags ten times slower; a
/// double-click resets to centre (0). Only presses inside the circle start a
/// gesture.
pub struct Knob<'a, Message> {
    value: f32,
    on_change: Option<Box<dyn Fn(f32) -> Message + 'a>>,
    on_release: Option<Message>,
    size: f32,
    style: AudioStyle,
}

impl<'a, Message> Knob<'a, Message> {
    /// A knob showing `value`, clamped to -1..=1.
    pub fn new(value: f32) -> Self {
        Self {
            value: clamp(value),
            on_change: None,
            on_release: None,
            size: 28.0,
            style: AudioStyle::default(),
        }
    }

    /// Enables the knob; each gesture step publishes the new value.
    pub fn on_change(mut self, callback: impl Fn(f32) -> Message + 'a) -> Self {
        self.on_change = Some(Box::new(callback));
        self
    }

    /// Published once when a drag ends.
    pub fn on_release(mut self, message: Message) -> Self {
        self.on_release = Some(message);
        self
    }

    /// Diameter in logical pixels (default 28).
    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    /// Colours; see `Tokens::audio_style`.
    pub fn style(mut self, style: AudioStyle) -> Self {
        self.style = style;
        self
    }
}

fn clamp(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(-1.0, 1.0)
    }
}

fn to_unit(value: f32) -> f32 {
    (clamp(value) + 1.0) / 2.0
}

fn from_unit(unit: f32) -> f32 {
    unit * 2.0 - 1.0
}

/// True when `point` is inside the knob's circle.
pub(crate) fn hit(bounds: Rectangle, point: Point) -> bool {
    let radius = bounds.width.min(bounds.height) / 2.0;
    let centre = bounds.center();
    let (dx, dy) = (point.x - centre.x, point.y - centre.y);
    dx * dx + dy * dy <= radius * radius
}

/// Centre of the indicator dot for `value`: straight up at 0, 135 degrees
/// either side at the ends.
pub(crate) fn indicator(bounds: Rectangle, value: f32) -> Point {
    let radius = bounds.width.min(bounds.height) / 2.0 * 0.62;
    let angle = clamp(value) * SWEEP;
    let centre = bounds.center();
    Point::new(
        centre.x + radius * angle.sin(),
        centre.y - radius * angle.cos(),
    )
}

impl<Message: Clone, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer>
    for Knob<'_, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PointerState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PointerState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.size, self.size)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<PointerState>();
        let bounds = layout.bounds();
        let Some(on_change) = &self.on_change else {
            return;
        };
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(position) = cursor.position() else {
                    return;
                };
                if shell.is_event_captured() || !hit(bounds, position) {
                    return;
                }
                if press_is_double(state, position) {
                    state.drag = None;
                    if self.value != 0.0 {
                        self.value = 0.0;
                        shell.publish(on_change(0.0));
                    }
                } else {
                    state.drag = Some(Drag::new(
                        position.y,
                        to_unit(self.value),
                        state.modifiers.shift(),
                    ));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some(drag) = &mut state.drag
                    && let Some(position) = cursor.position()
                {
                    let next = from_unit(drag.value(
                        position.y,
                        to_unit(self.value),
                        state.modifiers.shift(),
                        TRAVEL,
                    ));
                    if next != self.value {
                        self.value = next;
                        shell.publish(on_change(next));
                    }
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.drag.is_some() =>
            {
                state.drag = None;
                if let Some(message) = &self.on_release {
                    shell.publish(message.clone());
                }
                shell.capture_event();
            }
            Event::Window(iced::window::Event::Unfocused) => {
                if state.drag.take().is_some()
                    && let Some(message) = &self.on_release
                {
                    shell.publish(message.clone());
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let diameter = bounds.width.min(bounds.height);
        let body = Rectangle::new(
            Point::new(
                bounds.center_x() - diameter / 2.0,
                bounds.center_y() - diameter / 2.0,
            ),
            Size::new(diameter, diameter),
        );
        quad(
            renderer,
            body,
            self.style.track,
            diameter / 2.0,
            Some(self.style.border),
        );
        let dot = indicator(bounds, self.value);
        let dot_radius = (diameter * 0.1).max(2.0);
        quad(
            renderer,
            Rectangle::new(
                Point::new(dot.x - dot_radius, dot.y - dot_radius),
                Size::new(dot_radius * 2.0, dot_radius * 2.0),
            ),
            self.style.fill,
            dot_radius,
            None,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<PointerState>();
        if state.drag.is_some() {
            mouse::Interaction::Grabbing
        } else if self.on_change.is_some()
            && cursor
                .position()
                .is_some_and(|point| hit(layout.bounds(), point))
        {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a, Message: Clone + 'a, Theme: 'a, Renderer: renderer::Renderer + 'a> From<Knob<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(knob: Knob<'a, Message>) -> Self {
        Element::new(knob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> Rectangle {
        Rectangle::new(Point::new(0.0, 0.0), Size::new(20.0, 20.0))
    }

    #[test]
    fn value_mapping_is_clamped_and_centred() {
        assert_eq!(to_unit(0.0), 0.5);
        assert_eq!(to_unit(-5.0), 0.0);
        assert_eq!(to_unit(f32::NAN), 0.5);
        assert_eq!(from_unit(1.0), 1.0);
        assert_eq!(Knob::<()>::new(3.0).value, 1.0);
        // Full travel covers the whole range.
        let mut drag = Drag::new(100.0, to_unit(-1.0), false);
        assert_eq!(
            from_unit(drag.value(100.0 - TRAVEL, 0.0, false, TRAVEL)),
            1.0
        );
    }

    #[test]
    fn hit_is_circular() {
        assert!(hit(bounds(), Point::new(10.0, 10.0)));
        assert!(hit(bounds(), Point::new(10.0, 0.5)));
        assert!(!hit(bounds(), Point::new(1.0, 1.0)));
        assert!(!hit(bounds(), Point::new(25.0, 10.0)));
    }

    #[test]
    fn indicator_points_up_at_centre_and_sweeps() {
        let top = indicator(bounds(), 0.0);
        assert!((top.x - 10.0).abs() < 1e-4 && top.y < 10.0);
        let left = indicator(bounds(), -1.0);
        let right = indicator(bounds(), 1.0);
        assert!(left.x < 10.0 && right.x > 10.0);
        assert!(left.y > 10.0 && (left.y - right.y).abs() < 1e-4);
    }
}
