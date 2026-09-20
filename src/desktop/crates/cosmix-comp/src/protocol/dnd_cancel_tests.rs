// Exercise real wire offers, including acceptance and action negotiation.
//
// This precondition is LOAD-BEARING, not cosmetic. `DnDGrab::drop` (and its
// `ServerDnDGrab` twin) gates delivery of `wl_data_device.drop` on
// `validated = data.accepted && !data.chosen_action.is_empty()`:
// when `validated` is false, `drop()` ALREADY sends no `wl_data_device.drop`
// and ALREADY calls `source.cancelled()` — even before the fix. A drag
// whose destination never accepted an offer or never negotiated an action
// would make `dnd_session_lock_cancels_without_drop`'s
// assertions true before AND after the fix, proving nothing. So every test
// below must start from a drag that is accepted with a non-empty chosen
// action, and this helper asserts that precondition on the wire (not just
// assumes it) before returning.
fn start_accepted_pointer_drag(harness: &mut KeybindingHarness) -> (u32, u32, u32) {
    start_accepted_client_drag(harness, false)
}

fn begin_drag_contact(harness: &mut KeybindingHarness, touch: bool) -> u32 {
    map_initial_test_toplevel(harness);
    if touch {
        harness.route(InputEvent::DeviceAdded {
            device: FakeDevice::Touchscreen,
        });
        let object = harness.allocate_object_id();
        send_request(&mut harness.client, TEST_SEAT_ID, 2, &words(&[object]));
        let _ = harness.sync();
        touch_drag_down(harness, 0);
        let traffic = harness.sync();
        let down = traffic
            .iter()
            .find(|(id, opcode, _)| *id == object && *opcode == 0)
            .unwrap_or_else(|| panic!("initiating touch must reach the client: {traffic:?}"));
        assert_eq!(word(&down.2, 2), TEST_TOPLEVEL_SURFACE_ID);
        assert_eq!(word(&down.2, 3), 0);
        let serial = word(&down.2, 0);
        assert!(
            harness
                .server
                .state
                .seat
                .get_touch()
                .unwrap()
                .has_grab(serial.into())
        );
        assert!(!harness.server.state.pointer.is_grabbed());
        return serial;
    }
    let pointer = harness.bind_pointer();
    harness.prime_pointer_focus();
    route_pointer_button(harness, PRIMARY_POINTER_BUTTON, ButtonState::Pressed);
    let pressed = harness.sync();
    word(&pointer_body(&pressed, pointer, 3), 0)
}

fn touch_drag_down(harness: &mut KeybindingHarness, slot: u32) {
    let record = test_toplevel_record(harness);
    let normalised_x = (f64::from(record.layout.x) + 8.0) / 320.0;
    let normalised_y = (f64::from(record.layout.y) + 8.0) / 240.0;
    harness.route(InputEvent::TouchDown {
        event: FakeTouchPositionEvent {
            slot: TouchSlot::from(Some(slot)),
            normalised_x,
            normalised_y,
            time_us: FAKE_EVENT_TIME_US,
        },
    });
}

fn touch_drag_up(harness: &mut KeybindingHarness, slot: u32) {
    harness.route(InputEvent::TouchUp {
        event: FakeTouchSlotEvent {
            slot: TouchSlot::from(Some(slot)),
            time_us: FAKE_EVENT_TIME_US,
        },
    });
}

fn move_drag_contact(harness: &mut KeybindingHarness, touch: bool) {
    if touch {
        let record = test_toplevel_record(harness);
        let normalised_x = (f64::from(record.layout.x) + 9.0) / 320.0;
        let normalised_y = (f64::from(record.layout.y) + 9.0) / 240.0;
        harness.route(InputEvent::TouchMotion {
            event: FakeTouchPositionEvent {
                slot: TouchSlot::from(Some(0)),
                normalised_x,
                normalised_y,
                time_us: FAKE_EVENT_TIME_US,
            },
        });
    } else {
        harness.route(InputEvent::PointerMotionAbsolute {
            event: FakeAbsoluteEvent {
                normalised_x: 0.11,
                normalised_y: 0.11,
            },
        });
    }
}

