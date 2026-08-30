//! Backend-agnostic input ingress for the protocol thread.
//!
//! Rung E's requirement is not that bare-metal input *works* — it is that it
//! works *while the renderer is stalled*. The nested backend collects a
//! `Vec<HostInput>` and hands it over inside a Bevy frame command, so its input
//! latency is bounded by whatever the render schedule is doing; a blocked
//! `get_current_texture()` therefore delays every keystroke. On bare metal that
//! is unacceptable, because the key being delayed may be the one that ends the
//! session.
//!
//! So this module converts an [`InputEvent`] and dispatches it immediately from
//! the calloop callback, with no channel, no queue and no frame boundary in
//! between. The conversion is deliberately split from the dispatch:
//! [`host_input_from_event`] updates only per-device ingress bookkeeping while
//! converting an event into an ordered seat-operation batch, and
//! [`route_input_event`] does nothing but call it and hand each result to
//! [`WaylandState::handle_host_input`] — the one seat-policy entry point both
//! backends share.
//!
//! There is deliberately no test-only conversion, router or seat-policy path. A
//! fake [`InputBackend`] may emulate readiness and event production — that is
//! what a device does — but every `InputEvent<B>` it emits enters through the
//! same registered callback and the same [`route_input_event`] the libinput
//! backend will, which is what makes the offline coverage evidence about
//! production rather than about itself.
//!
//! Nothing registers a libinput source yet, and a compositor that opened
//! `/dev/input/event*` from a default `cargo test` run would forfeit the offline
//! discipline this ladder is built on. The anchors at the bottom of this file
//! keep the real production instantiation compiled and type-checked in the
//! meantime, without a lint silence and without touching a device.
//!
//! The libseat session that the opens go through stays on the KMS session thread
//! permanently — it is `!Send` by construction and cannot be moved here, which
//! an earlier version of this comment had wrong. E-4 built the adapter that
//! resolves it: `backend/libinput_live.rs` forwards only device open and close
//! to that thread by message, so libinput is still *constructed and polled*
//! here, where it must be. E-5 registers it.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    hash::{Hash, Hasher},
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use std::{sync::mpsc::SyncSender, time::Duration};

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
    InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    PointerMotionEvent, TouchEvent as _,
};
#[cfg(any(all(feature = "kms-live", not(test)), test))]
use smithay::reexports::calloop::channel;
use smithay::reexports::calloop::{EventSource, LoopHandle};
use smithay::utils::{Logical, Point};

use super::{HostAxis, HostButtonState, HostInput, WaylandState};

/// The held input contributions attributed to one device lifetime.
#[derive(Default)]
struct InputDeviceState {
    keys: HashSet<smithay::input::keyboard::Keycode>,
    buttons: HashSet<u32>,
    axes: HashSet<ActiveAxis>,
    /// The newest `time_msec` this device's own events have carried.
    ///
    /// Synthetic releases are stamped with this rather than the compositor's
    /// process-local clock: every timestamp the client has seen from this
    /// device is on the backend's own base, and `wl_pointer.axis_stop` in
    /// particular requires its time to be read on the same basis as the axis
    /// events that preceded it. A removal long after the last event therefore
    /// reuses that last timestamp — an equal time is still ordered; a value
    /// from a different base can regress. Held in delivery order (see
    /// `observe`), which is wrap-safe where a running maximum is not.
    last_event_time_msec: u32,
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    touch: bool,
}

/// One scroll sequence whose source promises a meaningful stop.
///
/// Smithay's vendored `AxisSource` does not implement `Hash`, so this wrapper
/// supplies the small stable hash needed by the per-device set without changing
/// that upstream public enum merely for downstream bookkeeping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveAxis {
    axis: Axis,
    source: AxisSource,
}

impl Hash for ActiveAxis {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.axis.hash(state);
        match self.source {
            AxisSource::Finger => 0_u8,
            AxisSource::Continuous => 1,
            AxisSource::Wheel => 2,
            AxisSource::WheelTilt => 3,
        }
        .hash(state);
    }
}

/// What the seat itself currently considers held.
///
/// Reconciliation intersects a departing device's bookkeeping with this, so a
/// press the seat has already forgotten is never released twice. Sampling it
/// clones two collections out from behind Smithay's locks, which is why it is
/// produced lazily and only for the one event class that needs it: every other
/// event class is on the ingress hot path this module exists to keep short.
///
/// The intersection proves the seat still holds the press — not that the
/// currently focused client received it. After a pointer focus retarget with a
/// button held (an unmapped surface's click grab is unset and focus re-hit-
/// tests onto whatever is underneath), the synthetic release reaches the new
/// focus exactly as the physical release would have; suppressing it instead
/// would leave the seat's pressed state stranded, which is the defect class
/// this reconciliation exists to close. Accepted deliberately in review.
pub(crate) struct SeatHeldState {
    pub(crate) keys: HashSet<smithay::input::keyboard::Keycode>,
    pub(crate) buttons: Vec<u32>,
}

