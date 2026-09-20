//! Vertical gain fader on the shared dB scale.
use iced_core::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced_core::{Element, Event, Length, Point, Rectangle, Size, keyboard};

use crate::AudioStyle;
use crate::audio_style::quad;
use crate::scale::Taper;

const THUMB_HEIGHT: f32 = 14.0;
/// Shift divides pointer travel by this.
pub(crate) const FINE_DIVISOR: f32 = 10.0;

/// A controlled vertical fader. Store each `on_change` value and pass it back.
///
/// Dragging is relative (pressing never jumps the value); holding Shift while
/// dragging moves ten times slower. A double-click resets to the default
/// (0 dB unless set). Travel follows the taper, `scale::Taper::DEFAULT`
/// unless the host supplies one.
pub struct Fader<'a, Message> {
    value_db: f32,
    default_db: f32,
    on_change: Option<Box<dyn Fn(f32) -> Message + 'a>>,
    on_release: Option<Message>,
    width: f32,
    height: Length,
    style: AudioStyle,
    taper: Taper<'a>,
}

impl<'a, Message> Fader<'a, Message> {
    /// A fader showing `value_db` (use `f32::NEG_INFINITY` for silence).
    pub fn new(value_db: f32) -> Self {
        Self {
            value_db,
            default_db: 0.0,
            on_change: None,
            on_release: None,
            width: 28.0,
            height: Length::Fixed(160.0),
            style: AudioStyle::default(),
            taper: Taper::DEFAULT,
        }
    }

    /// Enables the fader; each gesture step publishes the new gain in dB.
    pub fn on_change(mut self, callback: impl Fn(f32) -> Message + 'a) -> Self {
        self.on_change = Some(Box::new(callback));
        self
    }

    /// Published once when a drag ends, for undo grouping or automation.
    pub fn on_release(mut self, message: Message) -> Self {
        self.on_release = Some(message);
        self
    }

    /// The double-click reset value.
    pub fn default_db(mut self, db: f32) -> Self {
        self.default_db = db;
        self
    }

    /// Width in logical pixels (default 28).
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// Height (default 160 px).
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Colours; see `Tokens::audio_style`.
    pub fn style(mut self, style: AudioStyle) -> Self {
        self.style = style;
        self
    }

    /// The gain taper. The travel, the unity tick and a neighbouring
    /// `LevelMeter` given the same taper all follow it.
    pub fn taper(mut self, taper: Taper<'a>) -> Self {
        self.taper = taper;
        self
    }
}

/// Vertical drag state shared by `Fader` and `Knob`: a relative gesture that
/// rebases when fine mode toggles, so switching never jumps the value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Drag {
    origin_y: f32,
    origin: f32,
    fine: bool,
}

impl Drag {
    pub(crate) fn new(y: f32, value: f32, fine: bool) -> Self {
        Self {
            origin_y: y,
            origin: value,
            fine,
        }
    }

    /// The normalised value for pointer `y`. `current` is the value now shown,
    /// used as the new origin when fine mode changes. `travel` is the pixel
    /// distance for the full 0..=1 range. Upward motion increases the value.
    pub(crate) fn value(&mut self, y: f32, current: f32, fine: bool, travel: f32) -> f32 {
        if fine != self.fine {
            *self = Self::new(y, current, fine);
        }
        let divisor = if self.fine { FINE_DIVISOR } else { 1.0 };
        (self.origin + (self.origin_y - y) / travel.max(1.0) / divisor).clamp(0.0, 1.0)
    }
}

#[derive(Default)]
pub(crate) struct PointerState {
    pub(crate) drag: Option<Drag>,
    pub(crate) last_click: Option<mouse::Click>,
    pub(crate) modifiers: keyboard::Modifiers,
}

fn travel(bounds: Rectangle) -> f32 {
    (bounds.height - THUMB_HEIGHT).max(1.0)
}

/// The thumb rectangle for a travel `position` within `bounds`.
pub(crate) fn thumb_rect(bounds: Rectangle, position: f32) -> Rectangle {
    let y = bounds.y + (1.0 - position.clamp(0.0, 1.0)) * travel(bounds);
    Rectangle {
        x: bounds.x,
        y,
        width: bounds.width,
        height: THUMB_HEIGHT,
    }
}

