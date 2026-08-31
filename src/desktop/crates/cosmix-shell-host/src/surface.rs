//! One layer-shell panel surface and its configure/map/unmap lifecycle.

#![deny(unsafe_code)]

use bevy::camera::RenderTarget;
use bevy::prelude::*;
use bevy::ui::{UiTargetCamera, percent};
use bevy::window::{
    CompositeAlphaMode, RawHandleWrapper, WindowCreated, WindowRef, WindowResized,
    WindowScaleFactorChanged,
};
use cosmix_shell::chrome::QuoinPanelMounts;
use cosmix_shell::core::PanelMode;
use cosmix_shell::core::{Edge, LogicalSize};
use cosmix_shell::runtime::PanelPresentation;
use smithay_client_toolkit::shell::{
    WaylandSurface,
    wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface, LayerSurfaceConfigure},
};
use wayland_client::{Connection, Proxy, QueueHandle, protocol::wl_surface};
use wayland_protocols::wp::{
    fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1,
    viewporter::client::wp_viewport::WpViewport,
};

use crate::planner::{ProtocolAnchor, ProtocolKeyboardInteractivity, ProtocolLayer, ProtocolOp};
use crate::raw_handle::{RawHandleError, RetainedWindow, retained_raw_handle};

#[derive(Clone, Copy, Debug)]
pub(crate) struct SurfaceTag {
    pub edge: Edge,
}

#[derive(Debug)]
pub(crate) struct FractionalObjects {
    pub _scale: WpFractionalScaleV1,
    pub viewport: WpViewport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfacePhase {
    Unmapped,
    WaitingConfigure,
    Configured,
    PreparingUnmap,
    Closed,
}

#[derive(Debug)]
pub struct PanelSurface {
    pub edge: Edge,
    pub wl_surface: wl_surface::WlSurface,
    pub layer_surface: LayerSurface,
    pub window: Entity,
    pub camera: Entity,
    pub mount: Entity,
    pub phase: SurfacePhase,
    pub last_committed: Option<PanelPresentation>,
    pub presented: bool,
    pub frame_pending: bool,
    raw_handle: RawHandleWrapper,
    // Kept after unmap so the same protocol role can be replayed. It is
    // dropped only after renderer extraction has drained during teardown.
    _raw_owner: RetainedWindow,
    fractional: Option<FractionalObjects>,
    output_size: LogicalSize,
    output_scale: i32,
    preferred_fractional_scale: Option<f64>,
    configured_logical_size: Option<(u32, u32)>,
}

impl PanelSurface {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_wayland(
        app: &mut App,
        connection: &Connection,
        wl_surface: wl_surface::WlSurface,
        layer_surface: LayerSurface,
        output_size: LogicalSize,
        output_scale: i32,
        edge: Edge,
        fractional: Option<FractionalObjects>,
        retained_mount: Option<Entity>,
    ) -> Result<Self, RawHandleError> {
        let initial_logical = requested_logical_size(edge, output_size, 1.0);
        let scale = output_scale.max(1) as f32;
        let physical = (
            (initial_logical.0 as f32 * scale).ceil() as u32,
            (initial_logical.1 as f32 * scale).ceil() as u32,
        );
        let window = app
            .world_mut()
            .spawn(Window {
                title: format!("Cosmix Quoin — {edge:?}"),
                resolution: bevy::window::WindowResolution::new(physical.0, physical.1)
                    .with_scale_factor_override(scale),
                transparent: true,
                composite_alpha_mode: CompositeAlphaMode::PreMultiplied,
                ..default()
            })
            .id();
        let camera = app
            .world_mut()
            .spawn((
                Camera2d,
                Camera {
                    is_active: false,
                    ..default()
                },
                RenderTarget::Window(WindowRef::Entity(window)),
            ))
            .id();
        let mount = if let Some(mount) = retained_mount {
            app.world_mut()
                .entity_mut(mount)
                .insert(UiTargetCamera(camera));
            mount
        } else {
            app.world_mut()
                .spawn((
                    Node {
                        width: percent(100),
                        height: percent(100),
                        ..default()
                    },
                    UiTargetCamera(camera),
                ))
                .id()
        };
        let (_raw_owner, raw_handle) = retained_raw_handle(connection.clone(), wl_surface.clone())?;
        Ok(Self {
            edge,
            wl_surface,
            layer_surface,
            window,
            camera,
            mount,
            phase: SurfacePhase::Unmapped,
            last_committed: None,
            presented: false,
            frame_pending: false,
            raw_handle,
            _raw_owner,
            fractional,
            output_size,
            output_scale: output_scale.max(1),
            preferred_fractional_scale: None,
            configured_logical_size: None,
        })
    }

