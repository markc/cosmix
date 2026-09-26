//! The dialog surface (scene-editor plan §4.3 Q2): one centred overlay layer
//! for the dialog chrome root that `cosmix_shell::chrome::dialog` builds.
//!
//! - **Surface.** `Layer::Overlay`, no anchors — wlr-layer-shell centres an
//!   unanchored surface; comp centres it in the usable zone left by other
//!   layers' exclusive zones — `set_size(w, h)` from the seat, exclusive
//!   zone 0. Mapped while [`QuoinDialog::wants_surface`] holds on the
//!   selected output, unmapped (Wayland objects dropped after a render
//!   drain, the chrome root retained) otherwise.
//! - **Keyboard, grab then demote (D16).** Every map requests `Exclusive`,
//!   so Escape and typing reach the dialog without a click; the first
//!   keyboard enter demotes it to `OnDemand`, and comp keeps focus on a
//!   demoted layer while it stays mapped.
//! - **Input.** The dialog is a [`SurfaceTarget`] of kind
//!   [`SurfaceKind::Dialog`], so pointer, keyboard, touch and focus
//!   confinement resolve its own window while no panel semantics apply.
//!   It is not special-cased like the corner menu.
//! - **Output removal** unmaps it and clears `visible`, so `panel.changed`
//!   publishes the hide; the next show maps it on the selected output.
use super::*;
use crate::planner::ProtocolKeyboardInteractivity;
use cosmix_shell::chrome::dialog::QuoinDialog;
use cosmix_shell::runtime::ShellFrame;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity};

pub(super) struct NativeDialog {
    pub(super) surface: PanelSurface,
    output: OutputKey,
    size: (u32, u32),
    pub(super) origin: Vec2,
    keyboard: DialogKeyboard,
}

/// Grab-then-demote (D16) as data: what the layer asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DialogKeyboard {
    /// Mapped with `Exclusive`, waiting for the keyboard to arrive.
    Grabbing,
    /// The keyboard arrived; the layer now asks for `OnDemand`.
    Demoted,
}

impl DialogKeyboard {
    pub(crate) const fn requested(self) -> ProtocolKeyboardInteractivity {
        match self {
            Self::Grabbing => ProtocolKeyboardInteractivity::Exclusive,
            Self::Demoted => ProtocolKeyboardInteractivity::OnDemand,
        }
    }

    /// A keyboard enter on the dialog. Returns the interactivity to commit
    /// the first time only.
    pub(crate) fn entered(&mut self) -> Option<ProtocolKeyboardInteractivity> {
        (*self == Self::Grabbing).then(|| {
            *self = Self::Demoted;
            self.requested()
        })
    }
}

/// What the host should have mapped, from the shared dialog state.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DialogSpec {
    pub output: OutputKey,
    pub size: (u32, u32),
    pub root: Entity,
}

pub(crate) fn dialog_spec(dialog: &QuoinDialog, selected: Option<&OutputKey>) -> Option<DialogSpec> {
    if !dialog.wants_surface() {
        return None;
    }
    let seat = dialog.seat.as_ref()?;
    // A seat left on a removed output maps nowhere until a show retargets it.
    if Some(&seat.output) != selected {
        return None;
    }
    Some(DialogSpec {
        output: seat.output.clone(),
        // The fitted size: the authored one shrunk to the usable zone.
        size: dialog
            .size()
            .map(|size| (size.x.round().max(1.0) as u32, size.y.round().max(1.0) as u32))?,
        root: dialog.root?,
    })
}

/// Where comp puts the unanchored surface: centred in the zone the output
/// leaves after Quoin's docked panels' exclusive zones (`panel_layout`'s
/// canvas, the same zone the size is fitted to). Used to map dialog-local
/// input to output coordinates; comp's placement stays authoritative for the
/// pixels.
pub(crate) fn dialog_origin(_output: Vec2, size: (u32, u32), frame: &ShellFrame) -> Vec2 {
    let canvas = cosmix_shell::host::panel_layout(frame).canvas;
    let min = Vec2::new(canvas.x, canvas.y);
    let centre = min + Vec2::new(canvas.width, canvas.height) / 2.0;
    (centre - Vec2::new(size.0 as f32, size.1 as f32) / 2.0).round()
}

