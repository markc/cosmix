use super::*;

/// Publish the removal a subsurface re-create owes the renderer.
///
/// Only `new_subsurface` needs this, and only because we learn a
/// `wl_subsurface` died a whole dispatch late. Smithay unsets the parent in the
/// destructor, but *we* find out through `reconcile_subsurface_roles`, which
/// runs once per cycle after `dispatch_clients`. So a client that destroys its
/// `wl_subsurface` and calls `get_subsurface` again in the **same dispatch**
/// reaches the branch below with a record that is still `Subsurface`, still
/// `mapped`, and still drawn by the renderer. Re-creating the role unmaps it —
/// the record keeps its `buffer_dimensions`, but the entity described a surface
/// with a different parent and position, and the record cannot be called mapped
/// again until the client's next buffer commit publishes a complete upsert. The
/// renderer has to be told, or it goes on drawing the old entity. Reconciliation
/// will not say so either: by the time it runs, the parent matches again and it
/// sees nothing stale.
///
/// The xdg handlers need no equivalent. A duplicate role object never reaches
/// them: vendored smithay refuses the second `xdg_wm_base.get_xdg_surface` on a
/// role-bearing `wl_surface`, and refuses a second role object on one
/// `xdg_surface`, both *before* `data_init.init`. And the role destructors run
/// *synchronously*, so a legitimate re-role always finds the record already
/// `Dormant` — non-presentable, its removal already published by
/// `deactivate_surface_role`, and its upserts already suppressed by
/// `push_surface_upsert`. There is no entity left to strand.
///
/// Called after the frames generated earlier in the same dispatch, because
/// per-surface compaction is last-state-wins and a later `SurfaceUpserted`
/// replaces a queued tombstone. A `SurfaceRelayout` cannot: `push_with_limit`
/// deliberately preserves a queued removal against one.
fn publish_reroll_unmap(events: &mut Vec<ProtocolEvent>, id: SurfaceId, was_mapped: bool) {
    if was_mapped {
        events.push(ProtocolEvent::SurfaceUnmapped { id });
    }
}

