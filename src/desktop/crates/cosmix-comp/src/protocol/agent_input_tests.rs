// Included by input_injection_tests.rs; real protocol devices, no display/GPU.

fn bind_agent_devices(h: &mut KeybindingHarness) -> (u32, u32, u32) {
    let (_, seat, _) = named_seat_discovery_traffic(h, AGENT_SEAT_NAME, false);
    let keyboard = h.allocate_object_id();
    let pointer = h.allocate_object_id();
    send_request(&mut h.client, seat, 1, &words(&[keyboard]));
    send_request(&mut h.client, seat, 0, &words(&[pointer]));
    let _ = h.sync();
    (seat, keyboard, pointer)
}

fn agent_target(h: &KeybindingHarness, object: &ObjectId, op: InputOp) -> InputOp {
    let (id, generation) = window_id_and_generation(h, object);
    InputOp::OnSeat {
        seat: SeatKind::Agent,
        op: Box::new(InputOp::Targeted {
            id,
            generation,
            raise: false,
            op: Box::new(op),
        }),
    }
}

fn agent_key(action: PressAction, key: u32) -> InputOp {
    InputOp::Key {
        key: KeySpec::Evdev(key),
        action,
        modifiers: vec![],
    }
}

fn on_agent(op: InputOp) -> InputOp {
    InputOp::OnSeat {
        seat: SeatKind::Agent,
        op: Box::new(op),
    }
}

#[test]
fn bare_release_all_cleans_both_seats_but_scoped_cleanup_preserves_the_other() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    for seat in ["human", "agent", "both"] {
        let agent = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_A));
        assert_eq!(inject(&mut h, &ingress, &runtime, agent).0, 0);
        assert_eq!(inject(&mut h, &ingress, &runtime, agent_key(PressAction::Press, KEY_B)).0, 0);
        let args = if seat == "both" { json!({}) } else { json!({"seat":seat}) };
        let op = crate::port::parse_input_op("comp.input.release_all", &args).unwrap();
        let (rc, body) = inject(&mut h, &ingress, &runtime, op);
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["seat"], seat);
        assert_eq!(h.server.state.human.held.is_empty(), seat != "agent");
        assert_eq!(h.server.state.agent.held.is_empty(), seat != "human");
        assert_eq!(inject(&mut h, &ingress, &runtime, InputOp::ReleaseAll).0, 0);
    }
}

#[test]
fn legacy_sequence_cleanup_does_not_make_it_an_agent_sequence() {
    let (mut h, _, runtime, _, _, _) = two_windows();
    let release = crate::port::parse_input_op("comp.input.release_all", &json!({})).unwrap();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(crate::port::LongOp::Sequence(vec![
        step("comp.input.key", agent_key(PressAction::Press, KEY_A), 0),
        step("comp.input.release_all", release, 1),
    ]), sender, Instant::now());
    h.server.state.cancel_agent_sequences();
    assert_eq!(h.server.state.injection.sequences.len(), 1);
    finish_test_sequences(&mut h);
    let reply = runtime.block_on(receiver).unwrap().wire_json();
    assert!(reply.get("error").is_none(), "{reply}");
    assert!(h.server.state.human.held.is_empty());
}

fn device_key_events(traffic: &[(u32, u16, Vec<u8>)], keyboard: u32) -> Vec<(u32, u32)> {
    traffic
        .iter()
        .filter(|(object, opcode, body)| *object == keyboard && *opcode == 3 && body.len() >= 16)
        .map(|(_, _, body)| (word(body, 2), word(body, 3)))
        .collect()
}

fn finish_test_sequences(h: &mut KeybindingHarness) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.server.state.injection.sequences.is_empty() {
        assert!(Instant::now() < deadline, "sequence did not finish");
        h.server.dispatch_cycle(Some(Duration::from_millis(5))).unwrap();
    }
}

#[test]
fn agent_implicit_drag_crosses_ssd_chrome() {
    let (mut h, _, alpha, _) = positioned_test_ssd_harness(cosmix_deco::ChromeStyle::Win11);
    bind_agent_devices(&mut h);
    let press = agent_target(&h, &alpha, InputOp::PointerButton { button: BTN_LEFT, action: PressAction::Press });
    assert!(matches!(h.server.state.service_input_op(&press), ControlReply::Body(_)));
    let (x, y) = chrome_titlebar_point(&h);
    assert!(matches!(h.server.state.pointer_target_at(x, y), Some(PointerTarget::Chrome { .. })));
    let op = on_agent(move_op(PointerMoveTarget::Output { output: None, x, y }));
    assert!(matches!(h.server.state.service_input_op(&op), ControlReply::Body(_)));
    assert_eq!(h.server.state.agent.pointer.current_focus().and_then(|focus| focus.owned_surface()).map(|surface| surface.id()), Some(alpha));
}

#[cfg(feature = "embedded-quoin")]
#[test]
fn agent_coordinates_refuse_embedded_panel_but_implicit_drag_crosses_it() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    place_record(&mut h, &alpha, (80.0, 90.0, 200.0, 150.0));
    let press = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    assert_eq!(inject(&mut h, &ingress, &runtime, press).0, 0);
    let (id, generation) = window_id_and_generation(&h, &alpha);
    assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(move_op(PointerMoveTarget::Window {
        id, generation, x: 20.0, y: 30.0, require_hit: true,
    }))).0, 0);
    let before = h.server.state.agent.pointer.current_focus();
    let position = h.server.state.agent.pointer_position;
    h.server.state.embedded_shell = Some(crate::embedded_shell::EmbeddedShellBridge::for_test(vec![
        cosmix_shell::host::PanelRect { x: 80.0, y: 90.0, width: 200.0, height: 150.0 },
    ]));
    let motion = on_agent(move_op(PointerMoveTarget::Output { output: None, x: 110.0, y: 125.0 }));
    let (rc, body) = inject(&mut h, &ingress, &runtime, motion.clone());
    assert_eq!(rc, 10, "{body}");
    assert_eq!(body["error"], "chrome_target");
    assert_eq!(h.server.state.agent.pointer.current_focus(), before);
    assert_eq!(h.server.state.agent.pointer_position, position);
    assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(InputOp::PointerButton {
        button: BTN_LEFT, action: PressAction::Press,
    })).0, 0);
    assert_eq!(inject(&mut h, &ingress, &runtime, motion).0, 0);
}

