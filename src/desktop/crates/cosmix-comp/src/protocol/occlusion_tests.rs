//! Real socket/Smithay transactions; renderer certificates are injected at the
//! same shared-slot boundary as frame_content, without requiring a local GPU.
use super::*;
use crate::occlusion::{Bounds, Draw, TreeVisibility};

fn fixture() -> (KeybindingHarness, ObjectId, ObjectId) {
    let mut h = KeybindingHarness::new(true);
    map_initial_test_toplevel(&mut h);
    commit_test_buffer(&mut h, TEST_SUBSURFACE_SURFACE_ID);
    send_request(&mut h.client, TEST_TOPLEVEL_SURFACE_ID, 6, &[]);
    h.dispatch_client();
    let victim = test_toplevel_record(&h).role.wl_surface().id();
    let (_, _, _, cover) = map_named_test_toplevel(&mut h, "cover", "test.cover");
    (h, victim, cover)
}

pub(super) fn request(h: &mut KeybindingHarness, surface: u32) -> u32 {
    let callback = h.allocate_object_id();
    send_request(&mut h.client, surface, 3, &words(&[callback]));
    send_request(&mut h.client, surface, 6, &[]);
    h.dispatch_client();
    callback
}

pub(super) fn align(h: &mut KeybindingHarness, victim: &ObjectId, cover: &ObjectId) {
    let v = h.server.state.surfaces.get_mut(victim).unwrap();
    v.layout.x = 10.0;
    v.layout.y = 10.0;
    v.window_origin = v
        .committed_window_geometry
        .map_or((10.0, 10.0), |g| (10.0 + g.x, 10.0 + g.y));
    let size = (v.layout.width, v.layout.height);
    let root = v.id;
    for child in h
        .server
        .state
        .surfaces
        .values_mut()
        .filter(|r| r.layout.parent == Some(root))
    {
        child.layout.x = 10.0;
        child.layout.y = 10.0;
    }
    let c = h.server.state.surfaces.get_mut(cover).unwrap();
    c.layout.x = 10.0;
    c.layout.y = 10.0;
    c.window_origin = c
        .committed_window_geometry
        .map_or((10.0, 10.0), |g| (10.0 + g.x, 10.0 + g.y));
    c.layout.width = size.0;
    c.layout.height = size.1;
    c.layout.z = SurfaceStackKey::normal(1000);
}

pub(super) fn certify(
    h: &mut KeybindingHarness,
    opaque: bool,
) -> crate::occlusion::CoverageSnapshot {
    h.server.state.refresh_occlusion();
    let bridge = h.server.state.occlusion.bridge.clone();
    let mut exchange = bridge.0.lock().unwrap();
    let draws = exchange
        .scene
        .surfaces
        .iter()
        .map(|s| Draw {
            id: s.id,
            bounds: Bounds::new(
                f64::from(s.layout.x),
                f64::from(s.layout.y),
                f64::from(s.layout.width),
                f64::from(s.layout.height),
            ),
            opaque,
            rounded: false,
            chrome: Vec::new(),
            ready: true,
            sampled: true,
        })
        .collect::<Vec<_>>();
    let coverage = crate::occlusion::compute(&exchange.scene, &draws, exchange.revision);
    exchange.coverage = coverage.clone();
    drop(exchange);
    h.server.state.refresh_occlusion();
    coverage
}

pub(super) fn done(h: &mut KeybindingHarness, callback: u32) -> usize {
    h.sync()
        .iter()
        .filter(|(object, opcode, _)| *object == callback && *opcode == 0)
        .count()
}

