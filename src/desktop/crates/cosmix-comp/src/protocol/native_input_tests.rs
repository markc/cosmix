// Keyboard and IME for in-process content (included from tests.rs).

use crate::native_input::{
    NativeImeEvent, NativeImeRequest, NativeInputBridge, NativeOwner, NativeQueuedEvent,
};
use crate::port::ControlReply;
use smithay::wayland::input_method::InputMethodSeat;

/// `zwp_input_method_v2` request opcodes (the XML order).
const IM_COMMIT_STRING: u16 = 0;
const IM_SET_PREEDIT_STRING: u16 = 1;
const IM_DELETE_SURROUNDING_TEXT: u16 = 2;
const IM_COMMIT: u16 = 3;
const IM_GET_INPUT_POPUP_SURFACE: u16 = 4;
const IM_GRAB_KEYBOARD: u16 = 5;
/// `zwp_input_method_v2` event opcodes.
const IM_EVENT_ACTIVATE: u16 = 0;
const IM_EVENT_DEACTIVATE: u16 = 1;
const IM_EVENT_SURROUNDING_TEXT: u16 = 2;
const IM_EVENT_CONTENT_TYPE: u16 = 4;
const IM_EVENT_DONE: u16 = 5;
/// `zwp_input_method_keyboard_grab_v2` event opcodes.
const GRAB_EVENT_KEY: u16 = 1;

const KEY_A: u32 = 30;
const KEY_M: u32 = 50;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;

fn native_bridge(harness: &mut KeybindingHarness) -> (NativeInputBridge, NativeOwner) {
    let bridge = NativeInputBridge::default();
    harness.server.state.install_native_input(bridge.clone());
    (bridge, NativeOwner::next())
}

fn take_focus(
    harness: &mut KeybindingHarness,
    bridge: &NativeInputBridge,
    owner: NativeOwner,
    wanted: bool,
) {
    bridge.set_wants_focus(owner, wanted);
    harness.server.state.service_native_focus_request();
}

fn enable_native_ime(harness: &mut KeybindingHarness) {
    harness
        .server
        .state
        .service_native_ime_request(NativeImeRequest::Enable(true));
}

fn native_keys(bridge: &NativeInputBridge) -> Vec<(u32, bool, bool, Option<String>)> {
    bridge
        .drain_keys_for_test()
        .into_iter()
        .map(|event| (event.evdev, event.pressed, event.repeat, event.text))
        .collect()
}

fn native_ime_events(bridge: &NativeInputBridge) -> Vec<NativeImeEvent> {
    bridge
        .drain_for_test()
        .into_iter()
        .filter_map(|(_, _, event)| match event {
            NativeQueuedEvent::Ime(event) => Some(event),
            _ => None,
        })
        .collect()
}

/// In-process content is the LAST focus requester: it takes the keyboard
/// from a client that has it by default, and everything with a claim of its
/// own — the lock, an exclusive layer, a named window — takes it back.
#[test]
fn native_content_is_the_last_focus_requester() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let object = test_toplevel_record(&harness).role.wl_surface().id();
    let surface = harness.server.state.surfaces[&object]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    assert!(harness.server.state.surfaces[&object].focused);
    let (bridge, owner) = native_bridge(&mut harness);

    take_focus(&mut harness, &bridge, owner, true);
    assert!(bridge.focused(), "the content owns the keyboard");
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        None,
        "no client owns it while the content does"
    );
    assert!(!harness.server.state.surfaces[&object].focused);
    assert_eq!(bridge.focus_owner(), Some(owner));

    take_focus(&mut harness, &bridge, owner, false);
    assert!(!bridge.focused());
    assert_eq!(bridge.focus_owner(), None);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()).map(|s| s.id()),
        Some(object.clone()),
        "the window gets the keyboard back"
    );

    // An exclusive layer surface outranks it.
    let exclusive = zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive as u32;
    let (layer, _) = map_test_layer_surface(
        &mut harness,
        0,
        TestLayerSpec {
            keyboard_interactivity: exclusive,
            ..TestLayerSpec::default()
        },
    );
    take_focus(&mut harness, &bridge, owner, true);
    assert!(!bridge.focused(), "an exclusive layer keeps the keyboard");
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(
            test_layer_record(&harness, layer.surface)
                .role
                .wl_surface()
                .clone()
        )
    );

    // And so does a session lock.
    let lock = begin_test_session_lock(&mut harness);
    ack_and_map_test_lock_surface(&mut harness, lock);
    take_focus(&mut harness, &bridge, owner, true);
    assert!(!bridge.focused(), "the lock screen keeps the keyboard");
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus())
            .map(|surface| surface.id().protocol_id()),
        Some(lock.surface)
    );
}

