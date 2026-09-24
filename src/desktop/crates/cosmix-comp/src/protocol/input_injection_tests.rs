// `comp.input.*` through the real seat path (included from tests.rs).

use crate::port::{BTN_LEFT, ControlReply, InputOp, KeySpec, PointerMoveTarget, PressAction, ScrollSource};

const KEY_2: u32 = 3;
const KEY_RIGHTBRACE: u32 = 27;
const KEY_A: u32 = 30;
const KEY_B: u32 = 48;
const KEY_C: u32 = 46;
const KEY_K: u32 = 37;
const KEY_M: u32 = 50;
const KEY_O: u32 = 24;
const KEY_LEFTSHIFT: u32 = 42;

#[test]
fn keyboard_without_an_owner_refuses_and_cleans_up_generated_holds() {
    let (mut harness, ingress, _) = KeybindingHarness::new_with_port();
    let runtime = control_reply_runtime();
    harness
        .server
        .state
        .apply_corner_config(corner::CornerConfig {
            enabled: false,
            ..corner::CornerConfig::default()
        });
    harness.server.state.keyboard.clone().set_focus(
        &mut harness.server.state,
        None,
        SERIAL_COUNTER.next_serial(),
    );
    for op in [
        InputOp::Text("a".into()),
        InputOp::Key {
            key: KeySpec::Evdev(KEY_A),
            action: PressAction::Both,
            modifiers: vec![KeySpec::Name("Shift_L".into())],
        },
    ] {
        let (rc, body) = inject(&mut harness, &ingress, &runtime, op);
        assert_eq!(rc, 10);
        assert_eq!(body["error"], "no_keyboard_target");
        assert!(harness.server.state.injection.held.is_empty());
        assert!(harness.server.state.keyboard.pressed_keys().is_empty());
        assert_eq!(body["target"], Value::Null);
    }
}

#[test]
fn bare_modifier_without_focus_is_held_for_a_following_binding() {
    let (mut harness, ingress, _) = KeybindingHarness::new_with_port();
    let runtime = control_reply_runtime();
    // The harness maps a focused toplevel; this test wants no client focus.
    harness.server.state.keyboard.clone().set_focus(
        &mut harness.server.state,
        None,
        SERIAL_COUNTER.next_serial(),
    );
    assert!(harness.server.state.keyboard.current_focus().is_none());
    assert_eq!(harness.server.state.workspace_current(), 1);
    let key = |name: &str, action| InputOp::Key {
        key: KeySpec::Name(name.into()),
        action,
        modifiers: vec![],
    };
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        key("Super_L", PressAction::Press),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["target"], Value::Null);
    assert!(!harness.server.state.injection.held.is_empty());
    assert!(harness.server.state.keyboard.modifier_state().logo);

    // An unhandled payload is refused without releasing the earlier prefix.
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        key("a", PressAction::Press),
    );
    assert_eq!(rc, 10, "{body}");
    assert_eq!(body["error"], "no_keyboard_target");
    assert!(harness.server.state.keyboard.modifier_state().logo);
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        key("2", PressAction::Both),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(harness.server.state.workspace_current(), 2);
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        key("Super_L", PressAction::Release),
    );
    assert_eq!(rc, 0, "{body}");
    assert!(harness.server.state.injection.held.is_empty());
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
    assert!(!harness.server.state.keyboard.modifier_state().logo);
}

#[test]
fn release_and_already_held_keys_work_after_focus_is_lost() {
    let (mut harness, ingress, runtime, _, _, _) = two_windows();
    let key = |action| InputOp::Key {
        key: KeySpec::Name("Shift_L".into()),
        action,
        modifiers: vec![],
    };
    assert_eq!(
        inject(&mut harness, &ingress, &runtime, key(PressAction::Press)).0,
        0
    );
    harness.server.state.keyboard.clone().set_focus(
        &mut harness.server.state,
        None,
        SERIAL_COUNTER.next_serial(),
    );
    for action in [
        PressAction::Press,
        PressAction::Release,
        PressAction::Release,
    ] {
        let (rc, body) = inject(&mut harness, &ingress, &runtime, key(action));
        assert_eq!(rc, 0, "{body}");
    }
    assert!(harness.server.state.injection.held.is_empty());
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
    assert!(!harness.server.state.keyboard.modifier_state().shift);
}

