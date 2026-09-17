//! Single-line iced input with bounded, selection-aware undo history.
use std::cell::RefCell;
use std::time::{Duration, Instant};

use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, renderer, text, widget};
use iced::widget::text_input::{self, TextInput};
use iced::{Element, Event, Length, Padding, Pixels, Rectangle, Size, Theme, keyboard, mouse};

type Selection = text_input::cursor::State;

#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    value: String,
    selection: Selection,
}

#[derive(Clone, Debug)]
struct Edit {
    before: Snapshot,
    after: Snapshot,
}

#[derive(Default)]
struct History {
    expected: String,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    typing_at: Option<Instant>,
    composing: bool,
}

impl History {
    fn record(&mut self, before: Snapshot, after: Snapshot, typing: bool, now: Instant) {
        if before.value == after.value {
            return;
        }
        let coalesce = typing
            && self
                .typing_at
                .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
            && self.undo.last().is_some_and(|edit| edit.after == before);
        self.expected.clone_from(&after.value);
        if coalesce {
            self.undo.last_mut().expect("existing typing group").after = after;
        } else {
            self.undo.push(Edit { before, after });
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.typing_at = typing.then_some(now);
    }

    fn restore(&mut self, redo: bool) -> Option<Snapshot> {
        self.typing_at = None;
        let (source, destination) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        let edit = source.pop()?;
        let result = if redo {
            edit.after.clone()
        } else {
            edit.before.clone()
        };
        self.expected.clone_from(&result.value);
        destination.push(edit);
        Some(result)
    }
}

/// A controlled input: store each `on_input` message's value in application state.
///
/// Text entry, selection, clipboard handling and IME remain iced's responsibility.
/// Adjacent non-whitespace typing within one second forms an undo group. Cursor
/// movement, paste, composition, whitespace and deletion end the group. Up to
/// 100 groups are retained; an external value replacement clears history.
pub struct TextField<'a, Message, Renderer: text::Renderer> {
    input: TextInput<'a, String, Theme, Renderer>,
    value: String,
    on_input: Option<Box<dyn Fn(String) -> Message + 'a>>,
}

impl<'a, Message, Renderer: text::Renderer> TextField<'a, Message, Renderer> {
    /// Creates an input with a placeholder and the application's current value.
    pub fn new(placeholder: &str, value: &str) -> Self {
        Self {
            input: TextInput::new(placeholder, value),
            value: value.into(),
            on_input: None,
        }
    }

    /// Enables editing and maps new values into application messages.
    pub fn on_input(mut self, callback: impl Fn(String) -> Message + 'a) -> Self {
        self.input = self.input.on_input(std::convert::identity);
        self.on_input = Some(Box::new(callback));
        self
    }

    /// Uses iced's password masking and secure input-method purpose.
    pub fn secure(mut self, secure: bool) -> Self {
        self.input = self.input.secure(secure);
        self
    }

    /// Sets an ID for iced focus and selection operations.
    pub fn id(mut self, id: impl Into<widget::Id>) -> Self {
        self.input = self.input.id(id);
        self
    }

    /// Sets the input width.
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.input = self.input.width(width);
        self
    }

    /// Sets the input padding.
    pub fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.input = self.input.padding(padding);
        self
    }

    /// Sets the text size.
    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.input = self.input.size(size);
        self
    }

    /// Applies a style, including the crate's design-token adapter.
    pub fn style(
        mut self,
        style: impl Fn(&Theme, text_input::Status) -> text_input::Style + 'a,
    ) -> Self {
        self.input = self.input.style(style);
        self
    }
}

