// Exercise real wire offers, including acceptance and action negotiation.
//
// This precondition is LOAD-BEARING, not cosmetic. `DnDGrab::drop` (and its
// `ServerDnDGrab` twin) gates every observable outcome on
// `validated = data.accepted && !data.chosen_action.is_empty()`:
// when `validated` is false, `drop()` ALREADY sends no `wl_data_device.drop`
// and ALREADY calls `source.cancelled()` — on today's buggy code, not just
// the fixed one. A drag whose destination never accepted an offer or never
// negotiated an action would make `dnd_session_lock_cancels_without_drop`'s
// assertions true before AND after the fix, proving nothing. So every test
// below must start from a drag that is accepted with a non-empty chosen
// action, and this helper asserts that precondition on the wire (not just
// assumes it) before returning.
fn start_accepted_pointer_drag(harness: &mut KeybindingHarness) -> (u32, u32) {
    map_initial_test_toplevel(harness);
    let pointer = harness.bind_pointer();
    harness.prime_pointer_focus();
    route_pointer_button(harness, PRIMARY_POINTER_BUTTON, ButtonState::Pressed);
    let pressed = harness.sync();
    let pointer_serial = word(&pointer_body(&pressed, pointer, 3), 0);

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
        &words(&[source, TEST_TOPLEVEL_SURFACE_ID, 0, pointer_serial]),
    );
    let mut entered = harness.sync();
    // DnD focus is established by motion after start_drag, independently of
    // the ordinary pointer focus used to authorise that request.
    harness.route(InputEvent::PointerMotionAbsolute {
        event: FakeAbsoluteEvent {
            normalised_x: 0.11,
            normalised_y: 0.11,
        },
    });
    entered.extend(harness.sync());
    let enter = entered
        .iter()
        .find(|(object, opcode, _)| *object == device && *opcode == 1)
        .unwrap_or_else(|| panic!("drag must enter its target: {entered:?}"));
    assert_eq!(word(&enter.2, 1), TEST_TOPLEVEL_SURFACE_ID);
    let offer = word(&enter.2, 4);
    assert_ne!(offer, 0, "drag must carry a real data offer");
    let mut accept = words(&[word(&enter.2, 0)]);
    accept.extend(wire_string_argument("text/plain"));
    send_request(&mut harness.client, offer, 0, &accept);
    // wl_data_offer.set_actions(Copy, Copy).
    send_request(&mut harness.client, offer, 4, &words(&[1, 1]));
    let negotiated = harness.sync();
    // Proves `validated == true` on the wire, the exact precondition
    // `drop()` gates on: `wl_data_source.action` (opcode 5, value 1 = Copy)
    // only fires when `data.chosen_action` just became non-empty, and it can
    // only have become non-empty after the `wl_data_offer.accept` above set
    // `data.accepted = true` (same client, same stream, so the accept
    // request is processed strictly before this set_actions request). Two
    // independent objects (source AND offer) both report the negotiated
    // action so a bug in either side's bookkeeping still trips this.
    assert!(
        negotiated.iter().any(|(object, opcode, body)| {
            *object == source && *opcode == 5 && word(body, 0) == 1
        }),
        "source must observe its action negotiated to Copy before teardown \
         (proves chosen_action is non-empty): {negotiated:?}"
    );
    assert!(
        negotiated.iter().any(|(object, opcode, body)| {
            *object == offer && *opcode == 2 && word(body, 0) == 1
        }),
        "offer must observe its action negotiated to Copy before teardown \
         (corroborates chosen_action is non-empty): {negotiated:?}"
    );
    assert!(
        harness.server.state.pointer.is_grabbed(),
        "the drag grab must still be live before we test how it tears down"
    );
    (device, source)
}

#[test]
fn dnd_session_lock_cancels_without_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, source) = start_accepted_pointer_drag(&mut harness);

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
}

#[test]
fn dnd_pointer_release_still_delivers_drop() {
    let mut harness = KeybindingHarness::new(true);
    let (device, source) = start_accepted_pointer_drag(&mut harness);

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
    let (device, source) = start_accepted_pointer_drag(&mut harness);

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
}
