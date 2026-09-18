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

/// F1.2 at the X11 unminimise request: an off-workspace X11 window that a
/// client un-minimises is brought on screen by switching to its workspace
/// (never pulled across), un-suspended (D15) and focused — the request is
/// `restore_window` (D5), so it inherits the switch.
#[test]
fn x11_unminimise_of_an_off_workspace_window_switches_first() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let native = test_toplevel_record(&harness).role.wl_surface().id();
    let (id, surface, window, object) = associate_normal_window(&mut harness, 908);
    commit_dmabuf(&mut harness, id, 32, 24);
    assert_eq!(harness.server.state.surfaces[&object].workspace, 1);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&object, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    assert!(!harness.server.state.surfaces[&object].layout.visible);
    assert!(window.is_minimized(), "off-workspace: suspended (D15)");
    harness.server.state.minimize_toplevel(&surface);
    assert!(harness.server.state.surfaces[&object].minimized);
    assert_eq!(harness.server.state.workspace_current(), 1);

    harness.server.state.x11_unminimize_request(window.clone());
    let record = &harness.server.state.surfaces[&object];
    assert!(!record.minimized);
    assert_eq!(record.workspace, 2, "never pulled across");
    assert!(record.layout.visible);
    assert!(!window.is_minimized(), "on screen again: not suspended");
    assert_eq!(harness.server.state.workspace_current(), 2);
    assert!(!harness.server.state.surfaces[&native].layout.visible);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(surface)
    );
    assert!(harness.server.state.minimized_toplevels.is_empty());
}

/// D18: under a session lock the client-driven X11 paths cannot change the
/// workspace. `_NET_ACTIVE_WINDOW` is refused outright; an unminimise still
/// clears the minimised flag but the window stays off its workspace and
/// therefore suspended (D15), and `workspace_current()` is unchanged.
///
/// Base-discriminating: the SAME `_NET_ACTIVE_WINDOW` first runs unlocked
/// and does switch (to 2, focusing and resuming the window), so the locked
/// half is proven to be the lock holding, not the path never switching.
/// (The unminimise half's unlocked twin is
/// `x11_unminimise_of_an_off_workspace_window_switches_first`.)
#[test]
fn locked_x11_activation_and_unminimise_do_not_switch_workspace() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let native = test_toplevel_record(&harness).role.wl_surface().clone();
    let (id, surface, window, object) = associate_normal_window(&mut harness, 909);
    commit_dmabuf(&mut harness, id, 32, 24);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&object, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    harness.server.state.activate_managed_window(&native);
    let _ = harness.sync();
    assert!(window.is_minimized(), "off-workspace: suspended (D15)");

    // Unlocked: the activation switches to the window's workspace (never
    // pulls it across), focuses it, and the switch resumes it.
    harness.server.state.x11_activate_request(window.clone());
    assert_eq!(harness.server.state.workspace_current(), 2);
    let record = &harness.server.state.surfaces[&object];
    assert_eq!(record.workspace, 2, "never pulled across");
    assert!(record.layout.visible);
    assert!(!window.is_minimized(), "on screen: resumed");
    assert!(!harness.server.state.surfaces[&native.id()].layout.visible);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(surface)
    );

    // Back where the locked half starts: on 1, the native window focused.
    harness
        .server
        .state
        .switch_workspace(None, WorkspaceTarget::Index(1), true)
        .expect("back to 1");
    harness.server.state.activate_managed_window(&native);
    let _ = harness.sync();
    assert!(!harness.server.state.surfaces[&object].layout.visible);
    assert!(window.is_minimized(), "off screen again: suspended");
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(native.clone())
    );

    let lock = begin_test_session_lock(&mut harness);
    ack_and_map_test_lock_surface(&mut harness, lock);
    assert!(harness.server.state.session_lock_active());
    assert_eq!(harness.server.state.workspace_current(), 1);

    harness.server.state.x11_activate_request(window.clone());
    assert_eq!(harness.server.state.workspace_current(), 1);
    assert!(!harness.server.state.surfaces[&object].layout.visible);
    assert!(!harness.server.state.surfaces[&object].focused);

    harness
        .server
        .state
        .surfaces
        .get_mut(&object)
        .unwrap()
        .minimized = true;
    harness
        .server
        .state
        .minimized_toplevels
        .push(object.clone());
    harness.server.state.x11_unminimize_request(window.clone());
    assert_eq!(harness.server.state.workspace_current(), 1);
    let record = &harness.server.state.surfaces[&object];
    assert!(!record.minimized, "un-minimised, but not switched to");
    assert_eq!(record.workspace, 2);
    assert!(!record.layout.visible);
    assert!(
        window.is_minimized(),
        "still off screen: still suspended (D15)"
    );
    assert!(harness.server.state.minimized_toplevels.is_empty());
}