#[test]
fn occlusion_wire_retains_callbacks_and_exposure_resumes_without_victim_commit() {
    let (mut h, victim, cover) = fixture();
    let callback = request(&mut h, victim.protocol_id());
    align(&mut h, &victim, &cover);
    let coverage = certify(&mut h, true);
    let id = h.server.state.surfaces[&victim].id;
    assert_eq!(coverage.surfaces[&id], TreeVisibility::Occluded);
    for _ in 0..3 {
        h.frame(Vec::new());
        assert_eq!(done(&mut h, callback), 0);
    }
    let commits = h.server.state.surfaces[&victim].commit_count;
    // A compositor move, no client commit and no replacement certificate.
    h.server.state.surfaces.get_mut(&cover).unwrap().layout.x += 1.0;
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 1);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 0);
    assert_eq!(h.server.state.surfaces[&victim].commit_count, commits);
    assert!(
        h.server
            .state
            .occlusion
            .bridge
            .0
            .lock()
            .unwrap()
            .counters
            .resumes
            > 0
    );
}

#[test]
fn occlusion_wire_content_only_commits_never_flap_or_leak() {
    let (mut h, first, cover) = fixture();
    let mut victims = vec![first];
    for _ in 0..3 {
        victims.push(map_named_test_toplevel(&mut h, "covered", "test.covered").3);
    }
    let callbacks = victims
        .iter()
        .map(|v| request(&mut h, v.protocol_id()))
        .collect::<Vec<_>>();
    for v in &victims {
        align(&mut h, v, &cover);
    }
    certify(&mut h, true);
    h.frame(Vec::new());
    let decisions = h.server.state.occlusion.decisions.clone();
    let revisions = h.server.state.occlusion.decision_revisions.clone();
    let rebuilds = h.server.state.occlusion.scene_rebuilds;
    for _ in 0..20 {
        // Commit after certification, before the pulse: previously this cleared
        // all decisions and leaked every covered callback before re-extraction.
        commit_test_buffer(&mut h, cover.protocol_id());
        h.dispatch_client();
        h.frame(Vec::new());
        let events = h.sync();
        assert!(
            !events
                .iter()
                .any(|(id, op, _)| callbacks.contains(id) && *op == 0)
        );
        assert_eq!(h.server.state.occlusion.decisions, decisions);
        assert_eq!(h.server.state.occlusion.decision_revisions, revisions);
        assert_eq!(
            h.server
                .state
                .occlusion
                .bridge
                .0
                .lock()
                .unwrap()
                .counters
                .resumes,
            0
        );
        certify(&mut h, true);
    }
    assert_eq!(
        h.server.state.occlusion.scene_rebuilds, rebuilds,
        "no full scene rebuild on content-only/idle dispatch"
    );
    // These are exactly the three decision leaves diffed for both props rows.
    // Stable decisions + decision revisions imply zero decision prop changes.
}

#[test]
fn occlusion_wire_callback_cap_completes_excess_and_retains_latest() {
    let (mut h, victim, cover) = fixture();
    align(&mut h, &victim, &cover);
    certify(&mut h, true);
    let mut callbacks = Vec::new();
    for _ in 0..70 {
        callbacks.push(request(&mut h, victim.protocol_id()));
    }
    let events = h.sync();
    let completed = events
        .iter()
        .filter(|(id, op, _)| callbacks.contains(id) && *op == 0)
        .map(|(id, _, _)| *id)
        .collect::<Vec<_>>();
    assert_eq!(completed, callbacks[..6]);
    assert_eq!(
        h.server
            .state
            .occlusion
            .bridge
            .0
            .lock()
            .unwrap()
            .counters
            .resumes,
        0
    );
    h.server.state.surfaces.get_mut(&cover).unwrap().layout.x += 1.0;
    h.frame(Vec::new());
    let events = h.sync();
    assert_eq!(
        events
            .iter()
            .filter(|(id, op, _)| callbacks.contains(id) && *op == 0)
            .count(),
        64
    );
    assert_eq!(
        h.server
            .state
            .occlusion
            .bridge
            .0
            .lock()
            .unwrap()
            .counters
            .resumes,
        1
    );
    h.frame(Vec::new());
    assert!(
        !h.sync()
            .iter()
            .any(|(id, op, _)| callbacks.contains(id) && *op == 0)
    );
}

