//! The root key router (ced E1 plan §2, §4.6): an iced widget that sees every
//! key event before its children (the `apps/term/src/keys.rs` pattern — never
//! `event::listen`, which drops events under load), resolves
//! [`crate::keymap`] chords and Alt+mnemonics to actions, and passes
//! everything else down to the focused child.
//!
//! Precedence, in order:
//! 1. A modal dialog is open: nothing is resolved here except Escape (after
//!    the dialog had its chance); the dialog owns the keyboard.
//! 2. Alt+<mnemonic> (no Ctrl) opens that menu.
//! 3. A bound chord runs its action — unless a chrome text field (find bar)
//!    has focus and the chord is a text-editing one (Ctrl+A/C/V/X/Z/Y,
//!    Ctrl+Backspace/Delete, Shift+Insert…): the field keeps those.
//! 4. Everything else goes to the children (the editor widget types it).
//! 5. A key nobody captured (Escape, and Tab / arrows in a dialog) goes to
//!    the `on_unclaimed` hook: Escape closes the find bar or a dialog, Tab
//!    completes a path in the file dialog.
//!
//! Ctrl+wheel is zoom, taken before the editor would scroll with it.
//! F10 is not handled here: the menu bar widget opens itself on it.

use std::collections::HashMap;

use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use iced::keyboard::{self, Key, key::Named};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

use crate::actions::{ActionId, Menu};
use crate::keymap::{self, Chord};

/// What a chord runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    Action(ActionId),
    /// A macro, by stem.
    Macro(String),
}

/// Chord → binding, built from the frozen defaults plus macro chords.
#[derive(Debug, Clone, Default)]
pub struct Bindings {
    map: HashMap<Chord, Binding>,
}

impl Bindings {
    /// The defaults plus `macros` (`(stem, chord text)`; chords that do not
    /// parse or are taken are skipped — `macros::discover` already reported
    /// them).
    pub fn new<'a>(macros: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut map = HashMap::new();
        for (text, action) in keymap::DEFAULT {
            if let Some(chord) = keymap::parse_chord(text) {
                map.insert(chord, Binding::Action(*action));
            }
        }
        for (stem, text) in macros {
            if let Some(chord) = keymap::parse_chord(text) {
                map.entry(chord).or_insert_with(|| Binding::Macro(stem.to_owned()));
            }
        }
        Self { map }
    }

    pub fn get(&self, chord: &Chord) -> Option<&Binding> {
        self.map.get(chord)
    }
}

/// The chord a key press spells, in `keymap` terms. Letters come from the
/// Latin layout position when the layout is not Latin (so Ctrl+S works on a
/// Cyrillic layout, as in Notepad++).
pub fn chord_of(key: &Key, physical: keyboard::key::Physical, modifiers: keyboard::Modifiers) -> Option<Chord> {
    let name = match key {
        Key::Named(named) => named_key(*named)?.to_owned(),
        Key::Character(s) => {
            let c = key.to_latin(physical).or_else(|| s.chars().next())?;
            let c = match c {
                // Shifted digits and symbols on a US layout.
                '+' => '=',
                '_' => '-',
                '?' => '/',
                c => c,
            };
            if !(c.is_ascii_alphanumeric() || "=-/".contains(c)) {
                return None;
            }
            c.to_ascii_lowercase().to_string()
        }
        Key::Unidentified => return None,
    };
    Some(Chord { ctrl: modifiers.control(), alt: modifiers.alt(), shift: modifiers.shift(), key: name })
}

fn named_key(named: Named) -> Option<&'static str> {
    Some(match named {
        Named::F1 => "F1",
        Named::F2 => "F2",
        Named::F3 => "F3",
        Named::F4 => "F4",
        Named::F5 => "F5",
        Named::F6 => "F6",
        Named::F7 => "F7",
        Named::F8 => "F8",
        Named::F9 => "F9",
        Named::F10 => "F10",
        Named::F11 => "F11",
        Named::F12 => "F12",
        Named::Tab => "Tab",
        Named::PageUp => "PageUp",
        Named::PageDown => "PageDown",
        Named::ArrowUp => "Up",
        Named::ArrowDown => "Down",
        Named::ArrowLeft => "Left",
        Named::ArrowRight => "Right",
        Named::Home => "Home",
        Named::End => "End",
        Named::Backspace => "Backspace",
        Named::Delete => "Delete",
        Named::Insert => "Insert",
        Named::Enter => "Enter",
        Named::Escape => "Escape",
        _ => return None,
    })
}

/// Chords a focused text field needs for itself.
pub fn is_text_editing(chord: &Chord) -> bool {
    let key = chord.key.as_str();
    if chord.ctrl && !chord.alt {
        return matches!(key, "a" | "c" | "v" | "x" | "z" | "y" | "Backspace" | "Delete" | "Insert" | "Left" | "Right" | "Home" | "End");
    }
    !chord.ctrl && !chord.alt && matches!(key, "Insert" | "Delete")
}

