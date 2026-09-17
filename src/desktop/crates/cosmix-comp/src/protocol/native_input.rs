//! The protocol half of keyboard and IME for in-process content.
//!
//! Keys reach content the compositor draws itself only after the same seat
//! path a client's keys take: the input-method keyboard grab first, then the
//! binding filter, then this. Focus is decided in
//! `arbitrate_keyboard_focus` — in-process content is the LAST requester,
//! below the session lock, the exclusive layers, the override-redirect gate,
//! non-interactive layers and any explicitly requested client surface
//! (`comp.window.focus`, xdg-activation, a click) — so a scene can never
//! take the keyboard from the lock screen, and a client asked for by name
//! takes it back from the scene.

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
    NativeImeEvent, NativeImeRequest, NativeInputBridge, NativeKeyEvent, NativeModifiers,
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
    pub(crate) bridge: Option<NativeInputBridge>,
    repeat: Option<NativeKeyRepeat>,
    /// The input-method instance this field is already activated for, so
    /// `enable(true)` on every edit does not re-activate (which resets the
    /// instance serial and mismatches a batch in flight), while an input
    /// method that connects LATER — or reconnects — still gets its activate.
    activated_for: Option<u64>,
    /// An activation owed to the field once arbitration has actually moved
    /// the seat focus. Set inside `set_native_keyboard_focus`, which runs
    /// BEFORE `keyboard.set_focus`, when a client's text input still reads
    /// as active and the activation would refuse itself.
    refresh_pending: bool,
}

