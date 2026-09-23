//! Interim, output-sized menu layer: catches click-away without a popup grab.
//! The compositor's existing exclusive-layer policy owns focus on dismissal.
use super::*;
use cosmix_shell::chrome::corner_menu::{
    self as ui, CornerMenuExtraHook, CornerMenuRequest, MenuAction,
};
use cosmix_shell::core::PanelInput;
use smithay_client_toolkit::seat::pointer::{BTN_LEFT, PointerEventKind};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity};

pub(super) struct NativeCornerMenu {
    pub(super) surface: PanelSurface,
    request: CornerMenuRequest,
    origin: Vec2,
    rows: Vec<Entity>,
    selected: Option<usize>,
    pressed: Option<usize>,
}

/// Default hook always supplies the three mode items. The app may add extras.
pub fn open(world: &mut World, output: &OutputKey, corner: cosmix_shell::core::Corner) {
    let Some(frame) = world.get_resource::<ShellFrameState>() else {
        return;
    };
    let items = ui::menu_items(frame.0.panel(corner.summoned_edge()).mode, &[]);
    world.insert_resource(CornerMenuRequest {
        output: output.clone(),
        corner,
        items,
    });
}

impl RunnerState {
    pub(super) fn reconcile_corner_menu(
        &mut self,
        qh: &QueueHandle<Self>,
    ) -> Result<(), LayerHostError> {
        if let Some(menu) = self.menu.as_mut() {
            let mode = self
                .app
                .world()
                .resource::<ShellFrameState>()
                .0
                .panel(menu.request.corner.summoned_edge())
                .mode;
            if ui::refresh_mode(self.app.world_mut(), &menu.rows, &mut menu.request, mode) {
                menu.selected = menu.selected.filter(|i| !menu.request.items[*i].checked);
                menu.pressed = None;
                ui::highlight(self.app.world_mut(), &menu.rows, menu.selected);
                self.needs_update = true;
            }
        }
        let Some(mut request) = self.app.world_mut().remove_resource::<CornerMenuRequest>() else {
            return Ok(());
        };
        if self.selected_key.as_ref() != Some(&request.output) {
            return Ok(());
        }
        let Some(output) = self.outputs.get(&request.output) else {
            return Ok(());
        };
        let size = Vec2::new(output.logical_size.width(), output.logical_size.height());
        let mode = self
            .app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(request.corner.summoned_edge())
            .mode;
        for item in &mut request.items {
            if let MenuAction::Mode(value) = item.action {
                item.checked = value == mode;
            }
        }
        let wl = self.compositor_state.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            wl.clone(),
            Layer::Overlay,
            Some(format!("{}-corner-menu", self.namespace)),
            Some(&output.wl_output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        let mut surface = PanelSurface::from_wayland(
            &mut self.app,
            &self.connection,
            wl,
            layer,
            output.logical_size,
            output.scale,
            request.corner.summoned_edge(),
            None,
            None,
        )
        .map_err(|e| LayerHostError::new(e.to_string()))?;
        self.app
            .world_mut()
            .get_mut::<Camera>(surface.camera)
            .unwrap()
            .clear_color = ClearColorConfig::Custom(Color::NONE);
        let rows = ui::spawn_menu(self.app.world_mut(), surface.mount, &request, size);
        // The new layer takes pointer containment away from panel surfaces.
        // Clear their bridge state while the menu hold is already acquired.
        self.pointer_bridge
            .cleanup(&mut self.app, &request.output, None);
        self.keyboard_bridge.cleanup(&mut self.app, None);
        self.touch_bridge.cancel(&mut self.app);
        let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
        surface.apply_protocol_ops(&[ProtocolOp::CommitBufferless], elapsed);
        self.menu = Some(NativeCornerMenu {
            surface,
            origin: ui::menu_origin(request.corner, size, request.items.len()),
            request,
            rows,
            selected: None,
            pressed: None,
        });
        self.needs_update = true;
        Ok(())
    }

    /// Mode command precedes hold release in the same ingress queue. Dropping
    /// the exclusive layer happens only after the renderer drains its handle.
    pub(super) fn dismiss_corner_menu(&mut self, choice: Option<usize>) {
        if let Some(request) = self.app.world_mut().remove_resource::<CornerMenuRequest>() {
            stage_shell_command(
                &mut self.app,
                request.output,
                ShellCommandKind::Panel {
                    edge: request.corner.summoned_edge(),
                    input: PanelInput::MenuHold(false),
                },
            );
            self.needs_update = true;
        }
        let Some(mut menu) = self.menu.take() else {
            return;
        };
        dismiss(&mut self.app, &mut menu, choice);
        menu.surface.retire(&mut self.app);
        self.needs_update = true;
    }

    pub(super) fn menu_pointer(&mut self, events: &[PointerEvent]) -> bool {
        if self.menu.is_none() {
            return false;
        }
        for event in events {
            let menu = self.menu.as_mut().unwrap();
            if !menu.surface.matches_surface(&event.surface) {
                continue;
            }
            let row = ui::hit_row(
                Vec2::new(event.position.0 as f32, event.position.1 as f32),
                menu.origin,
                menu.rows.len(),
            );
            let enabled = row.filter(|i| !menu.request.items[*i].checked);
            match event.kind {
                PointerEventKind::Press { button, .. } => {
                    if row.is_none() {
                        self.dismiss_corner_menu(None);
                        return true;
                    }
                    if button == BTN_LEFT {
                        menu.pressed = enabled;
                    }
                }
                PointerEventKind::Release {
                    button: BTN_LEFT, ..
                } => {
                    if enabled.is_some() && menu.pressed.take() == enabled {
                        self.dismiss_corner_menu(enabled);
                        return true;
                    }
                }
                PointerEventKind::Motion { .. } | PointerEventKind::Enter { .. } => {
                    menu.selected = enabled;
                    ui::highlight(self.app.world_mut(), &menu.rows, enabled);
                    self.needs_update = true;
                }
                _ => {}
            }
        }
        true
    }

    pub(super) fn menu_key(&mut self, event: &KeyEvent) -> bool {
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        // XKB keysyms: Escape, Return, space, Up, Down, Tab.
        match event.keysym.raw() {
            0xff1b => self.dismiss_corner_menu(None),
            0xff0d | 0x20 => {
                let selected = menu.selected;
                if selected.is_some() {
                    self.dismiss_corner_menu(selected);
                }
            }
            0xff52 | 0xff54 | 0xff09 => {
                let count = menu.rows.len();
                let backwards = event.keysym.raw() == 0xff52;
                let mut index = menu
                    .selected
                    .unwrap_or(if backwards { 0 } else { count - 1 });
                for _ in 0..count {
                    index = if backwards {
                        (index + count - 1) % count
                    } else {
                        (index + 1) % count
                    };
                    if !menu.request.items[index].checked {
                        break;
                    }
                }
                menu.selected = Some(index);
                ui::highlight(self.app.world_mut(), &menu.rows, menu.selected);
                self.needs_update = true;
            }
            _ => {}
        }
        true
    }

    pub(super) fn configure_menu(
        &mut self,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: &LayerSurfaceConfigure,
    ) -> bool {
        let Some(menu) = self
            .menu
            .as_mut()
            .filter(|m| m.surface.matches_layer(layer))
        else {
            return false;
        };
        let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
        if configure.new_size.0 > 0 && configure.new_size.1 > 0 {
            menu.origin = ui::menu_origin(
                menu.request.corner,
                Vec2::new(configure.new_size.0 as f32, configure.new_size.1 as f32),
                menu.rows.len(),
            );
            let popup = self
                .app
                .world()
                .get::<ChildOf>(menu.rows[0])
                .unwrap()
                .parent();
            let mut node = self.app.world_mut().get_mut::<Node>(popup).unwrap();
            node.left = Val::Px(menu.origin.x);
            node.top = Val::Px(menu.origin.y);
        }
        if let Err(error) = menu.surface.configure(
            &mut self.app,
            qh,
            configure,
            elapsed,
            self.max_texture_dimension_2d,
        ) {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("menu-configure-{}", error.reason_suffix()));
        }
        self.needs_update = true;
        true
    }

