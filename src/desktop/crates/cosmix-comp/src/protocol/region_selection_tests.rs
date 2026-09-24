use super::*;
use crate::{
    port::{ControlReply, LongOp},
    region_scene::{RegionBridge, test_remove_frame},
};

fn begin(h: &mut KeybindingHarness) -> tokio::sync::oneshot::Receiver<ControlReply> {
    h.server
        .state
        .region
        .bridge
        .get_or_insert_with(RegionBridge::default);
    let (tx, rx) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(
        LongOp::RegionSelect {
            output: None,
            timeout: Duration::from_secs(30),
        },
        tx,
        Instant::now(),
    );
    rx
}
fn motion(h: &mut KeybindingHarness, x: f64, y: f64) {
    h.server
        .state
        .handle_host_input(HostInput::PointerMotionAbsolute { x, y, time: 1 });
}
fn button(h: &mut KeybindingHarness, button: u32, state: HostButtonState) {
    h.server.state.handle_host_input(HostInput::PointerButton {
        button,
        state,
        time: 2,
    });
}
fn clean_frame(h: &mut KeybindingHarness) {
    let bridge = h.server.state.region.bridge.as_ref().unwrap().clone();
    let revision = test_remove_frame(&bridge);
    for output in h.server.state.backend.occlusion_outputs() {
        if output.generation != 0 {
            bridge.presented(&output.name, output.generation, Some(revision));
        }
    }
    h.server.state.poll_region_selection(Instant::now());
}
fn body(rx: &mut tokio::sync::oneshot::Receiver<ControlReply>) -> serde_json::Value {
    match rx.try_recv().expect("terminal reply") {
        ControlReply::Body(value) => value,
        other => panic!("{other:?}"),
    }
}

#[test]
fn injected_escape_without_client_focus_cancels_region_and_releases() {
    use crate::port::{InputOp, KeySpec, PressAction};
    let mut h = KeybindingHarness::new(true);
    let mut rx = begin(&mut h);
    assert!(h.server.state.keyboard.current_focus().is_none());
    let reply = h.server.state.service_input_op(&InputOp::Key {
        key: KeySpec::Name("Escape".into()),
        action: PressAction::Both,
        modifiers: vec![],
    });
    assert!(matches!(reply, ControlReply::Body(_)), "{reply:?}");
    clean_frame(&mut h);
    assert_eq!(body(&mut rx)["status"], "cancelled");
    assert!(h.server.state.injection.held.is_empty());
    assert!(h.server.state.keyboard.pressed_keys().is_empty());
}

#[test]
fn targeted_button_during_region_selection_refuses_before_focus_or_input() {
    use crate::port::{BTN_LEFT, InputOp, PressAction};
    let mut h = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut h);
    let record = test_toplevel_record(&h);
    let (id, generation, z) = (record.id.0, record.generation, record.layout.z);
    let _rx = begin(&mut h);
    let before = h.server.state.injection.events;
    let reply = h.server.state.service_input_op(&InputOp::Targeted {
        id,
        generation,
        raise: true,
        op: Box::new(InputOp::PointerButton {
            button: BTN_LEFT,
            action: PressAction::Both,
        }),
    });
    assert_eq!(reply.wire_json()["reason"], "region_select");
    assert_eq!(h.server.state.injection.events, before);
    assert!(h.server.state.keyboard.current_focus().is_none());
    assert_eq!(test_toplevel_record(&h).layout.z, z);
    assert!(h.server.state.pointer.current_pressed().is_empty());
    assert!(h.server.state.region.suspended);
}

#[test]
fn region_touch_only_device_removal_admits_next_selection() {
    for direct_recovery in [true, false] {
        let mut h = KeybindingHarness::new(true);
        h.server
            .state
            .handle_host_input(HostInput::TouchDeviceAdded);
        h.server.state.handle_host_input(HostInput::TouchDown {
            slot: Some(0).into(),
            x: 20.,
            y: 20.,
            time: 1,
        });
        assert!(!h.server.state.region.touches.is_empty());
        let mut busy = begin(&mut h);
        assert_eq!(busy.try_recv().unwrap(), ControlReply::Busy);
        // Same recovery hook used by device removal, VT loss and session lock;
        // no run, keys, buttons or suppressed touches exist in this state.
        if direct_recovery {
            h.server.state.abandon_region_input();
            assert!(h.server.state.region.touches.is_empty());
        }
        h.server
            .state
            .handle_host_input(HostInput::TouchDeviceRemoved);
        assert!(h.server.state.region.touches.is_empty());
        let mut next = begin(&mut h);
        assert!(next.try_recv().is_err());
        assert!(h.server.state.region.suspended);
    }
}