#[test]
fn default_bus_key_delivers_to_agent_without_moving_human_focus() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    raise(&mut h, &beta);
    let (_, keyboard, _) = bind_agent_devices(&mut h);
    let human = focused_object(&h);
    assert_ne!(human, Some(alpha.clone()));
    let (id, generation) = window_id_and_generation(&h, &alpha);
    let op = crate::port::parse_input_op("comp.input.key", &json!({
        "window":{"id":id,"generation":generation}, "key":"a",
    })).unwrap();
    let (rc, body) = inject(&mut h, &ingress, &runtime, op);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["seat"], "agent");
    assert_eq!(focused_object(&h), human);
    let traffic = h.sync();
    assert_eq!(device_key_events(&traffic, keyboard), [(KEY_A, 1), (KEY_A, 0)]);
    assert!(keyboard_key_events(&traffic).is_empty());
    assert!(toplevel_configure_states(&traffic, TEST_TOPLEVEL_ID).is_empty());
}

#[test]
fn agent_targeted_key_preserves_human_focus_activation_and_stack() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    let (_, keyboard, _) = bind_agent_devices(&mut h);
    let human = focused_object(&h);
    let before = [&alpha, &beta].map(|object| {
        let record = &h.server.state.surfaces[object];
        (record.focused, record.layout.z)
    });
    let op = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    let (rc, body) = inject(&mut h, &ingress, &runtime, op);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["seat"], "agent");
    assert_eq!(focused_object(&h), human);
    assert_eq!(
        [&alpha, &beta].map(|object| {
            let record = &h.server.state.surfaces[object];
            (record.focused, record.layout.z)
        }),
        before
    );
    let traffic = h.sync();
    assert_eq!(
        device_key_events(&traffic, keyboard),
        [(KEY_A, 1), (KEY_A, 0)]
    );
    assert!(keyboard_key_events(&traffic).is_empty());
    assert!(
        toplevel_configure_states(&traffic, TEST_TOPLEVEL_ID).is_empty(),
        "no Activated configure"
    );
}

#[test]
fn agent_modifiers_bindings_and_reply_scratch_are_independent() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (_, keyboard, _) = bind_agent_devices(&mut h);
    let op = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_LEFTSHIFT));
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    assert!(h.server.state.agent.keyboard.modifier_state().shift);
    assert!(!h.server.state.human.keyboard.modifier_state().shift);
    let agent_delivery = h.server.state.agent.delivery.key_delivery;
    let _ = h.sync();
    h.key(KEY_A, HostButtonState::Pressed);
    h.key(KEY_A, HostButtonState::Released);
    assert_eq!(h.server.state.agent.delivery.key_delivery, agent_delivery);
    assert_eq!(keyboard_key_events(&h.sync()), [(KEY_A, 1), (KEY_A, 0)]);
    // Human XKB is independently unshifted, so its A key resolves to lowercase.
    h.server
        .state
        .human
        .keyboard
        .clone()
        .with_xkb_state(&mut h.server.state, |context| {
            let xkb = context.xkb().lock().unwrap();
            assert_eq!(
                unsafe { xkb.state() }.key_get_utf8(Keycode::new(KEY_A + 8)),
                "a"
            );
        });
    assert_eq!(
        inject(&mut h, &ingress, &runtime, on_agent(InputOp::ReleaseAll)).0,
        0
    );
    h.server.state.surfaces.get_mut(&alpha).unwrap().minimized = true;
    h.server.state.recompute_effective_visibility();
    let op = agent_target(
        &h,
        &alpha,
        InputOp::Key {
            key: KeySpec::Evdev(KEY_M),
            action: PressAction::Both,
            modifiers: vec![
                KeySpec::Name("Super_L".into()),
                KeySpec::Name("Shift_L".into()),
            ],
        },
    );
    let _ = h.sync();
    let (rc, body) = inject(&mut h, &ingress, &runtime, op);
    assert_eq!(rc, 0, "{body}");
    assert!(
        h.server.state.surfaces[&alpha].minimized,
        "restore binding must not run"
    );
    assert!(device_key_events(&h.sync(), keyboard).contains(&(KEY_M, 1)));
}

#[test]
fn agent_background_click_preserves_workspace_and_human_cursor() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (_, _, pointer) = bind_agent_devices(&mut h);
    h.server.state.surfaces.get_mut(&alpha).unwrap().workspace = 2;
    h.server.state.recompute_effective_visibility();
    let callback = h.allocate_object_id();
    send_request(&mut h.client, alpha.protocol_id(), 3, &words(&[callback]));
    send_request(&mut h.client, alpha.protocol_id(), 6, &[]);
    h.dispatch_client();
    let cursor = h.server.state.cursor_position;
    let status = h.server.state.cursor_selection.clone();
    let focus = focused_object(&h);
    let (id, generation) = window_id_and_generation(&h, &alpha);
    let (rc, body) = inject(
        &mut h,
        &ingress,
        &runtime,
        on_agent(move_op(PointerMoveTarget::Window {
            id,
            generation,
            x: 40.0,
            y: 30.0,
            require_hit: true,
        })),
    );
    assert_eq!(rc, 0, "{body}");
    let op = agent_target(
        &h,
        &alpha,
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Both,
        },
    );
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    let traffic = h.sync();
    assert_eq!(pointer_bodies(&traffic, pointer, 3).len(), 2);
    assert_eq!(h.server.state.workspace_current(), 1);
    assert_eq!(focused_object(&h), focus);
    assert_eq!(h.server.state.cursor_position, cursor);
    assert_eq!(h.server.state.cursor_selection, status);
    assert!(!h.server.state.surfaces[&alpha].layout.visible);
    h.server.state.handle_frame(Vec::new());
    assert!(!h.sync().iter().any(|(object, opcode, _)| *object == callback && *opcode == 0),
        "agent delivery does not wake off-workspace frame callbacks");
}

