use std::{
    cell::RefCell,
    fmt,
    os::unix::io::{AsFd, OwnedFd},
    sync::{Arc, Mutex},
};

use wayland_server::{
    backend::{protocol::Message, ClientId, Handle, ObjectData, ObjectId},
    protocol::{
        wl_data_device_manager::DndAction,
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
        wl_surface::WlSurface,
    },
    DisplayHandle, Resource,
};

use crate::{
    input::{
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
            GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
            PointerInnerHandle, RelativeMotionEvent,
        },
        touch::{GrabStartData as TouchGrabStartData, TouchGrab},
        Seat, SeatHandler,
    },
    utils::{IsAlive, Logical, Point, Serial, SERIAL_COUNTER},
    wayland::{seat::WaylandFocus, selection::seat_data::SeatData},
};

use super::{with_source_metadata, ClientDndGrabHandler, DataDeviceHandler};

/// Grab during a client-initiated DnD operation.
pub struct DnDGrab<D: SeatHandler> {
    dh: DisplayHandle,
    pointer_start_data: Option<PointerGrabStartData<D>>,
    touch_start_data: Option<TouchGrabStartData<D>>,
    data_source: Option<wl_data_source::WlDataSource>,
    current_focus: Option<WlSurface>,
    pending_offers: Vec<wl_data_offer::WlDataOffer>,
    offer_data: Option<Arc<Mutex<OfferData>>>,
    // cosmix patch: only the grab's own release handler may authorise a drop — see
    // `conclude_pointer_release_as_drop`/`conclude_touch_release_as_drop`, the ONLY
    // two places in this file allowed to set this true. Every other path that reaches
    // `unset()` leaves it false, so `unset()` cancels. That set includes six call
    // sites outside this module, generic to every grab type, not just DnD:
    // input/pointer/mod.rs:544,560,762,783,909 and input/touch/mod.rs:424,433,530,537,676.
    // None of today's six violates the contract — they replace whatever grab happens
    // to be active for an unrelated reason (session lock, popup install, a new grab
    // superseding this one, ...) and never intend to "deliver" a drag. This is safe
    // only because no caller outside this file can reach a concrete `&mut DnDGrab` at
    // all — it is boxed inside a private `GrabStatus` — so nothing external can even
    // attempt to force a drop today. If a public "commit this drag's drop" API is
    // ever added, it MUST go through one of the two `conclude_*_release_as_drop`
    // helpers (or an equivalent atomic set-flag-then-unset step); never add a second,
    // ad hoc `pending_drop = true; handle.unset_grab(...)` pair anywhere.
    pending_drop: bool,
    // cosmix patch: cancel()/drop() each run at most once, guaranteeing the
    // ClientDndGrabHandler::dropped() callback fires exactly once per grab even
    // if some future teardown path were to reach unset() a second time.
    finished: bool,
    icon: Option<WlSurface>,
    origin: WlSurface,
    seat: Seat<D>,
}

impl<D: SeatHandler + 'static> fmt::Debug for DnDGrab<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DnDGrab")
            .field("dh", &self.dh)
            .field("pointer_start_data", &self.pointer_start_data)
            .field("touch_start_data", &self.touch_start_data)
            .field("data_source", &self.data_source)
            .field("current_focus", &self.current_focus)
            .field("pending_offers", &self.pending_offers)
            .field("offer_data", &self.offer_data)
            // cosmix patch: without these two, a trace dump of a mid-teardown grab
            // misrepresents which arm `unset()` is about to take.
            .field("pending_drop", &self.pending_drop)
            .field("finished", &self.finished)
            .field("icon", &self.icon)
            .field("origin", &self.origin)
            .field("seat", &self.seat)
            .finish()
    }
}

impl<D: SeatHandler> DnDGrab<D> {
    pub(crate) fn new_pointer(
        dh: &DisplayHandle,
        start_data: PointerGrabStartData<D>,
        source: Option<wl_data_source::WlDataSource>,
        origin: WlSurface,
        seat: Seat<D>,
        icon: Option<WlSurface>,
    ) -> Self {
        Self {
            dh: dh.clone(),
            pointer_start_data: Some(start_data),
            touch_start_data: None,
            data_source: source,
            current_focus: None,
            pending_offers: Vec::with_capacity(1),
            offer_data: None,
            // cosmix patch: external teardown cancels unless a release authorises a drop.
            pending_drop: false,
            finished: false,
            origin,
            icon,
            seat,
        }
    }