    pub fn mounts(panels: &[Self; 4]) -> QuoinPanelMounts {
        QuoinPanelMounts::for_layer_surfaces(
            panels[Edge::Left.index()].mount,
            panels[Edge::Bottom.index()].mount,
            panels[Edge::Right.index()].mount,
            panels[Edge::Top.index()].mount,
        )
    }

    pub fn apply_protocol_ops(&mut self, operations: &[ProtocolOp]) {
        for operation in operations {
            match *operation {
                ProtocolOp::SetLayer(layer) => self.layer_surface.set_layer(match layer {
                    ProtocolLayer::Top => Layer::Top,
                    ProtocolLayer::Overlay => Layer::Overlay,
                }),
                ProtocolOp::SetAnchor(anchor) => {
                    self.layer_surface.set_anchor(sctk_anchor(anchor));
                }
                ProtocolOp::SetSize { width, height } => {
                    self.layer_surface.set_size(width, height);
                }
                ProtocolOp::SetExclusiveZone(zone) => {
                    self.layer_surface.set_exclusive_zone(zone);
                }
                ProtocolOp::SetMargin(margin) => self.layer_surface.set_margin(
                    margin.top,
                    margin.right,
                    margin.bottom,
                    margin.left,
                ),
                ProtocolOp::SetKeyboardInteractivity(interactivity) => self
                    .layer_surface
                    .set_keyboard_interactivity(match interactivity {
                        ProtocolKeyboardInteractivity::None => KeyboardInteractivity::None,
                        ProtocolKeyboardInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
                    }),
                ProtocolOp::CommitBufferless => {
                    self.layer_surface.commit();
                    self.phase = SurfacePhase::WaitingConfigure;
                    self.presented = false;
                }
                ProtocolOp::Commit => self.layer_surface.commit(),
                ProtocolOp::Unmap => {
                    // Runner performs this split operation around one Bevy drain update.
                }
            }
        }
    }

    pub fn begin_unmap(&mut self, app: &mut App) {
        if matches!(
            self.phase,
            SurfacePhase::Configured | SurfacePhase::WaitingConfigure
        ) {
            app.world_mut()
                .entity_mut(self.window)
                .remove::<RawHandleWrapper>();
            if let Some(mut camera) = app.world_mut().get_mut::<Camera>(self.camera) {
                camera.is_active = false;
            }
            self.phase = SurfacePhase::PreparingUnmap;
            self.frame_pending = false;
        }
    }

    pub fn finish_unmap(&mut self) {
        if self.phase == SurfacePhase::PreparingUnmap {
            self.layer_surface.attach(None, 0, 0);
            self.layer_surface.commit();
            self.phase = SurfacePhase::Unmapped;
            self.configured_logical_size = None;
            self.presented = false;
        }
    }

    pub fn configure<State>(
        &mut self,
        app: &mut App,
        qh: &QueueHandle<State>,
        configure: &LayerSurfaceConfigure,
    ) where
        State: wayland_client::Dispatch<
                wayland_client::protocol::wl_callback::WlCallback,
                wl_surface::WlSurface,
            > + 'static,
    {
        if self.phase != SurfacePhase::WaitingConfigure {
            return;
        }
        let thickness = self
            .last_committed
            .as_ref()
            .map_or(1.0, |panel| panel.thickness_px);
        let requested = requested_logical_size(self.edge, self.output_size, thickness);
        let logical = (
            if configure.new_size.0 == 0 {
                requested.0
            } else {
                configure.new_size.0
            },
            if configure.new_size.1 == 0 {
                requested.1
            } else {
                configure.new_size.1
            },
        );
        self.configured_logical_size = Some(logical);
        self.apply_scale_to_surface();
        self.update_bevy_window(app, logical);
        app.world_mut()
            .entity_mut(self.window)
            .insert(self.raw_handle.clone());
        if let Some(mut camera) = app.world_mut().get_mut::<Camera>(self.camera) {
            camera.is_active = true;
        }
        app.world_mut().write_message(WindowCreated {
            window: self.window,
        });
        app.world_mut().write_message(WindowResized {
            window: self.window,
            width: logical.0 as f32,
            height: logical.1 as f32,
        });
        app.world_mut().write_message(WindowScaleFactorChanged {
            window: self.window,
            scale_factor: self.effective_scale(),
        });
        self.request_frame(qh);
        self.phase = SurfacePhase::Configured;
    }

