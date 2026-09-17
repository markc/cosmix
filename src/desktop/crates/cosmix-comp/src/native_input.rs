//! Keyboard and IME for content the compositor draws itself.
//!
//! A Wayland client gets keys through the seat and IME through
//! `zwp_text_input_v3`. In-process content (a scene mounted in comp's own
//! Bevy app) has no client and no surface, so both paths end at the
//! compositor. This module is the two bridges that close them:
//!
//! - [`NativeKeyboardBridge`] carries seat keys, already past the binding
//!   filter, into the Bevy world as `KeyboardInput` (repeats included).
//! - [`NativeImeBridge`] carries input-method output in (through the
//!   vendored sink, see `vendor/README.md`) and the field's own state out
//!   (enable, caret rectangle, surrounding text, content type).
//!
//! Both are `Arc<Mutex<..>>` handles like [`crate::native_shell`], because
//! the protocol thread writes them and the render thread reads them.
//!
//! The registration API is the Bevy side: add [`NativeInputPlugin`], then
//! ask for the keyboard with [`NativeKeyboard::request_focus`] and drive the
//! IME through [`NativeIme`]. Focus is arbitrated by the compositor, so the
//! request is a request: a session lock, an exclusive layer or an
//! input-method grab still wins.

use bevy::input::{
    ButtonState,
    keyboard::{Key, KeyCode, KeyboardInput, NativeKey, NativeKeyCode},
};
use bevy::prelude::*;
use smithay::input::keyboard::{ModifiersState, xkb};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

/// The most key events kept for the render thread; a scene that stops
/// draining (a stalled frame) drops the oldest rather than growing.
const MAX_PENDING_KEYS: usize = 256;
/// The most IME events kept for the render thread.
const MAX_PENDING_IME: usize = 64;

/// One seat key, as the compositor resolved it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NativeKeyEvent {
    /// evdev code (the XKB keycode minus 8).
    pub(crate) evdev: u32,
    /// The keysym after the layout and modifiers.
    pub(crate) keysym: u32,
    /// The text this key produces, if any.
    pub(crate) text: Option<String>,
    pub(crate) pressed: bool,
    /// A compositor-generated repeat rather than a device event.
    pub(crate) repeat: bool,
    pub(crate) modifiers: NativeModifiers,
}

/// The modifier state at the moment of a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NativeModifiers {
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
    pub(crate) logo: bool,
    pub(crate) caps_lock: bool,
    pub(crate) num_lock: bool,
}

impl From<&ModifiersState> for NativeModifiers {
    fn from(modifiers: &ModifiersState) -> Self {
        Self {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            logo: modifiers.logo,
            caps_lock: modifiers.caps_lock,
            num_lock: modifiers.num_lock,
        }
    }
}

#[derive(Default)]
struct KeyboardState {
    /// The content asked for the keyboard.
    wanted: bool,
    /// Arbitration gave it the keyboard.
    focused: bool,
    events: VecDeque<NativeKeyEvent>,
    dropped: u64,
}

/// Seat keys for in-process content. The protocol thread pushes; the
/// render thread drains.
#[derive(Resource, Clone, Default)]
pub(crate) struct NativeKeyboardBridge(Arc<Mutex<KeyboardState>>);

impl NativeKeyboardBridge {
    fn state(&self) -> std::sync::MutexGuard<'_, KeyboardState> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The content's standing request for the keyboard.
    pub(crate) fn wants_focus(&self) -> bool {
        self.state().wanted
    }

    pub(crate) fn set_wants_focus(&self, wanted: bool) {
        self.state().wanted = wanted;
    }

    /// What arbitration decided. Keys are only pushed while this is set.
    pub(crate) fn focused(&self) -> bool {
        self.state().focused
    }

    pub(crate) fn set_focused(&self, focused: bool) {
        let mut state = self.state();
        if state.focused != focused {
            state.focused = focused;
            // Nothing is "still held" across a focus change: the content
            // hears the keys it has, then the focus edge.
            state.events.clear();
        }
    }

