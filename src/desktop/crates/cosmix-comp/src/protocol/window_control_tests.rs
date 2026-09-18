// `comp.window.*` and `comp.windows.list` (included from tests.rs).

use crate::port::{
    LongOp, PlaceSpec, WaitSpec, WaitUntil, WindowMatch, WindowOp, WorkspaceIndex,
};
use workspaces::WorkspaceTarget;

fn window_op(
    harness: &mut KeybindingHarness,
    ingress: &crate::port::PortIngress,
    runtime: &tokio::runtime::Runtime,
    op: WindowOp,
) -> (u8, Value) {
    let admission = ingress.request_window(op).expect("window verb admitted");
    serviced_control_reply(harness, runtime, admission)
}

/// Admit a long window verb and drive the loop until its waiter is gone.
fn long_window_op(
    harness: &mut KeybindingHarness,
    ingress: &crate::port::PortIngress,
    runtime: &tokio::runtime::Runtime,
    op: LongOp,
    between: impl FnOnce(&mut KeybindingHarness),
) -> (u8, Value) {
    let admission = ingress.request_long(op).expect("long verb admitted");
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("registration cycle");
    between(harness);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !harness.server.state.window_waiters.waiters.is_empty() {
        harness
            .server
            .dispatch_cycle(Some(Duration::from_millis(5)))
            .expect("waiter cycle");
        assert!(Instant::now() < deadline, "waiter never resolved");
    }
    let (rc, body) = runtime
        .block_on(admission.receive())
        .expect("long verb reply")
        .into_wire();
    (rc, serde_json::from_str(&body).unwrap())
}

fn wait_for(window: WindowMatch, until: WaitUntil, timeout_ms: u64) -> LongOp {
    LongOp::Wait(WaitSpec {
        window,
        until,
        timeout: Duration::from_millis(timeout_ms),
    })
}

fn by_id(id: u64, generation: u64) -> WindowMatch {
    WindowMatch {
        id: Some(id),
        generation: Some(generation),
        ..WindowMatch::default()
    }
}

fn two_mapped_windows() -> (
    KeybindingHarness,
    crate::port::PortIngress,
    port_observation::ObservationOutbox,
    tokio::runtime::Runtime,
    ObjectId,
    ObjectId,
) {
    let (mut harness, ingress, observations) = KeybindingHarness::new_with_port();
    map_initial_test_toplevel(&mut harness);
    let alpha = test_toplevel_record(&harness).role.wl_surface().id();
    let (_, _, _, beta) = map_named_test_toplevel(&mut harness, "Beta", "dev.cosmix.Beta");
    let surface = harness.server.state.surfaces[&beta]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&surface);
    let _ = harness.sync();
    (
        harness,
        ingress,
        observations,
        control_reply_runtime(),
        alpha,
        beta,
    )
}

fn place(id: u64, generation: u64) -> PlaceSpec {
    PlaceSpec {
        id,
        generation,
        output: None,
        x: None,
        y: None,
        width: None,
        height: None,
    }
}

#[test]
fn place_moves_the_window_geometry_origin_output_locally() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let _ = take_renderer_events(&mut harness);
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            x: Some(12.0),
            y: Some(34.0),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 0, "{body}");
    assert!(body["output"].is_string());
    assert_eq!((body["window_x"].clone(), body["window_y"].clone()), (json!(12.0), json!(34.0)));
    assert_eq!(body["requested"], Value::Null);
    assert_eq!(body["configure_pending"], false);
    let record = &harness.server.state.surfaces[&alpha];
    assert_eq!(record.window_origin, (12.0, 34.0));
    let offset = record
        .committed_window_geometry
        .map_or((0.0, 0.0), |geometry| (geometry.x, geometry.y));
    assert_eq!(
        (record.layout.x, record.layout.y),
        (12.0 - offset.0, 34.0 - offset.1)
    );
    let events = take_renderer_events(&mut harness);
    assert!(
        events.iter().any(|event| matches!(
            event,
            ProtocolEvent::SurfaceRelayout { id: moved, .. } if moved.0 == id
        )),
        "the move reaches the renderer: {events:?}"
    );
    // The snapshot row agrees, and an absent coordinate keeps its value.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            y: Some(5.0),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(harness.server.state.surfaces[&alpha].window_origin, (12.0, 5.0));
    let context = harness.server.state.port_context.clone().expect("context");
    let snapshot = port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot");
    let row = &snapshot.windows[&format!("s{id}")];
    assert_eq!((row.window_x, row.window_y), (12.0, 5.0));

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            output: Some("o_elsewhere".into()),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 10);
    assert_eq!(body, json!({"error": "unknown_output", "output": "o_elsewhere"}));
}

/// Every renderer batch published so far, flattened.
fn take_renderer_events(harness: &mut KeybindingHarness) -> Vec<ProtocolEvent> {
    let mut events = std::mem::take(&mut harness.server.state.events);
    while let Ok(batch) = harness.renderer_events.try_recv() {
        events.extend(batch);
    }
    events
}

#[test]
fn place_resize_sends_a_clamped_configure_and_refuses_maximized_windows() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            x: Some(0.0),
            y: Some(0.0),
            width: Some(10_000),
            height: Some(200),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["configure_pending"], true);
    let (min, _) = clamped_toplevel_constraints(
        harness
            .server
            .state
            .managed_size_constraints(&harness.server.state.surfaces[&alpha].role.wl_surface().clone()),
    );
    let expected = (10_000.min(MAX_SURFACE_DIMENSION as i32), 200.max(min.1));
    assert_eq!(
        body["requested"],
        json!({"width": expected.0, "height": expected.1})
    );
    assert!(expected.0 < 10_000, "the width was clamped to the maximum");
    assert_eq!(harness.server.state.surfaces[&alpha].configured_size, expected);
    assert_eq!(configured_toplevel_size(&harness.sync()), expected);
    assert_eq!(harness.server.state.surfaces[&alpha].window_origin, (0.0, 0.0));

    // The same size again is not a new configure.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            width: Some(expected.0),
            height: Some(expected.1),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["configure_pending"], false);

    let _ = request_test_maximized(&mut harness, true);
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            x: Some(9.0),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "invalid_state");
    assert_eq!(body["maximized"], true);
    assert_eq!(harness.server.state.surfaces[&alpha].window_origin, (0.0, 0.0));
}

#[test]
fn focus_raise_and_close_act_on_the_named_window() {
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    assert!(harness.server.state.surfaces[&beta].focused);
    let z = |harness: &KeybindingHarness, object: &ObjectId| {
        harness.server.state.surfaces[object].layout.z
    };

    // Focus without raise: keyboard moves, stacking does not.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Focus {
            id: alpha_id,
            generation: alpha_generation,
            raise: false,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(
        body,
        json!({"id": alpha_id, "generation": alpha_generation, "focused": true})
    );
    assert!(harness.server.state.surfaces[&alpha].focused);
    assert!(z(&harness, &alpha) < z(&harness, &beta));

    // Raise without focus.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Raise {
            id: alpha_id,
            generation: alpha_generation,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["raised"], true);
    assert!(z(&harness, &alpha) > z(&harness, &beta));

    // Focus with raise (the default) brings Beta back on top and focused.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Focus {
            id: beta_id,
            generation: beta_generation,
            raise: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["focused"], true);
    assert!(z(&harness, &beta) > z(&harness, &alpha));
    assert!(harness.server.state.surfaces[&beta].focused);

    // A minimised window cannot take focus, and says why.
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&surface);
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Focus {
            id: alpha_id,
            generation: alpha_generation,
            raise: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["focused"], false);
    assert_eq!(body["reason"], "minimized");
    harness.server.state.restore_window(&alpha);
    let _ = harness.sync();

    // Polite close is the xdg close event, nothing more.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Close {
            id: alpha_id,
            generation: alpha_generation,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["closed"], "polite");
    let traffic = harness.sync();
    assert!(
        traffic
            .iter()
            .any(|(object, opcode, _)| *object == TEST_TOPLEVEL_ID && *opcode == 1),
        "xdg_toplevel.close is sent: {traffic:?}"
    );
    harness.assert_client_connected("after a polite close");
}