#[test]
fn region_normalise_rejects_coordinates_beyond_i32() {
    let h = KeybindingHarness::new(true);
    let mut output = h.server.state.backend.occlusion_outputs().remove(0);
    output.bounds.w = f64::from(u32::MAX);
    assert_eq!(
        super::super::region_selection::normalise(
            &output,
            (0., 0.),
            (f64::from(i32::MAX) + 1., 10.),
        ),
        None
    );
}

#[test]
fn region_touch_device_removal_aborts_pointer_drag() {
    let mut h = KeybindingHarness::new(true);
    h.server
        .state
        .handle_host_input(HostInput::TouchDeviceAdded);
    let mut rx = begin(&mut h);
    motion(&mut h, 20., 20.);
    button(&mut h, 0x110, HostButtonState::Pressed);
    motion(&mut h, 80., 80.);
    assert!(h.server.state.region.suspended);
    h.server
        .state
        .handle_host_input(HostInput::TouchDeviceRemoved);
    assert_eq!(rx.try_recv().unwrap(), ControlReply::Busy);
    assert!(!h.server.state.region.suspended);
    let mut next = begin(&mut h);
    assert!(
        next.try_recv().is_err(),
        "device loss must clear held input"
    );
    assert!(h.server.state.region.suspended);
}

#[test]
fn region_cancel_and_timeout_need_no_presentation() {
    for started_drag in [false, true] {
        for timeout in [false, true] {
            let mut h = KeybindingHarness::new(true);
            let mut rx = begin(&mut h);
            if started_drag {
                motion(&mut h, 20., 20.);
                button(&mut h, 0x110, HostButtonState::Pressed);
                motion(&mut h, 80., 80.);
            }
            if timeout {
                h.server
                    .state
                    .poll_region_selection(Instant::now() + Duration::from_secs(31));
            } else {
                h.server.state.handle_host_input(HostInput::Key {
                    keycode: Keycode::new(9),
                    state: HostButtonState::Pressed,
                    time: 1,
                });
            }
            // No output has submitted anything, including the selected output
            // in the started-drag case. This is stronger than a sleeping peer.
            assert_eq!(
                body(&mut rx)["status"],
                if timeout { "timeout" } else { "cancelled" }
            );
            assert!(!h.server.state.region.suspended);
            let bridge = h.server.state.region.bridge.as_ref().unwrap();
            assert!(!bridge.clean(1), "reply must not depend on removal proof");
            // Removal is still published, and later cleanup cannot reply twice.
            clean_frame(&mut h);
            assert!(matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ));
        }
    }
}

#[test]
fn region_focus_loss_after_cancel_drains_held_keys_and_buttons() {
    for event in [
        HostInput::KeyboardFocusLost,
        HostInput::KeyboardFocusLostKeepingKeys,
        HostInput::PointerLeave,
    ] {
        let mut h = KeybindingHarness::new(true);
        let mut rx = begin(&mut h);
        h.key(42, HostButtonState::Pressed);
        button(&mut h, 0x111, HostButtonState::Pressed);
        clean_frame(&mut h);
        assert_eq!(body(&mut rx)["status"], "cancelled");
        assert!(!h.server.state.region.suspended);
        let mut busy = begin(&mut h);
        assert_eq!(busy.try_recv().unwrap(), ControlReply::Busy);
        h.server.state.handle_host_input(event);
        assert!(h.server.state.keyboard.pressed_keys().is_empty());
        let mut next = begin(&mut h);
        assert!(next.try_recv().is_err());
        assert!(h.server.state.region.suspended);
    }
}

