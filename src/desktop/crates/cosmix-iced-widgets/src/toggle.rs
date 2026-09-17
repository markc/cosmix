//! Latching mute/solo-style button.
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer, text,
    widget::{Tree, tree},
};
use iced::{Element, Event, Length, Point, Rectangle, Size};

use crate::AudioStyle;
use crate::audio_style::quad;

/// A controlled on/off button that flips on press, as mixer mute and solo
/// buttons do. `alert(true)` uses the alert colour (mute style); otherwise the
/// active colour (solo style).
pub struct Toggle<'a, Message> {
    label: String,
    on: bool,
    alert: bool,
    on_toggle: Option<Box<dyn Fn(bool) -> Message + 'a>>,
    width: f32,
    height: f32,
    style: AudioStyle,
}

impl<'a, Message> Toggle<'a, Message> {
    /// A toggle with a short `label` (for example "M" or "S").
    pub fn new(label: impl Into<String>, on: bool) -> Self {
        Self {
            label: label.into(),
            on,
            alert: false,
            on_toggle: None,
            width: 24.0,
            height: 20.0,
            style: AudioStyle::default(),
        }
    }

    /// Enables the toggle; a press publishes the new state.
    pub fn on_toggle(mut self, callback: impl Fn(bool) -> Message + 'a) -> Self {
        self.on_toggle = Some(Box::new(callback));
        self
    }

    /// Use the alert colour when on.
    pub fn alert(mut self, alert: bool) -> Self {
        self.alert = alert;
        self
    }

    /// Size in logical pixels (default 24 x 20).
    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Colours; see `Tokens::audio_style`.
    pub fn style(mut self, style: AudioStyle) -> Self {
        self.style = style;
        self
    }

    fn colours(&self) -> (iced::Color, iced::Color) {
        let style = self.style;
        match (self.on, self.alert) {
            (false, _) => (style.track, style.muted_text),
            (true, false) => (style.active, style.active_text),
            (true, true) => (style.alert, style.alert_text),
        }
    }
}

impl<Message, Theme, Renderer: text::Renderer> Widget<Message, Theme, Renderer>
    for Toggle<'_, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.width), Length::Fixed(self.height))
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
        _tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event
            && let Some(on_toggle) = &self.on_toggle
            && !shell.is_event_captured()
            && cursor.is_over(layout.bounds())
        {
            // Flip locally too, so a second press in the same batch flips back.
            self.on = !self.on;
            shell.publish(on_toggle(self.on));
            shell.capture_event();
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
        let (fill, label) = self.colours();
        quad(
            renderer,
            bounds,
            fill,
            self.style.radius,
            Some(self.style.border),
        );
        renderer.fill_text(
            text::Text {
                content: self.label.clone(),
                bounds: bounds.size(),
                size: (bounds.height * 0.6).max(8.0).into(),
                line_height: text::LineHeight::default(),
                font: renderer.default_font(),
                align_x: text::Alignment::Center,
                align_y: iced::alignment::Vertical::Center,
                shaping: text::Shaping::Basic,
                wrapping: text::Wrapping::None,
            },
            Point::new(bounds.center_x(), bounds.center_y()),
            label,
            bounds,
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.on_toggle.is_some() && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: text::Renderer + 'a> From<Toggle<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(toggle: Toggle<'a, Message>) -> Self {
        Element::new(toggle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(toggle: &mut Toggle<'_, bool>, at: Point) -> Vec<bool> {
        let mut tree = Tree::empty();
        let node = layout::Node::new(Size::new(24.0, 20.0));
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        Widget::<bool, iced::Theme, ()>::update(
            toggle,
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(at),
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &Rectangle::with_size(Size::INFINITE),
        );
        messages
    }

    #[test]
    fn press_inside_flips_and_outside_is_ignored() {
        let mut toggle = Toggle::new("M", false).on_toggle(|on| on).alert(true);
        assert_eq!(
            press(&mut toggle, Point::new(30.0, 5.0)),
            Vec::<bool>::new()
        );
        assert_eq!(press(&mut toggle, Point::new(5.0, 5.0)), [true]);
        assert_eq!(press(&mut toggle, Point::new(5.0, 5.0)), [false]);
        let mut disabled: Toggle<'_, bool> = Toggle::new("S", false);
        assert!(press(&mut disabled, Point::new(5.0, 5.0)).is_empty());
    }

    #[test]
    fn colours_follow_state_and_kind() {
        let style = AudioStyle::default();
        assert_eq!(
            Toggle::<()>::new("S", true).colours(),
            (style.active, style.active_text)
        );
        assert_eq!(
            Toggle::<()>::new("M", true).alert(true).colours(),
            (style.alert, style.alert_text)
        );
        assert_eq!(
            Toggle::<()>::new("M", false).alert(true).colours(),
            (style.track, style.muted_text)
        );
    }
}