#[test]
fn focus_under_an_exclusive_layer_is_refused_with_a_reason() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let exclusive = zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive as u32;
    let (layer, _) = map_test_layer_surface(
        &mut harness,
        0,
        TestLayerSpec {
            keyboard_interactivity: exclusive,
            ..TestLayerSpec::default()
        },
    );
    let _ = harness.sync();
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Focus {
            id,
            generation,
            raise: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(
        body,
        json!({"id": id, "generation": generation, "focused": false, "reason": "exclusive_layer"})
    );
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(
            test_layer_record(&harness, layer.surface)
                .role
                .wl_surface()
                .clone()
        )
    );
}

/// Every window verb is fenced by `{id, generation}`, and all of them are
/// refused under a session lock.
#[test]
fn window_verbs_refuse_stale_targets_and_the_lock() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let stale = generation + 5;
    let ops = |generation: u64| {
        vec![
            WindowOp::Minimize { id, generation },
            WindowOp::Restore {
                target: Some((id, generation)),
            },
            WindowOp::Focus {
                id,
                generation,
                raise: true,
            },
            WindowOp::Raise { id, generation },
            WindowOp::Close { id, generation },
            WindowOp::Place(PlaceSpec {
                x: Some(1.0),
                ..place(id, generation)
            }),
            WindowOp::SendToWorkspace {
                id,
                generation,
                index: WorkspaceIndex::Absolute(2),
                follow: true,
            },
        ]
    };
    let origin = harness.server.state.surfaces[&alpha].window_origin;
    for op in ops(stale) {
        let (rc, body) = window_op(&mut harness, &ingress, &runtime, op.clone());
        assert_eq!(rc, 10, "{op:?}");
        assert_eq!(
            body,
            json!({"error": "stale_target", "id": id, "generation": stale, "current": generation}),
            "{op:?}"
        );
    }
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        LongOp::ForceClose {
            id,
            generation: stale,
            timeout: Duration::from_millis(10),
        },
        |_| {},
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "stale_target");
    assert_eq!(harness.server.state.surfaces[&alpha].window_origin, origin);
    assert!(!harness.server.state.surfaces[&alpha].minimized);
    harness.assert_client_connected("after stale verbs");

    let lock = begin_test_session_lock(&mut harness);
    ack_and_map_test_lock_surface(&mut harness, lock);
    for op in ops(generation) {
        let (rc, body) = window_op(&mut harness, &ingress, &runtime, op.clone());
        assert_eq!((rc, body), (10, json!({"error": "locked"})), "{op:?}");
    }
    assert_eq!(harness.server.state.surfaces[&alpha].window_origin, origin);
    // D12: a switch names no window, but it changes what is on screen, so
    // the lock refuses it too and `current` stays where it was.
    let current = harness.server.state.workspace_current();
    for index in [WorkspaceIndex::Absolute(2), WorkspaceIndex::Next] {
        let op = WindowOp::SwitchWorkspace {
            output: None,
            index,
            wrap: true,
        };
        let (rc, body) = window_op(&mut harness, &ingress, &runtime, op.clone());
        assert_eq!((rc, body), (10, json!({"error": "locked"})), "{op:?}");
    }
    assert_eq!(harness.server.state.workspace_current(), current);
    assert_eq!(harness.server.state.surfaces[&alpha].workspace, current);
}

/// Rule 7 over the verb: `next`/`prev` wrap at the ends, `wrap:false`
/// refuses `at_end` and leaves `current` alone, an index outside
/// `1..=count` (0 included) and an unknown output are `invalid_value`, and
/// a switch reaches the observation lane (the window rows' `visible`).
#[test]
fn workspace_switch_verb_wraps_and_refuses_at_end() {
    let (mut harness, ingress, observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, _) = window_id_and_generation(&harness, &alpha);
    let watch = ingress.request_watch().expect("watch admitted");
    serviced_watch(&mut harness, &runtime, watch);
    port_observation::service_observations(&mut harness.server.state);
    drain_observations(&observations);
    let switch = |index: WorkspaceIndex, wrap: bool| WindowOp::SwitchWorkspace {
        output: None,
        index,
        wrap,
    };
    let output = "o_cosmix_nested_0";
    assert_eq!(
        harness.server.state.default_output_key().as_deref(),
        Some(output)
    );

    for (from, to) in [(1, 2), (2, 3), (3, 4), (4, 1)] {
        let (rc, body) = window_op(
            &mut harness,
            &ingress,
            &runtime,
            switch(WorkspaceIndex::Next, true),
        );
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body, json!({"output": output, "from": from, "to": to}));
        assert_eq!(harness.server.state.workspace_current(), to);
    }
    assert!(harness.server.state.surfaces[&alpha].layout.visible);
    assert!(harness.server.state.surfaces[&beta].layout.visible);

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        switch(WorkspaceIndex::Prev, true),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body, json!({"output": output, "from": 1, "to": 4}));
    assert_eq!(harness.server.state.workspace_current(), 4);
    assert!(!harness.server.state.surfaces[&alpha].layout.visible);
    assert!(!harness.server.state.surfaces[&alpha].minimized);
    // D7: the switch marks the whole tree dirty with its own cause. The
    // mark is not observable here — the verb is serviced inside the
    // dispatch cycle's calloop turn and `service_observations` runs later
    // in that same cycle, so by the time `window_op` returns the mark has
    // been taken and diffed. What IS observable is the diff it scheduled:
    // the row's `visible` flips with cause `workspace.switch`.
    port_observation::service_observations(&mut harness.server.state);
    let changed = drain_observations(&observations);
    let alpha_visible = format!("windows.s{alpha_id}.visible");
    assert!(
        changed.iter().any(|record| matches!(
            record,
            port_observation::ObservationRecord::PropsChanged { path, new, cause, .. }
                if *path == alpha_visible
                    && new.wire_value() == json!(false)
                    && *cause == "workspace.switch"
        )),
        "the switch re-diffs the whole tree with its own cause (D7): {changed:?}"
    );

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        switch(WorkspaceIndex::Next, false),
    );
    assert_eq!(rc, 10, "{body}");
    assert_eq!(
        body,
        json!({"error": "at_end", "output": output, "from": 4, "count": 4})
    );
    assert_eq!(harness.server.state.workspace_current(), 4);
    // Addressed by output NAME, the refusal still names the output by its
    // key, as the success reply does — a caller keying replies by output
    // sees one spelling on both paths.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::SwitchWorkspace {
            output: Some("cosmix-nested-0".into()),
            index: WorkspaceIndex::Next,
            wrap: false,
        },
    );
    assert_eq!(rc, 10, "{body}");
    assert_eq!(
        body,
        json!({"error": "at_end", "output": output, "from": 4, "count": 4})
    );

    for (index, output) in [
        (WorkspaceIndex::Absolute(5), None),
        (WorkspaceIndex::Absolute(0), None),
        (WorkspaceIndex::Absolute(1), Some("o_nope")),
    ] {
        let (rc, body) = window_op(
            &mut harness,
            &ingress,
            &runtime,
            WindowOp::SwitchWorkspace {
                output: output.map(str::to_string),
                index,
                wrap: true,
            },
        );
        assert_eq!(rc, 10, "{index:?} {output:?}: {body}");
        assert_eq!(
            body["error"], "invalid_value",
            "{index:?} {output:?}: {body}"
        );
        assert_eq!(
            body["path"],
            if output.is_some() { "output" } else { "index" },
            "{index:?} {output:?}: {body}"
        );
        assert_eq!(harness.server.state.workspace_current(), 4);
    }
    assert_eq!(
        window_op(
            &mut harness,
            &ingress,
            &runtime,
            WindowOp::SwitchWorkspace {
                output: Some(output.into()),
                index: WorkspaceIndex::Absolute(1),
                wrap: true,
            },
        ),
        (0, json!({"output": output, "from": 4, "to": 1})),
        "the default output by its key"
    );
    let index_refusal = window_op(
        &mut harness,
        &ingress,
        &runtime,
        switch(WorkspaceIndex::Absolute(5), true),
    )
    .1;
    assert_eq!(index_refusal["range"], "1..=4");
}

