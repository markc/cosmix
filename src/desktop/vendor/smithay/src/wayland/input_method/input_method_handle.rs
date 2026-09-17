use std::{
    fmt,
    sync::{Arc, Mutex},
};

use tracing::warn;
use wayland_protocols_misc::zwp_input_method_v2::server::{
    zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
    zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
};
use wayland_server::{backend::ClientId, protocol::wl_surface::WlSurface};
use wayland_server::{
    protocol::wl_keyboard::KeymapFormat, Client, DataInit, Dispatch, DisplayHandle, Resource,
};

use crate::{
    input::{keyboard::KeyboardHandle, SeatHandler},
    utils::{alive_tracker::AliveTracker, Logical, Rectangle, SERIAL_COUNTER},
    wayland::{compositor, seat::WaylandFocus, text_input::TextInputHandle},
};

use super::{
    input_method_keyboard_grab::InputMethodKeyboardGrab,
    input_method_popup_surface::{PopupHandle, PopupParent, PopupSurface},
    InputMethodHandler, InputMethodKeyboardUserData, InputMethodManagerState,
    InputMethodPopupSurfaceUserData, INPUT_POPUP_SURFACE_ROLE,
};

#[derive(Default)]
pub(crate) struct InputMethod {
    pub instance: Option<Instance>,
    pub popup_handle: PopupHandle,
    pub keyboard_grab: InputMethodKeyboardGrab,
    pub sink: Option<InputMethodSink>,
}

impl fmt::Debug for InputMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputMethod")
            .field("instance", &self.instance)
            .field("popup_handle", &self.popup_handle)
            .field("keyboard_grab", &self.keyboard_grab)
            .field("sink", &self.sink.is_some())
            .finish()
    }
}

/// What an input method produced, for content the compositor draws itself.
///
/// COSMIX PATCH (see `vendor/README.md`): a compositor that renders its own
/// text field has no `wl_surface` and therefore no `zwp_text_input_v3`, so
/// upstream's routing (`with_active_text_input`) drops every request. These
/// are handed to the sink instead, and only when no focused client has an
/// active text input.
#[derive(Debug, Clone, PartialEq)]
pub enum InputMethodSinkEvent {
    /// Text to insert at the cursor.
    CommitString(String),
    /// The preedit (composing) string and its cursor span.
    PreeditString {
        /// The composing text.
        text: String,
        /// Cursor start, in bytes of `text`.
        cursor_begin: i32,
        /// Cursor end, in bytes of `text`.
        cursor_end: i32,
    },
    /// Delete around the cursor before applying the rest of this batch.
    DeleteSurroundingText {
        /// Bytes to delete before the cursor.
        before_length: u32,
        /// Bytes to delete after the cursor.
        after_length: u32,
    },
    /// The end of one input-method batch; apply what came before it.
    /// `discard_state` mirrors upstream's serial mismatch.
    Done {
        /// The batch's serial did not match: discard rather than apply.
        discard_state: bool,
    },
}

/// Sink for [`InputMethodSinkEvent`] (see `InputMethodHandle::set_sink`).
pub type InputMethodSink = Arc<dyn Fn(InputMethodSinkEvent) + Send + Sync>;

#[derive(Debug)]
pub(crate) struct Instance {
    pub object: ZwpInputMethodV2,
    pub serial: u32,
}

impl Instance {
    /// Send the done incrementing the serial.
    pub(crate) fn done(&mut self) {
        self.object.done();
        self.serial += 1;
    }
}

/// Handle to an input method instance
#[derive(Default, Debug, Clone)]
pub struct InputMethodHandle {
    pub(crate) inner: Arc<Mutex<InputMethod>>,
}