impl RunnerState {
    /// Map, keep or unmap the dialog surface to match the shared state.
    pub(super) fn reconcile_dialog(&mut self, qh: &QueueHandle<Self>) -> Result<(), LayerHostError> {
        let spec = self
            .app
            .world()
            .get_resource::<QuoinDialog>()
            .and_then(|dialog| dialog_spec(dialog, self.selected_key.as_ref()));
        let keep = matches!(
            (&self.dialog, &spec),
            (Some(native), Some(spec)) if native.output == spec.output && native.size == spec.size
        );
        if !keep && self.dialog.is_some() {
            self.close_dialog();
        }
        let Some(spec) = spec else {
            return Ok(());
        };
        let Some(output) = self.outputs.get(&spec.output) else {
            return Ok(());
        };
        let output_size = Vec2::new(output.logical_size.width(), output.logical_size.height());
        let origin = {
            let frame = &self.app.world().resource::<ShellFrameState>().0;
            dialog_origin(output_size, spec.size, frame)
        };
        if keep {
            let native = self.dialog.as_mut().expect("kept dialog exists");
            if native.origin != origin {
                native.origin = origin;
                self.app.world_mut().resource_mut::<QuoinDialog>().origin = Some(origin);
            }
            return Ok(());
        }
        // An open corner menu holds the exclusive keyboard and catches every
        // pointer frame on its output-sized layer; a dialog mapped under or
        // over it would be dead. Dismiss it first, as a new menu replaces an
        // old one (Stage R, GLM M2). A menu opened later is created after the
        // dialog, so it stacks above it and dismisses like any menu.
        if self.menu.is_some() || self.app.world().contains_resource::<cosmix_shell::chrome::corner_menu::CornerMenuRequest>() {
            self.dismiss_corner_menu(None);
        }
        let Some(output) = self.outputs.get(&spec.output) else {
            return Ok(());
        };
        let wl = self.compositor_state.create_surface(qh);
        let identity = crate::holders::new_layer_identity(&format!("{}-dialog", self.namespace));
        let layer = self.layer_shell.create_layer_surface(
            qh,
            wl.clone(),
            Layer::Overlay,
            Some(identity),
            Some(&output.wl_output),
        );
        layer.set_anchor(Anchor::empty());
        layer.set_size(spec.size.0, spec.size.1);
        layer.set_exclusive_zone(0);
        let keyboard = DialogKeyboard::Grabbing;
        layer.set_keyboard_interactivity(match keyboard.requested() {
            ProtocolKeyboardInteractivity::Exclusive => KeyboardInteractivity::Exclusive,
            ProtocolKeyboardInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
            ProtocolKeyboardInteractivity::None => KeyboardInteractivity::None,
        });
        // The tag's edge is never read for the dialog: its fractional scale
        // is matched by proxy before any panel is.
        let fractional = match (self.fractional_manager.as_ref(), self.viewporter.as_ref()) {
            (Some(manager), Some(viewporter)) => Some(FractionalObjects {
                scale: Some(manager.get_fractional_scale(&wl, qh, SurfaceTag { edge: Edge::Right })),
                viewport: Some(viewporter.get_viewport(&wl, qh, GlobalData)),
            }),
            _ => None,
        };
        let mut surface = PanelSurface::from_wayland(
            &mut self.app,
            &self.connection,
            wl,
            layer,
            output.logical_size,
            output.scale,
            Edge::Right,
            fractional,
            Some(spec.root),
        )
        .map_err(|error| LayerHostError::new(format!("dialog-raw-handle-failed-{error}")))?;
        surface.set_fixed_size(spec.size);
        self.app
            .world_mut()
            .get_mut::<Camera>(surface.camera)
            .expect("hosted render target has a camera")
            .clear_color = ClearColorConfig::Custom(Color::NONE);
        let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
        surface.apply_protocol_ops(&[ProtocolOp::CommitBufferless], elapsed);
        {
            let mut dialog = self.app.world_mut().resource_mut::<QuoinDialog>();
            dialog.window = Some(surface.window);
            dialog.origin = Some(origin);
            dialog.preedit = false;
        }
        tracing::debug!(
            event = "quoin_dialog_mapped",
            output = spec.output.as_str(),
            width = spec.size.0,
            height = spec.size.1,
            x = origin.x,
            y = origin.y
        );
        self.dialog = Some(NativeDialog {
            surface,
            output: spec.output,
            size: spec.size,
            origin,
            keyboard,
        });
        self.needs_update = true;
        Ok(())
    }

