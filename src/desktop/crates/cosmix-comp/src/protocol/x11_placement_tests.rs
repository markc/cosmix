fn reserve_left(harness: &mut KeybindingHarness) {
    map_test_layer_surface(
        harness,
        0,
        TestLayerSpec {
            size: (240, 0),
            anchor: 1 | 2 | 4,
            exclusive_zone: 240,
            ..TestLayerSpec::default()
        },
    );
    assert_eq!(harness.server.state.usable_output_rect().x, 240.0);
}

fn assert_outer_inside(harness: &KeybindingHarness, object: &ObjectId) {
    let record = &harness.server.state.surfaces[object];
    let usable = harness.server.state.usable_output_rect();
    let extents = DecoExtents::of(&harness.server.state.decoration.theme);
    let ssd = record.committed_decoration == SceneDecorationMode::ServerSide;
    let left = if ssd { extents.left } else { 0.0 };
    let top = if ssd { extents.top } else { 0.0 };
    let right = if ssd { extents.right } else { 0.0 };
    let bottom = if ssd { extents.bottom } else { 0.0 };
    assert!(record.window_origin.0 - left >= usable.x);
    assert!(record.window_origin.1 - top >= usable.y);
    assert!(
        record.window_origin.0 + record.configured_size.0 as f32 + right <= usable.x + usable.width
    );
    assert!(
        record.window_origin.1 + record.configured_size.1 as f32 + bottom
            <= usable.y + usable.height
    );
}

#[test]
fn x11_premap_size_only_uses_panel_aware_placement_in_both_association_orders() {
    for association_first in [false, true] {
        let mut harness = KeybindingHarness::new(true);
        reserve_left(&mut harness);
        let (sid, surface) = roleless_wl_surface(&mut harness);
        let window = fake_x11_window(200, false, Rectangle::new((0, 0).into(), (200, 150).into()));
        window.set_wl_surface_offline(Some(surface.clone()));
        harness.server.state.x11_new_window(window.clone());
        if association_first {
            harness
                .server
                .state
                .x11_associate_window(surface.clone(), window.clone());
        }
        harness.server.state.x11_configure_request(
            window.clone(),
            None,
            None,
            Some(200),
            Some(150),
            None,
        );
        let first_index = harness.server.state.next_layout_index;
        harness.server.state.x11_configure_request(
            window.clone(),
            None,
            None,
            Some(210),
            Some(160),
            None,
        );
        assert_eq!(
            harness.server.state.next_layout_index, first_index,
            "repeated pre-map configure must not advance the cascade"
        );
        harness.server.state.x11_map_window_request(window.clone());
        if !association_first {
            harness
                .server
                .state
                .x11_associate_window(surface.clone(), window);
        }
        commit_dmabuf(&mut harness, sid, 32, 24);
        assert_outer_inside(&harness, &surface.id());
    }
}

#[test]
fn x11_pending_grant_is_revalidated_when_panel_arrives_before_map() {
    let mut harness = KeybindingHarness::new(true);
    let (sid, surface) = roleless_wl_surface(&mut harness);
    let window = fake_x11_window(201, false, Rectangle::new((0, 0).into(), (200, 150).into()));
    window.set_wl_surface_offline(Some(surface.clone()));
    harness.server.state.x11_new_window(window.clone());
    harness.server.state.x11_configure_request(
        window.clone(),
        None,
        None,
        Some(200),
        Some(150),
        None,
    );
    reserve_left(&mut harness);
    harness.server.state.x11_map_window_request(window.clone());
    harness
        .server
        .state
        .x11_associate_window(surface.clone(), window);
    commit_dmabuf(&mut harness, sid, 32, 24);
    assert_outer_inside(&harness, &surface.id());
}

#[test]
fn x11_initial_placement_keeps_ssd_inside_right_bottom_and_small_output() {
    for size in [(800, 600), (50, 60)] {
        for preconfigure in [false, true] {
            let mut harness = KeybindingHarness::new(true);
            harness.server.state.resize_output(size.0, size.1);
            let usable = harness.server.state.usable_output_rect();
            let (sid, surface) = roleless_wl_surface(&mut harness);
            let origin = (
                (usable.x + usable.width - 1.0) as i32,
                (usable.y + usable.height - 1.0) as i32,
            );
            let window =
                fake_x11_window(204, false, Rectangle::new(origin.into(), (200, 150).into()));
            window.set_wl_surface_offline(Some(surface.clone()));
            harness.server.state.x11_new_window(window.clone());
            if preconfigure {
                harness
                    .server
                    .state
                    .x11_associate_window(surface.clone(), window.clone());
                harness.server.state.x11_configure_request(
                    window.clone(),
                    Some(origin.0),
                    Some(origin.1),
                    Some(200),
                    Some(150),
                    None,
                );
            }
            harness.server.state.x11_map_window_request(window.clone());
            if !preconfigure {
                harness
                    .server
                    .state
                    .x11_associate_window(surface.clone(), window);
            }
            commit_dmabuf(&mut harness, sid, 32, 24);
            assert_outer_inside(&harness, &surface.id());
        }
    }
}