#[test]
fn region_nested_production_presentation_completes_selection() {
    let mut h = KeybindingHarness::new(true);
    let mut rx = begin(&mut h);
    motion(&mut h, 20., 20.);
    button(&mut h, 0x110, HostButtonState::Pressed);
    motion(&mut h, 80., 80.);
    button(&mut h, 0x110, HostButtonState::Released);
    let bridge = h.server.state.region.bridge.as_ref().unwrap().clone();
    let revision = test_remove_frame(&bridge);
    // Exercise the production handoff, without deriving identity from the view.
    crate::complete_nested_region_presentation(&bridge, false, Some(revision));
    h.server.state.poll_region_selection(Instant::now());
    assert!(rx.try_recv().is_err());
    crate::complete_nested_region_presentation(&bridge, true, Some(revision - 1));
    h.server.state.poll_region_selection(Instant::now());
    assert!(rx.try_recv().is_err());
    crate::complete_nested_region_presentation(&bridge, true, Some(revision));
    h.server.state.poll_region_selection(Instant::now());
    assert_eq!(body(&mut rx)["status"], "selected");
}

#[test]
fn region_decided_rectangle_survives_geometry_change() {
    let mut h = KeybindingHarness::new(true);
    let mut rx = begin(&mut h);
    motion(&mut h, 20., 20.);
    button(&mut h, 0x110, HostButtonState::Pressed);
    motion(&mut h, 80., 80.);
    button(&mut h, 0x110, HostButtonState::Released);
    h.server.state.resize_output(800, 600);
    h.server.state.poll_region_selection(Instant::now());
    clean_frame(&mut h);
    let result = body(&mut rx);
    assert_eq!(result["status"], "selected");
    assert_eq!(
        result["region"],
        serde_json::json!({"x":20,"y":20,"width":60,"height":60})
    );
}

#[test]
fn region_nonexistent_output_is_unknown_output() {
    let mut h = KeybindingHarness::new(true);
    h.server.state.region.bridge = Some(RegionBridge::default());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(
        LongOp::RegionSelect {
            output: Some("does-not-exist".into()),
            timeout: Duration::from_secs(30),
        },
        tx,
        Instant::now(),
    );
    assert!(matches!(
        rx.try_recv().unwrap(),
        ControlReply::Refused {
            error: "unknown_output",
            ..
        }
    ));
    assert!(!h.server.state.region.suspended);
}

#[test]
fn region_reply_deadline_is_timeout_plus_three_seconds() {
    let mut h = KeybindingHarness::new(true);
    h.server.state.region.bridge = Some(RegionBridge::default());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let admitted = Instant::now();
    h.server.state.start_long_op(
        LongOp::RegionSelect {
            output: None,
            timeout: Duration::from_secs(55),
        },
        tx,
        admitted,
    );
    motion(&mut h, 20., 20.);
    button(&mut h, 0x110, HostButtonState::Pressed);
    motion(&mut h, 80., 80.);
    button(&mut h, 0x110, HostButtonState::Released);
    h.server
        .state
        .poll_region_selection(admitted + Duration::from_millis(57_999));
    assert!(rx.try_recv().is_err());
    h.server
        .state
        .poll_region_selection(admitted + Duration::from_secs(58));
    assert_eq!(rx.try_recv().unwrap(), ControlReply::Busy);
}

#[test]
fn region_reverse_drag_waits_for_clean_frame_and_normalises() {
    let mut h = KeybindingHarness::new(true);
    let mut rx = begin(&mut h);
    motion(&mut h, 300.9, 210.2);
    button(&mut h, 0x110, HostButtonState::Pressed);
    motion(&mut h, 100.2, 80.1);
    button(&mut h, 0x110, HostButtonState::Released);
    assert!(!h.server.state.region.suspended);
    assert!(
        rx.try_recv().is_err(),
        "despawn request is not submission evidence"
    );
    let bridge = h.server.state.region.bridge.as_ref().unwrap().clone();
    let revision = test_remove_frame(&bridge);
    let o = h.server.state.backend.occlusion_outputs().remove(0);
    bridge.presented(&o.name, o.generation, Some(revision - 1));
    h.server.state.poll_region_selection(Instant::now());
    assert!(
        rx.try_recv().is_err(),
        "old in-flight frames cannot finish selection"
    );
    bridge.presented(&o.name, o.generation, Some(revision));
    h.server.state.poll_region_selection(Instant::now());
    let result = body(&mut rx);
    assert_eq!(result["status"], "selected");
    assert_eq!(
        result["region"],
        serde_json::json!({"x":100,"y":80,"width":201,"height":131})
    );
    assert_eq!(result["coordinate_space"], "output-local-logical");
    assert_eq!(result["output_generation"], o.generation);
}