    /// Unmap: release input attributed to the window, drain the renderer,
    /// then drop the Wayland objects. The chrome root and scene stay.
    pub(super) fn close_dialog(&mut self) {
        let Some(mut native) = self.dialog.take() else {
            return;
        };
        let window = native.surface.window;
        if let Some(output) = self.selected_key.clone() {
            self.pointer_bridge.cleanup(&mut self.app, &output, Some(window));
        }
        self.keyboard_bridge.cleanup(&mut self.app, Some(window));
        self.touch_bridge.cleanup(&mut self.app, Some(window));
        native.surface.close(&mut self.app);
        self.app.update();
        native.surface.retire(&mut self.app);
        if let Some(mut dialog) = self.app.world_mut().get_resource_mut::<QuoinDialog>() {
            dialog.window = None;
            dialog.origin = None;
            dialog.preedit = false;
        }
        tracing::debug!(event = "quoin_dialog_unmapped");
        self.needs_update = true;
    }

    /// The output went away or the compositor closed the layer: unmap and
    /// clear `visible`, so the change publishes and a later show remaps.
    pub(super) fn drop_dialog(&mut self) {
        self.close_dialog();
        if let Some(mut dialog) = self.app.world_mut().get_resource_mut::<QuoinDialog>()
            && dialog.visible
        {
            dialog.visible = false;
        }
    }

    /// Grab then demote: the first keyboard enter on the dialog asks for
    /// `OnDemand`, which comp honours without taking focus away.
    pub(super) fn dialog_keyboard_entered(&mut self, surface: &wl_surface::WlSurface) {
        let Some(native) = self
            .dialog
            .as_mut()
            .filter(|native| native.surface.matches_surface(surface))
        else {
            return;
        };
        if let Some(interactivity) = native.keyboard.entered() {
            let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
            native.surface.apply_protocol_ops(
                &[
                    ProtocolOp::SetKeyboardInteractivity(interactivity),
                    ProtocolOp::Commit,
                ],
                elapsed,
            );
            tracing::debug!(event = "quoin_dialog_keyboard_demoted");
        }
    }

    pub(super) fn dialog_target(&self) -> Option<SurfaceTarget> {
        let native = self.dialog.as_ref()?;
        let output = self.outputs.get(&native.output)?;
        native.surface.wayland_surface().map(|surface| SurfaceTarget {
            surface,
            window: native.surface.window,
            output_size: Vec2::new(output.logical_size.width(), output.logical_size.height()),
            kind: SurfaceKind::Dialog {
                origin: native.origin,
            },
        })
    }

