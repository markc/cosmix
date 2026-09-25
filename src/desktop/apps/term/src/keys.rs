//! The keyboard path, and the reason it is a widget rather than a
//! subscription.
//!
//! `iced::event::listen_with` looks like the obvious way to read key presses,
//! and it **drops them under load**. Every subscription gets a
//! `futures::channel::mpsc::channel(100)` and the runtime broadcasts into it
//! with `try_send`, logging a warning and discarding the event when it is full
//! (`iced_futures-0.14.0/src/subscription/tracker.rs:91` and `:146`). The
//! subscription's draining future runs on the executor, so while the UI thread
//! is busy repainting — which, in a terminal, is exactly while keys are
//! arriving — nothing drains it.
//!
//! Measured on this frontend before the change: 60 injected characters, 51
//! seen by `update`. Nine keystrokes silently gone, and the terminal looked
//! like it had a stuck key. The Bevy frontend never hit it because Bevy hands
//! key events to observers directly.
//!
//! A widget's `update` is called synchronously during event dispatch and
//! publishes through `Shell`, which the runtime drains in the same turn. There
//! is no channel and nothing to overflow. So the renderer — whichever arm is
//! compiled in — is wrapped in this, and `listen_with` is left to window
//! events, where a dropped resize is corrected by the next one and a burst of
//! a hundred is not a thing that happens.

use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

type KeyHandler<'a, Message> = Box<dyn Fn(&iced::keyboard::Event) -> Option<Message> + 'a>;
type PointerHandler<'a, Message> = Box<dyn Fn(iced::Point) -> Option<Message> + 'a>;
type MouseHandler<'a, Message> =
    Box<dyn Fn(&mouse::Event, Option<iced::Point>) -> Option<Message> + 'a>;
type Redraw<Message> = (
    Option<std::time::Instant>,
    fn(std::time::Instant) -> Message,
);

/// Wraps `content` and reports every key press it sees, losslessly.
pub struct Keys<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    on_press: KeyHandler<'a, Message>,
    on_pointer: Option<PointerHandler<'a, Message>>,
    on_mouse: Option<MouseHandler<'a, Message>>,
    redraw: Option<Redraw<Message>>,
}

/// Wrap `content` so `on_press` sees every keyboard event.
pub fn keys<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    on_press: impl Fn(&iced::keyboard::Event) -> Option<Message> + 'a,
) -> Keys<'a, Message, Theme, Renderer> {
    Keys {
        content: content.into(),
        on_press: Box::new(on_press),
        on_pointer: None,
        on_mouse: None,
        redraw: None,
    }
}

impl<'a, Message, Theme, Renderer> Keys<'a, Message, Theme, Renderer> {
    /// Buttons and drag endpoints use the same lossless path as keys. The
    /// callback claims only terminal events, leaving tab-strip widgets alone.
    pub fn on_mouse(
        mut self,
        callback: impl Fn(&mouse::Event, Option<iced::Point>) -> Option<Message> + 'a,
    ) -> Self {
        self.on_mouse = Some(Box::new(callback));
        self
    }

    pub fn on_pointer(mut self, callback: impl Fn(iced::Point) -> Option<Message> + 'a) -> Self {
        self.on_pointer = Some(Box::new(callback));
        self
    }

    /// iced drains widget messages and rebuilds the UI before drawing. The
    /// last handled timestamp prevents its redraw retry from painting twice.
    pub fn on_redraw(
        mut self,
        last: Option<std::time::Instant>,
        message: fn(std::time::Instant) -> Message,
    ) -> Self {
        self.redraw = Some((last, message));
        self
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for Keys<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Mouse(iced::mouse::Event::CursorMoved { position }) = event
            && let Some(callback) = &self.on_pointer
            && let Some(message) = callback(*position)
        {
            shell.publish(message);
        }
        if let Event::Window(iced::window::Event::RedrawRequested(at)) = event
            && let Some((last, message)) = self.redraw
            && last != Some(*at)
        {
            shell.publish(message(*at));
        }
        if let Event::Mouse(event) = event
            && let Some(callback) = &self.on_mouse
            && let Some(message) = callback(event, cursor.position())
        {
            shell.publish(message);
            shell.capture_event();
            return;
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        // After the child, and only if the child did not claim it: a future
        // text field or menu in the tree (T3) must win the key it is focused
        // on, exactly as it does in the Bevy frontend.
        if shell.is_event_captured() {
            return;
        }
        if let Event::Keyboard(event) = event
            && let Some(message) = (self.on_press)(event)
        {
            shell.publish(message);
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<Keys<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(keys: Keys<'a, Message, Theme, Renderer>) -> Self {
        Element::new(keys)
    }
}