/// The refusal half of F1.2 on the client-driven paths: a
/// `_NET_ACTIVE_WINDOW` or an xdg-activation for a MINIMISED window on
/// another workspace is a no-op — nothing is focused, and because the
/// switch is gated on the same terms it does not change the workspace
/// either. `_NET_ACTIVE_WINDOW` always refused a minimised target
/// (`window_switch_candidate`); xdg-activation did NOT on the base tree —
/// `arbitrate_keyboard_focus` has no minimised term, so the keyboard focus
/// moved to the hidden window — and now refuses at `request_activation`.
/// A switch that then focuses nothing would be a client-driven, unreported
/// change of the user's desktop. (The unminimise request is the restore
/// path and does switch:
/// `x11_unminimise_of_an_off_workspace_window_switches_first`.)
#[test]
fn x11_activation_of_a_minimised_off_workspace_window_does_not_switch() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let native = test_toplevel_record(&harness).role.wl_surface().clone();
    let (id, surface, window, object) = associate_normal_window(&mut harness, 910);
    commit_dmabuf(&mut harness, id, 32, 24);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&object, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    harness.server.state.minimize_toplevel(&surface);
    harness.server.state.activate_managed_window(&native);
    let _ = harness.sync();
    assert_eq!(harness.server.state.workspace_current(), 1);
    assert!(!harness.server.state.session_lock_active());

    harness.server.state.x11_activate_request(window.clone());
    assert_eq!(
        harness.server.state.workspace_current(),
        1,
        "_NET_ACTIVE_WINDOW for a minimised window does not switch"
    );
    let record = &harness.server.state.surfaces[&object];
    assert!(record.minimized);
    assert_eq!(record.workspace, 2, "never pulled across");
    assert!(!record.layout.visible);
    assert!(!record.focused);
    assert!(window.is_minimized(), "still off screen: still suspended");
    assert!(harness.server.state.surfaces[&native.id()].layout.visible);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(native.clone())
    );

    XdgActivationHandler::request_activation(
        &mut harness.server.state,
        XdgActivationToken::from(String::from("test-token")),
        XdgActivationTokenData::default(),
        surface.clone(),
    );
    assert_eq!(
        harness.server.state.workspace_current(),
        1,
        "xdg-activation for a minimised window does not switch"
    );
    let record = &harness.server.state.surfaces[&object];
    assert!(record.minimized && !record.layout.visible && !record.focused);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(native)
    );
    assert_eq!(harness.server.state.minimized_toplevels, vec![object]);
}