/// A client asked for by name preempts the content, and the content's
/// standing request is honoured again once that client goes away.
#[cfg(feature = "bus")]
#[test]
fn a_named_window_preempts_native_content_and_it_regains_focus() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let record = test_toplevel_record(&harness);
    let id = record.id.0;
    let generation = record.generation;
    let object = record.role.wl_surface().id();
    let surface = harness.server.state.surfaces[&object]
        .role
        .wl_surface()
        .clone();
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    assert!(bridge.focused());

    // `comp.window.focus` is an explicit claim and wins.
    let reply = harness
        .server
        .state
        .service_window_op(&crate::port::WindowOp::Focus {
            id,
            generation,
            raise: false,
        });
    let ControlReply::Body(body) = reply else {
        panic!("comp.window.focus answered something else");
    };
    assert_eq!(body["focused"], json!(true), "{body}");
    assert_eq!(body.get("reason"), None, "{body}");
    assert!(!bridge.focused(), "the named window took the keyboard");
    assert!(
        bridge.wants_focus(),
        "the content's request is still standing"
    );

    // The content hears the edge, both ways, and nothing it queued before
    // the edge survives it.
    let edges: Vec<_> = bridge
        .drain_for_test()
        .into_iter()
        .filter_map(|(_, _, event)| match event {
            NativeQueuedEvent::Focus { focused, .. } => Some(focused),
            _ => None,
        })
        .collect();
    assert_eq!(edges, [false], "the falling edge is delivered: {edges:?}");

    // When that window unmaps, the standing request is honoured again.
    let _ = &surface;
    send_request(
        &mut harness.client,
        TEST_TOPLEVEL_SURFACE_ID,
        1,
        &words(&[0, 0, 0]),
    );
    send_request(&mut harness.client, TEST_TOPLEVEL_SURFACE_ID, 6, &[]);
    harness.dispatch_client();
    assert!(bridge.focused(), "the content gets the keyboard back");
}

/// While the content owns the keyboard the client hears nothing, bindings
/// still come first, and the seat repeats for content that has no
/// `repeat_info` of its own.
#[test]
fn keys_reach_native_content_after_the_bindings_and_not_the_client() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let object = test_toplevel_record(&harness).role.wl_surface().id();
    let surface = harness.server.state.surfaces[&object]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    let (bridge, owner) = native_bridge(&mut harness);

    // Before the content asks, the client gets the key.
    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert_eq!(keyboard_key_events(&harness.sync()), [(KEY_A, 1), (KEY_A, 0)]);
    assert!(native_keys(&bridge).is_empty());

    take_focus(&mut harness, &bridge, owner, true);
    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert!(
        keyboard_key_events(&harness.sync()).is_empty(),
        "no client hears a key the content owns"
    );
    assert_eq!(
        native_keys(&bridge),
        [
            (KEY_A, true, false, Some("a".into())),
            (KEY_A, false, false, Some("a".into())),
        ]
    );

    // Modifiers are ordinary keys for the content, not something the seat
    // keeps to itself.
    harness.key(KEY_LEFTSHIFT, HostButtonState::Pressed);
    harness.key(KEY_LEFTSHIFT, HostButtonState::Released);
    let keys = native_keys(&bridge);
    assert!(
        keys.iter().any(|(evdev, ..)| *evdev == KEY_LEFTSHIFT),
        "the modifiers themselves arrive: {keys:?}"
    );

    // A binding still wins: Super+Shift+M restores, and the M never reaches
    // the content. Restoring also FOCUSES the window, which outranks the
    // content's standing request, so the chord's own keys die with the
    // generation they belonged to.
    harness.server.state.minimize_toplevel(&surface);
    assert!(harness.server.state.surfaces[&object].minimized);
    harness.chord(&[KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_M]);
    assert!(!harness.server.state.surfaces[&object].minimized);
    assert!(
        !bridge.focused(),
        "the restored window took the keyboard back"
    );
    let keys = native_keys(&bridge);
    assert!(
        keys.iter().all(|(evdev, ..)| *evdev != KEY_M),
        "the binding swallowed M: {keys:?}"
    );
    // The window had the keyboard for the tail of the chord, so the
    // releases of Super and Shift went to it; drain them before the last
    // assertion looks at what the client heard.
    let _ = harness.sync();
    // Asking again is how preempted content gets the keyboard back.
    take_focus(&mut harness, &bridge, owner, true);
    assert!(bridge.focused());

    // Repeats: the compositor generates them, because the content has no
    // `wl_keyboard.repeat_info` to do it for itself.
    harness.key(KEY_A, HostButtonState::Pressed);
    assert_eq!(native_keys(&bridge).len(), 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut repeats = 0;
    while repeats < 2 {
        harness
            .server
            .event_loop
            .dispatch(Some(Duration::from_millis(50)), &mut harness.server.state)
            .expect("repeat dispatch");
        repeats += native_keys(&bridge)
            .iter()
            .filter(|(evdev, pressed, repeat, _)| *evdev == KEY_A && *pressed && *repeat)
            .count();
        assert!(Instant::now() < deadline, "no repeat arrived");
    }

    // A MODIFIER pressed while a key repeats does not end the repeat — the
    // capital letters of a held key are the whole point.
    harness.key(KEY_LEFTSHIFT, HostButtonState::Pressed);
    let _ = native_keys(&bridge);
    let mut repeats_after_shift = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while repeats_after_shift == 0 {
        harness
            .server
            .event_loop
            .dispatch(Some(Duration::from_millis(50)), &mut harness.server.state)
            .expect("repeat dispatch");
        repeats_after_shift += native_keys(&bridge)
            .iter()
            .filter(|(evdev, pressed, repeat, _)| *evdev == KEY_A && *pressed && *repeat)
            .count();
        assert!(
            Instant::now() < deadline,
            "the modifier press cancelled the repeat"
        );
    }
    harness.key(KEY_LEFTSHIFT, HostButtonState::Released);

    harness.key(KEY_A, HostButtonState::Released);
    let _ = native_keys(&bridge);
    for _ in 0..4 {
        harness
            .server
            .event_loop
            .dispatch(Some(Duration::from_millis(50)), &mut harness.server.state)
            .expect("post-release dispatch");
    }
    assert!(native_keys(&bridge).is_empty(), "the release ends the repeat");

    // Giving the keyboard back restores client delivery.
    take_focus(&mut harness, &bridge, owner, false);
    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert_eq!(keyboard_key_events(&harness.sync()), [(KEY_A, 1), (KEY_A, 0)]);
}

