// `comp.input.*` through the real seat path (included from tests.rs).

use crate::port::{BTN_LEFT, InputOp, KeySpec, PointerMoveTarget, PressAction, ScrollSource};

const KEY_A: u32 = 30;
const KEY_B: u32 = 48;
const KEY_C: u32 = 46;
const KEY_K: u32 = 37;
const KEY_M: u32 = 50;
const KEY_O: u32 = 24;
const KEY_LEFTSHIFT: u32 = 42;

fn inject(
    harness: &mut KeybindingHarness,
    ingress: &crate::port::PortIngress,
    runtime: &tokio::runtime::Runtime,
    op: InputOp,
) -> (u8, Value) {
    let admission = ingress.request_input(op).expect("input admitted");
    serviced_control_reply(harness, runtime, admission)
}

fn bind_bare_pointer(harness: &mut KeybindingHarness) -> u32 {
    let pointer = harness.allocate_object_id();
    send_request(&mut harness.client, TEST_SEAT_ID, 0, &words(&[pointer]));
    let _ = harness.sync();
    pointer
}

/// Put a mapped toplevel at a known place and size (no CSD offset).
fn place_record(harness: &mut KeybindingHarness, object: &ObjectId, rect: (f32, f32, f32, f32)) {
    let record = harness
        .server
        .state
        .surfaces
        .get_mut(object)
        .expect("record");
    record.layout.x = rect.0;
    record.layout.y = rect.1;
    record.layout.width = rect.2;
    record.layout.height = rect.3;
    record.layout.visible = true;
    let delta = (rect.0 - record.window_origin.0, rect.1 - record.window_origin.1);
    record.window_origin = (rect.0, rect.1);
    record.committed_window_geometry = None;
    let id = record.id;
    harness.server.state.shift_surface_descendants(id, delta);
}

fn raise(harness: &mut KeybindingHarness, object: &ObjectId) {
    let surface = harness.server.state.surfaces[object]
        .role
        .wl_surface()
        .clone();
    harness.server.state.raise_surface(&surface);
}

fn focused_object(harness: &KeybindingHarness) -> Option<ObjectId> {
    focused_surface(harness.server.state.keyboard.current_focus()).map(|surface| surface.id())
}

fn two_windows() -> (
    KeybindingHarness,
    crate::port::PortIngress,
    tokio::runtime::Runtime,
    u32,
    ObjectId,
    ObjectId,
) {
    let (mut harness, ingress, _observations) = KeybindingHarness::new_with_port();
    map_initial_test_toplevel(&mut harness);
    let alpha = test_toplevel_record(&harness).role.wl_surface().id();
    let (_, _, _, beta) = map_named_test_toplevel(&mut harness, "Beta", "dev.cosmix.Beta");
    let pointer = bind_bare_pointer(&mut harness);
    place_record(&mut harness, &alpha, (0.0, 0.0, 200.0, 150.0));
    place_record(&mut harness, &beta, (300.0, 0.0, 100.0, 100.0));
    (harness, ingress, control_reply_runtime(), pointer, alpha, beta)
}