#[test]
fn occlusion_wire_translucency_and_one_pixel_strip_do_not_withhold() {
    for opaque in [false, true] {
        let (mut h, victim, cover) = fixture();
        let callback = request(&mut h, victim.protocol_id());
        align(&mut h, &victim, &cover);
        h.server.state.backend.change_host_output_scale(2.5);
        certify(&mut h, true);
        h.frame(Vec::new());
        assert_eq!(
            done(&mut h, callback),
            0,
            "opaque baseline must withhold before testing exposure"
        );
        if opaque {
            h.server.state.surfaces.get_mut(&cover).unwrap().layout.x += 0.4;
        }
        certify(&mut h, opaque);
        h.frame(Vec::new());
        assert_eq!(done(&mut h, callback), 1);
    }
}

#[test]
fn occlusion_wire_region_only_commit_invalidates_cover() {
    let (mut h, victim, cover) = fixture();
    let callback = request(&mut h, victim.protocol_id());
    let region = h.allocate_object_id();
    send_request(&mut h.client, TEST_COMPOSITOR_ID, 1, &words(&[region]));
    send_request(&mut h.client, region, 1, &words(&[0, 0, 4096, 4096]));
    send_request(&mut h.client, cover.protocol_id(), 4, &words(&[region]));
    send_request(&mut h.client, cover.protocol_id(), 6, &[]);
    h.dispatch_client();
    align(&mut h, &victim, &cover);
    certify(&mut h, false);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 0);
    let seq = h.server.state.surfaces[&cover].content_seq;
    send_request(&mut h.client, cover.protocol_id(), 4, &words(&[0]));
    send_request(&mut h.client, cover.protocol_id(), 6, &[]);
    h.dispatch_client();
    assert_eq!(h.server.state.surfaces[&cover].content_seq, seq);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 1);
}

#[test]
fn occlusion_wire_bufferless_layer_and_subsurface_regions_cover_union() {
    for layer_role in [false, true] {
        let (mut h, parent, other) = fixture();
        let (victim, occluder) = if layer_role {
            let (layer, _) = map_test_layer_surface(&mut h, 0, TestLayerSpec::default());
            (
                parent.clone(),
                test_layer_record(&h, layer.surface).role.wl_surface().id(),
            )
        } else {
            (
                other,
                h.server
                    .state
                    .surfaces
                    .values()
                    .find(|r| r.role.wl_surface().id().protocol_id() == TEST_SUBSURFACE_SURFACE_ID)
                    .unwrap()
                    .role
                    .wl_surface()
                    .id(),
            )
        };
        let before = h.server.state.surfaces[&occluder].content_seq;
        let (width, height) = h.server.state.surfaces[&occluder]
            .buffer_dimensions
            .unwrap();
        let region = h.allocate_object_id();
        send_request(&mut h.client, TEST_COMPOSITOR_ID, 1, &words(&[region]));
        send_request(&mut h.client, region, 1, &words(&[0, 0, width / 2, height]));
        send_request(
            &mut h.client,
            region,
            1,
            &words(&[width / 2, 0, width - width / 2, height]),
        );
        send_request(&mut h.client, occluder.protocol_id(), 4, &words(&[region]));
        send_request(&mut h.client, occluder.protocol_id(), 6, &[]);
        if !layer_role {
            send_request(&mut h.client, parent.protocol_id(), 6, &[]);
        }
        h.dispatch_client();
        assert_eq!(
            h.server.state.surfaces[&occluder].content_seq, before,
            "region-setting transaction has no new buffer"
        );
        align(&mut h, &victim, &occluder);
        let id = h.server.state.surfaces[&occluder].id;
        let victim_id = h.server.state.surfaces[&victim].id;
        h.server.state.refresh_occlusion();
        assert_eq!(
            h.server
                .state
                .occlusion
                .bridge
                .0
                .lock()
                .unwrap()
                .scene
                .surfaces
                .iter()
                .find(|s| s.id == id)
                .unwrap()
                .opacity
                .operations
                .as_ref()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            certify(&mut h, false).surfaces[&victim_id],
            TreeVisibility::Occluded,
            "bufferless region union must remain usable for either role"
        );
    }
}

