//! The protocol half of keyboard and IME for in-process content.
//!
//! Keys reach content the compositor draws itself only after the same seat
//! path a client's keys take: the binding filter first, then this. Focus is
//! decided in `arbitrate_keyboard_focus` — in-process content is one more
//! requester, below the structural gates (session lock, exclusive layer,
//! override-redirect) — so a scene can never take the keyboard from the
//! lock screen.

use smithay::input::keyboard::{KeysymHandle, ModifiersState, xkb};
use smithay::reexports::calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{
    ContentHint, ContentPurpose,
};
use smithay::wayland::input_method::{InputMethodSeat, InputMethodSinkEvent};

use super::*;
use crate::native_input::{
    NativeImeBridge, NativeImeEvent, NativeImeRequest, NativeKeyEvent, NativeKeyboardBridge,
    NativeModifiers,
};

/// The seat's own repeat settings (`seat.add_keyboard`): a client gets these
/// as `wl_keyboard.repeat_info` and repeats for itself. In-process content
/// has no such protocol, so the compositor repeats for it on the same terms.
const REPEAT_DELAY: Duration = Duration::from_millis(500);
const REPEAT_INTERVAL: Duration = Duration::from_micros(1_000_000 / 30);

/// What is being repeated for in-process content, and the timer doing it.
pub(crate) struct NativeKeyRepeat {
    event: NativeKeyEvent,
    keycode: Keycode,
    timer: RegistrationToken,
}

#[derive(Default)]
pub(crate) struct NativeInputState {
    pub(crate) keyboard: Option<NativeKeyboardBridge>,
    pub(crate) ime: Option<NativeImeBridge>,
    repeat: Option<NativeKeyRepeat>,
}

impl WaylandState {
    /// The render thread registered its bridges: keys and IME can flow.
    pub(crate) fn install_native_input(
        &mut self,
        keyboard: NativeKeyboardBridge,
        ime: NativeImeBridge,
    ) {
        self.native_input.keyboard = Some(keyboard);
        self.native_input.ime = Some(ime.clone());
        // The vendored sink: input-method output goes to in-process content
        // only while no focused client has an active text input.
        self.seat
            .input_method()
            .set_sink(Some(std::sync::Arc::new(move |event| {
                ime.push(match event {
                    InputMethodSinkEvent::CommitString(text) => NativeImeEvent::Commit(text),
                    InputMethodSinkEvent::PreeditString {
                        text,
                        cursor_begin,
                        cursor_end,
                    } => NativeImeEvent::Preedit {
                        text,
                        cursor_begin,
                        cursor_end,
                    },
                    InputMethodSinkEvent::DeleteSurroundingText {
                        before_length,
                        after_length,
                    } => NativeImeEvent::DeleteSurrounding {
                        before: before_length,
                        after: after_length,
                    },
                    InputMethodSinkEvent::Done { discard_state } => NativeImeEvent::Done {
                        discard: discard_state,
                    },
                });
            })));
    }

    /// Whether in-process content is asking for the keyboard. Read inside
    /// arbitration, after the structural gates.
    pub(crate) fn native_keyboard_wants_focus(&self) -> bool {
        self.native_input
            .keyboard
            .as_ref()
            .is_some_and(NativeKeyboardBridge::wants_focus)
    }

    /// Record what arbitration decided; losing focus also ends a repeat.
    pub(crate) fn set_native_keyboard_focus(&mut self, focused: bool) {
        let changed = self
            .native_input
            .keyboard
            .as_ref()
            .is_some_and(|bridge| bridge.focused() != focused);
        if let Some(bridge) = self.native_input.keyboard.as_ref() {
            bridge.set_focused(focused);
        }
        if changed && !focused {
            self.cancel_native_key_repeat();
        }
    }

    pub(crate) fn native_keyboard_focused(&self) -> bool {
        self.native_input
            .keyboard
            .as_ref()
            .is_some_and(NativeKeyboardBridge::focused)
    }

    /// The content's focus request changed: arbitrate again, with the
    /// highest visible toplevel as the fallback when it gave the keyboard
    /// back.
    pub(crate) fn service_native_focus_request(&mut self) {
        self.arbitrate_keyboard_focus(None, true, false);
    }