/// Rule 5 over the verb: a send without `follow` moves the window and
/// leaves `current` alone; with `follow` it switches and activates it;
/// `next`/`prev` are relative to the window's own workspace, not the
/// current one.
#[test]
fn send_to_workspace_moves_and_follows() {
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    let send = |id, generation, index, follow| WindowOp::SendToWorkspace {
        id,
        generation,
        index,
        follow,
    };

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        send(beta_id, beta_generation, WorkspaceIndex::Absolute(3), false),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(
        body,
        json!({"id": beta_id, "generation": beta_generation, "index": 3})
    );
    let state = &harness.server.state;
    assert_eq!(state.workspace_current(), 1);
    assert_eq!(state.surfaces[&beta].workspace, 3);
    assert!(!state.surfaces[&beta].layout.visible);
    assert!(!state.surfaces[&beta].minimized);
    assert!(state.surfaces[&alpha].layout.visible);
    assert!(state.surfaces[&alpha].focused, "focus falls back to alpha");
    assert_eq!(
        window_id_and_generation(&harness, &beta),
        (beta_id, beta_generation),
        "a move never bumps the generation"
    );

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        send(beta_id, beta_generation, WorkspaceIndex::Absolute(4), true),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(
        body,
        json!({"id": beta_id, "generation": beta_generation, "index": 4, "followed": true})
    );
    let state = &harness.server.state;
    assert_eq!(state.workspace_current(), 4);
    assert_eq!(state.surfaces[&beta].workspace, 4);
    assert!(state.surfaces[&beta].layout.visible);
    assert!(state.surfaces[&beta].focused, "follow activates the window");
    assert!(!state.surfaces[&alpha].layout.visible);
    assert!(!state.surfaces[&alpha].focused);
    // A follow to the workspace the window is already current on is still
    // a follow (it activates), and says so.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        send(beta_id, beta_generation, WorkspaceIndex::Absolute(4), true),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["followed"], true);
    assert_eq!(harness.server.state.workspace_current(), 4);

    // Alpha is on 1 while current is 4: `next` relative to the window's
    // own workspace is 2 (relative to the current one it would wrap to 1).
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        send(alpha_id, alpha_generation, WorkspaceIndex::Next, false),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["index"], 2);
    let state = &harness.server.state;
    assert_eq!(state.surfaces[&alpha].workspace, 2);
    assert_eq!(state.workspace_current(), 4);
    assert!(!state.surfaces[&alpha].layout.visible);
    // `prev` from 1 wraps to the last workspace.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        send(
            alpha_id,
            alpha_generation,
            WorkspaceIndex::Absolute(1),
            false,
        ),
    );
    assert_eq!(rc, 0, "{body}");
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        send(alpha_id, alpha_generation, WorkspaceIndex::Prev, false),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["index"], 4);
    assert!(harness.server.state.surfaces[&alpha].layout.visible);
    // Out of range is `invalid_value` naming the count.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        send(
            alpha_id,
            alpha_generation,
            WorkspaceIndex::Absolute(5),
            false,
        ),
    );
    assert_eq!(rc, 10, "{body}");
    assert_eq!(body["error"], "invalid_value");
    assert_eq!(body["path"], "index");
    assert_eq!(body["range"], "1..=4");
    assert_eq!(harness.server.state.surfaces[&alpha].workspace, 4);
}

/// D18 over the verb: under an exclusive layer `send_to_workspace
/// {follow:true}` still moves the window, but the follow is inert — the
/// screen is not re-arranged under the layer, `current` stays, the window
/// is not activated, and the reply says `followed:false` rather than
/// claiming a switch that did not happen.
#[test]
fn send_to_workspace_follow_is_inert_under_an_exclusive_layer() {
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    let exclusive = zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive as u32;
    let (layer, _) = map_test_layer_surface(
        &mut harness,
        0,
        TestLayerSpec {
            keyboard_interactivity: exclusive,
            ..TestLayerSpec::default()
        },
    );
    let _ = harness.sync();
    let layer_surface = test_layer_record(&harness, layer.surface)
        .role
        .wl_surface()
        .clone();
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::SendToWorkspace {
            id: beta_id,
            generation: beta_generation,
            index: WorkspaceIndex::Absolute(3),
            follow: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(
        body,
        json!({"id": beta_id, "generation": beta_generation, "index": 3, "followed": false})
    );
    let state = &harness.server.state;
    assert_eq!(state.workspace_current(), 1);
    assert_eq!(state.surfaces[&beta].workspace, 3);
    assert!(!state.surfaces[&beta].layout.visible);
    assert!(!state.surfaces[&beta].focused);
    assert!(state.surfaces[&alpha].layout.visible);
    assert_eq!(
        focused_surface(state.keyboard.current_focus()),
        Some(layer_surface),
        "the exclusive layer keeps the keyboard"
    );
}

/// Rule 12 (F1.9): `wait {until:"visible"}` and `{until:"presented"}` on
/// an off-workspace window time out rather than resolving, and resolve
/// once a switch brings its workspace on screen.
#[test]
fn wait_until_visible_times_out_off_workspace_and_resolves_after_a_switch() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let surface_id = harness.server.state.surfaces[&alpha].id;
    assert!(harness.server.state.surfaces[&alpha].layout.visible);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    assert!(!harness.server.state.surfaces[&alpha].layout.visible);
    assert!(!harness.server.state.surfaces[&alpha].minimized);

    // Mapped regardless of workspace.
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(id, generation), WaitUntil::Mapped, 30),
        |_| {},
    );
    assert_eq!(rc, 0, "mapped is workspace-blind: {body}");

    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(id, generation), WaitUntil::Visible, 30),
        |_| {},
    );
    assert_eq!(rc, 10, "off-workspace is not visible: {body}");
    assert_eq!(body["error"], "timeout");
    assert_eq!(body["until"], "visible");

    // A frame the renderer claims to have shown while the window is off
    // its workspace is Hidden, never presented.
    let (presentation, _) = bind_test_presentation(&mut harness);
    let _feedback = request_presentation_feedback(&mut harness, presentation);
    commit_test_buffer(&mut harness, TEST_TOPLEVEL_SURFACE_ID);
    harness.dispatch_client();
    let (frame, content) = test_frame_report(
        surface_id,
        monotonic_micros(),
        content_seq(&harness, &alpha),
        true,
    );
    harness.server.state.frame_presented(frame, content);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(id, generation), WaitUntil::Presented, 30),
        |_| {},
    );
    assert_eq!(rc, 10, "off-workspace is not presented: {body}");
    assert_eq!(body["error"], "timeout");

    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(id, generation), WaitUntil::Visible, 5_000),
        |harness| {
            harness
                .server
                .state
                .switch_workspace(None, WorkspaceTarget::Index(2), true)
                .expect("switch to the window's workspace");
        },
    );
    assert_eq!(rc, 0, "visible after the switch: {body}");
    assert_eq!(body["until"], "visible");
    assert_eq!(body["window"]["id"], id);
    assert!(harness.server.state.surfaces[&alpha].layout.visible);

    let _feedback = request_presentation_feedback(&mut harness, presentation);
    commit_test_buffer(&mut harness, TEST_TOPLEVEL_SURFACE_ID);
    harness.dispatch_client();
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(id, generation), WaitUntil::Presented, 5_000),
        |harness| {
            let (frame, content) = test_frame_report(
                surface_id,
                monotonic_micros(),
                content_seq(harness, &alpha),
                true,
            );
            harness.server.state.frame_presented(frame, content);
        },
    );
    assert_eq!(rc, 0, "presented once its workspace is current: {body}");
    assert_eq!(body["until"], "presented");
}