impl WaylandState {
    /// The render thread registered its bridge: keys and IME can flow.
    ///
    /// SINGLE OWNER. One seat has one keyboard and one input method, so a
    /// second install is refused rather than silently replacing the first
    /// (whose bridge would then be fed by nothing while its consumer still
    /// believed it held the keyboard). An owner that is going away calls
    /// [`Self::uninstall_native_input`] first.
    pub(crate) fn install_native_input(&mut self, bridge: NativeInputBridge) {
        if self.native_input.bridge.is_some() {
            tracing::error!(
                "native input already has an owner; refusing the second install \
                 (the previous owner must uninstall first)"
            );
            return;
        }
        bridge.set_installed(true);
        self.native_input.bridge = Some(bridge.clone());
        // The vendored sink: input-method output goes to in-process content
        // only while no focused client has an active text input AND the
        // content itself has the input method enabled. Without the second
        // half a scene that never enabled would still swallow what the
        // input method produced for nobody.
        self.seat
            .input_method()
            .set_sink(Some(std::sync::Arc::new(move |event| {
                if !bridge.enabled() {
                    return;
                }
                bridge.push_ime(match event {
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

    /// The owner is going away: stop feeding it, unregister the sink, and
    /// give the keyboard back to whichever client should have it.
    pub(crate) fn uninstall_native_input(&mut self) {
        let Some(bridge) = self.native_input.bridge.take() else {
            return;
        };
        self.cancel_native_key_repeat();
        let input_method = self.seat.input_method().clone();
        if self.native_input.activated_for.take().is_some() {
            input_method.deactivate_input_method(self);
        }
        // A popup the input method created before any field activated has
        // no parent and belongs to nobody; unregistering the sink below
        // would leave it on screen with nothing able to dismiss it.
        input_method.dismiss_parentless_popup::<WaylandState>(self);
        // Unregister the sink, or input-method output would keep being
        // swallowed by a bridge nobody drains.
        input_method.set_sink(None);
        // The departing owner hears the edge (it may already have emitted
        // it itself, synchronously, in `NativeKeyboard::uninstall`), and
        // the bridge's field state is cleared so a re-install cannot start
        // with a stale standing request or caret. Clearing does not bump
        // the generation, so the edge survives to the next drain.
        bridge.set_focused(false);
        bridge.set_installed(false);
        self.arbitrate_keyboard_focus(None, true, false);
    }

    /// Whether in-process content is asking for the keyboard. Read inside
    /// arbitration, after every client-side gate.
    pub(crate) fn native_keyboard_wants_focus(&self) -> bool {
        self.native_input
            .bridge
            .as_ref()
            .is_some_and(NativeInputBridge::wants_focus)
    }

    /// Record what arbitration decided; losing focus also ends a repeat.
    pub(crate) fn set_native_keyboard_focus(&mut self, focused: bool) {
        // The edge is the keyboard changing hands, INCLUDING between two
        // in-process owners while the compositor keeps it: the new owner is
        // a different field and must inherit neither the IME session nor
        // the caret of the one before it.
        let changed = self
            .native_input
            .bridge
            .as_ref()
            .is_some_and(|bridge| bridge.set_focused(focused));
        if !changed {
            return;
        }
        if !focused {
            self.cancel_native_key_repeat();
        }
        // The content cannot end its own IME session once it has lost the
        // keyboard — its requests stop at the gate in
        // `service_native_ime_request` — so the compositor ends it here, on
        // every edge. The candidate window belongs to a field nobody is
        // typing into, and it would otherwise sit over whoever has the
        // keyboard now.
        self.end_native_ime_session();
        if focused {
            // And re-arm for whoever holds it now, if THEY claimed an input
            // method (a handover clears that claim; a plain regain keeps
            // it, so the field that was preempted gets its IME back).
            //
            // DEFERRED: this runs inside arbitration, before
            // `keyboard.set_focus` has taken the keyboard off the client, so
            // that client's text input still reads active and the activation
            // would refuse itself. `service_native_ime_refresh` runs it once
            // the focus change is in effect.
            self.native_input.refresh_pending = true;
        }
    }

    /// Run the activation arbitration owed the field, now that the seat
    /// focus change it waited for has actually happened.
    pub(crate) fn service_native_ime_refresh(&mut self) {
        if !std::mem::take(&mut self.native_input.refresh_pending) {
            return;
        }
        self.refresh_native_ime_activation();
    }

    /// End the input-method session this field had, if any.
    fn end_native_ime_session(&mut self) {
        if self.native_input.activated_for.take().is_none() {
            return;
        }
        let input_method = self.seat.input_method().clone();
        input_method.deactivate_input_method(self);
        if let Some(bridge) = self.native_input.bridge.as_ref() {
            bridge.set_ime_active(false);
        }
    }

    /// Activate the input method for in-process content when it claims one,
    /// holds the keyboard, no client owns a text input, and the instance is
    /// not the one already activated for. Idempotent by that last test: an
    /// `activate` resets the instance serial, so doing it twice would
    /// mismatch a batch in flight.
    fn refresh_native_ime_activation(&mut self) {
        let enabled = self
            .native_input
            .bridge
            .as_ref()
            .is_some_and(NativeInputBridge::enabled);
        if !enabled
            || !self.native_keyboard_focused()
            || self.seat.text_input().has_active_text_input()
        {
            return;
        }
        let input_method = self.seat.input_method().clone();
        let instance = input_method.instance_epoch();
        if instance.is_none() || instance == self.native_input.activated_for {
            return;
        }
        input_method.activate_for_sink(self);
        self.native_input.activated_for = instance;
        if let Some(bridge) = self.native_input.bridge.as_ref() {
            bridge.set_ime_active(true);
        }
    }

    pub(crate) fn native_keyboard_focused(&self) -> bool {
        self.native_input
            .bridge
            .as_ref()
            .is_some_and(NativeInputBridge::focused)
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
        // An input method holding the keyboard grab is the one consumer
        // that outranks the content itself: its keys come BACK through the
        // sink as composition. Taking them here would starve the grab and
        // make every IME dead while a scene has focus.
        if self.seat.input_method().keyboard_grabbed() {
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
        if let Some(bridge) = self.native_input.bridge.as_ref() {
            bridge.push_key(event.clone());
        }
        // One key repeats at a time, exactly as a client would do it from
        // `repeat_info`: a new repeating press replaces the old, a release
        // of the repeating key ends it. A MODIFIER press does neither —
        // holding a key and then pressing Shift must not stop the repeat,
        // it only changes what the repeat produces.
        let repeating = self
            .native_input
            .repeat
            .as_ref()
            .is_some_and(|repeat| repeat.keycode == keycode);
        if !pressed {
            if repeating {
                self.cancel_native_key_repeat();
            }
            return true;
        }
        if key_repeats(handle, keycode) {
            self.cancel_native_key_repeat();
            self.arm_native_key_repeat(keycode, event, REPEAT_DELAY);
        } else if is_modifier(handle) {
            // Keep the repeat, and re-resolve what the HELD key produces
            // under the new modifiers: pressing Shift while `a` repeats
            // must start delivering `A`, not keep replaying the `a` the
            // original press resolved to.
            let repeating_code = self.native_input.repeat.as_ref().map(|repeat| repeat.keycode);
            if let Some(code) = repeating_code {
                let (text, keysym) = key_text_and_sym(handle, code);
                if let Some(repeat) = self.native_input.repeat.as_mut() {
                    repeat.event.modifiers = NativeModifiers::from(modifiers);
                    repeat.event.text = text;
                    repeat.event.keysym = keysym;
                }
            }
        } else {
            self.cancel_native_key_repeat();
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
        if let Some(bridge) = self.native_input.bridge.as_ref() {
            bridge.push_key(event);
        }
    }

    pub(crate) fn cancel_native_key_repeat(&mut self) {
        if let Some(repeat) = self.native_input.repeat.take() {
            self.capture_loop_handle.remove(repeat.timer);
        }
    }

    /// The content's own text-input state, on its way to the input method.
    pub(crate) fn service_native_ime_request(&mut self, request: NativeImeRequest) {
        // The content's own state is tracked whatever the input method is
        // doing: the caret feeds the popup anchor and `enabled` gates the
        // sink, and both must be right the moment the content DOES own it.
        if let Some(bridge) = self.native_input.bridge.as_ref() {
            bridge.note_request(&request);
        }
        // Drive the input method only while in-process content actually
        // holds the keyboard and no client holds a text input. Otherwise
        // these calls would activate, deactivate or re-anchor the input
        // method under a client's feet — `deactivate_input_method` in
        // particular ends the client's composition.
        //
        // Dropped, not queued: an IME request is a snapshot of a field's
        // state, and the content re-sends the whole set (enable,
        // surrounding text, content type, caret, done) after every edit, so
        // the first request once it owns the field again carries the truth.
        // A queue would instead replay a stale caret over the live one.
        if !self.native_keyboard_focused() || self.seat.text_input().has_active_text_input() {
            tracing::debug!(
                native_focused = self.native_keyboard_focused(),
                "native IME request dropped: the input method belongs to a client"
            );
            return;
        }
        // Activate on the false->true edge, and again only when a DIFFERENT
        // input-method instance has appeared (one that connected after the
        // field did, or replaced the one that was here). Activating on every
        // `enable(true)` would reset the instance serial and make the IM's
        // next commit arrive mismatched — which is exactly what a field
        // re-reporting its state after each edit does.
        self.refresh_native_ime_activation();
        let input_method = self.seat.input_method().clone();
        match request {
            // The activation above is the whole of `enable(true)`.
            NativeImeRequest::Enable(true) => {}
            NativeImeRequest::Enable(false) => {
                self.native_input.activated_for = None;
                if let Some(bridge) = self.native_input.bridge.as_ref() {
                    bridge.set_ime_active(false);
                }
                input_method.deactivate_input_method(self);
            }
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

    /// The caret rectangle in-process content last reported, in comp's
    /// global logical coordinates (the space surface layouts and
    /// `comp.windows.list` use).
    pub(crate) fn native_ime_caret(&self) -> Option<Rectangle<i32, Logical>> {
        let (x, y, width, height) = self.native_input.bridge.as_ref()?.caret()?;
        Some(Rectangle::new((x, y).into(), (width, height).into()))
    }
}

/// The text a key produces, empty and control characters excluded.
fn key_text(handle: &KeysymHandle<'_>) -> Option<String> {
    let text = xkb::keysym_to_utf8(handle.modified_sym());
    let text = text.trim_end_matches('\0').to_string();
    (!text.is_empty()).then_some(text)
}

/// What a key produces under the CURRENT modifier state, for a keycode
/// other than the one that was just pressed.
fn key_text_and_sym(handle: &KeysymHandle<'_>, keycode: Keycode) -> (Option<String>, u32) {
    let xkb = handle
        .xkb()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: the state reference does not outlive this lock guard.
    let keysym = unsafe { xkb.state() }.key_get_one_sym(keycode);
    let text = xkb::keysym_to_utf8(keysym);
    let text = text.trim_end_matches('\0').to_string();
    ((!text.is_empty()).then_some(text), keysym.raw())
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

/// Whether this key is a modifier, which changes a live repeat rather than
/// ending it.
fn is_modifier(handle: &KeysymHandle<'_>) -> bool {
    use xkb::keysyms as sym;
    handle.raw_syms().iter().any(|keysym| {
        matches!(
            keysym.raw(),
            sym::KEY_Shift_L
                | sym::KEY_Shift_R
                | sym::KEY_Control_L
                | sym::KEY_Control_R
                | sym::KEY_Alt_L
                | sym::KEY_Alt_R
                | sym::KEY_Meta_L
                | sym::KEY_Meta_R
                | sym::KEY_Super_L
                | sym::KEY_Super_R
                | sym::KEY_Hyper_L
                | sym::KEY_Hyper_R
                | sym::KEY_ISO_Level3_Shift
                | sym::KEY_ISO_Level5_Shift
                | sym::KEY_Caps_Lock
                | sym::KEY_Num_Lock
                | sym::KEY_Shift_Lock
        )
    })
}