/// Gate G3's core in-process: a window-local move and a click land on the
/// window under the point, focus and raise it, and the event times share
/// CLOCK_MONOTONIC with the reply.
#[test]
fn injected_move_and_click_reach_the_window_under_the_point() {
    let (mut harness, ingress, runtime, pointer, alpha, beta) = two_windows();
    raise(&mut harness, &beta);
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    // Start on empty space, so reaching Alpha is an enter.
    let (rc, away) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerMove(PointerMoveTarget::Output {
            output: None,
            x: 250.0,
            y: 200.0,
        }),
    );
    assert_eq!(rc, 0, "{away}");
    assert_eq!(away["target"], Value::Null);
    let _ = harness.sync();

    let (rc, moved) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerMove(PointerMoveTarget::Window {
            id: alpha_id,
            generation: alpha_generation,
            x: 40.0,
            y: 30.0,
            require_hit: true,
        }),
    );
    assert_eq!(rc, 0, "{moved}");
    assert_eq!(
        moved["target"],
        json!({"id": alpha_id, "generation": alpha_generation})
    );
    assert_eq!(moved["pointer"]["x"], 40.0);
    assert_eq!(moved["pointer"]["y"], 30.0);
    assert!(moved["pointer"]["output"].is_string());
    let entered = harness.sync();
    let enter = pointer_body(&entered, pointer, 0);
    assert_eq!(word(&enter, 1), TEST_TOPLEVEL_SURFACE_ID);
    assert_eq!((fixed(&enter, 2), fixed(&enter, 3)), (40.0, 30.0));

    let (rc, clicked) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Both,
        },
    );
    assert_eq!(rc, 0, "{clicked}");
    assert!(clicked["input_seq"].as_u64() > moved["input_seq"].as_u64());
    let traffic = harness.sync();
    let buttons = pointer_bodies(&traffic, pointer, 3);
    assert_eq!(
        buttons
            .iter()
            .map(|body| (word(body, 2), word(body, 3)))
            .collect::<Vec<_>>(),
        [(BTN_LEFT, 1), (BTN_LEFT, 0)]
    );
    // Event time and reply share one clock.
    let injected_ms = (clicked["injected_at_us"].as_u64().unwrap() / 1_000) as u32;
    assert_eq!(word(&buttons[0], 1), injected_ms);
    let now_ms = monotonic_millis();
    assert!(now_ms.wrapping_sub(injected_ms) < 5_000);

    // The click raised and focused Alpha, exactly as a device click does.
    assert_eq!(focused_object(&harness), Some(alpha.clone()));
    let surfaces = &harness.server.state.surfaces;
    assert!(surfaces[&alpha].layout.z > surfaces[&beta].layout.z);
    assert!(harness.server.state.pointer.current_pressed().is_empty());
    assert_eq!(
        harness.server.state.take_input_mark(SurfaceId(alpha_id)),
        Some(input_injection::InputMark {
            input_seq: clicked["input_seq"].as_u64().unwrap(),
            injected_at_us: clicked["injected_at_us"].as_u64().unwrap(),
        })
    );
}

#[test]
fn require_hit_refuses_an_occluded_point_without_moving() {
    let (mut harness, ingress, runtime, _pointer, alpha, beta) = two_windows();
    place_record(&mut harness, &beta, (0.0, 0.0, 100.0, 100.0));
    raise(&mut harness, &beta);
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    harness.server.state.cursor_position = (250.0, 250.0);
    let window = |x, y, require_hit| {
        InputOp::PointerMove(PointerMoveTarget::Window {
            id: alpha_id,
            generation: alpha_generation,
            x,
            y,
            require_hit,
        })
    };

    let (rc, body) = inject(&mut harness, &ingress, &runtime, window(40.0, 30.0, true));
    assert_eq!(rc, 10);
    assert_eq!(
        body,
        json!({
            "error": "occluded",
            "id": alpha_id,
            "under": {"id": beta_id, "generation": beta_generation},
        })
    );
    assert_eq!(harness.server.state.cursor_position, (250.0, 250.0));

    // Alpha's exposed part is a hit.
    let (rc, body) = inject(&mut harness, &ingress, &runtime, window(150.0, 120.0, true));
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["target"]["id"], alpha_id);
    // Without require_hit the move goes where it was told.
    let (rc, body) = inject(&mut harness, &ingress, &runtime, window(40.0, 30.0, false));
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["target"]["id"], beta_id);

    // Stale and unknown targets are refused before anything moves.
    let stale = InputOp::PointerMove(PointerMoveTarget::Window {
        id: alpha_id,
        generation: alpha_generation + 1,
        x: 1.0,
        y: 1.0,
        require_hit: false,
    });
    let (rc, body) = inject(&mut harness, &ingress, &runtime, stale);
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "stale_target");
    assert_eq!(harness.server.state.cursor_position, (40.0, 30.0));
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerMove(PointerMoveTarget::Output {
            output: Some("o_nowhere".into()),
            x: 1.0,
            y: 1.0,
        }),
    );
    assert_eq!(rc, 10);
    assert_eq!(body, json!({"error": "unknown_output", "output": "o_nowhere"}));
    let (width, height) = harness.server.state.backend.seat_extent();
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerMove(PointerMoveTarget::Output {
            output: None,
            x: f64::from(width),
            y: 0.0,
        }),
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "out_of_bounds");
    assert_eq!(body["height"], height);
    assert_eq!(harness.server.state.cursor_position, (40.0, 30.0));

    // Relative motion adds to the cursor like a relative device.
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerMove(PointerMoveTarget::Relative { dx: 5.0, dy: -2.0 }),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(harness.server.state.cursor_position, (45.0, 28.0));
}

