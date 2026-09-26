//! The root key router, in the shape of ced's `keys.rs` (the
//! `apps/term/src/keys.rs` pattern — never `event::listen`, which drops keys
//! under load) but resolving through [`cosmix_actions`] — the same
//! engine-independent keymap filemgr uses, so a `keymap.conf.mix` written for
//! one drives the other.
//!
//! - Keymap: the packaged filemgr defaults ([`FILEMGR_DEFAULT_KEYMAP_MIX`])
//!   layered with the user's `<config>/keymap.conf.mix` overlay —
//!   [`load`] is filemgr's `load_effective_keymap` verbatim. Invalid overlays
//!   keep the current keymap.
//! - Hot reload: on window focus the app calls [`reload`]; a changed overlay
//!   replaces the keymap and cancels any pending chord (filemgr's
//!   `reload_keymap_on_focus` rule).
//! - Resolution: every key event through the widget becomes a [`RawInput`],
//!   resolved against a [`FocusContext`] built from the router's focus state
//!   (`focus_editable` is on while a location bar is being edited, so chords
//!   with `allow_in_editable: false` — every default — route the keys into
//!   the editor instead of firing actions; `modal` is set while a dialog is
//!   up, and its [`FocusContext::modal_scope`] suppresses every non-modal
//!   binding — filemgr's `ModalCapture::top_owner` rule, so no file/browse
//!   chord fires under a dialog) with a monotonic [`Tick`]. Emitted actions
//!   are published to the app; the app decides which are keyboard-served
//!   and which are refused.
//! - Modal capture: while a dialog is up the router turns Enter and Escape
//!   into [`ModalKey`] messages BEFORE its children see them (the
//!   [`crate::view::location::Capture`] shape, generalised): the dialog owns
//!   them (Enter = confirm/submit, Escape = dismiss) and the prompt's text
//!   field receives every other keystroke untouched.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use iced::keyboard::{self, Key, key::Named};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

use cosmix_actions::{
    ActionId, FocusContext, Key as AKey, Keymap, Modifiers as AModifiers, RawInput, RawInputState,
    ResolveState, Tick, resolve,
};
use cosmix_actions::{FILEMGR_DEFAULT_KEYMAP_MIX, load_keymap, parse_keymap};

/// The effective keymap: packaged filemgr defaults + the user's overlay
/// (filemgr/src/action.rs `load_effective_keymap`, kept in step).
pub fn load(custom_path: Option<&Path>) -> Result<Keymap, String> {
    let mut keymap = parse_keymap(FILEMGR_DEFAULT_KEYMAP_MIX)
        .expect("checked-in FileMgr keymap must stay valid");
    let Some(path) = custom_path else {
        return Ok(keymap);
    };
    if !path.exists() {
        return Ok(keymap);
    }
    let custom = load_keymap(path)
        .map_err(|error| format!("loading dopus keymap overlay {}: {error}", path.display()))?;
    keymap.chord_timeout_ms = custom.chord_timeout_ms;
    keymap.custom = custom.custom;
    keymap
        .validate()
        .map_err(|error| format!("invalid dopus keymap overlay {}: {error}", path.display()))?;
    Ok(keymap)
}

/// The keymap plus the chord-progress state one input adapter owns. Shared
/// between the router widget and the app (hot reload, the tick poll, the
/// focus hand-off) behind a mutex — both live on the UI thread, so
/// contention is nil.
#[derive(Default)]
pub struct Router {
    pub keymap: Keymap,
    pub state: ResolveState,
    /// Whether the focused widget edits text (a location bar). The resolver
    /// sees it through the [`FocusContext`]; the app flips it when a location
    /// bar enters or leaves edit mode.
    pub focus_editable: bool,
    /// The modal scope name while a dialog owns the keyboard
    /// ([`MODAL_SCOPE`]); `None` otherwise.
    pub modal: Option<&'static str>,
}

pub type SharedRouter = Arc<Mutex<Router>>;