#[test]
fn targeted_text_stops_after_a_binding_changes_focus() {
    for (text, completed, blocked) in [("\ta", 2, KEY_A), ("a\tb", 4, KEY_B)] {
        let (mut harness, ingress, runtime, _, alpha, beta) = two_windows();
        let (id, generation) = window_id_and_generation(&harness, &alpha);
        let (rc, body) = inject(
            &mut harness,
            &ingress,
            &runtime,
            InputOp::Key {
                key: KeySpec::Name("Alt_L".into()),
                action: PressAction::Press,
                modifiers: vec![],
            },
        );
        assert_eq!(rc, 0, "{body}");
        let _ = harness.sync();
        let (rc, body) = inject(
            &mut harness,
            &ingress,
            &runtime,
            InputOp::Targeted {
                id,
                generation,
                raise: true,
                op: Box::new(InputOp::Text(text.into())),
            },
        );
        assert_eq!(rc, 10, "{body}");
        assert_eq!(body["error"], "target_changed");
        assert_eq!(body["completed_events"], completed);
        assert_eq!(
            body["target"],
            if completed == 2 {
                Value::Null
            } else {
                json!({"id":id,"generation":generation})
            }
        );
        assert_eq!(body["targeted"], json!({"id":id,"generation":generation}));
        assert_eq!(focused_object(&harness), Some(beta));
        let keys = keyboard_key_events(&harness.sync());
        assert!(keys.iter().all(|(key, _)| *key != blocked), "{keys:?}");
        let _ = inject(&mut harness, &ingress, &runtime, InputOp::ReleaseAll);
        assert!(harness.server.state.injection.held.is_empty());
        assert!(harness.server.state.keyboard.pressed_keys().is_empty());
    }
}

#[test]
fn restore_binding_works_with_every_window_minimized() {
    let (mut harness, ingress, runtime, _, alpha, beta) = two_windows();
    for object in [&alpha, &beta] {
        let surface = harness.server.state.surfaces[object]
            .role
            .wl_surface()
            .clone();
        harness.server.state.minimize_toplevel(&surface);
    }
    assert!(harness.server.state.keyboard.current_focus().is_none());
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
    assert_eq!(body["target"], Value::Null);
    assert!(!harness.server.state.surfaces[&beta].minimized);
    assert!(harness.server.state.injection.held.is_empty());
}