#[test]
fn region_zero_area_stays_armed_and_second_caller_is_busy() {
    let mut h = KeybindingHarness::new(true);
    let mut first = begin(&mut h);
    let mut second = begin(&mut h);
    assert_eq!(second.try_recv().unwrap(), ControlReply::Busy);
    motion(&mut h, 40.5, 50.5);
    button(&mut h, 0x110, HostButtonState::Pressed);
    button(&mut h, 0x110, HostButtonState::Released);
    assert!(h.server.state.region.suspended);
    assert!(first.try_recv().is_err());
    button(&mut h, 0x110, HostButtonState::Pressed);
    motion(&mut h, 70.0, 50.5);
    button(&mut h, 0x110, HostButtonState::Released);
    assert!(
        h.server.state.region.suspended,
        "horizontal line is not a region"
    );
}

#[test]
fn region_escape_and_right_button_cancel_and_swallow_release_tails() {
    for escape in [true, false] {
        let mut h = KeybindingHarness::new(true);
        let mut rx = begin(&mut h);
        if escape {
            h.server.state.handle_host_input(HostInput::Key {
                keycode: Keycode::new(9),
                state: HostButtonState::Pressed,
                time: 1,
            });
        } else {
            button(&mut h, 0x111, HostButtonState::Pressed);
        }
        // No frame is submitted: cancellation must reply immediately.
        assert_eq!(
            body(&mut rx),
            serde_json::json!({"version":1,"status":"cancelled","reason":if escape {"escape"}else{"right_button"}})
        );
        if escape {
            h.server.state.handle_host_input(HostInput::Key {
                keycode: Keycode::new(9),
                state: HostButtonState::Released,
                time: 2,
            });
            assert!(h.server.state.keyboard.pressed_keys().is_empty());
        } else {
            button(&mut h, 0x111, HostButtonState::Released);
        }
        assert!(h.server.state.pointer.current_pressed().is_empty());
        let mut next = begin(&mut h);
        assert!(next.try_recv().is_err(), "tails do not strand busy state");
    }
}

#[test]
fn region_timeout_and_disappeared_waiter_restore_seat() {
    let mut h = KeybindingHarness::new(true);
    let mut rx = begin(&mut h);
    h.server
        .state
        .poll_region_selection(Instant::now() + Duration::from_secs(31));
    assert!(!h.server.state.region.suspended);
    assert_eq!(body(&mut rx)["status"], "timeout");
    let rx = begin(&mut h);
    drop(rx);
    h.server.state.poll_region_selection(Instant::now());
    assert!(!h.server.state.region.suspended);
    let mut next = begin(&mut h);
    assert!(next.try_recv().is_err());
}

#[test]
fn region_output_geometry_change_refuses_and_restores_seat() {
    let mut h = KeybindingHarness::new(true);
    let mut rx = begin(&mut h);
    h.server.state.resize_output(800, 600);
    h.server.state.poll_region_selection(Instant::now());
    assert!(matches!(
        rx.try_recv().unwrap(),
        ControlReply::Refused {
            error: "output_changed",
            ..
        }
    ));
    assert!(!h.server.state.region.suspended);
}

#[test]
fn region_no_clean_frame_never_reports_success() {
    let mut h = KeybindingHarness::new(true);
    let mut rx = begin(&mut h);
    motion(&mut h, 20., 20.);
    button(&mut h, 0x110, HostButtonState::Pressed);
    motion(&mut h, 80., 80.);
    button(&mut h, 0x110, HostButtonState::Released);
    h.server
        .state
        .poll_region_selection(Instant::now() + Duration::from_secs(34));
    assert_eq!(rx.try_recv().unwrap(), ControlReply::Busy);
    assert!(!h.server.state.region.suspended);
}