    pub(crate) fn new_touch(
        dh: &DisplayHandle,
        start_data: TouchGrabStartData<D>,
        source: Option<wl_data_source::WlDataSource>,
        origin: WlSurface,
        seat: Seat<D>,
        icon: Option<WlSurface>,
    ) -> Self {
        Self {
            dh: dh.clone(),
            pointer_start_data: None,
            touch_start_data: Some(start_data),
            data_source: source,
            current_focus: None,
            pending_offers: Vec::with_capacity(1),
            offer_data: None,
            // cosmix patch: external teardown cancels unless a release authorises a drop.
            pending_drop: false,
            finished: false,
            origin,
            icon,
            seat,
        }
    }

    // cosmix patch: source destruction must identify the matching active grab, not another grab.
    pub(super) fn has_source(&self, source: &WlDataSource) -> bool {
        self.data_source.as_ref() == Some(source)
    }
}

impl<D> DnDGrab<D>
where
    D: DataDeviceHandler,
    D: SeatHandler,
    D: 'static,
{
    fn update_focus<F: WaylandFocus>(
        &mut self,
        focus: Option<(F, Point<f64, Logical>)>,
        location: Point<f64, Logical>,
        serial: Serial,
        time: u32,
    ) {
        let seat_data = self
            .seat
            .user_data()
            .get::<RefCell<SeatData<D::SelectionUserData>>>()
            .unwrap()
            .borrow_mut();
        if focus.as_ref().and_then(|(s, _)| s.wl_surface()).as_deref() != self.current_focus.as_ref() {
            // focus changed, we need to make a leave if appropriate
            if let Some(surface) = self.current_focus.take() {
                // only leave if there is a data source or we are on the original client
                if self.data_source.is_some() || self.origin.id().same_client_as(&surface.id()) {
                    for device in seat_data.known_data_devices() {
                        if device.id().same_client_as(&surface.id()) {
                            device.leave();
                        }
                    }
                    // disable the offers
                    self.pending_offers.clear();
                    if let Some(offer_data) = self.offer_data.take() {
                        offer_data.lock().unwrap().active = false;
                    }
                }
            }
        }
        if let Some((surface, surface_location)) = focus
            .as_ref()
            .and_then(|(h, loc)| h.wl_surface().map(|s| (s, loc)))
        {
            // early return if the surface is no longer valid
            let client = match self.dh.get_client(surface.id()) {
                Ok(c) => c,
                Err(_) => return,
            };
            let (x, y) = (location - *surface_location).into();
            if self.current_focus.is_none() {
                // We entered a new surface, send the data offer if appropriate
                if let Some(ref source) = self.data_source {
                    let offer_data = Arc::new(Mutex::new(OfferData {
                        active: true,
                        dropped: false,
                        accepted: true,
                        finished: false,
                        chosen_action: DndAction::empty(),
                    }));
                    for device in seat_data
                        .known_data_devices()
                        .filter(|d| d.id().same_client_as(&surface.id()))
                    {
                        let handle = self.dh.backend_handle();

                        // create a data offer
                        let offer = handle
                            .create_object::<D>(
                                client.id(),
                                WlDataOffer::interface(),
                                device.version(),
                                Arc::new(DndDataOffer {
                                    offer_data: offer_data.clone(),
                                    source: source.clone(),
                                }),
                            )
                            .unwrap();
                        let offer = WlDataOffer::from_id(&self.dh, offer).unwrap();

                        // advertize the offer to the client
                        device.data_offer(&offer);
                        with_source_metadata(source, |meta| {
                            for mime_type in meta.mime_types.iter().cloned() {
                                offer.offer(mime_type);
                            }
                            offer.source_actions(meta.dnd_action);
                        })
                        .unwrap();
                        device.enter(serial.into(), &surface, x, y, Some(&offer));
                        self.pending_offers.push(offer);
                    }
                    self.offer_data = Some(offer_data);
                } else {
                    // only send if we are on a surface of the same client
                    if self.origin.id().same_client_as(&surface.id()) {
                        for device in seat_data.known_data_devices() {
                            if device.id().same_client_as(&surface.id()) {
                                device.enter(serial.into(), &surface, x, y, None);
                            }
                        }
                    }
                }
                self.current_focus = Some(surface.into_owned());
            } else {
                // make a move
                if self.data_source.is_some() || self.origin.id().same_client_as(&surface.id()) {
                    for device in seat_data.known_data_devices() {
                        if device.id().same_client_as(&surface.id()) {
                            device.motion(time, x, y);
                        }
                    }
                }
            }
        }
    }

    // cosmix patch: teardown revokes offers and leaves the target without delivering a drop.
    fn cancel(&mut self, data: &mut D) {
        // cosmix patch: cancel()/drop() must each run at most once — see `finished`.
        if self.finished {
            return;
        }
        self.finished = true;
        self.pending_drop = false;
        self.pending_offers.clear();
        if let Some(offer_data) = self.offer_data.take() {
            offer_data.lock().unwrap().active = false;
        }
        let focus = self.current_focus.take();
        if let Some(ref surface) = focus {
            if self.data_source.is_some() || self.origin.id().same_client_as(&surface.id()) {
                let seat_data = self
                    .seat
                    .user_data()
                    .get::<RefCell<SeatData<D::SelectionUserData>>>()
                    .unwrap()
                    .borrow();
                for device in seat_data.known_data_devices() {
                    if device.id().same_client_as(&surface.id()) {
                        device.leave();
                    }
                }
            }
        }
        if let Some(source) = self.data_source.take() {
            // cosmix patch: no liveness guard, matching drop()'s unvalidated branch
            // below — sending an event to an already-destroyed resource is a no-op
            // in wayland-server (the object id no longer resolves, so the event is
            // simply never encoded), not a panic risk. Upstream's own `drop()`
            // calls `cancelled()` here completely unguarded already.
            source.cancelled();
        }
        self.icon = None;
        // cosmix patch (MAJOR fix): cancel is an end-of-session too, exactly like drop() —
        // the handler must be told so per-drag cleanup (e.g. removing a composited drag
        // icon) runs on every teardown, not only on a successful release. `validated` is
        // false, the same value an unaccepted drop already passes.
        ClientDndGrabHandler::dropped(data, focus, false, self.seat.clone());
    }

    fn drop(&mut self, data: &mut D) {
        // cosmix patch: cancel()/drop() must each run at most once — see `finished`.
        if self.finished {
            return;
        }
        self.finished = true;
        self.pending_drop = false;
        // the user dropped, proceed to the drop
        let seat_data = self
            .seat
            .user_data()
            .get::<RefCell<SeatData<D::SelectionUserData>>>()
            .unwrap()
            .borrow_mut();
        let validated = if let Some(ref offer_data) = self.offer_data {
            let offer_data = offer_data.lock().unwrap();
            offer_data.accepted && (!offer_data.chosen_action.is_empty())
        } else {
            false
        };
        let has_source = self.data_source.is_some();
        let focus = self.current_focus.take();
        if let Some(ref surface) = focus {
            if has_source || self.origin.id().same_client_as(&surface.id()) {
                for device in seat_data.known_data_devices() {
                    if device.id().same_client_as(&surface.id()) && validated {
                        device.drop();
                    }
                }
            }
        }
        if let Some(offer_data) = self.offer_data.take() {
            let mut offer_data = offer_data.lock().unwrap();
            if validated {
                offer_data.dropped = true;
            } else {
                offer_data.active = false;
            }
        }
        if let Some(source) = self.data_source.take() {
            if !validated {
                source.cancelled();
            } else if source.version() >= wl_data_source::EVT_DND_DROP_PERFORMED_SINCE {
                source.dnd_drop_performed();
            }
        }

        ClientDndGrabHandler::dropped(data, focus.clone(), validated, self.seat.clone());
        self.icon = None;
        // in all cases abandon the drop
        // no more buttons are pressed, release the grab
        if let Some(ref surface) = focus {
            for device in seat_data.known_data_devices() {
                if device.id().same_client_as(&surface.id()) {
                    device.leave();
                }
            }
        }
    }

    // cosmix patch: the ONLY sanctioned way to end this grab as a drop from a pointer
    // release. Bundles setting `pending_drop` with the generic `unset_grab` call so the
    // two cannot be split, forgotten, or reordered — see `pending_drop`'s doc comment
    // for the full contract this protects.
    fn conclude_pointer_release_as_drop(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        serial: Serial,
        time: u32,
    ) where
        <D as SeatHandler>::PointerFocus: WaylandFocus,
    {
        self.pending_drop = true;
        handle.unset_grab(self, data, serial, time, true);
    }

    // cosmix patch: the touch twin of `conclude_pointer_release_as_drop` — see there.
    fn conclude_touch_release_as_drop(
        &mut self,
        data: &mut D,
        handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
    ) where
        <D as SeatHandler>::TouchFocus: WaylandFocus,
    {
        self.pending_drop = true;
        handle.unset_grab(self, data);
    }
}