/// Per-device input state retained before events lose their device identity.
///
/// This state never decides whether an ordinary event reaches the seat. It is
/// updated alongside conversion and is consulted only when a device disappears
/// (or all contributions must be released after input authority is lost).
#[derive(Default)]
pub(crate) struct InputIngressState {
    devices: HashMap<String, InputDeviceState>,
    /// Scroll sequences whose newest client-visible word is "moving".
    ///
    /// Keys and buttons intersect a departing device's bookkeeping against the
    /// seat's own held state; Smithay keeps no equivalent truth for scroll
    /// activity, so this set is it. An axis enters when a non-zero amount is
    /// forwarded and leaves when any device's zero stop — or a synthetic one —
    /// is, whichever device carried it: once the client has been told the axis
    /// stopped, a departing device that was still physically scrolling owes it
    /// nothing until fresh motion re-arms it.
    client_axes: HashSet<ActiveAxis>,
}

impl InputIngressState {
    fn added(&mut self, device: &impl Device) {
        let id = device.id();
        if let Some(_existing) = self.devices.get_mut(&id) {
            // Ids are unique among live devices, so a second add for one is a
            // duplicate announcement of the same lifetime — and `state_for`
            // deliberately tolerates an event arriving before its add, so the
            // record may legitimately predate this call. Replacing it would
            // discard held state and suppress the very releases removal owes;
            // id reuse across lifetimes is already safe because `removed`
            // deletes the record.
            #[cfg(any(all(feature = "kms-live", not(test)), test))]
            {
                _existing.touch = device.has_capability(DeviceCapability::Touch);
            }
            tracing::warn!(
                device_id = id,
                "input device id was added twice; keeping held state and refreshing capabilities"
            );
            return;
        }
        self.devices.insert(
            id,
            InputDeviceState {
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                touch: device.has_capability(DeviceCapability::Touch),
                ..InputDeviceState::default()
            },
        );
    }

    fn state_for(&mut self, device: &impl Device) -> &mut InputDeviceState {
        let id = device.id();
        self.devices.entry(id.clone()).or_insert_with(|| {
            tracing::debug!(
                device_id = id,
                "input event preceded device-added; creating ingress record"
            );
            InputDeviceState::default()
        })
    }

    /// Record that this device produced an event stamped `time_msec`, and hand
    /// back its record for whatever bookkeeping the event class needs.
    ///
    /// Assignment in delivery order, deliberately not `max`: a `u32` msec
    /// clock wraps every ~49.7 days, so `max` would pin the record at the
    /// pre-wrap value for the rest of the device's lifetime — and compositor
    /// uptimes past the wrap are ordinary. The anti-regression property `max`
    /// appeared to buy was illusory anyway: a backend that delivers slightly
    /// out of order already handed the client that regression through
    /// ordinary forwarding, so a synthetic release stamped with the
    /// last-delivered time is exactly as ordered as the stream itself.
    fn observe(&mut self, device: &impl Device, time_msec: u32) -> &mut InputDeviceState {
        let record = self.state_for(device);
        record.last_event_time_msec = time_msec;
        record
    }

    /// Take a departing device's record and reconcile what it was holding.
    ///
    /// The record is removed rather than emptied, so an id the backend later
    /// re-uses for a different device starts from nothing
    /// (`vendor/smithay/src/backend/input/mod.rs:20-24`).
    fn removed(&mut self, device: &impl Device, held: &SeatHeldState) -> Vec<HostInput> {
        let id = device.id();
        let Some(departing) = self.devices.remove(&id) else {
            tracing::debug!(device_id = id, "removed input device had no ingress record");
            return Vec::new();
        };
        self.releases_for(&departing, held)
    }