impl<Message, Renderer: text::Renderer> Widget<Message, Theme, Renderer>
    for TextField<'_, Message, Renderer>
{
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<History>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(History {
            expected: self.value.clone(),
            ..History::default()
        })
    }

    fn children(&self) -> Vec<widget::Tree> {
        vec![widget::Tree::new(
            &self.input as &dyn Widget<String, Theme, Renderer>,
        )]
    }

    fn diff(&self, tree: &mut widget::Tree) {
        self.input.diff(&mut tree.children[0]);
        let history = tree.state.downcast_mut::<History>();
        if history.expected != self.value {
            *history = History {
                expected: self.value.clone(),
                ..History::default()
            };
        }
    }

    fn size(&self) -> Size<Length> {
        Widget::size(&self.input)
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        Widget::layout(&mut self.input, &mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.input
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let history = tree.state.downcast_mut::<History>();
        let child = &mut tree.children[0];
        let state = child
            .state
            .downcast_mut::<text_input::State<Renderer::Paragraph>>();
        let selection = state.cursor().state(&text_input::Value::new(&self.value));
        if state.is_focused()
            && self.on_input.is_some()
            && !history.composing
            && let Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(key),
                modifiers,
                ..
            }) = event
            && modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
            && (key.eq_ignore_ascii_case("z") || key.eq_ignore_ascii_case("y"))
        {
            if let Some(snapshot) =
                history.restore(key.eq_ignore_ascii_case("y") || modifiers.shift())
            {
                match snapshot.selection {
                    Selection::Index(index) => state.move_cursor_to(index),
                    Selection::Selection { start, end } => state.select_range(start, end),
                }
                shell.publish(self.on_input.as_ref().expect("enabled input")(
                    snapshot.value,
                ));
                shell.invalidate_widgets();
                shell.request_redraw();
            }
            shell.capture_event();
            return;
        }

        let typing = matches!(event,
            Event::Keyboard(keyboard::Event::KeyPressed { text: Some(text), modifiers, .. })
            if !modifiers.control() && !modifiers.alt() && !modifiers.logo()
                && text.chars().count() == 1 && !text.chars().any(char::is_whitespace))
            && matches!(selection, Selection::Index(_))
            && !history.composing;
        match event {
            Event::InputMethod(iced::advanced::input_method::Event::Preedit(text, _)) => {
                history.composing = !text.is_empty()
            }
            Event::InputMethod(
                iced::advanced::input_method::Event::Commit(_)
                | iced::advanced::input_method::Event::Closed,
            ) => history.composing = false,
            _ => {}
        }
        // Redraws and modifier releases must not split an otherwise contiguous group.
        if !typing
            && matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyPressed { .. })
                    | Event::Mouse(mouse::Event::ButtonPressed(_))
                    | Event::Touch(_)
                    | Event::InputMethod(_)
                    | Event::Window(iced::window::Event::Unfocused)
            )
        {
            history.typing_at = None;
        }
        let mut messages = Vec::new();
        let mut inner_shell = Shell::new(&mut messages);
        self.input.update(
            child,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            &mut inner_shell,
            viewport,
        );
        let input_state = child
            .state
            .downcast_ref::<text_input::State<Renderer::Paragraph>>();
        let history = RefCell::new(history);
        let value = RefCell::new(&mut self.value);
        shell.merge(inner_shell, |next| {
            let after = Snapshot {
                selection: input_state.cursor().state(&text_input::Value::new(&next)),
                value: next.clone(),
            };
            let before = Snapshot {
                value: (**value.borrow()).clone(),
                selection,
            };
            history
                .borrow_mut()
                .record(before, after, typing, Instant::now());
            **value.borrow_mut() = next.clone();
            self.on_input
                .as_ref()
                .expect("iced only emits for enabled input")(next)
        });
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        Widget::draw(
            &self.input,
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.input
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
}

impl<'a, Message: 'a, Renderer: text::Renderer + 'a> From<TextField<'a, Message, Renderer>>
    for Element<'a, Message, Theme, Renderer>
{
    fn from(input: TextField<'a, Message, Renderer>) -> Self {
        Element::new(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(value: &str, cursor: usize) -> Snapshot {
        Snapshot {
            value: value.into(),
            selection: Selection::Index(cursor),
        }
    }

    #[test]
    fn adjacent_typing_coalesces_and_redoes() {
        let mut history = History::default();
        let now = Instant::now();
        history.record(snap("", 0), snap("a", 1), true, now);
        history.record(
            snap("a", 1),
            snap("ab", 2),
            true,
            now + Duration::from_millis(100),
        );
        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.restore(false), Some(snap("", 0)));
        assert_eq!(history.restore(true), Some(snap("ab", 2)));
    }

    #[test]
    fn pause_and_cursor_movement_split_groups() {
        let mut history = History::default();
        let now = Instant::now();
        history.record(snap("", 0), snap("a", 1), true, now);
        history.record(
            snap("a", 1),
            snap("ab", 2),
            true,
            now + Duration::from_secs(2),
        );
        history.record(
            snap("ab", 0),
            snap("cab", 1),
            true,
            now + Duration::from_millis(2100),
        );
        assert_eq!(history.undo.len(), 3);
    }

    #[test]
    fn selection_and_composition_are_atomic_and_new_edits_clear_redo() {
        let before = Snapshot {
            value: "abcd".into(),
            selection: Selection::Selection { start: 3, end: 1 },
        };
        let mut history = History::default();
        let now = Instant::now();
        history.record(before.clone(), snap("a界d", 2), false, now);
        assert_eq!(history.restore(false), Some(before.clone()));
        history.record(before, snap("ad", 1), false, now);
        assert_eq!(history.restore(true), None);
    }

    #[test]
    fn history_is_bounded_and_noops_do_not_clear_redo() {
        let mut history = History::default();
        let now = Instant::now();
        for n in 0..110 {
            history.record(
                snap(&n.to_string(), 0),
                snap(&(n + 1).to_string(), 0),
                false,
                now,
            );
        }
        assert_eq!(history.undo.len(), 100);
        history.restore(false);
        history.record(snap("109", 0), snap("109", 0), false, now);
        assert!(history.restore(true).is_some());
    }
}