#[test]
fn agent_popup_provenance_is_invalidated_by_device_focus_changes() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    bind_agent_devices(&mut h);
    let tap = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    assert_eq!(inject(&mut h, &ingress, &runtime, tap).0, 0);
    assert!(h.server.state.agent.last_keyboard_action.is_some());
    let other = h.server.state.surfaces[&beta].role.wl_surface().clone();
    h.server.state.agent.keyboard.clone().set_focus(&mut h.server.state, Some(other.into()), SERIAL_COUNTER.next_serial());
    assert!(h.server.state.agent.last_keyboard_action.is_none());
    let click = agent_target(&h, &alpha, InputOp::PointerButton { button: BTN_LEFT, action: PressAction::Both });
    assert_eq!(inject(&mut h, &ingress, &runtime, click).0, 0);
    assert!(h.server.state.agent.last_pointer_action.is_some());
    let (id, generation) = window_id_and_generation(&h, &beta);
    assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(move_op(PointerMoveTarget::Window {
        id, generation, x: 20.0, y: 20.0, require_hit: true,
    }))).0, 0);
    assert!(h.server.state.agent.last_pointer_action.is_none());
}

#[test]
fn agent_unbound_client_and_session_lock_refuse_before_focus() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    for payload in [
        agent_key(PressAction::Both, KEY_A),
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Both,
        },
    ] {
        let op = agent_target(&h, &alpha, payload);
        let (rc, body) = inject(&mut h, &ingress, &runtime, op);
        assert_eq!(rc, 10);
        assert_eq!(body["error"], "agent_seat_unbound");
        assert_eq!(body["hint"]["seat"], "human");
        assert!(h.server.state.agent.keyboard.current_focus().is_none());
        assert!(h.server.state.agent.pointer.current_focus().is_none());
    }
    bind_agent_devices(&mut h);
    let _ = request_test_session_lock(&mut h);
    let op = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    let (rc, body) = inject(&mut h, &ingress, &runtime, op);
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "session_lock");
    assert!(h.server.state.agent.keyboard.current_focus().is_none());
}

#[test]
fn bare_release_all_does_not_report_agent_cleanup_as_input() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let press = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_LEFTSHIFT));
    assert_eq!(inject(&mut h, &ingress, &runtime, press).0, 0);
    assert_eq!(inject(&mut h, &ingress, &runtime, agent_key(PressAction::Both, KEY_A)).0, 0);
    let origin = h.server.state.last_input_origin;
    let agent_time = h.server.state.agent.last_input_us;
    assert_eq!(origin, Some(SeatKind::Human));
    assert_eq!(inject(&mut h, &ingress, &runtime, InputOp::ReleaseAll).0, 0);
    assert!(h.server.state.agent.keyboard.pressed_keys().is_empty());
    assert_eq!(h.server.state.last_input_origin, origin);
    assert_eq!(h.server.state.agent.last_input_us, agent_time);
}

#[test]
fn agent_release_all_preserves_human_holds_and_origin_tracks_delivery() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    h.key(KEY_B, HostButtonState::Pressed);
    let human_time = h.server.state.human.last_input_us;
    let op = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_LEFTSHIFT));
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    assert_eq!(h.server.state.last_input_origin, Some(SeatKind::Agent));
    assert!(h.server.state.agent.last_input_us.is_some());
    assert_eq!(h.server.state.human.last_input_us, human_time);
    let release = crate::port::parse_input_op("comp.input.release_all", &json!({})).unwrap();
    assert_eq!(inject(&mut h, &ingress, &runtime, release).0, 0);
    assert!(h.server.state.agent.keyboard.pressed_keys().is_empty());
    assert!(
        h.server
            .state
            .human
            .keyboard
            .pressed_keys()
            .contains(&Keycode::new(KEY_B + 8))
    );
    h.key(KEY_B, HostButtonState::Released);
    assert_eq!(h.server.state.last_input_origin, Some(SeatKind::Human));
}

#[test]
fn agent_click_popup_survives_human_focus_and_outside_click_dismisses() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    let (seat, _, pointer) = bind_agent_devices(&mut h);
    let op = agent_target(
        &h,
        &alpha,
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Both,
        },
    );
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    assert!(!h.server.state.agent.pointer.is_grabbed(), "click released atomically");
    let traffic = h.sync();
    let serial = pointer_bodies(&traffic, pointer, 3).into_iter()
        .find(|body| word(body, 3) == 1).map(|body| word(&body, 0)).unwrap();
    let (_, popup) = map_test_popup_on_seat(&mut h, Some((seat, serial)));
    assert!(h.server.state.agent.pointer.is_grabbed());
    let other = h.server.state.surfaces[&beta].role.wl_surface().clone();
    h.server.state.human.keyboard.clone().set_focus(
        &mut h.server.state,
        Some(other.into()),
        SERIAL_COUNTER.next_serial(),
    );
    assert!(
        !h.sync()
            .iter()
            .any(|(object, opcode, _)| *object == popup && *opcode == 1)
    );
    assert_eq!(
        inject(
            &mut h,
            &ingress,
            &runtime,
            on_agent(InputOp::PointerButton {
                button: BTN_LEFT,
                action: PressAction::Release
            })
        )
        .0,
        0
    );
    assert_eq!(
        inject(
            &mut h,
            &ingress,
            &runtime,
            on_agent(move_op(PointerMoveTarget::Output {
                output: None,
                x: 250.0,
                y: 200.0,
            }))
        )
        .0,
        0
    );
    assert_eq!(
        inject(
            &mut h,
            &ingress,
            &runtime,
            on_agent(InputOp::PointerButton {
                button: BTN_LEFT,
                action: PressAction::Both
            })
        )
        .0,
        0
    );
    assert!(
        h.sync()
            .iter()
            .any(|(object, opcode, _)| *object == popup && *opcode == 1)
    );
    assert!(h.server.state.agent.popup_grab.is_none(), "outside dismissal retires stored chain");
}

