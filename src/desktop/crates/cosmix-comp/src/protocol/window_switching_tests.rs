use super::*;

/// Offline source guard: delayed root/grab X events cannot replace the
/// compositor seat's publication. Actual X property delivery is a live gate.
#[test]
fn active_window_publication_is_seat_owned_not_raw_focus_event_owned() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source =
        std::fs::read_to_string(root.join("../../vendor/smithay/src/xwayland/xwm/mod.rs")).unwrap();
    assert!(source.contains("Event::FocusIn(_) | Event::FocusOut(_) => {}"));
    assert!(!source.contains("&[n.event]"));
    assert!(source.contains("window.xwm_id() == Some(self.id)"));
    let handler = std::fs::read_to_string(root.join("src/protocol/handlers.rs")).unwrap();
    assert!(handler.contains("self.publish_x11_active_window(focused_root.as_ref());"));
}

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

/// EWMH desktops (workspaces 0.59.0, slice 6): the vendored
/// `_NET_WM_DESKTOP` ClientMessage arm hands the client's request to the
/// policy callback unchanged (32-bit only, the window found by XID, data[0]
/// desktop and data[1] source), and comp's delegate is 1:1 and
/// generation-gated. Source pins: the XWM handshake is unavailable offline.
#[test]
fn vendored_desktop_dispatch_keeps_format_and_policy_callback() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source =
        std::fs::read_to_string(root.join("../../vendor/smithay/src/xwayland/xwm/mod.rs")).unwrap();
    let branch = source
        .split("x if x == xwm.atoms._NET_WM_DESKTOP && msg.format == 32 => {")
        .nth(1)
        .expect("32-bit desktop dispatch");
    let branch = branch
        .split("x if x == xwm.atoms.WL_SURFACE_ID")
        .next()
        .unwrap();
    assert!(branch.contains("surface.window_id() == msg.window"));
    assert!(branch.contains("state.desktop_request(xwm_id, surface, data[0], data[1]);"));
    assert!(
        source.contains(
            "fn desktop_request(&mut self, _xwm: XwmId, _window: X11Surface, _desktop: u32, _source: u32) {}"
        ),
        "the trait default is a no-op: policy is the compositor's"
    );
    let comp = std::fs::read_to_string(root.join("src/protocol/xwayland.rs")).unwrap();
    let delegate = comp
        .split("fn desktop_request(&mut self, xwm: XwmId, window: X11Surface, desktop: u32, _source: u32) {")
        .nth(1)
        .expect("comp delegate");
    let delegate = delegate.split("fn xwm_state").next().unwrap();
    assert!(delegate.contains("if !self.xwm_event_is_live(xwm) {"));
    assert!(delegate.contains("self.x11_desktop_request(window, desktop);"));

    // The root `_NET_CURRENT_DESKTOP` arm (a pager's switch request): no
    // window lookup, data[0] the desktop and data[1] the timestamp, the
    // same no-op default and the same generation-gated delegate. Both
    // root atoms are advertised in `_NET_SUPPORTED`, so a pager that reads
    // it and sends the standard message must be answered.
    let root_arm = source
        .split("x if x == xwm.atoms._NET_CURRENT_DESKTOP && msg.format == 32 => {")
        .nth(1)
        .expect("32-bit current-desktop dispatch");
    let root_arm = root_arm
        .split("x if x == xwm.atoms.WL_SURFACE_ID")
        .next()
        .unwrap();
    assert!(!root_arm.contains("surface.window_id() == msg.window"), "a root message: no window");
    assert!(root_arm.contains("state.current_desktop_request(xwm_id, data[0], data[1]);"));
    assert!(
        source.contains(
            "fn current_desktop_request(&mut self, _xwm: XwmId, _desktop: u32, _timestamp: u32) {}"
        ),
        "the trait default is a no-op: policy is the compositor's"
    );
    let delegate = comp
        .split("fn current_desktop_request(&mut self, xwm: XwmId, desktop: u32, _timestamp: u32) {")
        .nth(1)
        .expect("comp delegate for the root message");
    let delegate = delegate.split("fn xwm_state").next().unwrap();
    assert!(delegate.contains("if !self.xwm_event_is_live(xwm) {"));
    assert!(delegate.contains("self.x11_current_desktop_request(desktop);"));
}

