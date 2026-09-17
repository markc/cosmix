use super::*;

#[test]
fn active_window_tracks_managed_transfer_and_native_or_none_clears() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let native = test_toplevel_record(&harness).role.wl_surface().clone();
    let (id_a, surface_a, window_a, _) = associate_normal_window(&mut harness, 902);
    commit_dmabuf(&mut harness, id_a, 32, 24);
    let (id_b, surface_b, window_b, _) = associate_normal_window(&mut harness, 903);
    commit_dmabuf(&mut harness, id_b, 32, 24);
    for (surface, expected) in [
        (&surface_a, Some(window_a)),
        (&surface_b, Some(window_b)),
        (&native, None),
    ] {
        harness.server.state.activate_managed_window(surface);
        let focused = focused_surface(harness.server.state.keyboard.current_focus());
        assert_eq!(
            harness
                .server
                .state
                .x11_active_window_for_root(focused.as_ref()),
            expected
        );
    }
    harness
        .server
        .state
        .arbitrate_keyboard_focus(None, false, true);
    let focused = focused_surface(harness.server.state.keyboard.current_focus());
    assert!(focused.is_none());
    assert!(
        harness
            .server
            .state
            .x11_active_window_for_root(focused.as_ref())
            .is_none()
    );
}

#[test]
fn active_window_rejects_old_generation_override_redirect_and_shutdown() {
    let mut harness = KeybindingHarness::new(true);
    let (id, surface, _, object) = associate_normal_window(&mut harness, 904);
    commit_dmabuf(&mut harness, id, 32, 24);
    assert!(
        harness
            .server
            .state
            .x11_active_window_for_root(Some(&surface))
            .is_some()
    );
    harness.server.state.xwayland.generation += 1;
    assert!(
        harness
            .server
            .state
            .x11_active_window_for_root(Some(&surface))
            .is_none()
    );
    harness.server.state.xwayland.generation -= 1;
    if let SurfaceRole::X11(role) =
        &mut harness.server.state.surfaces.get_mut(&object).unwrap().role
    {
        role.override_redirect = true;
    }
    assert!(
        harness
            .server
            .state
            .x11_active_window_for_root(Some(&surface))
            .is_none()
    );
    if let SurfaceRole::X11(role) =
        &mut harness.server.state.surfaces.get_mut(&object).unwrap().role
    {
        role.override_redirect = false;
    }
    harness.server.state.xwayland.shutting_down = true;
    assert!(
        harness
            .server
            .state
            .x11_active_window_for_root(Some(&surface))
            .is_none()
    );
}

#[test]
fn x11_activation_and_cycle_use_x11_focus_and_reject_unmapped_target() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let native = test_toplevel_record(&harness).role.wl_surface().clone();
    let (id, surface, window, object) = associate_normal_window(&mut harness, 901);
    commit_dmabuf(&mut harness, id, 32, 24);
    harness.server.state.activate_managed_window(&native);
    harness.server.state.x11_activate_request(window.clone());
    assert!(matches!(
        harness.server.state.keyboard.current_focus(),
        Some(SeatFocusTarget::X11(_))
    ));
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(surface)
    );
    harness.server.state.cycle_window(false);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(native.clone())
    );
    harness
        .server
        .state
        .surfaces
        .get_mut(&object)
        .unwrap()
        .minimized = true;
    harness.server.state.x11_activate_request(window.clone());
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(native.clone())
    );
    harness
        .server
        .state
        .surfaces
        .get_mut(&object)
        .unwrap()
        .minimized = false;
    if let SurfaceRole::X11(role) =
        &mut harness.server.state.surfaces.get_mut(&object).unwrap().role
    {
        role.override_redirect = true;
    }
    harness.server.state.x11_activate_request(window.clone());
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(native.clone())
    );
    if let SurfaceRole::X11(role) =
        &mut harness.server.state.surfaces.get_mut(&object).unwrap().role
    {
        role.override_redirect = false;
    }
    harness
        .server
        .state
        .surfaces
        .get_mut(&object)
        .unwrap()
        .mapped = false;
    harness.server.state.x11_activate_request(window);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(native)
    );
}

/// The minimised prop drives X11 windows through the same funnel: EWMH
/// hidden state is set on minimise and cleared on restore.
#[cfg(feature = "bus")]
#[test]
fn minimized_prop_suspends_and_resumes_x11_windows() {
    let (mut harness, ingress, _observations) = KeybindingHarness::new_with_port();
    let (surface_id, _, window, object) = associate_normal_window(&mut harness, 905);
    commit_dmabuf(&mut harness, surface_id, 32, 24);
    let record = &harness.server.state.surfaces[&object];
    assert!(record.mapped && record.role.managed_toplevel());
    let path = format!("windows.s{}.minimized", record.id.0);
    let generation = record.generation;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("control reply runtime");
    for minimized in [true, false] {
        let admission = ingress
            .request_set_fenced(path.clone(), json!(minimized), Some(generation))
            .expect("set admitted");
        harness
            .server
            .dispatch_cycle(Some(Duration::ZERO))
            .expect("set service cycle");
        let (rc, body) = runtime
            .block_on(admission.receive())
            .expect("set reply")
            .into_wire();
        assert_eq!(rc, 0, "{body}");
        assert_eq!(harness.server.state.surfaces[&object].minimized, minimized);
        assert_eq!(window.is_minimized(), minimized, "EWMH hidden state");
    }
    assert!(harness.server.state.minimized_toplevels.is_empty());
}

/// X11 windows have no `windows.*` row, so their fence comes from
/// `surfaces.s<id>.generation`; the id form of the window verbs then works
/// on them, and `focus.window` names a focused X11 window.
#[cfg(feature = "bus")]
#[test]
fn x11_generation_is_readable_and_fences_window_verbs() {
    let (mut harness, ingress, _observations) = KeybindingHarness::new_with_port();
    let (surface_id, _, window, object) = associate_normal_window(&mut harness, 906);
    commit_dmabuf(&mut harness, surface_id, 32, 24);
    let context = harness
        .server
        .state
        .port_context
        .clone()
        .expect("port context");
    let id = harness.server.state.surfaces[&object].id.0;
    let key = format!("s{id}");
    let snapshot = port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot");
    let row = &snapshot.surfaces[&key];
    assert_eq!(row.role, "x11-toplevel");
    assert!(!snapshot.windows.contains_key(&key), "no X11 window rows yet");
    let generation = row.generation;
    assert_eq!(generation, harness.server.state.surfaces[&object].generation);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("control reply runtime");
    for op in [
        crate::port::WindowOp::Minimize { id, generation },
        crate::port::WindowOp::Restore {
            target: Some((id, generation)),
        },
    ] {
        let admission = ingress.request_window(op).expect("verb admitted");
        harness
            .server
            .dispatch_cycle(Some(Duration::ZERO))
            .expect("verb service cycle");
        let (rc, body) = runtime
            .block_on(admission.receive())
            .expect("verb reply")
            .into_wire();
        assert_eq!(rc, 0, "{op:?}: {body}");
        let minimized = matches!(op, crate::port::WindowOp::Minimize { .. });
        assert_eq!(harness.server.state.surfaces[&object].minimized, minimized);
        assert_eq!(window.is_minimized(), minimized);
    }

    // The restore focused it: focus.window names the X11 window.
    let snapshot = port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot");
    assert!(harness.server.state.surfaces[&object].focused);
    assert_eq!(snapshot.focus.window.id, Some(id));
    assert_eq!(snapshot.focus.window.generation, Some(generation));
}
