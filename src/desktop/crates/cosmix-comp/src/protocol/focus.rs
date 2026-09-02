//! Cross-protocol seat focus target.
//!
//! The seat's keyboard/pointer/touch focus is this wrapper rather than a raw
//! `WlSurface` because X11 windows need the X half of focus as well as the
//! Wayland half: Smithay's `X11Surface` keyboard target applies the ICCCM
//! input mode (`SetInputFocus` and/or `WM_TAKE_FOCUS`) *before* forwarding
//! the Wayland keyboard enter to the associated `wl_surface`. Focusing only
//! the raw `wl_surface` would render an X11 window that never reliably
//! accepts keyboard input.
//!
//! Without the `xwayland` feature the enum has a single `Wayland` variant and
//! every impl delegates 1:1 to the `WlSurface` implementations Smithay
//! already provides — the no-feature build behaves exactly as before.

use super::*;
use smithay::{
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{
            GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
            GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
            GestureSwipeEndEvent, GestureSwipeUpdateEvent, PointerTarget as SmithayPointerTarget,
            RelativeMotionEvent,
        },
        touch::{
            OrientationEvent as TouchOrientationEvent, ShapeEvent as TouchShapeEvent, TouchTarget,
        },
    },
    utils::IsAlive,
    wayland::seat::WaylandFocus,
};
use std::borrow::Cow;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SeatFocusTarget {
    Wayland(WlSurface),
    #[cfg(feature = "xwayland")]
    X11(smithay::xwayland::X11Surface),
}

impl SeatFocusTarget {
    /// The Wayland surface input events ultimately reach. For X11 targets
    /// this is the associated surface, present once association completed.
    pub(crate) fn surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) => Some(Cow::Borrowed(surface)),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => surface.wl_surface().map(Cow::Owned),
        }
    }

    pub(crate) fn surface_id(&self) -> Option<ObjectId> {
        self.surface().map(|surface| surface.id())
    }

    pub(crate) fn owned_surface(&self) -> Option<WlSurface> {
        self.surface().map(Cow::into_owned)
    }
}

/// Whether a seat focus resolves to exactly this `wl_surface`.
pub(super) fn focus_targets_surface(focus: Option<&SeatFocusTarget>, surface: &WlSurface) -> bool {
    focus
        .and_then(SeatFocusTarget::surface)
        .is_some_and(|focused| focused.as_ref() == surface)
}

impl From<WlSurface> for SeatFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}

impl From<PopupKind> for SeatFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

impl IsAlive for SeatFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => surface.alive(),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => surface.alive(),
        }
    }
}

impl WaylandFocus for SeatFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        self.surface()
    }

    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            Self::Wayland(surface) => surface.same_client_as(object_id),
            #[cfg(feature = "xwayland")]
            Self::X11(surface) => surface.same_client_as(object_id),
        }
    }
}

macro_rules! delegate_focus {
    ($self:ident, $surface:ident => $call:expr) => {
        match $self {
            SeatFocusTarget::Wayland($surface) => $call,
            #[cfg(feature = "xwayland")]
            SeatFocusTarget::X11($surface) => $call,
        }
    };
}

impl KeyboardTarget<WaylandState> for SeatFocusTarget {
    fn enter(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        delegate_focus!(self, surface => KeyboardTarget::enter(surface, seat, data, keys, serial))
    }

    fn leave(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, serial: Serial) {
        delegate_focus!(self, surface => KeyboardTarget::leave(surface, seat, data, serial))
    }

    fn key(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        delegate_focus!(self, surface => KeyboardTarget::key(surface, seat, data, key, state, serial, time))
    }

    fn modifiers(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        delegate_focus!(self, surface => KeyboardTarget::modifiers(surface, seat, data, modifiers, serial))
    }
}

impl SmithayPointerTarget<WaylandState> for SeatFocusTarget {
    fn enter(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, event: &MotionEvent) {
        delegate_focus!(self, surface => SmithayPointerTarget::enter(surface, seat, data, event))
    }

    fn motion(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, event: &MotionEvent) {
        delegate_focus!(self, surface => SmithayPointerTarget::motion(surface, seat, data, event))
    }

    fn relative_motion(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &RelativeMotionEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::relative_motion(surface, seat, data, event))
    }

    fn button(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, event: &ButtonEvent) {
        delegate_focus!(self, surface => SmithayPointerTarget::button(surface, seat, data, event))
    }

    fn axis(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, frame: AxisFrame) {
        delegate_focus!(self, surface => SmithayPointerTarget::axis(surface, seat, data, frame))
    }

    fn frame(&self, seat: &Seat<WaylandState>, data: &mut WaylandState) {
        delegate_focus!(self, surface => SmithayPointerTarget::frame(surface, seat, data))
    }

    fn gesture_swipe_begin(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GestureSwipeBeginEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_swipe_begin(surface, seat, data, event))
    }

    fn gesture_swipe_update(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GestureSwipeUpdateEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_swipe_update(surface, seat, data, event))
    }

    fn gesture_swipe_end(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GestureSwipeEndEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_swipe_end(surface, seat, data, event))
    }

    fn gesture_pinch_begin(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GesturePinchBeginEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_pinch_begin(surface, seat, data, event))
    }

    fn gesture_pinch_update(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GesturePinchUpdateEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_pinch_update(surface, seat, data, event))
    }

    fn gesture_pinch_end(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GesturePinchEndEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_pinch_end(surface, seat, data, event))
    }

    fn gesture_hold_begin(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GestureHoldBeginEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_hold_begin(surface, seat, data, event))
    }

    fn gesture_hold_end(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &GestureHoldEndEvent,
    ) {
        delegate_focus!(self, surface => SmithayPointerTarget::gesture_hold_end(surface, seat, data, event))
    }

    fn leave(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, serial: Serial, time: u32) {
        delegate_focus!(self, surface => SmithayPointerTarget::leave(surface, seat, data, serial, time))
    }
}

impl TouchTarget<WaylandState> for SeatFocusTarget {
    fn down(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &TouchDownEvent,
        seq: Serial,
    ) {
        delegate_focus!(self, surface => TouchTarget::down(surface, seat, data, event, seq))
    }

    fn up(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &TouchUpEvent,
        seq: Serial,
    ) {
        delegate_focus!(self, surface => TouchTarget::up(surface, seat, data, event, seq))
    }

    fn motion(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &TouchMotionEvent,
        seq: Serial,
    ) {
        delegate_focus!(self, surface => TouchTarget::motion(surface, seat, data, event, seq))
    }

    fn frame(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, seq: Serial) {
        delegate_focus!(self, surface => TouchTarget::frame(surface, seat, data, seq))
    }

    fn cancel(&self, seat: &Seat<WaylandState>, data: &mut WaylandState, seq: Serial) {
        delegate_focus!(self, surface => TouchTarget::cancel(surface, seat, data, seq))
    }

    fn shape(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &TouchShapeEvent,
        seq: Serial,
    ) {
        delegate_focus!(self, surface => TouchTarget::shape(surface, seat, data, event, seq))
    }

    fn orientation(
        &self,
        seat: &Seat<WaylandState>,
        data: &mut WaylandState,
        event: &TouchOrientationEvent,
        seq: Serial,
    ) {
        delegate_focus!(self, surface => TouchTarget::orientation(surface, seat, data, event, seq))
    }
}
