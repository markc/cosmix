// Keyboard and IME for in-process content (included from tests.rs).

use crate::native_input::{
    NativeImeBridge, NativeImeEvent, NativeImeRequest, NativeKeyboardBridge,
};

/// `zwp_input_method_v2` request opcodes (the XML order).
const IM_COMMIT_STRING: u16 = 0;
const IM_SET_PREEDIT_STRING: u16 = 1;
const IM_DELETE_SURROUNDING_TEXT: u16 = 2;
const IM_COMMIT: u16 = 3;
const IM_GET_INPUT_POPUP_SURFACE: u16 = 4;
/// `zwp_input_method_v2` event opcodes.
const IM_EVENT_ACTIVATE: u16 = 0;
const IM_EVENT_DEACTIVATE: u16 = 1;
const IM_EVENT_SURROUNDING_TEXT: u16 = 2;
const IM_EVENT_CONTENT_TYPE: u16 = 4;
const IM_EVENT_DONE: u16 = 5;

const KEY_A: u32 = 30;
const KEY_M: u32 = 50;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;

fn native_bridges(harness: &mut KeybindingHarness) -> (NativeKeyboardBridge, NativeImeBridge) {
    let keyboard = NativeKeyboardBridge::default();
    let ime = NativeImeBridge::default();
    harness
        .server
        .state
        .install_native_input(keyboard.clone(), ime.clone());
    (keyboard, ime)
}

fn take_focus(harness: &mut KeybindingHarness, keyboard: &NativeKeyboardBridge, wanted: bool) {
    keyboard.set_wants_focus(wanted);
    harness.server.state.service_native_focus_request();
}

fn native_keys(keyboard: &NativeKeyboardBridge) -> Vec<(u32, bool, bool, Option<String>)> {
    keyboard
        .drain_for_test()
        .into_iter()
        .map(|event| (event.evdev, event.pressed, event.repeat, event.text))
        .collect()
}

/// In-process content is one more requester in arbitration: it takes the
/// keyboard from a client, gives it back, and never takes it from the
/// structural gates.
#[test]
fn native_content_is_one_more_focus_requester() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let object = test_toplevel_record(&harness).role.wl_surface().id();
    let surface = harness.server.state.surfaces[&object]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    assert!(harness.server.state.surfaces[&object].focused);
    let (keyboard, _ime) = native_bridges(&mut harness);

    take_focus(&mut harness, &keyboard, true);
    assert!(keyboard.focused(), "the content owns the keyboard");
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        None,
        "no client owns it while the content does"
    );
    assert!(!harness.server.state.surfaces[&object].focused);

    take_focus(&mut harness, &keyboard, false);
    assert!(!keyboard.focused());
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
    take_focus(&mut harness, &keyboard, true);
    assert!(!keyboard.focused(), "an exclusive layer keeps the keyboard");
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
    take_focus(&mut harness, &keyboard, true);
    assert!(!keyboard.focused(), "the lock screen keeps the keyboard");
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus())
            .map(|surface| surface.id().protocol_id()),
        Some(lock.surface)
    );
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
    let (keyboard, _ime) = native_bridges(&mut harness);

    // Before the content asks, the client gets the key.
    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert_eq!(keyboard_key_events(&harness.sync()), [(KEY_A, 1), (KEY_A, 0)]);
    assert!(native_keys(&keyboard).is_empty());

    take_focus(&mut harness, &keyboard, true);
    harness.key(KEY_A, HostButtonState::Pressed);
    harness.key(KEY_A, HostButtonState::Released);
    assert!(
        keyboard_key_events(&harness.sync()).is_empty(),
        "no client hears a key the content owns"
    );
    assert_eq!(
        native_keys(&keyboard),
        [
            (KEY_A, true, false, Some("a".into())),
            (KEY_A, false, false, Some("a".into())),
        ]
    );

    // A binding still wins: Super+Shift+M restores, and the M never reaches
    // the content.
    harness.server.state.minimize_toplevel(&surface);
    assert!(harness.server.state.surfaces[&object].minimized);
    harness.chord(&[KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_M]);
    assert!(!harness.server.state.surfaces[&object].minimized);
    let keys = native_keys(&keyboard);
    assert!(
        keys.iter().all(|(evdev, ..)| *evdev != KEY_M),
        "the binding swallowed M: {keys:?}"
    );
    assert!(
        keys.iter().any(|(evdev, ..)| *evdev == KEY_LEFTSHIFT),
        "the modifiers themselves still arrive: {keys:?}"
    );
    // The restore focused the window again, so the content gave up focus.
    take_focus(&mut harness, &keyboard, true);

    // Repeats: the compositor generates them, because the content has no
    // `wl_keyboard.repeat_info` to do it for itself.
    harness.key(KEY_A, HostButtonState::Pressed);
    assert_eq!(native_keys(&keyboard).len(), 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut repeats = 0;
    while repeats < 2 {
        harness
            .server
            .event_loop
            .dispatch(Some(Duration::from_millis(50)), &mut harness.server.state)
            .expect("repeat dispatch");
        repeats += native_keys(&keyboard)
            .iter()
            .filter(|(evdev, pressed, repeat, _)| *evdev == KEY_A && *pressed && *repeat)
            .count();
        assert!(Instant::now() < deadline, "no repeat arrived");
    }
    harness.key(KEY_A, HostButtonState::Released);
    let _ = native_keys(&keyboard);
    for _ in 0..4 {
        harness
            .server
            .event_loop
            .dispatch(Some(Duration::from_millis(50)), &mut harness.server.state)
            .expect("post-release dispatch");
    }
    assert!(
        native_keys(&keyboard).is_empty(),
        "the release ends the repeat"
    );

    // Giving the keyboard back restores client delivery.
    take_focus(&mut harness, &keyboard, false);
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
    let (keyboard, _ime) = native_bridges(&mut harness);
    take_focus(&mut harness, &keyboard, true);
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
    let text = native_keys(&keyboard)
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

/// The vendored sink both ways: input-method output reaches in-process
/// content when no client text input is active, and the content's own
/// state reaches the input method.
#[test]
fn ime_sink_carries_both_directions_for_native_content() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let (keyboard, ime) = native_bridges(&mut harness);
    take_focus(&mut harness, &keyboard, true);
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
        ime.drain_for_test(),
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

/// A focused client's text input still takes priority: the sink only gets
/// what no client would.
#[test]
fn a_focused_client_text_input_outranks_the_sink() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let surface = test_toplevel_record(&harness).role.wl_surface().clone();
    harness.server.state.activate_managed_window(&surface);
    let (_keyboard, ime) = native_bridges(&mut harness);
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

    send_request(
        &mut harness.client,
        input_method,
        IM_COMMIT_STRING,
        &wire_string_argument("client"),
    );
    send_request(&mut harness.client, input_method, IM_COMMIT, &words(&[0]));
    harness.dispatch_client();
    assert!(
        ime.drain_for_test().is_empty(),
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

/// A candidate popup for compositor-drawn content has no parent surface, so
/// it is anchored under the caret the content reported.
#[test]
fn ime_popup_anchors_on_the_native_caret() {
    let mut harness = KeybindingHarness::new(true);
    let (keyboard, _ime) = native_bridges(&mut harness);
    take_focus(&mut harness, &keyboard, true);
    let input_method = bind_test_input_method(&mut harness);
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
}