#[test]
fn wait_resolves_now_on_an_edge_or_times_out() {
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);

    // Already true: answered at registration, with the row.
    let started = Instant::now();
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(
            WindowMatch {
                app_id: Some("dev.cosmix.Beta".into()),
                ..WindowMatch::default()
            },
            WaitUntil::Mapped,
            5_000,
        ),
        |_| {},
    );
    assert_eq!(rc, 0, "{body}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(body["until"], "mapped");
    assert_eq!(body["window"]["id"], beta_id);
    assert_eq!(body["window"]["generation"], beta_generation);
    assert_eq!(body["window"]["title"], "Beta");

    // On the edge: Alpha is not focused until a verb focuses it.
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(
            by_id(alpha_id, alpha_generation),
            WaitUntil::Focused,
            5_000,
        ),
        |harness| {
            assert_eq!(
                harness.server.state.window_waiters.waiters.len(),
                1,
                "not true yet, so the waiter is registered"
            );
            let surface = harness.server.state.surfaces[&alpha]
                .role
                .wl_surface()
                .clone();
            harness.server.state.activate_managed_window(&surface);
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["window"]["id"], alpha_id);
    assert_eq!(body["window"]["focused"], true);

    // Minimised: visible times out on the timer; restore resolves it.
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&surface);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(alpha_id, alpha_generation), WaitUntil::Visible, 30),
        |_| {},
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "timeout");
    assert_eq!(body["until"], "visible");
    assert!(body["waited_ms"].as_u64().unwrap() >= 30);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(alpha_id, alpha_generation), WaitUntil::Visible, 5_000),
        |harness| assert!(harness.server.state.restore_window(&alpha)),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["window"]["visible"], true);

    // Size is the window-geometry size; a stale generation never matches,
    // so `gone` for it holds at once.
    let size = geometry_size_of(&harness, &alpha);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(
            by_id(alpha_id, alpha_generation),
            WaitUntil::Size {
                width: size.0,
                height: size.1,
            },
            5_000,
        ),
        |_| {},
    );
    assert_eq!(rc, 0, "{body}");
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(alpha_id, alpha_generation + 9), WaitUntil::Gone, 5_000),
        |_| {},
    );
    assert_eq!((rc, body["window"].clone()), (0, Value::Null), "{body}");

    // Destroying the role is `gone` for that generation, on the edge.
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(alpha_id, alpha_generation), WaitUntil::Gone, 5_000),
        |harness| {
            send_request(&mut harness.client, TEST_TOPLEVEL_ID, 0, &[]);
            harness.dispatch_client();
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["until"], "gone");
}

fn geometry_size_of(harness: &KeybindingHarness, object: &ObjectId) -> (i32, i32) {
    let record = &harness.server.state.surfaces[object];
    record.committed_window_geometry.map_or(
        (
            record.layout.width.round() as i32,
            record.layout.height.round() as i32,
        ),
        |geometry| (geometry.width.round() as i32, geometry.height.round() as i32),
    )
}

/// `close {force}` kills only when the same generation is still alive at
/// the deadline; a window that goes first is `gone` and nothing is killed.
#[test]
fn force_close_kills_only_a_window_still_alive_at_the_deadline() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        LongOp::ForceClose {
            id,
            generation,
            timeout: Duration::from_secs(5),
        },
        |harness| {
            // The client honours the close by destroying the role.
            send_request(&mut harness.client, TEST_TOPLEVEL_ID, 0, &[]);
            harness.dispatch_client();
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["closed"], "gone");
    harness.assert_client_connected("a window that closed itself is not killed");

    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, _) = window_id_and_generation(&harness, &beta);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        LongOp::ForceClose {
            id,
            generation,
            timeout: Duration::from_millis(20),
        },
        |_| {},
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["closed"], "killed");
    assert_eq!(body["scope"], "client");
    assert_eq!(body["pid"], std::process::id());
    let windows = body["windows"].as_array().expect("windows");
    assert!(windows.contains(&json!(id)) && windows.contains(&json!(beta_id)));
    assert!(body["waited_ms"].as_u64().unwrap() >= 20);
    let _ = harness
        .server
        .display
        .dispatch_clients(&mut harness.server.state);
    let reason = harness
        .client_state
        .disconnect_reason
        .lock()
        .expect("disconnect-reason mutex")
        .clone();
    assert_eq!(
        reason.as_deref(),
        Some("ConnectionClosed"),
        "the whole client connection was killed"
    );
}

#[test]
fn windows_list_filters_rows_in_id_order() {
    let (mut harness, _ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, _) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&surface);
    let context = harness.server.state.port_context.clone().expect("context");
    let snapshot = Arc::new(
        port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot"),
    );
    let list = |args: Value| {
        let (rc, body) = runtime.block_on(port_snapshot::dispatch_read(
            Arc::clone(&snapshot),
            "comp.windows.list".into(),
            args,
        ));
        (rc, serde_json::from_str::<Value>(&body).unwrap())
    };
    let ids = |body: &Value| {
        body["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };

    let (rc, body) = list(Value::Null);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(ids(&body), [alpha_id, beta_id]);
    let beta_row = &body["windows"][1];
    assert_eq!(beta_row["generation"], beta_generation);
    assert_eq!(beta_row["app_id"], "dev.cosmix.Beta");
    for leaf in ["window_x", "window_y", "visible", "pid"] {
        assert!(beta_row.get(leaf).is_some(), "row has {leaf}: {beta_row}");
    }
    assert_eq!(ids(&list(json!({"visible": true})).1), [beta_id]);
    assert_eq!(ids(&list(json!({"visible": false})).1), [alpha_id]);
    assert_eq!(ids(&list(json!({"app_id": "dev.cosmix.Beta"})).1), [beta_id]);
    assert_eq!(ids(&list(json!({"title_contains": "et"})).1), [beta_id]);
    assert_eq!(ids(&list(json!({"title": "Nope"})).1), Vec::<u64>::new());

    let (rc, body) = list(json!({"appid": "x"}));
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "invalid_args");
    assert_eq!(body["field"], "appid");
    let (rc, body) = list(json!({"visible": "yes"}));
    assert_eq!(rc, 10);
    assert_eq!(body["path"], "visible");
}