/// An injected chord that matches a binding is consumed by the binding,
/// exactly like a real one: Super+Shift+M restores and the client never
/// sees the M.
#[test]
fn injected_binding_chord_is_consumed_by_the_binding() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&surface);
    assert!(harness.server.state.surfaces[&alpha].minimized);
    let _ = harness.sync();

    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Key {
            key: KeySpec::Name("m".into()),
            action: PressAction::Both,
            modifiers: vec![
                KeySpec::Name("Super_L".into()),
                KeySpec::Name("Shift_L".into()),
            ],
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert!(!harness.server.state.surfaces[&alpha].minimized);
    assert_eq!(focused_object(&harness), Some(alpha.clone()));
    let keys = keyboard_key_events(&harness.sync());
    assert!(
        keys.iter().all(|(key, _)| *key != KEY_M),
        "the binding swallowed M: {keys:?}"
    );
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
    assert!(harness.server.state.injection.held_keys.is_empty());

    // An unknown key name is refused and sends nothing.
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Key {
            key: KeySpec::Name("NoSuchKeyName".into()),
            action: PressAction::Both,
            modifiers: vec![KeySpec::Name("Shift_L".into())],
        },
    );
    assert_eq!(rc, 10);
    assert_eq!(body, json!({"error": "unknown_key", "key": "NoSuchKeyName"}));
    assert!(keyboard_key_events(&harness.sync()).is_empty());
}

#[test]
fn text_types_through_the_keymap_and_refuses_unmappable_input_whole() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    let _ = harness.sync();
    let (alpha_id, _) = window_id_and_generation(&harness, &alpha);

    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Text("oK".into()),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["target"]["id"], alpha_id);
    assert_eq!(
        keyboard_key_events(&harness.sync()),
        [
            (KEY_O, 1),
            (KEY_O, 0),
            (KEY_LEFTSHIFT, 1),
            (KEY_K, 1),
            (KEY_K, 0),
            (KEY_LEFTSHIFT, 0),
        ]
    );

    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Text("ok\u{1F600}".into()),
    );
    assert_eq!(rc, 10);
    assert_eq!(
        body,
        json!({"error": "unmappable", "char": "\u{1F600}", "index": 2})
    );
    assert!(
        keyboard_key_events(&harness.sync()).is_empty(),
        "nothing of a refused text is typed"
    );
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
}

/// release_all releases what injections hold and nothing a device holds;
/// taps never leave a key down.
#[test]
fn release_all_clears_only_injected_holds() {
    let (mut harness, ingress, runtime, pointer, alpha, _beta) = two_windows();
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    harness.key(KEY_B, HostButtonState::Pressed);
    for op in [
        InputOp::PointerMove(PointerMoveTarget::Output {
            output: None,
            x: 10.0,
            y: 10.0,
        }),
        InputOp::Key {
            key: KeySpec::Evdev(KEY_A),
            action: PressAction::Press,
            modifiers: Vec::new(),
        },
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Press,
        },
        InputOp::Key {
            key: KeySpec::Name("c".into()),
            action: PressAction::Both,
            modifiers: Vec::new(),
        },
    ] {
        let (rc, body) = inject(&mut harness, &ingress, &runtime, op);
        assert_eq!(rc, 0, "{body}");
    }
    let pressed = |harness: &KeybindingHarness| {
        let mut keys = harness
            .server
            .state
            .keyboard
            .pressed_keys()
            .into_iter()
            .map(|key| key.raw() - 8)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys
    };
    assert_eq!(pressed(&harness), [KEY_A, KEY_B], "the tap left C up");
    assert_eq!(harness.server.state.pointer.current_pressed(), [BTN_LEFT]);
    let _ = harness.sync();

    let (rc, body) = inject(&mut harness, &ingress, &runtime, InputOp::ReleaseAll);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(pressed(&harness), [KEY_B], "the device-held key stays down");
    assert!(harness.server.state.pointer.current_pressed().is_empty());
    let traffic = harness.sync();
    assert_eq!(keyboard_key_events(&traffic), [(KEY_A, 0)]);
    assert_eq!(
        pointer_bodies(&traffic, pointer, 3)
            .iter()
            .map(|body| (word(body, 2), word(body, 3)))
            .collect::<Vec<_>>(),
        [(BTN_LEFT, 0)]
    );
    // Idempotent: nothing is held any more.
    let (rc, _) = inject(&mut harness, &ingress, &runtime, InputOp::ReleaseAll);
    assert_eq!(rc, 0);
    assert!(keyboard_key_events(&harness.sync()).is_empty());
}

