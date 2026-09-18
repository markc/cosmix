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
    assert_eq!(
        harness.server.state.full_dirty_cause(),
        Some("workspace.switch"),
        "a switch re-diffs the whole tree (D7)"
    );
    port_observation::service_observations(&mut harness.server.state);
    let changed = drain_observations(&observations);
    let alpha_visible = format!("windows.s{alpha_id}.visible");
    assert!(
        changed.iter().any(|record| matches!(
            record,
            port_observation::ObservationRecord::PropsChanged { path, new, .. }
                if *path == alpha_visible && new.wire_value() == json!(false)
        )),
        "the switch reaches props.changed: {changed:?}"
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