/// The modal scope a dialog is captured under. No default binding carries a
/// `modal:` scope, so setting this suppresses EVERY default chord — exactly
/// filemgr's `ModalCapture::top_owner` posture via
/// [`FocusContext::modal_scope`].
pub const MODAL_SCOPE: &str = "dopus.dialog";

/// Build the starting router (packaged defaults, overlay applied on top).
pub fn initial(custom_path: Option<&Path>) -> Result<SharedRouter, String> {
    Ok(Arc::new(Mutex::new(Router {
        keymap: load(custom_path)?,
        state: ResolveState::default(),
        focus_editable: false,
        modal: None,
    })))
}

/// Hand focus to (or take it back from) a text editor. Every default binding
/// is `allow_in_editable: false`, so while this is on the resolver emits no
/// actions and keys fall through to the editor widget.
pub fn set_focus_editable(shared: &SharedRouter, editable: bool) {
    let mut router = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if router.focus_editable != editable {
        router.focus_editable = editable;
        // Focus changed hands: a half-typed chord must not fire into (or
        // out of) the new context.
        router.state.cancel();
    }
}

/// Open (`true`) or close (`false`) the modal scope: while open, no default
/// chord resolves (every default is global-scope and [`FocusContext::admits`]
/// rejects globals under a modal), and the router turns Enter/Escape into
/// [`ModalKey`] messages for the dialog.
pub fn set_modal(shared: &SharedRouter, open: bool) {
    let mut router = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let next = open.then_some(MODAL_SCOPE);
    if router.modal != next {
        router.modal = next;
        // The keyboard changed owners: a half-typed chord must not fire into
        // (or out of) the dialog.
        router.state.cancel();
    }
}

/// The context the resolver resolves against this tick: the modal scope when
/// a dialog is up (which suppresses every global binding regardless of the
/// editable flag — the modal owns the keyboard outright), else the editable
/// flag from [`Router::focus_editable`].
fn focus_context(router: &Router) -> FocusContext {
    match router.modal {
        // MODAL_SCOPE is a checked-in constant, so the validator never
        // refuses it here.
        Some(scope) => FocusContext::modal(scope).unwrap_or_else(|_| FocusContext::global()),
        None => FocusContext::global().with_editable(router.focus_editable),
    }
}

/// A key the open dialog owns: Enter confirms/submits, Escape dismisses
/// (the app maps both through the front dialog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKey {
    Confirm,
    Dismiss,
}

/// Hot reload on window focus (filemgr's `reload_keymap_on_focus`): replace
/// the keymap when the overlay changed, keep it on error, cancel a pending
/// chord either way.
pub fn reload(shared: &SharedRouter, custom_path: Option<&Path>) {
    let Ok(reloaded) = load(custom_path) else {
        // load() already validated; a broken overlay keeps the current keymap.
        return;
    };
    let mut router = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if reloaded == router.keymap {
        return;
    }
    router.keymap = reloaded;
    router.state.cancel();
}

/// Resolve a chord whose deadline expired while idle: the app's `Msg::Tick`
/// calls this every 200 ms, so a pending chord times out without waiting for
/// the next keypress. Returns any actions the expiry emitted.
pub fn poll_timeout(shared: &SharedRouter) -> Vec<ActionId> {
    let mut router = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = tick();
    let context = focus_context(&router);
    let Router { keymap, state, .. } = &mut *router;
    if state.deadline().is_some_and(|deadline| now >= deadline) {
        cosmix_actions::resolve_timeout(&context, keymap, state, now).actions
    } else {
        Vec::new()
    }
}

/// Milliseconds since process start, the monotonic [`Tick`] cosmix-actions
/// resolves against (it never reads a clock itself).
pub fn tick() -> Tick {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    Tick(EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64)
}