#[test]
fn injected_scroll_keeps_absent_axes_absent() {
    let (mut harness, ingress, runtime, pointer, _alpha, _beta) = two_windows();
    let (rc, _) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerMove(PointerMoveTarget::Output {
            output: None,
            x: 20.0,
            y: 20.0,
        }),
    );
    assert_eq!(rc, 0);
    let _ = harness.sync();

    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerScroll {
            dx: None,
            dy: Some(15.0),
            source: ScrollSource::Wheel,
            v120: (None, Some(120)),
        },
    );
    assert_eq!(rc, 0, "{body}");
    let traffic = harness.sync();
    let axes = pointer_bodies(&traffic, pointer, 4)
        .iter()
        .map(|body| (word(body, 1), fixed(body, 2)))
        .collect::<Vec<_>>();
    assert_eq!(axes, [(AXIS_VERTICAL, 15.0)]);
    assert!(axis_stops(&traffic, pointer).is_empty());

    // A finger's reported zero is a stop, on that axis only.
    let (rc, _) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerScroll {
            dx: Some(0.0),
            dy: None,
            source: ScrollSource::Finger,
            v120: (None, None),
        },
    );
    assert_eq!(rc, 0);
    let traffic = harness.sync();
    assert_eq!(axis_stops(&traffic, pointer), [AXIS_HORIZONTAL]);
    assert!(pointer_bodies(&traffic, pointer, 4).is_empty());
}

/// Under a session lock the seat delivers injected input only to the lock
/// surface, and the reply says so.
#[test]
fn injection_while_locked_reaches_only_the_lock_surface() {
    let (mut harness, ingress, _observations) = KeybindingHarness::new_with_port();
    let runtime = control_reply_runtime();
    map_initial_test_toplevel(&mut harness);
    let pointer = harness.bind_pointer();
    let lock = begin_test_session_lock(&mut harness);
    ack_and_map_test_lock_surface(&mut harness, lock);
    let _ = harness.sync();
    let lock_record = test_lock_record(&harness, lock.surface);
    let lock_target = json!({"id": lock_record.id.0, "generation": lock_record.generation});

    let (rc, moved) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerMove(PointerMoveTarget::Output {
            output: None,
            x: 30.0,
            y: 30.0,
        }),
    );
    assert_eq!(rc, 0, "{moved}");
    assert_eq!(moved["target"], lock_target);
    let (rc, _) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Both,
        },
    );
    assert_eq!(rc, 0);
    let (rc, typed) = inject(&mut harness, &ingress, &runtime, InputOp::Text("o".into()));
    assert_eq!(rc, 0, "{typed}");
    assert_eq!(typed["target"], lock_target);
    let traffic = harness.sync();
    assert!(keyboard_key_events(&traffic).contains(&(KEY_O, 1)));
    assert!(pointer_bodies(&traffic, pointer, 0)
        .iter()
        .all(|enter| word(enter, 1) == lock.surface));
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus())
            .map(|surface| surface.id().protocol_id()),
        Some(lock.surface)
    );
    assert!(!test_toplevel_record(&harness).focused);
}

