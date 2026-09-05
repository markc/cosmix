use super::*;

/// The privileged XWM handshake is unavailable in the offline harness.
/// Pin the vendor dispatch as a source-presence guard, not a live wire test.
#[test]
fn vendored_active_window_dispatch_keeps_format_and_policy_callback() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/smithay/src/xwayland/xwm/mod.rs"),
    )
    .unwrap();
    let branch = source
        .split("x if x == xwm.atoms._NET_ACTIVE_WINDOW && msg.format == 32 => {")
        .nth(1)
        .expect("32-bit active-window dispatch");
    let branch = branch
        .split("x if x == xwm.atoms.WL_SURFACE_ID")
        .next()
        .unwrap();
    assert!(branch.contains("surface.window_id() == msg.window"));
    assert!(branch.contains("state.activate_request(xwm_id, surface, data[0], data[1]);"));
}

#[test]
fn alt_tab_cycles_three_windows_and_reverse_uses_real_xkb() {
    for profile in [BindingProfile::Nested, BindingProfile::KmsLive] {
        let mut harness = KeybindingHarness::new(true);
        harness.server.state.bindings = BindingState::for_profile(profile, true);
        map_initial_test_toplevel(&mut harness);
        let first = test_toplevel_record(&harness).role.wl_surface().clone();
        let second_id = map_test_undecorated_toplevel(&mut harness);
        let third_id = map_test_undecorated_toplevel(&mut harness);
        let second = harness.server.state.surfaces[&second_id]
            .role
            .wl_surface()
            .clone();
        let third = harness.server.state.surfaces[&third_id]
            .role
            .wl_surface()
            .clone();
        harness.server.state.activate_managed_window(&first);
        harness.key(56, HostButtonState::Pressed); // Alt
        for expected in [&second, &third, &first, &second] {
            harness.key(15, HostButtonState::Pressed); // Tab
            harness.key(15, HostButtonState::Released);
            assert_eq!(
                focused_surface(harness.server.state.keyboard.current_focus()).as_ref(),
                Some(expected)
            );
            assert_eq!(
                harness
                    .server
                    .state
                    .highest_visible_toplevel_surface()
                    .as_ref(),
                Some(expected)
            );
        }
        harness.key(42, HostButtonState::Pressed); // Shift
        harness.key(15, HostButtonState::Pressed);
        assert_eq!(
            focused_surface(harness.server.state.keyboard.current_focus()),
            Some(first)
        );
        // Modifiers may be released before the swallowed Tab release.
        harness.key(56, HostButtonState::Released);
        harness.key(42, HostButtonState::Released);
        harness.key(15, HostButtonState::Released);
        let events = harness.sync();
        assert!(
            !keyboard_key_events(&events)
                .iter()
                .any(|(key, _)| *key == 15)
        );
    }
}

#[test]
fn switching_skips_minimised_and_unmapped_windows() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let first = test_toplevel_record(&harness).role.wl_surface().clone();
    let second = map_test_undecorated_toplevel(&mut harness);
    let third = map_test_undecorated_toplevel(&mut harness);
    harness
        .server
        .state
        .surfaces
        .get_mut(&second)
        .unwrap()
        .minimized = true;
    harness
        .server
        .state
        .surfaces
        .get_mut(&third)
        .unwrap()
        .mapped = false;
    harness.server.state.activate_managed_window(&first);
    harness.server.state.cycle_window(false);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(first.clone())
    );
    let stale = harness.server.state.surfaces[&second]
        .role
        .wl_surface()
        .clone();
    harness.server.state.activate_managed_window(&stale);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(first)
    );
}

#[test]
fn switching_does_not_raise_or_focus_through_exclusive_layer_or_lock() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let target = test_toplevel_record(&harness).role.wl_surface().clone();
    map_test_layer_surface(
        &mut harness,
        0,
        TestLayerSpec {
            anchor: 1 | 4,
            keyboard_interactivity: zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive as u32,
            ..TestLayerSpec::default()
        },
    );
    let focus = focused_surface(harness.server.state.keyboard.current_focus());
    let z = test_toplevel_record(&harness).layout.z;
    harness.server.state.cycle_window(false);
    harness.server.state.activate_managed_window(&target);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        focus
    );
    assert_eq!(test_toplevel_record(&harness).layout.z, z);

    let mut locked = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut locked);
    let target = test_toplevel_record(&locked).role.wl_surface().clone();
    let _lock = begin_test_session_lock(&mut locked);
    let focus = focused_surface(locked.server.state.keyboard.current_focus());
    let z = test_toplevel_record(&locked).layout.z;
    locked.server.state.cycle_window(false);
    locked.server.state.activate_managed_window(&target);
    assert_eq!(
        focused_surface(locked.server.state.keyboard.current_focus()),
        focus
    );
    assert_eq!(test_toplevel_record(&locked).layout.z, z);
}