impl BufferHandler for WaylandState {
    fn buffer_destroyed(&mut self, buffer: &wl_buffer::WlBuffer) {
        if let Some(buffer_id) = self.dmabuf_buffer_ids.remove(&buffer.id()) {
            self.events
                .push(ProtocolEvent::DmabufBufferDestroyed { buffer_id });
        }
        let captures = self
            .capture_frames
            .iter()
            .filter_map(|(id, frame)| {
                frame
                    .buffer
                    .as_ref()
                    .is_some_and(|candidate| candidate.id() == buffer.id())
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in captures {
            self.fail_capture(id);
        }
    }
}

impl CompositorHandler for WaylandState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<WaylandClientState>()
            .expect("all clients are registered with WaylandClientState")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        compositor::add_pre_commit_hook::<WaylandState, _>(surface, |state, _, surface| {
            state.prepare_scene_commit(surface);
            state.prepare_acquire_gate(surface);
        });
        self.surface_count = self.surface_count.saturating_add(1);
        let _ = Self::with_client_state(surface, |client_state| {
            client_state.surface_count.fetch_add(1, Ordering::Relaxed) + 1
        });
        self.subsurface_topology
            .insert(surface.id(), SubsurfaceTopology::default());
        let scale = self.backend.output_scale();
        compositor::with_states(surface, |states| {
            fractional_scale::with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }

    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        let former_root = self.toplevel_root_for_surface(surface);
        self.attach_subsurface_topology(surface, parent);

        let parent_record = self.surfaces.get(&parent.id());
        let parent_id = parent_record.map(|record| record.id);
        let parent_layout = parent_record.map(|record| record.layout);
        let (x, y, z) = parent_layout.map_or_else(
            || (0.0, 0.0, self.allocate_stack_key(StackBand::Normal)),
            |layout| {
                (
                    layout.x,
                    layout.y,
                    SurfaceStackKey {
                        tree_index: layout.z.tree_index.saturating_add(1),
                        ..layout.z
                    },
                )
            },
        );
        let layout = SurfaceLayout {
            x,
            y,
            width: 1.0,
            height: 1.0,
            z,
            source: None,
            parent: parent_id,
            transform: SurfaceTransform::Normal,
            visible: false,
            toplevel: None,
        };
        // Whether the renderer can be holding an entity for this surface. A
        // re-role unmaps it below, and a transition to unmapped is only real to
        // the renderer if it is published — see the removal at the end.
        let was_mapped = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| record.mapped);
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(surface);
        let id = if let Some(record) = self.surfaces.get_mut(&surface.id()) {
            let id = record.id;
            record.role = SurfaceRole::Subsurface {
                surface: surface.clone(),
                parent: parent.clone(),
            };
            // Not `buffer_dimensions.is_some()`, even though the buffer may
            // still be applied. Adding a subsurface is double-buffered on the
            // *parent*: the association is not current until the parent
            // commits, so claiming presence here would be early. Nothing here
            // could publish that presence anyway —
            // `refresh_subsurface_position` emits a relayout, and a relayout
            // for an id the renderer does not hold is a no-op.
            //
            // So the surface is unmapped now, absent from every roster and
            // `Gone` to dirty recovery. `commit_subsurface_stack` is what
            // brings it back, on the parent commit that makes the association
            // current — from retained content if the client sends no new
            // buffer, which the protocol does not oblige it to.
            record.mapped = false;
            record.layout.x = x;
            record.layout.y = y;
            record.layout.z = z;
            record.layout.parent = parent_id;
            record.layout.toplevel = None;
            record.title = None;
            record.app_id = None;
            // Deliberately *not* `record.layout.visible = false`. The
            // `recompute_effective_visibility` below acts only on a *change*,
            // so clearing the flag by hand hides the true -> false transition
            // from it: no `SurfaceRelayout` is published for this surface, it
            // keeps the `wl_output.enter` it was given, and
            // `clear_focus_for_surface` never runs — so a surface the renderer
            // has been told to drop can still hold keyboard and pointer focus.
            // Left alone, the recompute sees the change and does all three.
            record.window_origin = (x, y);
            record.required_configure = None;
            record.last_acked_configure = None;
            record.last_acked_size = None;
            record.decoration_object_bound = false;
            record.committed_decoration = SceneDecorationMode::Unbound;
            record.requested_maximized = false;
            record.committed_maximized = false;
            record.normal_restore = None;
            record.pending_window_state = None;
            record.configured_window_states.clear();
            record.minimized = false;
            record.focused = false;
            record.committed_window_geometry = None;
            record.committed_window_geometry_explicit = false;
            record.pending_popup_reposition = None;
            record.parent_association_committed = false;
            id
        } else {
            let id = SurfaceId(self.next_surface_id);
            self.next_surface_id = self.next_surface_id.saturating_add(1);
            self.surfaces.insert(
                surface.id(),
                SurfaceRecord {
                    id,
                    role: SurfaceRole::Subsurface {
                        surface: surface.clone(),
                        parent: parent.clone(),
                    },
                    mapped: false,
                    layout,
                    title: None,
                    app_id: None,
                    window_origin: (x, y),
                    configured_size: (1, 1),
                    commit_count: 0,
                    shm_backing: None,
                    dmabuf_backing: None,
                    buffer_dimensions: None,
                    required_configure: None,
                    last_acked_configure: None,
                    last_acked_size: None,
                    decoration_object_bound: false,
                    committed_decoration: SceneDecorationMode::Unbound,
                    requested_maximized: false,
                    committed_maximized: false,
                    normal_restore: None,
                    pending_window_state: None,
                    configured_window_states: Vec::new(),
                    minimized: false,
                    focused: false,
                    chrome_pointer: ChromePointerSceneState::default(),
                    committed_window_geometry: None,
                    committed_window_geometry_explicit: false,
                    pending_popup_reposition: None,
                    parent_association_committed: false,
                    committed_input_region: None,
                    pixel_probe_logged: false,
                    logged_diagnostics: HashSet::new(),
                },
            );
            self.surface_objects.insert(id, surface.id());
            id
        };
        self.committed_surface_stacks
            .insert(surface.id(), vec![surface.id()]);
        self.refresh_subsurface_position(surface);
        self.recompute_effective_visibility();
        if let Some(former_root) = former_root {
            self.refresh_toplevel_window_geometry(&former_root);
        }
        publish_reroll_unmap(&mut self.events, id, was_mapped);
        tracing::info!(
            surface_id = id.0,
            surface = ?surface.id(),
            parent = ?parent.id(),
            "new wl_subsurface tracked"
        );
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.committed_surfaces.insert(surface.id());
        // Smithay invokes this handler only when a transaction is applied.
        // Synchronized-child commits remain counted while cached under their
        // parent, then reset here when the parent makes them current.
        self.pointer_hit_test_transaction_applying = true;
        self.damage_requests_since_apply.remove(&surface.id());
        self.popup_manager.commit(surface);
        let scene_commit = current_scene_commit_state(surface);
        let mapped_before = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| record.mapped);
        let hit_test_geometry_before = self.surfaces.get(&surface.id()).map(|record| {
            (
                record.layout.x.to_bits(),
                record.layout.y.to_bits(),
                record.layout.width.to_bits(),
                record.layout.height.to_bits(),
                record.layout.z,
            )
        });
        self.refresh_subsurface_position(surface);
        let subsurface_remapped = self.commit_subsurface_stack(surface);
        // A committed roleless surface may become a cursor later: Wayland
        // permits attach + commit before wl_pointer.set_cursor assigns the
        // cursor role. Keep its buffer and frame callbacks in Smithay's current
        // cache until a role tells us how to consume them. Damage has no
        // roleless consumer and the first cursor import necessarily builds a
        // complete backing, so retaining it would only let repeated commits
        // grow Smithay's merged damage vector without bound.
        if compositor::get_role(surface).is_none() && !self.surfaces.contains_key(&surface.id()) {
            compositor::with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .damage
                    .clear();
            });
            return;
        }
        let input_region_changed = self.refresh_committed_input_region(surface);
        if subsurface_remapped {
            self.recompute_effective_visibility();
        }
        if input_region_changed || subsurface_remapped {
            self.mark_pointer_hit_test_dirty();
        }
        let (buffer, mut damage, buffer_scale, buffer_transform, buffer_delta) =
            compositor::with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let current = attributes.current();
                let buffer = current.buffer.take();
                let damage = mem::take(&mut current.damage);
                let buffer_delta = current.buffer_delta.take().map(|delta| (delta.x, delta.y));
                (
                    buffer,
                    damage,
                    current.buffer_scale,
                    current.buffer_transform,
                    buffer_delta,
                )
            });
        let force_full_damage = damage.len() > MAX_DAMAGE_RECTS;
        if force_full_damage {
            tracing::warn!(
                surface = ?surface.id(),
                rectangles = damage.len(),
                limit = MAX_DAMAGE_RECTS,
                "collapsed excessive damage list to full-surface damage"
            );
            damage.clear();
        }
        if self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| matches!(record.role, SurfaceRole::Toplevel(_)))
            && let Err(error) = validate_toplevel_constraints(surface_size_constraints(surface))
        {
            if let Some(toplevel) = self
                .surfaces
                .get(&surface.id())
                .and_then(|record| record.role.toplevel())
            {
                toplevel
                    .xdg_toplevel()
                    .post_error(xdg_toplevel::Error::InvalidSize, error);
            }
            return;
        }

        let invalid_popup = matches!(buffer.as_ref(), Some(BufferAssignment::NewBuffer(_)))
            .then(|| {
                self.surfaces
                    .get(&surface.id())
                    .and_then(|record| match &record.role {
                        SurfaceRole::Popup(popup) => popup
                            .get_parent_surface()
                            .filter(|parent| {
                                self.surfaces
                                    .get(&parent.id())
                                    .is_some_and(|parent_record| {
                                        parent_record.mapped
                                            && parent_record.layout.visible
                                            && !matches!(
                                                parent_record.role,
                                                SurfaceRole::Dormant(_)
                                            )
                                    })
                            })
                            .is_none()
                            .then_some(popup.clone()),
                        SurfaceRole::Toplevel(_)
                        | SurfaceRole::Layer(_)
                        | SurfaceRole::LockSurface(_)
                        | SurfaceRole::Subsurface { .. }
                        | SurfaceRole::Dormant(_) => None,
                    })
            })
            .flatten();
        if let Some(popup) = invalid_popup {
            if let Some(BufferAssignment::NewBuffer(buffer)) = buffer {
                self.retire_buffer_immediately(buffer);
            }
            tracing::warn!(
                surface = ?surface.id(),
                "dismissed popup commit because its parent is not mapped"
            );
            popup.send_popup_done();
            return;
        }
        self.apply_acked_popup_reposition(surface);

        if self.layer_role_is_closed(surface) {
            if let Some(BufferAssignment::NewBuffer(buffer)) = buffer {
                self.retire_buffer_immediately(buffer);
            }
            return;
        }
        let layer_restarts_configure_cycle =
            self.surfaces.get(&surface.id()).is_some_and(|record| {
                matches!(record.role, SurfaceRole::Layer(_))
                    && record.required_configure.is_some()
                    && matches!(buffer, Some(BufferAssignment::Removed))
            });
        if layer_restarts_configure_cycle {
            self.dismiss_popup_descendants(surface);
            self.unmap_layer_from_output(surface);
            self.reset_configure_sequence(surface);
        } else if self.layer_output_for_surface(surface).is_some() {
            self.sync_layer_stack_band(surface);
            if let Err(error) = self.validate_layer_surface_state(surface) {
                if let Some(BufferAssignment::NewBuffer(buffer)) = buffer {
                    self.retire_buffer_immediately(buffer);
                }
                self.unmap_layer_from_output(surface);
                self.post_invalid_layer_state(surface, error);
                return;
            }
            self.ensure_layer_mapped_and_arranged(surface);
        }

        let configure_target =
            self.surfaces
                .get(&surface.id())
                .and_then(|record| match &record.role {
                    SurfaceRole::Toplevel(toplevel) => {
                        Some(ConfigureTarget::Toplevel(toplevel.clone()))
                    }
                    SurfaceRole::Popup(popup) => Some(ConfigureTarget::Popup(popup.clone())),
                    SurfaceRole::Layer(role) => Some(ConfigureTarget::Layer(role.surface.clone())),
                    SurfaceRole::LockSurface(role) => {
                        Some(ConfigureTarget::Lock(role.surface.clone()))
                    }
                    SurfaceRole::Subsurface { .. } | SurfaceRole::Dormant(_) => None,
                });
        if configure_target.is_some() && !layer_restarts_configure_cycle {
            let initial_sent = self
                .surfaces
                .get(&surface.id())
                .is_some_and(|record| record.required_configure.is_some());
            if !initial_sent {
                if let Some(BufferAssignment::NewBuffer(buffer)) = buffer.as_ref() {
                    self.retire_buffer_immediately(buffer.clone());
                    let _ = self.ensure_current_configure_sequence_is_acked(surface);
                    return;
                }
                let _ = self.send_initial_configure(surface);
                return;
            }
            if matches!(buffer.as_ref(), Some(BufferAssignment::NewBuffer(_)))
                && !self.ensure_current_configure_sequence_is_acked(surface)
            {
                if let Some(BufferAssignment::NewBuffer(buffer)) = buffer.as_ref() {
                    self.retire_buffer_immediately(buffer.clone());
                }
                return;
            }
        }
        self.apply_acked_layer_state(surface);
        self.apply_acked_lock_state(surface);
        self.apply_committed_toplevel_state(surface, scene_commit);

        if self.commit_cursor_surface(
            surface,
            CursorCommit {
                buffer: buffer.as_ref(),
                damage: &damage,
                force_full_damage,
                buffer_scale,
                buffer_transform,
                buffer_delta,
            },
        ) {
            return;
        }

        if !self.surfaces.contains_key(&surface.id()) {
            if let Some(BufferAssignment::NewBuffer(buffer)) = buffer {
                // Drag-icon and otherwise unassigned surfaces do not
                // participate in the window scene yet. Cursor surfaces have
                // already taken their separate state path above.
                self.retire_untracked_surface_buffer(surface, buffer);
            }
            return;
        }
        let publish_surface = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| self.surface_is_renderer_presentable(record));

        match buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                self.commit_new_buffer(
                    surface,
                    buffer,
                    SurfaceBufferCommit {
                        damage,
                        force_full_damage,
                        buffer_scale,
                        buffer_transform,
                        window_geometry_changed: scene_commit.window_geometry_changed,
                    },
                );
            }
            Some(BufferAssignment::Removed) => {
                let was_mapped = self
                    .surfaces
                    .get(&surface.id())
                    .is_some_and(|record| record.mapped);
                #[cfg(feature = "bus")]
                self.mark_surface_unmapped(surface);
                let (unmapped_id, released_shm_bytes, released_dmabuf_token) = {
                    let record = self
                        .surfaces
                        .get_mut(&surface.id())
                        .expect("surface existence checked above");
                    let released_shm_bytes = record
                        .shm_backing
                        .take()
                        .map_or(0, |backing| backing.rgba.len());
                    let released_dmabuf_token = record
                        .dmabuf_backing
                        .take()
                        .map(|backing| backing.retention_token);
                    record.buffer_dimensions = None;
                    record.minimized = false;
                    if was_mapped {
                        record.mapped = false;
                        (Some(record.id), released_shm_bytes, released_dmabuf_token)
                    } else {
                        (None, released_shm_bytes, released_dmabuf_token)
                    }
                };
                if released_shm_bytes > 0 {
                    self.release_shm_bytes(surface, released_shm_bytes);
                }
                if let Some(released_dmabuf_token) = released_dmabuf_token {
                    self.release_buffer_token(released_dmabuf_token);
                }
                if let Some(id) = unmapped_id {
                    self.close_foreign_toplevel(surface);
                    self.cancel_chrome_pointer_grab_for_surface(surface, false);
                    self.reset_chrome_pointer_tracking(&surface.id());
                    self.minimized_toplevels
                        .retain(|object| *object != surface.id());
                    self.reset_configure_sequence(surface);
                    if interactive_surface(self.interactive_pointer.as_ref())
                        .is_some_and(|interactive| interactive == surface)
                    {
                        self.interactive_pointer = None;
                    }
                    self.recompute_effective_visibility();
                    self.events.push(ProtocolEvent::SurfaceUnmapped { id });
                    self.refresh_chrome_pointer_after_scene_change();
                }
            }
            None => {
                let dimensions = self
                    .surfaces
                    .get(&surface.id())
                    .and_then(|record| record.buffer_dimensions);
                if let Some((width, height)) = dimensions {
                    match surface_presentation(
                        surface,
                        width,
                        height,
                        buffer_scale,
                        buffer_transform,
                    ) {
                        Ok(presentation) => {
                            let window_geometry = self.committed_toplevel_window_geometry(
                                surface,
                                presentation.size,
                                scene_commit.window_geometry_changed,
                            );
                            let record = self
                                .surfaces
                                .get_mut(&surface.id())
                                .expect("surface existence checked above");
                            let old_origin = (record.layout.x, record.layout.y);
                            if let Some(window_geometry) = window_geometry {
                                record.layout.x = record.window_origin.0 - window_geometry.x;
                                record.layout.y = record.window_origin.1 - window_geometry.y;
                                record.committed_window_geometry = Some(window_geometry);
                            }
                            record.layout.width = presentation.size.0;
                            record.layout.height = presentation.size.1;
                            record.layout.source = presentation.source;
                            record.layout.transform = presentation.transform;
                            sync_toplevel_scene_state(record);
                            let delta = (
                                record.layout.x - old_origin.0,
                                record.layout.y - old_origin.1,
                            );
                            let record_id = record.id;
                            if publish_surface {
                                self.events.push(ProtocolEvent::SurfaceRelayout {
                                    id: record.id,
                                    scene: record.scene_snapshot(),
                                });
                            }
                            if delta != (0.0, 0.0) {
                                self.shift_surface_descendants(record_id, delta);
                            }
                        }
                        Err(error) => {
                            let surface_id = self
                                .surfaces
                                .get(&surface.id())
                                .map(|record| record.id)
                                .expect("surface existence checked above");
                            self.reject_invalid_presentation(
                                surface,
                                surface_id,
                                0,
                                "viewport-only",
                                error,
                            );
                        }
                    }
                }
            }
        }
        if scene_commit.refresh_ancestor_window_geometry {
            self.refresh_ancestor_window_geometry(surface);
        }
        self.recompute_effective_visibility();
        let mapped_after = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| record.mapped);
        let hit_test_geometry_after = self.surfaces.get(&surface.id()).map(|record| {
            (
                record.layout.x.to_bits(),
                record.layout.y.to_bits(),
                record.layout.width.to_bits(),
                record.layout.height.to_bits(),
                record.layout.z,
            )
        });
        if mapped_before != mapped_after || hit_test_geometry_before != hit_test_geometry_after {
            self.mark_pointer_hit_test_dirty();
        }
        let is_layer = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| matches!(record.role, SurfaceRole::Layer(_)));
        let is_lock = self
            .surfaces
            .get(&surface.id())
            .is_some_and(|record| matches!(record.role, SurfaceRole::LockSurface(_)));
        let layer_focus_policy_changed = self.sync_committed_layer_focus_policy(surface);
        if is_layer && (mapped_before != mapped_after || layer_focus_policy_changed) {
            self.arbitrate_keyboard_focus(None, false, false);
        }
        if is_lock && mapped_before != mapped_after {
            self.arbitrate_keyboard_focus(Some(surface.clone()), false, true);
        }
        self.sync_foreign_toplevel(surface);
    }

    fn transaction_applied(&mut self) {
        self.pointer_hit_test_transaction_applying = false;
        self.reconcile_deferred_pointer_hit_test();
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        let former_root = self.toplevel_root_for_surface(surface);
        self.buffer_history_surfaces.remove(&surface.id());
        self.attach_history_surfaces.remove(&surface.id());
        self.committed_surfaces.remove(&surface.id());
        self.warned_unsupported_surfaces.remove(&surface.id());
        self.damage_requests_since_apply.remove(&surface.id());
        self.remove_subsurface_topology(surface);
        self.destroy_cursor_surface(surface);
        self.destroy_surface_record(surface);
        if let Some(former_root) = former_root {
            self.refresh_toplevel_window_geometry(&former_root);
        }
        self.surface_count = self.surface_count.saturating_sub(1);
        let _ = Self::with_client_state(surface, |client_state| {
            let _ = client_state.surface_count.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |count| Some(count.saturating_sub(1)),
            );
        });
        // Must stay LAST. Releasing a gate wakes the client, which can synchronously
        // apply a fused sibling's commit; that commit is charged against the shm and
        // dmabuf budgets this surface's teardown has only just refunded above.
        self.destroy_surface_acquire_gates(surface);
    }
}