/// iced's logical key → the cosmix-actions physical vocabulary. Characters
/// use the Latin layout position when the layout is not Latin (ced's rule);
/// only the ASCII alphanumeric core of the vocabulary is reachable from a
/// character key.
fn iced_key(key: &Key, physical: keyboard::key::Physical) -> Option<AKey> {
    match key {
        Key::Named(named) => named_key(*named),
        Key::Character(s) => {
            let c = key.to_latin(physical).or_else(|| s.chars().next())?;
            AKey::character(c).ok()
        }
        Key::Unidentified => None,
    }
}

fn named_key(named: Named) -> Option<AKey> {
    Some(match named {
        Named::Space => AKey::Space,
        Named::Enter => AKey::Enter,
        Named::Escape => AKey::Escape,
        Named::Tab => AKey::Tab,
        Named::Backspace => AKey::Backspace,
        Named::Delete => AKey::Delete,
        Named::Insert => AKey::Insert,
        Named::Home => AKey::Home,
        Named::End => AKey::End,
        Named::PageUp => AKey::PageUp,
        Named::PageDown => AKey::PageDown,
        Named::ArrowUp => AKey::ArrowUp,
        Named::ArrowDown => AKey::ArrowDown,
        Named::ArrowLeft => AKey::ArrowLeft,
        Named::ArrowRight => AKey::ArrowRight,
        Named::F1 => AKey::Function(1),
        Named::F2 => AKey::Function(2),
        Named::F3 => AKey::Function(3),
        Named::F4 => AKey::Function(4),
        Named::F5 => AKey::Function(5),
        Named::F6 => AKey::Function(6),
        Named::F7 => AKey::Function(7),
        Named::F8 => AKey::Function(8),
        Named::F9 => AKey::Function(9),
        Named::F10 => AKey::Function(10),
        Named::F11 => AKey::Function(11),
        Named::F12 => AKey::Function(12),
        _ => return None,
    })
}

fn modifiers(m: keyboard::Modifiers) -> AModifiers {
    AModifiers { control: m.control(), alt: m.alt(), shift: m.shift(), super_key: m.logo() }
}

/// An iced keyboard event becomes the [`RawInput`] the resolver takes,
/// carrying iced's own `repeat` flag (term/src/main.rs's rule — never
/// reconstruct repeat by comparing strokes).
fn key_input(event: &keyboard::Event) -> Option<RawInput> {
    match event {
        keyboard::Event::KeyPressed { key, physical_key, modifiers, repeat, .. } => {
            raw_input(key, *physical_key, *modifiers, true, *repeat)
        }
        keyboard::Event::KeyReleased { key, physical_key, modifiers, .. } => {
            raw_input(key, *physical_key, *modifiers, false, false)
        }
        _ => None,
    }
}

/// The pure translation, testable without a widget tree: an iced key event
/// becomes the [`RawInput`] the resolver takes.
pub fn raw_input(
    key: &Key,
    physical: keyboard::key::Physical,
    mods: keyboard::Modifiers,
    pressed: bool,
    repeat: bool,
) -> Option<RawInput> {
    Some(RawInput {
        key: iced_key(key, physical)?,
        modifiers: modifiers(mods),
        state: if pressed { RawInputState::Pressed } else { RawInputState::Released },
        repeat,
    })
}

type ActionsFn<'a, Message> = Box<dyn Fn(Vec<ActionId>) -> Message + 'a>;
type ModalKeyFn<'a, Message> = Box<dyn Fn(ModalKey) -> Message + 'a>;

/// Wraps the whole window content; sees every key before its children.
/// Resolved actions are published; everything else reaches the children.
pub struct KeyRouter<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    shared: SharedRouter,
    on_actions: ActionsFn<'a, Message>,
    /// A dialog is up: no chord resolves and Enter/Escape become
    /// [`ModalKey`] messages before the children see them.
    modal: bool,
    on_modal_key: Option<ModalKeyFn<'a, Message>>,
}

pub fn router<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    shared: SharedRouter,
    on_actions: impl Fn(Vec<ActionId>) -> Message + 'a,
) -> KeyRouter<'a, Message, Theme, Renderer> {
    KeyRouter {
        content: content.into(),
        shared,
        on_actions: Box::new(on_actions),
        modal: false,
        on_modal_key: None,
    }
}