    pub fn set_fractional_scale(&mut self, app: &mut App, preferred_scale: u32) {
        if preferred_scale == 0 {
            return;
        }
        self.preferred_fractional_scale = Some(f64::from(preferred_scale) / 120.0);
        self.apply_scale_to_surface();
        if let Some(logical) = self.configured_logical_size {
            self.update_bevy_window(app, logical);
            app.world_mut().write_message(WindowResized {
                window: self.window,
                width: logical.0 as f32,
                height: logical.1 as f32,
            });
            app.world_mut().write_message(WindowScaleFactorChanged {
                window: self.window,
                scale_factor: self.effective_scale(),
            });
        }
    }

    pub fn update_output_metrics(
        &mut self,
        app: &mut App,
        logical_size: LogicalSize,
        integer_scale: i32,
    ) {
        self.output_size = logical_size;
        self.output_scale = integer_scale.max(1);
        self.apply_scale_to_surface();
        if let Some(logical) = self.configured_logical_size {
            self.update_bevy_window(app, logical);
            app.world_mut().write_message(WindowScaleFactorChanged {
                window: self.window,
                scale_factor: self.effective_scale(),
            });
        }
    }

    pub fn request_frame<State>(&mut self, qh: &QueueHandle<State>)
    where
        State: wayland_client::Dispatch<
                wayland_client::protocol::wl_callback::WlCallback,
                wl_surface::WlSurface,
            > + 'static,
    {
        if (self.phase == SurfacePhase::Configured || self.phase == SurfacePhase::WaitingConfigure)
            && !self.frame_pending
        {
            self.wl_surface.frame(qh, self.wl_surface.clone());
            self.frame_pending = true;
        }
    }

    pub fn commit_frame_request(&self) {
        self.layer_surface.commit();
    }

    pub fn frame_done(&mut self) {
        self.frame_pending = false;
        if self.phase == SurfacePhase::Configured {
            self.presented = true;
        }
    }

    pub fn wants_animation_callback(&self) -> bool {
        self.last_committed.as_ref().is_some_and(|panel| {
            panel.mapped
                && match panel.mode {
                    PanelMode::Pinned | PanelMode::Revealed => panel.visible_fraction < 1.0,
                    PanelMode::Hidden => panel.visible_fraction > 0.0,
                }
        })
    }

    pub fn close(&mut self, app: &mut App) {
        app.world_mut()
            .entity_mut(self.window)
            .remove::<RawHandleWrapper>();
        if let Some(mut camera) = app.world_mut().get_mut::<Camera>(self.camera) {
            camera.is_active = false;
        }
        self.phase = SurfacePhase::Closed;
        self.frame_pending = false;
    }

    /// Drop a drained protocol/WSI owner while preserving the mount and its
    /// chrome subtree for a replacement output runtime.
    pub fn retire(self, app: &mut App) {
        app.world_mut().despawn(self.camera);
        app.world_mut().despawn(self.window);
    }

    fn effective_scale(&self) -> f64 {
        self.preferred_fractional_scale
            .unwrap_or(f64::from(self.output_scale))
    }

    fn apply_scale_to_surface(&self) {
        if self.preferred_fractional_scale.is_some() {
            if self.wl_surface.version() >= 3 {
                self.wl_surface.set_buffer_scale(1);
            }
            if let (Some(fractional), Some((width, height))) =
                (&self.fractional, self.configured_logical_size)
            {
                fractional
                    .viewport
                    .set_destination(width as i32, height as i32);
            }
        } else if self.wl_surface.version() >= 3 {
            self.wl_surface.set_buffer_scale(self.output_scale);
        }
    }

    fn update_bevy_window(&self, app: &mut App, logical: (u32, u32)) {
        let scale = self.effective_scale() as f32;
        let physical = (
            (logical.0 as f32 * scale).ceil().max(1.0) as u32,
            (logical.1 as f32 * scale).ceil().max(1.0) as u32,
        );
        if let Some(mut window) = app.world_mut().get_mut::<Window>(self.window) {
            window.resolution.set_scale_factor_override(Some(scale));
            window
                .resolution
                .set_physical_resolution(physical.0, physical.1);
        }
    }
}

fn requested_logical_size(edge: Edge, output: LogicalSize, thickness: f32) -> (u32, u32) {
    let thickness = thickness.round().max(1.0) as u32;
    match edge {
        Edge::Left | Edge::Right => (thickness, output.height().round().max(1.0) as u32),
        Edge::Top | Edge::Bottom => (output.width().round().max(1.0) as u32, thickness),
    }
}

fn sctk_anchor(anchor: ProtocolAnchor) -> Anchor {
    let mut value = Anchor::empty();
    if anchor.top {
        value |= Anchor::TOP;
    }
    if anchor.right {
        value |= Anchor::RIGHT;
    }
    if anchor.bottom {
        value |= Anchor::BOTTOM;
    }
    if anchor.left {
        value |= Anchor::LEFT;
    }
    value
}