    pub(crate) fn push(&self, event: NativeKeyEvent) {
        let mut state = self.state();
        if !state.focused {
            return;
        }
        if state.events.len() >= MAX_PENDING_KEYS {
            state.events.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.events.push_back(event);
    }

    /// What the render thread would receive this frame.
    #[cfg(test)]
    pub(crate) fn drain_for_test(&self) -> Vec<NativeKeyEvent> {
        self.drain().0
    }

    fn drain(&self) -> (Vec<NativeKeyEvent>, u64, bool) {
        let mut state = self.state();
        let dropped = std::mem::take(&mut state.dropped);
        let focused = state.focused;
        (state.events.drain(..).collect(), dropped, focused)
    }
}

/// What the input method produced for in-process content, in the shape the
/// vendored sink hands over.
#[derive(Clone, Debug, PartialEq, Message)]
pub(crate) enum NativeImeEvent {
    /// Insert this text at the cursor.
    Commit(String),
    /// Replace the composing text.
    Preedit {
        text: String,
        cursor_begin: i32,
        cursor_end: i32,
    },
    /// Delete around the cursor, in bytes.
    DeleteSurrounding { before: u32, after: u32 },
    /// End of a batch: apply it, or discard it.
    Done { discard: bool },
}

/// What in-process content asks of the input method.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeImeRequest {
    /// The field took or gave up the input method.
    Enable(bool),
    /// The caret rectangle, in the content's output-space logical pixels.
    /// The compositor anchors the candidate popup under it.
    Caret {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// `zwp_text_input_v3.set_surrounding_text`.
    SurroundingText {
        text: String,
        cursor: u32,
        anchor: u32,
    },
    /// `zwp_text_input_v3.set_content_type` (hint and purpose as the
    /// protocol numbers them).
    ContentType { hint: u32, purpose: u32 },
    /// End the batch of requests above.
    Done,
}

#[derive(Default)]
struct ImeState {
    events: VecDeque<NativeImeEvent>,
    dropped: u64,
    /// The latest caret rectangle the content reported, for the popup
    /// anchor fallback.
    caret: Option<(i32, i32, i32, i32)>,
    enabled: bool,
}

/// Input-method traffic for in-process content.
#[derive(Resource, Clone, Default)]
pub(crate) struct NativeImeBridge(Arc<Mutex<ImeState>>);

impl NativeImeBridge {
    fn state(&self) -> std::sync::MutexGuard<'_, ImeState> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The vendored sink's entry point (called on the protocol thread).
    pub(crate) fn push(&self, event: NativeImeEvent) {
        let mut state = self.state();
        if state.events.len() >= MAX_PENDING_IME {
            state.events.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.events.push_back(event);
    }

    #[cfg(test)]
    pub(crate) fn drain_for_test(&self) -> Vec<NativeImeEvent> {
        self.drain().0
    }

    fn drain(&self) -> (Vec<NativeImeEvent>, u64) {
        let mut state = self.state();
        let dropped = std::mem::take(&mut state.dropped);
        (state.events.drain(..).collect(), dropped)
    }

    /// The caret rectangle the content last reported (output-space logical
    /// pixels); the popup anchor falls back to it when the caret owner has
    /// no client surface.
    pub(crate) fn caret(&self) -> Option<(i32, i32, i32, i32)> {
        self.state().caret
    }

    pub(crate) fn note_request(&self, request: &NativeImeRequest) {
        let mut state = self.state();
        match *request {
            NativeImeRequest::Enable(enabled) => {
                state.enabled = enabled;
                if !enabled {
                    state.caret = None;
                }
            }
            NativeImeRequest::Caret {
                x,
                y,
                width,
                height,
            } => state.caret = Some((x, y, width, height)),
            _ => {}
        }
    }

    /// Whether the content currently claims the input method.
    pub(crate) fn enabled(&self) -> bool {
        self.state().enabled
    }
}

/// Registration handle for the scene: ask for the keyboard, read what
/// arbitration decided.
#[derive(Resource, Clone)]
pub(crate) struct NativeKeyboard {
    bridge: NativeKeyboardBridge,
    feed: crate::protocol::ClientSceneFeedHandle,
}

impl NativeKeyboard {
    /// Ask for (or give up) the keyboard. The compositor arbitrates: a
    /// session lock, an exclusive layer surface or an input-method grab
    /// still takes priority, and [`Self::focused`] reports the outcome.
    pub(crate) fn request_focus(&self, wanted: bool) {
        if self.bridge.wants_focus() == wanted {
            return;
        }
        self.bridge.set_wants_focus(wanted);
        self.feed.native_focus_changed();
    }