#[test]
fn occlusion_wire_stale_certificate_cannot_restore_withholding() {
    let (mut h, victim, cover) = fixture();
    let callback = request(&mut h, victim.protocol_id());
    align(&mut h, &victim, &cover);
    let stale = certify(&mut h, true);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 0);
    h.server.state.surfaces.get_mut(&cover).unwrap().layout.x += 1.0;
    h.server.state.refresh_occlusion();
    h.server.state.occlusion.bridge.0.lock().unwrap().coverage = stale;
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 1);
}

#[test]
fn occlusion_wire_exposed_popup_keeps_parent_callback_running() {
    let (mut h, victim, cover) = fixture();
    let callback = request(&mut h, victim.protocol_id());
    align(&mut h, &victim, &cover);
    certify(&mut h, true);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 0);
    let (popup, _) = map_test_popup(&mut h, None);
    h.server.state.surfaces.get_mut(&popup).unwrap().layout.z = SurfaceStackKey::normal(2000);
    certify(&mut h, true);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 1);
}

#[test]
fn occlusion_wire_layer_tree_withholds_and_resumes() {
    let (mut h, _, cover) = fixture();
    let (layer, _) = map_test_layer_surface(&mut h, 0, TestLayerSpec::default());
    let object = test_layer_record(&h, layer.surface).role.wl_surface().id();
    let callback = request(&mut h, layer.surface);
    align(&mut h, &object, &cover);
    h.server
        .state
        .surfaces
        .get_mut(&object)
        .unwrap()
        .layout
        .z
        .band = StackBand::Background;
    certify(&mut h, true);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 0);
    h.server.state.surfaces.get_mut(&cover).unwrap().layout.x += 1.0;
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 1);
}

#[test]
fn occlusion_wire_single_output_feedback_discards_only_evidenced_sequences() {
    let (mut h, victim, cover) = fixture();
    let (presentation, _) = bind_test_presentation(&mut h);
    let first = request_presentation_feedback(&mut h, presentation);
    commit_test_buffer(&mut h, TEST_TOPLEVEL_SURFACE_ID);
    h.dispatch_client();
    let seq = h.server.state.surfaces[&victim].content_seq;
    let later = request_presentation_feedback(&mut h, presentation);
    commit_test_buffer(&mut h, TEST_TOPLEVEL_SURFACE_ID);
    h.dispatch_client();
    align(&mut h, &victim, &cover);
    let coverage = certify(&mut h, true);
    let id = h.server.state.surfaces[&victim].id;
    let (frame, mut content) = test_frame_report(id, 1_000_000, seq, true);
    crate::frame_content::apply_presentation_coverage(&mut content, &coverage, 1);
    assert!(!content.surfaces[0].shown);
    h.server.state.frame_presented(frame, content);
    let events = h.sync();
    assert_eq!(feedback_opcodes(&events, first), [2]);
    assert!(feedback_opcodes(&events, later).is_empty());
    let (_, mut multi) = test_frame_report(id, 2_000_000, seq + 1, true);
    crate::frame_content::apply_presentation_coverage(&mut multi, &coverage, 2);
    assert!(multi.surfaces[0].shown, "documented multi-output limit");
}

