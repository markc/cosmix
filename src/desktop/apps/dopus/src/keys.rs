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
//!   resolved against [`FocusContext::global`] (P1 has no editables, no
//!   modals) with a monotonic [`Tick`]. Emitted actions are published to the
//!   app; the app decides which are P1 (navigation/view/theme) and which
//!   arrive in P2/P3.

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
/// between the router widget and the app (hot reload, the tick poll) behind
/// a mutex — both live on the UI thread, so contention is nil.
#[derive(Default)]
pub struct Router {
    pub keymap: Keymap,
    pub state: ResolveState,
}

pub type SharedRouter = Arc<Mutex<Router>>;

/// Build the starting router (packaged defaults, overlay applied on top).
pub fn initial(custom_path: Option<&Path>) -> Result<SharedRouter, String> {
    Ok(Arc::new(Mutex::new(Router {
        keymap: load(custom_path)?,
        state: ResolveState::default(),
    })))
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
    let Router { keymap, state, .. } = &mut *router;
    if state.deadline().is_some_and(|deadline| now >= deadline) {
        cosmix_actions::resolve_timeout(&FocusContext::global(), keymap, state, now).actions
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

/// Wraps the whole window content; sees every key before its children.
/// Resolved actions are published; everything else reaches the children.
pub struct KeyRouter<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    shared: SharedRouter,
    on_actions: ActionsFn<'a, Message>,
}

pub fn router<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    shared: SharedRouter,
    on_actions: impl Fn(Vec<ActionId>) -> Message + 'a,
) -> KeyRouter<'a, Message, Theme, Renderer> {
    KeyRouter { content: content.into(), shared, on_actions: Box::new(on_actions) }
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
        if let Event::Keyboard(keyboard_event) = event
            && let Some(input) = key_input(keyboard_event)
        {
            let resolved = {
                let mut router = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = tick();
                // Split-borrow the router's own fields so the resolver can
                // hold the keymap while mutating the chord state.
                let Router { keymap, state, .. } = &mut *router;
                let mut resolved = resolve(input, &FocusContext::global(), keymap, state, now);
                // A chord that was waiting for a second stroke expired while
                // nothing was pressed: resolve it opportunistically here (P1
                // defaults have no multi-stroke chords; users can add them).
                if resolved.actions.is_empty()
                    && let Some(deadline) = state.deadline()
                    && now >= deadline
                {
                    let late = cosmix_actions::resolve_timeout(
                        &FocusContext::global(),
                        keymap,
                        state,
                        now,
                    );
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
        let (mods, name) = match text.split_once('+') {
            Some(("Ctrl", name)) => (keyboard::Modifiers::CTRL, name),
            Some(("Alt", name)) => (keyboard::Modifiers::ALT, name),
            Some(("Shift", name)) => (keyboard::Modifiers::SHIFT, name),
            _ => (keyboard::Modifiers::empty(), text),
        };
        let (key, physical) = match name {
            "F5" => (Key::Named(Named::F5), Physical::Unidentified(NativeCode::Unidentified)),
            "ArrowDown" => (Key::Named(Named::ArrowDown), Physical::Unidentified(NativeCode::Unidentified)),
            "ArrowUp" => (Key::Named(Named::ArrowUp), Physical::Unidentified(NativeCode::Unidentified)),
            "Enter" => (Key::Named(Named::Enter), Physical::Unidentified(NativeCode::Unidentified)),
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