/// Alt+<letter> without Ctrl → the menu index it opens.
pub fn mnemonic(chord: &Chord) -> Option<usize> {
    if !chord.alt || chord.ctrl || chord.shift {
        return None;
    }
    let c = chord.key.chars().next()?;
    (chord.key.chars().count() == 1).then_some(())?;
    Menu::ALL.iter().position(|m| m.mnemonic() == c)
}

/// What the router decided for a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    OpenMenu(usize),
    Run(Binding),
}

/// The pure routing decision (tested without a widget tree).
pub fn route(bindings: &Bindings, chord: &Chord, modal: bool, text_field: bool) -> Option<Routed> {
    if modal {
        return None;
    }
    if let Some(index) = mnemonic(chord) {
        return Some(Routed::OpenMenu(index));
    }
    if text_field && is_text_editing(chord) {
        return None;
    }
    bindings.get(chord).cloned().map(Routed::Run)
}

type RouteFn<'a, Message> = Box<dyn Fn(Routed) -> Message + 'a>;
type UnclaimedFn<'a, Message> = Box<dyn Fn(&Key, keyboard::Modifiers) -> Option<Message> + 'a>;

/// Wraps the whole window content.
pub struct KeyRouter<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    bindings: &'a Bindings,
    modal: bool,
    text_field: bool,
    on_route: RouteFn<'a, Message>,
    on_unclaimed: Option<UnclaimedFn<'a, Message>>,
    on_zoom: Option<Box<dyn Fn(f32) -> Message + 'a>>,
}

pub fn router<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    bindings: &'a Bindings,
    on_route: impl Fn(Routed) -> Message + 'a,
) -> KeyRouter<'a, Message, Theme, Renderer> {
    KeyRouter {
        content: content.into(),
        bindings,
        modal: false,
        text_field: false,
        on_route: Box::new(on_route),
        on_unclaimed: None,
        on_zoom: None,
    }
}

impl<'a, Message, Theme, Renderer> KeyRouter<'a, Message, Theme, Renderer> {
    /// A modal dialog is open.
    pub fn modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    /// A chrome text field has keyboard focus.
    pub fn text_field(mut self, focused: bool) -> Self {
        self.text_field = focused;
        self
    }

    /// Called for a key press no child captured.
    pub fn on_unclaimed(mut self, f: impl Fn(&Key, keyboard::Modifiers) -> Option<Message> + 'a) -> Self {
        self.on_unclaimed = Some(Box::new(f));
        self
    }

    /// Ctrl+wheel: published with the vertical line delta (+ = zoom in).
    pub fn on_zoom(mut self, f: impl Fn(f32) -> Message + 'a) -> Self {
        self.on_zoom = Some(Box::new(f));
        self
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for KeyRouter<'_, Message, Theme, Renderer>
where
    Message: Clone,
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

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
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
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, physical_key, modifiers, .. }) => {
                if let Some(chord) = chord_of(key, *physical_key, *modifiers)
                    && let Some(routed) = route(self.bindings, &chord, self.modal, self.text_field)
                {
                    shell.publish((self.on_route)(routed));
                    shell.capture_event();
                    return;
                }
            }
            Event::Mouse(iced::mouse::Event::WheelScrolled { delta }) if !self.modal => {
                if let Some(zoom) = &self.on_zoom
                    && cursor.is_over(layout.bounds())
                    && ctrl_held()
                {
                    let lines = match delta {
                        iced::mouse::ScrollDelta::Lines { y, .. } => *y,
                        iced::mouse::ScrollDelta::Pixels { y, .. } => *y / 40.0,
                    };
                    shell.publish(zoom(lines));
                    shell.capture_event();
                    return;
                }
            }
            _ => {}
        }
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(m)) = event {
            MODIFIERS.with(|cell| cell.set(*m));
        }
        self.content
            .as_widget_mut()
            .update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
        if !shell.is_event_captured()
            && let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
            && let Some(f) = &self.on_unclaimed
            && let Some(message) = f(key, *modifiers)
        {
            shell.publish(message);
            shell.capture_event();
        }
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
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

/// Shows `content` but keeps keyboard and IME input from it — what sits
/// under a modal dialog, so typing into the dialog never also types into the
/// editor behind it.
pub struct Inert<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
}

pub fn inert<'a, Message, Theme, Renderer>(content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Inert<'a, Message, Theme, Renderer> {
    Inert { content: content.into() }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Inert<'_, Message, Theme, Renderer>
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

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
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
        let input = matches!(
            event,
            Event::Keyboard(keyboard::Event::KeyPressed { .. } | keyboard::Event::KeyReleased { .. })
                | Event::InputMethod(_)
        );
        if !input {
            self.content
                .as_widget_mut()
                .update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
        }
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
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<Inert<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(inert: Inert<'a, Message, Theme, Renderer>) -> Self {
        Element::new(inert)
    }
}