/// Injected `comp.input.key` reaches in-process content by the same path a
/// device key does.
#[cfg(feature = "bus")]
#[test]
fn injected_text_reaches_native_content() {
    let (mut harness, ingress, _observations) = KeybindingHarness::new_with_port();
    map_initial_test_toplevel(&mut harness);
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    let runtime = control_reply_runtime();
    let admission = ingress
        .request_input(crate::port::InputOp::Text("ok".into()))
        .expect("text admitted");
    let (rc, body) = serviced_control_reply(&mut harness, &runtime, admission);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(
        body["target"],
        Value::Null,
        "no client surface owns the keyboard"
    );
    let text = native_keys(&bridge)
        .into_iter()
        .filter(|(_, pressed, _, _)| *pressed)
        .filter_map(|(_, _, _, text)| text)
        .collect::<String>();
    assert_eq!(text, "ok");
    assert!(keyboard_key_events(&harness.sync()).is_empty());
}

/// Bind the harness client as the input method, the way `cosmix-imeprobe`
/// does.
fn bind_test_input_method(harness: &mut KeybindingHarness) -> u32 {
    let manager = harness.bind_test_global("zwp_input_method_manager_v2", 1);
    let input_method = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        manager,
        0,
        &words(&[TEST_SEAT_ID, input_method]),
    );
    let _ = harness.sync();
    input_method
}

fn input_method_events(traffic: &[(u32, u16, Vec<u8>)], object: u32) -> Vec<u16> {
    traffic
        .iter()
        .filter(|(target, _, _)| *target == object)
        .map(|(_, opcode, _)| *opcode)
        .collect()
}

/// An input method holding the keyboard grab outranks in-process content:
/// the key goes to the IM, not to the bridge, or every IME is dead while a
/// scene has focus.
#[test]
fn an_input_method_keyboard_grab_outranks_native_content() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    let input_method = bind_test_input_method(&mut harness);
    enable_native_ime(&mut harness);
    let _ = harness.sync();

    // Without a grab the content receives the key.
    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert_eq!(native_keys(&bridge).len(), 2);

    let grab = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        input_method,
        IM_GRAB_KEYBOARD,
        &words(&[grab]),
    );
    let _ = harness.sync();
    assert!(
        harness.server.state.seat.input_method().keyboard_grabbed(),
        "the input method holds the keyboard grab"
    );

    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    let traffic = harness.sync();
    assert!(
        native_keys(&bridge).is_empty(),
        "the grab takes the key, not the content"
    );
    assert_eq!(
        input_method_events(&traffic, grab)
            .into_iter()
            .filter(|opcode| *opcode == GRAB_EVENT_KEY)
            .count(),
        2,
        "the input method received both halves of the key: {traffic:?}"
    );
}

