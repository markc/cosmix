use super::*;

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