    /// Whether in-process content owns the keyboard right now.
    pub(crate) fn focused(&self) -> bool {
        self.bridge.focused()
    }
}

/// Registration handle for the scene's text field.
#[derive(Resource, Clone)]
pub(crate) struct NativeIme {
    bridge: NativeImeBridge,
    feed: crate::protocol::ClientSceneFeedHandle,
}

impl NativeIme {
    /// Whether this field currently claims the input method.
    pub(crate) fn enabled(&self) -> bool {
        self.bridge.enabled()
    }

    /// Take or give up the input method (`zwp_text_input_v3.enable` for a
    /// client).
    pub(crate) fn enable(&self, enabled: bool) {
        self.request(NativeImeRequest::Enable(enabled));
    }

    /// Report the caret rectangle in output-space logical pixels.
    pub(crate) fn set_caret(&self, x: i32, y: i32, width: i32, height: i32) {
        self.request(NativeImeRequest::Caret {
            x,
            y,
            width,
            height,
        });
    }

    pub(crate) fn set_surrounding_text(&self, text: String, cursor: u32, anchor: u32) {
        self.request(NativeImeRequest::SurroundingText {
            text,
            cursor,
            anchor,
        });
    }

    pub(crate) fn set_content_type(&self, hint: u32, purpose: u32) {
        self.request(NativeImeRequest::ContentType { hint, purpose });
    }

    /// End a batch of the requests above (`commit` for a client).
    pub(crate) fn done(&self) {
        self.request(NativeImeRequest::Done);
    }

    fn request(&self, request: NativeImeRequest) {
        self.bridge.note_request(&request);
        self.feed.native_ime_request(request);
    }
}

/// Keys and IME for in-process content. Install it beside the scene's own
/// plugin; it does nothing until the content asks for focus.
pub(crate) struct NativeInputPlugin;

impl Plugin for NativeInputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NativeKeyboardBridge>()
            .init_resource::<NativeImeBridge>()
            .add_message::<NativeImeEvent>()
            .add_systems(Startup, attach_protocol)
            .add_systems(PreUpdate, (native_keys, native_ime).chain());
    }
}

/// An exclusive system, so the registration handles exist for the rest of
/// Startup rather than after the schedule's command flush.
fn attach_protocol(world: &mut World) {
    let feed = world.resource::<crate::protocol::ClientSceneFeed>();
    let handle = feed.handle();
    let keyboard = world.resource::<NativeKeyboardBridge>().clone();
    let ime = world.resource::<NativeImeBridge>().clone();
    world
        .resource::<crate::protocol::ClientSceneFeed>()
        .install_native_input(keyboard.clone(), ime.clone());
    world.insert_resource(NativeKeyboard {
        bridge: keyboard,
        feed: handle.clone(),
    });
    world.insert_resource(NativeIme {
        bridge: ime,
        feed: handle,
    });
}

fn native_keys(
    bridge: Res<NativeKeyboardBridge>,
    windows: Query<Entity, With<Window>>,
    mut keys: MessageWriter<KeyboardInput>,
) {
    let (events, dropped, focused) = bridge.drain();
    if dropped > 0 {
        warn!(dropped, "native keyboard events dropped before the frame");
    }
    if !focused || events.is_empty() {
        return;
    }
    // In-process content has no window of its own; the compositor's own
    // window (nested) or the placeholder entity (kms) is the only target.
    let window = windows.iter().next().unwrap_or(Entity::PLACEHOLDER);
    for event in events {
        keys.write(keyboard_input(&event, window));
    }
}

fn native_ime(bridge: Res<NativeImeBridge>, mut events: MessageWriter<NativeImeEvent>) {
    let (drained, dropped) = bridge.drain();
    if dropped > 0 {
        warn!(dropped, "native IME events dropped before the frame");
    }
    for event in drained {
        events.write(event);
    }
}

/// One seat key as Bevy sees it.
pub(crate) fn keyboard_input(event: &NativeKeyEvent, window: Entity) -> KeyboardInput {
    KeyboardInput {
        key_code: key_code(event.evdev),
        logical_key: logical_key(event.keysym, event.text.as_deref()),
        state: if event.pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        text: event
            .text
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(Into::into),
        repeat: event.repeat,
        window,
    }
}