#[test]
fn targeted_binding_reports_consumption_separately_from_intended_window() {
    let (mut harness, ingress, runtime, _, alpha, _) = two_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Targeted {
            id,
            generation,
            raise: false,
            op: Box::new(InputOp::Key {
                key: KeySpec::Name("2".into()),
                action: PressAction::Both,
                modifiers: vec![KeySpec::Name("Super_L".into())],
            }),
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(harness.server.state.workspace_current(), 2);
    assert_eq!(body["target"], Value::Null);
    assert_eq!(body["targeted"], json!({"id":id,"generation":generation}));
    assert!(harness.server.state.injection.held.is_empty());
}

#[test]
fn targeted_buttons_obey_kms_delivery_and_quarantine_gates() {
    for blocked in [true, false] {
        let (mut harness, _, _) = KeybindingHarness::new_with_port_backend(BackendKind::Kms, "kms");
        let output = kms_security_test_key(226, "Input-test");
        submit_kms_security_lifecycle(
            &mut harness,
            KmsTopologyLifecycleEvent::Initial(kms_security_test_snapshot(&output, 41)),
        );
        map_initial_test_toplevel(&mut harness);
        let record = test_toplevel_record(&harness);
        let (id, generation) = (record.id.0, record.generation);
        if blocked {
            harness.server.state.kms_session_lock_gate.deferred_unlock = true;
        } else {
            harness
                .server
                .state
                .kms_session_lock_gate
                .quarantine_current_input([], [BTN_LEFT]);
        }
        let before = harness.server.state.injection.events;
        let reply = harness.server.state.service_input_op(&InputOp::Targeted {
            id,
            generation,
            raise: false,
            op: Box::new(InputOp::PointerButton {
                button: BTN_LEFT,
                action: PressAction::Both,
            }),
        });
        if blocked {
            // Deferred unlock keeps session_lock_active() true; targeted
            // input rejects that before the surface-presentability check.
            let wire = reply.wire_json();
            assert_eq!(wire["error"], "target_unfocusable");
            assert_eq!(wire["reason"], "session_lock");
            assert_eq!(harness.server.state.injection.events, before);
        } else {
            assert!(matches!(&reply, ControlReply::Body(_)), "{reply:?}");
            assert_eq!(reply.wire_json()["target"], Value::Null);
        }
        assert!(harness.server.state.pointer.current_pressed().is_empty());
        assert!(
            !harness
                .server
                .state
                .kms_session_lock_gate
                .suppressed_buttons
                .contains(&BTN_LEFT)
        );
        assert!(harness.server.state.injection.held.is_empty());
    }
}

#[test]
fn targeted_button_release_respects_corner_ownership() {
    let (mut harness, ingress, runtime, pointer, alpha, _) = two_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    harness
        .server
        .state
        .apply_corner_config(corner::CornerConfig::default());
    route_pointer_to(&mut harness, 1.0, 1.0);
    let _ = harness.sync();
    harness
        .server
        .event_loop
        .dispatch(Some(Duration::from_millis(250)), &mut harness.server.state)
        .unwrap();
    assert!(harness.server.state.corner_engaged());
    harness.server.state.keyboard.clone().set_focus(
        &mut harness.server.state,
        None,
        SERIAL_COUNTER.next_serial(),
    );
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Key {
            key: KeySpec::Evdev(KEY_A),
            action: PressAction::Press,
            modifiers: vec![],
        },
    );
    assert_eq!(rc, 10, "a corner does not consume keys: {body}");
    assert_eq!(body["error"], "no_keyboard_target");
    assert_eq!(body["target"], Value::Null);
    assert!(harness.server.state.injection.held.is_empty());
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
    route_pointer_button(&mut harness, BTN_LEFT, ButtonState::Pressed);
    route_pointer_to(&mut harness, 100.0, 100.0);
    let _ = harness.sync();
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Targeted {
            id,
            generation,
            raise: false,
            op: Box::new(InputOp::PointerButton {
                button: BTN_LEFT,
                action: PressAction::Release,
            }),
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["target"], Value::Null);
    assert!(pointer_bodies(&harness.sync(), pointer, 3).is_empty());
    assert!(!harness.server.state.consume_corner_release(BTN_LEFT));
}

#[test]
fn targeted_key_and_button_focus_without_raising_when_requested() {
    let (mut harness, ingress, runtime, pointer, alpha, beta) = two_windows();
    raise(&mut harness, &beta);
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let z = harness.server.state.surfaces[&alpha].layout.z;
    // The cursor is outside Alpha: a targeted button must not click Beta.
    let (rc, moved) = inject(
        &mut harness,
        &ingress,
        &runtime,
        move_op(PointerMoveTarget::Output {
            output: None,
            x: 310.0,
            y: 20.0,
        }),
    );
    assert_eq!(rc, 0, "{moved}");
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    assert_eq!(
        moved["target"],
        json!({"id": beta_id, "generation": beta_generation})
    );
    assert_eq!(harness.server.state.cursor_position, (310.0, 20.0));
    let _ = harness.sync();
    for op in [
        InputOp::Key {
            key: KeySpec::Evdev(KEY_A),
            action: PressAction::Both,
            modifiers: vec![],
        },
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Both,
        },
    ] {
        let (rc, body) = inject(
            &mut harness,
            &ingress,
            &runtime,
            InputOp::Targeted {
                id,
                generation,
                raise: false,
                op: Box::new(op),
            },
        );
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["target"], json!({"id":id,"generation":generation}));
        assert_eq!(focused_object(&harness), Some(alpha.clone()));
        assert_eq!(harness.server.state.surfaces[&alpha].layout.z, z);
    }
    let traffic = harness.sync();
    assert_eq!(pointer_bodies(&traffic, pointer, 3).len(), 2);
    assert_eq!(keyboard_key_events(&traffic), vec![(KEY_A, 1), (KEY_A, 0)]);
    assert!(harness.server.state.pointer.current_pressed().is_empty());
    assert_ne!(
        harness
            .server
            .state
            .pointer
            .current_focus()
            .and_then(|target| target.owned_surface())
            .map(|surface| surface.id()),
        Some(alpha.clone())
    );
    assert_eq!(harness.server.state.cursor_position, (310.0, 20.0));
    assert_eq!(
        harness
            .server
            .state
            .pointer
            .current_focus()
            .and_then(|target| target.owned_surface())
            .map(|surface| surface.id()),
        Some(beta.clone())
    );
    let op = crate::port::parse_input_op(
        "comp.input.key",
        &json!({
            "key":"a", "window":{"id":id,"generation":generation},
        }),
    )
    .unwrap();
    let (rc, body) = inject(&mut harness, &ingress, &runtime, op);
    assert_eq!(rc, 0, "{body}");
    assert!(
        harness.server.state.surfaces[&alpha].layout.z
            > harness.server.state.surfaces[&beta].layout.z
    );
}