impl WlrLayerShellHandler for WaylandState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<wl_output_protocol::WlOutput>,
        layer: WlrLayer,
        namespace: String,
    ) {
        let surface_object = surface.wl_surface().id();
        let desktop_surface = DesktopLayerSurface::new(surface.clone(), namespace);
        let output_binding = match output {
            Some(output) => self
                .backend
                .output_from_resource(&output)
                .map(LayerOutputBinding::Explicit)
                .unwrap_or(LayerOutputBinding::Closed),
            None => self
                .backend
                .default_output()
                .map(LayerOutputBinding::Default)
                .unwrap_or(LayerOutputBinding::Closed),
        };
        let mapped_output = output_binding.output().cloned();
        let output_origin = mapped_output
            .as_ref()
            .map(Output::current_location)
            .unwrap_or_default();

        let z = self.allocate_stack_key(StackBand::for_layer(layer));
        let layout = SurfaceLayout {
            x: output_origin.x as f32,
            y: output_origin.y as f32,
            width: 1.0,
            height: 1.0,
            z,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: false,
            toplevel: None,
        };
        let role = LayerRole {
            surface: desktop_surface.clone(),
            output: output_binding,
            initial_layer: layer,
            committed_layer: layer,
            committed_keyboard_interactivity: KeyboardInteractivity::None,
        };
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(surface.wl_surface());
        let id = if let Some(record) = self.surfaces.get_mut(&surface_object) {
            let id = record.id;
            record.role = SurfaceRole::Layer(role);
            record.mapped = false;
            record.layout = layout;
            record.title = None;
            record.app_id = None;
            record.window_origin = (layout.x, layout.y);
            record.configured_size = (1, 1);
            record.required_configure = None;
            record.last_acked_configure = None;
            record.last_acked_size = None;
            record.decoration_object_bound = false;
            record.committed_decoration = SceneDecorationMode::Unbound;
            record.requested_maximized = false;
            record.committed_maximized = false;
            record.normal_restore = None;
            record.pending_window_state = None;
            record.configured_window_states.clear();
            record.minimized = false;
            record.focused = false;
            record.committed_window_geometry = None;
            record.committed_window_geometry_explicit = false;
            record.pending_popup_reposition = None;
            record.parent_association_committed = true;
            id
        } else {
            let id = SurfaceId(self.next_surface_id);
            self.next_surface_id = self.next_surface_id.saturating_add(1);
            self.surfaces.insert(
                surface_object.clone(),
                SurfaceRecord {
                    id,
                    role: SurfaceRole::Layer(role),
                    mapped: false,
                    layout,
                    title: None,
                    app_id: None,
                    window_origin: (layout.x, layout.y),
                    configured_size: (1, 1),
                    commit_count: 0,
                    shm_backing: None,
                    dmabuf_backing: None,
                    buffer_dimensions: None,
                    required_configure: None,
                    last_acked_configure: None,
                    last_acked_size: None,
                    decoration_object_bound: false,
                    committed_decoration: SceneDecorationMode::Unbound,
                    requested_maximized: false,
                    committed_maximized: false,
                    normal_restore: None,
                    pending_window_state: None,
                    configured_window_states: Vec::new(),
                    minimized: false,
                    focused: false,
                    chrome_pointer: ChromePointerSceneState::default(),
                    committed_window_geometry: None,
                    committed_window_geometry_explicit: false,
                    pending_popup_reposition: None,
                    parent_association_committed: true,
                    committed_input_region: None,
                    pixel_probe_logged: false,
                    logged_diagnostics: HashSet::new(),
                },
            );
            self.surface_objects.insert(id, surface_object.clone());
            id
        };
        self.committed_surface_stacks
            .insert(surface_object.clone(), vec![surface_object.clone()]);

        // Layer state is double-buffered and the requests that initialise it
        // follow get_layer_surface. Defer LayerMap insertion until the first
        // wl_surface commit so validation sees that committed state.
        let mapped = mapped_output.is_some();
        if !mapped {
            if let Some(record) = self.surfaces.get_mut(&surface_object)
                && let SurfaceRole::Layer(role) = &mut record.role
            {
                role.output = LayerOutputBinding::Closed;
            }
            surface.send_close();
        }

        tracing::info!(
            surface_id = id.0,
            surface = ?surface_object,
            ?layer,
            mapped,
            "new layer-shell surface staged"
        );
    }

    fn new_popup(&mut self, parent: WlrLayerSurface, popup: PopupSurface) {
        let popup_object = popup.wl_surface().id();
        let parent_is_layer = self
            .surfaces
            .get(&parent.wl_surface().id())
            .is_some_and(|record| matches!(record.role, SurfaceRole::Layer(_)));
        let Some(positioner) = self.pending_parentless_popups.remove(&popup_object) else {
            tracing::warn!(
                popup = ?popup_object,
                "dismissed layer popup without deferred xdg popup state"
            );
            popup.send_popup_done();
            return;
        };
        if !parent_is_layer {
            popup.send_popup_done();
            return;
        }
        <Self as XdgShellHandler>::new_popup(self, popup, positioner);
    }

    fn ack_configure(&mut self, surface: WlSurface, configure: LayerSurfaceConfigure) {
        let configured_size = configure
            .state
            .size
            .map(|size| (size.w.max(1), size.h.max(1)))
            .unwrap_or((1, 1));
        let gate = if let Some(record) = self.surfaces.get_mut(&surface.id())
            && matches!(record.role, SurfaceRole::Layer(_))
        {
            record.last_acked_configure = Some(configure.serial);
            record.last_acked_size = Some(configured_size);
            Some((record.id, record.required_configure))
        } else {
            None
        };
        let smithay_state = compositor::with_states(&surface, |states| {
            let attributes = states
                .data_map
                .get::<LayerSurfaceData>()
                .expect("layer ack owns protocol attributes")
                .lock()
                .expect("layer attributes lock");
            (
                attributes.configured,
                attributes.configure_serial,
                attributes.initial_configure_sent,
            )
        });
        if let Some((surface_id, required_configure)) = gate {
            debug_assert!(smithay_state.0);
            debug_assert_eq!(smithay_state.1, Some(configure.serial));
            tracing::debug!(
                surface_id = surface_id.0,
                surface = ?surface.id(),
                serial = ?configure.serial,
                ?required_configure,
                smithay_configured = smithay_state.0,
                smithay_acked = ?smithay_state.1,
                smithay_initial_configure_sent = smithay_state.2,
                "acknowledged layer configure in Smithay and common gate"
            );
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        self.unmap_layer_from_output(surface.wl_surface());
        self.deactivate_surface_role(surface.wl_surface());
    }
}