/// The vendored sink both ways: input-method output reaches in-process
/// content when no client text input is active, and the content's own
/// state reaches the input method.
#[test]
fn ime_sink_carries_both_directions_for_native_content() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    let input_method = bind_test_input_method(&mut harness);

    // Content -> input method: enable, caret, surrounding text, content
    // type, done.
    for request in [
        NativeImeRequest::Enable(true),
        NativeImeRequest::Caret {
            x: 40,
            y: 80,
            width: 2,
            height: 18,
        },
        NativeImeRequest::SurroundingText {
            text: "hi".into(),
            cursor: 2,
            anchor: 2,
        },
        NativeImeRequest::ContentType { hint: 0, purpose: 0 },
        NativeImeRequest::Done,
    ] {
        harness.server.state.service_native_ime_request(request);
    }
    let traffic = harness.sync();
    let events = input_method_events(&traffic, input_method);
    assert!(
        events.contains(&IM_EVENT_ACTIVATE)
            && events.contains(&IM_EVENT_SURROUNDING_TEXT)
            && events.contains(&IM_EVENT_CONTENT_TYPE)
            && events.contains(&IM_EVENT_DONE),
        "the input method was driven by content with no surface: {events:?}"
    );

    // Input method -> content: a preedit, a delete, a commit, a done.
    let mut preedit = wire_string_argument("cosmix");
    preedit.extend_from_slice(&0_i32.to_ne_bytes());
    preedit.extend_from_slice(&6_i32.to_ne_bytes());
    send_request(
        &mut harness.client,
        input_method,
        IM_SET_PREEDIT_STRING,
        &preedit,
    );
    send_request(
        &mut harness.client,
        input_method,
        IM_DELETE_SURROUNDING_TEXT,
        &words(&[1, 0]),
    );
    send_request(
        &mut harness.client,
        input_method,
        IM_COMMIT_STRING,
        &wire_string_argument("ok"),
    );
    send_request(&mut harness.client, input_method, IM_COMMIT, &words(&[0]));
    harness.dispatch_client();
    harness.assert_client_connected("after driving the input method");
    assert_eq!(
        native_ime_events(&bridge),
        [
            NativeImeEvent::Preedit {
                text: "cosmix".into(),
                cursor_begin: 0,
                cursor_end: 6,
            },
            NativeImeEvent::DeleteSurrounding {
                before: 1,
                after: 0
            },
            NativeImeEvent::Commit("ok".into()),
            // The probe committed with serial 0 while the compositor had
            // already sent one `done`, so upstream's mismatch rule applies to
            // the sink exactly as it would to a client.
            NativeImeEvent::Done { discard: true },
        ]
    );

    // Disabling deactivates the input method.
    harness
        .server
        .state
        .service_native_ime_request(NativeImeRequest::Enable(false));
    let events = input_method_events(&harness.sync(), input_method);
    assert!(events.contains(&IM_EVENT_DEACTIVATE), "{events:?}");
}

/// THE BLOCKER: a batch abandoned by the compositor field must not leave
/// the vendored latch pointing at the sink, or the next batch — a client's —
/// is routed to a sink that drops it and the client never sees its commit.
#[test]
fn an_abandoned_native_batch_does_not_swallow_the_next_client_batch() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let surface = test_toplevel_record(&harness).role.wl_surface().clone();
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    enable_native_ime(&mut harness);
    let input_method = bind_test_input_method(&mut harness);
    report_field_state(&mut harness);
    let _ = harness.sync();

    // The field starts a composition, then loses the keyboard to a client
    // (which ends its IME session and abandons the batch).
    let mut preedit = wire_string_argument("half");
    preedit.extend_from_slice(&0_i32.to_ne_bytes());
    preedit.extend_from_slice(&4_i32.to_ne_bytes());
    send_request(
        &mut harness.client,
        input_method,
        IM_SET_PREEDIT_STRING,
        &preedit,
    );
    harness.dispatch_client();
    harness.server.state.activate_managed_window(&surface);
    assert!(!bridge.focused());

    // That client now enables a text input and the input method commits to
    // it. This is the typing-into-a-real-app path.
    let manager = harness.bind_test_global("zwp_text_input_manager_v3", 1);
    let text_input = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        manager,
        1,
        &words(&[text_input, TEST_SEAT_ID]),
    );
    let _ = harness.sync();
    send_request(&mut harness.client, text_input, 1, &[]);
    send_request(&mut harness.client, text_input, 7, &[]);
    let _ = harness.sync();
    send_request(
        &mut harness.client,
        input_method,
        IM_COMMIT_STRING,
        &wire_string_argument("client"),
    );
    send_request(&mut harness.client, input_method, IM_COMMIT, &words(&[0]));
    harness.dispatch_client();
    let traffic = harness.sync();
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == text_input && *opcode == 3),
        "the client never received its commit — the abandoned batch's latch \
         routed it to the sink: {traffic:?}"
    );
    assert!(
        native_ime_events(&bridge).is_empty(),
        "the unfocused field received a client's batch"
    );
}