/// D15 at the switch halves: switching away suspends an X11 window left
/// behind, switching back resumes it. This pins `current` moving BEFORE
/// the leaving loop in `switch_workspace`: the withdraw half derives the
/// flag from `current` (`sync_x11_suspended`), so a leaving window must
/// already read as off the current workspace — with the old order every
/// leaving X11 window kept rendering after a switch and no other test
/// noticed.
#[test]
fn switching_workspace_suspends_leaving_x11_windows_and_resumes_them_on_return() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let mut harness = KeybindingHarness::new(true);
    let (id, _surface, window, object) = associate_normal_window(&mut harness, 911);
    commit_dmabuf(&mut harness, id, 32, 24);
    assert_eq!(harness.server.state.surfaces[&object].workspace, 1);
    assert!(harness.server.state.surfaces[&object].layout.visible);
    assert!(!window.is_minimized(), "on screen: not suspended");

    harness
        .server
        .state
        .switch_workspace(None, WorkspaceTarget::Index(2), true)
        .expect("switch to 2");
    let record = &harness.server.state.surfaces[&object];
    assert!(!record.minimized, "left behind, not minimised");
    assert_eq!(record.workspace, 1);
    assert!(!record.layout.visible);
    assert!(window.is_minimized(), "left behind: suspended (D15)");

    harness
        .server
        .state
        .switch_workspace(None, WorkspaceTarget::Index(1), true)
        .expect("back to 1");
    let record = &harness.server.state.surfaces[&object];
    assert!(record.layout.visible);
    assert!(!window.is_minimized(), "back on screen: resumed");
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
        let admission = ingress.request_window(op.clone()).expect("verb admitted");
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

/// Destroying a mapped surface (Xwayland destroys the wl_surface of an X11
/// window without a role object to tear down first) discards its pending
/// presentation feedback.
#[test]
fn x11_surface_destroy_discards_pending_presentation_feedback() {
    let mut harness = KeybindingHarness::new(true);
    let (surface_id, _, _window, object) = associate_normal_window(&mut harness, 907);
    commit_dmabuf(&mut harness, surface_id, 32, 24);
    let id = harness.server.state.surfaces[&object].id;
    let (presentation, _) = bind_test_presentation(&mut harness);
    let callback = request_surface_feedback(&mut harness, presentation, surface_id);
    send_request(&mut harness.client, surface_id, 6, &[]);
    harness.dispatch_client();
    assert_eq!(
        harness.server.state.presentation.ledger.pending_count(id),
        1
    );
    send_request(&mut harness.client, surface_id, 0, &[]);
    harness.dispatch_client();
    harness.assert_client_connected("after destroying the X11 surface");
    assert_eq!(
        harness.server.state.presentation.ledger.pending_count(id),
        0
    );
    let events = harness.sync();
    assert_eq!(feedback_opcodes(&events, callback), [2], "{events:?}");
}

/// `close {force}` on an X11 window sends the polite close and refuses the
/// kill at once (Xwayland is the Wayland client), without waiting out the
/// timeout and without disconnecting anything.
#[cfg(feature = "bus")]
#[test]
fn x11_forced_close_is_refused_immediately() {
    let (mut harness, ingress, _observations) = KeybindingHarness::new_with_port();
    let (surface_id, _, _window, object) = associate_normal_window(&mut harness, 907);
    commit_dmabuf(&mut harness, surface_id, 32, 24);
    let record = &harness.server.state.surfaces[&object];
    let (id, generation) = (record.id.0, record.generation);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("control reply runtime");
    let started = std::time::Instant::now();
    let admission = ingress
        .request_long(crate::port::LongOp::ForceClose {
            id,
            generation,
            timeout: Duration::from_secs(30),
        })
        .expect("admitted");
    harness
        .server
        .dispatch_cycle(Some(Duration::ZERO))
        .expect("service cycle");
    let reply = runtime
        .block_on(admission.receive())
        .expect("reply")
        .wire_json();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(reply["error"], "still_open", "{reply}");
    assert_eq!(reply["error_code"], "still_open");
    assert_eq!(reply["reason"], "x11_kill_unsupported");
    assert_eq!(reply["polite_close_sent"], true);
    assert!(harness.server.state.window_waiters.waiters.is_empty());
    harness.assert_client_connected("the X11 refusal kills nothing");
}