/// Rule 4: rows carry `workspace`; the `workspace` filter takes an index,
/// `"current"` (resolved against the snapshot's own current workspace, so
/// it follows a switch) or `"all"` (the default, every window), composes
/// with the other filters, and a bad value names the field.
#[test]
fn windows_list_filters_by_workspace() {
    use workspaces::WorkspaceTarget;
    let (mut harness, _ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, _) = window_id_and_generation(&harness, &alpha);
    let (beta_id, _) = window_id_and_generation(&harness, &beta);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&beta, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    let context = harness.server.state.port_context.clone().expect("context");
    let snapshot = Arc::new(
        port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot"),
    );
    let list = |snapshot: &Arc<port_snapshot::CompSnapshot>, args: Value| {
        let (rc, body) = runtime.block_on(port_snapshot::dispatch_read(
            Arc::clone(snapshot),
            "comp.windows.list".into(),
            args,
        ));
        (rc, serde_json::from_str::<Value>(&body).unwrap())
    };
    let ids = |body: &Value| {
        body["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };

    let (rc, body) = list(&snapshot, Value::Null);
    assert_eq!(rc, 0, "{body}");
    assert_eq!(ids(&body), [alpha_id, beta_id]);
    assert_eq!(body["windows"][0]["workspace"], 1);
    assert_eq!(body["windows"][1]["workspace"], 2);
    assert_eq!(body["windows"][1]["visible"], false);
    assert_eq!(body["windows"][1]["minimized"], false);
    assert_eq!(
        ids(&list(&snapshot, json!({"workspace": "current"})).1),
        [alpha_id]
    );
    assert_eq!(ids(&list(&snapshot, json!({"workspace": 2})).1), [beta_id]);
    assert_eq!(
        ids(&list(&snapshot, json!({"workspace": "all"})).1),
        [alpha_id, beta_id]
    );
    assert_eq!(
        ids(&list(&snapshot, json!({"workspace": null})).1),
        [alpha_id, beta_id]
    );
    assert_eq!(
        ids(&list(&snapshot, json!({"workspace": 3})).1),
        Vec::<u64>::new()
    );
    assert_eq!(
        ids(&list(&snapshot, json!({"workspace": 2, "visible": true})).1),
        Vec::<u64>::new(),
        "filters compose"
    );
    // An index above the count is "no such workspace", not an empty list
    // (every other workspace input refuses it too); the u64 that does not
    // fit a u32 is refused the same way rather than clamped.
    for bad in [
        json!("sideways"),
        json!(0),
        json!(-1),
        json!(true),
        json!(1.5),
        json!(5),
        json!(5_000_000_000_u64),
    ] {
        let (rc, body) = list(&snapshot, json!({"workspace": bad}));
        assert_eq!(rc, 10, "{bad}: {body}");
        assert_eq!(body["error"], "invalid_value");
        assert_eq!(body["path"], "workspace");
        assert_eq!(body["range"], "1..=count|current|all");
    }

    // "current" follows the snapshot's current workspace.
    harness
        .server
        .state
        .switch_workspace(None, WorkspaceTarget::Index(2), true)
        .expect("switch to 2");
    let snapshot = Arc::new(
        port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot"),
    );
    assert_eq!(
        ids(&list(&snapshot, json!({"workspace": "current"})).1),
        [beta_id]
    );
    assert_eq!(
        ids(&list(&snapshot, json!({"workspace": "current", "visible": true})).1),
        [beta_id]
    );
    assert_eq!(
        ids(&list(&snapshot, json!({"visible": false})).1),
        [alpha_id]
    );
}

fn unmap_alpha(harness: &mut KeybindingHarness) {
    send_request(
        &mut harness.client,
        TEST_TOPLEVEL_SURFACE_ID,
        1,
        &words(&[0, 0, 0]),
    );
    send_request(&mut harness.client, TEST_TOPLEVEL_SURFACE_ID, 6, &[]);
    harness.dispatch_client();
}

/// A hide-on-close app unmaps but stays alive: that is not `gone`, and the
/// deadline kills it, saying it was unmapped.
#[test]
fn force_close_kills_a_window_that_only_unmapped() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        LongOp::ForceClose {
            id,
            generation,
            timeout: Duration::from_millis(40),
        },
        |harness| {
            unmap_alpha(harness);
            assert!(!harness.server.state.surfaces[&alpha].mapped);
            harness
                .server
                .dispatch_cycle(Some(Duration::ZERO))
                .expect("cycle after the unmap");
            assert_eq!(
                harness.server.state.window_waiters.waiters.len(),
                1,
                "an unmapped window is not gone"
            );
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["closed"], "killed");
    assert_eq!(body["window"], "unmapped");
}

/// The deadline runs from admission, not from when the protocol thread
/// took the request.
#[test]
fn force_close_deadline_counts_from_admission() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let admission = ingress
        .request_long(LongOp::ForceClose {
            id,
            generation,
            timeout: Duration::from_millis(600),
        })
        .expect("admitted");
    // Sit in the queue for most of the budget before the dequeue.
    std::thread::sleep(Duration::from_millis(500));
    let dequeued = Instant::now();
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("registration cycle");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !harness.server.state.window_waiters.waiters.is_empty() {
        harness
            .server
            .dispatch_cycle(Some(Duration::from_millis(5)))
            .expect("cycle");
        assert!(Instant::now() < deadline, "the waiter never expired");
    }
    assert!(
        dequeued.elapsed() < Duration::from_millis(450),
        "the timer was armed for what was left of the budget: {:?}",
        dequeued.elapsed()
    );
    let body = runtime
        .block_on(admission.receive())
        .expect("reply")
        .wire_json();
    assert_eq!(body["closed"], "killed", "{body}");
    assert!(body["waited_ms"].as_u64().unwrap() >= 600);
}

/// A caller that stopped waiting gets no kill on its behalf.
#[test]
fn force_close_does_nothing_for_a_caller_that_left() {
    let (mut harness, ingress, _observations, _runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let admission = ingress
        .request_long(LongOp::ForceClose {
            id,
            generation,
            timeout: Duration::from_millis(30),
        })
        .expect("admitted");
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("registration cycle");
    assert_eq!(harness.server.state.window_waiters.waiters.len(), 1);
    drop(admission);
    std::thread::sleep(Duration::from_millis(40));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !harness.server.state.window_waiters.waiters.is_empty() {
        harness
            .server
            .dispatch_cycle(Some(Duration::from_millis(5)))
            .expect("cycle");
        assert!(Instant::now() < deadline, "the waiter was never dropped");
    }
    harness.dispatch_client();
    harness.assert_client_connected("nobody was waiting, so nothing was killed");
    assert!(harness.server.state.surfaces[&alpha].mapped);
}

/// `presented` means a frame of the current mapping: a window presented
/// before it unmapped does not satisfy it again after the remap.
#[test]
fn presented_waits_for_a_frame_of_the_current_mapping() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let surface_id = harness.server.state.surfaces[&alpha].id;
    let (presentation, _) = bind_test_presentation(&mut harness);
    let _feedback = request_presentation_feedback(&mut harness, presentation);
    commit_test_buffer(&mut harness, TEST_TOPLEVEL_SURFACE_ID);
    harness.dispatch_client();
    let (frame, content) =
        test_frame_report(surface_id, monotonic_micros(), content_seq(&harness, &alpha), true);
    harness.server.state.frame_presented(frame, content);
    let presented = wait_for(by_id(id, generation), WaitUntil::Presented, 30);
    let (rc, body) = long_window_op(&mut harness, &ingress, &runtime, presented.clone(), |_| {});
    assert_eq!(rc, 0, "presented before the remap: {body}");

    let before_remap = monotonic_micros();
    unmap_alpha(&mut harness);
    let mut traffic = harness.sync();
    send_request(&mut harness.client, TEST_TOPLEVEL_SURFACE_ID, 6, &[]);
    traffic.extend(harness.sync());
    ack_and_map_test_toplevel(&mut harness, configured_toplevel_serial(&traffic));
    assert!(harness.server.state.surfaces[&alpha].mapped);
    assert_eq!(
        window_id_and_generation(&harness, &alpha),
        (id, generation),
        "a remap keeps the generation"
    );
    let (rc, body) = long_window_op(&mut harness, &ingress, &runtime, presented, |_| {});
    assert_eq!(rc, 10, "the old frame does not count: {body}");
    assert_eq!(body["error"], "timeout");

    // A late report of a frame shown before this mapping raises the count
    // but is older than the map: still not presented.
    let _feedback = request_presentation_feedback(&mut harness, presentation);
    commit_test_buffer(&mut harness, TEST_TOPLEVEL_SURFACE_ID);
    harness.dispatch_client();
    let (frame, content) =
        test_frame_report(surface_id, before_remap, content_seq(&harness, &alpha), true);
    harness.server.state.frame_presented(frame, content);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(id, generation), WaitUntil::Presented, 30),
        |_| {},
    );
    assert_eq!(rc, 10, "a frame older than the map does not count: {body}");

    let _feedback = request_presentation_feedback(&mut harness, presentation);
    commit_test_buffer(&mut harness, TEST_TOPLEVEL_SURFACE_ID);
    harness.dispatch_client();
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(id, generation), WaitUntil::Presented, 5_000),
        |harness| {
            let (frame, content) =
                test_frame_report(surface_id, monotonic_micros(), content_seq(harness, &alpha), true);
            harness.server.state.frame_presented(frame, content);
        },
    );
    assert_eq!(rc, 0, "a frame of this mapping: {body}");
}