#[test]
fn targeted_input_refusals_inject_nothing_and_do_not_switch_workspaces() {
    let (mut harness, ingress, runtime, _, alpha, _) = two_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let targeted = |generation| InputOp::Targeted {
        id,
        generation,
        raise: true,
        op: Box::new(InputOp::Text("a".into())),
    };
    let before = harness.server.state.injection.events;
    let (rc, body) = inject(&mut harness, &ingress, &runtime, targeted(generation + 1));
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "stale_target");
    for (field, reason) in [
        ("minimized", "minimized"),
        ("mapped", "unmapped"),
        ("workspace", "other_workspace"),
    ] {
        {
            let record = harness.server.state.surfaces.get_mut(&alpha).unwrap();
            record.minimized = field == "minimized";
            record.mapped = field != "mapped";
            record.workspace = if field == "workspace" { 2 } else { 1 };
        }
        let (rc, body) = inject(&mut harness, &ingress, &runtime, targeted(generation));
        assert_eq!(rc, 10, "{body}");
        assert_eq!(body["error"], "target_unfocusable");
        assert_eq!(body["reason"], reason);
        assert_eq!(harness.server.state.workspace_current(), 1);
        assert_eq!(harness.server.state.injection.events, before);
    }
}

fn move_op(target: PointerMoveTarget) -> InputOp {
    InputOp::PointerMove {
        target,
        corners: true,
    }
}

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
    let delta = (
        rect.0 - record.window_origin.0,
        rect.1 - record.window_origin.1,
    );
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
    (
        harness,
        ingress,
        control_reply_runtime(),
        pointer,
        alpha,
        beta,
    )
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
        move_op(PointerMoveTarget::Output {
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
        move_op(PointerMoveTarget::Window {
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
    // The mark went to the one store, the presentation stats registry.
    let injected_at_us = clicked["injected_at_us"].as_u64().unwrap();
    let input_seq = clicked["input_seq"].as_u64().unwrap();
    assert_eq!(
        harness
            .server
            .state
            .presentation
            .stats
            .input_mark(input_seq, injected_at_us),
        Some(input_injection::InputMark {
            input_seq,
            injected_at_us,
        })
    );
    let alpha_generation = harness.server.state.surfaces[&alpha].generation;
    assert!(
        harness
            .server
            .state
            .presentation
            .stats
            .window(alpha_id, alpha_generation)
            .is_some(),
        "the click marked Alpha's window"
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
        move_op(PointerMoveTarget::Window {
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
    let stale = move_op(PointerMoveTarget::Window {
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
        move_op(PointerMoveTarget::Output {
            output: Some("o_nowhere".into()),
            x: 1.0,
            y: 1.0,
        }),
    );
    assert_eq!(rc, 10);
    assert_eq!(
        body,
        json!({"error": "unknown_output", "output": "o_nowhere"})
    );
    let (width, height) = harness.server.state.backend.seat_extent();
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        move_op(PointerMoveTarget::Output {
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
        move_op(PointerMoveTarget::Relative { dx: 5.0, dy: -2.0 }),
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
    assert!(harness.server.state.injection.held.is_empty());

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
    assert_eq!(
        body,
        json!({"error": "unknown_key", "key": "NoSuchKeyName"})
    );
    assert!(keyboard_key_events(&harness.sync()).is_empty());
}

/// An injected workspace chord is consumed like a real one: Super+2 switches
/// and Super+bracketright steps, and the client never sees the digit or the
/// bracket.
#[test]
fn injected_workspace_chord_is_consumed() {
    let (mut harness, ingress, runtime, _pointer, alpha, beta) = two_windows();
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    let _ = harness.sync();
    assert_eq!(harness.server.state.workspace_current(), 1);

    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Key {
            key: KeySpec::Name("2".into()),
            action: PressAction::Both,
            modifiers: vec![KeySpec::Name("Super_L".into())],
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(harness.server.state.workspace_current(), 2);
    assert!(!harness.server.state.surfaces[&alpha].layout.visible);
    assert_eq!(harness.server.state.surfaces[&alpha].workspace, 1);
    let keys = keyboard_key_events(&harness.sync());
    assert!(
        keys.iter().all(|(key, _)| *key != KEY_2),
        "the binding swallowed the digit: {keys:?}"
    );

    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Key {
            key: KeySpec::Name("bracketright".into()),
            action: PressAction::Both,
            modifiers: vec![KeySpec::Name("Super_L".into())],
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["target"], Value::Null);
    assert_eq!(harness.server.state.workspace_current(), 3);
    let keys = keyboard_key_events(&harness.sync());
    assert!(
        keys.iter().all(|(key, _)| *key != KEY_RIGHTBRACE),
        "the binding swallowed the bracket: {keys:?}"
    );

    // The move chord through the injected keymap's Shift level. Both
    // windows are hidden on 1; the binding is still consumed with no focus.
    let move_chord = |harness: &mut KeybindingHarness| {
        inject(
            harness,
            &ingress,
            &runtime,
            InputOp::Key {
                key: KeySpec::Name("2".into()),
                action: PressAction::Both,
                modifiers: vec![
                    KeySpec::Name("Super_L".into()),
                    KeySpec::Name("Shift_L".into()),
                ],
            },
        )
    };
    let (rc, body) = move_chord(&mut harness);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["target"], Value::Null);
    assert_eq!(harness.server.state.workspace_current(), 3);
    assert_eq!(harness.server.state.surfaces[&alpha].workspace, 1);

    // Back on 1 with alpha focused, the same chord moves alpha to 2 and
    // follows it; the level-0 digit is still swallowed under Shift.
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        InputOp::Key {
            key: KeySpec::Name("1".into()),
            action: PressAction::Both,
            modifiers: vec![KeySpec::Name("Super_L".into())],
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(harness.server.state.workspace_current(), 1);
    harness.server.state.activate_managed_window(&surface);
    let _ = harness.sync();
    let (rc, body) = move_chord(&mut harness);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(harness.server.state.workspace_current(), 2);
    assert_eq!(harness.server.state.surfaces[&alpha].workspace, 2);
    assert!(harness.server.state.surfaces[&alpha].layout.visible);
    assert_eq!(harness.server.state.surfaces[&beta].workspace, 1);
    assert_eq!(focused_object(&harness), Some(alpha.clone()));
    let keys = keyboard_key_events(&harness.sync());
    assert!(
        keys.iter().all(|(key, _)| *key != KEY_2),
        "the move chord swallowed the digit under Shift: {keys:?}"
    );
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
    assert!(harness.server.state.injection.held.is_empty());
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

    let (rc, body) = inject(&mut harness, &ingress, &runtime, InputOp::Text("oK".into()));
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
        move_op(PointerMoveTarget::Output {
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
        move_op(PointerMoveTarget::Output {
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
    let window = test_toplevel_record(&harness);
    let targeted = InputOp::Targeted {
        id:window.id.0, generation:window.generation, raise:true,
        op:Box::new(InputOp::Text("a".into())),
    };
    let before = harness.server.state.injection.events;
    let (rc, body) = inject(&mut harness, &ingress, &runtime, targeted);
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "target_unfocusable");
    assert_eq!(body["reason"], "session_lock");
    assert_eq!(harness.server.state.injection.events, before);

    let (rc, moved) = inject(
        &mut harness,
        &ingress,
        &runtime,
        move_op(PointerMoveTarget::Output {
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
    assert!(
        pointer_bodies(&traffic, pointer, 0)
            .iter()
            .all(|enter| word(enter, 1) == lock.surface)
    );
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
        .request_set(port_observation::HOST_PASSTHROUGH_PATH.into(), json!(false))
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
                move_op(PointerMoveTarget::Output {
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
                move_op(PointerMoveTarget::Relative { dx: 20.0, dy: 5.0 }),
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
                move_op(PointerMoveTarget::Window {
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

/// A failing sequence releases only what it pressed: another caller's held
/// button and key stay down.
#[test]
fn a_failed_sequence_releases_only_its_own_holds() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    for op in [
        move_op(PointerMoveTarget::Output {
            output: None,
            x: 10.0,
            y: 10.0,
        }),
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Press,
        },
        InputOp::Key {
            key: KeySpec::Evdev(KEY_A),
            action: PressAction::Press,
            modifiers: Vec::new(),
        },
    ] {
        let (rc, body) = inject(&mut harness, &ingress, &runtime, op);
        assert_eq!(rc, 0, "{body}");
    }
    let (rc, body) = run_sequence(
        &mut harness,
        &ingress,
        &runtime,
        vec![
            step(
                "comp.input.key",
                InputOp::Key {
                    key: KeySpec::Evdev(KEY_B),
                    action: PressAction::Press,
                    modifiers: Vec::new(),
                },
                0,
            ),
            step(
                "comp.input.pointer.button",
                InputOp::PointerButton {
                    button: 0x111,
                    action: PressAction::Press,
                },
                0,
            ),
            step(
                "comp.input.pointer.move",
                move_op(PointerMoveTarget::Window {
                    id: alpha_id,
                    generation: alpha_generation + 3,
                    x: 1.0,
                    y: 1.0,
                    require_hit: false,
                }),
                0,
            ),
        ],
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "step_failed");
    let state = &harness.server.state;
    let mut keys = state
        .keyboard
        .pressed_keys()
        .into_iter()
        .map(|key| key.raw() - 8)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [KEY_A],
        "the run's B was released, the other A was not"
    );
    assert_eq!(state.pointer.current_pressed(), [BTN_LEFT]);
    // release_all is still global.
    let (rc, _) = inject(&mut harness, &ingress, &runtime, InputOp::ReleaseAll);
    assert_eq!(rc, 0);
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
    assert!(harness.server.state.pointer.current_pressed().is_empty());
}

/// Two sequences interleave on timers; one failing leaves the other's
/// hold alone, and that one still completes.
#[test]
fn concurrent_sequences_keep_separate_holds() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let press = |key| InputOp::Key {
        key: KeySpec::Evdev(key),
        action: PressAction::Press,
        modifiers: Vec::new(),
    };
    let release = |key| InputOp::Key {
        key: KeySpec::Evdev(key),
        action: PressAction::Release,
        modifiers: Vec::new(),
    };
    let long = ingress
        .request_long(crate::port::LongOp::Sequence(vec![
            step("comp.input.key", press(KEY_A), 0),
            step("comp.input.key", release(KEY_A), 80),
        ]))
        .expect("long run admitted");
    let failing = ingress
        .request_long(crate::port::LongOp::Sequence(vec![
            step("comp.input.key", press(KEY_B), 0),
            step(
                "comp.input.pointer.move",
                move_op(PointerMoveTarget::Window {
                    id: alpha_id,
                    generation: alpha_generation + 1,
                    x: 0.0,
                    y: 0.0,
                    require_hit: false,
                }),
                20,
            ),
        ]))
        .expect("failing run admitted");
    let (rc, body) = long_reply(&mut harness, &runtime, failing, |state| {
        state.injection.sequences.len() == 1
    });
    assert_eq!((rc, body["error"].clone()), (10, json!("step_failed")));
    let pressed = harness
        .server
        .state
        .keyboard
        .pressed_keys()
        .into_iter()
        .map(|key| key.raw() - 8)
        .collect::<Vec<_>>();
    assert_eq!(
        pressed,
        [KEY_A],
        "the other run's hold survives the failure"
    );
    let (rc, body) = long_reply(&mut harness, &runtime, long, |state| {
        state.injection.sequences.is_empty()
    });
    assert_eq!(rc, 0, "{body}");
    assert!(harness.server.state.keyboard.pressed_keys().is_empty());
}

/// A long zero-delay run yields to the loop instead of injecting
/// everything in one callback.
#[test]
fn a_long_sequence_yields_between_chunks() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    let steps = (0..30)
        .map(|_| step("comp.input.key", InputOp::Text("abcdefghij".into()), 0))
        .collect::<Vec<_>>();
    let admission = ingress
        .request_long(crate::port::LongOp::Sequence(steps))
        .expect("admitted");
    let before = harness.server.state.injection.events;
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("first cycle");
    let first_pass = harness.server.state.injection.events - before;
    assert!(
        (256..256 + 40).contains(&first_pass),
        "one pass stops at the yield: {first_pass}"
    );
    assert_eq!(harness.server.state.injection.sequences.len(), 1);
    let (rc, body) = long_reply(&mut harness, &runtime, admission, |state| {
        state.injection.sequences.is_empty()
    });
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["steps"].as_array().unwrap().len(), 30);
    assert_eq!(harness.server.state.injection.events - before, 30 * 20);
    harness.assert_client_connected("the client kept up with the run");
}

/// `corners: false` moves the pointer without arming a hot corner; a
/// default move into the corner arms it like a mouse.
#[test]
fn corners_false_skips_hot_corner_sampling() {
    let (mut harness, ingress, observations) = KeybindingHarness::new_with_port();
    let runtime = control_reply_runtime();
    port_observation::service_observations(&mut harness.server.state);
    harness.server.state.refresh_corner_regions();
    drain_observations(&observations);
    let corner_move = |corners| InputOp::PointerMove {
        target: PointerMoveTarget::Output {
            output: None,
            x: 5.0,
            y: 5.0,
        },
        corners,
    };
    let entered = |harness: &mut KeybindingHarness| {
        harness
            .server
            .event_loop
            .dispatch(Some(Duration::from_millis(250)), &mut harness.server.state)
            .expect("corner deadline dispatch");
        port_observation::service_observations(&mut harness.server.state);
        drain_observations(&observations).iter().any(|record| {
            matches!(
                record,
                port_observation::ObservationRecord::CornerEntered { .. }
            )
        })
    };
    harness.server.state.cursor_position = (100.0, 100.0);
    let (rc, _) = inject(&mut harness, &ingress, &runtime, corner_move(false));
    assert_eq!(rc, 0);
    assert_eq!(harness.server.state.cursor_position, (5.0, 5.0));
    assert!(!harness.server.state.injection.suppress_corners);
    assert!(!entered(&mut harness), "corners:false arms nothing");

    let (rc, _) = inject(
        &mut harness,
        &ingress,
        &runtime,
        move_op(PointerMoveTarget::Output {
            output: None,
            x: 100.0,
            y: 100.0,
        }),
    );
    assert_eq!(rc, 0);
    let (rc, _) = inject(&mut harness, &ingress, &runtime, corner_move(true));
    assert_eq!(rc, 0);
    assert!(entered(&mut harness), "a default move arms the corner");
}

fn key_op(key: u32, action: PressAction) -> InputOp {
    InputOp::Key {
        key: KeySpec::Evdev(key),
        action,
        modifiers: Vec::new(),
    }
}

fn pressed_evdev(harness: &KeybindingHarness) -> Vec<u32> {
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
}

fn failing_move(alpha_id: u64, stale_generation: u64, delay_ms: u64) -> crate::port::SequenceStep {
    step(
        "comp.input.pointer.move",
        move_op(PointerMoveTarget::Window {
            id: alpha_id,
            generation: stale_generation,
            x: 0.0,
            y: 0.0,
            require_hit: false,
        }),
        delay_ms,
    )
}

/// Holds are counted by owner: two runs pressing the same key, or a run
/// and a single verb, never release each other's hold on abort.
#[test]
fn shared_holds_are_released_by_their_last_owner() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let key_a = input_injection::Hold::Key(KEY_A + 8);

    // Two runs, one key.
    let keeper = ingress
        .request_long(crate::port::LongOp::Sequence(vec![
            step("comp.input.key", key_op(KEY_A, PressAction::Press), 0),
            step("comp.input.key", key_op(KEY_A, PressAction::Release), 80),
        ]))
        .expect("keeper admitted");
    let failing = ingress
        .request_long(crate::port::LongOp::Sequence(vec![
            step("comp.input.key", key_op(KEY_A, PressAction::Press), 0),
            failing_move(alpha_id, alpha_generation + 1, 20),
        ]))
        .expect("failing admitted");
    let (rc, _) = long_reply(&mut harness, &runtime, failing, |state| {
        state.injection.sequences.len() == 1
    });
    assert_eq!(rc, 10);
    assert_eq!(pressed_evdev(&harness), [KEY_A], "the keeper still holds A");
    assert_eq!(harness.server.state.injection.held.owners_of(key_a), 1);
    let (rc, _) = long_reply(&mut harness, &runtime, keeper, |state| {
        state.injection.sequences.is_empty()
    });
    assert_eq!(rc, 0);
    assert!(pressed_evdev(&harness).is_empty());
    assert!(harness.server.state.injection.held.is_empty());

    // A single verb re-presses what a run held after releasing it: the run's
    // abort leaves the single verb's hold down.
    let run = ingress
        .request_long(crate::port::LongOp::Sequence(vec![
            step("comp.input.key", key_op(KEY_A, PressAction::Press), 0),
            failing_move(alpha_id, alpha_generation + 1, 60),
        ]))
        .expect("run admitted");
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("run starts");
    assert_eq!(pressed_evdev(&harness), [KEY_A]);
    for action in [PressAction::Release, PressAction::Press] {
        let (rc, _) = inject(&mut harness, &ingress, &runtime, key_op(KEY_A, action));
        assert_eq!(rc, 0);
    }
    assert_eq!(
        harness.server.state.injection.held.owners_of(key_a),
        1,
        "the release cleared the run's claim; the press is the verb's"
    );
    let (rc, _) = long_reply(&mut harness, &runtime, run, |state| {
        state.injection.sequences.is_empty()
    });
    assert_eq!(rc, 10);
    assert_eq!(pressed_evdev(&harness), [KEY_A], "the verb's hold survives");

    // And the plain case: a verb's hold survives a run that shared it.
    let (rc, body) = run_sequence(
        &mut harness,
        &ingress,
        &runtime,
        vec![
            step("comp.input.key", key_op(KEY_A, PressAction::Press), 0),
            failing_move(alpha_id, alpha_generation + 1, 0),
        ],
    );
    assert_eq!((rc, body["error"].clone()), (10, json!("step_failed")));
    assert_eq!(pressed_evdev(&harness), [KEY_A]);
    let (rc, _) = inject(&mut harness, &ingress, &runtime, InputOp::ReleaseAll);
    assert_eq!(rc, 0);
    assert!(pressed_evdev(&harness).is_empty());
}

/// `corners: false` suppresses arming only: it still disarms a pending
/// dwell and leaves an engaged corner.
#[test]
fn corners_false_still_leaves_and_disarms() {
    let (mut harness, ingress, observations) = KeybindingHarness::new_with_port();
    let runtime = control_reply_runtime();
    port_observation::service_observations(&mut harness.server.state);
    harness.server.state.refresh_corner_regions();
    drain_observations(&observations);
    let to = |x, y, corners| InputOp::PointerMove {
        target: PointerMoveTarget::Output { output: None, x, y },
        corners,
    };
    let settle = |harness: &mut KeybindingHarness| {
        harness
            .server
            .event_loop
            .dispatch(Some(Duration::from_millis(250)), &mut harness.server.state)
            .expect("corner deadline dispatch");
        port_observation::service_observations(&mut harness.server.state);
        drain_observations(&observations)
    };
    harness.server.state.cursor_position = (100.0, 100.0);

    // Arm (default move), then move away with corners:false before the
    // dwell: nothing enters.
    assert_eq!(
        inject(&mut harness, &ingress, &runtime, to(5.0, 5.0, true)).0,
        0
    );
    assert_eq!(
        inject(&mut harness, &ingress, &runtime, to(100.0, 100.0, false)).0,
        0
    );
    let records = settle(&mut harness);
    assert!(
        !records.iter().any(|record| matches!(
            record,
            port_observation::ObservationRecord::CornerEntered { .. }
        )),
        "the pending dwell was disarmed: {records:?}"
    );

    // Engage, then leave with corners:false: the corner is left.
    assert_eq!(
        inject(&mut harness, &ingress, &runtime, to(5.0, 5.0, true)).0,
        0
    );
    let records = settle(&mut harness);
    assert!(records.iter().any(|record| matches!(
        record,
        port_observation::ObservationRecord::CornerEntered { .. }
    )));
    assert_eq!(
        inject(&mut harness, &ingress, &runtime, to(100.0, 100.0, false)).0,
        0
    );
    let records = settle(&mut harness);
    assert!(
        records.iter().any(|record| matches!(
            record,
            port_observation::ObservationRecord::CornerLeft { .. }
        )),
        "a suppressed move still leaves the corner: {records:?}"
    );
}

/// `require_hit` on a point no output shows is `off_output`, not an
/// occlusion by nothing.
#[test]
fn require_hit_off_every_output_is_off_output() {
    let (mut harness, ingress, runtime, _pointer, alpha, _beta) = two_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let (rc, body) = inject(
        &mut harness,
        &ingress,
        &runtime,
        move_op(PointerMoveTarget::Window {
            id,
            generation,
            x: -10.0,
            y: 5.0,
            require_hit: true,
        }),
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "off_output", "{body}");
}