#[test]
fn agent_vt_switch_and_unmap_clear_holds_and_focus() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let op = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_LEFTSHIFT));
    assert_eq!(inject(&mut h, &ingress, &runtime, op.clone()).0, 0);
    h.server
        .state
        .handle_binding_action(BindingAction::SwitchVt(2));
    assert!(h.server.state.agent.keyboard.current_focus().is_none());
    assert!(h.server.state.agent.keyboard.pressed_keys().is_empty());
    assert!(h.server.state.agent.held.is_empty());
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    h.server.state.surfaces.get_mut(&alpha).unwrap().mapped = false;
    h.server.state.recompute_effective_visibility();
    assert!(h.server.state.agent.keyboard.current_focus().is_none());
    assert!(h.server.state.agent.held.is_empty());
}

#[test]
fn agent_live_surface_unmap_clears_click_grab_without_disturbing_keyboard_or_sequence() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    bind_agent_devices(&mut h);
    let first = agent_target(&h, &beta, agent_key(PressAction::Press, KEY_LEFTSHIFT));
    let next = agent_target(&h, &beta, agent_key(PressAction::Both, KEY_B));
    let (sender, _receiver) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(crate::port::LongOp::Sequence(vec![
        step("comp.input.key", first, 0),
        step("comp.input.key", next, 1_000),
    ]), sender, Instant::now());
    h.server.state.service_ready_agent_sequence();
    let keyboard_focus = h.server.state.agent.keyboard.current_focus();
    let keys = h.server.state.agent.keyboard.pressed_keys();
    let key_hold = input_injection::Hold::Key(KEY_LEFTSHIFT + 8);
    assert_eq!(h.server.state.agent.held.owners_of(key_hold), 1);
    assert!(h.server.state.agent.keyboard.modifier_state().shift);

    let (id, generation) = window_id_and_generation(&h, &alpha);
    let motion = on_agent(move_op(PointerMoveTarget::Window {
        id, generation, x: 20.0, y: 20.0, require_hit: true,
    }));
    assert_eq!(inject(&mut h, &ingress, &runtime, motion).0, 0);
    assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(InputOp::PointerButton {
        button: BTN_LEFT, action: PressAction::Press,
    })).0, 0);
    assert!(h.server.state.agent.pointer.with_grab(|_, grab|
        grab.is::<smithay::input::pointer::ClickGrab<WaylandState>>()
    ).unwrap_or(false));
    let button_hold = input_injection::Hold::Button(BTN_LEFT);
    assert_eq!(h.server.state.agent.held.owners_of(button_hold), 1);
    let surface = h.server.state.surfaces[&alpha].role.wl_surface().clone();

    // Keep the resource alive and run exactly one unmap visibility pass: a
    // second reconciliation must not be needed to clear the stale focus.
    h.server.state.surfaces.get_mut(&alpha).unwrap().mapped = false;
    h.server.state.recompute_effective_visibility();
    assert!(surface.is_alive());
    assert!(h.server.state.agent.pointer.current_focus().is_none());
    assert!(!h.server.state.agent.pointer.is_grabbed());
    assert!(h.server.state.agent.pointer.current_pressed().is_empty());
    assert_eq!(h.server.state.agent.held.owners_of(button_hold), 0);
    assert_eq!(h.server.state.agent.keyboard.current_focus(), keyboard_focus);
    assert_eq!(h.server.state.agent.keyboard.pressed_keys(), keys);
    assert!(h.server.state.agent.keyboard.modifier_state().shift);
    assert_eq!(h.server.state.agent.held.owners_of(key_hold), 1);
    assert_eq!(h.server.state.injection.sequences.len(), 1);
}

#[test]
fn full_agent_click_switches_popup_roots_without_losing_new_press() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (seat_a, _, pointer_a) = bind_agent_devices(&mut h);
    let click = agent_target(&h, &alpha, InputOp::PointerButton { button: BTN_LEFT, action: PressAction::Both });
    assert_eq!(inject(&mut h, &ingress, &runtime, click).0, 0);
    let serial = pointer_bodies(&h.sync(), pointer_a, 3).into_iter()
        .find(|body| word(body, 3) == 1).map(|body| word(&body, 0)).unwrap();
    map_test_popup_on_seat(&mut h, Some((seat_a, serial)));

    let mut other = connect_other_layer_client(&mut h);
    swap_test_client(&mut h, &mut other);
    let (seat_b, _, pointer_b) = bind_agent_devices(&mut h);
    let (_, xdg_b, _, beta) = map_named_test_toplevel(&mut h, "menu-b", "menu-b");
    let (id, generation) = window_id_and_generation(&h, &beta);
    assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(move_op(PointerMoveTarget::Window {
        id, generation, x: 10.0, y: 10.0, require_hit: true,
    }))).0, 0);
    assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(InputOp::PointerButton {
        button: BTN_LEFT, action: PressAction::Both,
    })).0, 0);
    let serial = pointer_bodies(&h.sync(), pointer_b, 3).into_iter()
        .find(|body| word(body, 3) == 1).map(|body| word(&body, 0)).unwrap();
    assert_eq!(h.server.state.agent.last_pointer_action.as_ref().map(|(serial, _)| u32::from(*serial)), Some(serial));
    let (_, popup) = map_test_popup_with_parent_on_seat(&mut h, xdg_b, Some((seat_b, serial)));
    assert!(h.server.state.agent.pointer.with_grab(|_, grab| grab.is::<PopupPointerGrab<WaylandState>>()).unwrap_or(false));
    assert!(!h.sync().iter().any(|(object, opcode, _)| *object == popup && *opcode == 1));
}