    /// One key the binding filter did not take, while in-process content
    /// owns the keyboard. Returns whether it was delivered.
    pub(crate) fn deliver_native_key(
        &mut self,
        keycode: Keycode,
        handle: &KeysymHandle<'_>,
        modifiers: &ModifiersState,
        pressed: bool,
    ) -> bool {
        if !self.native_keyboard_focused() {
            return false;
        }
        let event = NativeKeyEvent {
            evdev: keycode.raw().saturating_sub(8),
            keysym: handle.modified_sym().raw(),
            text: key_text(handle),
            pressed,
            repeat: false,
            modifiers: NativeModifiers::from(modifiers),
        };
        let repeats = pressed && key_repeats(handle, keycode);
        if let Some(bridge) = self.native_input.keyboard.as_ref() {
            bridge.push(event.clone());
        }
        // One key repeats at a time, exactly as a client would do it from
        // `repeat_info`: a new press replaces the old, a release of the
        // repeating key ends it.
        let repeating = self
            .native_input
            .repeat
            .as_ref()
            .is_some_and(|repeat| repeat.keycode == keycode);
        if !pressed && !repeating {
            return true;
        }
        self.cancel_native_key_repeat();
        if repeats {
            self.arm_native_key_repeat(keycode, event, REPEAT_DELAY);
        }
        true
    }

    fn arm_native_key_repeat(&mut self, keycode: Keycode, event: NativeKeyEvent, delay: Duration) {
        let armed = self
            .capture_loop_handle
            .insert_source(Timer::from_duration(delay), move |_, _, state| {
                state.repeat_native_key();
                TimeoutAction::ToDuration(REPEAT_INTERVAL)
            });
        match armed {
            Ok(timer) => {
                self.native_input.repeat = Some(NativeKeyRepeat {
                    event: NativeKeyEvent {
                        repeat: true,
                        ..event
                    },
                    keycode,
                    timer,
                });
            }
            Err(error) => tracing::warn!(%error, "native key repeat timer unavailable"),
        }
    }

    fn repeat_native_key(&mut self) {
        let Some(repeat) = self.native_input.repeat.as_ref() else {
            return;
        };
        // A repeat the content can no longer receive, or one the seat no
        // longer has pressed, stops here rather than running forever.
        if !self.native_keyboard_focused() || !self.keyboard.pressed_keys().contains(&repeat.keycode)
        {
            self.cancel_native_key_repeat();
            return;
        }
        let event = repeat.event.clone();
        if let Some(bridge) = self.native_input.keyboard.as_ref() {
            bridge.push(event);
        }
    }

    pub(crate) fn cancel_native_key_repeat(&mut self) {
        if let Some(repeat) = self.native_input.repeat.take() {
            self.capture_loop_handle.remove(repeat.timer);
        }
    }

    /// The content's own text-input state, on its way to the input method.
    pub(crate) fn service_native_ime_request(&mut self, request: NativeImeRequest) {
        if let Some(bridge) = self.native_input.ime.as_ref() {
            bridge.note_request(&request);
        }
        let input_method = self.seat.input_method().clone();
        match request {
            NativeImeRequest::Enable(true) => input_method.activate_for_sink(self),
            NativeImeRequest::Enable(false) => input_method.deactivate_input_method(self),
            NativeImeRequest::Caret {
                x,
                y,
                width,
                height,
            } => {
                let rect = Rectangle::new((x, y).into(), (width, height).into());
                input_method.set_text_input_rectangle::<WaylandState>(self, rect);
            }
            NativeImeRequest::SurroundingText {
                text,
                cursor,
                anchor,
            } => input_method.with_active_instance(|instance| {
                instance.surrounding_text(text, cursor, anchor);
            }),
            NativeImeRequest::ContentType { hint, purpose } => {
                input_method.with_active_instance(|instance| {
                    instance.content_type(
                        ContentHint::from_bits_truncate(hint),
                        ContentPurpose::try_from(purpose).unwrap_or(ContentPurpose::Normal),
                    );
                });
            }
            NativeImeRequest::Done => input_method.send_done(),
        }
    }

    /// The caret rectangle in-process content last reported, in
    /// output-space logical pixels.
    pub(crate) fn native_ime_caret(&self) -> Option<Rectangle<i32, Logical>> {
        let (x, y, width, height) = self.native_input.ime.as_ref()?.caret()?;
        Some(Rectangle::new((x, y).into(), (width, height).into()))
    }
}

/// The text a key produces, empty and control characters excluded.
fn key_text(handle: &KeysymHandle<'_>) -> Option<String> {
    let text = xkb::keysym_to_utf8(handle.modified_sym());
    let text = text.trim_end_matches('\0').to_string();
    (!text.is_empty()).then_some(text)
}

/// Whether the keymap says this key repeats (a modifier does not).
fn key_repeats(handle: &KeysymHandle<'_>, keycode: Keycode) -> bool {
    let xkb = handle
        .xkb()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: the keymap reference does not outlive this lock guard.
    unsafe { xkb.keymap() }.key_repeats(keycode)
}