#[test]
fn region_existing_pressed_pointer_is_busy() {
    let mut h = KeybindingHarness::new(true);
    button(&mut h, 0x110, HostButtonState::Pressed);
    let mut rx = begin(&mut h);
    assert_eq!(rx.try_recv().unwrap(), ControlReply::Busy);
}

#[test]
fn region_focus_suspension_preserves_fullscreen_activation_and_stack() {
    let mut h = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut h);
    let surface = test_toplevel_record(&h).role.wl_surface().clone();
    h.server
        .state
        .arbitrate_keyboard_focus(Some(surface.clone()), false, false);
    let record = h.server.state.surfaces.get_mut(&surface.id()).unwrap();
    record.committed_fullscreen = true;
    let before = (record.focused, record.layout.z);
    assert!(before.0);
    let mut rx = begin(&mut h);
    assert!(h.server.state.keyboard.current_focus().is_none());
    assert_eq!(
        (
            test_toplevel_record(&h).focused,
            test_toplevel_record(&h).layout.z
        ),
        before
    );
    // Visibility/focus reconciliation during a modal operation must not enter
    // a client again or unset the operation's exclusive routing.
    h.server
        .state
        .arbitrate_keyboard_focus(Some(surface.clone()), true, false);
    h.server.state.retarget_pointer_after_visibility_change();
    assert!(h.server.state.keyboard.current_focus().is_none());
    assert!(h.server.state.pointer.current_focus().is_none());
    button(&mut h, 0x111, HostButtonState::Pressed);
    clean_frame(&mut h);
    assert_eq!(body(&mut rx)["status"], "cancelled");
    assert_eq!(
        h.server
            .state
            .keyboard
            .current_focus()
            .unwrap()
            .owned_surface(),
        Some(surface)
    );
    assert_eq!(
        (
            test_toplevel_record(&h).focused,
            test_toplevel_record(&h).layout.z
        ),
        before
    );
}

#[test]
fn region_kms_generation_loss_cancels_in_flight_selection() {
    let mut h = KeybindingHarness::new_with_backend(false, BackendKind::Kms);
    let key = kms_security_test_key(226, "Region-1");
    submit_kms_security_lifecycle(
        &mut h,
        KmsTopologyLifecycleEvent::Initial(kms_security_test_snapshot(&key, 91)),
    );
    h.commands
        .send(ProtocolCommand::KmsRenderReply {
            reply: KmsRenderReply::OutputReady {
                generation: 1,
                key: key.clone(),
            },
        })
        .unwrap();
    pump_protocol_event_loop_until(&mut h.server, "region output ready", |s| {
        s.backend.kms_output_is_ready(1, &key)
    });
    let mut rx = begin(&mut h);
    assert!(h.server.state.region.suspended);
    submit_kms_security_lifecycle(&mut h, KmsTopologyLifecycleEvent::Pause);
    // The lifecycle itself must latch the specific reason before generic
    // authority-loss cleanup. No later poll or clean frame should be needed.
    assert_eq!(
        rx.try_recv().unwrap(),
        ControlReply::Refused {
            error: "output_changed",
            detail: serde_json::json!({}),
        }
    );
    assert!(!h.server.state.region.suspended);
}

#[test]
fn region_popup_grab_and_touch_sequence_are_busy() {
    let (mut h, pointer, _, _) = positioned_test_ssd_harness(cosmix_deco::ChromeStyle::Mac);
    route_pointer_to(&mut h, 60., 80.);
    let _ = h.sync();
    route_pointer_button(&mut h, PRIMARY_POINTER_BUTTON, ButtonState::Pressed);
    let serial = word(&pointer_body(&h.sync(), pointer, 3), 0);
    map_test_popup(&mut h, Some(serial));
    route_pointer_button(&mut h, PRIMARY_POINTER_BUTTON, ButtonState::Released);
    let _ = h.sync();
    assert!(h.server.state.pointer.is_grabbed());
    let mut rx = begin(&mut h);
    assert_eq!(rx.try_recv().unwrap(), ControlReply::Busy);
    assert!(
        h.server.state.pointer.is_grabbed(),
        "admission must not destroy the existing popup grab"
    );

    let mut h = KeybindingHarness::new(true);
    h.server.state.handle_host_input(HostInput::TouchDown {
        slot: Some(0).into(),
        x: 20.,
        y: 20.,
        time: 1,
    });
    let mut rx = begin(&mut h);
    assert_eq!(rx.try_recv().unwrap(), ControlReply::Busy);
}

