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