/// Taking the keyboard from a client that was itself editing: the field's
/// input method must come up. The activation waits for the seat focus to
/// actually move, because until it does the client's text input still reads
/// as active and outranks the field.
#[test]
fn taking_focus_from_a_client_editor_still_activates_the_field() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let surface = test_toplevel_record(&harness).role.wl_surface().clone();
    harness.server.state.activate_managed_window(&surface);
    let input_method = bind_test_input_method(&mut harness);

    // The focused client is editing: an ACTIVE text input.
    let manager = harness.bind_test_global("zwp_text_input_manager_v3", 1);
    let text_input = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        manager,
        1,
        &words(&[text_input, TEST_SEAT_ID]),
    );
    let _ = harness.sync();
    send_request(&mut harness.client, text_input, 1, &[]);
    send_request(&mut harness.client, text_input, 7, &[]);
    let _ = harness.sync();
    assert!(harness.server.state.seat.text_input().has_active_text_input());

    // The scene claims a text field and then asks for the keyboard.
    let (bridge, owner) = native_bridge(&mut harness);
    harness
        .server
        .state
        .service_native_ime_request(NativeImeRequest::Enable(true));
    take_focus(&mut harness, &bridge, owner, true);
    assert!(bridge.focused());
    assert_eq!(
        activate_count(&harness.sync(), input_method),
        1,
        "the field took the keyboard from an editor and got no input method"
    );
    assert!(bridge.ime_active());
}

/// An owner handover inherits NOTHING: the previous owner's input-method
/// session is ended and its caret goes with it, so the new field does not
/// find itself with a live IME it never asked for, anchored on the old one.
#[test]
fn a_handover_does_not_inherit_the_previous_owners_ime() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let (bridge, first) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, first, true);
    let input_method = bind_test_input_method(&mut harness);
    enable_native_ime(&mut harness);
    harness
        .server
        .state
        .service_native_ime_request(NativeImeRequest::Caret {
            x: 120,
            y: 200,
            width: 2,
            height: 18,
        });
    report_field_state(&mut harness);
    assert_eq!(activate_count(&harness.sync(), input_method), 1);
    assert!(bridge.ime_active());

    // A second owner takes the standing request; the keyboard never leaves
    // in-process content.
    let second = NativeOwner::next();
    take_focus(&mut harness, &bridge, second, true);
    assert!(bridge.focused());
    assert_eq!(bridge.focus_owner(), Some(second));
    let events = input_method_events(&harness.sync(), input_method);
    assert!(
        events.contains(&IM_EVENT_DEACTIVATE),
        "the first owner's IME session outlived it: {events:?}"
    );
    assert!(
        !bridge.ime_active() && !bridge.enabled(),
        "the new owner inherited an input method it never asked for"
    );
    assert_eq!(
        bridge.caret(),
        None,
        "the new owner inherited the old field's caret"
    );

    // The new owner asking for one gets its own session.
    enable_native_ime(&mut harness);
    report_field_state(&mut harness);
    assert_eq!(activate_count(&harness.sync(), input_method), 1);
    assert!(bridge.ime_active());
}