impl XdgShellHandler for WaylandState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.retire_unadopted_roleless_buffer(surface.wl_surface());
        let surface_object = surface.wl_surface().id();
        if let Some(xdg_surface) = self.dispatching_xdg_surface.clone() {
            self.xdg_surface_objects
                .insert(xdg_surface, surface_object.clone());
        }

        let cascade = self.next_layout_index % 6;
        self.next_layout_index = self.next_layout_index.saturating_add(1);
        let z = self.allocate_stack_key(StackBand::Normal);
        let usable = self.usable_output_rect();
        let x = usable.x + CASCADE_ORIGIN + cascade as f32 * CASCADE_STEP;
        let y = usable.y + CASCADE_ORIGIN + cascade as f32 * CASCADE_STEP;
        let configured_size = sensible_toplevel_size(usable, x, y);
        let layout = SurfaceLayout {
            x,
            y,
            width: configured_size.0 as f32,
            height: configured_size.1 as f32,
            z,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: false,
            toplevel: None,
        };
        set_toplevel_configuration(&surface, configured_size);
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(surface.wl_surface());
        let id = if let Some(record) = self.surfaces.get_mut(&surface_object) {
            let id = record.id;
            record.role = SurfaceRole::Toplevel(surface);
            record.mapped = false;
            record.layout = layout;
            record.title = None;
            record.app_id = None;
            record.window_origin = (layout.x, layout.y);
            record.configured_size = configured_size;
            record.required_configure = None;
            record.last_acked_configure = None;
            record.last_acked_size = None;
            record.decoration_object_bound = false;
            record.committed_decoration = SceneDecorationMode::Unbound;
            record.requested_maximized = false;
            record.committed_maximized = false;
            record.normal_restore = None;
            record.pending_window_state = None;
            record.configured_window_states.clear();
            record.minimized = false;
            record.focused = false;
            record.committed_window_geometry = None;
            record.committed_window_geometry_explicit = false;
            record.pending_popup_reposition = None;
            record.parent_association_committed = true;
            id
        } else {
            let id = SurfaceId(self.next_surface_id);
            self.next_surface_id = self.next_surface_id.saturating_add(1);
            self.surfaces.insert(
                surface_object.clone(),
                SurfaceRecord {
                    id,
                    role: SurfaceRole::Toplevel(surface),
                    mapped: false,
                    layout,
                    title: None,
                    app_id: None,
                    window_origin: (layout.x, layout.y),
                    configured_size,
                    commit_count: 0,
                    shm_backing: None,
                    dmabuf_backing: None,
                    buffer_dimensions: None,
                    required_configure: None,
                    last_acked_configure: None,
                    last_acked_size: None,
                    decoration_object_bound: false,
                    committed_decoration: SceneDecorationMode::Unbound,
                    requested_maximized: false,
                    committed_maximized: false,
                    normal_restore: None,
                    pending_window_state: None,
                    configured_window_states: Vec::new(),
                    minimized: false,
                    focused: false,
                    chrome_pointer: ChromePointerSceneState::default(),
                    committed_window_geometry: None,
                    committed_window_geometry_explicit: false,
                    pending_popup_reposition: None,
                    parent_association_committed: true,
                    committed_input_region: None,
                    pixel_probe_logged: false,
                    logged_diagnostics: HashSet::new(),
                },
            );
            self.surface_objects.insert(id, surface_object.clone());
            id
        };

        tracing::info!(
            surface_id = id.0,
            surface = ?surface_object,
            width = configured_size.0,
            height = configured_size.1,
            preferred_scale = self.backend.output_scale(),
            "new xdg-shell toplevel configured"
        );
        self.committed_surface_stacks
            .insert(surface_object.clone(), vec![surface_object]);
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let publish = !self.session_lock_active();
        let title = compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("toplevel owns xdg role state")
                .lock()
                .expect("xdg toplevel state lock")
                .title
                .as_deref()
                .map(capped_toplevel_title)
        });
        let (event, _changed_id) =
            self.surfaces
                .get_mut(&surface.wl_surface().id())
                .map_or((None, None), |record| {
                    if record.title == title {
                        return (None, None);
                    }
                    record.title = title;
                    (
                        (record.mapped && publish).then(|| ProtocolEvent::SurfaceRelayout {
                            id: record.id,
                            scene: record.scene_snapshot(),
                        }),
                        Some(record.id),
                    )
                });
        if let Some(event) = event {
            self.events.push(event);
        }
        #[cfg(feature = "bus")]
        if let Some(id) = _changed_id {
            self.mark_surface_dirty(id, "wayland.map");
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let app_id = compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|data| {
                    data.lock()
                        .ok()?
                        .app_id
                        .as_deref()
                        .map(capped_toplevel_title)
                })
        });
        let changed_id = self
            .surfaces
            .get_mut(&surface.wl_surface().id())
            .and_then(|record| {
                if record.app_id == app_id {
                    return None;
                }
                record.app_id = app_id;
                Some(record.id)
            });
        if changed_id.is_some() {
            self.sync_foreign_toplevel(surface.wl_surface());
        }
        #[cfg(feature = "bus")]
        if let Some(id) = changed_id {
            self.mark_surface_dirty(id, "wayland.map");
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        self.retire_unadopted_roleless_buffer(surface.wl_surface());
        let surface_object = surface.wl_surface().id();
        if let Some(xdg_surface) = self.dispatching_xdg_surface.clone() {
            self.xdg_surface_objects
                .insert(xdg_surface, surface_object.clone());
        }

        let Some(parent) = surface.get_parent_surface() else {
            self.pending_parentless_popups
                .insert(surface_object, positioner);
            return;
        };
        if !self.surfaces.get(&parent.id()).is_some_and(|record| {
            matches!(record.role, SurfaceRole::Layer(_))
                || (record.mapped
                    && record.layout.visible
                    && !matches!(record.role, SurfaceRole::Dormant(_)))
        }) {
            tracing::warn!(
                surface = ?surface.wl_surface().id(),
                parent = ?parent.id(),
                "dismissed popup whose parent is not mapped"
            );
            surface.send_popup_done();
            return;
        }
        let band = self
            .surfaces
            .get(&parent.id())
            .map_or(StackBand::Normal, |record| record.layout.z.band);
        let z = self.allocate_stack_key(band);
        let Some(ResolvedPopupGeometry {
            geometry,
            mut layout,
            window_origin,
        }) = self.resolve_popup_geometry(&parent, positioner)
        else {
            tracing::warn!(
                surface = ?surface.wl_surface().id(),
                parent = ?parent.id(),
                "dismissed popup with unknown parent surface"
            );
            surface.send_popup_done();
            return;
        };
        layout.z = z;
        surface.with_pending_state(|state| {
            state.positioner = positioner;
            state.geometry = geometry;
        });
        if let Err(error) = self
            .popup_manager
            .track_popup(PopupKind::Xdg(surface.clone()))
        {
            tracing::warn!(%error, "failed to track xdg popup");
            surface.send_popup_done();
            return;
        }

        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(surface.wl_surface());
        let id = if let Some(record) = self.surfaces.get_mut(&surface_object) {
            let id = record.id;
            record.role = SurfaceRole::Popup(surface);
            record.mapped = false;
            record.layout = layout;
            record.title = None;
            record.app_id = None;
            record.window_origin = window_origin;
            record.configured_size = (geometry.size.w, geometry.size.h);
            record.required_configure = None;
            record.last_acked_configure = None;
            record.last_acked_size = None;
            record.decoration_object_bound = false;
            record.committed_decoration = SceneDecorationMode::Unbound;
            record.requested_maximized = false;
            record.committed_maximized = false;
            record.normal_restore = None;
            record.pending_window_state = None;
            record.configured_window_states.clear();
            record.minimized = false;
            record.focused = false;
            record.committed_window_geometry = None;
            record.committed_window_geometry_explicit = false;
            record.pending_popup_reposition = None;
            record.parent_association_committed = true;
            id
        } else {
            let id = SurfaceId(self.next_surface_id);
            self.next_surface_id = self.next_surface_id.saturating_add(1);
            self.surfaces.insert(
                surface_object.clone(),
                SurfaceRecord {
                    id,
                    role: SurfaceRole::Popup(surface),
                    mapped: false,
                    layout,
                    title: None,
                    app_id: None,
                    window_origin,
                    configured_size: (geometry.size.w, geometry.size.h),
                    commit_count: 0,
                    shm_backing: None,
                    dmabuf_backing: None,
                    buffer_dimensions: None,
                    required_configure: None,
                    last_acked_configure: None,
                    last_acked_size: None,
                    decoration_object_bound: false,
                    committed_decoration: SceneDecorationMode::Unbound,
                    requested_maximized: false,
                    committed_maximized: false,
                    normal_restore: None,
                    pending_window_state: None,
                    configured_window_states: Vec::new(),
                    minimized: false,
                    focused: false,
                    chrome_pointer: ChromePointerSceneState::default(),
                    committed_window_geometry: None,
                    committed_window_geometry_explicit: false,
                    pending_popup_reposition: None,
                    parent_association_committed: true,
                    committed_input_region: None,
                    pixel_probe_logged: false,
                    logged_diagnostics: HashSet::new(),
                },
            );
            self.surface_objects.insert(id, surface_object.clone());
            id
        };
        tracing::info!(
            surface_id = id.0,
            surface = ?surface_object,
            parent = ?parent.id(),
            x = layout.x,
            y = layout.y,
            width = layout.width,
            height = layout.height,
            "new xdg popup staged"
        );
        self.committed_surface_stacks
            .insert(surface_object.clone(), vec![surface_object]);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let surface_id = surface.wl_surface().id();
        self.xdg_surface_objects
            .retain(|_, mapped_surface| mapped_surface != &surface_id);
        self.deactivate_surface_role(surface.wl_surface());
    }

    fn move_request(
        &mut self,
        surface: ToplevelSurface,
        seat_resource: wl_seat::WlSeat,
        serial: Serial,
    ) {
        if self.chrome_pointer_grab.is_some()
            || !self.seat.owns(&seat_resource)
            || !self.pointer.has_grab(serial)
            || !pointer_grab_targets_surface(
                &self.pointer,
                &self.popup_manager,
                surface.wl_surface(),
            )
        {
            tracing::debug!(
                ?serial,
                "rejected xdg move request without matching pointer grab"
            );
            return;
        }
        let Some(record) = self.surfaces.get(&surface.wl_surface().id()) else {
            return;
        };
        if record.committed_maximized {
            return;
        }
        self.interactive_pointer = Some(InteractivePointer::Move {
            surface: surface.wl_surface().clone(),
            start_pointer: self.cursor_position,
            start_origin: record.window_origin,
        });
        tracing::debug!(
            surface_id = record.id.0,
            ?serial,
            "started interactive move"
        );
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat_resource: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        if self.chrome_pointer_grab.is_some()
            || edges == xdg_toplevel::ResizeEdge::None
            || !self.seat.owns(&seat_resource)
            || !self.pointer.has_grab(serial)
            || !pointer_grab_targets_surface(
                &self.pointer,
                &self.popup_manager,
                surface.wl_surface(),
            )
        {
            tracing::debug!(
                ?serial,
                ?edges,
                "rejected xdg resize request without matching pointer grab"
            );
            return;
        }
        let Some(record) = self.surfaces.get(&surface.wl_surface().id()) else {
            return;
        };
        if record.committed_maximized {
            return;
        }
        let surface_id = record.id;
        self.interactive_pointer = Some(InteractivePointer::Resize {
            surface: surface.wl_surface().clone(),
            edges,
            start_pointer: self.cursor_position,
            start_origin: record.window_origin,
            start_size: record.last_acked_size.unwrap_or(record.configured_size),
        });
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });
        let wl_surface = surface.wl_surface().clone();
        let _ = self.send_pending_toplevel_configure(&wl_surface, false);
        tracing::debug!(
            surface_id = surface_id.0,
            ?serial,
            ?edges,
            "started interactive resize"
        );
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.request_maximized_state(surface.wl_surface(), true);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.request_maximized_state(surface.wl_surface(), false);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        self.minimize_toplevel(surface.wl_surface());
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output_protocol::WlOutput>,
    ) {
        let _ = self.send_pending_toplevel_configure(surface.wl_surface(), true);
    }

    fn ack_configure(&mut self, surface: WlSurface, configure: Configure) {
        let (serial, acknowledged_size, acknowledged_decoration) = match configure {
            Configure::Toplevel(configure) => (
                configure.serial,
                configure.state.size.map(|size| (size.w, size.h)),
                configure.state.decoration_mode.map(scene_decoration_mode),
            ),
            Configure::Popup(configure) => (
                configure.serial,
                Some((
                    configure.state.geometry.size.w,
                    configure.state.geometry.size.h,
                )),
                None,
            ),
        };
        if let Some(record) = self.surfaces.get_mut(&surface.id()) {
            let acknowledged_window_state = record
                .configured_window_states
                .iter()
                .find(|snapshot| snapshot.serial == serial)
                .map(|snapshot| snapshot.state);
            record
                .configured_window_states
                .retain(|snapshot| snapshot.serial > serial);
            record.last_acked_configure = Some(serial);
            if let Some(size) = acknowledged_size {
                record.last_acked_size = Some(size);
            }
            let acknowledged_decoration =
                acknowledged_decoration.filter(|_| record.decoration_object_bound);
            if let Some(decoration) = acknowledged_decoration {
                update_pending_scene_commit_state(&surface, |state| {
                    state.acknowledged_decoration = Some(decoration);
                });
            }
            if let Some(window_state) = acknowledged_window_state {
                update_pending_scene_commit_state(&surface, |state| {
                    state.acknowledged_window_state = Some(window_state);
                });
            }
            tracing::debug!(
                surface_id = record.id.0,
                ?serial,
                ?acknowledged_size,
                ?acknowledged_decoration,
                ?acknowledged_window_state,
                configured_size = ?record.configured_size,
                "client acknowledged xdg configure"
            );
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat_resource: wl_seat::WlSeat, serial: Serial) {
        if !self.seat.owns(&seat_resource) {
            tracing::warn!("dismissed popup grab for an unknown seat");
            surface.send_popup_done();
            return;
        }
        let popup = PopupKind::Xdg(surface.clone());
        let Ok(root) = find_popup_root_surface(&popup) else {
            tracing::warn!("dismissed popup grab without a live root surface");
            surface.send_popup_done();
            return;
        };
        let pointer_action = self.pointer.has_grab(serial)
            && pointer_grab_targets_surface(&self.pointer, &self.popup_manager, &root);
        let keyboard_action = keyboard_action_matches_root(
            self.last_keyboard_action
                .as_ref()
                .map(|(action_serial, focus)| {
                    (
                        *action_serial,
                        canonical_root_surface(&self.popup_manager, focus),
                    )
                }),
            serial,
            canonical_root_surface(&self.popup_manager, &root),
        );
        let touch_action = self.seat.get_touch().is_some_and(|touch| {
            touch.has_grab(serial)
                && touch
                    .grab_start_data()
                    .and_then(|start| start.focus)
                    .is_some_and(|(focus, _)| {
                        canonical_root_surface(&self.popup_manager, &focus)
                            == canonical_root_surface(&self.popup_manager, &root)
                    })
        });
        if !popup_grab_has_live_action(pointer_action, keyboard_action, touch_action) {
            tracing::warn!(
                ?serial,
                "dismissed popup grab without a matching live input action"
            );
            surface.send_popup_done();
            return;
        }
        if let Some(exclusive) = self.exclusive_keyboard_focus.as_ref()
            && self.layer_root_object_for_surface(&root).as_ref() != Some(exclusive)
        {
            // xdg_popup.grab requires the topmost grabbing popup to retain
            // keyboard focus. An unrelated popup cannot satisfy that while an
            // Exclusive layer owns the latch, so denying and dismissing the
            // popup is protocol-correct; a pointer-only explicit grab is not.
            tracing::debug!(
                popup = ?surface.wl_surface().id(),
                exclusive = ?exclusive,
                "dismissed popup grab that cannot take keyboard focus from Exclusive layer"
            );
            surface.send_popup_done();
            return;
        }
        let seat = self.seat.clone();
        match self.popup_manager.grab_popup(root, popup, &seat, serial) {
            Ok(grab) => {
                self.cancel_chrome_pointer_grab(true);
                let pointer = self.pointer.clone();
                pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
                if self.layer_keyboard_interactivity_for_surface(surface.wl_surface())
                    != Some(KeyboardInteractivity::None)
                {
                    let keyboard = self.keyboard.clone();
                    keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
                }
            }
            Err(error) => {
                tracing::debug!(%error, "xdg popup grab was rejected");
            }
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let Some(parent) = surface.get_parent_surface() else {
            return;
        };
        let Some(ResolvedPopupGeometry {
            geometry,
            mut layout,
            window_origin,
        }) = self.resolve_popup_geometry(&parent, positioner)
        else {
            return;
        };
        let surface_object = surface.wl_surface().id();
        let Some(z) = self.surfaces.get(&surface_object).and_then(|record| {
            record.required_configure?;
            Some(record.layout.z)
        }) else {
            tracing::debug!(
                surface = ?surface_object,
                token,
                "ignored reposition request for popup that is not mapped"
            );
            return;
        };
        layout.z = z;
        surface.with_pending_state(|state| {
            state.positioner = positioner;
            state.geometry = geometry;
        });
        let Some(serial) = self.send_popup_repositioned(surface.wl_surface(), token) else {
            return;
        };
        if let Some(record) = self.surfaces.get_mut(&surface_object) {
            record.pending_popup_reposition = Some(PendingPopupReposition {
                serial,
                layout,
                window_origin,
                configured_size: (geometry.size.w, geometry.size.h),
            });
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        let surface_id = surface.wl_surface().id();
        self.pending_parentless_popups.remove(&surface_id);
        self.xdg_surface_objects
            .retain(|_, mapped_surface| mapped_surface != &surface_id);
        self.deactivate_surface_role(surface.wl_surface());
    }
}

impl SessionLockHandler for WaylandState {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.begin_session_lock(confirmation);
    }

    fn unlock(&mut self) {
        self.leave_session_lock();
    }

    fn new_surface(
        &mut self,
        _surface: LockSurface,
        _output_resource: wl_output_protocol::WlOutput,
    ) {
        unreachable!("vendored Smithay must retain the originating lock resource");
    }

    fn lock_object_may_create_surface(&self, lock: &ExtSessionLockV1) -> bool {
        match &self.lock_lifecycle {
            LockLifecycle::Locking { lock_resource, .. }
            | LockLifecycle::Locked { lock_resource, .. } => lock_resource == lock,
            LockLifecycle::Unlocked | LockLifecycle::OrphanedLocked { .. } => false,
        }
    }

    fn lock_surface_already_constructed(&self, surface: &WlSurface) -> bool {
        self.attach_history_surfaces.contains(&surface.id())
            || self.committed_surfaces.contains(&surface.id())
    }

    fn new_surface_for_lock(
        &mut self,
        originating_lock: ExtSessionLockV1,
        surface: LockSurface,
        output_resource: wl_output_protocol::WlOutput,
    ) {
        let Some(output) = self.backend.output_from_resource(&output_resource) else {
            return;
        };
        let Some(client_id) = surface.wl_surface().client().map(|client| client.id()) else {
            return;
        };
        let Some((owner, generation, lock_resource)) = (match &self.lock_lifecycle {
            LockLifecycle::Locking {
                owner,
                generation,
                lock_resource,
                ..
            }
            | LockLifecycle::Locked {
                owner,
                generation,
                lock_resource,
            } => Some((owner.clone(), *generation, lock_resource.clone())),
            LockLifecycle::Unlocked | LockLifecycle::OrphanedLocked { .. } => None,
        }) else {
            return;
        };
        if owner != client_id {
            return;
        }
        if lock_resource != originating_lock {
            originating_lock.post_error(
                SessionLockError::InvalidUnlock,
                "lock surface did not originate from the active lock object",
            );
            return;
        }

        let output_name = output.name();
        if self.lock_surfaces_by_output.contains_key(&output_name) {
            lock_resource.post_error(
                SessionLockError::DuplicateOutput,
                "physical output already has a lock surface",
            );
            return;
        }
        let (x, y, width, height) = self.backend.logical_output_rect();
        let size = (width, height);
        surface.with_pending_state(|state| {
            state.size = Some(size.into());
        });
        let z = self.allocate_stack_key(StackBand::Lock);
        let layout = SurfaceLayout {
            x: x as f32,
            y: y as f32,
            width: width as f32,
            height: height as f32,
            z,
            source: None,
            parent: None,
            transform: SurfaceTransform::Normal,
            visible: false,
            toplevel: None,
        };
        let role = LockSurfaceRole {
            surface: surface.clone(),
            output: output.clone(),
            lock_generation: generation,
        };
        let surface_object = surface.wl_surface().id();
        #[cfg(feature = "bus")]
        self.mark_surface_unmapped(surface.wl_surface());
        let id = if let Some(record) = self.surfaces.get_mut(&surface_object) {
            let id = record.id;
            record.role = SurfaceRole::LockSurface(role);
            record.mapped = false;
            record.layout = layout;
            record.title = None;
            record.app_id = None;
            record.window_origin = (layout.x, layout.y);
            record.configured_size = (width as i32, height as i32);
            record.required_configure = None;
            record.last_acked_configure = None;
            record.last_acked_size = None;
            record.decoration_object_bound = false;
            record.committed_decoration = SceneDecorationMode::Unbound;
            record.requested_maximized = false;
            record.committed_maximized = false;
            record.normal_restore = None;
            record.pending_window_state = None;
            record.configured_window_states.clear();
            record.minimized = false;
            record.focused = false;
            record.committed_window_geometry = None;
            record.committed_window_geometry_explicit = false;
            record.pending_popup_reposition = None;
            record.parent_association_committed = true;
            id
        } else {
            let id = SurfaceId(self.next_surface_id);
            self.next_surface_id = self.next_surface_id.saturating_add(1);
            self.surfaces.insert(
                surface_object.clone(),
                SurfaceRecord {
                    id,
                    role: SurfaceRole::LockSurface(role),
                    mapped: false,
                    layout,
                    title: None,
                    app_id: None,
                    window_origin: (layout.x, layout.y),
                    configured_size: (width as i32, height as i32),
                    commit_count: 0,
                    shm_backing: None,
                    dmabuf_backing: None,
                    buffer_dimensions: None,
                    required_configure: None,
                    last_acked_configure: None,
                    last_acked_size: None,
                    decoration_object_bound: false,
                    committed_decoration: SceneDecorationMode::Unbound,
                    requested_maximized: false,
                    committed_maximized: false,
                    normal_restore: None,
                    pending_window_state: None,
                    configured_window_states: Vec::new(),
                    minimized: false,
                    focused: false,
                    chrome_pointer: ChromePointerSceneState::default(),
                    committed_window_geometry: None,
                    committed_window_geometry_explicit: false,
                    pending_popup_reposition: None,
                    parent_association_committed: true,
                    committed_input_region: None,
                    pixel_probe_logged: false,
                    logged_diagnostics: HashSet::new(),
                },
            );
            self.surface_objects.insert(id, surface_object.clone());
            id
        };
        self.committed_surface_stacks
            .insert(surface_object.clone(), vec![surface_object.clone()]);
        self.lock_surfaces_by_output
            .insert(output_name, surface_object);
        self.backend.output_enter(surface.wl_surface());
        let serial = self
            .send_lock_configure(surface.wl_surface())
            .expect("new lock surface has an immediate configure");
        tracing::info!(
            surface_id = id.0,
            ?serial,
            generation,
            "new lock surface configured"
        );
    }

    fn ack_configure(&mut self, surface: WlSurface, configure: LockSurfaceConfigure) {
        let Some(record) = self.surfaces.get_mut(&surface.id()) else {
            return;
        };
        if !matches!(record.role, SurfaceRole::LockSurface(_)) {
            return;
        }
        record.last_acked_configure = Some(configure.serial);
        record.last_acked_size = configure
            .state
            .size
            .map(|size| (size.w as i32, size.h as i32));
    }

    fn lock_surface_destroyed(&mut self, surface: WlSurface) {
        let output = self.surfaces.get(&surface.id()).and_then(|record| {
            let SurfaceRole::LockSurface(role) = &record.role else {
                return None;
            };
            Some(role.output.name())
        });
        if let Some(output) = output {
            self.lock_surfaces_by_output.remove(&output);
            self.deactivate_surface_role(&surface);
        }
    }

    fn lock_destroyed(&mut self, lock: ExtSessionLockV1) {
        let abort = matches!(
            &self.lock_lifecycle,
            LockLifecycle::Locking { lock_resource, .. } if lock_resource == &lock
        );
        if abort {
            self.abort_locking_after_owner_death(&lock);
            tracing::info!("session-lock object destroyed during Locking; lock aborted");
        }
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for WaylandState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if let Err(error) = validate_dmabuf_metadata(
            &dmabuf,
            &self.supported_dmabuf_formats,
            self.backend.output_size(),
        ) {
            tracing::warn!(
                format = ?dmabuf.format(),
                planes = dmabuf.num_planes(),
                %error,
                "rejected invalid or unsupported DMA-BUF metadata"
            );
            notifier.failed();
            return;
        }
        let Some(validation) = &self.dmabuf_validation else {
            if let Err(error) = notifier.successful::<Self>() {
                tracing::debug!(%error, "DMA-BUF client destroyed params during import");
            }
            return;
        };
        // `validate_dmabuf_metadata` immediately above has already established
        // a positive, u32-representable size and exactly one plane. With
        // `describe_dmabuf`'s current operations, only duplicating that owned
        // plane file descriptor can therefore return `Err` at this call site.
        // That fallback is deliberately not induced by an offline test, because
        // lowering the process descriptor limit is global and would race every
        // other test in the binary. Revisit this claim if these calls are
        // reordered or `describe_dmabuf` gains another fallible operation.
        let descriptor = match describe_dmabuf(&dmabuf) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                tracing::warn!(
                    format = ?dmabuf.format(),
                    %error,
                    "failed to duplicate DMA-BUF for asynchronous validation"
                );
                notifier.failed();
                return;
            }
        };
        let request = DmabufValidationRequest {
            descriptor,
            notifier,
            format: dmabuf.format(),
        };
        match validation.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) => {
                tracing::warn!(
                    capacity = DMABUF_VALIDATION_QUEUE_CAPACITY,
                    "DMA-BUF validation queue is full; refusing import without blocking protocol"
                );
                request.notifier.failed();
            }
            Err(TrySendError::Disconnected(request)) => {
                tracing::error!("DMA-BUF validation worker stopped");
                request.notifier.failed();
            }
        }
    }
}