/// The logical key: the text it produced, else the named key, else the
/// keysym so a binding UI can still identify it.
fn logical_key(keysym: u32, text: Option<&str>) -> Key {
    if let Some(named) = named_key(keysym) {
        return named;
    }
    match text.filter(|text| !text.is_empty() && !text.chars().any(char::is_control)) {
        Some(text) => Key::Character(text.into()),
        None => Key::Unidentified(NativeKey::Xkb(keysym)),
    }
}

fn named_key(keysym: u32) -> Option<Key> {
    use xkb::keysyms as sym;
    Some(match keysym {
        sym::KEY_Return | sym::KEY_KP_Enter => Key::Enter,
        sym::KEY_BackSpace => Key::Backspace,
        sym::KEY_Tab | sym::KEY_ISO_Left_Tab => Key::Tab,
        sym::KEY_Escape => Key::Escape,
        sym::KEY_Delete | sym::KEY_KP_Delete => Key::Delete,
        sym::KEY_Insert => Key::Insert,
        sym::KEY_Home | sym::KEY_KP_Home => Key::Home,
        sym::KEY_End | sym::KEY_KP_End => Key::End,
        sym::KEY_Page_Up | sym::KEY_KP_Page_Up => Key::PageUp,
        sym::KEY_Page_Down | sym::KEY_KP_Page_Down => Key::PageDown,
        sym::KEY_Left | sym::KEY_KP_Left => Key::ArrowLeft,
        sym::KEY_Right | sym::KEY_KP_Right => Key::ArrowRight,
        sym::KEY_Up | sym::KEY_KP_Up => Key::ArrowUp,
        sym::KEY_Down | sym::KEY_KP_Down => Key::ArrowDown,
        sym::KEY_Shift_L | sym::KEY_Shift_R => Key::Shift,
        sym::KEY_Control_L | sym::KEY_Control_R => Key::Control,
        sym::KEY_Alt_L | sym::KEY_Alt_R => Key::Alt,
        sym::KEY_ISO_Level3_Shift => Key::AltGraph,
        sym::KEY_Super_L | sym::KEY_Super_R => Key::Super,
        sym::KEY_Caps_Lock => Key::CapsLock,
        sym::KEY_Num_Lock => Key::NumLock,
        sym::KEY_F1 => Key::F1,
        sym::KEY_F2 => Key::F2,
        sym::KEY_F3 => Key::F3,
        sym::KEY_F4 => Key::F4,
        sym::KEY_F5 => Key::F5,
        sym::KEY_F6 => Key::F6,
        sym::KEY_F7 => Key::F7,
        sym::KEY_F8 => Key::F8,
        sym::KEY_F9 => Key::F9,
        sym::KEY_F10 => Key::F10,
        sym::KEY_F11 => Key::F11,
        sym::KEY_F12 => Key::F12,
        _ => return None,
    })
}