/// Regaining the keyboard re-arms the input method the field still claims.
#[test]
fn regaining_focus_reactivates_the_input_method() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let surface = test_toplevel_record(&harness).role.wl_surface().clone();
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    enable_native_ime(&mut harness);
    let input_method = bind_test_input_method(&mut harness);
    report_field_state(&mut harness);
    assert_eq!(activate_count(&harness.sync(), input_method), 1);

    // A client takes the keyboard: the session ends.
    harness.server.state.activate_managed_window(&surface);
    let events = input_method_events(&harness.sync(), input_method);
    assert!(events.contains(&IM_EVENT_DEACTIVATE), "{events:?}");

    // The field takes it back, and its IME comes back with it WITHOUT the
    // field having to send anything.
    take_focus(&mut harness, &bridge, owner, true);
    assert!(bridge.focused());
    assert_eq!(
        activate_count(&harness.sync(), input_method),
        1,
        "the field'\''s input method was not re-armed on the focus edge"
    );
    assert!(bridge.ime_active(), "active() must report the live session");
}

/// The input method is activated ONCE per instance: a field that re-reports
/// its state after every edit must not keep resetting the instance serial,
/// or the IM's next commit comes back mismatched. An input method that
/// connects later still gets its activate.
/// Everything a field reports to the input method after an edit.
fn report_field_state(harness: &mut KeybindingHarness) {
    for request in [
        NativeImeRequest::Enable(true),
        NativeImeRequest::SurroundingText {
            text: "hi".into(),
            cursor: 2,
            anchor: 2,
        },
        NativeImeRequest::Caret {
            x: 40,
            y: 80,
            width: 2,
            height: 18,
        },
        NativeImeRequest::Done,
    ] {
        harness.server.state.service_native_ime_request(request);
    }
}

fn activate_count(traffic: &[(u32, u16, Vec<u8>)], object: u32) -> usize {
    input_method_events(traffic, object)
        .into_iter()
        .filter(|opcode| *opcode == IM_EVENT_ACTIVATE)
        .count()
}

#[test]
fn the_input_method_is_activated_once_per_instance() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);

    // The field enables BEFORE any input method exists — the ordering a
    // compositor-drawn field always has, because it is up first.
    enable_native_ime(&mut harness);
    let input_method = bind_test_input_method(&mut harness);
    assert_eq!(
        activate_count(&harness.sync(), input_method),
        0,
        "there was nothing to activate when the field enabled"
    );

    // The next thing the field reports finds the new instance and activates
    // it once.
    report_field_state(&mut harness);
    assert_eq!(
        activate_count(&harness.sync(), input_method),
        1,
        "an input method that connected after the field is never activated"
    );

    // Everything after that must NOT re-activate: an activate resets the
    // instance serial, and the IM's next commit would come back mismatched.
    for _ in 0..3 {
        report_field_state(&mut harness);
    }
    assert_eq!(
        activate_count(&harness.sync(), input_method),
        0,
        "re-reporting state re-activated the input method"
    );
}

/// One owner: a second install is refused rather than silently replacing the
/// first, and uninstalling gives the keyboard and the input method back.
#[test]
fn native_input_has_one_owner_and_can_be_uninstalled() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let (bridge, owner) = native_bridge(&mut harness);
    let (second, second_owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    assert!(bridge.focused(), "the first owner keeps the seat");
    assert!(!second.focused());
    take_focus(&mut harness, &second, second_owner, true);
    assert!(
        !second.focused(),
        "the refused install must not receive focus"
    );

    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert_eq!(native_keys(&bridge).len(), 2);
    assert!(native_keys(&second).is_empty());

    // A refused install is observable to the consumer it refused.
    assert!(bridge.installed());
    assert!(!second.installed(), "a refused bridge must not look installed");

    // The owner goes away.
    harness.server.state.uninstall_native_input();
    assert!(!bridge.focused(), "the departing owner hears the edge");
    assert!(
        harness.server.state.seat.input_method().sink().is_none(),
        "the sink outlived its owner"
    );
    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert!(native_keys(&bridge).is_empty(), "nothing is fed after uninstall");
    assert_eq!(keyboard_key_events(&harness.sync()), [(KEY_A, 1), (KEY_A, 0)]);
    assert!(!bridge.installed());
    assert!(
        !bridge.wants_focus() && !bridge.enabled() && bridge.caret().is_none(),
        "uninstall left field state behind for the next install"
    );
}

/// A protocol-side uninstall (nobody called `NativeKeyboard::uninstall`, so
/// no synchronous edge) still DELIVERS the falling edge: clearing the
/// bridge must not bump past the event that announces it.
#[test]
fn a_protocol_side_uninstall_still_delivers_the_edge() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    assert!(bridge.focused());
    let _ = bridge.drain_for_test();

    harness.server.state.uninstall_native_input();
    let edges: Vec<_> = bridge
        .drain_for_test()
        .into_iter()
        .filter_map(|(_, _, event)| match event {
            NativeQueuedEvent::Focus { focused, .. } => Some(focused),
            _ => None,
        })
        .collect();
    assert_eq!(edges, [false], "the release edge was discarded: {edges:?}");
}