#[test]
fn occlusion_wire_subsurface_callbacks_follow_family_and_remain_queued() {
    let (mut h, victim, cover) = fixture();
    let callback = request(&mut h, TEST_SUBSURFACE_SURFACE_ID);
    // Apply the synchronized child's callback transaction on the parent.
    send_request(&mut h.client, TEST_TOPLEVEL_SURFACE_ID, 6, &[]);
    h.dispatch_client();
    align(&mut h, &victim, &cover);
    certify(&mut h, true);
    for _ in 0..2 {
        h.frame(Vec::new());
        assert_eq!(done(&mut h, callback), 0);
    }
    h.server.state.surfaces.get_mut(&cover).unwrap().layout.x += 1.0;
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 1);
}

#[test]
fn occlusion_wire_cover_minimise_and_workspace_move_resume_victim() {
    for workspace in [false, true] {
        let (mut h, victim, cover) = fixture();
        let callback = request(&mut h, victim.protocol_id());
        align(&mut h, &victim, &cover);
        certify(&mut h, true);
        h.frame(Vec::new());
        assert_eq!(done(&mut h, callback), 0);
        if workspace {
            h.server
                .state
                .move_window_to_workspace(&cover, workspaces::WorkspaceTarget::Index(2))
                .unwrap();
        } else {
            let surface = h.server.state.surfaces[&cover].role.wl_surface().clone();
            h.server.state.minimize_toplevel(&surface);
        }
        h.frame(Vec::new());
        assert_eq!(done(&mut h, callback), 1);
    }
}

#[test]
fn occlusion_wire_unmapped_child_preserves_bootstrap_delivery() {
    let (mut h, victim, cover) = fixture();
    let first = request(&mut h, victim.protocol_id());
    align(&mut h, &victim, &cover);
    certify(&mut h, true);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, first), 0);
    let (child, _) = stage_test_synchronized_subsurface(&mut h, TEST_TOPLEVEL_SURFACE_ID);
    send_request(&mut h.client, child, 1, &words(&[0, 0, 0]));
    let child_callback = request(&mut h, child);
    send_request(&mut h.client, TEST_TOPLEVEL_SURFACE_ID, 6, &[]);
    h.dispatch_client();
    certify(&mut h, true);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, first), 1);
    // The same flush carried both; inspect committed callback storage to avoid
    // consuming the second event in a second socket drain.
    let child = h
        .server
        .state
        .surfaces
        .values()
        .find(|s| s.role.wl_surface().id().protocol_id() == child)
        .unwrap();
    compositor::with_states(child.role.wl_surface(), |states| {
        assert!(
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .is_empty(),
            "bootstrap callback {child_callback} drained"
        );
    });
}

#[test]
fn occlusion_refused_content_cannot_lend_opacity_to_retained_texture() {
    let (mut h, victim, cover) = fixture();
    let callback = request(&mut h, victim.protocol_id());
    let region = h.allocate_object_id();
    send_request(&mut h.client, TEST_COMPOSITOR_ID, 1, &words(&[region]));
    send_request(&mut h.client, region, 1, &words(&[0, 0, 4096, 4096]));
    send_request(&mut h.client, cover.protocol_id(), 4, &words(&[region]));
    send_request(&mut h.client, cover.protocol_id(), 6, &[]);
    h.dispatch_client();
    align(&mut h, &victim, &cover);
    certify(&mut h, false);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 0);
    let record = &h.server.state.surfaces[&cover];
    let surface = record.role.wl_surface().clone();
    let buffer = record.dmabuf_backing.as_ref().unwrap().buffer.clone();
    // Model the soft-refusal boundary (e.g. duplicating a DMA-BUF fd fails):
    // an applied replacement is consumed but no new content is published.
    compositor::with_states(&surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .buffer = Some(BufferAssignment::NewBuffer(buffer));
    });
    h.server.state.invalidate_committed_opacity(&surface);
    compositor::with_states(&surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .buffer = None;
    });
    h.server.state.capture_bufferless_opacity(&surface);
    certify(&mut h, false);
    h.frame(Vec::new());
    assert_eq!(done(&mut h, callback), 1);
}