#[test]
fn agent_menu_root_unmap_retires_grabs_and_allows_other_window_keys() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    let (seat, _, _) = bind_agent_devices(&mut h);
    let open = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    assert_eq!(inject(&mut h, &ingress, &runtime, open).0, 0);
    let serial = h.server.state.agent.last_keyboard_action.as_ref().unwrap().0;
    map_test_popup_on_seat(&mut h, Some((seat, serial.into())));
    assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(agent_key(PressAction::Press, KEY_LEFTSHIFT))).0, 0);
    let root = h.server.state.surfaces[&alpha].role.wl_surface().clone();
    h.server.state.agent.last_pointer_action = Some((serial, root));
    send_request(&mut h.client, alpha.protocol_id(), 1, &words(&[0, 0, 0]));
    send_request(&mut h.client, alpha.protocol_id(), 6, &[]);
    let _ = h.sync();
    assert!(!h.server.state.surfaces[&alpha].mapped);
    assert!(h.server.state.surfaces[&alpha].role.wl_surface().is_alive());
    assert!(h.server.state.agent.popup_grab.is_none());
    assert!(!h.server.state.agent.keyboard.is_grabbed());
    assert!(!h.server.state.agent.pointer.is_grabbed());
    assert!(h.server.state.agent.keyboard.pressed_keys().is_empty());
    assert!(h.server.state.agent.last_pointer_action.is_none());
    assert!(h.server.state.agent.last_keyboard_action.is_none());
    let key = agent_target(&h, &beta, agent_key(PressAction::Both, KEY_B));
    let (rc, body) = inject(&mut h, &ingress, &runtime, key);
    assert_eq!(rc, 0, "{body}");
}

#[test]
fn agent_key_before_popup_first_buffer_refuses_the_actual_unmapped_destination() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (seat, keyboard, _) = bind_agent_devices(&mut h);
    let open = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    assert_eq!(inject(&mut h, &ingress, &runtime, open).0, 0);
    let serial = h.server.state.agent.last_keyboard_action.as_ref().unwrap().0;
    let (surface, _, _, _) = configure_test_popup_with_parent_on_seat(
        &mut h, TEST_XDG_SURFACE_ID, Some((seat, serial.into())),
    );
    let record = h.server.state.surfaces.values().find(|record| record.role.wl_surface().id().protocol_id() == surface).unwrap();
    assert!(!record.mapped);
    assert_eq!(record.content_seq, 0);
    assert!(h.server.state.agent.keyboard.is_grabbed());
    let _ = h.sync();
    let (rc, reply) = inject(&mut h, &ingress, &runtime, on_agent(agent_key(PressAction::Both, KEY_B)));
    assert_eq!(rc, 10, "{reply}");
    assert_eq!(reply["error"], "unmapped");
    assert!(device_key_events(&h.sync(), keyboard).is_empty());
    assert!(h.server.state.agent.held.is_empty());
}

#[test]
fn popup_handle_retirement_drops_dead_root_actions_but_preserves_fresh_live_root_actions() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    let (seat, _, _) = bind_agent_devices(&mut h);
    let open = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    assert_eq!(inject(&mut h, &ingress, &runtime, open).0, 0);
    let serial = h.server.state.agent.last_keyboard_action.as_ref().unwrap().0;
    map_test_popup_on_seat(&mut h, Some((seat, serial.into())));
    let old_root = h.server.state.surfaces[&alpha].role.wl_surface().clone();
    let next_root = h.server.state.surfaces[&beta].role.wl_surface().clone();
    h.server.state.agent.last_keyboard_action = Some((serial, old_root));
    h.server.state.agent.last_pointer_action = Some((serial, next_root.clone()));
    h.server.state.surfaces.get_mut(&alpha).unwrap().mapped = false;
    // Isolate handle retirement from subsequent focus reconciliation, which
    // might otherwise hide a stale action restored by this helper itself.
    h.server.state.retire_agent_popup_handles();
    assert!(h.server.state.agent.last_keyboard_action.is_none());
    assert_eq!(h.server.state.agent.last_pointer_action, Some((serial, next_root)));
    assert!(h.server.state.agent.popup_grab.is_none());
}

#[test]
fn agent_submenu_destruction_keeps_keys_on_the_live_parent_menu() {
    for keep_keyboard_grab in [true, false] {
        let (mut h, ingress, runtime, _, alpha, _) = two_windows();
        let (seat, keyboard, _) = bind_agent_devices(&mut h);
        let open = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
        assert_eq!(inject(&mut h, &ingress, &runtime, open).0, 0);
        let serial = h.server.state.agent.last_keyboard_action.as_ref().unwrap().0;
        let (parent_surface, parent_popup) = map_test_popup_on_seat(&mut h, Some((seat, serial.into())));
        assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(agent_key(PressAction::Both, KEY_A))).0, 0);
        let serial = h.server.state.agent.last_keyboard_action.as_ref().unwrap().0;
        let (child_surface, child_popup) = map_test_popup_with_parent_on_seat(
            &mut h, parent_popup - 1, Some((seat, serial.into())),
        );
        assert_eq!(inject(&mut h, &ingress, &runtime, on_agent(agent_key(PressAction::Both, KEY_B))).0, 0);
        assert_eq!(h.server.state.agent.keyboard.current_focus().and_then(|focus| focus.owned_surface()).map(|surface| surface.id()), Some(child_surface.clone()));
        if !keep_keyboard_grab {
            h.server.state.agent.keyboard.clone().unset_grab(&mut h.server.state);
        }
        let _ = h.sync();
        send_request(&mut h.client, child_popup, 0, &[]);
        send_request(&mut h.client, child_popup - 1, 0, &[]);
        send_request(&mut h.client, child_surface.protocol_id(), 0, &[]);
        let mut traffic = h.sync();
        assert!(h.server.state.agent.popup_grab.is_some(), "parent chain remains open");
        assert_eq!(h.server.state.agent.keyboard.with_grab(|_, grab|
            grab.is::<PopupKeyboardGrab<WaylandState>>()
        ).unwrap_or(false), keep_keyboard_grab);
        let (rc, body) = inject(&mut h, &ingress, &runtime, on_agent(agent_key(PressAction::Both, KEY_A)));
        assert_eq!(rc, 0, "{body}");
        traffic.extend(h.sync());
        assert!(traffic.iter().any(|(object, opcode, body)|
            *object == keyboard && *opcode == 1 && word(body, 1) == parent_surface.protocol_id()
        ), "keyboard enters the surviving parent popup");
        assert_eq!(device_key_events(&traffic, keyboard), [(KEY_A, 1), (KEY_A, 0)]);
        assert_eq!(h.server.state.agent.keyboard.current_focus().and_then(|focus| focus.owned_surface()).map(|surface| surface.id()), Some(parent_surface.clone()));

        send_request(&mut h.client, parent_popup, 0, &[]);
        send_request(&mut h.client, parent_popup - 1, 0, &[]);
        send_request(&mut h.client, parent_surface.protocol_id(), 0, &[]);
        let _ = h.sync();
        assert!(h.server.state.agent.popup_grab.is_none(), "destroying the last popup retires stored chain");
    }
}