/// Losing the keyboard ends the content's IME session. It cannot end it
/// itself once its requests stop at the gate, and the candidate window would
/// otherwise sit over the client that now has the keyboard.
#[test]
fn losing_the_keyboard_deactivates_the_input_method() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let surface = test_toplevel_record(&harness).role.wl_surface().clone();
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    let input_method = bind_test_input_method(&mut harness);
    enable_native_ime(&mut harness);
    let events = input_method_events(&harness.sync(), input_method);
    assert!(events.contains(&IM_EVENT_ACTIVATE), "{events:?}");

    // A client takes the keyboard by name.
    harness.server.state.activate_managed_window(&surface);
    assert!(!bridge.focused());
    let events = input_method_events(&harness.sync(), input_method);
    assert!(
        events.contains(&IM_EVENT_DEACTIVATE),
        "the field's IME session outlived its focus: {events:?}"
    );
}

/// A focused client's text input still takes priority: the sink only gets
/// what no client would, and the content's own requests stop at the door
/// rather than deactivating the input method under that client.
#[test]
fn a_focused_client_text_input_outranks_the_sink() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let surface = test_toplevel_record(&harness).role.wl_surface().clone();
    harness.server.state.activate_managed_window(&surface);
    let (bridge, _owner) = native_bridge(&mut harness);
    let input_method = bind_test_input_method(&mut harness);

    // The focused client enables a text input, as any editor does.
    let manager = harness.bind_test_global("zwp_text_input_manager_v3", 1);
    let text_input = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        manager,
        1,
        &words(&[text_input, TEST_SEAT_ID]),
    );
    let _ = harness.sync();
    send_request(&mut harness.client, text_input, 1, &[]);
    send_request(&mut harness.client, text_input, 7, &[]);
    let _ = harness.sync();

    // The content is not focused and a client holds the input method: its
    // requests are dropped, not forwarded.
    let before = harness.sync().len();
    for request in [
        NativeImeRequest::Enable(true),
        NativeImeRequest::Caret {
            x: 10,
            y: 10,
            width: 2,
            height: 18,
        },
        NativeImeRequest::Enable(false),
    ] {
        harness.server.state.service_native_ime_request(request);
    }
    let events = input_method_events(&harness.sync(), input_method);
    assert!(
        !events.contains(&IM_EVENT_DEACTIVATE),
        "the client's composition was ended by the content: {events:?} (before {before})"
    );

    send_request(
        &mut harness.client,
        input_method,
        IM_COMMIT_STRING,
        &wire_string_argument("client"),
    );
    send_request(&mut harness.client, input_method, IM_COMMIT, &words(&[0]));
    harness.dispatch_client();
    assert!(
        native_ime_events(&bridge).is_empty(),
        "the client's text input took it"
    );
    let traffic = harness.sync();
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == text_input && *opcode == 3),
        "the client received commit_string: {traffic:?}"
    );
}

/// A batch keeps the destination it started with WITHIN a session. A client
/// activating is a new session: it releases the latch, the tail follows the
/// new field, and the half the compositor field saw dies with its
/// generation. What must never happen is the tail landing in a place that
/// drops it — that is the blocker covered by
/// `an_abandoned_native_batch_does_not_swallow_the_next_client_batch`.
#[test]
fn a_client_activating_mid_batch_takes_the_input_method() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let surface = test_toplevel_record(&harness).role.wl_surface().clone();
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    enable_native_ime(&mut harness);
    let input_method = bind_test_input_method(&mut harness);
    let _ = harness.sync();

    // The batch starts while the compositor owns the input method.
    let mut preedit = wire_string_argument("half");
    preedit.extend_from_slice(&0_i32.to_ne_bytes());
    preedit.extend_from_slice(&4_i32.to_ne_bytes());
    send_request(
        &mut harness.client,
        input_method,
        IM_SET_PREEDIT_STRING,
        &preedit,
    );
    harness.dispatch_client();

    // Mid-batch a client takes the keyboard and activates a text input.
    harness.server.state.activate_managed_window(&surface);
    let manager = harness.bind_test_global("zwp_text_input_manager_v3", 1);
    let text_input = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        manager,
        1,
        &words(&[text_input, TEST_SEAT_ID]),
    );
    let _ = harness.sync();
    send_request(&mut harness.client, text_input, 1, &[]);
    send_request(&mut harness.client, text_input, 7, &[]);
    let _ = harness.sync();

    // The rest of the batch follows the new session, to the client.
    send_request(
        &mut harness.client,
        input_method,
        IM_COMMIT_STRING,
        &wire_string_argument("done"),
    );
    send_request(&mut harness.client, input_method, IM_COMMIT, &words(&[0]));
    harness.dispatch_client();
    let traffic = harness.sync();
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == text_input && *opcode == 3),
        "the client that now owns the input method got nothing: {traffic:?}"
    );
    // And the half the field had saw dies with the generation the focus
    // edge bumped, so the field never applies a preedit it cannot finish.
    assert!(
        native_ime_events(&bridge).is_empty(),
        "the field kept half a composition it will never be able to end"
    );
}