impl DrmSyncobjHandler for WaylandState {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        match self.drm_syncobj_state.as_mut() {
            Some(ExplicitSyncGlobal::Live(state)) => Some(state),
            #[cfg(test)]
            Some(ExplicitSyncGlobal::Probe(_)) => None,
            None => None,
        }
    }
}

impl SeatHandler for WaylandState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        invalidate_keyboard_action(&mut self.last_keyboard_action);
        let focused_root =
            focused.map(|surface| canonical_root_surface(&self.popup_manager, surface));
        set_data_device_focus(
            &self.display_handle,
            seat,
            (!self.session_lock_active())
                .then(|| focused.and_then(|surface| surface.client()))
                .flatten(),
        );
        let toplevels = self
            .surfaces
            .values()
            .filter_map(|record| {
                record
                    .role
                    .toplevel()
                    .map(|toplevel| (toplevel.clone(), toplevel.wl_surface().clone()))
            })
            .collect::<Vec<_>>();
        for (toplevel, surface) in toplevels {
            let active = focused_root
                .as_ref()
                .is_some_and(|focused| focused == toplevel.wl_surface());
            if let Some(record) = self.surfaces.get_mut(&surface.id())
                && record.focused != active
            {
                record.focused = active;
                sync_toplevel_scene_state(record);
                if record.mapped && record.committed_decoration == SceneDecorationMode::ServerSide {
                    self.events.push(ProtocolEvent::SurfaceRelayout {
                        id: record.id,
                        scene: record.scene_snapshot(),
                    });
                }
            }
            toplevel.with_pending_state(|state| {
                if active {
                    state.states.set(xdg_toplevel::State::Activated);
                } else {
                    state.states.unset(xdg_toplevel::State::Activated);
                }
            });
            let _ = self.send_pending_toplevel_configure(&surface, false);
        }
        tracing::info!(
            surface = ?focused.map(|surface| surface.id()),
            "keyboard focus changed"
        );
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.set_cursor_image(image);
    }
}