impl InputMethodHandle {
    pub(super) fn add_instance(&self, instance: &ZwpInputMethodV2) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(instance) = inner.instance.as_mut() {
            instance.serial = 0;
            instance.object.unavailable();
        } else {
            inner.instance = Some(Instance {
                object: instance.clone(),
                serial: 0,
            });
        }
    }

    /// COSMIX PATCH: route input-method output to compositor-drawn content
    /// when no focused client has an active text input. `None` restores the
    /// upstream behaviour of dropping those requests.
    pub fn set_sink(&self, sink: Option<InputMethodSink>) {
        self.inner.lock().unwrap().sink = sink;
    }

    /// COSMIX PATCH: the registered sink, if any.
    pub fn sink(&self) -> Option<InputMethodSink> {
        self.inner.lock().unwrap().sink.clone()
    }

    /// COSMIX PATCH: deliver to the compositor sink when the focused client
    /// has no active text input. Returns whether the sink took it.
    fn to_sink(&self, text_input: &TextInputHandle, event: InputMethodSinkEvent) -> bool {
        if text_input.has_active_text_input() {
            return false;
        }
        let Some(sink) = self.sink() else {
            return false;
        };
        sink(event);
        true
    }

    /// COSMIX PATCH: activate the input method for compositor-drawn content.
    /// There is no client surface, so the popup keeps the parent it has (none
    /// for a compositor field) and the compositor places it from its own
    /// caret rectangle.
    pub fn activate_for_sink<D: SeatHandler + 'static>(&self, state: &mut D) {
        self.with_input_method(|im| {
            if let Some(instance) = im.instance.as_ref() {
                instance.object.activate();
                if let Some(popup) = im.popup_handle.surface.as_ref() {
                    let data = instance.object.data::<InputMethodUserData<D>>().unwrap();
                    (data.popup_repositioned)(state, popup.clone());
                }
            }
        });
    }

    /// COSMIX PATCH: the compositor's own text field reporting its state to
    /// the input method (`surrounding_text`, `content_type`,
    /// `text_change_cause`), ending with [`Self::send_done`].
    pub fn with_active_instance<F>(&self, f: F)
    where
        F: FnOnce(&ZwpInputMethodV2),
    {
        let inner = self.inner.lock().unwrap();
        if let Some(instance) = inner.instance.as_ref() {
            f(&instance.object);
        }
    }

    /// COSMIX PATCH: end a compositor-side batch (upstream's `done`, serial
    /// incremented).
    pub fn send_done(&self) {
        self.with_instance(|instance| instance.done());
    }

    /// Whether there's an acitve instance of input-method.
    pub(crate) fn has_instance(&self) -> bool {
        self.inner.lock().unwrap().instance.is_some()
    }

    /// Callback function to access the input method object
    pub(crate) fn with_instance<F>(&self, f: F)
    where
        F: FnOnce(&mut Instance),
    {
        let mut inner = self.inner.lock().unwrap();
        if let Some(instance) = inner.instance.as_mut() {
            f(instance);
        }
    }

    /// Callback function to access the input method.
    pub(crate) fn with_input_method<F>(&self, mut f: F)
    where
        F: FnMut(&mut InputMethod),
    {
        let mut inner = self.inner.lock().unwrap();
        f(&mut inner);
    }

    /// Indicates that an input method has grabbed a keyboard
    pub fn keyboard_grabbed(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        let keyboard = inner.keyboard_grab.inner.lock().unwrap();
        keyboard.grab.is_some()
    }

    /// COSMIX PATCH: `pub` so the compositor can report the caret rectangle
    /// of content it draws itself (upstream reaches this only through a
    /// client's `zwp_text_input_v3`).
    pub fn set_text_input_rectangle<D: SeatHandler + 'static>(
        &self,
        state: &mut D,
        rect: Rectangle<i32, Logical>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.popup_handle.rectangle = rect;

        let mut popup_surface = match inner.popup_handle.surface.clone() {
            Some(popup_surface) => popup_surface,
            None => return,
        };

        popup_surface.set_text_input_rectangle(rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);

        if let Some(instance) = &inner.instance {
            let data = instance.object.data::<InputMethodUserData<D>>().unwrap();
            (data.popup_repositioned)(state, popup_surface);
        };
    }

    /// Activate input method on the given surface.
    pub(crate) fn activate_input_method<D: SeatHandler + 'static>(&self, state: &mut D, surface: &WlSurface) {
        self.with_input_method(|im| {
            if let Some(instance) = im.instance.as_ref() {
                instance.object.activate();
                if let Some(popup) = im.popup_handle.surface.as_mut() {
                    let data = instance.object.data::<InputMethodUserData<D>>().unwrap();
                    let location = (data.popup_geometry_callback)(state, surface);
                    // Remove old popup.
                    (data.dismiss_popup)(state, popup.clone());

                    // Add a new one with updated parent.
                    let parent = PopupParent {
                        surface: surface.clone(),
                        location,
                    };
                    popup.set_parent(Some(parent));
                    (data.new_popup)(state, popup.clone());
                }
            }
        });
    }

    /// Deactivate the active input method.
    ///
    /// The `done` is always send when deactivating IME. COSMIX PATCH: `pub`
    /// so compositor-drawn content can end its own IME session.
    pub fn deactivate_input_method<D: SeatHandler + 'static>(&self, state: &mut D) {
        self.with_input_method(|im| {
            if let Some(instance) = im.instance.as_mut() {
                instance.object.deactivate();
                instance.done();
                if let Some(popup) = im.popup_handle.surface.as_mut() {
                    let data = instance.object.data::<InputMethodUserData<D>>().unwrap();
                    if popup.get_parent().is_some() {
                        (data.dismiss_popup)(state, popup.clone());
                    }
                    popup.set_parent(None);
                }
            }
        });
    }
}