/// `input.host.passthrough=false` drops host pointer and key input but
/// never strands a host hold, and never releases an injected one.
#[test]
fn host_passthrough_off_drops_host_input_without_stranding_holds() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    let time = monotonic_millis();
    harness.frame(vec![
        HostInput::key_from_evdev(KEY_C, HostButtonState::Pressed, time),
        HostInput::PointerButton {
            button: BTN_LEFT,
            state: HostButtonState::Pressed,
            time,
        },
    ]);
    let (rc, _) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Key {
            key: KeySpec::Evdev(KEY_B),
            action: PressAction::Press,
            modifiers: Vec::new(),
        },
    );
    assert_eq!(rc, 0);

    let path = port_observation::HOST_PASSTHROUGH_PATH.to_string();
    let admission = ingress
        .request_set(path.clone(), json!(false))
        .expect("set admitted");
    let (rc, body) = serviced_control_reply(&mut harness, &runtime, admission);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body, json!({"path": path, "old": true, "new": false}));
    let context = harness.server.state.port_context.clone().expect("context");
    let snapshot = port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot");
    assert_eq!(
        snapshot.input.host,
        Some(port_snapshot::HostInputSnapshot { passthrough: false })
    );

    harness.server.state.cursor_position = (5.0, 5.0);
    harness.frame(vec![
        HostInput::PointerMotionAbsolute {
            x: 150.0,
            y: 100.0,
            time,
        },
        HostInput::key_from_evdev(KEY_A, HostButtonState::Pressed, time),
        HostInput::PointerButton {
            button: 0x111,
            state: HostButtonState::Pressed,
            time,
        },
        // The host button held from before still gets its release.
        HostInput::PointerButton {
            button: BTN_LEFT,
            state: HostButtonState::Released,
            time,
        },
        // A focus loss releases the host-held C only.
        HostInput::KeyboardFocusLost,
    ]);
    let state = &harness.server.state;
    assert_eq!(state.cursor_position, (5.0, 5.0));
    assert!(state.pointer.current_pressed().is_empty());
    assert_eq!(
        state
            .keyboard
            .pressed_keys()
            .into_iter()
            .map(|key| key.raw() - 8)
            .collect::<Vec<_>>(),
        [KEY_B]
    );

    // Back on: host input flows again.
    let admission = ingress.request_set(path, json!(true)).expect("admitted");
    let (rc, _) = serviced_control_reply(&mut harness, &runtime, admission);
    assert_eq!(rc, 0);
    harness.frame(vec![HostInput::PointerMotionAbsolute {
        x: 150.0,
        y: 100.0,
        time,
    }]);
    assert_eq!(harness.server.state.cursor_position, (150.0, 100.0));
}

#[test]
fn host_passthrough_leaf_does_not_exist_on_kms() {
    let (mut harness, ingress, _observations) =
        KeybindingHarness::new_with_port_backend(BackendKind::Kms, "kms");
    let runtime = control_reply_runtime();
    let admission = ingress
        .request_set(
            port_observation::HOST_PASSTHROUGH_PATH.into(),
            json!(false),
        )
        .expect("admitted");
    let (rc, body) = serviced_control_reply(&mut harness, &runtime, admission);
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "unknown_path");
    let context = harness.server.state.port_context.clone().expect("context");
    if let Some(snapshot) = port_snapshot::snapshot(&harness.server.state, &context) {
        assert_eq!(snapshot.input.host, None);
    }
}

fn run_sequence(
    harness: &mut KeybindingHarness,
    ingress: &crate::port::PortIngress,
    runtime: &tokio::runtime::Runtime,
    steps: Vec<crate::port::SequenceStep>,
) -> (u8, Value) {
    let admission = ingress
        .request_long(crate::port::LongOp::Sequence(steps))
        .expect("sequence admitted");
    long_reply(harness, runtime, admission, |state| {
        state.injection.sequences.is_empty()
    })
}