/// D11: an X11 toplevel has no `windows.*` row (P2), so its workspace is
/// read from `surfaces.s<id>.workspace` — null before its first map,
/// stamped with the current workspace at the map (rule 2), and following a
/// move like any managed window.
#[cfg(feature = "bus")]
#[test]
fn x11_window_workspace_is_readable_on_the_surfaces_row() {
    use workspaces::WorkspaceTarget;
    let (mut harness, _ingress, _observations) = KeybindingHarness::new_with_port();
    let (surface_id, _surface, _window, object) = associate_normal_window(&mut harness, 905);
    let context = harness
        .server
        .state
        .port_context
        .clone()
        .expect("port context");
    let key = format!("s{}", harness.server.state.surfaces[&object].id.0);
    let before = port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot");
    assert!(!before.surfaces[&key].mapped);
    assert_eq!(
        before.surfaces[&key].workspace, None,
        "no workspace before the first map"
    );

    commit_dmabuf(&mut harness, surface_id, 32, 24);
    let current = harness.server.state.workspace_current();
    assert_eq!(current, 1);
    let mapped = port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot");
    let row = &mapped.surfaces[&key];
    assert!(row.mapped);
    assert_eq!(row.role, "x11-toplevel");
    assert_eq!(row.workspace, Some(current));
    assert!(row.visible);
    assert!(
        !mapped.windows.contains_key(&key),
        "X11 rows are not windows.* rows in 0.59"
    );
    // ...but `workspaces.list` counts it: the pager reading that leaf must
    // not show the workspace empty while the X11 window is on it.
    let counts = |snapshot: &port_snapshot::CompSnapshot| {
        snapshot
            .workspaces
            .list
            .iter()
            .map(|row| row.windows)
            .collect::<Vec<_>>()
    };
    assert_eq!(counts(&before), [0, 0, 0, 0], "unmapped: on no workspace");
    assert_eq!(counts(&mapped), [1, 0, 0, 0]);

    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&object, WorkspaceTarget::Index(3)),
        Ok((1, 3))
    );
    let moved = port_snapshot::snapshot(&harness.server.state, &context).expect("snapshot");
    assert_eq!(moved.surfaces[&key].workspace, Some(3));
    assert!(!moved.surfaces[&key].visible);
    assert!(!moved.surfaces[&key].minimized);
    assert_eq!(moved.workspaces.current, 1);
    assert_eq!(counts(&moved), [0, 0, 1, 0]);
}

/// EWMH (slice 6): a client `_NET_WM_DESKTOP` message is a MOVE, never a
/// switch — `_NET_ACTIVE_WINDOW` is the request that brings a window on
/// screen. The offline connection is dead, so property delivery is the live
/// gate's (rule 10); what the fake can show is the value the WM was asked to
/// write (`X11Surface::desktop`, mirrored before the wire): 0-based, stamped
/// at the map edge, rewritten by the move. `0xFFFFFFFF` (all desktops), an
/// index at or above the count, a stale identity for the same wl_surface and
/// a session lock are all ignored, leaving the record and the mirror alone.
#[test]
fn x11_desktop_request_moves_the_window_without_switching() {
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let native = test_toplevel_record(&harness).role.wl_surface().clone();
    let (id, surface, window, object) = associate_normal_window(&mut harness, 912);
    assert_eq!(window.desktop(), None, "nothing published before the map edge");
    commit_dmabuf(&mut harness, id, 32, 24);
    assert_eq!(harness.server.state.surfaces[&object].workspace, 1);
    assert_eq!(window.desktop(), Some(0), "stamped at the map edge, 0-based");

    harness.server.state.x11_desktop_request(window.clone(), 1);
    let record = &harness.server.state.surfaces[&object];
    assert_eq!(record.workspace, 2);
    assert!(!record.layout.visible);
    assert!(!record.minimized, "moved, not minimised");
    assert!(window.is_minimized(), "off-workspace: EWMH hidden (D15)");
    assert_eq!(window.desktop(), Some(1));
    assert_eq!(
        harness.server.state.workspace_current(),
        1,
        "a move never switches"
    );
    assert!(harness.server.state.surfaces[&native.id()].layout.visible);

    // Ignored: all-desktops, at/above the count, a stale identity.
    let count = harness.server.state.workspaces.count;
    let stale = fake_x11_window(913, false, Rectangle::new((0, 0).into(), (200, 150).into()));
    stale.set_wl_surface_offline(Some(surface.clone()));
    for (target, desktop) in [(&window, u32::MAX), (&window, count), (&stale, 0)] {
        harness
            .server
            .state
            .x11_desktop_request(target.clone(), desktop);
        assert_eq!(harness.server.state.surfaces[&object].workspace, 2);
        assert_eq!(window.desktop(), Some(1));
        assert_eq!(stale.desktop(), None);
    }

    // Back onto the current workspace: on screen again, resumed, 0 published.
    harness.server.state.x11_desktop_request(window.clone(), 0);
    let record = &harness.server.state.surfaces[&object];
    assert_eq!(record.workspace, 1);
    assert!(record.layout.visible);
    assert!(!window.is_minimized(), "on screen: resumed");
    assert_eq!(window.desktop(), Some(0));
    assert_eq!(harness.server.state.workspace_current(), 1);

    // Under a session lock the request is inert, like the props write.
    let lock = begin_test_session_lock(&mut harness);
    ack_and_map_test_lock_surface(&mut harness, lock);
    assert!(harness.server.state.session_lock_active());
    harness.server.state.x11_desktop_request(window.clone(), 2);
    assert_eq!(harness.server.state.surfaces[&object].workspace, 1);
    assert_eq!(window.desktop(), Some(0));
}