fn start_accepted_client_drag(harness: &mut KeybindingHarness, touch: bool) -> (u32, u32, u32) {
    let serial = begin_drag_contact(harness, touch);

    let manager = harness.bind_test_global("wl_data_device_manager", 3);
    let device = harness.allocate_object_id();
    let source = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        manager,
        1,
        &words(&[device, TEST_SEAT_ID]),
    );
    send_request(&mut harness.client, manager, 0, &words(&[source]));
    send_request(
        &mut harness.client,
        source,
        0,
        &wire_string_argument("text/plain"),
    );
    // wl_data_source.set_actions(Copy).
    send_request(&mut harness.client, source, 2, &words(&[1]));
    send_request(
        &mut harness.client,
        device,
        0,
        &words(&[source, TEST_TOPLEVEL_SURFACE_ID, 0, serial]),
    );
    let mut entered = harness.sync();
    // DnD focus is established by motion after start_drag, independently of
    // the implicit pointer/touch grab used to authorise that request.
    move_drag_contact(harness, touch);
    entered.extend(harness.sync());
    let offer = accept_drag_offer(harness, device, Some(source), &entered);
    assert_drag_grab(harness, touch, true);
    (device, source, offer)
}

fn assert_drag_grab(harness: &KeybindingHarness, touch: bool, expected: bool) {
    let grabbed = if touch {
        harness.server.state.seat.get_touch().unwrap().is_grabbed()
    } else {
        harness.server.state.pointer.is_grabbed()
    };
    assert_eq!(grabbed, expected, "drag grab lifetime (touch={touch})");
}

fn start_accepted_server_drag(harness: &mut KeybindingHarness, touch: bool) -> (u32, u32) {
    use smithay::reexports::wayland_server::protocol::wl_data_device_manager::DndAction;
    use smithay::wayland::selection::data_device::{SourceMetadata, start_dnd};

    let serial = begin_drag_contact(harness, touch);
    let manager = harness.bind_test_global("wl_data_device_manager", 3);
    let device = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        manager,
        1,
        &words(&[device, TEST_SEAT_ID]),
    );
    let _ = harness.sync();

    let seat = harness.server.state.seat.clone();
    let dh = harness.server.state.display_handle.clone();
    // start_dnd is Smithay's production API for compositor-owned sources;
    // there is deliberately no client start_drag request or wl_data_source.
    // Reuse real implicit-grab data, including its actual focus and contact.
    let (pointer_start, touch_start) = if touch {
        let start = seat
            .get_touch()
            .unwrap()
            .grab_start_data()
            .expect("implicit touch grab");
        assert!(start.focus.is_some());
        assert_eq!(start.slot, TouchSlot::from(Some(0)));
        (None, Some(start))
    } else {
        let start = seat
            .get_pointer()
            .unwrap()
            .grab_start_data()
            .expect("implicit pointer grab");
        assert!(start.focus.is_some());
        assert_eq!(start.button, PRIMARY_POINTER_BUTTON);
        (Some(start), None)
    };
    start_dnd(
        &dh,
        &seat,
        &mut harness.server.state,
        serial.into(),
        pointer_start,
        touch_start,
        SourceMetadata {
            mime_types: vec!["text/plain".into()],
            dnd_action: DndAction::Copy,
        },
    );
    let mut entered = harness.sync();
    move_drag_contact(harness, touch);
    entered.extend(harness.sync());
    // This asserts a real enter, announced offer, advertised MIME and Copy
    // action on the destination wire, not merely successful grab installation.
    let offer = accept_drag_offer(harness, device, None, &entered);
    assert_drag_grab(harness, touch, true);
    (device, offer)
}