impl OutputHandler for WaylandState {}

impl IdleNotifierHandler for WaylandState {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

impl ForeignToplevelListHandler for WaylandState {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

impl SelectionHandler for WaylandState {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        tracing::debug!(
            ?target,
            mime_types = ?source.as_ref().map(SelectionSource::mime_types),
            "nested client selection changed"
        );
        // Client-to-client transfers remain entirely fd-driven inside
        // Smithay: the source receives wl_data_source.send with the receiver's
        // pipe fd and writes directly, so the protocol thread never copies or
        // blocks on clipboard payload bytes. Host clipboard bridging is a
        // later phase concern.
    }
}

impl XdgDecorationHandler for WaylandState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        let replacement = self
            .surfaces
            .get_mut(&toplevel.wl_surface().id())
            .is_some_and(|record| {
                record.decoration_object_bound = true;
                let mut replacement = false;
                update_pending_scene_commit_state(toplevel.wl_surface(), |state| {
                    replacement = mem::take(&mut state.decoration_reverts);
                    state.acknowledged_decoration = None;
                });
                replacement
            });
        if replacement && self.decoration.enabled {
            compositor::with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .expect("toplevel owns xdg role state")
                    .lock()
                    .expect("xdg toplevel state lock")
                    .initial_decoration_configure_sent = false;
            });
        }
        self.configure_decoration(&toplevel, None);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        self.configure_decoration(&toplevel, Some(mode));
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.configure_decoration(&toplevel, None);
    }
}