#[test]
fn x11_fullscreen_returns_to_maximized_work_area_without_consuming_normal_restore() {
    let mut harness = KeybindingHarness::new(true);
    let (sid, _, window, object) = associate_normal_window(&mut harness, 205);
    commit_dmabuf(&mut harness, sid, 32, 24);
    harness.server.state.request_x11_maximized(&window, true);
    let restore = harness.server.state.surfaces[&object].normal_restore;
    harness.server.state.request_x11_fullscreen(&window, true);
    reserve_left(&mut harness);
    harness.server.state.request_x11_fullscreen(&window, false);
    let record = &harness.server.state.surfaces[&object];
    assert!(record.committed_maximized);
    assert_eq!(record.normal_restore, restore);
    assert_outer_inside(&harness, &object);
    let extents = DecoExtents::of(&harness.server.state.decoration.theme);
    let usable = harness.server.state.usable_output_rect();
    assert_eq!(
        record.configured_size.0 as f32 + extents.left + extents.right,
        usable.width
    );
    harness.server.state.request_x11_maximized(&window, false);
    assert!(!harness.server.state.surfaces[&object].committed_maximized);
    assert_outer_inside(&harness, &object);
}

#[test]
fn x11_restack_does_not_resize_fullscreen_or_maximized_windows() {
    for committed in [false, true] {
        for fullscreen in [false, true] {
            let mut harness = KeybindingHarness::new(true);
            let (sid, _, window, object) = associate_normal_window(&mut harness, 206);
            if committed {
                commit_dmabuf(&mut harness, sid, 32, 24);
            }
            if fullscreen {
                harness.server.state.request_x11_fullscreen(&window, true);
            } else {
                harness.server.state.request_x11_maximized(&window, true);
            }
            let before = harness.server.state.surfaces[&object]
                .role
                .x11()
                .unwrap()
                .granted_geometry;
            harness.server.state.x11_configure_request(
                window.clone(),
                None,
                None,
                None,
                None,
                Some(smithay::xwayland::xwm::Reorder::Top),
            );
            assert_eq!(
                harness.server.state.surfaces[&object]
                    .role
                    .x11()
                    .unwrap()
                    .granted_geometry,
                before
            );
            harness.server.state.x11_configure_request(
                window,
                None,
                None,
                Some(100),
                Some(100),
                None,
            );
            assert_eq!(
                harness.server.state.surfaces[&object]
                    .role
                    .x11()
                    .unwrap()
                    .granted_geometry,
                before
            );
        }
    }
}

#[test]
fn x11_clamped_restore_publishes_state_even_when_final_geometry_is_unchanged() {
    let mut harness = KeybindingHarness::new(true);
    let (sid, _, window, object) = associate_normal_window(&mut harness, 207);
    commit_dmabuf(&mut harness, sid, 32, 24);
    harness
        .server
        .state
        .apply_x11_geometry(207, Rectangle::new((36, 36).into(), (780, 550).into()));
    harness.server.state.request_x11_maximized(&window, true);
    harness.server.state.resize_output(400, 300);
    let before = harness.server.state.surfaces[&object]
        .role
        .x11()
        .unwrap()
        .granted_geometry;
    let id = harness.server.state.surfaces[&object].id;
    harness.server.state.events.clear();
    harness.server.state.request_x11_maximized(&window, false);
    let record = &harness.server.state.surfaces[&object];
    assert!(!record.committed_maximized);
    assert_eq!(
        record.role.x11().unwrap().granted_geometry,
        before,
        "fixture must exercise equal final geometry"
    );
    assert!(harness.server.state.events.iter().any(|event| matches!(event, ProtocolEvent::SurfaceRelayout { id: emitted, .. } if *emitted == id)));
}

#[test]
fn x11_panel_change_reflows_normal_and_maximized_but_not_fullscreen_or_menu() {
    for mode in ["normal", "maximized", "fullscreen"] {
        let mut harness = KeybindingHarness::new(true);
        let (sid, _, window, object) = associate_normal_window(&mut harness, 202);
        commit_dmabuf(&mut harness, sid, 32, 24);
        match mode {
            "maximized" => harness.server.state.request_x11_maximized(&window, true),
            "fullscreen" => harness.server.state.request_x11_fullscreen(&window, true),
            _ => {}
        }
        let restore = harness.server.state.surfaces[&object].normal_restore;
        reserve_left(&mut harness);
        let record = &harness.server.state.surfaces[&object];
        assert_eq!(
            record.normal_restore, restore,
            "reflow must preserve restore memory"
        );
        if mode == "fullscreen" {
            assert_eq!(record.window_origin, (0.0, 0.0));
            harness.server.state.request_x11_fullscreen(&window, false);
        } else if mode == "maximized" {
            assert_outer_inside(&harness, &object);
            harness.server.state.request_x11_maximized(&window, false);
        }
        assert_outer_inside(&harness, &object);
    }

    let mut harness = KeybindingHarness::new(true);
    let (sid, surface) = roleless_wl_surface(&mut harness);
    let geometry = Rectangle::new((-2, 83).into(), (203, 234).into());
    let menu = fake_x11_window(203, true, geometry);
    menu.set_wl_surface_offline(Some(surface.clone()));
    harness
        .server
        .state
        .x11_new_override_redirect_window(menu.clone());
    harness
        .server
        .state
        .x11_associate_window(surface.clone(), menu.clone());
    harness
        .server
        .state
        .x11_mapped_override_redirect_window(menu);
    commit_dmabuf(&mut harness, sid, 32, 24);
    reserve_left(&mut harness);
    assert_eq!(
        harness.server.state.surfaces[&surface.id()]
            .role
            .x11()
            .unwrap()
            .granted_geometry,
        geometry
    );
}
