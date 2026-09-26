// Included by input_injection_tests.rs: real queued Bus controls and wire devices.

#[test]
fn initial_agent_sequence_motion_precedes_a_later_standalone_click() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    let (_, _, pointer) = bind_agent_devices(&mut h);
    let motion = |object: &ObjectId| {
        let (id, generation) = window_id_and_generation(&h, object);
        on_agent(move_op(PointerMoveTarget::Window { id, generation, x: 10.0, y: 10.0, require_hit: true }))
    };
    let to_alpha = motion(&alpha);
    let to_beta = motion(&beta);
    assert_eq!(inject(&mut h, &ingress, &runtime, to_alpha).0, 0);
    let _ = h.sync();
    let sequence = ingress.request_long(crate::port::LongOp::Sequence(vec![
        step("comp.input.pointer.move", to_beta, 0),
    ])).unwrap();
    let click = ingress.request_input(on_agent(InputOp::PointerButton {
        button: BTN_LEFT, action: PressAction::Both,
    })).unwrap();
    h.server.dispatch_cycle(Some(Duration::ZERO)).unwrap();
    let click = runtime.block_on(click.receive()).unwrap().wire_json();
    let (id, generation) = window_id_and_generation(&h, &beta);
    assert_eq!(click["target"], json!({"id":id, "generation":generation}));
    assert!(runtime.block_on(sequence.receive()).unwrap().wire_json().get("error").is_none());
    let events = h.sync();
    let enter = events.iter().position(|(object, opcode, body)|
        *object == pointer && *opcode == 0 && word(body, 1) == beta.protocol_id()).unwrap();
    let press = events.iter().position(|(object, opcode, body)|
        *object == pointer && *opcode == 3 && word(body, 3) == 1).unwrap();
    assert!(enter < press, "motion/enter reaches B before its click");
}

#[test]
fn authority_loss_refuses_parked_agent_controls_without_recreating_holds() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let (id, generation) = window_id_and_generation(&h, &alpha);
    let mut admissions = Vec::new();
    for x in 0..11 {
        admissions.push(ingress.request_input(on_agent(move_op(PointerMoveTarget::Window {
            id, generation, x: 10.0 + f64::from(x), y: 10.0, require_hit: true,
        }))).unwrap());
    }
    admissions.push(ingress.request_input(on_agent(InputOp::PointerButton {
        button: BTN_LEFT, action: PressAction::Press,
    })).unwrap());
    h.server.dispatch_cycle(Some(Duration::ZERO)).unwrap();
    assert_eq!(h.server.state.pending_port_controls.len(), 4);
    h.server.state.reconcile_all_input_authority_loss();
    h.server.dispatch_cycle(Some(Duration::ZERO)).unwrap();
    for (index, admission) in admissions.into_iter().enumerate() {
        let reply = runtime.block_on(admission.receive()).unwrap().wire_json();
        if index < 8 { assert!(reply.get("error").is_none(), "{reply}"); }
        else {
            assert_eq!(reply["error"], "input_cleared");
            assert_eq!(reply["seat"], "agent");
            assert_eq!(reply["released"], true);
        }
    }
    assert!(h.server.state.agent.held.is_empty());
    assert!(h.server.state.agent.pointer.current_pressed().is_empty());
    assert!(!h.server.state.agent.pointer.is_grabbed());
    assert!(h.server.state.agent.pointer_position.is_none());
}

#[test]
fn authority_loss_epoch_refuses_ingress_inputs_and_sequence_admissions_only_for_agent() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let press = agent_target(&h, &alpha, agent_key(PressAction::Press, KEY_A));
    let input = ingress.request_input(press.clone()).unwrap();
    let sequence = ingress.request_long(crate::port::LongOp::Sequence(vec![
        step("comp.input.key", press, 0),
    ])).unwrap();
    let human = ingress.request_input(move_op(PointerMoveTarget::Output {
        output: None, x: 40.0, y: 30.0,
    })).unwrap();
    // All three requests are still in ingress, not pending_port_controls.
    h.server.state.reconcile_all_input_authority_loss();
    h.server.dispatch_cycle(Some(Duration::ZERO)).unwrap();
    assert_eq!(runtime.block_on(input.receive()).unwrap().wire_json()["error"], "input_cleared");
    assert_eq!(runtime.block_on(sequence.receive()).unwrap().wire_json()["error"], "input_cleared");
    assert!(runtime.block_on(human.receive()).unwrap().wire_json().get("error").is_none());
    assert!(h.server.state.agent.held.is_empty());
    assert!(h.server.state.injection.sequences.is_empty());
    assert_eq!(h.server.state.human.pointer.current_location(), (40.0, 30.0).into());
}