/// Drive the protocol loop (its timers included) until `done`, then read
/// the long verb's reply.
fn long_reply(
    harness: &mut KeybindingHarness,
    runtime: &tokio::runtime::Runtime,
    admission: crate::port::LongAdmission,
    done: impl Fn(&WaylandState) -> bool,
) -> (u8, Value) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        harness
            .server
            .dispatch_cycle(Some(Duration::from_millis(5)))
            .expect("long verb cycle");
        if done(&harness.server.state) {
            break;
        }
        assert!(Instant::now() < deadline, "long verb never answered");
    }
    let reply = runtime
        .block_on(admission.receive())
        .expect("long verb reply");
    let (rc, body) = reply.into_wire();
    (rc, serde_json::from_str(&body).unwrap())
}

fn step(verb: &'static str, op: InputOp, delay_ms: u64) -> crate::port::SequenceStep {
    crate::port::SequenceStep {
        verb,
        op,
        delay: Duration::from_millis(delay_ms),
    }
}

/// A timed drag runs its steps in order on calloop timers; a failing step
/// aborts the run and releases the held button.
#[test]
fn sequence_runs_timed_steps_and_releases_on_failure() {
    let (mut harness, ingress, runtime, pointer, alpha, _beta) = two_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let started = Instant::now();
    let (rc, body) = run_sequence(
        &mut harness,
        &ingress,
        &runtime,
        vec![
            step(
                "comp.input.pointer.move",
                InputOp::PointerMove(PointerMoveTarget::Output {
                    output: None,
                    x: 10.0,
                    y: 10.0,
                }),
                0,
            ),
            step(
                "comp.input.pointer.button",
                InputOp::PointerButton {
                    button: BTN_LEFT,
                    action: PressAction::Press,
                },
                0,
            ),
            step(
                "comp.input.pointer.move",
                InputOp::PointerMove(PointerMoveTarget::Relative { dx: 20.0, dy: 5.0 }),
                30,
            ),
            step(
                "comp.input.pointer.button",
                InputOp::PointerButton {
                    button: BTN_LEFT,
                    action: PressAction::Release,
                },
                30,
            ),
        ],
    );
    assert_eq!(rc, 0, "{body}");
    assert!(started.elapsed() >= Duration::from_millis(60));
    assert!(body["elapsed_ms"].as_u64().unwrap() >= 60);
    let steps = body["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 4);
    assert!(
        steps
            .windows(2)
            .all(|pair| pair[0]["input_seq"].as_u64() < pair[1]["input_seq"].as_u64())
    );
    assert_eq!(harness.server.state.cursor_position, (30.0, 15.0));
    assert!(harness.server.state.pointer.current_pressed().is_empty());
    let traffic = harness.sync();
    assert_eq!(pointer_bodies(&traffic, pointer, 3).len(), 2);

    let (rc, body) = run_sequence(
        &mut harness,
        &ingress,
        &runtime,
        vec![
            step(
                "comp.input.pointer.button",
                InputOp::PointerButton {
                    button: BTN_LEFT,
                    action: PressAction::Press,
                },
                0,
            ),
            step(
                "comp.input.pointer.move",
                InputOp::PointerMove(PointerMoveTarget::Window {
                    id: alpha_id,
                    generation: alpha_generation + 7,
                    x: 1.0,
                    y: 1.0,
                    require_hit: false,
                }),
                10,
            ),
            step("comp.input.release_all", InputOp::ReleaseAll, 0),
        ],
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "step_failed");
    assert_eq!(body["index"], 1);
    assert_eq!(body["verb"], "comp.input.pointer.move");
    assert_eq!(body["step"]["error"], "stale_target");
    assert_eq!(body["completed"].as_array().unwrap().len(), 1);
    assert_eq!(body["released"], true);
    assert!(harness.server.state.pointer.current_pressed().is_empty());
    assert!(harness.server.state.injection.sequences.is_empty());
}

#[test]
fn event_time_base_is_clock_monotonic() {
    let mut expected = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: expected points to a valid, writable timespec.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut expected) },
        0
    );
    let expected_us = expected.tv_sec as u64 * 1_000_000 + expected.tv_nsec as u64 / 1_000;
    let micros = monotonic_micros();
    assert!(micros >= expected_us && micros - expected_us < 1_000_000);
    let millis = monotonic_millis();
    assert!(millis.wrapping_sub((expected_us / 1_000) as u32) < 1_000);
}