/// Publishes `message` when a mouse button goes down over `content`, before
/// the content sees it and without capturing it — how the app learns that a
/// chrome text field took keyboard focus (iced text inputs do not say).
pub struct FocusProbe<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    message: Message,
}

pub fn focus_probe<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    message: Message,
) -> FocusProbe<'a, Message, Theme, Renderer> {
    FocusProbe { content: content.into(), message }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for FocusProbe<'_, Message, Theme, Renderer>
where
    Message: Clone,
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

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(tree, layout, renderer, operation);
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
        if let Event::Mouse(iced::mouse::Event::ButtonPressed(_)) = event
            && cursor.is_over(layout.bounds())
        {
            shell.publish(self.message.clone());
        }
        self.content
            .as_widget_mut()
            .update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
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
        self.content.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<FocusProbe<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(probe: FocusProbe<'a, Message, Theme, Renderer>) -> Self {
        Element::new(probe)
    }
}

thread_local! {
    /// Modifier state as last reported (a wheel event carries none). The UI
    /// runs on one thread, so a thread-local is exact.
    static MODIFIERS: std::cell::Cell<keyboard::Modifiers> = const { std::cell::Cell::new(keyboard::Modifiers::empty()) };
}

fn ctrl_held() -> bool {
    MODIFIERS.with(|cell| cell.get().control())
}

impl<'a, Message, Theme, Renderer> From<KeyRouter<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(router: KeyRouter<'a, Message, Theme, Renderer>) -> Self {
        Element::new(router)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::{NativeCode, Physical};

    fn chord(text: &str) -> Chord {
        keymap::parse_chord(text).unwrap()
    }

    fn unidentified() -> Physical {
        Physical::Unidentified(NativeCode::Unidentified)
    }

    #[test]
    fn key_events_spell_keymap_chords() {
        let c = |s: &str| Key::Character(s.into());
        let ctrl = keyboard::Modifiers::CTRL;
        assert_eq!(chord_of(&c("s"), unidentified(), ctrl), Some(chord("Ctrl+S")));
        assert_eq!(chord_of(&c("S"), unidentified(), ctrl | keyboard::Modifiers::SHIFT), Some(chord("Ctrl+Shift+S")));
        assert_eq!(chord_of(&c("+"), unidentified(), ctrl | keyboard::Modifiers::SHIFT), Some(chord("Ctrl+Shift+=")));
        assert_eq!(chord_of(&c("/"), unidentified(), ctrl), Some(chord("Ctrl+/")));
        assert_eq!(chord_of(&Key::Named(Named::F3), unidentified(), keyboard::Modifiers::SHIFT), Some(chord("Shift+F3")));
        assert_eq!(chord_of(&Key::Named(Named::ArrowUp), unidentified(), ctrl | keyboard::Modifiers::SHIFT), Some(chord("Ctrl+Shift+Up")));
        assert_eq!(chord_of(&c("é"), unidentified(), ctrl), None);
    }

    #[test]
    fn every_default_binding_routes_and_none_is_shadowed_by_a_mnemonic() {
        let b = Bindings::new([]);
        for (text, action) in keymap::DEFAULT {
            assert_eq!(route(&b, &chord(text), false, false), Some(Routed::Run(Binding::Action(*action))), "{text}");
        }
    }

    #[test]
    fn mnemonics_open_menus_in_bar_order() {
        let b = Bindings::new([]);
        for (index, menu) in Menu::ALL.iter().enumerate() {
            let text = format!("Alt+{}", menu.mnemonic().to_ascii_uppercase());
            assert_eq!(route(&b, &chord(&text), false, false), Some(Routed::OpenMenu(index)), "{text}");
        }
        assert_eq!(mnemonic(&chord("Ctrl+Alt+F")), None);
    }

    #[test]
    fn modal_and_text_fields_keep_their_keys() {
        let b = Bindings::new([]);
        assert_eq!(route(&b, &chord("Ctrl+S"), true, false), None, "a dialog owns the keyboard");
        assert_eq!(route(&b, &chord("Ctrl+V"), false, true), None, "the find field pastes");
        assert_eq!(route(&b, &chord("Ctrl+Z"), false, true), None);
        assert_eq!(
            route(&b, &chord("F3"), false, true),
            Some(Routed::Run(Binding::Action(ActionId::SearchFindNext))),
            "F3 still finds from inside the field"
        );
        assert_eq!(route(&b, &chord("Ctrl+S"), false, true), Some(Routed::Run(Binding::Action(ActionId::FileSave))));
    }

    #[test]
    fn macro_chords_bind_but_never_override_defaults() {
        let b = Bindings::new([("upper", "Ctrl+Alt+U"), ("evil", "Ctrl+S")]);
        assert_eq!(b.get(&chord("Ctrl+Alt+U")), Some(&Binding::Macro("upper".into())));
        assert_eq!(b.get(&chord("Ctrl+S")), Some(&Binding::Action(ActionId::FileSave)));
    }
}