/// Shared press handling: returns true if a double-click reset should fire.
pub(crate) fn press_is_double(state: &mut PointerState, position: Point) -> bool {
    let click = mouse::Click::new(position, mouse::Button::Left, state.last_click);
    state.last_click = Some(click);
    click.kind() == mouse::click::Kind::Double
}

impl<Message: Clone, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer>
    for Fader<'_, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PointerState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PointerState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.width), self.height)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.width, self.height)
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
                let Some(position) = cursor.position_over(bounds) else {
                    return;
                };
                if shell.is_event_captured() {
                    return;
                }
                if press_is_double(state, position) {
                    state.drag = None;
                    if self.value_db != self.default_db {
                        self.value_db = self.default_db;
                        shell.publish(on_change(self.default_db));
                    }
                } else {
                    state.drag = Some(Drag::new(
                        position.y,
                        self.taper.position(self.value_db),
                        state.modifiers.shift(),
                    ));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // The cursor, unlike the event, is in this widget's coordinates
                // inside scrollables.
                if let Some(drag) = &mut state.drag
                    && let Some(position) = cursor.position()
                {
                    let current = self.taper.position(self.value_db);
                    let next =
                        drag.value(position.y, current, state.modifiers.shift(), travel(bounds));
                    let db = self.taper.db(next);
                    if db != self.value_db {
                        self.value_db = db;
                        shell.publish(on_change(db));
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
            Event::Window(iced_core::window::Event::Unfocused) => {
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
        let style = self.style;
        let position = self.taper.position(self.value_db);
        let thumb = thumb_rect(bounds, position);
        let track = Rectangle {
            x: bounds.center_x() - 2.0,
            y: bounds.y + THUMB_HEIGHT / 2.0,
            width: 4.0,
            height: travel(bounds),
        };
        quad(renderer, track, style.track, 2.0, None);
        let centre = thumb.center_y();
        quad(
            renderer,
            Rectangle {
                y: centre,
                height: (track.y + track.height - centre).max(0.0),
                ..track
            },
            style.fill,
            2.0,
            None,
        );
        let unity = thumb_rect(bounds, self.taper.position(0.0)).center_y();
        quad(
            renderer,
            Rectangle {
                x: bounds.x,
                y: unity - 0.5,
                width: bounds.width,
                height: 1.0,
            },
            style.border,
            0.0,
            None,
        );
        quad(
            renderer,
            thumb,
            style.thumb,
            style.radius,
            Some(style.border),
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
        } else if self.on_change.is_some() && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a, Message: Clone + 'a, Theme: 'a, Renderer: renderer::Renderer + 'a> From<Fader<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(fader: Fader<'a, Message>) -> Self {
        Element::new(fader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> Rectangle {
        Rectangle {
            x: 10.0,
            y: 20.0,
            width: 28.0,
            height: 114.0,
        }
    }

    #[test]
    fn thumb_spans_the_travel() {
        assert_eq!(thumb_rect(bounds(), 1.0).y, 20.0);
        assert_eq!(thumb_rect(bounds(), 0.0).y, 120.0);
        assert_eq!(thumb_rect(bounds(), 0.5).center_y(), 77.0);
    }

    #[test]
    fn drag_is_relative_and_fine_mode_rebases() {
        let mut drag = Drag::new(50.0, 0.5, false);
        // 100 px travel: 10 px up is +0.1.
        let value = drag.value(40.0, 0.5, false, 100.0);
        assert!((value - 0.6).abs() < 1e-6);
        // Switching to fine keeps the value, then moves ten times slower.
        assert!((drag.value(40.0, value, true, 100.0) - 0.6).abs() < 1e-6);
        assert!((drag.value(30.0, value, true, 100.0) - 0.61).abs() < 1e-6);
        let mut coarse = Drag::new(50.0, 0.5, false);
        assert_eq!(coarse.value(-1000.0, 0.5, false, 100.0), 1.0);
        assert_eq!(coarse.value(1000.0, 1.0, false, 100.0), 0.0);
    }

    #[test]
    fn double_click_is_detected_at_the_same_point() {
        let mut state = PointerState::default();
        assert!(!press_is_double(&mut state, Point::new(1.0, 1.0)));
        assert!(press_is_double(&mut state, Point::new(1.0, 1.0)));
    }
}