impl<'a, Message, Theme, Renderer> KeyRouter<'a, Message, Theme, Renderer> {
    /// A modal dialog is up (mirrors [`Router::modal`], which the resolver
    /// sees through the [`FocusContext`]).
    pub fn modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    /// Where Enter/Escape go while a dialog is up.
    pub fn on_modal_key(mut self, f: impl Fn(ModalKey) -> Message + 'a) -> Self {
        self.on_modal_key = Some(Box::new(f));
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
        // A dialog owns Enter and Escape outright (location.rs's `Capture`
        // rule, dialog-wide): publish them before any child — the prompt's
        // text field included — can react, and let every other keystroke
        // through to whoever holds focus.
        if self.modal
            && let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
            && let Some(modal_key) = match key {
                Key::Named(Named::Enter) => Some(ModalKey::Confirm),
                Key::Named(Named::Escape) => Some(ModalKey::Dismiss),
                _ => None,
            }
            && let Some(on_modal_key) = &self.on_modal_key
        {
            shell.publish(on_modal_key(modal_key));
            shell.capture_event();
            return;
        }
        if let Event::Keyboard(keyboard_event) = event
            && let Some(input) = key_input(keyboard_event)
        {
            let resolved = {
                let mut router = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = tick();
                let context = focus_context(&router);
                // Split-borrow the router's own fields so the resolver can
                // hold the keymap while mutating the chord state.
                let Router { keymap, state, .. } = &mut *router;
                let mut resolved = resolve(input, &context, keymap, state, now);
                // A chord that was waiting for a second stroke expired while
                // nothing was pressed: resolve it opportunistically here (the
                // defaults have no multi-stroke chords; users can add them).
                if resolved.actions.is_empty()
                    && let Some(deadline) = state.deadline()
                    && now >= deadline
                {
                    let late = cosmix_actions::resolve_timeout(&context, keymap, state, now);
                    resolved.actions.extend(late.actions);
                }
                resolved
            };
            for diagnostic in &resolved.diagnostics {
                tracing::debug!(?diagnostic, "keymap resolution diagnostic");
            }
            if !resolved.actions.is_empty() {
                shell.publish((self.on_actions)(resolved.actions));
                shell.capture_event();
                return;
            }
            if matches!(resolved.outcome, cosmix_actions::ResolveOutcome::Pending { .. }) {
                // A longer chord may still win: do not let a child treat the
                // stroke as its own (P1: only matters with custom overlays).
                shell.capture_event();
                return;
            }
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
    use cosmix_actions::filemgr;
    use iced::keyboard::key::{NativeCode, Physical};

    fn press(text: &str) -> Option<RawInput> {
        // Modifier prefixes, longest-first and stackable (Ctrl+Shift+N).
        let mut mods = keyboard::Modifiers::empty();
        let mut name = text;
        while let Some((prefix, rest)) = name.split_once('+') {
            match prefix {
                "Ctrl" => mods |= keyboard::Modifiers::CTRL,
                "Alt" => mods |= keyboard::Modifiers::ALT,
                "Shift" => mods |= keyboard::Modifiers::SHIFT,
                _ => break,
            }
            name = rest;
        }
        let (key, physical) = match name {
            "F2" => (Key::Named(Named::F2), Physical::Unidentified(NativeCode::Unidentified)),
            "F5" => (Key::Named(Named::F5), Physical::Unidentified(NativeCode::Unidentified)),
            "ArrowDown" => (Key::Named(Named::ArrowDown), Physical::Unidentified(NativeCode::Unidentified)),
            "ArrowUp" => (Key::Named(Named::ArrowUp), Physical::Unidentified(NativeCode::Unidentified)),
            "Enter" => (Key::Named(Named::Enter), Physical::Unidentified(NativeCode::Unidentified)),
            "Delete" => (Key::Named(Named::Delete), Physical::Unidentified(NativeCode::Unidentified)),
            c => (Key::Character(c.into()), Physical::Unidentified(NativeCode::Unidentified)),
        };
        raw_input(&key, physical, mods, true, false)
    }

    #[test]
    fn the_packaged_defaults_load() {
        let shared = initial(None).unwrap();
        let router = shared.lock().unwrap();
        assert_eq!(router.keymap.defaults.len(), 28);
        assert!(router.keymap.custom.is_empty());
    }

    #[test]
    fn missing_overlay_is_the_packaged_defaults() {
        let keymap = load(Some(Path::new("/nonexistent/keymap.conf.mix"))).unwrap();
        assert_eq!(keymap.defaults.len(), 28);
    }

    #[test]
    fn arrow_keys_resolve_to_selection_actions() {
        let shared = initial(None).unwrap();
        let router = shared.lock().unwrap();
        let mut state = ResolveState::default();
        for (text, action) in [
            ("ArrowDown", filemgr::SELECT_NEXT),
            ("ArrowUp", filemgr::SELECT_PREVIOUS),
            ("F5", filemgr::VIEW_REFRESH),
            ("Ctrl+H", filemgr::VIEW_TOGGLE_HIDDEN),
            ("Ctrl+1", filemgr::VIEW_SORT_NAME),
            ("Ctrl+2", filemgr::VIEW_SORT_SIZE),
            ("Ctrl+3", filemgr::VIEW_SORT_MODIFIED),
        ] {
            let input = press(text).unwrap_or_else(|| panic!("{text}"));
            let resolved = resolve(input, &FocusContext::global(), &router.keymap, &mut state, tick());
            assert_eq!(resolved.actions, vec![action], "{text}");
        }
    }

    #[test]
    fn non_vocabulary_keys_are_no_match_not_a_crash() {
        let shared = initial(None).unwrap();
        let router = shared.lock().unwrap();
        let mut state = ResolveState::default();
        let input = raw_input(
            &Key::Character("é".into()),
            Physical::Unidentified(NativeCode::Unidentified),
            keyboard::Modifiers::empty(),
            true,
            false,
        );
        assert!(input.is_none(), "untranslatable keys never reach the resolver");
        let input = press("Enter").unwrap();
        let resolved = resolve(input, &FocusContext::global(), &router.keymap, &mut state, tick());
        assert_eq!(resolved.actions, vec![filemgr::FILE_OPEN]);
    }

    #[test]
    fn a_modal_suppresses_every_chord_and_the_router_owns_enter_escape() {
        let shared = initial(None).unwrap();
        let mut router = shared.lock().unwrap();
        router.modal = Some(MODAL_SCOPE);
        let context = focus_context(&router);
        let mut state = ResolveState::default();
        // Every one of these resolves to an action globally (Enter is even
        // file.open); under a modal scope none of them fires — the dialog
        // owns the keyboard, and Enter/Escape reach it only through the
        // router's ModalKey capture.
        for text in ["Enter", "Escape", "F5", "Delete", "Ctrl+C", "F2", "Ctrl+Shift+N"] {
            let input = press(text).unwrap_or_else(|| panic!("{text}"));
            let resolved = resolve(input, &context, &router.keymap, &mut state, tick());
            assert!(resolved.actions.is_empty(), "{text} fired under a modal");
        }
    }

    #[test]
    fn releases_resolve_nothing() {
        let shared = initial(None).unwrap();
        let router = shared.lock().unwrap();
        let mut state = ResolveState::default();
        let input = raw_input(
            &Key::Named(Named::ArrowDown),
            Physical::Unidentified(NativeCode::Unidentified),
            keyboard::Modifiers::empty(),
            false,
            false,
        )
        .unwrap();
        let resolved = resolve(input, &FocusContext::global(), &router.keymap, &mut state, tick());
        assert!(resolved.actions.is_empty());
        assert_eq!(resolved.outcome, cosmix_actions::ResolveOutcome::IgnoredRelease);
    }
}