#[test]
fn agent_motion_burst_coalesces_without_crossing_buttons_or_human_motion() {
    let (mut h, ingress, runtime, human_pointer, alpha, _) = two_windows();
    let (_, _, agent_pointer) = bind_agent_devices(&mut h);
    let (id, generation) = window_id_and_generation(&h, &alpha);
    let motion = |x| on_agent(move_op(PointerMoveTarget::Window {
        id, generation, x, y: 25.0, require_hit: true,
    }));
    assert_eq!(inject(&mut h, &ingress, &runtime, motion(5.0)).0, 0);
    h.frame(vec![HostInput::PointerMotionAbsolute { x: 5.0, y: 25.0, time: 1 }]);
    let _ = h.sync();
    let operations = vec![motion(10.0), motion(20.0), motion(30.0), on_agent(InputOp::PointerButton {
        button: BTN_LEFT, action: PressAction::Both,
    }), on_agent(move_op(PointerMoveTarget::Relative { dx: 5.0, dy: 0.0 })),
        on_agent(move_op(PointerMoveTarget::Relative { dx: 7.0, dy: 0.0 }))];
    let admissions: Vec<_> = operations.into_iter().map(|op| ingress.request_input(op).unwrap()).collect();
    h.commands.send(ProtocolCommand::Frame { inputs: vec![
        HostInput::PointerMotionAbsolute { x: 75.0, y: 25.0, time: 2 },
    ] }).unwrap();
    h.server.dispatch_cycle(Some(Duration::ZERO)).unwrap();
    let replies: Vec<_> = admissions.into_iter().map(|admission|
        runtime.block_on(admission.receive()).unwrap().wire_json()).collect();
    for reply in &replies { assert!(reply.get("error").is_none(), "{reply}"); }
    assert_eq!(replies[0]["coalesced"], 3);
    assert_eq!(replies[0]["input_seq"], replies[2]["input_seq"]);
    assert_eq!(replies[4]["coalesced"], 2);
    assert_eq!(replies[4]["input_seq"], replies[5]["input_seq"]);
    assert_eq!(h.server.state.human.pointer.current_location(), (75.0, 25.0).into());
    assert_eq!(h.server.state.agent.pointer.current_location(), (42.0, 25.0).into());
    let traffic = h.sync();
    let events: Vec<_> = traffic.iter().filter(|(object, opcode, _)|
        (*object == agent_pointer || *object == human_pointer) && matches!(opcode, 2 | 3)
    ).collect();
    assert_eq!((events[0].0, events[0].1), (human_pointer, 2), "queued human input drains first");
    let agent: Vec<_> = events.into_iter().filter(|(object, _, _)| *object == agent_pointer).collect();
    assert_eq!(agent.iter().map(|(_, opcode, _)| *opcode).collect::<Vec<_>>(), [2, 3, 3, 2]);
    assert_eq!(fixed(&agent[0].2, 1), 30.0);
    assert_eq!(fixed(&agent[3].2, 1), 42.0);
}