/// Waits learn nothing under a session lock (a named id's `gone` still
/// resolves), a kill never lands under it, and a never-issued id is
/// refused.
#[test]
fn wait_respects_the_lock_and_refuses_unissued_ids() {
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(999_999, 1), WaitUntil::Gone, 5_000),
        |_| {},
    );
    assert_eq!((rc, body["error"].clone()), (10, json!("unknown_window")), "{body}");

    // The lock arrives while a forced close waits: the deadline refuses.
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        LongOp::ForceClose {
            id: beta_id,
            generation: beta_generation,
            timeout: Duration::from_millis(40),
        },
        |harness| {
            let lock = begin_test_session_lock(harness);
            ack_and_map_test_lock_surface(harness, lock);
        },
    );
    assert_eq!((rc, body["error"].clone()), (10, json!("locked")), "{body}");
    harness.assert_client_connected("no kill under the lock");

    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(
            WindowMatch {
                app_id: Some("dev.cosmix.Beta".into()),
                ..WindowMatch::default()
            },
            WaitUntil::Mapped,
            40,
        ),
        |_| {},
    );
    assert_eq!(rc, 10, "a mapped window is hidden while locked: {body}");
    assert_eq!(body["error"], "timeout");
    let (rc, _) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(beta_id, beta_generation), WaitUntil::Visible, 40),
        |_| {},
    );
    assert_eq!(rc, 10);
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(by_id(alpha_id, alpha_generation), WaitUntil::Gone, 5_000),
        |harness| {
            send_request(&mut harness.client, TEST_TOPLEVEL_ID, 0, &[]);
            harness.dispatch_client();
        },
    );
    assert_eq!(rc, 0, "gone by id still resolves under the lock: {body}");
}

/// A same-cycle unmap and remap under a new role emits both edges.
#[test]
fn role_replacement_in_one_cycle_emits_unmap_then_map() {
    let (mut harness, _ingress, observations, _runtime, alpha, _beta) = two_mapped_windows();
    port_observation::service_observations(&mut harness.server.state);
    drain_observations(&observations);
    let (id, old_generation) = window_id_and_generation(&harness, &alpha);
    let surface = harness.server.state.surfaces[&alpha].role.wl_surface().clone();
    harness.server.state.mark_surface_unmapped(&surface);
    harness
        .server
        .state
        .surfaces
        .get_mut(&alpha)
        .expect("record")
        .generation += 7;
    port_observation::service_observations(&mut harness.server.state);
    let edges = drain_observations(&observations)
        .into_iter()
        .filter_map(|record| match record {
            port_observation::ObservationRecord::SurfaceMapped { id: edge, window, .. } => {
                Some(("mapped", edge, window.generation))
            }
            port_observation::ObservationRecord::SurfaceUnmapped { id: edge, window, .. } => {
                Some(("unmapped", edge, window.generation))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        edges,
        [
            ("unmapped", id, old_generation),
            ("mapped", id, old_generation + 7)
        ]
    );
}

/// An absent size axis is the committed geometry, and placing a window
/// at its own size needs no configure and still satisfies a size wait.
#[test]
fn place_keeps_the_real_size_and_refuses_off_output() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let current = geometry_size_of(&harness, &alpha);
    harness
        .server
        .state
        .surfaces
        .get_mut(&alpha)
        .expect("record")
        .configured_size = (current.0 + 50, current.1 + 50);
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            height: Some(current.1),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["requested"], json!({"width": current.0, "height": current.1}));
    let (rc, body) = long_window_op(
        &mut harness,
        &ingress,
        &runtime,
        wait_for(
            by_id(id, generation),
            WaitUntil::Size {
                width: current.0,
                height: current.1,
            },
            5_000,
        ),
        |_| {},
    );
    assert_eq!(rc, 0, "{body}");

    let origin = harness.server.state.surfaces[&alpha].window_origin;
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Place(PlaceSpec {
            x: Some(100_000.0),
            ..place(id, generation)
        }),
    );
    assert_eq!(rc, 10);
    assert_eq!(body["error"], "off_output", "{body}");
    assert_eq!(harness.server.state.surfaces[&alpha].window_origin, origin);
}