#[test]
fn region_locked_pointer_is_released_and_reconsidered_after_selection() {
    let mut h = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut h);
    let pointer = h.bind_pointer();
    h.prime_pointer_focus();
    let surface = test_toplevel_record(&h).role.wl_surface().clone();
    let manager = h.bind_test_global("zwp_pointer_constraints_v1", 1);
    let locked = h.allocate_object_id();
    send_request(
        &mut h.client,
        manager,
        1,
        &words(&[locked, TEST_TOPLEVEL_SURFACE_ID, pointer, 0, 2]),
    );
    let _ = h.sync();
    assert!(h.server.state.pointer_is_locked());
    let mut rx = begin(&mut h);
    let active = |h: &KeybindingHarness| {
        with_pointer_constraint(&surface, &h.server.state.pointer, |c| {
            c.is_some_and(|c| c.is_active())
        })
    };
    assert!(!active(&h));
    let before = h.server.state.cursor_position;
    h.server.state.handle_host_input(HostInput::PointerMotion {
        dx: 10.,
        dy: 10.,
        dx_unaccel: 10.,
        dy_unaccel: 10.,
        time: 2,
    });
    assert_ne!(
        before, h.server.state.cursor_position,
        "locked client's pointer must not freeze selection"
    );
    button(&mut h, 0x111, HostButtonState::Pressed);
    clean_frame(&mut h);
    assert_eq!(body(&mut rx)["status"], "cancelled");
    assert!(
        active(&h),
        "persistent constraint reactivated only after pointer focus restoration"
    );
}

#[test]
fn region_entry_reconciles_preheld_keys_without_stale_enter_keys() {
    let mut h = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut h);
    h.key(42, HostButtonState::Pressed); // Shift
    let mut rx = begin(&mut h);
    assert!(h.server.state.keyboard.pressed_keys().is_empty());
    h.key(42, HostButtonState::Released);
    button(&mut h, 0x111, HostButtonState::Pressed);
    clean_frame(&mut h);
    assert_eq!(body(&mut rx)["status"], "cancelled");
    assert!(h.server.state.keyboard.pressed_keys().is_empty());
}

#[test]
fn region_saved_focus_is_not_restored_to_a_minimised_window() {
    let mut h = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut h);
    let surface = test_toplevel_record(&h).role.wl_surface().clone();
    h.server
        .state
        .arbitrate_keyboard_focus(Some(surface.clone()), false, false);
    let mut rx = begin(&mut h);
    h.server.state.minimize_toplevel(&surface);
    button(&mut h, 0x111, HostButtonState::Pressed);
    clean_frame(&mut h);
    assert_eq!(body(&mut rx)["status"], "cancelled");
    assert!(h.server.state.keyboard.current_focus().is_none());
    assert!(!test_toplevel_record(&h).focused);
}

#[test]
fn region_timer_expires_without_an_input_event() {
    let mut h = KeybindingHarness::new(true);
    h.server.state.region.bridge = Some(RegionBridge::default());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(
        LongOp::RegionSelect {
            output: None,
            timeout: Duration::from_millis(20),
        },
        tx,
        Instant::now(),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while h.server.state.region.suspended {
        assert!(
            Instant::now() < deadline,
            "calloop timer must not depend on input"
        );
        h.server
            .dispatch_cycle(Some(Duration::from_millis(30)))
            .unwrap();
    }
    clean_frame(&mut h);
    assert_eq!(body(&mut rx)["status"], "timeout");
}