#[test]
fn agent_motion_surface_transition_is_a_coalescing_fence() {
    let (mut h, ingress, runtime, _, alpha, beta) = two_windows();
    let (_, _, pointer) = bind_agent_devices(&mut h);
    let ops: Vec<_> = [&alpha, &beta, &alpha].into_iter().map(|object| {
        let (id, generation) = window_id_and_generation(&h, object);
        on_agent(move_op(PointerMoveTarget::Window { id, generation, x: 10.0, y: 10.0, require_hit: true }))
    }).collect();
    let _ = h.sync();
    let admissions: Vec<_> = ops.into_iter().map(|op| ingress.request_input(op).unwrap()).collect();
    h.server.dispatch_cycle(Some(Duration::ZERO)).unwrap();
    for admission in admissions {
        let reply = runtime.block_on(admission.receive()).unwrap().wire_json();
        assert!(reply.get("coalesced").is_none(), "{reply}");
    }
    assert_eq!(pointer_bodies(&h.sync(), pointer, 0).len(), 3, "all three enters are delivered");
}

#[test]
fn bounded_agent_controls_progress_each_turn_under_continuous_human_motion() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let (id, generation) = window_id_and_generation(&h, &alpha);
    let admissions: Vec<_> = (0..16).map(|index| ingress.request_input(on_agent(move_op(
        PointerMoveTarget::Window { id, generation, x: 10.0 + f64::from(index), y: 10.0, require_hit: true }
    ))).unwrap()).collect();
    for turn in 0..2 {
        h.commands.send(ProtocolCommand::Frame { inputs: vec![
            HostInput::PointerMotionAbsolute { x: 100.0 + f64::from(turn), y: 50.0, time: 1 },
        ] }).unwrap();
        h.server.dispatch_cycle(Some(Duration::ZERO)).unwrap();
        assert_eq!(h.server.state.agent.pointer_position, Some((17.0 + 8.0 * f64::from(turn), 10.0)));
        assert_eq!(h.server.state.pending_port_controls.len(), if turn == 0 { 8 } else { 0 });
    }
    for admission in admissions {
        assert!(runtime.block_on(admission.receive()).unwrap().wire_json().get("error").is_none());
    }
}

#[test]
fn agent_sequence_motion_burst_yields_and_wakes_without_a_poll_timer() {
    let (mut h, ingress, runtime, _, alpha, _) = two_windows();
    bind_agent_devices(&mut h);
    let (id, generation) = window_id_and_generation(&h, &alpha);
    let initial = on_agent(move_op(PointerMoveTarget::Window { id, generation, x: 10.0, y: 10.0, require_hit: true }));
    assert_eq!(inject(&mut h, &ingress, &runtime, initial).0, 0);
    // Stay within the public 256-step limit, but cross the 256-event burst
    // bound. Key taps fence the two motion runs and each deliver two events.
    let mut steps: Vec<_> = (0..100).map(|_| step("comp.input.pointer.move",
        on_agent(move_op(PointerMoveTarget::Relative { dx: 0.125, dy: 0.0 })), 0)).collect();
    steps.extend((0..130).map(|_| step("comp.input.key",
        agent_target(&h, &alpha, agent_key(PressAction::Both, KEY_A)), 0)));
    steps.extend((0..20).map(|_| step("comp.input.pointer.move",
        on_agent(move_op(PointerMoveTarget::Relative { dx: 0.125, dy: 0.0 })), 0)));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    h.server.state.start_long_op(crate::port::LongOp::Sequence(steps), sender, Instant::now());
    for (turn, expected) in [22.5, 25.0].into_iter().enumerate() {
        // First turn also has host input; subsequent turns must wake solely
        // from the queued continuation, without unrelated client traffic.
        if turn == 0 {
            h.commands.send(ProtocolCommand::Frame { inputs: vec![
                HostInput::PointerMotionAbsolute { x: 125.0, y: 50.0, time: 1 },
            ] }).unwrap();
        }
        let began = Instant::now();
        h.server.dispatch_cycle(Some(Duration::from_secs(1))).unwrap();
        assert!(began.elapsed() < Duration::from_millis(500), "runnable work must wake dispatch");
        assert_eq!(h.server.state.agent.pointer_position, Some((expected, 10.0)));
    }
    let reply = runtime.block_on(receiver).unwrap().wire_json();
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(reply["steps"].as_array().unwrap().len(), 250);
    assert_eq!(reply["steps"][0]["coalesced"], 100);
    assert_eq!(h.server.state.human.pointer.current_location(), (125.0, 50.0).into());
}