    pub(super) fn menu_matches_layer(&self, layer: &LayerSurface) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|m| m.surface.matches_layer(layer))
    }

    pub(super) fn menu_scale(
        &mut self,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale: i32,
    ) -> bool {
        let Some(menu) = self
            .menu
            .as_mut()
            .filter(|m| m.surface.matches_surface(surface))
        else {
            return false;
        };
        let Some(output) = self.outputs.get(&menu.request.output) else {
            return true;
        };
        let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
        if let Err(error) = menu.surface.update_output_metrics(
            &mut self.app,
            output.logical_size,
            scale,
            qh,
            elapsed,
            self.max_texture_dimension_2d,
        ) {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("menu-scale-{}", error.reason_suffix()));
        }
        self.needs_update = true;
        true
    }
}

fn dismiss(app: &mut App, menu: &mut NativeCornerMenu, choice: Option<usize>) {
    let edge = menu.request.corner.summoned_edge();
    let item = choice
        .and_then(|i| menu.request.items.get(i))
        .filter(|i| !i.checked)
        .cloned();
    if let Some(command) = item.as_ref().and_then(|i| i.command(edge)) {
        stage_shell_command(app, menu.request.output.clone(), command);
    }
    stage_shell_command(
        app,
        menu.request.output.clone(),
        ShellCommandKind::Panel {
            edge,
            input: PanelInput::MenuHold(false),
        },
    );
    if let Some(ui::MenuItem {
        action: MenuAction::Extra(extra),
        ..
    }) = item
        && let Some(hook) = app.world().get_resource::<CornerMenuExtraHook>().copied()
    {
        (hook.0)(app.world_mut(), extra);
    }
    menu.surface.close(app);
    app.update();
    app.world_mut().despawn(menu.surface.mount);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[test]
    fn menu_dismissal_releases_hold_and_drains_layer_before_retirement() {
        // Escape/click-away share None; enabled item choices use the same exit.
        for choice in [None, Some(0), Some(1)] {
            let output = OutputKey::new("test-output").unwrap();
            let mut model = ShellModel::new(
                output.clone(),
                LogicalSize::new(1000.0, 800.0).unwrap(),
                Duration::ZERO,
                Duration::from_millis(800),
                Duration::from_millis(200),
            )
            .unwrap();
            model
                .panel_input(Edge::Left, Duration::ZERO, PanelInput::Reveal)
                .unwrap();
            model
                .panel_input(Edge::Left, Duration::ZERO, PanelInput::MenuHold(true))
                .unwrap();
            let mut app = App::new();
            configure_ingress(&mut app);
            app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)));
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
            let sequence = Arc::new(Mutex::new(Vec::new()));
            let surface =
                PanelSurface::test_double(&mut app, SurfacePhase::Configured, sequence.clone());
            app.world_mut()
                .entity_mut(surface.camera)
                .insert(Camera::default());
            let request = CornerMenuRequest {
                output,
                corner: cosmix_shell::core::Corner::TopLeft,
                items: ui::menu_items(PanelMode::Hidden, &[]),
            };
            let mut menu = NativeCornerMenu {
                surface,
                request,
                origin: Vec2::ZERO,
                rows: vec![],
                selected: None,
                pressed: None,
            };
            dismiss(&mut app, &mut menu, choice);
            let panel = app
                .world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left);
            assert_eq!(
                panel.mode,
                match choice {
                    Some(0) => PanelMode::Pinned,
                    Some(1) => PanelMode::Docked,
                    _ => PanelMode::Hidden,
                }
            );
            assert!(!panel.transient_revealed);
            assert!(
                !app.world()
                    .get::<Camera>(menu.surface.camera)
                    .unwrap()
                    .is_active
            );
            assert_eq!(menu.surface.phase, SurfacePhase::Closed);
            assert!(
                sequence.lock().unwrap().is_empty(),
                "retain layer through extraction"
            );
            menu.surface.retire(&mut app);
            assert!(
                sequence.lock().unwrap().contains(&"layer"),
                "dropping the exclusive layer releases keyboard focus"
            );
        }
    }
}