/// The physical key. This is the inverse of the nested backend's
/// `evdev_keycode`, and `native_key_codes_round_trip` holds them together.
pub(crate) fn key_code(evdev: u32) -> KeyCode {
    use KeyCode::*;
    match evdev {
        1 => Escape,
        2 => Digit1,
        3 => Digit2,
        4 => Digit3,
        5 => Digit4,
        6 => Digit5,
        7 => Digit6,
        8 => Digit7,
        9 => Digit8,
        10 => Digit9,
        11 => Digit0,
        12 => Minus,
        13 => Equal,
        14 => Backspace,
        15 => Tab,
        16 => KeyQ,
        17 => KeyW,
        18 => KeyE,
        19 => KeyR,
        20 => KeyT,
        21 => KeyY,
        22 => KeyU,
        23 => KeyI,
        24 => KeyO,
        25 => KeyP,
        26 => BracketLeft,
        27 => BracketRight,
        28 => Enter,
        29 => ControlLeft,
        30 => KeyA,
        31 => KeyS,
        32 => KeyD,
        33 => KeyF,
        34 => KeyG,
        35 => KeyH,
        36 => KeyJ,
        37 => KeyK,
        38 => KeyL,
        39 => Semicolon,
        40 => Quote,
        41 => Backquote,
        42 => ShiftLeft,
        43 => Backslash,
        44 => KeyZ,
        45 => KeyX,
        46 => KeyC,
        47 => KeyV,
        48 => KeyB,
        49 => KeyN,
        50 => KeyM,
        51 => Comma,
        52 => Period,
        53 => Slash,
        54 => ShiftRight,
        55 => NumpadMultiply,
        56 => AltLeft,
        57 => Space,
        58 => CapsLock,
        59 => F1,
        60 => F2,
        61 => F3,
        62 => F4,
        63 => F5,
        64 => F6,
        65 => F7,
        66 => F8,
        67 => F9,
        68 => F10,
        69 => NumLock,
        70 => ScrollLock,
        71 => Numpad7,
        72 => Numpad8,
        73 => Numpad9,
        74 => NumpadSubtract,
        75 => Numpad4,
        76 => Numpad5,
        77 => Numpad6,
        78 => NumpadAdd,
        79 => Numpad1,
        80 => Numpad2,
        81 => Numpad3,
        82 => Numpad0,
        83 => NumpadDecimal,
        87 => F11,
        88 => F12,
        96 => NumpadEnter,
        97 => ControlRight,
        98 => NumpadDivide,
        100 => AltRight,
        102 => Home,
        103 => ArrowUp,
        104 => PageUp,
        105 => ArrowLeft,
        106 => ArrowRight,
        107 => End,
        108 => ArrowDown,
        109 => PageDown,
        110 => Insert,
        111 => Delete,
        119 => Pause,
        125 => SuperLeft,
        126 => SuperRight,
        127 => ContextMenu,
        // Everything else keeps its XKB keycode, which is what the nested
        // backend's mapping accepts back.
        other => Unidentified(NativeKeyCode::Xkb(other + 8)),
    }
}


/// A test-only text field drawn by the compositor itself.
///
/// It is the gate client for this module: no Wayland client can prove that
/// in-process content receives keys and IME, because a client has both by
/// definition. `COSMIX_COMP_NATIVE_INPUT_PROBE=1` installs it; it takes the
/// keyboard, enables the input method, reports a caret, and prints one
/// `NATIVE_INPUT_PROBE` line whenever its state changes.
#[derive(Resource, Default)]
struct NativeProbeField {
    text: String,
    preedit: String,
    focused: bool,
    commits: u32,
}

/// Install the bridges, and the probe field when its variable is set.
pub(crate) fn install_from_environment(app: &mut App) {
    if std::env::var("COSMIX_COMP_NATIVE_INPUT_PROBE").as_deref() != Ok("1") {
        return;
    }
    app.add_plugins(NativeInputPlugin)
        .init_resource::<NativeProbeField>()
        .add_systems(Startup, probe_take_focus.after(attach_protocol))
        .add_systems(Update, probe_edit.after(native_keys));
}

fn probe_take_focus(keyboard: Res<NativeKeyboard>, ime: Res<NativeIme>) {
    keyboard.request_focus(true);
    ime.enable(true);
    // A caret an input method can place its candidate window under.
    ime.set_caret(40, 80, 2, 18);
    ime.done();
    info!("NATIVE_INPUT_PROBE requested focus and enabled the input method");
}