    fn releases_for(
        &mut self,
        departing: &InputDeviceState,
        held: &SeatHeldState,
    ) -> Vec<HostInput> {
        // Synthetic releases ride the departing device's own timestamp base;
        // see `InputDeviceState::last_event_time_msec`. A record that never
        // carried a timestamped event holds nothing, so the zero default is
        // never dispatched.
        let time = departing.last_event_time_msec;
        let SeatHeldState {
            keys: pressed_keys,
            buttons: pressed_buttons,
        } = held;
        let mut keys = departing
            .keys
            .iter()
            .copied()
            .filter(|key| {
                !self
                    .devices
                    .values()
                    .any(|device| device.keys.contains(key))
            })
            .filter(|key| pressed_keys.contains(key))
            .collect::<Vec<_>>();
        keys.sort_unstable_by_key(|key| key.raw());

        let mut buttons = departing
            .buttons
            .iter()
            .copied()
            .filter(|button| {
                !self
                    .devices
                    .values()
                    .any(|device| device.buttons.contains(button))
            })
            .filter(|button| pressed_buttons.contains(button))
            .collect::<Vec<_>>();
        buttons.sort_unstable();

        let mut releases = Vec::with_capacity(keys.len() + buttons.len() + departing.axes.len());
        // Removal order is a policy decision: reconcile keys, then buttons,
        // then scroll sequences. The caller appends touch removal last.
        releases.extend(keys.into_iter().map(|keycode| HostInput::Key {
            keycode,
            state: HostButtonState::Released,
            time,
        }));
        releases.extend(buttons.into_iter().map(|button| HostInput::PointerButton {
            button,
            state: HostButtonState::Released,
            time,
        }));

        for source in [AxisSource::Finger, AxisSource::Continuous] {
            // Both axes of one source stopping at the same instant belong in
            // the same `wl_pointer.frame`: separate frames would tell the
            // client one axis stopped while the other was still moving.
            // `pointer_axis` emits one frame per `HostInput::PointerAxis`, so
            // sharing the frame means sharing the event. Distinct sources stay
            // distinct events because `wl_pointer.axis_source` is per-frame.
            let stopped: Vec<Axis> = [Axis::Horizontal, Axis::Vertical]
                .into_iter()
                .filter(|&axis| {
                    let active = ActiveAxis { axis, source };
                    departing.axes.contains(&active)
                        && !self
                            .devices
                            .values()
                            .any(|device| device.axes.contains(&active))
                        && self.client_axes.contains(&active)
                })
                .collect();
            if stopped.is_empty() {
                continue;
            }
            // Truth-maintenance: the client's newest word is now "stopped".
            // Its absence is currently unobservable — every device-set insert
            // re-arms `client_axes` in the same statement, so a stale armed
            // entry can never be consulted while wrong — which makes the
            // mutation deleting this loop a declared-equivalent survivor in
            // the sweep, kept because code reading `client_axes` tomorrow
            // deserves state that is true today.
            for &axis in &stopped {
                self.client_axes.remove(&ActiveAxis { axis, source });
            }
            let stop = HostAxis {
                amount: 0.0,
                v120: None,
            };
            releases.push(HostInput::PointerAxis {
                horizontal: stopped.contains(&Axis::Horizontal).then_some(stop),
                vertical: stopped.contains(&Axis::Vertical).then_some(stop),
                source,
                // A named limitation, not an oversight: the bookkeeping
                // records that an axis is active, not which relative
                // direction the departing device was reporting, so a stop
                // for a naturally-scrolling device claims `Identical`. The
                // value is inert — Smithay emits `axis_relative_direction`
                // only for a non-zero axis value, and a stop's value is 0.0 —
                // and carrying it would mean tracking a per-axis direction
                // that nothing else reads.
                relative_direction: (
                    smithay::backend::input::AxisRelativeDirection::Identical,
                    smithay::backend::input::AxisRelativeDirection::Identical,
                ),
                time,
            });
        }
        releases
    }

    /// Reconcile every device lifetime after the whole input authority is lost.
    ///
    /// Each record is still removed through [`Self::releases_for`], exactly as
    /// an ordinary `DeviceRemoved` event is. The only extra work here is
    /// deterministic batching: releases from several devices are grouped in
    /// the established key, button, axis, touch order before they enter the
    /// compositor's one seat-policy path.
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn all_devices_lost_authority(&mut self, held: &SeatHeldState) -> Vec<HostInput> {
        let mut device_ids = self.devices.keys().cloned().collect::<Vec<_>>();
        device_ids.sort();
        let touch_devices = device_ids
            .iter()
            .filter(|id| self.devices.get(*id).is_some_and(|device| device.touch))
            .count();
        let mut keys = Vec::new();
        let mut buttons = Vec::new();
        let mut axes = Vec::new();
        for id in device_ids {
            let departing = self
                .devices
                .remove(&id)
                .expect("the all-device reconciliation collected a live id");
            for input in self.releases_for(&departing, held) {
                match input {
                    HostInput::Key { .. } => keys.push(input),
                    HostInput::PointerButton { .. } => buttons.push(input),
                    HostInput::PointerAxis { .. } => axes.push(input),
                    _ => unreachable!("per-device reconciliation emits only held releases"),
                }
            }
        }
        keys.extend(buttons);
        keys.extend(axes);
        // The established `TouchDeviceRemoved` dispatch cancels the live touch
        // sequence before updating the capability count. Reuse that operation
        // rather than manufacturing a pause-only cancellation path here.
        keys.extend((0..touch_devices).map(|_| HostInput::TouchDeviceRemoved));
        keys
    }
}

