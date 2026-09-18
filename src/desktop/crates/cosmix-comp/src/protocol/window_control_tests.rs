// `comp.window.*` and `comp.windows.list` (included from tests.rs).

use crate::port::{LongOp, PlaceSpec, WaitSpec, WaitUntil, WindowMatch, WindowOp};

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

/// The other rule-6 refusal rungs, after the switch-first: an
/// off-workspace target under an exclusive layer replies `exclusive_layer`,
/// and one behind the KMS input gate (`normal_scene_restricted`: the VT is
/// switched away) replies `not_presentable` — not `not_visible`, which is
/// only what the withheld switch left it as — and neither changes the
/// workspace. Each arm is base-discriminating: lifting the gate makes the
/// same focus switch and succeed.
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