impl FractionalScaleHandler for WaylandState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.backend.output_scale();
        compositor::with_states(&surface, |states| {
            fractional_scale::with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl ClientDndGrabHandler for WaylandState {
    fn started(
        &mut self,
        _source: Option<smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource>,
        icon: Option<WlSurface>,
        _seat: Seat<Self>,
    ) {
        if let Some(icon) = icon {
            self.retire_unadopted_roleless_buffer(&icon);
        }
    }
}
impl ServerDndGrabHandler for WaylandState {}

impl DataDeviceHandler for WaylandState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        compositor: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        data: &(),
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(&request, wl_compositor::Request::CreateSurface { .. }) {
            let client_surfaces = client
                .get_data::<WaylandClientState>()
                .map(|client_state| client_state.surface_count.load(Ordering::Relaxed))
                .unwrap_or_default();
            if surface_budget_exhausted(client_surfaces, state.surface_count) {
                state.reject_client_resource_limit(
                    client,
                    format!(
                        "live wl_surface budget exceeded (client {client_surfaces}/{MAX_CLIENT_SURFACES}, global {}/{MAX_GLOBAL_SURFACES})",
                        state.surface_count
                    ),
                );
                return;
            }
        }
        <CompositorState as Dispatch<wl_compositor::WlCompositor, (), WaylandState>>::request(
            state, client, compositor, request, data, handle, data_init,
        );
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        subcompositor: &wl_subcompositor::WlSubcompositor,
        request: wl_subcompositor::Request,
        data: &(),
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_subcompositor::Request::GetSubsurface {
            surface, parent, ..
        } = &request
        {
            let depth = state.proposed_subsurface_depth(surface, parent);
            if depth.is_none_or(|depth| depth > MAX_SUBSURFACE_DEPTH) {
                state.reject_client_resource_limit(
                    client,
                    format!("wl_subsurface depth exceeds hard limit {MAX_SUBSURFACE_DEPTH}"),
                );
                return;
            }
        }
        <CompositorState as Dispatch<wl_subcompositor::WlSubcompositor, (), WaylandState>>::request(
            state,
            client,
            subcompositor,
            request,
            data,
            handle,
            data_init,
        );
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, SubsurfaceUserData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        subsurface: &wl_subsurface::WlSubsurface,
        request: wl_subsurface::Request,
        data: &SubsurfaceUserData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        <CompositorState as Dispatch<
            wl_subsurface::WlSubsurface,
            SubsurfaceUserData,
            WaylandState,
        >>::request(state, client, subsurface, request, data, handle, data_init);
    }

    fn destroyed(
        state: &mut Self,
        client_id: ClientId,
        subsurface: &wl_subsurface::WlSubsurface,
        data: &SubsurfaceUserData,
    ) {
        let former_root = state.toplevel_root_for_surface(data.surface());
        state.detach_subsurface_topology(data.surface());
        <CompositorState as Dispatch<
            wl_subsurface::WlSubsurface,
            SubsurfaceUserData,
            WaylandState,
        >>::destroyed(state, client_id, subsurface, data);
        if let Some(former_root) = former_root {
            state.refresh_toplevel_window_geometry(&former_root);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, SurfaceUserData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceUserData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(
            &request,
            wl_surface::Request::Damage { .. } | wl_surface::Request::DamageBuffer { .. }
        ) {
            let count = state
                .damage_requests_since_apply
                .entry(surface.id())
                .or_default();
            match damage_cap_action(*count, 1) {
                DamageCapAction::Accept | DamageCapAction::Saturate => {
                    *count = count.saturating_add(1);
                }
                DamageCapAction::Drop => return,
            }
        }
        if matches!(&request, wl_surface::Request::Attach { .. }) {
            state.attach_history_surfaces.insert(surface.id());
        }
        if matches!(
            &request,
            wl_surface::Request::Attach {
                buffer: Some(_),
                ..
            }
        ) {
            state.buffer_history_surfaces.insert(surface.id());
            let unconfigured_layer = state.surfaces.get(&surface.id()).and_then(|record| {
                if record.required_configure.is_some() {
                    return None;
                }
                let SurfaceRole::Layer(role) = &record.role else {
                    return None;
                };
                if matches!(role.output, LayerOutputBinding::Closed) {
                    return None;
                }
                Some(role.surface.layer_surface().clone())
            });
            if let Some(layer) = unconfigured_layer {
                let _ = layer.ensure_configured();
                return;
            }
        }
        <CompositorState as Dispatch<
            wl_surface::WlSurface,
            SurfaceUserData,
            WaylandState,
        >>::request(state, client, surface, request, data, handle, data_init);
    }

    fn destroyed(
        state: &mut Self,
        client_id: ClientId,
        surface: &wl_surface::WlSurface,
        data: &SurfaceUserData,
    ) {
        <CompositorState as Dispatch<
            wl_surface::WlSurface,
            SurfaceUserData,
            WaylandState,
        >>::destroyed(state, client_id, surface, data);
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, XdgShellSurfaceUserData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        toplevel: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        data: &XdgShellSurfaceUserData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let Some(surface) = state.surfaces.values().find_map(|record| {
            record
                .role
                .toplevel()
                .filter(|surface| surface.xdg_toplevel() == toplevel)
                .map(|surface| surface.wl_surface().clone())
        }) && let Some(constraints) =
            constraints_after_toplevel_request(pending_surface_size_constraints(&surface), &request)
            && let Err(error) = validate_toplevel_constraints(constraints)
        {
            toplevel.post_error(xdg_toplevel::Error::InvalidSize, error);
            return;
        }
        <XdgShellState as Dispatch<
            xdg_toplevel::XdgToplevel,
            XdgShellSurfaceUserData,
            WaylandState,
        >>::request(state, client, toplevel, request, data, handle, data_init);
    }

    fn destroyed(
        state: &mut Self,
        client_id: ClientId,
        toplevel: &xdg_toplevel::XdgToplevel,
        data: &XdgShellSurfaceUserData,
    ) {
        <XdgShellState as Dispatch<
            xdg_toplevel::XdgToplevel,
            XdgShellSurfaceUserData,
            WaylandState,
        >>::destroyed(state, client_id, toplevel, data);
    }
}

impl Dispatch<xdg_surface::XdgSurface, XdgSurfaceUserData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        xdg_surface: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        data: &XdgSurfaceUserData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let constructs_role = matches!(
            &request,
            xdg_surface::Request::GetToplevel { .. } | xdg_surface::Request::GetPopup { .. }
        );
        if constructs_role {
            state.dispatching_xdg_surface = Some(xdg_surface.id());
        }
        let active_surface = state.xdg_surface_objects.get(&xdg_surface.id()).cloned();
        if let xdg_surface::Request::SetWindowGeometry { width, height, .. } = &request
            && (*width <= 0 || *height <= 0)
            && active_surface.is_some()
        {
            xdg_surface.post_error(
                xdg_surface::Error::InvalidSize,
                format!("window geometry must be positive, got {width}x{height}"),
            );
            state.dispatching_xdg_surface = None;
            return;
        }
        let marks_toplevel_geometry = matches!(
            &request,
            xdg_surface::Request::SetWindowGeometry { width, height, .. }
                if *width > 0 && *height > 0
        ) && active_surface.as_ref().is_some_and(|surface| {
            state
                .surfaces
                .get(surface)
                .is_some_and(|record| matches!(record.role, SurfaceRole::Toplevel(_)))
        });
        <XdgShellState as Dispatch<xdg_surface::XdgSurface, XdgSurfaceUserData, WaylandState>>::request(
            state,
            client,
            xdg_surface,
            request,
            data,
            handle,
            data_init,
        );
        if marks_toplevel_geometry
            && let Some(surface) = active_surface
                .and_then(|surface| state.surfaces.get(&surface))
                .map(|record| record.role.wl_surface().clone())
        {
            update_pending_scene_commit_state(&surface, |state| {
                state.window_geometry_changed = true;
            });
        }
        if constructs_role {
            state.dispatching_xdg_surface = None;
        }
    }

    fn destroyed(
        state: &mut Self,
        client_id: ClientId,
        xdg_surface: &xdg_surface::XdgSurface,
        data: &XdgSurfaceUserData,
    ) {
        <XdgShellState as Dispatch<
            xdg_surface::XdgSurface,
            XdgSurfaceUserData,
            WaylandState,
        >>::destroyed(state, client_id, xdg_surface, data);
        state.xdg_surface_objects.remove(&xdg_surface.id());
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ToplevelSurface>
    for WaylandState
{
    fn request(
        state: &mut Self,
        client: &Client,
        decoration: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        request: zxdg_toplevel_decoration_v1::Request,
        toplevel: &ToplevelSurface,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Request::SetMode {
            mode: WEnum::Unknown(mode),
        } = &request
        {
            decoration.post_error(
                zxdg_toplevel_decoration_v1::Error::InvalidMode,
                format!("invalid decoration mode {mode}"),
            );
            return;
        }
        if matches!(request, zxdg_toplevel_decoration_v1::Request::Destroy) {
            if let Some(record) = state.surfaces.get_mut(&toplevel.wl_surface().id()) {
                record.decoration_object_bound = false;
            }
            update_pending_scene_commit_state(toplevel.wl_surface(), |state| {
                state.decoration_reverts = true;
                state.acknowledged_decoration = None;
            });
            compositor::with_states(toplevel.wl_surface(), |states| {
                let mut attributes = states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .expect("toplevel owns xdg role state")
                    .lock()
                    .expect("xdg toplevel state lock");
                attributes.current.decoration_mode = None;
                if let Some(last_acked) = attributes.last_acked.as_mut() {
                    last_acked.decoration_mode = None;
                }
                attributes.initial_decoration_configure_sent = false;
            });
            toplevel.with_pending_state(|pending| pending.decoration_mode = None);
        }
        <XdgDecorationState as Dispatch<
            zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
            ToplevelSurface,
            WaylandState,
        >>::request(
            state, client, decoration, request, toplevel, handle, data_init,
        );
    }
}

smithay::reexports::wayland_server::delegate_global_dispatch!(WaylandState: [
    wl_compositor::WlCompositor: ()
] => CompositorState);
smithay::reexports::wayland_server::delegate_global_dispatch!(WaylandState: [
    wl_subcompositor::WlSubcompositor: ()
] => CompositorState);
smithay::reexports::wayland_server::delegate_dispatch!(WaylandState: [
    wl_region::WlRegion: RegionUserData
] => CompositorState);
smithay::reexports::wayland_server::delegate_dispatch!(WaylandState: [
    wl_callback::WlCallback: ()
] => CompositorState);
smithay::reexports::wayland_server::delegate_global_dispatch!(WaylandState: [
    xdg_wm_base::XdgWmBase: ()
] => XdgShellState);
smithay::reexports::wayland_server::delegate_dispatch!(WaylandState: [
    xdg_wm_base::XdgWmBase: XdgWmBaseUserData
] => XdgShellState);
smithay::reexports::wayland_server::delegate_dispatch!(WaylandState: [
    xdg_positioner::XdgPositioner: XdgPositionerUserData
] => XdgShellState);
smithay::reexports::wayland_server::delegate_dispatch!(WaylandState: [
    xdg_popup::XdgPopup: XdgShellSurfaceUserData
] => XdgShellState);
smithay::reexports::wayland_server::delegate_global_dispatch!(WaylandState: [
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1: XdgDecorationManagerGlobalData
] => XdgDecorationState);
smithay::reexports::wayland_server::delegate_dispatch!(WaylandState: [
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1: ()
] => XdgDecorationState);
delegate_fractional_scale!(WaylandState);
delegate_viewporter!(WaylandState);
delegate_shm!(WaylandState);
delegate_dmabuf!(WaylandState);
smithay::delegate_drm_syncobj!(WaylandState);
delegate_seat!(WaylandState);
delegate_data_device!(WaylandState);
delegate_output!(WaylandState);
delegate_idle_notify!(WaylandState);
delegate_foreign_toplevel_list!(WaylandState);
delegate_session_lock!(WaylandState);
impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for WaylandState {
    fn bind(
        state: &mut Self,
        _display: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let id = state.create_screencopy_manager(client.id());
        let manager = data_init.init(resource, ScreencopyManagerData { id });
        if id.is_none() {
            // zwlr_screencopy_manager_v1 defines no named errors. Code zero is
            // this implementation's fatal implementation-limit error on the
            // newly-created manager object.
            manager.post_error(
                SCREENCOPY_MANAGER_ERROR_IMPLEMENTATION_LIMIT,
                format!(
                    "screencopy manager implementation limit exceeded (per-client {}, global {})",
                    MAX_CLIENT_CAPTURE_MANAGERS, MAX_GLOBAL_CAPTURE_MANAGERS
                ),
            );
        }
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ScreencopyManagerData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        data: &ScreencopyManagerData,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(manager_id) = data.id else {
            return;
        };
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                let id = state.allocate_capture_id();
                let resource = data_init.init(frame, ScreencopyFrameData { id });
                state.create_screencopy_frame(
                    id,
                    manager_id,
                    client,
                    resource,
                    &output,
                    overlay_cursor,
                    None,
                );
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let id = state.allocate_capture_id();
                let resource = data_init.init(frame, ScreencopyFrameData { id });
                state.create_screencopy_frame(
                    id,
                    manager_id,
                    client,
                    resource,
                    &output,
                    overlay_cursor,
                    Some((x, y, width, height)),
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        _resource: &ZwlrScreencopyManagerV1,
        data: &ScreencopyManagerData,
    ) {
        if let Some(id) = data.id {
            state.destroy_screencopy_manager(id);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &ScreencopyFrameData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                state.submit_screencopy(data.id, frame, buffer, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                state.submit_screencopy(data.id, frame, buffer, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        _resource: &ZwlrScreencopyFrameV1,
        data: &ScreencopyFrameData,
    ) {
        state.destroy_screencopy_frame(data.id);
    }
}

smithay::reexports::wayland_server::delegate_global_dispatch!(WaylandState: [
    ZwlrLayerShellV1: WlrLayerShellGlobalData
] => WlrLayerShellState);
smithay::reexports::wayland_server::delegate_dispatch!(WaylandState: [
    ZwlrLayerSurfaceV1: WlrLayerSurfaceUserData
] => WlrLayerShellState);

impl Dispatch<ZwlrLayerShellV1, ()> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        shell: &ZwlrLayerShellV1,
        request: zwlr_layer_shell_v1::Request,
        data: &(),
        display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_layer_shell_v1::Request::GetLayerSurface { surface, .. } = &request {
            if compositor::get_role(surface).is_some() {
                shell.post_error(
                    zwlr_layer_shell_v1::Error::Role,
                    "Surface already has a role.",
                );
                return;
            }
            if state.layer_role_creation_is_already_constructed(surface) {
                shell.post_error(
                    zwlr_layer_shell_v1::Error::AlreadyConstructed,
                    "wl_surface already has a buffer attached",
                );
                return;
            }
        }
        <WlrLayerShellState as Dispatch<ZwlrLayerShellV1, (), WaylandState>>::request(
            state, client, shell, request, data, display, data_init,
        );
    }
}