#[test]
fn destroying_agent_popup_preserves_queued_parent_input() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (seat, keyboard, _) = bind_agent_devices(&mut h);
    let press = agent_target(&h, &alpha, InputOp::PointerButton {
        button: BTN_LEFT, action: PressAction::Press,
    });
    assert_eq!(inject(&mut h, &ingress, &runtime, press).0, 0);
    let serial = h.server.state.agent.pointer.with_grab(|serial, _| serial).unwrap();
    let (popup_surface, popup) = map_test_popup_on_seat(&mut h, Some((seat, serial.into())));
    let layout = h.server.state.surfaces[&popup_surface].layout;
    let pointer = h.server.state.agent.pointer.clone();
    let surface = h.server.state.surfaces[&popup_surface].role.wl_surface().clone();
    pointer.motion(&mut h.server.state, Some((surface.into(), (f64::from(layout.x), f64::from(layout.y)).into())), &MotionEvent {
        location: (f64::from(layout.x) + 1.0, f64::from(layout.y) + 1.0).into(),
        serial: SERIAL_COUNTER.next_serial(), time: monotonic_millis(),
    });
    let next = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_B));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(crate::port::LongOp::Sequence(vec![
        step("comp.input.pointer.button", on_agent(InputOp::PointerButton {
            button: BTN_LEFT, action: PressAction::Both,
        }), 0),
        step("comp.input.key", next, 20),
    ]), sender, Instant::now());
    h.server.state.service_ready_agent_sequence();
    let _ = h.sync();
    send_request(&mut h.client, popup, 0, &[]);
    send_request(&mut h.client, popup - 1, 0, &[]); // xdg_surface
    send_request(&mut h.client, popup_surface.protocol_id(), 0, &[]);
    h.dispatch_client();
    assert_eq!(h.server.state.injection.sequences.len(), 1);
    assert!(h.server.state.agent.pointer.current_focus().is_none());
    assert!(h.server.state.agent.popup_grab.is_none());
    assert!(!h.server.state.agent.pointer.is_grabbed(), "ended popup must not block the queued parent key");
    assert!(!h.server.state.agent.keyboard.is_grabbed());
    assert!(h.server.state.agent.pointer.current_pressed().is_empty());
    assert_eq!(h.server.state.agent.keyboard.current_focus().and_then(|target| target.owned_surface()).map(|s| s.id()), Some(alpha));
    finish_test_sequences(&mut h);
    let reply = runtime.block_on(receiver).unwrap().wire_json();
    assert!(reply.get("error").is_none(), "{reply}");
    assert!(device_key_events(&h.sync(), keyboard).contains(&(KEY_B, 1)));
}

#[test]
fn mixed_seat_sequence_failure_releases_both_seats() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let agent = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_LEFTSHIFT));
    let (rc, body) = run_sequence(
        &mut h,
        &ingress,
        &runtime,
        vec![
            step("comp.input.key", agent_key(PressAction::Press, KEY_B), 0),
            step("comp.input.key", agent, 0),
            step(
                "comp.input.key",
                on_agent(InputOp::Key {
                    key: KeySpec::Name("NoSuchKeyName".into()),
                    action: PressAction::Both,
                    modifiers: vec![],
                }),
                0,
            ),
        ],
    );
    assert_eq!(rc, 10, "{body}");
    assert_eq!(body["error"], "step_failed");
    assert!(h.server.state.human.held.is_empty());
    assert!(h.server.state.agent.held.is_empty());
    assert!(h.server.state.human.keyboard.pressed_keys().is_empty());
    assert!(h.server.state.agent.keyboard.pressed_keys().is_empty());
}

#[test]
fn agent_drag_start_never_installs_a_dnd_grab_or_icon_role() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (seat, _, _) = bind_agent_devices(&mut h);
    let op = agent_target(
        &h,
        &alpha,
        InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Press,
        },
    );
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    let serial = h
        .server
        .state
        .agent
        .pointer
        .with_grab(|serial, _| serial)
        .unwrap();
    let manager = h.bind_test_global("wl_data_device_manager", 3);
    let source = h.allocate_object_id();
    let device = h.allocate_object_id();
    let icon = h.allocate_object_id();
    send_request(&mut h.client, manager, 0, &words(&[source]));
    send_request(&mut h.client, manager, 1, &words(&[device, seat]));
    send_request(&mut h.client, TEST_COMPOSITOR_ID, 0, &words(&[icon]));
    let _ = h.sync();
    send_request(
        &mut h.client,
        device,
        0,
        &words(&[source, TEST_TOPLEVEL_SURFACE_ID, icon, serial.into()]),
    );
    let traffic = h.sync();
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == source && *opcode == 2),
        "source cancelled"
    );
    assert!(
        h.server
            .state
            .agent
            .pointer
            .with_grab(|_, grab| grab.is::<smithay::input::pointer::ClickGrab<WaylandState>>())
            .unwrap_or(false)
    );
    let client = h.subsurface().client().unwrap();
    let icon = client
        .object_from_protocol_id::<WlSurface>(&h.server.state.display_handle, icon)
        .unwrap();
    assert!(compositor::get_role(&icon).is_none());
    assert!(!h.server.state.human.pointer.is_grabbed());
}