fn accept_drag_offer(
    harness: &mut KeybindingHarness,
    device: u32,
    source: Option<u32>,
    entered: &[(u32, u16, Vec<u8>)],
) -> u32 {
    let enter = entered
        .iter()
        .find(|(object, opcode, _)| *object == device && *opcode == 1)
        .unwrap_or_else(|| panic!("drag must enter its target: {entered:?}"));
    assert_eq!(word(&enter.2, 1), TEST_TOPLEVEL_SURFACE_ID);
    let offer = word(&enter.2, 4);
    assert_ne!(offer, 0, "drag must carry a real data offer");
    assert!(
        entered
            .iter()
            .any(|(object, opcode, body)| *object == device
                && *opcode == 0
                && word(body, 0) == offer),
        "offer must be announced: {entered:?}"
    );
    assert!(
        entered.iter().any(|(object, opcode, body)| *object == offer
            && *opcode == 0
            && *body == wire_string_argument("text/plain")),
        "offer must advertise text/plain: {entered:?}"
    );
    let mut accept = words(&[word(&enter.2, 0)]);
    accept.extend(wire_string_argument("text/plain"));
    send_request(&mut harness.client, offer, 0, &accept);
    // wl_data_offer.set_actions(Copy, Copy).
    send_request(&mut harness.client, offer, 4, &words(&[1, 1]));
    let negotiated = harness.sync();
    // LOAD-BEARING for every caller: Accept checks this advertised MIME in
    // dnd_grab.rs:641-650 / server_dnd_grab.rs:605-611 and sets accepted=true.
    // Same-client request ordering processes it before SetActions. The action
    // event proves chosen_action=Copy (client:725-727 / server:675-677).
    // Action negotiation itself does NOT imply acceptance; the matching MIME
    // and ordered Accept above establish that separate half of validated.
    // Thus both drop gates (client:302-305 / server:252-255) are true before
    // teardown. Client drags additionally confirm the source action event;
    // server drags have no wl_data_source, only the real destination offer.
    if let Some(source) = source {
        assert!(
            negotiated.iter().any(|(object, opcode, body)| {
                *object == source && *opcode == 5 && word(body, 0) == 1
            }),
            "source must observe its action negotiated to Copy before teardown \
         (proves chosen_action is non-empty): {negotiated:?}"
        );
    }
    assert!(
        negotiated.iter().any(|(object, opcode, body)| {
            *object == offer && *opcode == 2 && word(body, 0) == 1
        }),
        "offer must observe its action negotiated to Copy before teardown \
        (corroborates chosen_action is non-empty): {negotiated:?}"
    );
    offer
}

fn assert_offer_revoked(harness: &mut KeybindingHarness, offer: u32) {
    use smithay::reexports::wayland_server::protocol::wl_data_offer;

    // wl_data_offer.finish (opcode 3, no args).
    send_request(&mut harness.client, offer, 3, &[]);
    harness.dispatch_client();
    let (offending, code, message) = read_protocol_error(&mut harness.client);
    assert_eq!(offending, offer, "wrong error object: {message}");
    assert_eq!(
        code,
        wl_data_offer::Error::InvalidFinish as u32,
        "finishing a cancelled offer must be a protocol error, not silence: {message}"
    );
    // LOAD-BEARING: an active but undropped offer also returns InvalidFinish.
    assert_eq!(
        message, "Cannot finish a data offer that is no longer active.",
        "finish must fail because cancel revoked the offer"
    );
}

fn assert_drag_outcome(
    traffic: &[(u32, u16, Vec<u8>)],
    device: u32,
    source: Option<u32>,
    dropped: bool,
) {
    let count = |id, op| {
        traffic
            .iter()
            .filter(|(object, opcode, _)| *object == id && *opcode == op)
            .count()
    };
    assert_eq!(
        count(device, 4),
        usize::from(dropped),
        "destination drop: {traffic:?}"
    );
    assert_eq!(count(device, 2), 1, "destination leave: {traffic:?}");
    if let Some(source) = source {
        assert_eq!(
            count(source, 3),
            usize::from(dropped),
            "source drop_performed: {traffic:?}"
        );
        assert_eq!(
            count(source, 2),
            usize::from(!dropped),
            "source cancelled: {traffic:?}"
        );
    }
}