fn probe_edit(
    mut field: ResMut<NativeProbeField>,
    keyboard: Res<NativeKeyboard>,
    ime_field: Res<NativeIme>,
    mut keys: MessageReader<KeyboardInput>,
    mut ime: MessageReader<NativeImeEvent>,
) {
    let mut changed = false;
    let focused = keyboard.focused();
    if field.focused != focused {
        field.focused = focused;
        changed = true;
    }
    for key in keys.read() {
        if key.state != ButtonState::Pressed {
            continue;
        }
        match &key.logical_key {
            Key::Backspace => {
                field.text.pop();
                changed = true;
            }
            Key::Character(text) => {
                field.text.push_str(text);
                changed = true;
            }
            _ => {}
        }
    }
    for event in ime.read() {
        match event {
            NativeImeEvent::Commit(text) => {
                let text = text.clone();
                field.text.push_str(&text);
                field.commits += 1;
                changed = true;
            }
            NativeImeEvent::Preedit { text, .. } => {
                field.preedit.clone_from(text);
                changed = true;
            }
            NativeImeEvent::DeleteSurrounding { before, .. } => {
                let keep = field.text.len().saturating_sub(*before as usize);
                field.text.truncate(keep);
                changed = true;
            }
            NativeImeEvent::Done { discard } => {
                if *discard {
                    field.preedit.clear();
                }
                changed = true;
            }
        }
    }
    if changed {
        // What a real field reports back to the input method after every
        // edit: the text around the cursor, what kind of field it is, and
        // where the caret now sits.
        // Re-enable on every edit: an input method that connects after the
        // field did would otherwise never hear the activate.
        ime_field.enable(true);
        let cursor = field.text.len() as u32;
        ime_field.set_surrounding_text(field.text.clone(), cursor, cursor);
        ime_field.set_content_type(0, 0);
        ime_field.set_caret(40, 80, 2, 18);
        ime_field.done();
        info!(
            "NATIVE_INPUT_PROBE focused={} text={:?} preedit={:?} commits={} ime_enabled={}",
            field.focused,
            field.text,
            field.preedit,
            field.commits,
            ime_field.enabled(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The physical mapping is the inverse of the nested backend's, so a
    /// key injected through the seat is the key the scene sees.
    #[test]
    fn native_key_codes_round_trip() {
        for evdev in 1..=127_u32 {
            let code = key_code(evdev);
            assert_eq!(
                crate::evdev_keycode(code),
                Some(evdev),
                "evdev {evdev} maps to {code:?}, which maps back elsewhere"
            );
        }
    }

    #[test]
    fn logical_keys_prefer_named_then_text() {
        assert_eq!(logical_key(xkb::keysyms::KEY_Return, Some("\r")), Key::Enter);
        assert_eq!(
            logical_key(xkb::keysyms::KEY_a, Some("a")),
            Key::Character("a".into())
        );
        assert_eq!(
            logical_key(xkb::keysyms::KEY_XF86AudioPlay, None),
            Key::Unidentified(NativeKey::Xkb(xkb::keysyms::KEY_XF86AudioPlay))
        );
        // A control character is not text.
        assert_eq!(
            logical_key(0x1234_5678, Some("\u{1}")),
            Key::Unidentified(NativeKey::Xkb(0x1234_5678))
        );
    }

    #[test]
    fn keys_are_only_kept_while_focused_and_are_bounded() {
        let bridge = NativeKeyboardBridge::default();
        let event = NativeKeyEvent {
            evdev: 30,
            keysym: xkb::keysyms::KEY_a,
            text: Some("a".into()),
            pressed: true,
            repeat: false,
            modifiers: NativeModifiers::default(),
        };
        bridge.push(event.clone());
        assert!(bridge.drain().0.is_empty(), "unfocused content hears nothing");

        bridge.set_focused(true);
        for _ in 0..MAX_PENDING_KEYS + 10 {
            bridge.push(event.clone());
        }
        let (events, dropped, focused) = bridge.drain();
        assert!(focused);
        assert_eq!(events.len(), MAX_PENDING_KEYS);
        assert_eq!(dropped, 10);

        bridge.push(event.clone());
        bridge.set_focused(false);
        assert!(
            bridge.drain().0.is_empty(),
            "losing focus drops what was never delivered"
        );
    }

    #[test]
    fn ime_requests_track_the_caret_and_enable_state() {
        let bridge = NativeImeBridge::default();
        assert!(!bridge.enabled());
        assert_eq!(bridge.caret(), None);
        bridge.note_request(&NativeImeRequest::Enable(true));
        bridge.note_request(&NativeImeRequest::Caret {
            x: 10,
            y: 20,
            width: 2,
            height: 16,
        });
        assert!(bridge.enabled());
        assert_eq!(bridge.caret(), Some((10, 20, 2, 16)));
        bridge.note_request(&NativeImeRequest::Enable(false));
        assert!(!bridge.enabled());
        assert_eq!(bridge.caret(), None, "a disabled field has no caret");
    }

    #[test]
    fn ime_events_are_bounded_and_drained_in_order() {
        let bridge = NativeImeBridge::default();
        for index in 0..MAX_PENDING_IME + 3 {
            bridge.push(NativeImeEvent::Commit(index.to_string()));
        }
        let (events, dropped) = bridge.drain();
        assert_eq!(dropped, 3);
        assert_eq!(events.len(), MAX_PENDING_IME);
        assert_eq!(events[0], NativeImeEvent::Commit("3".into()));
        assert!(bridge.drain().0.is_empty());
    }
}