/// Every refusal reaches the wire with `error_code` beside `error`.
#[test]
fn refusals_carry_error_code() {
    let (mut harness, ingress, _observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    let admission = ingress
        .request_window(WindowOp::Raise {
            id,
            generation: generation + 1,
        })
        .expect("admitted");
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("cycle");
    let body = runtime
        .block_on(admission.receive())
        .expect("reply")
        .wire_json();
    assert_eq!(body["error"], "stale_target");
    assert_eq!(body["error_code"], "stale_target");
    assert_eq!(body["current"], generation);
    let admission = ingress
        .request_window(WindowOp::SwitchWorkspace {
            output: None,
            index: WorkspaceIndex::Prev,
            wrap: false,
        })
        .expect("admitted");
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("cycle");
    let body = runtime
        .block_on(admission.receive())
        .expect("reply")
        .wire_json();
    assert_eq!(body["error"], "at_end");
    assert_eq!(body["error_code"], "at_end");
    assert_eq!(body["from"], 1);
    assert_eq!(body["count"], 4);
    let (rc, wire) = crate::port::with_error_code(10, Arc::from(r#"{"error":"busy"}"#));
    assert_eq!((rc, &*wire), (10, r#"{"error":"busy","error_code":"busy"}"#));
    let (_, untouched) = crate::port::with_error_code(0, Arc::from(r#"{"error":"x"}"#));
    assert_eq!(&*untouched, r#"{"error":"x"}"#);
}

/// Rule 6 (F1.2): `comp.window.focus` on a window that lives on another
/// workspace switches to that workspace and focuses it — never pulls the
/// window across — on both the raise and the focus-only path, and the
/// reason ladder sees it as on-current (no `reason` key).
#[test]
fn focus_on_an_off_workspace_window_switches_and_focuses() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    for raise in [true, false] {
        assert_eq!(harness.server.state.workspace_current(), 1);
        assert!(!harness.server.state.surfaces[&alpha].layout.visible);
        assert!(harness.server.state.surfaces[&beta].layout.visible);
        let (rc, body) = window_op(
            &mut harness,
            &ingress,
            &runtime,
            WindowOp::Focus {
                id,
                generation,
                raise,
            },
        );
        assert_eq!(rc, 0, "raise {raise}: {body}");
        assert_eq!(body["focused"], true, "raise {raise}: {body}");
        assert!(body.get("reason").is_none(), "raise {raise}: {body}");
        assert_eq!(harness.server.state.workspace_current(), 2);
        let state = &harness.server.state;
        assert_eq!(state.surfaces[&alpha].workspace, 2, "never pulled across");
        assert!(state.surfaces[&alpha].layout.visible);
        assert!(state.surfaces[&alpha].focused);
        assert!(!state.surfaces[&beta].layout.visible);
        assert_eq!(
            focused_surface(state.keyboard.current_focus()).map(|surface| surface.id()),
            Some(alpha.clone())
        );
        // Back to 1 for the focus-only pass.
        harness
            .server
            .state
            .switch_workspace(None, WorkspaceTarget::Index(1), true)
            .expect("back to 1");
        let _ = harness.sync();
    }
}

/// Rule 6's refusal half: a focus the reason ladder refuses must not change
/// the desktop. Alpha minimised on workspace 2, the user on 1: `focus` on
/// either path replies `reason: "minimized"`, and the current workspace,
/// beta's visibility and the keyboard focus are exactly as they were — the
/// switch is gated on the same terms as the ladder, so nothing switches
/// and then refuses.
#[test]
fn focus_on_a_minimised_off_workspace_window_is_refused_without_switching() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&surface);
    assert!(harness.server.state.surfaces[&alpha].minimized);
    assert_eq!(harness.server.state.workspace_current(), 1);
    for raise in [true, false] {
        let (rc, body) = window_op(
            &mut harness,
            &ingress,
            &runtime,
            WindowOp::Focus {
                id,
                generation,
                raise,
            },
        );
        assert_eq!(rc, 0, "raise {raise}: {body}");
        assert_eq!(body["focused"], false, "raise {raise}: {body}");
        assert_eq!(body["reason"], "minimized", "raise {raise}: {body}");
        let state = &harness.server.state;
        assert_eq!(
            state.workspace_current(),
            1,
            "raise {raise}: a refused focus does not switch"
        );
        assert!(state.surfaces[&alpha].minimized);
        assert_eq!(state.surfaces[&alpha].workspace, 2, "never pulled across");
        assert!(!state.surfaces[&alpha].layout.visible);
        assert!(!state.surfaces[&alpha].focused);
        assert!(state.surfaces[&beta].layout.visible);
        assert_eq!(
            focused_surface(state.keyboard.current_focus()).map(|surface| surface.id()),
            Some(beta.clone())
        );
        assert_ne!(state.full_dirty_cause(), Some("workspace.switch"));
    }
}

/// `comp.window.raise` is stacking only, as the manual says: an
/// off-workspace window is restacked in place without a switch, and a
/// minimised one (on either workspace) stays minimised. `focus` and
/// `restore` are the bring-into-view verbs; this pins that raise is not.
#[test]
fn raise_on_an_off_workspace_or_minimised_window_never_switches_or_unminimises() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    assert_eq!(harness.server.state.workspace_current(), 1);
    assert!(harness.server.state.surfaces[&beta].focused);

    let raise = |harness: &mut KeybindingHarness, id: u64, generation: u64| {
        let (rc, body) = window_op(
            harness,
            &ingress,
            &runtime,
            WindowOp::Raise { id, generation },
        );
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["id"], id, "{body}");
        assert_eq!(body["generation"], generation, "{body}");
        assert!(body["raised"].is_boolean(), "{body}");
        assert!(body.get("reason").is_none(), "raise has no reason ladder: {body}");
    };
    let unchanged = |harness: &KeybindingHarness, what: &str| {
        let state = &harness.server.state;
        assert_eq!(state.workspace_current(), 1, "{what}: raise never switches");
        assert_eq!(state.surfaces[&alpha].workspace, 2, "{what}: never pulled across");
        assert!(!state.surfaces[&alpha].layout.visible, "{what}");
        assert!(!state.surfaces[&alpha].focused, "{what}");
        assert_ne!(state.full_dirty_cause(), Some("workspace.switch"), "{what}");
    };

    // Off-workspace, not minimised: restacked in place, nothing comes on screen.
    raise(&mut harness, alpha_id, alpha_generation);
    unchanged(&harness, "off-workspace");
    {
        let state = &harness.server.state;
        assert!(state.surfaces[&beta].layout.visible);
        assert!(state.surfaces[&beta].focused, "focus stays where it was");
    }

    // Off-workspace AND minimised: still no switch, still minimised.
    let alpha_surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&alpha_surface);
    assert!(harness.server.state.surfaces[&alpha].minimized);
    raise(&mut harness, alpha_id, alpha_generation);
    unchanged(&harness, "off-workspace minimised");
    assert!(
        harness.server.state.surfaces[&alpha].minimized,
        "raise never un-minimises"
    );

    // Minimised on the current workspace: raise leaves it minimised and
    // unfocused; only restore/focus bring it back.
    let beta_surface = harness.server.state.surfaces[&beta]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&beta_surface);
    {
        let state = &harness.server.state;
        assert!(state.surfaces[&beta].minimized);
        assert!(!state.surfaces[&beta].layout.visible);
        assert!(!state.surfaces[&beta].focused);
    }
    raise(&mut harness, beta_id, beta_generation);
    {
        let state = &harness.server.state;
        assert!(state.surfaces[&beta].minimized, "raise never un-minimises");
        assert!(!state.surfaces[&beta].layout.visible);
        assert!(!state.surfaces[&beta].focused);
        assert_eq!(state.workspace_current(), 1);
    }
    // The discriminating half: restore is the verb that does bring it back.
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Restore {
            target: Some((beta_id, beta_generation)),
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["changed"], true, "{body}");
    {
        let state = &harness.server.state;
        assert!(!state.surfaces[&beta].minimized);
        assert!(state.surfaces[&beta].layout.visible);
        assert!(state.surfaces[&beta].focused);
    }
}

/// The other rule-6 refusal rungs, after the switch-first: an
/// off-workspace target under an exclusive layer replies `exclusive_layer`,
/// and one behind the KMS input gate (`normal_scene_restricted`: the VT is
/// switched away) replies `not_presentable` — not `not_visible`, which is
/// only what the withheld switch left it as — and neither changes the
/// workspace. The KMS arm is base-discriminating: lifting the gate makes
/// the same focus switch and succeed. The exclusive-layer arm is a guard
/// (the layer's refusal predates the workspaces work); what it pins here
/// is that the reason names the layer and the workspace is unchanged.
#[test]
fn focus_on_an_off_workspace_window_names_the_gate_that_held_the_switch() {
    use crate::protocol::workspaces::WorkspaceTarget;
    // Exclusive layer.
    {
        let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
        let (id, generation) = window_id_and_generation(&harness, &alpha);
        assert_eq!(
            harness
                .server
                .state
                .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
            Ok((1, 2))
        );
        const TOP_LEFT: u32 = 1 | 4;
        let _ = map_test_layer_surface(
            &mut harness,
            0,
            TestLayerSpec {
                anchor: TOP_LEFT,
                keyboard_interactivity: zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive
                    as u32,
                ..TestLayerSpec::default()
            },
        );
        let _ = harness.sync();
        assert!(harness.server.state.highest_exclusive_layer().is_some());
        for raise in [true, false] {
            let (rc, body) = window_op(
                &mut harness,
                &ingress,
                &runtime,
                WindowOp::Focus {
                    id,
                    generation,
                    raise,
                },
            );
            assert_eq!(rc, 0, "raise {raise}: {body}");
            assert_eq!(body["focused"], false, "raise {raise}: {body}");
            assert_eq!(body["reason"], "exclusive_layer", "raise {raise}: {body}");
            let state = &harness.server.state;
            assert_eq!(state.workspace_current(), 1, "raise {raise}: no switch");
            assert_eq!(state.surfaces[&alpha].workspace, 2, "never pulled across");
            assert!(!state.surfaces[&alpha].layout.visible);
            assert!(!state.surfaces[&alpha].focused);
            assert!(state.surfaces[&beta].layout.visible);
            assert_ne!(state.full_dirty_cause(), Some("workspace.switch"));
        }
    }
    // The KMS input gate, then lifted.
    {
        let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
        let (id, generation) = window_id_and_generation(&harness, &alpha);
        assert_eq!(
            harness
                .server
                .state
                .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
            Ok((1, 2))
        );
        harness.server.state.kms_session_lock_gate.deferred_unlock = true;
        {
            let state = &harness.server.state;
            assert!(state.kms_session_lock_gate.normal_scene_restricted());
            assert!(!state.surface_is_input_presentable(&state.surfaces[&alpha]));
        }
        for raise in [true, false] {
            let (rc, body) = window_op(
                &mut harness,
                &ingress,
                &runtime,
                WindowOp::Focus {
                    id,
                    generation,
                    raise,
                },
            );
            assert_eq!(rc, 0, "raise {raise}: {body}");
            assert_eq!(body["focused"], false, "raise {raise}: {body}");
            assert_eq!(body["reason"], "not_presentable", "raise {raise}: {body}");
            let state = &harness.server.state;
            assert_eq!(state.workspace_current(), 1, "raise {raise}: no switch");
            assert_eq!(state.surfaces[&alpha].workspace, 2, "never pulled across");
            assert!(!state.surfaces[&alpha].layout.visible);
            assert!(!state.surfaces[&alpha].focused);
            assert!(state.surfaces[&beta].layout.visible);
            assert_ne!(state.full_dirty_cause(), Some("workspace.switch"));
        }
        harness.server.state.kms_session_lock_gate.deferred_unlock = false;
        let (rc, body) = window_op(
            &mut harness,
            &ingress,
            &runtime,
            WindowOp::Focus {
                id,
                generation,
                raise: true,
            },
        );
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["focused"], true, "gate lifted: {body}");
        assert!(body.get("reason").is_none(), "{body}");
        let state = &harness.server.state;
        assert_eq!(state.workspace_current(), 2, "gate lifted: switched");
        assert!(state.surfaces[&alpha].layout.visible);
        assert!(state.surfaces[&alpha].focused);
        assert!(!state.surfaces[&beta].layout.visible);
    }
}

