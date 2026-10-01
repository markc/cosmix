//! Client-owned drag artwork uses the ordinary surface import and tree renderer.
use super::*;

impl WaylandState {
    pub(super) fn start_drag_icon(&mut self, surface: WlSurface) {
        self.finish_drag_icon();
        let object = surface.id();
        let z = self.allocate_stack_key(StackBand::DragIcon);
        let generation = self.role_change_generation(&object);
        if let Some(record) = self.surfaces.get_mut(&object) {
            record.role = SurfaceRole::DragIcon {
                surface: surface.clone(),
                offset: (0, 0),
            };
            record.generation = generation;
            record.layout.z = z;
            record.layout.parent = None;
            record.parent_association_committed = true;
        } else {
            let id = SurfaceId(self.next_surface_id);
            self.next_surface_id = self.next_surface_id.saturating_add(1);
            let layout = SurfaceLayout {
                x: self.cursor_position.0 as f32,
                y: self.cursor_position.1 as f32,
                width: 1.0,
                height: 1.0,
                z,
                source: None,
                parent: None,
                transform: SurfaceTransform::Normal,
                visible: false,
                toplevel: None,
            };
            self.surfaces.insert(
                object.clone(),
                SurfaceRecord {
                    id,
                    generation,
                    role: SurfaceRole::DragIcon {
                        surface: surface.clone(),
                        offset: (0, 0),
                    },
                    mapped: false,
                    layout,
                    title: None,
                    app_id: None,
                    window_origin: (layout.x, layout.y),
                    configured_size: (1, 1),
                    commit_count: 0,
                    content_seq: 0,
                    shm_backing: None,
                    dmabuf_backing: None,
                    buffer_dimensions: None,
                    required_configure: None,
                    last_acked_configure: None,
                    last_acked_size: None,
                    decoration_object_bound: false,
                    committed_decoration: SceneDecorationMode::Unbound,
                    requested_maximized: false,
                    fullscreen_output: None,
                    requested_fullscreen: false,
                    fullscreen_restore_band: None,
                    committed_maximized: false,
                    committed_fullscreen: false,
                    normal_restore: None,
                    pending_window_state: None,
                    configured_window_states: Vec::new(),
                    minimized: false,
                    workspace: 0,
                    focused: false,
                    chrome_pointer: ChromePointerSceneState::default(),
                    committed_window_geometry: None,
                    committed_window_geometry_explicit: false,
                    grid_placement: None,
                    pending_popup_reposition: None,
                    parent_association_committed: true,
                    committed_input_region: None,
                    pixel_probe_logged: false,
                    logged_diagnostics: HashSet::new(),
                },
            );
            self.surface_objects.insert(id, object);
        }
        self.reposition_drag_icon();
        // A client may attach/commit the icon before start_drag assigns its role.
        // Import already-applied state, including children, without applying pending
        // synchronized transactions early. Frame callbacks stay on the surface tree.
        let mut applied = Vec::new();
        with_surface_tree_downward(
            &surface,
            (),
            |_, _, &()| TraversalAction::DoChildren(()),
            |child, _, &()| applied.push(child.clone()),
            |_, _, &()| true,
        );
        CompositorHandler::commit(self, &surface);
        for child in applied {
            if child != surface {
                CompositorHandler::commit(self, &child);
            }
        }
        self.refresh_committed_surface_stack(&surface);
        self.recompute_effective_visibility();
        CompositorHandler::transaction_applied(self);
    }

    pub(super) fn apply_drag_icon_offset(
        &mut self,
        surface: &WlSurface,
        delta: Option<(i32, i32)>,
    ) {
        let Some(delta) = delta else {
            return;
        };
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return;
        };
        let SurfaceRole::DragIcon { offset, .. } = &mut record.role else {
            return;
        };
        // attach offsets (v1-4) and wl_surface.offset (v5+) are cumulative,
        // in logical surface coordinates; buffer_scale/viewport affect size only.
        offset.0 = offset.0.saturating_add(delta.0);
        offset.1 = offset.1.saturating_add(delta.1);
        self.reposition_drag_icon();
    }

    pub(super) fn reposition_drag_icon(&mut self) {
        let Some(record) = self
            .surfaces
            .values_mut()
            .find(|record| matches!(record.role, SurfaceRole::DragIcon { .. }))
        else {
            return;
        };
        let SurfaceRole::DragIcon { offset, .. } = &record.role else {
            unreachable!();
        };
        let x = self.cursor_position.0 as f32 + offset.0 as f32;
        let y = self.cursor_position.1 as f32 + offset.1 as f32;
        let delta = (x - record.layout.x, y - record.layout.y);
        if delta == (0.0, 0.0) {
            return;
        }
        record.layout.x = x;
        record.layout.y = y;
        record.window_origin = (x, y);
        let id = record.id;
        if record.mapped {
            self.events.push(ProtocolEvent::SurfaceRelayout {
                id,
                scene: record.scene_snapshot(),
            });
        }
        self.shift_surface_descendants(id, delta);
    }

    pub(super) fn finish_drag_icon(&mut self) {
        let Some(surface) = self
            .surfaces
            .values()
            .find_map(|record| match &record.role {
                SurfaceRole::DragIcon { surface, .. } => Some(surface.clone()),
                _ => None,
            })
        else {
            return;
        };
        // Smithay invokes dropped() with the pointer/touch grab locked. Artwork
        // cannot own focus, so retire it without the ordinary role-deactivation
        // path, whose focus reconciliation would re-enter that lock.
        let generation = self.role_change_generation(&surface.id());
        if let Some(record) = self.surfaces.get_mut(&surface.id()) {
            record.role = SurfaceRole::Dormant(surface.clone());
            record.generation = generation;
        }
        // Remove the whole artwork, not only the root. Retire protocol backing
        // ownership now; renderer-owned DMA-BUF uses retain their existing fence.
        let members = self
            .surfaces
            .values()
            .filter(|record| record.layout.z.band == StackBand::DragIcon)
            .map(|record| record.role.wl_surface().clone())
            .collect::<Vec<_>>();
        for member in members {
            let Some(record) = self.surfaces.get_mut(&member.id()) else {
                continue;
            };
            let bytes = record
                .shm_backing
                .take()
                .map_or(0, |backing| backing.rgba.len());
            let token = record
                .dmabuf_backing
                .take()
                .map(|backing| backing.retention_token);
            record.buffer_dimensions = None;
            let mapped = mem::replace(&mut record.mapped, false);
            let visible = mem::replace(&mut record.layout.visible, false);
            let id = record.id;
            if visible {
                self.backend.output_leave(&member);
            }
            self.release_shm_bytes(&member, bytes);
            if let Some(token) = token {
                self.release_buffer_token(token);
            }
            if mapped {
                self.events.push(ProtocolEvent::SurfaceUnmapped { id });
            }
            #[cfg(feature = "bus")]
            self.mark_surface_unmapped(&member);
        }
    }

    pub(super) fn inactive_drag_icon_member(&self, surface: &WlSurface) -> bool {
        self.surfaces.get(&surface.id()).is_some_and(|record| {
            record.layout.z.band == StackBand::DragIcon
                && record_root_id(&self.surfaces, &self.surface_objects, surface.id())
                    .and_then(|id| self.surface_objects.get(&id))
                    .and_then(|object| self.surfaces.get(object))
                    .is_none_or(|root| !matches!(root.role, SurfaceRole::DragIcon { .. }))
        })
    }
}