#[test]
fn agent_window_geometry_coordinates_match_human_coordinates_with_csd_offset() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (_, _, pointer) = bind_agent_devices(&mut h);
    place_record(&mut h, &alpha, (80.0, 90.0, 200.0, 150.0));
    h.server.state.surfaces.get_mut(&alpha).unwrap().window_origin = (90.0, 105.0);
    let (id, generation) = window_id_and_generation(&h, &alpha);
    let moved = |x, y| {
        on_agent(move_op(PointerMoveTarget::Window {
            id,
            generation,
            x,
            y,
            require_hit: false,
        }))
    };
    let (rc, body) = inject(&mut h, &ingress, &runtime, moved(-30.0, -30.0));
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "chrome_target");
    assert!(h.server.state.agent.pointer.current_focus().is_none());
    assert_eq!(inject(&mut h, &ingress, &runtime, moved(20.0, 30.0)).0, 0);
    assert_eq!(h.server.state.agent.pointer_position, Some((110.0, 135.0)));
    let human = h.server.state.pointer_move_input(&PointerMoveTarget::Window {
        id, generation, x: 20.0, y: 30.0, require_hit: false,
    }, 0).unwrap();
    assert!(matches!(human, HostInput::PointerMotionAbsolute { x: 110.0, y: 135.0, .. }));
    let traffic = h.sync();
    let enter = pointer_bodies(&traffic, pointer, 0);
    assert_eq!(enter.len(), 1);
    assert_eq!((fixed(&enter[0], 2), fixed(&enter[0], 3)), (30.0, 45.0));
}

#[test]
fn agent_targeted_releases_retire_holds_after_target_unmaps() {
    for keyboard in [true, false] {
        let (mut h, ingress, runtime, _, alpha, _) = two_windows();
        bind_agent_devices(&mut h);
        let payload = |action| if keyboard { agent_key(action, KEY_LEFTSHIFT) } else {
            InputOp::PointerButton { button: BTN_LEFT, action }
        };
        let press = agent_target(&h, &alpha, payload(PressAction::Press));
        let release = agent_target(&h, &alpha, payload(PressAction::Release));
        assert_eq!(inject(&mut h, &ingress, &runtime, press).0, 0);
        h.server.state.surfaces.get_mut(&alpha).unwrap().mapped = false;
        let (rc, body) = inject(&mut h, &ingress, &runtime, release);
        assert_eq!(rc, 0, "{body}");
        assert!(h.server.state.agent.held.is_empty());
        assert!(h.server.state.agent.keyboard.pressed_keys().is_empty());
        assert!(h.server.state.agent.pointer.current_pressed().is_empty());
    }
}

#[test]
fn human_targeted_button_keeps_the_existing_implicit_grab_delivery_path() {
    let (mut h, _, _, pointer, alpha, beta) = two_windows();
    let (id, generation) = window_id_and_generation(&h, &alpha);
    h.server.state.targeted_pointer_button(id, generation, BTN_LEFT, HostButtonState::Pressed, monotonic_millis());
    let _ = h.sync();
    let (id, generation) = window_id_and_generation(&h, &beta);
    h.server.state.targeted_pointer_button(id, generation, 0x111, HostButtonState::Pressed, monotonic_millis());
    assert!(h.server.state.human.pointer.current_pressed().contains(&0x111));
    assert!(pointer_bodies(&h.sync(), pointer, 3).iter().any(|body| word(body, 2) == 0x111 && word(body, 3) == 1));
}

#[test]
fn nested_human_focus_loss_preserves_agent_holds_and_queued_work() {
    for event in [HostInput::KeyboardFocusLost, HostInput::KeyboardFocusLostKeepingKeys] {
        let (mut h, _, _, _, alpha, _) = two_windows();
        bind_agent_devices(&mut h);
        let first = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_A));
        let (sender, _receiver) = tokio::sync::oneshot::channel();
        h.server.state.start_long_op(crate::port::LongOp::Sequence(vec![
            step("comp.input.key", first, 0),
            step("comp.input.key", on_agent(agent_key(PressAction::Both, KEY_B)), 1_000),
        ]), sender, Instant::now());
        h.server.state.service_ready_agent_sequence();
        let before = h.server.state.agent.keyboard.current_focus();
        h.server.state.handle_host_input(event);
        assert_eq!(h.server.state.agent.keyboard.current_focus(), before);
        assert!(!h.server.state.agent.held.is_empty());
        assert_eq!(h.server.state.injection.sequences.len(), 1);
    }
}

#[test]
fn human_region_selection_preserves_agent_keyboard_hold() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let op = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_A));
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    h.server.state.region.bridge = Some(crate::region_scene::RegionBridge::default());
    let (sender, _receiver) = tokio::sync::oneshot::channel();
    h.server.state.start_region_selection(None, Duration::from_secs(5), sender, Instant::now());
    assert!(h.server.state.region_selection_active());
    assert!(h.server.state.agent.keyboard.pressed_keys().contains(&Keycode::new(KEY_A + 8)));
    assert!(!h.server.state.agent.held.is_empty());
}

#[test]
fn agent_keyboard_popup_uses_the_delivered_press_serial() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (seat, keyboard, _) = bind_agent_devices(&mut h);
    let op = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
    let traffic = h.sync();
    let serial = traffic
        .iter()
        .find_map(|(object, opcode, body)| {
            (*object == keyboard && *opcode == 3).then(|| word(body, 0))
        })
        .unwrap();
    assert_eq!(
        h.server
            .state
            .agent
            .last_keyboard_action
            .as_ref()
            .map(|(serial, _)| u32::from(*serial)),
        Some(serial)
    );
    let (_, popup) = map_test_popup_on_seat(&mut h, Some((seat, serial)));
    assert!(h.server.state.agent.keyboard.has_grab(serial.into()));
    assert!(
        !h.sync()
            .iter()
            .any(|(object, opcode, _)| *object == popup && *opcode == 1)
    );
}