/// EWMH desktops: the three atoms are advertised in `_NET_SUPPORTED`, the
/// root pair is written at WM start (1 desktop, current 0) so `xprop` never
/// sees it absent, the root setters exist, and comp republishes the pair
/// 0-based after every switch and count change — after `current` moved, so
/// the property reads the new value. The property writes themselves are the
/// live gate's (rule 10, `xprop -root _NET_CURRENT_DESKTOP`); the offline
/// suite has no XWM.
#[test]
fn supported_atoms_list_desktop_atoms() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source =
        std::fs::read_to_string(root.join("../../vendor/smithay/src/xwayland/xwm/mod.rs")).unwrap();
    let supported = source
        .split("atoms._NET_SUPPORTED,")
        .nth(1)
        .expect("_NET_SUPPORTED write in start_wm")
        .split("],")
        .next()
        .unwrap();
    for atom in ["_NET_NUMBER_OF_DESKTOPS", "_NET_CURRENT_DESKTOP", "_NET_WM_DESKTOP"] {
        assert!(supported.contains(&format!("atoms.{atom},")), "{atom} advertised");
    }
    let compact: String = source.split_whitespace().collect();
    assert!(compact.contains("atoms._NET_NUMBER_OF_DESKTOPS,AtomEnum::CARDINAL,&[1],"));
    assert!(compact.contains("atoms._NET_CURRENT_DESKTOP,AtomEnum::CARDINAL,&[0],"));
    assert!(source.contains("pub fn set_number_of_desktops(&self, count: u32) -> Result<(), ConnectionError>"));
    assert!(source.contains("pub fn set_current_desktop(&self, index: u32) -> Result<(), ConnectionError>"));
    let surface =
        std::fs::read_to_string(root.join("../../vendor/smithay/src/xwayland/xwm/surface.rs")).unwrap();
    assert!(surface.contains("pub fn set_desktop(&self, desktop: u32) -> Result<(), ConnectionError>"));
    let surface_compact: String = surface.split_whitespace().collect();
    assert!(surface_compact.contains("self.atoms._NET_WM_DESKTOP,AtomEnum::CARDINAL,&[desktop],"));

    let comp = std::fs::read_to_string(root.join("src/protocol/xwayland.rs")).unwrap();
    let publish = comp
        .split("pub(super) fn publish_x11_desktops(&self) {")
        .nth(1)
        .expect("publish_x11_desktops")
        .split("pub(super) fn sync_x11_desktops")
        .next()
        .unwrap();
    assert!(publish.contains("wm.set_number_of_desktops(count)"));
    assert!(publish.contains("let current = self.workspace_current().saturating_sub(1);"));
    assert!(publish.contains("wm.set_current_desktop(current)"));

    let workspaces = std::fs::read_to_string(root.join("src/protocol/workspaces.rs")).unwrap();
    let switch = workspaces
        .split("pub(crate) fn switch_workspace(")
        .nth(1)
        .expect("switch_workspace")
        .split("pub(crate) fn move_window_to_workspace(")
        .next()
        .unwrap();
    let moved = switch
        .find("self.workspaces.current.insert(output.clone(), to);")
        .expect("current moves in switch_workspace");
    let published = switch
        .find("self.publish_x11_desktops();")
        .expect("switch_workspace publishes the root pair");
    assert!(moved < published, "published AFTER current moved");
    let count = workspaces
        .split("pub(crate) fn set_workspace_count(")
        .nth(1)
        .expect("set_workspace_count")
        .split("pub(crate) fn ensure_workspace_shown(")
        .next()
        .unwrap();
    assert_eq!(
        count.matches("self.publish_x11_desktops();").count(),
        2,
        "both the grow and the shrink arm publish the count"
    );
    let windows_synced = count
        .find("self.sync_x11_desktops();")
        .expect("a shrink republishes every window");
    let root_published = count.rfind("self.publish_x11_desktops();").unwrap();
    assert!(
        windows_synced < root_published,
        "shrink: windows first, then the root count, so no reader sees a desktop >= count"
    );
    // The XWM start publishes the real model over start_wm's 1/0.
    let ready = comp
        .split("\"XWayland ready; XWM started\"")
        .nth(1)
        .expect("ready log")
        .split("Err(error) =>")
        .next()
        .unwrap();
    assert!(ready.contains("self.publish_x11_desktops();"));
    assert!(ready.contains("self.sync_x11_desktops();"));
    // A KMS topology change can replace the default output, whose current
    // workspace `_NET_CURRENT_DESKTOP` mirrors (D3): the apply site hands
    // the pre-apply key and current to
    // `reconcile_workspace_current_after_topology_change` after the output
    // bindings are reconciled, and THAT carries the workspace over,
    // republishes the root pair and settles when the value changed (the
    // behaviour is unit-tested on the helper in `tests.rs`;
    // `a_replaced_default_output_keeps_its_workspace_and_a_changed_one_settles`).
    // A source pin for the wiring: the site is KMS event plumbing the
    // offline harness cannot drive.
    let protocol = std::fs::read_to_string(root.join("src/protocol/mod.rs")).unwrap();
    let apply = protocol
        .split("ChannelEvent::Msg(ProtocolCommand::KmsTopologyLifecycle {")
        .nth(1)
        .expect("topology apply site")
        .split("state.end_pointer_hit_test_batch();")
        .next()
        .unwrap();
    let read_before = apply
        .find("let previous_workspace_key = state.default_output_key();")
        .expect("the pre-apply key is read");
    let applied = apply
        .find(".apply_kms_topology_lifecycle(event)")
        .expect("the backend applies the event");
    let reconciled = apply
        .find("state.reconcile_workspace_current_after_topology_change(")
        .expect("a topology change reconciles the default output's current workspace");
    assert!(
        read_before < applied && applied < reconciled,
        "key and current are read BEFORE the apply and reconciled AFTER it"
    );
    let helper = workspaces
        .split("pub(super) fn reconcile_workspace_current_after_topology_change(")
        .nth(1)
        .expect("reconcile_workspace_current_after_topology_change")
        .split("\n    }\n")
        .next()
        .unwrap();
    assert!(
        helper.contains("self.publish_x11_desktops();"),
        "a topology change republishes the root pair"
    );
    let resynced = helper
        .find("self.sync_x11_suspended_for_workspaces();")
        .expect("a changed current re-derives every X11 suspended flag");
    let settled = helper
        .find("self.settle_workspace_visibility(None);")
        .expect("a changed current settles");
    assert!(resynced < settled, "flags before the settle, as the shrink does it");
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