/// D19: after a MapRequest a first-map X11 record is still unmapped and has
/// no workspace (0) and no `_NET_WM_DESKTOP`; the stamp — and the property —
/// come at the first buffer commit, onto the workspace current THEN. A
/// `set_desktop` at MapRequest would have written `0 - 1`.
#[test]
fn x11_window_maps_onto_the_current_workspace() {
    use crate::protocol::workspaces::WorkspaceTarget;
    let mut harness = KeybindingHarness::new(true);
    harness
        .server
        .state
        .switch_workspace(None, WorkspaceTarget::Index(2), true)
        .expect("switch to 2");
    // `associate_normal_window` sends the MapRequest before association.
    let (id, _surface, window, object) = associate_normal_window(&mut harness, 914);
    let record = &harness.server.state.surfaces[&object];
    assert!(!record.mapped);
    assert_eq!(record.workspace, 0, "unmapped: on no workspace");
    assert_eq!(window.desktop(), None, "no desktop before the stamping edge");

    commit_dmabuf(&mut harness, id, 32, 24);
    let record = &harness.server.state.surfaces[&object];
    assert!(record.mapped);
    assert_eq!(record.workspace, 2, "joins the workspace current at the map edge");
    assert!(record.layout.visible);
    assert!(!window.is_minimized());
    assert_eq!(window.desktop(), Some(1), "0-based");
}

/// A count shrink strands the window onto the last workspace and republishes
/// its `_NET_WM_DESKTOP` (the mass re-derivation, not the per-move write).
#[test]
fn shrinking_the_count_republishes_x11_desktops() {
    let mut harness = KeybindingHarness::new(true);
    let (id, _surface, window, object) = associate_normal_window(&mut harness, 915);
    commit_dmabuf(&mut harness, id, 32, 24);
    harness.server.state.x11_desktop_request(window.clone(), 3);
    assert_eq!(harness.server.state.surfaces[&object].workspace, 4);
    assert_eq!(window.desktop(), Some(3));
    assert!(window.is_minimized(), "off-workspace: hidden");

    assert_eq!(harness.server.state.set_workspace_count(2), Ok((4, 2)));
    let record = &harness.server.state.surfaces[&object];
    assert_eq!(record.workspace, 2, "stranded onto the last workspace");
    assert_eq!(window.desktop(), Some(1));
    assert!(window.is_minimized(), "still off the current workspace (1)");

    assert_eq!(harness.server.state.set_workspace_count(1), Ok((2, 1)));
    let record = &harness.server.state.surfaces[&object];
    assert_eq!(record.workspace, 1);
    assert!(record.layout.visible);
    assert_eq!(window.desktop(), Some(0));
    assert!(!window.is_minimized(), "on the only workspace: resumed");
}