/// What a watcher sees of a cross-workspace focus. The switch plants a
/// full-snapshot cause (D7: every row's visibility may change), and
/// `service_observations` attributes the WHOLE diff to that cause and
/// discards the per-surface marks — so the target's `focused` edge reports
/// `workspace.switch`, not `comp.window`, whichever is marked first. The
/// verb's mark-before-switch order matches its siblings; it cannot change
/// this, and this test says so rather than letting the order look
/// load-bearing.
#[test]
fn focus_on_an_off_workspace_window_reports_every_change_as_the_switch() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let (mut harness, ingress, observations, runtime, alpha, _beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    let watch = ingress.request_watch().expect("watch admitted");
    serviced_watch(&mut harness, &runtime, watch);
    port_observation::service_observations(&mut harness.server.state);
    drain_observations(&observations);

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Focus {
            id,
            generation,
            raise: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["focused"], true, "{body}");
    assert_eq!(harness.server.state.workspace_current(), 2);
    port_observation::service_observations(&mut harness.server.state);
    let changed = drain_observations(&observations);
    let focused_path = format!("windows.s{id}.focused");
    let props = changed
        .iter()
        .filter_map(|record| match record {
            port_observation::ObservationRecord::PropsChanged { path, cause, .. } => {
                Some((path.as_str(), *cause))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        props.contains(&(focused_path.as_str(), "workspace.switch")),
        "the target's focused edge is attributed to the switch: {props:?}"
    );
    assert!(
        props.iter().all(|(_, cause)| *cause == "workspace.switch"),
        "a full-snapshot diff carries one cause: {props:?}"
    );
}

/// A focus refused `not_visible` past the `may_focus` terms attributes
/// nothing: the verb plants `comp.window` first (so its cause beats the
/// core's), and takes the mark back when it refuses, so the next unrelated
/// edge on that surface keeps its own cause. The first half proves the
/// marks are live (a focus that succeeds leaves `comp.window` planted); the
/// second fakes the one state the ladder answers `not_visible` for — on the
/// current workspace (nothing for the switch to do) yet not visible — which
/// no recompute would leave a mapped, non-minimised toplevel in, so it is
/// set by hand; the verb runs its ladder without recomputing.
#[test]
fn focus_refused_not_visible_attributes_nothing() {
    let (mut harness, ingress, observations, runtime, alpha, beta) = two_mapped_windows();
    let (alpha_id, alpha_generation) = window_id_and_generation(&harness, &alpha);
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    let watch = ingress.request_watch().expect("watch admitted");
    serviced_watch(&mut harness, &runtime, watch);
    port_observation::service_observations(&mut harness.server.state);
    drain_observations(&observations);

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Focus {
            id: beta_id,
            generation: beta_generation,
            raise: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["focused"], true, "{body}");
    assert_eq!(
        harness.server.state.surface_dirty_cause(beta_id),
        Some("comp.window"),
        "a focus that succeeds leaves its mark"
    );

    harness
        .server
        .state
        .surfaces
        .get_mut(&alpha)
        .expect("alpha exists")
        .layout
        .visible = false;
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Focus {
            id: alpha_id,
            generation: alpha_generation,
            raise: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["focused"], false, "{body}");
    assert_eq!(body["reason"], "not_visible", "{body}");
    let state = &harness.server.state;
    assert_eq!(
        state.surface_dirty_cause(alpha_id),
        None,
        "a refused focus plants nothing"
    );
    assert_ne!(state.full_dirty_cause(), Some("workspace.switch"));
    assert!(!state.surfaces[&alpha].focused);
}

/// `send_to_workspace {follow:true}` moves and switches in ONE settle
/// (`move_window_and_follow`): a window already on the target workspace,
/// highest there, never gains the keyboard while the sent window arrives —
/// no `wl_keyboard.enter` names it — and the sent window lands on top of
/// it. (A move then a switch settled the target once with the sent window
/// still hidden, and that bystander held focus for one round-trip.)
#[test]
fn send_to_workspace_follow_never_focuses_the_targets_bystander() {
    let (mut harness, ingress, _observations, runtime, _alpha, beta) = two_mapped_windows();
    let (beta_id, beta_generation) = window_id_and_generation(&harness, &beta);
    harness
        .server
        .state
        .switch_workspace(None, WorkspaceTarget::Index(3), true)
        .expect("switch to 3");
    let (gamma_id, _, _, gamma) =
        map_named_test_toplevel(&mut harness, "Gamma", "dev.cosmix.Gamma");
    assert_eq!(harness.server.state.surfaces[&gamma].workspace, 3);
    harness
        .server
        .state
        .switch_workspace(None, WorkspaceTarget::Index(1), true)
        .expect("back to 1");
    let beta_surface = harness.server.state.surfaces[&beta]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&beta_surface);
    let _ = harness.sync();
    assert!(harness.server.state.surfaces[&beta].focused);
    assert!(!harness.server.state.surfaces[&gamma].layout.visible);

    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::SendToWorkspace {
            id: beta_id,
            generation: beta_generation,
            index: WorkspaceIndex::Absolute(3),
            follow: true,
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(
        body,
        json!({"id": beta_id, "generation": beta_generation, "index": 3, "followed": true})
    );
    {
        let state = &harness.server.state;
        assert_eq!(state.workspace_current(), 3);
        assert_eq!(state.surfaces[&beta].workspace, 3);
        assert!(state.surfaces[&beta].layout.visible);
        assert!(state.surfaces[&beta].focused, "follow activates the sent window");
        assert!(state.surfaces[&gamma].layout.visible);
        assert!(!state.surfaces[&gamma].focused);
        assert!(
            surface_stack_cmp(&state.surfaces[&beta], &state.surfaces[&gamma]).is_gt(),
            "the sent window arrives on top of the target's bystander"
        );
    }
    let entered = keyboard_enter_surfaces(&harness.sync());
    assert!(
        !entered.contains(&gamma_id),
        "the target's bystander never gains keyboard focus: {entered:?}"
    );
}

/// F1.2 at `comp.window.restore {id, generation}`: restoring a window that
/// was minimised on another workspace switches to that workspace and shows
/// it there.
#[test]
fn restore_by_id_switches_to_the_windows_workspace() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let (mut harness, ingress, _observations, runtime, alpha, beta) = two_mapped_windows();
    let (id, generation) = window_id_and_generation(&harness, &alpha);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&alpha, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    let surface = harness.server.state.surfaces[&alpha]
        .role
        .wl_surface()
        .clone();
    harness.server.state.minimize_toplevel(&surface);
    assert!(harness.server.state.surfaces[&alpha].minimized);
    assert_eq!(harness.server.state.workspace_current(), 1);
    let (rc, body) = window_op(
        &mut harness,
        &ingress,
        &runtime,
        WindowOp::Restore {
            target: Some((id, generation)),
        },
    );
    assert_eq!(rc, 0, "{body}");
    assert_eq!(body["minimized"], false);
    assert_eq!(body["changed"], true);
    assert_eq!(harness.server.state.workspace_current(), 2);
    let state = &harness.server.state;
    assert!(!state.surfaces[&alpha].minimized);
    assert_eq!(state.surfaces[&alpha].workspace, 2, "never pulled across");
    assert!(state.surfaces[&alpha].layout.visible);
    assert!(state.surfaces[&alpha].focused);
    assert!(!state.surfaces[&beta].layout.visible);
    assert!(state.minimized_toplevels.is_empty());
}