#[test]
fn release_all_dismisses_agent_popup_but_human_scoped_cleanup_does_not() {
    for args in [json!({}), json!({"seat":"agent"})] {
        let (mut h, ingress, runtime, _, alpha, _) = two_windows();
        let (seat, keyboard, _) = bind_agent_devices(&mut h);
        let op = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
        assert_eq!(inject(&mut h, &ingress, &runtime, op).0, 0);
        let traffic = h.sync();
        let serial = traffic.iter().find_map(|(object, opcode, body)| {
            (*object == keyboard && *opcode == 3).then(|| word(body, 0))
        }).unwrap();
        let (_, popup) = map_test_popup_on_seat(&mut h, Some((seat, serial)));
        let human = crate::port::parse_input_op("comp.input.release_all", &json!({"seat":"human"})).unwrap();
        assert_eq!(inject(&mut h, &ingress, &runtime, human).0, 0);
        assert!(h.server.state.agent.pointer.is_grabbed());
        let _ = h.sync();
        let release = crate::port::parse_input_op("comp.input.release_all", &args).unwrap();
        assert_eq!(inject(&mut h, &ingress, &runtime, release).0, 0);
        assert!(!h.server.state.agent.pointer.is_grabbed());
        assert!(!h.server.state.agent.keyboard.is_grabbed());
        assert!(h.sync().iter().any(|(object, opcode, _)| *object == popup && *opcode == 1));
        assert!(h.server.state.agent.last_keyboard_action.is_none());
        assert!(h.server.state.agent.last_pointer_action.is_none());
    }
}

#[test]
fn agent_sequence_is_cancelled_at_authority_loss_without_resuming_a_queued_step() {
    let (mut h, _, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let first = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_A));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(
        crate::port::LongOp::Sequence(vec![
            step("comp.input.key", first, 0),
            step(
                "comp.input.key",
                on_agent(agent_key(PressAction::Press, KEY_B)),
                1_000,
            ),
        ]),
        sender,
        Instant::now(),
    );
    h.server.state.service_ready_agent_sequence();
    assert_eq!(h.server.state.injection.sequences.len(), 1);
    h.server.state.reconcile_all_input_authority_loss();
    assert!(h.server.state.injection.sequences.is_empty());
    assert!(h.server.state.agent.held.is_empty());
    assert!(h.server.state.agent.keyboard.current_focus().is_none());
    let reply = runtime.block_on(receiver).unwrap().wire_json();
    assert_eq!(reply["error"], "input_cleared");
    assert_eq!(reply["completed"].as_array().unwrap().len(), 1);
}

#[test]
fn explicit_human_input_keeps_bindings_and_cannot_release_a_physical_hold() {
    let (mut h, ingress, runtime, _, _, _) = two_windows();
    let op = crate::port::parse_input_op(
        "comp.input.key",
        &json!({
            "seat":"human", "key":"2", "modifiers":["super"]
        }),
    )
    .unwrap();
    let (rc, body) = inject(&mut h, &ingress, &runtime, op);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["seat"], "human");
    assert_eq!(h.server.state.workspace_current(), 2);
    h.frame(vec![HostInput::key_from_evdev(
        KEY_LEFTSHIFT,
        HostButtonState::Pressed,
        monotonic_millis(),
    )]);
    assert_eq!(
        inject(
            &mut h,
            &ingress,
            &runtime,
            agent_key(PressAction::Press, KEY_LEFTSHIFT)
        )
        .0,
        0
    );
    assert_eq!(inject(&mut h, &ingress, &runtime, InputOp::ReleaseAll).0, 0);
    assert!(
        h.server
            .state
            .human
            .keyboard
            .pressed_keys()
            .contains(&Keycode::new(KEY_LEFTSHIFT + 8))
    );
}

#[test]
fn unseated_sequence_reports_human_or_mixed_on_success_and_failure() {
    for mixed in [false, true] {
        for fail in [false, true] {
            let (mut h, ingress, runtime, _, alpha, _) = two_windows();
            bind_agent_devices(&mut h);
            let mut steps = vec![step("comp.input.key", agent_key(PressAction::Both, KEY_B), 0)];
            if mixed {
                steps.push(step("comp.input.key", agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A)), 0));
            }
            if fail {
                steps.push(step("comp.input.key", InputOp::Key {
                    key: crate::port::KeySpec::Name("not_a_real_keysym".into()),
                    action: PressAction::Both, modifiers: Vec::new(),
                }, 0));
            }
            let admission = ingress.request_long(crate::port::LongOp::Sequence(steps)).unwrap();
            let (rc, body) = long_reply(&mut h, &runtime, admission, |state| state.injection.sequences.is_empty());
            assert_eq!(rc == 0, !fail, "{body}");
            assert_eq!(body["seat"], if mixed { "mixed" } else { "human" });
            if fail { assert_eq!(body["error"], "step_failed"); }
        }
    }
}

#[test]
fn unseated_agent_sequence_counts_a_refusing_human_step() {
    for fail in [false, true] {
        let (mut h, ingress, runtime, _, alpha, _) = two_windows();
        bind_agent_devices(&mut h);
        let mut steps = vec![step("comp.input.key", agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A)), 0)];
        if fail {
            steps.push(step("comp.input.key", InputOp::Key {
                key: crate::port::KeySpec::Name("not_a_real_keysym".into()),
                action: PressAction::Both, modifiers: Vec::new(),
            }, 0));
        }
        let admission = ingress.request_long(crate::port::LongOp::Sequence(steps)).unwrap();
        let (rc, body) = long_reply(&mut h, &runtime, admission, |state| state.injection.sequences.is_empty());
        assert_eq!(rc == 0, !fail, "{body}");
        assert_eq!(body["seat"], if fail { "mixed" } else { "agent" });
        if fail {
            assert_eq!(body["error"], "step_failed");
            assert_eq!(body["completed"][0]["seat"], "agent");
            assert_eq!(body["step"]["seat"], "human");
        } else {
            assert_eq!(body["steps"][0]["seat"], "agent");
        }
    }
}

#[test]
fn mixed_seat_sequence_replies_name_the_default_and_each_driven_seat() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let agent = agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A));
    let admission = ingress
        .request_long(crate::port::LongOp::SeatedSequence {
            seat: SeatKind::Agent,
            steps: vec![
                step("comp.input.key", agent_key(PressAction::Both, KEY_B), 0),
                step("comp.input.key", agent, 0),
            ],
        })
        .unwrap();
    let (rc, body) = long_reply(&mut h, &runtime, admission, |state| {
        state.injection.sequences.is_empty()
    });
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["seat"], "agent");
    assert_eq!(body["steps"][0]["seat"], "human");
    assert_eq!(body["steps"][1]["seat"], "agent");
    assert!(h.server.state.human.held.is_empty());
    assert!(h.server.state.agent.held.is_empty());
}