/// A candidate popup for compositor-drawn content has no parent surface, so
/// it is anchored under the caret the content reported — not under the
/// client rectangle the same popup would otherwise use — and it is
/// dismissed when the field disables.
#[test]
fn ime_popup_anchors_on_the_native_caret_and_is_dismissed() {
    let mut harness = KeybindingHarness::new(true);
    let (bridge, owner) = native_bridge(&mut harness);
    take_focus(&mut harness, &bridge, owner, true);
    let input_method = bind_test_input_method(&mut harness);
    // The caret the CONTENT reports, and a different rectangle in the
    // parent-relative space the client path would use. If the fallback ever
    // stopped firing, the popup would land on the second one.
    for request in [
        NativeImeRequest::Enable(true),
        NativeImeRequest::Caret {
            x: 120,
            y: 200,
            width: 2,
            height: 18,
        },
        NativeImeRequest::Done,
    ] {
        harness.server.state.service_native_ime_request(request);
    }

    let surface = harness.allocate_object_id();
    let popup = harness.allocate_object_id();
    send_request(
        &mut harness.client,
        TEST_COMPOSITOR_ID,
        0,
        &words(&[surface]),
    );
    send_request(
        &mut harness.client,
        input_method,
        IM_GET_INPUT_POPUP_SURFACE,
        &words(&[popup, surface]),
    );
    harness.dispatch_client();
    harness.assert_client_connected("after creating the candidate window");
    let record = harness
        .server
        .state
        .surfaces
        .values()
        .find(|record| matches!(record.role, SurfaceRole::ImePopup(_)))
        .expect("the compositor adopted the parentless popup");
    assert_eq!(
        (record.layout.x, record.layout.y),
        (120.0, 218.0),
        "the popup sits under the caret the content reported"
    );

    // Make the two branches DISAGREE: move the content's caret without
    // telling the input method, so the popup's own `text_input_rectangle`
    // still says (120, 200). The fallback must answer from the caret; the
    // upstream arm would answer (120, 218) again.
    let popup_surface = harness
        .server
        .state
        .surfaces
        .values()
        .find_map(|record| match &record.role {
            SurfaceRole::ImePopup(popup) => Some((**popup).clone()),
            _ => None,
        })
        .expect("the popup surface");
    assert_eq!(
        (
            popup_surface.text_input_rectangle().loc.x,
            popup_surface.text_input_rectangle().loc.y
        ),
        (120, 200),
        "the rectangle the upstream arm would use"
    );
    bridge.note_request(&NativeImeRequest::Caret {
        x: 300,
        y: 400,
        width: 2,
        height: 20,
    });
    assert_eq!(
        harness.server.state.ime_popup_anchor(&popup_surface),
        (300, 420).into(),
        "the anchor follows the content's caret, not the popup rectangle"
    );

    // The caret is GLOBAL logical space, so a caret past the first output's
    // width anchors there rather than being read as an offset inside it.
    bridge.note_request(&NativeImeRequest::Caret {
        x: 2400,
        y: 130,
        width: 2,
        height: 18,
    });
    assert_eq!(
        harness.server.state.ime_popup_anchor(&popup_surface),
        (2400, 148).into(),
        "a caret on another output is passed through, not clamped or offset"
    );

    // Disabling the field dismisses it; a parentless popup used to stay on
    // screen for ever.
    harness
        .server
        .state
        .service_native_ime_request(NativeImeRequest::Enable(false));
    assert!(
        !harness
            .server
            .state
            .surfaces
            .values()
            .any(|record| matches!(record.role, SurfaceRole::ImePopup(_))),
        "the sink's popup was dismissed with the field"
    );
}