    pub(super) fn configure_dialog(
        &mut self,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: &LayerSurfaceConfigure,
    ) -> bool {
        let Some(native) = self
            .dialog
            .as_mut()
            .filter(|native| native.surface.matches_layer(layer))
        else {
            return false;
        };
        let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
        if let Err(error) = native.surface.configure(
            &mut self.app,
            qh,
            configure,
            elapsed,
            self.max_texture_dimension_2d,
        ) {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("dialog-configure-{}", error.reason_suffix()));
        }
        self.needs_update = true;
        true
    }

    pub(super) fn dialog_matches_layer(&self, layer: &LayerSurface) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|native| native.surface.matches_layer(layer))
    }

    pub(super) fn dialog_scale(
        &mut self,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale: i32,
    ) -> bool {
        let Some(native) = self
            .dialog
            .as_mut()
            .filter(|native| native.surface.matches_surface(surface))
        else {
            return false;
        };
        let Some(output) = self.outputs.get(&native.output) else {
            return true;
        };
        let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
        if let Err(error) = native.surface.update_output_metrics(
            &mut self.app,
            output.logical_size,
            scale,
            qh,
            elapsed,
            self.max_texture_dimension_2d,
        ) {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("dialog-scale-{}", error.reason_suffix()));
        }
        self.needs_update = true;
        true
    }

    pub(super) fn dialog_fractional_scale(
        &mut self,
        qh: &QueueHandle<Self>,
        proxy: &WpFractionalScaleV1,
        scale: u32,
    ) -> bool {
        let Some(native) = self
            .dialog
            .as_mut()
            .filter(|native| native.surface.matches_fractional_scale(proxy))
        else {
            return false;
        };
        let elapsed = self.app.world().resource::<Time<Real>>().elapsed();
        if let Err(error) = native.surface.set_fractional_scale(
            &mut self.app,
            scale,
            qh,
            elapsed,
            self.max_texture_dimension_2d,
        ) {
            self.abnormal_exit = true;
            self.exit_reason = Some(format!("dialog-scale-{}", error.reason_suffix()));
        } else {
            self.needs_update = true;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::DialogSeat;

    fn state(visible: bool, content: bool) -> QuoinDialog {
        let mut dialog = QuoinDialog {
            root: Some(Entity::from_raw_u32(7).unwrap()),
            content: content.then(|| Entity::from_raw_u32(8).unwrap()),
            ..Default::default()
        };
        dialog.set_seat(Some(DialogSeat {
            scene: "editor".into(),
            owner: "scenes".into(),
            accepted_at: 1,
            output: OutputKey::new("DP-1").unwrap(),
            w: 880.0,
            h: 620.0,
            title: None,
            chrome: true,
        }));
        if visible {
            dialog.show("editor");
        }
        dialog
    }

    #[test]
    fn a_dialog_maps_only_when_visible_mounted_and_on_the_selected_output() {
        let selected = OutputKey::new("DP-1").unwrap();
        assert_eq!(
            dialog_spec(&state(true, true), Some(&selected)),
            Some(DialogSpec {
                output: selected.clone(),
                size: (880, 620),
                root: Entity::from_raw_u32(7).unwrap(),
            })
        );
        // Loaded but hidden: the seat is reserved, nothing is mapped.
        assert_eq!(dialog_spec(&state(false, true), Some(&selected)), None);
        assert_eq!(dialog_spec(&state(true, false), Some(&selected)), None);
        let other = OutputKey::new("HDMI-A-1").unwrap();
        assert_eq!(dialog_spec(&state(true, true), Some(&other)), None);
        // A small output maps the fitted size, not the authored one.
        let mut fitted = state(true, true);
        fitted.fitted = Some(Vec2::new(880.0, 512.0));
        assert_eq!(dialog_spec(&fitted, Some(&selected)).unwrap().size, (880, 512));
        assert_eq!(dialog_spec(&state(true, true), None), None);
    }

    #[test]
    fn keyboard_grabs_on_every_map_and_demotes_once_after_enter() {
        let mut keyboard = DialogKeyboard::Grabbing;
        assert_eq!(keyboard.requested(), ProtocolKeyboardInteractivity::Exclusive);
        assert_eq!(keyboard.entered(), Some(ProtocolKeyboardInteractivity::OnDemand));
        assert_eq!(keyboard.requested(), ProtocolKeyboardInteractivity::OnDemand);
        assert_eq!(keyboard.entered(), None, "a later enter commits nothing");
    }

    #[test]
    fn origin_centres_in_the_zone_left_by_docked_panels() {
        let model = ShellModel::new(
            OutputKey::new("DP-1").unwrap(),
            LogicalSize::new(1920.0, 1080.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut frame = cosmix_shell::runtime::ShellFrame::from_model(&model);
        let output = Vec2::new(1920.0, 1080.0);
        assert_eq!(dialog_origin(output, (880, 620), &frame), Vec2::new(520.0, 230.0));
        // The 52 px bottom panel docked: comp centres 26 px higher (Stage S
        // note (a)).
        let bottom = &mut frame.panels[Edge::Bottom.index()];
        bottom.mapped = true;
        bottom.exclusive_zone_px = 52.0;
        assert_eq!(dialog_origin(output, (880, 620), &frame), Vec2::new(520.0, 204.0));
    }
}
