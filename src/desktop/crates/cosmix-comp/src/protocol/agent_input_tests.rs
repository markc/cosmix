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
    let (_, popup) = map_test_popup_on_seat(&mut h, Some((seat, serial.into())));
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
    let _ = h.sync();
    send_request(&mut h.client, popup, 0, &[]);
    send_request(&mut h.client, popup - 1, 0, &[]); // xdg_surface
    send_request(&mut h.client, popup_surface.protocol_id(), 0, &[]);
    h.dispatch_client();
    assert_eq!(h.server.state.injection.sequences.len(), 1);
    assert!(h.server.state.agent.pointer.current_focus().is_none());
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
fn agent_root_local_motion_rejects_chrome_before_focus_and_uses_local_coordinates() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (_, _, pointer) = bind_agent_devices(&mut h);
    place_record(&mut h, &alpha, (80.0, 90.0, 200.0, 150.0));
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
    let (rc, body) = inject(&mut h, &ingress, &runtime, moved(-10.0, -10.0));
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "chrome_target");
    assert!(h.server.state.agent.pointer.current_focus().is_none());
    assert_eq!(inject(&mut h, &ingress, &runtime, moved(20.0, 30.0)).0, 0);
    assert_eq!(h.server.state.agent.pointer_position, Some((100.0, 120.0)));
    let traffic = h.sync();
    let enter = pointer_bodies(&traffic, pointer, 0);
    assert_eq!(enter.len(), 1);
    assert_eq!((fixed(&enter[0], 2), fixed(&enter[0], 3)), (20.0, 30.0));
}

#[test]
fn agent_keyboard_popup_uses_the_delivered_press_serial() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    let (seat, keyboard, _) = bind_agent_devices(&mut h);
    let op = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_A));
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