#[test]
fn dnd_touch_session_lock_cancels_without_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, source, offer) = start_accepted_client_drag(&mut harness, true);
    // LOAD-BEARING: accept_drag_offer proves accepted text/plain + Copy before
    // any release. Targets dnd_grab.rs touch unset: old line 495's unconditional
    // self.drop(data), replaced by pending_drop gating (current lines 577-584).
    // Session lock must take cancel, even though this offer validates for drop.
    let (_, _, _, traffic) = request_test_session_lock_with_traffic(&mut harness);
    assert_drag_outcome(&traffic, device, Some(source), false);
    assert_drag_grab(&harness, true, false);
    // LOAD-BEARING: proves cancel revoked the offer (offer_data.active = false).
    assert_offer_revoked(&mut harness, offer);
}

#[test]
fn dnd_touch_release_still_delivers_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, source, _) = start_accepted_client_drag(&mut harness, true);
    // Same accepted + Copy precondition as cancellation. Targets the added
    // pending_drop=true in DnDGrab::up (dnd_grab.rs:512) and the true branch
    // in touch unset: omitting release authorisation would cancel this drag.
    touch_drag_up(&mut harness, 0);
    assert_drag_outcome(&harness.sync(), device, Some(source), true);
    assert_drag_grab(&harness, true, false);
}

#[test]
fn dnd_touch_second_finger_release_does_not_authorise_drop() {
    // Both endings use the shared accepted + Copy proof. In DnDGrab::up,
    // the slot guard must precede the added pending_drop=true (currently
    // dnd_grab.rs:507-512). The two endings check that the initiating finger
    // can still drop and that cancellation after the ignored finger still works.
    for release_initiator in [false, true] {
        let mut harness = KeybindingHarness::new(true);
        let (device, source, offer) = start_accepted_client_drag(&mut harness, true);
        touch_drag_down(&mut harness, 1);
        let mut traffic = harness.sync();
        touch_drag_up(&mut harness, 1);
        traffic.extend(harness.sync());
        assert!(
            !traffic.iter().any(|(object, opcode, _)| (*object == device
                && matches!(opcode, 2 | 4))
                || (*object == source && matches!(opcode, 2 | 3))),
            "non-initiating finger must neither drop nor cancel: {traffic:?}"
        );
        assert_drag_grab(&harness, true, true);
        let traffic = if release_initiator {
            touch_drag_up(&mut harness, 0);
            harness.sync()
        } else {
            request_test_session_lock_with_traffic(&mut harness).3
        };
        assert_drag_outcome(&traffic, device, Some(source), release_initiator);
        assert_drag_grab(&harness, true, false);
        if !release_initiator {
            // LOAD-BEARING: proves cancel revoked the offer (offer_data.active = false).
            assert_offer_revoked(&mut harness, offer);
        }
    }
}

#[test]
fn dnd_server_pointer_session_lock_cancels_without_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, offer) = start_accepted_server_drag(&mut harness, false);
    // LOAD-BEARING: the real destination offer is accepted + Copy, so the
    // server drop gate validates. Targets server_dnd_grab.rs pointer unset:
    // old line 376 self.drop(data) becomes the pending_drop branch (now
    // lines 421-427). No release occurred, so drop must not reach the wire.
    // WaylandState's empty ServerDndGrabHandler uses no-op callbacks; there
    // is no client source on which cancelled/drop_performed could be observed.
    let (_, _, _, traffic) = request_test_session_lock_with_traffic(&mut harness);
    assert_drag_outcome(&traffic, device, None, false);
    assert_drag_grab(&harness, false, false);
    // LOAD-BEARING: proves cancel revoked the offer (offer_data.active = false).
    assert_offer_revoked(&mut harness, offer);
}

#[test]
fn dnd_server_pointer_release_still_delivers_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, _) = start_accepted_server_drag(&mut harness, false);
    // The same real, accepted + Copy server offer must still drop. Targets
    // server_dnd_grab.rs button's added pending_drop=true (now line 331),
    // and pointer unset's true branch (423-424), not a client DnDGrab.
    route_pointer_button(&mut harness, PRIMARY_POINTER_BUTTON, ButtonState::Released);
    assert_drag_outcome(&harness.sync(), device, None, true);
    assert_drag_grab(&harness, false, false);
}