/// What one backend event means to the seat.
///
/// `Ignored` carries a reason rather than being a bare `None` so an event that
/// produces no seat operation shows up as a named gap in a log rather than as
/// silence that reads exactly like a device sending nothing. Two distinct
/// things reach it: an event class this rung does not claim, and a claimed
/// event that genuinely reconciled nothing. Both are worth a reason string;
/// neither is worth a dispatch.
#[derive(Clone, Debug)]
pub(crate) enum InputRouting {
    Deliver(Vec<HostInput>),
    Ignored(&'static str),
}

impl InputRouting {
    fn deliver(input: HostInput) -> Self {
        Self::Deliver(vec![input])
    }
}

/// Convert one backend event into the compositor's own seat-command batch.
///
/// `output_extent` is the coordinate space absolute devices are transformed
/// into — a touchscreen or tablet reports a normalised position, which is
/// meaningless until it is scaled by the output it is bonded to.
///
/// `seat_held` is a thunk rather than a value on purpose. Only `DeviceRemoved`
/// reconciliation consults the seat's held state, and sampling it clones a
/// `HashSet` and a `Vec` out from behind Smithay's locks; making every key,
/// motion and axis event pay for that would put an allocation pair on the one
/// path this module exists to keep free of avoidable work.
pub(crate) fn host_input_from_event<B: InputBackend>(
    ingress: &mut InputIngressState,
    event: &InputEvent<B>,
    output_extent: (u32, u32),
    seat_held: impl FnOnce() -> SeatHeldState,
) -> InputRouting {
    match event {
        InputEvent::Keyboard { event } => {
            let record = ingress.observe(&event.device(), event.time_msec());
            match event.state() {
                KeyState::Pressed => {
                    record.keys.insert(event.key_code());
                }
                KeyState::Released => {
                    record.keys.remove(&event.key_code());
                }
            }
            InputRouting::deliver(HostInput::Key {
                // Already an XKB keycode: Smithay's libinput backend applies the
                // evdev offset. See `HostInput::key_from_evdev`.
                keycode: event.key_code(),
                state: host_key_state(event.state()),
                time: event.time_msec(),
            })
        }
        InputEvent::PointerMotion { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::deliver(HostInput::PointerMotion {
                dx: event.delta_x(),
                dy: event.delta_y(),
                time: event.time_msec(),
            })
        }
        InputEvent::PointerMotionAbsolute { event } => {
            ingress.observe(&event.device(), event.time_msec());
            let position = transform_absolute::<B>(event, output_extent);
            InputRouting::deliver(HostInput::PointerMotionAbsolute {
                x: position.x,
                y: position.y,
                time: event.time_msec(),
            })
        }
        InputEvent::PointerButton { event } => {
            let record = ingress.observe(&event.device(), event.time_msec());
            match event.state() {
                ButtonState::Pressed => {
                    record.buttons.insert(event.button_code());
                }
                ButtonState::Released => {
                    record.buttons.remove(&event.button_code());
                }
            }
            InputRouting::deliver(HostInput::PointerButton {
                button: event.button_code(),
                state: host_button_state(event.state()),
                time: event.time_msec(),
            })
        }
        InputEvent::PointerAxis { event } => {
            let device = event.device();
            let source = event.source();
            ingress.observe(&device, event.time_msec());
            if matches!(source, AxisSource::Finger | AxisSource::Continuous) {
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let Some(amount) = event.amount(axis) else {
                        continue;
                    };
                    let active = ActiveAxis { axis, source };
                    if amount == 0.0 {
                        ingress.state_for(&device).axes.remove(&active);
                        ingress.client_axes.remove(&active);
                    } else {
                        ingress.state_for(&device).axes.insert(active);
                        ingress.client_axes.insert(active);
                    }
                }
            }
            InputRouting::deliver(HostInput::PointerAxis {
                horizontal: host_axis::<B>(event, Axis::Horizontal, source),
                vertical: host_axis::<B>(event, Axis::Vertical, source),
                source,
                relative_direction: (
                    event.relative_direction(Axis::Horizontal),
                    event.relative_direction(Axis::Vertical),
                ),
                time: event.time_msec(),
            })
        }
        // Arrival changes no seat state for a keyboard or a pointer: those are
        // created once with the compositor and are not conditional on a device
        // existing. A touch device is different — `wl_seat.capabilities` only
        // carries the touch bit while one is attached — so its arrival is a real
        // seat command. Non-touch arrivals are still reported rather than
        // silently dropped, so a log distinguishes "no devices" from "devices
        // never enumerated".
        InputEvent::DeviceAdded { device } => {
            ingress.added(device);
            if device.has_capability(DeviceCapability::Touch) {
                InputRouting::deliver(HostInput::TouchDeviceAdded)
            } else {
                InputRouting::Ignored("input device added")
            }
        }
        InputEvent::DeviceRemoved { device } => {
            let mut inputs = ingress.removed(device, &seat_held());
            if device.has_capability(DeviceCapability::Touch) {
                // Touch removal is last by decision, after key, button and axis
                // reconciliation from a multi-capability device.
                inputs.push(HostInput::TouchDeviceRemoved);
            }
            if inputs.is_empty() {
                // The record was consumed either way; an idle device leaving is
                // a reason, not silence.
                InputRouting::Ignored("input device removed; nothing held to reconcile")
            } else {
                InputRouting::Deliver(inputs)
            }
        }
        // Touch coordinates are normalised by the device and meaningless until
        // scaled, exactly like absolute pointer motion, so they go through the
        // same transform against the same extent.
        InputEvent::TouchDown { event } => {
            ingress.observe(&event.device(), event.time_msec());
            let position = transform_absolute::<B>(event, output_extent);
            InputRouting::deliver(HostInput::TouchDown {
                slot: event.slot(),
                x: position.x,
                y: position.y,
                time: event.time_msec(),
            })
        }
        InputEvent::TouchMotion { event } => {
            ingress.observe(&event.device(), event.time_msec());
            let position = transform_absolute::<B>(event, output_extent);
            InputRouting::deliver(HostInput::TouchMotion {
                slot: event.slot(),
                x: position.x,
                y: position.y,
                time: event.time_msec(),
            })
        }
        InputEvent::TouchUp { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::deliver(HostInput::TouchUp {
                slot: event.slot(),
                time: event.time_msec(),
            })
        }
        // The slot a cancel arrives on is deliberately discarded: cancellation
        // is not per-contact. `wl_touch.cancel` ends the client's whole touch
        // session, and Smithay's `TouchHandle::cancel` takes no slot to match.
        InputEvent::TouchCancel { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::deliver(HostInput::TouchCancel)
        }
        InputEvent::TouchFrame { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::deliver(HostInput::TouchFrame)
        }
        // Tablets, gestures and switches are not claimed by this rung. Naming
        // them one by one rather than behind a catch-all is what keeps a newly
        // supported event class from being swallowed by an arm that was written
        // before it existed.
        InputEvent::GestureSwipeBegin { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::GestureSwipeUpdate { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::GestureSwipeEnd { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::GesturePinchBegin { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::GesturePinchUpdate { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::GesturePinchEnd { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::GestureHoldBegin { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::GestureHoldEnd { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("pointer gestures are not supported yet")
        }
        InputEvent::TabletToolAxis { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("tablet tools are not supported yet")
        }
        InputEvent::TabletToolProximity { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("tablet tools are not supported yet")
        }
        InputEvent::TabletToolTip { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("tablet tools are not supported yet")
        }
        InputEvent::TabletToolButton { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("tablet tools are not supported yet")
        }
        InputEvent::SwitchToggle { event } => {
            ingress.observe(&event.device(), event.time_msec());
            InputRouting::Ignored("switches are not supported yet")
        }
        InputEvent::Special(_) => InputRouting::Ignored("backend-specific event"),
    }
}

/// Dispatch one backend event to the seat, now.
///
/// This is the whole bare-metal ingress. Anything added between the conversion
/// and `handle_host_input` — a queue, a channel, an ECS resource, a frame
/// boundary — reintroduces exactly the latency this rung exists to exclude.
///
/// The oracle for that property is
/// `injected_key_reaches_a_focused_client_while_acquire_is_blocked`. It reads
/// one exact `wl_keyboard.key` off a real client socket while the render path
/// is parked inside `acquire_output_frames`, with no `finish_frame` and no
/// `wl_display.sync` — both of which a frame-bounded input queue would still
/// satisfy, which is why the pre-existing blocked-acquire tests are not
/// evidence about input latency and are not claimed here as such. Everything in
/// that test before the blocked read is deliberately reachable with a queue in
/// place, so the defect it rejects is exactly this one: a `pending_kms_input`
/// queue drained at `handle_frame` leaves it the only assertion that fails.
pub(crate) fn route_input_event<B: InputBackend>(state: &mut WaylandState, event: InputEvent<B>) {
    let keyboard = &state.keyboard;
    let pointer = &state.pointer;
    let routing = host_input_from_event(
        &mut state.input_ingress,
        &event,
        state.backend.seat_extent(),
        || SeatHeldState {
            keys: keyboard.pressed_keys(),
            buttons: pointer.current_pressed(),
        },
    );
    match routing {
        InputRouting::Deliver(inputs) => {
            for input in inputs {
                state.handle_host_input(input);
            }
        }
        InputRouting::Ignored(reason) => {
            tracing::trace!(reason, "input event carries no seat operation");
        }
    }
}

/// Read one axis of a scroll event, or `None` if the device did not report it.
///
/// Each axis is read on its own because libinput reports them on their own: an
/// ordinary vertical wheel event has no horizontal axis, and both `amount` and
/// `amount_v120` return `None` for it
/// (`vendor/smithay/src/backend/libinput/mod.rs:170-219`). Reading the pair
/// together and requiring both — the shape this function replaced — threw away
/// the vertical v120 of every ordinary scroll because the horizontal one was
/// absent.
///
/// `PointerAxisEvent` guarantees `amount` only for finger and continuous
/// devices and `amount_v120` only for wheels, so a wheel read through `amount`
/// alone can legitimately be `None` — and a `None` treated as zero is a scroll
/// that silently does nothing. The fallback converts detents back into the
/// units `wl_pointer.axis` carries, at the 15-per-step scale the nested backend
/// already uses for a wheel line.
/// Scale a normalised absolute position into compositor coordinates.
///
/// Shared by absolute pointer motion and by touch rather than written out at
/// each site, because the two must use the *same* extent. A touchscreen and a
/// tablet report the same `[0, 1]` range, so a disagreement about what to
/// multiply it by does not fail — it silently maps the same physical spot to
/// two different surfaces depending on which device the user touched it with.
///
/// The extent is `seat_extent()`, the bounding box of the seat, and not the
/// confinement region: an absolute device addresses the whole seat, and the
/// caller clamps afterwards.
fn transform_absolute<B: InputBackend>(
    event: &impl AbsolutePositionEvent<B>,
    output_extent: (u32, u32),
) -> Point<f64, Logical> {
    event.position_transformed(
        (
            i32::try_from(output_extent.0).unwrap_or(i32::MAX),
            i32::try_from(output_extent.1).unwrap_or(i32::MAX),
        )
            .into(),
    )
}

fn host_axis<B: InputBackend>(
    event: &B::PointerAxisEvent,
    axis: Axis,
    source: AxisSource,
) -> Option<HostAxis> {
    let v120 = event.amount_v120(axis);
    let amount = match event.amount(axis) {
        Some(amount) => Some(amount),
        None => match source {
            AxisSource::Wheel | AxisSource::WheelTilt => {
                v120.map(|v120| v120 / 120.0 * WHEEL_STEP_AMOUNT)
            }
            // Never fabricate an amount for a finger or continuous device. A
            // reported zero is that axis's stop, so an invented one would
            // suppress the `wl_pointer.axis_stop` a client waits on; and an
            // axis the device never reported must stay absent rather than
            // become a stop it never sent.
            AxisSource::Finger | AxisSource::Continuous => None,
        },
    };
    Some(HostAxis {
        amount: amount?,
        v120: v120.map(|v120| v120 as i32),
    })
}

/// One wheel detent in `wl_pointer.axis` units, matching the nested backend's
/// `MouseScrollUnit::Line` scale so a client cannot tell the transports apart.
const WHEEL_STEP_AMOUNT: f64 = 15.0;

fn host_button_state(state: ButtonState) -> HostButtonState {
    match state {
        ButtonState::Pressed => HostButtonState::Pressed,
        ButtonState::Released => HostButtonState::Released,
    }
}

fn host_key_state(state: KeyState) -> HostButtonState {
    match state {
        KeyState::Pressed => HostButtonState::Pressed,
        KeyState::Released => HostButtonState::Released,
    }
}

/// Something the protocol thread can turn into a registered input source.
///
/// The generics are erased at this boundary and nowhere else. `ProtocolServer`
/// is not generic and must not become so to carry one input backend, but
/// `insert_source` needs the concrete `EventSource` type — so the type
/// parameter is closed over inside [`InputSourceFactory`] and only `register`
/// crosses the boundary.
///
/// **A source, not a factory, was the first shape tried, and it does not
/// compile.** `LibinputInputBackend` is not `Send`: it owns a raw
/// `*mut libinput` and an `Rc<dyn LibinputInterface>` (`input-0.9.1`
/// `context.rs`). So a source cannot be built on the calling thread and moved
/// here — the production backend must be *constructed* on the thread that will
/// poll it. That is a constraint the compiler enforces rather than a
/// convention, and it is why the `Send` bound sits on the factory closure while
/// the source it returns needs no such bound.
pub(crate) trait InputSourceRegistration: Send {
    fn register(
        self: Box<Self>,
        handle: &LoopHandle<'static, WaylandState>,
    ) -> Result<(), Box<dyn Error>>;
}

/// A closure that builds one input backend on the protocol thread.
///
/// The backend must also be its own calloop source. That is not a convenience:
/// it is the shape `LibinputInputBackend` already has
/// (`vendor/smithay/src/backend/libinput/mod.rs`, `impl EventSource` with
/// `Event = InputEvent<LibinputInputBackend>`), so requiring it means the
/// production backend needs no adapter, and a test backend satisfying the same
/// bound exercises the same registration rather than a parallel one.
pub(crate) struct InputSourceFactory<F>(pub(crate) F);

impl<F, B> InputSourceRegistration for InputSourceFactory<F>
where
    F: FnOnce() -> Result<B, Box<dyn Error + Send + Sync>> + Send + 'static,
    B: InputBackend + EventSource<Event = InputEvent<B>, Metadata = (), Ret = ()> + 'static,
{
    /// Build the source here, then register it with the one callback this
    /// module allows.
    ///
    /// The callback body is a single call to [`route_input_event`], and that is
    /// the whole point of the bound: nothing about the source gets to choose
    /// what an event means to the seat. A source decides *when* an event exists
    /// — that is what a device does — but the path from there to
    /// [`WaylandState::handle_host_input`] is the same for every backend.
    fn register(
        self: Box<Self>,
        handle: &LoopHandle<'static, WaylandState>,
    ) -> Result<(), Box<dyn Error>> {
        // Not `?`: there is no blanket conversion from a `Send + Sync` boxed
        // error into a plain one, and widening it by hand keeps the factory's
        // error `Send` — which it must be, since the factory itself crosses a
        // thread boundary.
        let source = (self.0)().map_err(|error| -> Box<dyn Error> { error })?;
        handle
            .insert_source(source, |event, (), state| route_input_event(state, event))
            .map_err(|error| -> Box<dyn Error> { error.to_string().into() })?;
        Ok(())
    }
}

/// An input backend whose authority can be controlled from another source on
/// the same protocol event loop.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) trait LifecycleInputBackend: InputBackend {
    type Control: InputLifecycleControl + 'static;

    fn lifecycle_control(&self) -> Self::Control;
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) trait InputLifecycleControl {
    fn suspend(&mut self);
    fn resume(&mut self) -> Result<(), String>;
}

#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) struct LibinputLifecycleControl(smithay::reexports::input::Libinput);

#[cfg(all(feature = "kms-live", not(test)))]
impl InputLifecycleControl for LibinputLifecycleControl {
    fn suspend(&mut self) {
        self.0.suspend();
    }

    fn resume(&mut self) -> Result<(), String> {
        self.0
            .resume()
            .map_err(|()| "libinput resume failed".to_string())
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LifecycleInputBackend for smithay::backend::libinput::LibinputInputBackend {
    type Control = LibinputLifecycleControl;

    fn lifecycle_control(&self) -> Self::Control {
        LibinputLifecycleControl(self.context().clone())
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
enum InputLifecycleCommand {
    ReconcileAndSuspend {
        acknowledgement: SyncSender<Result<(), String>>,
    },
    Resume {
        acknowledgement: SyncSender<Result<(), String>>,
    },
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone)]
pub(crate) struct InputLifecycleClient {
    commands: channel::Sender<InputLifecycleCommand>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl InputLifecycleClient {
    fn request(
        &self,
        command: impl FnOnce(SyncSender<Result<(), String>>) -> InputLifecycleCommand,
        timeout: Duration,
        operation: &'static str,
    ) -> Result<(), String> {
        let (acknowledgement, reply) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(command(acknowledgement))
            .map_err(|_| format!("input lifecycle source disconnected before {operation}"))?;
        reply
            .recv_timeout(timeout)
            .map_err(|error| format!("input {operation} acknowledgement failed: {error}"))?
    }

    pub(crate) fn reconcile_and_suspend(&self, timeout: Duration) -> Result<(), String> {
        self.request(
            |acknowledgement| InputLifecycleCommand::ReconcileAndSuspend { acknowledgement },
            timeout,
            "suspend",
        )
    }

    pub(crate) fn resume(&self, timeout: Duration) -> Result<(), String> {
        self.request(
            |acknowledgement| InputLifecycleCommand::Resume { acknowledgement },
            timeout,
            "resume",
        )
    }
}

/// Registration that retains a control clone beside the ordinary input source.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) struct LifecycleInputSourceFactory<F> {
    factory: F,
    lifecycle: channel::Channel<InputLifecycleCommand>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) fn lifecycle_input_source<F>(
    factory: F,
) -> (LifecycleInputSourceFactory<F>, InputLifecycleClient) {
    let (commands, lifecycle) = channel::channel();
    (
        LifecycleInputSourceFactory { factory, lifecycle },
        InputLifecycleClient { commands },
    )
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl<F, B> InputSourceRegistration for LifecycleInputSourceFactory<F>
where
    F: FnOnce() -> Result<B, Box<dyn Error + Send + Sync>> + Send + 'static,
    B: LifecycleInputBackend
        + EventSource<Event = InputEvent<B>, Metadata = (), Ret = ()>
        + 'static,
{
    fn register(
        self: Box<Self>,
        handle: &LoopHandle<'static, WaylandState>,
    ) -> Result<(), Box<dyn Error>> {
        let source = (self.factory)().map_err(|error| -> Box<dyn Error> { error })?;
        let mut control = source.lifecycle_control();
        handle
            .insert_source(source, |event, (), state| route_input_event(state, event))
            .map_err(|error| -> Box<dyn Error> { error.to_string().into() })?;
        handle
            .insert_source(self.lifecycle, move |event, (), state| {
                let channel::Event::Msg(command) = event else {
                    return;
                };
                match command {
                    InputLifecycleCommand::ReconcileAndSuspend { acknowledgement } => {
                        state.reconcile_all_input_authority_loss();
                        control.suspend();
                        let _ = acknowledgement.send(Ok(()));
                    }
                    InputLifecycleCommand::Resume { acknowledgement } => {
                        let _ = acknowledgement.send(control.resume());
                    }
                }
            })
            .map_err(|error| -> Box<dyn Error> { error.to_string().into() })?;
        Ok(())
    }
}

/// Keep the production instantiation compiled while the source that will drive
/// it is still on the wrong thread.
///
/// The tests exercise these functions through a fake backend, which proves the
/// logic but not that the generic parameter Rung E-5 will actually pass —
/// `LibinputInputBackend` — satisfies it. Naming the specialisation here
/// type-checks that today. It also keeps every function in this module
/// reachable, which is the honest way to have no dead code, as opposed to an
/// `#[expect(dead_code)]`: `unfulfilled_lint_expectations` is warn-by-default
/// and this crate does not deny warnings, so such an expectation would have been
/// a comment claiming to be a gate.
///
/// It is a type ascription and nothing else. No value is constructed, no
/// function is called, and no device is opened, in a normal build or under
/// `cargo test`.
const _: fn(&mut WaylandState, InputEvent<smithay::backend::libinput::LibinputInputBackend>) =
    route_input_event::<smithay::backend::libinput::LibinputInputBackend>;

/// The same for the registration path, which is the part E-5 actually calls.
///
/// The router anchor above proves `LibinputInputBackend` satisfies
/// `InputBackend`; this proves it also satisfies the `EventSource` half of
/// [`InputSourceRegistration`]'s bound, which is the claim that would otherwise
/// rest on a test backend written to fit. Without it a `FakeInput` satisfying
/// the bound would be evidence only about `FakeInput` — the fake could be
/// adjusted until it compiled and nothing would check that the production
/// backend still fit.
///
/// It earned its place immediately: the first version of this seam took the
/// source by value and required it to be `Send`, which every test backend
/// satisfies and `LibinputInputBackend` does not. This anchor is what said so,
/// at compile time, before the shape could be built on.
type LibinputFactory =
    fn() -> Result<smithay::backend::libinput::LibinputInputBackend, Box<dyn Error + Send + Sync>>;

/// The signature of [`InputSourceRegistration::register`] for one implementor.
///
/// Spelled once, as a name, so the anchor below is a comparison rather than a
/// second transcription of the trait method: a signature written out twice can
/// drift from the trait and still compile, because the ascription would then be
/// checking the copy.
type RegisterFn<S> = fn(Box<S>, &LoopHandle<'static, WaylandState>) -> Result<(), Box<dyn Error>>;

const _: RegisterFn<InputSourceFactory<LibinputFactory>> =
    <InputSourceFactory<LibinputFactory> as InputSourceRegistration>::register;

/// The factory shape the live adapter actually produces.
///
/// A bare `fn()` pointer, as anchored above, cannot capture anything — and the
/// live adapter must capture a command sender and a seat name. So it hands over
/// a boxed closure instead, and that is a different `F`: `Box<dyn FnOnce…>`
/// implements `FnOnce`, but only because of a blanket impl that a signature
/// change could remove without touching this crate.
///
/// Named here rather than in `backend/kms_live.rs` because the anchor below has
/// to mention [`WaylandState`], which does not leave this module. Naming it once
/// here and importing it there is what keeps the type private and the check
/// real, instead of widening `WaylandState` to `pub(crate)` for the sake of a
/// compile-time assertion.
pub(crate) type BoxedLibinputFactory = Box<
    dyn FnOnce() -> Result<
            smithay::backend::libinput::LibinputInputBackend,
            Box<dyn Error + Send + Sync>,
        > + Send
        + 'static,
>;

const _: RegisterFn<InputSourceFactory<BoxedLibinputFactory>> =
    <InputSourceFactory<BoxedLibinputFactory> as InputSourceRegistration>::register;