/// F1.7: Alt+Tab scoping is automatic — a window moved off the current
/// workspace drops out of the cycle exactly as a minimised one does. Unlike
/// a minimised one, activating it directly is NOT refused: F1.2 switches
/// to its workspace first (never pulls it across) and then focuses it.
#[test]
fn switching_skips_off_workspace_windows() {
    use workspaces::WorkspaceTarget;
    let mut harness = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut harness);
    let first = test_toplevel_record(&harness).role.wl_surface().clone();
    let second = map_test_undecorated_toplevel(&mut harness);
    let third = map_test_undecorated_toplevel(&mut harness);
    assert_eq!(
        harness
            .server
            .state
            .move_window_to_workspace(&second, WorkspaceTarget::Index(2)),
        Ok((1, 2))
    );
    assert!(!harness.server.state.surfaces[&second].layout.visible);
    assert!(!harness.server.state.surfaces[&second].minimized);
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
    let elsewhere = harness.server.state.surfaces[&second]
        .role
        .wl_surface()
        .clone();
    assert_eq!(harness.server.state.workspace_current(), 1);
    harness.server.state.activate_managed_window(&elsewhere);
    assert_eq!(harness.server.state.workspace_current(), 2);
    assert_eq!(harness.server.state.surfaces[&second].workspace, 2);
    assert!(harness.server.state.surfaces[&second].layout.visible);
    assert!(!harness.server.state.surfaces[&first.id()].layout.visible);
    assert_eq!(
        focused_surface(harness.server.state.keyboard.current_focus()),
        Some(elsewhere)
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