#[test]
fn dnd_server_touch_session_lock_cancels_without_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, offer) = start_accepted_server_drag(&mut harness, true);
    // LOAD-BEARING: enter + MIME + accepted Copy are verified on the real
    // server offer before cancellation, making drop's validated gate true.
    // Targets server_dnd_grab.rs touch unset: old line 472 self.drop(data)
    // is now pending_drop-gated (528-535). Touch cancel clears pending_drop
    // (502) then unsets; no initiating up has authorised delivery.
    let (_, _, _, traffic) = request_test_session_lock_with_traffic(&mut harness);
    assert_drag_outcome(&traffic, device, None, false);
    assert_drag_grab(&harness, true, false);
    // LOAD-BEARING: proves cancel revoked the offer (offer_data.active = false).
    assert_offer_revoked(&mut harness, offer);
}

#[test]
fn dnd_server_touch_release_still_delivers_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, _) = start_accepted_server_drag(&mut harness, true);
    // Same accepted + Copy precondition as server-touch cancellation. Targets
    // the added pending_drop=true after the initiating-slot guard in server
    // up (server_dnd_grab.rs:456-461), and touch unset's true branch (530-531).
    // Without that release authorisation, unset would cancel this valid drop.
    touch_drag_up(&mut harness, 0);
    assert_drag_outcome(&harness.sync(), device, None, true);
    assert_drag_grab(&harness, true, false);
}

#[test]
fn dnd_session_lock_cancels_without_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, source, offer) = start_accepted_pointer_drag(&mut harness);

    // No button release: entering session lock tears down the active grab.
    let (_, _, _, traffic) = request_test_session_lock_with_traffic(&mut harness);
    assert!(
        !traffic
            .iter()
            .any(|(object, opcode, _)| *object == device && *opcode == 4),
        "session lock must not send wl_data_device.drop (opcode 4): {traffic:?}"
    );
    assert!(
        !traffic
            .iter()
            .any(|(object, opcode, _)| *object == source && *opcode == 3),
        "session lock must not send wl_data_source.dnd_drop_performed (opcode 3): {traffic:?}"
    );
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == source && *opcode == 2),
        "session lock must send wl_data_source.cancelled (opcode 2): {traffic:?}"
    );
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == device && *opcode == 2),
        "session lock must send wl_data_device.leave (opcode 2): {traffic:?}"
    );
    assert!(!harness.server.state.pointer.is_grabbed());
    // LOAD-BEARING: proves cancel revoked the offer (offer_data.active = false).
    assert_offer_revoked(&mut harness, offer);
}

#[test]
fn dnd_pointer_release_still_delivers_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, source, _) = start_accepted_pointer_drag(&mut harness);

    route_pointer_button(&mut harness, PRIMARY_POINTER_BUTTON, ButtonState::Released);
    let traffic = harness.sync();
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == device && *opcode == 4),
        "button release must send wl_data_device.drop (opcode 4): {traffic:?}"
    );
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == source && *opcode == 3),
        "button release must send wl_data_source.dnd_drop_performed (opcode 3): {traffic:?}"
    );
    assert!(
        !traffic
            .iter()
            .any(|(object, opcode, _)| *object == source && *opcode == 2),
        "accepted button-release drop must not cancel its source: {traffic:?}"
    );
    assert!(!harness.server.state.pointer.is_grabbed());
}

#[test]
fn dnd_source_destroy_cancels_without_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, source, offer) = start_accepted_pointer_drag(&mut harness);

    // wl_data_source.destroy; a destroyed object cannot receive cancelled.
    send_request(&mut harness.client, source, 1, &[]);
    let traffic = harness.sync();
    assert!(
        !traffic
            .iter()
            .any(|(object, opcode, _)| *object == device && *opcode == 4),
        "source destruction must not send wl_data_device.drop (opcode 4): {traffic:?}"
    );
    assert!(
        !traffic
            .iter()
            .any(|(object, opcode, _)| *object == source && *opcode == 3),
        "source destruction must not send dnd_drop_performed: {traffic:?}"
    );
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == device && *opcode == 2),
        "source destruction must send wl_data_device.leave (opcode 2): {traffic:?}"
    );
    assert!(!harness.server.state.pointer.is_grabbed());
    // LOAD-BEARING: proves cancel revoked the offer (offer_data.active = false).
    assert_offer_revoked(&mut harness, offer);
}