impl<D> PointerGrab<D> for DnDGrab<D>
where
    D: DataDeviceHandler,
    D: SeatHandler,
    <D as SeatHandler>::PointerFocus: WaylandFocus,
    D: 'static,
{
    fn motion(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        focus: Option<(<D as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        self.update_focus(focus, event.location, event.serial, event.time);
    }

    fn relative_motion(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        focus: Option<(<D as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>, event: &ButtonEvent) {
        if handle.current_pressed().is_empty() {
            self.conclude_pointer_release_as_drop(data, handle, event.serial, event.time);
        }
    }

    fn axis(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>, details: AxisFrame) {
        // we just forward the axis events as is
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut D, handle: &mut PointerInnerHandle<'_, D>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut D,
        handle: &mut PointerInnerHandle<'_, D>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<D> {
        self.pointer_start_data.as_ref().unwrap()
    }

    fn unset(&mut self, data: &mut D) {
        // cosmix patch: unset from any teardown other than our own release must cancel.
        if self.pending_drop {
            self.drop(data);
        } else {
            self.cancel(data);
        }
    }
}

impl<D> TouchGrab<D> for DnDGrab<D>
where
    D: DataDeviceHandler,
    D: SeatHandler,
    <D as SeatHandler>::TouchFocus: WaylandFocus,
    D: 'static,
{
    fn down(
        &mut self,
        _data: &mut D,
        _handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
        _focus: Option<(<D as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        _event: &crate::input::touch::DownEvent,
        _seq: crate::utils::Serial,
    ) {
        // Ignore
    }

    fn up(
        &mut self,
        data: &mut D,
        handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
        event: &crate::input::touch::UpEvent,
        _seq: crate::utils::Serial,
    ) {
        if event.slot != self.start_data().slot {
            return;
        }

        // cosmix patch: only the initiating touch's release authorises a drop.
        self.conclude_touch_release_as_drop(data, handle);
    }

    fn motion(
        &mut self,
        _data: &mut D,
        _handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
        focus: Option<(<D as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &crate::input::touch::MotionEvent,
        _seq: crate::utils::Serial,
    ) {
        if event.slot != self.start_data().slot {
            return;
        }

        self.update_focus(focus, event.location, SERIAL_COUNTER.next_serial(), event.time);
    }

    fn frame(
        &mut self,
        _data: &mut D,
        _handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
        _seq: crate::utils::Serial,
    ) {
    }

    fn cancel(
        &mut self,
        data: &mut D,
        handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
        seq: crate::utils::Serial,
    ) {
        // cosmix patch: an external touch-cancel (the stream was claimed as a gesture)
        // must still send wl_touch.cancel and drain touch focus — `unset_grab` alone
        // only tears down this grab, it never reaches `TouchInternal::cancel`, which is
        // exactly the existing cosmix fix at input/touch/mod.rs:637 that this call site
        // was leaving dead whenever a DnD grab was active. Do both.
        handle.cancel(data, seq);
        self.pending_drop = false;
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        _data: &mut D,
        _handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
        _event: &crate::input::touch::ShapeEvent,
        _seq: Serial,
    ) {
    }

    fn orientation(
        &mut self,
        _data: &mut D,
        _handle: &mut crate::input::touch::TouchInnerHandle<'_, D>,
        _event: &crate::input::touch::OrientationEvent,
        _seq: Serial,
    ) {
    }

    fn start_data(&self) -> &TouchGrabStartData<D> {
        self.touch_start_data.as_ref().unwrap()
    }

    fn unset(&mut self, data: &mut D) {
        // cosmix patch: unset from any teardown other than our own release must cancel.
        if self.pending_drop {
            self.drop(data);
        } else {
            self.cancel(data);
        }
    }
}

#[derive(Debug)]
struct OfferData {
    active: bool,
    dropped: bool,
    accepted: bool,
    finished: bool,
    chosen_action: DndAction,
}

#[derive(Debug)]
struct DndDataOffer {
    offer_data: Arc<Mutex<OfferData>>,
    source: WlDataSource,
}

impl<D> ObjectData<D> for DndDataOffer
where
    D: DataDeviceHandler,
    D: 'static,
{
    fn request(
        self: Arc<Self>,
        dh: &Handle,
        handler: &mut D,
        _client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<D>>> {
        let dh = DisplayHandle::from(dh.clone());
        if let Ok((resource, request)) = WlDataOffer::parse_request(&dh, msg) {
            handle_dnd(handler, &resource, request, &self);
        }

        None
    }

    fn destroyed(
        self: Arc<Self>,
        _handle: &Handle,
        _data: &mut D,
        _client_id: ClientId,
        _object_id: ObjectId,
    ) {
    }
}

fn handle_dnd<D>(handler: &mut D, offer: &WlDataOffer, request: wl_data_offer::Request, data: &DndDataOffer)
where
    D: DataDeviceHandler,
    D: 'static,
{
    use self::wl_data_offer::Request;
    let source = &data.source;
    let mut data = data.offer_data.lock().unwrap();
    match request {
        Request::Accept { mime_type, .. } => {
            if let Some(mtype) = mime_type {
                if let Err(crate::utils::UnmanagedResource) = with_source_metadata(source, |meta| {
                    data.accepted = meta.mime_types.contains(&mtype);
                }) {
                    data.accepted = false;
                }
            } else {
                data.accepted = false;
            }
        }
        Request::Receive { mime_type, fd } => {
            // check if the source and associated mime type is still valid
            let valid = with_source_metadata(source, |meta| meta.mime_types.contains(&mime_type))
                .unwrap_or(false)
                && source.alive()
                && data.active;
            if valid {
                source.send(mime_type, fd.as_fd());
            }
        }
        Request::Destroy => {
            if source.version() >= 3 && data.dropped && !data.finished {
                source.cancelled();
            }
        }
        Request::Finish => {
            if !data.active {
                offer.post_error(
                    wl_data_offer::Error::InvalidFinish,
                    "Cannot finish a data offer that is no longer active.",
                );
                return;
            }
            if !data.accepted {
                offer.post_error(
                    wl_data_offer::Error::InvalidFinish,
                    "Cannot finish a data offer that has not been accepted.",
                );
                return;
            }
            if !data.dropped {
                offer.post_error(
                    wl_data_offer::Error::InvalidFinish,
                    "Cannot finish a data offer that has not been dropped.",
                );
                return;
            }
            if data.chosen_action.is_empty() {
                offer.post_error(
                    wl_data_offer::Error::InvalidFinish,
                    "Cannot finish a data offer with no valid action.",
                );
                return;
            }
            source.dnd_finished();
            data.active = false;
            data.finished = true;
        }
        Request::SetActions {
            dnd_actions,
            preferred_action,
        } => {
            let dnd_actions = dnd_actions.into_result().unwrap_or(DndAction::None);
            let preferred_action = preferred_action.into_result().unwrap_or(DndAction::None);

            // preferred_action must only contain one bitflag at the same time
            if ![DndAction::None, DndAction::Move, DndAction::Copy, DndAction::Ask]
                .contains(&preferred_action)
            {
                offer.post_error(wl_data_offer::Error::InvalidAction, "Invalid preferred action.");
                return;
            }

            let source_actions =
                with_source_metadata(source, |meta| meta.dnd_action).unwrap_or_else(|_| DndAction::empty());
            let possible_actions = source_actions & dnd_actions;
            let chosen_action = handler.action_choice(possible_actions, preferred_action);
            // check that the user provided callback respects that one precise action should be chosen
            debug_assert!(
                [DndAction::None, DndAction::Move, DndAction::Copy, DndAction::Ask].contains(&chosen_action),
                "Only one precise action should be chosen"
            );
            if chosen_action != data.chosen_action {
                data.chosen_action = chosen_action;
                offer.action(chosen_action);
                source.action(chosen_action);
            }
        }
        _ => unreachable!(),
    }
}