/// User data of ZwpInputMethodV2 object
pub struct InputMethodUserData<D: SeatHandler> {
    pub(super) handle: InputMethodHandle,
    pub(crate) text_input_handle: TextInputHandle,
    pub(crate) keyboard_handle: KeyboardHandle<D>,
    pub(crate) popup_geometry_callback: fn(&D, &WlSurface) -> Rectangle<i32, Logical>,
    pub(crate) new_popup: fn(&mut D, PopupSurface),
    pub(crate) popup_repositioned: fn(&mut D, PopupSurface),
    pub(crate) dismiss_popup: fn(&mut D, PopupSurface),
}

impl<D: SeatHandler> fmt::Debug for InputMethodUserData<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputMethodUserData")
            .field("handle", &self.handle)
            .field("text_input_handle", &self.text_input_handle)
            .field("keyboard_handle", &self.keyboard_handle)
            .finish()
    }
}

impl<D> Dispatch<ZwpInputMethodV2, InputMethodUserData<D>, D> for InputMethodManagerState
where
    D: Dispatch<ZwpInputMethodV2, InputMethodUserData<D>>,
    D: Dispatch<ZwpInputPopupSurfaceV2, InputMethodPopupSurfaceUserData>,
    D: Dispatch<ZwpInputMethodKeyboardGrabV2, InputMethodKeyboardUserData<D>>,
    D: SeatHandler,
    D: InputMethodHandler,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
    D: 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        seat: &ZwpInputMethodV2,
        request: zwp_input_method_v2::Request,
        data: &InputMethodUserData<D>,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwp_input_method_v2::Request::CommitString { text } => {
                if !data.handle.to_sink(
                    &data.text_input_handle,
                    InputMethodSinkEvent::CommitString(text.clone()),
                ) {
                    data.text_input_handle.with_active_text_input(|ti, _surface| {
                        ti.commit_string(Some(text.clone()));
                    });
                }
            }
            zwp_input_method_v2::Request::SetPreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                if !data.handle.to_sink(
                    &data.text_input_handle,
                    InputMethodSinkEvent::PreeditString {
                        text: text.clone(),
                        cursor_begin,
                        cursor_end,
                    },
                ) {
                    data.text_input_handle.with_active_text_input(|ti, _surface| {
                        ti.preedit_string(Some(text.clone()), cursor_begin, cursor_end);
                    });
                }
            }
            zwp_input_method_v2::Request::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                if !data.handle.to_sink(
                    &data.text_input_handle,
                    InputMethodSinkEvent::DeleteSurroundingText {
                        before_length,
                        after_length,
                    },
                ) {
                    data.text_input_handle.with_active_text_input(|ti, _surface| {
                        ti.delete_surrounding_text(before_length, after_length);
                    });
                }
            }
            zwp_input_method_v2::Request::Commit { serial } => {
                let current_serial = data
                    .handle
                    .inner
                    .lock()
                    .unwrap()
                    .instance
                    .as_ref()
                    .map(|i| i.serial)
                    .unwrap_or(0);

                let discard_state = serial != current_serial;
                if !data.handle.to_sink(
                    &data.text_input_handle,
                    InputMethodSinkEvent::Done { discard_state },
                ) {
                    data.text_input_handle.done(discard_state);
                }
            }
            zwp_input_method_v2::Request::GetInputPopupSurface { id, surface } => {
                if compositor::give_role(&surface, INPUT_POPUP_SURFACE_ROLE).is_err()
                    && compositor::get_role(&surface) != Some(INPUT_POPUP_SURFACE_ROLE)
                {
                    // Protocol requires this raise an error, but doesn't define an error enum
                    seat.post_error(0u32, "Surface already has a role.");
                    return;
                }

                let parent = match data.text_input_handle.focus().clone() {
                    Some(parent) => {
                        let location = state.parent_geometry(&parent);
                        Some(PopupParent {
                            surface: parent,
                            location,
                        })
                    }
                    None => None,
                };
                let mut input_method = data.handle.inner.lock().unwrap();

                let instance = data_init.init(
                    id,
                    InputMethodPopupSurfaceUserData {
                        alive_tracker: AliveTracker::default(),
                    },
                );
                let popup_rect = Arc::new(Mutex::new(input_method.popup_handle.rectangle));
                let popup = PopupSurface::new(instance, surface, popup_rect, parent);
                input_method.popup_handle.surface = Some(popup.clone());
                // COSMIX PATCH: with a compositor sink there is no parent
                // surface, and the compositor places the popup from its own
                // caret rectangle.
                if popup.get_parent().is_some() || input_method.sink.is_some() {
                    drop(input_method);
                    state.new_popup(popup);
                }
            }
            zwp_input_method_v2::Request::GrabKeyboard { keyboard } => {
                let input_method = data.handle.inner.lock().unwrap();
                data.keyboard_handle.set_grab(
                    state,
                    input_method.keyboard_grab.clone(),
                    SERIAL_COUNTER.next_serial(),
                );
                let instance = data_init.init(
                    keyboard,
                    InputMethodKeyboardUserData {
                        handle: input_method.keyboard_grab.clone(),
                        keyboard_handle: data.keyboard_handle.clone(),
                    },
                );
                let mut keyboard = input_method.keyboard_grab.inner.lock().unwrap();
                keyboard.grab = Some(instance.clone());
                keyboard.text_input_handle = data.text_input_handle.clone();
                let guard = data.keyboard_handle.arc.internal.lock().unwrap();
                instance.repeat_info(guard.repeat_rate, guard.repeat_delay);
                let keymap_file = data.keyboard_handle.arc.keymap.lock().unwrap();
                let res = keymap_file.with_fd(false, |fd, size| {
                    instance.keymap(KeymapFormat::XkbV1, fd, size as u32);
                });

                if let Err(err) = res {
                    warn!(err = ?err, "Failed to send keymap to client");
                } else {
                    // Modifiers can be latched when taking the grab, thus we must send them to keep
                    // them in sync.
                    let mods = guard.mods_state.serialized;
                    instance.modifiers(
                        SERIAL_COUNTER.next_serial().into(),
                        mods.depressed,
                        mods.latched,
                        mods.locked,
                        mods.layout_effective,
                    );
                }
            }
            zwp_input_method_v2::Request::Destroy => {
                // Nothing to do
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        _state: &mut D,
        _client: ClientId,
        _input_method: &ZwpInputMethodV2,
        data: &InputMethodUserData<D>,
    ) {
        data.handle.inner.lock().unwrap().instance = None;
        data.text_input_handle.leave();
    }
}
