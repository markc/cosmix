//! One layer-shell panel surface and its configure/map/unmap lifecycle.

#![deny(unsafe_code)]

use std::time::Duration;

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

/// Conservative fallback used because the render device is not available at
/// layer configure time. This matches wgpu's common default 2D texture limit.
const MAX_SURFACE_DIMENSION: u32 = 16_384;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SurfaceTag {
    pub edge: Edge,
}

#[derive(Debug)]
pub(crate) struct FractionalObjects {
    pub scale: WpFractionalScaleV1,
    pub viewport: WpViewport,
}

impl FractionalObjects {
    fn destroy(self) {
        self.viewport.destroy();
        self.scale.destroy();
    }
}

#[derive(Debug)]
struct WaylandObjects {
    fractional: Option<FractionalObjects>,
    // SCTK's drop implementation destroys the layer role before its owned
    // wl_surface. Keep it ahead of the retained raw owner for drop ordering.
    layer_surface: LayerSurface,
    raw_handle: RawHandleWrapper,
    _raw_owner: RetainedWindow,
}

impl WaylandObjects {
    fn destroy(mut self) {
        if let Some(fractional) = self.fractional.take() {
            fractional.destroy();
        }
        drop(self);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfacePhase {
    Unmapped,
    WaitingConfigure,
    Configured,
    PreparingUnmap,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigureEffect {
    Ignore,
    InitialMap,
    Update { size_changed: bool },
}

fn configure_effect(
    phase: SurfacePhase,
    previous: Option<(u32, u32)>,
    logical: (u32, u32),
) -> ConfigureEffect {
    match phase {
        SurfacePhase::WaitingConfigure => ConfigureEffect::InitialMap,
        SurfacePhase::Configured => ConfigureEffect::Update {
            size_changed: previous != Some(logical),
        },
        SurfacePhase::PreparingUnmap | SurfacePhase::Unmapped | SurfacePhase::Closed => {
            ConfigureEffect::Ignore
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SurfaceSizeError {
    InvalidScale(f64),
    LogicalOutOfRange { width: u32, height: u32 },
    PhysicalOutOfRange { width: f64, height: f64 },
}

impl SurfaceSizeError {
    pub(crate) fn reason_suffix(self) -> String {
        match self {
            Self::InvalidScale(scale) => format!("invalid-scale-{scale}"),
            Self::LogicalOutOfRange { width, height } => {
                format!("logical-{width}x{height}")
            }
            Self::PhysicalOutOfRange { width, height } => {
                format!("physical-{width:.0}x{height:.0}")
            }
        }
    }
}

fn validate_surface_size(logical: (u32, u32), scale: f64) -> Result<(u32, u32), SurfaceSizeError> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(SurfaceSizeError::InvalidScale(scale));
    }
    if logical.0 == 0
        || logical.1 == 0
        || logical.0 > i32::MAX as u32
        || logical.1 > i32::MAX as u32
    {
        return Err(SurfaceSizeError::LogicalOutOfRange {
            width: logical.0,
            height: logical.1,
        });
    }
    let physical = (f64::from(logical.0) * scale, f64::from(logical.1) * scale);
    if !physical.0.is_finite()
        || !physical.1.is_finite()
        || physical.0.ceil() > f64::from(MAX_SURFACE_DIMENSION)
        || physical.1.ceil() > f64::from(MAX_SURFACE_DIMENSION)
    {
        return Err(SurfaceSizeError::PhysicalOutOfRange {
            width: physical.0.ceil(),
            height: physical.1.ceil(),
        });
    }
    Ok((physical.0.ceil() as u32, physical.1.ceil() as u32))
}

#[derive(Debug)]
pub struct PanelSurface {
    pub edge: Edge,
    pub window: Entity,
    pub camera: Entity,
    pub mount: Entity,
    pub phase: SurfacePhase,
    pub last_committed: Option<PanelPresentation>,
    pub presented: bool,
    pub frame_pending: bool,
    pub frame_requested_at: Option<Duration>,
    pub waiting_configure_since: Option<Duration>,
    wayland: Option<WaylandObjects>,
    output_size: LogicalSize,
    output_scale: i32,
    preferred_fractional_scale: Option<f64>,
    configured_logical_size: Option<(u32, u32)>,
    announced_scale: Option<f64>,
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
        let window = app
            .world_mut()
            .spawn(Window {
                title: format!("Cosmix Quoin — {edge:?}"),
                // The compositor configure is validated before the real WSI
                // dimensions are installed.
                resolution: bevy::window::WindowResolution::new(1, 1)
                    .with_scale_factor_override(1.0),
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
        let (_raw_owner, raw_handle) = retained_raw_handle(connection.clone(), wl_surface)?;
        Ok(Self {
            edge,
            window,
            camera,
            mount,
            phase: SurfacePhase::Unmapped,
            last_committed: None,
            presented: false,
            frame_pending: false,
            frame_requested_at: None,
            waiting_configure_since: None,
            wayland: Some(WaylandObjects {
                fractional,
                layer_surface,
                raw_handle,
                _raw_owner,
            }),
            output_size,
            output_scale: output_scale.max(1),
            preferred_fractional_scale: None,
            configured_logical_size: None,
            announced_scale: None,
        })
    }

    pub(crate) fn install_wayland(
        &mut self,
        connection: &Connection,
        wl_surface: wl_surface::WlSurface,
        layer_surface: LayerSurface,
        fractional: Option<FractionalObjects>,
    ) -> Result<(), RawHandleError> {
        debug_assert!(self.wayland.is_none());
        let (_raw_owner, raw_handle) = retained_raw_handle(connection.clone(), wl_surface)?;
        self.wayland = Some(WaylandObjects {
            fractional,
            layer_surface,
            raw_handle,
            _raw_owner,
        });
        self.preferred_fractional_scale = None;
        self.announced_scale = None;
        self.frame_requested_at = None;
        self.waiting_configure_since = None;
        Ok(())
    }

    pub(crate) fn has_wayland_objects(&self) -> bool {
        self.wayland.is_some()
    }

    pub(crate) fn matches_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.wayland
            .as_ref()
            .is_some_and(|objects| objects.layer_surface.wl_surface() == surface)
    }

    pub(crate) fn matches_layer(&self, layer: &LayerSurface) -> bool {
        self.wayland
            .as_ref()
            .is_some_and(|objects| objects.layer_surface == *layer)
    }

    pub(crate) fn matches_fractional_scale(&self, scale: &WpFractionalScaleV1) -> bool {
        self.wayland
            .as_ref()
            .and_then(|objects| objects.fractional.as_ref())
            .is_some_and(|fractional| fractional.scale == *scale)
    }

    pub fn mounts(panels: &[Self; 4]) -> QuoinPanelMounts {
        QuoinPanelMounts::for_layer_surfaces(
            panels[Edge::Left.index()].mount,
            panels[Edge::Bottom.index()].mount,
            panels[Edge::Right.index()].mount,
            panels[Edge::Top.index()].mount,
        )
    }

    pub fn apply_protocol_ops(&mut self, operations: &[ProtocolOp], elapsed: Duration) {
        if operations.is_empty() {
            return;
        }
        let objects = self
            .wayland
            .as_ref()
            .expect("mapped panel has fresh Wayland objects");
        for operation in operations {
            match *operation {
                ProtocolOp::CreateSurface => {
                    // Runner creates the objects before replay reaches here.
                }
                ProtocolOp::SetLayer(layer) => objects.layer_surface.set_layer(match layer {
                    ProtocolLayer::Top => Layer::Top,
                    ProtocolLayer::Overlay => Layer::Overlay,
                }),
                ProtocolOp::SetAnchor(anchor) => {
                    objects.layer_surface.set_anchor(sctk_anchor(anchor));
                }
                ProtocolOp::SetSize { width, height } => {
                    objects.layer_surface.set_size(width, height);
                }
                ProtocolOp::SetExclusiveZone(zone) => {
                    objects.layer_surface.set_exclusive_zone(zone);
                }
                ProtocolOp::SetMargin(margin) => objects.layer_surface.set_margin(
                    margin.top,
                    margin.right,
                    margin.bottom,
                    margin.left,
                ),
                ProtocolOp::SetKeyboardInteractivity(interactivity) => objects
                    .layer_surface
                    .set_keyboard_interactivity(match interactivity {
                        ProtocolKeyboardInteractivity::None => KeyboardInteractivity::None,
                        ProtocolKeyboardInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
                    }),
                ProtocolOp::CommitBufferless => {
                    objects.layer_surface.commit();
                    self.phase = SurfacePhase::WaitingConfigure;
                    self.waiting_configure_since = Some(elapsed);
                    self.presented = false;
                }
                ProtocolOp::Commit => objects.layer_surface.commit(),
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
            self.frame_requested_at = None;
            self.waiting_configure_since = None;
        }
    }

    pub fn finish_unmap(&mut self) {
        if self.phase == SurfacePhase::PreparingUnmap {
            if let Some(objects) = self.wayland.take() {
                objects.destroy();
            }
            self.phase = SurfacePhase::Unmapped;
            self.configured_logical_size = None;
            self.announced_scale = None;
            self.presented = false;
            self.frame_requested_at = None;
            self.waiting_configure_since = None;
        }
    }

    pub(crate) fn configure<State>(
        &mut self,
        app: &mut App,
        qh: &QueueHandle<State>,
        configure: &LayerSurfaceConfigure,
        elapsed: Duration,
    ) -> Result<(), SurfaceSizeError>
    where
        State: wayland_client::Dispatch<
                wayland_client::protocol::wl_callback::WlCallback,
                wl_surface::WlSurface,
            > + 'static,
    {
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
        let effect = configure_effect(self.phase, self.configured_logical_size, logical);
        if effect == ConfigureEffect::Ignore {
            return Ok(());
        }
        let scale = self.effective_scale();
        let physical = validate_surface_size(logical, scale)?;
        let scale_changed = self.announced_scale != Some(scale);
        self.configured_logical_size = Some(logical);
        self.apply_scale_to_surface();
        self.update_bevy_window(app, physical, scale);
        match effect {
            ConfigureEffect::InitialMap => {
                self.waiting_configure_since = None;
                let raw_handle = self
                    .wayland
                    .as_ref()
                    .expect("configured panel has Wayland objects")
                    .raw_handle
                    .clone();
                app.world_mut().entity_mut(self.window).insert(raw_handle);
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
                self.request_frame(qh, elapsed);
                self.phase = SurfacePhase::Configured;
            }
            ConfigureEffect::Update { size_changed } => {
                if size_changed {
                    app.world_mut().write_message(WindowResized {
                        window: self.window,
                        width: logical.0 as f32,
                        height: logical.1 as f32,
                    });
                    self.presented = false;
                    self.request_frame(qh, elapsed);
                }
            }
            ConfigureEffect::Ignore => unreachable!("ignored configure returned above"),
        }
        if scale_changed {
            app.world_mut().write_message(WindowScaleFactorChanged {
                window: self.window,
                scale_factor: scale,
            });
            self.announced_scale = Some(scale);
        }
        Ok(())
    }

    pub(crate) fn set_fractional_scale(
        &mut self,
        app: &mut App,
        preferred_scale: u32,
    ) -> Result<(), SurfaceSizeError> {
        if preferred_scale == 0 {
            return Ok(());
        }
        let scale = f64::from(preferred_scale) / 120.0;
        let physical = self
            .configured_logical_size
            .map(|logical| validate_surface_size(logical, scale))
            .transpose()?;
        let scale_changed = self.announced_scale != Some(scale);
        self.preferred_fractional_scale = Some(scale);
        self.apply_scale_to_surface();
        if let (Some(logical), Some(physical)) = (self.configured_logical_size, physical) {
            self.update_bevy_window(app, physical, scale);
            app.world_mut().write_message(WindowResized {
                window: self.window,
                width: logical.0 as f32,
                height: logical.1 as f32,
            });
            if scale_changed {
                app.world_mut().write_message(WindowScaleFactorChanged {
                    window: self.window,
                    scale_factor: scale,
                });
                self.announced_scale = Some(scale);
            }
        }
        Ok(())
    }

    pub(crate) fn update_output_metrics(
        &mut self,
        app: &mut App,
        logical_size: LogicalSize,
        integer_scale: i32,
    ) -> Result<(), SurfaceSizeError> {
        let scale = self
            .preferred_fractional_scale
            .unwrap_or(f64::from(integer_scale.max(1)));
        let physical = self
            .configured_logical_size
            .map(|logical| validate_surface_size(logical, scale))
            .transpose()?;
        let scale_changed = self.announced_scale != Some(scale);
        self.output_size = logical_size;
        self.output_scale = integer_scale.max(1);
        self.apply_scale_to_surface();
        if let (Some(_), Some(physical)) = (self.configured_logical_size, physical) {
            self.update_bevy_window(app, physical, scale);
            if scale_changed {
                app.world_mut().write_message(WindowScaleFactorChanged {
                    window: self.window,
                    scale_factor: scale,
                });
                self.announced_scale = Some(scale);
            }
        }
        Ok(())
    }

    pub fn request_frame<State>(&mut self, qh: &QueueHandle<State>, elapsed: Duration)
    where
        State: wayland_client::Dispatch<
                wayland_client::protocol::wl_callback::WlCallback,
                wl_surface::WlSurface,
            > + 'static,
    {
        if (self.phase == SurfacePhase::Configured || self.phase == SurfacePhase::WaitingConfigure)
            && !self.frame_pending
        {
            let surface = self
                .wayland
                .as_ref()
                .expect("mapped panel has Wayland objects")
                .layer_surface
                .wl_surface();
            surface.frame(qh, surface.clone());
            self.frame_pending = true;
            self.frame_requested_at = Some(elapsed);
        }
    }

    pub fn commit_frame_request(&self) {
        self.wayland
            .as_ref()
            .expect("mapped panel has Wayland objects")
            .layer_surface
            .commit();
    }

    pub fn frame_done(&mut self) {
        self.frame_pending = false;
        self.frame_requested_at = None;
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
        self.frame_requested_at = None;
        self.waiting_configure_since = None;
    }

    pub fn clear_overdue_frame(&mut self, elapsed: Duration, backstop: Duration) -> bool {
        if self.frame_pending
            && self
                .frame_requested_at
                .is_some_and(|requested| elapsed >= requested.saturating_add(backstop))
        {
            self.frame_pending = false;
            self.frame_requested_at = None;
            true
        } else {
            false
        }
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
        let Some(objects) = &self.wayland else {
            return;
        };
        let wl_surface = objects.layer_surface.wl_surface();
        if self.preferred_fractional_scale.is_some() {
            if wl_surface.version() >= 3 {
                wl_surface.set_buffer_scale(1);
            }
            if let (Some(fractional), Some((width, height))) =
                (&objects.fractional, self.configured_logical_size)
            {
                fractional
                    .viewport
                    .set_destination(width as i32, height as i32);
            }
        } else if wl_surface.version() >= 3 {
            wl_surface.set_buffer_scale(self.output_scale);
        }
    }

    fn update_bevy_window(&self, app: &mut App, physical: (u32, u32), scale: f64) {
        if let Some(mut window) = app.world_mut().get_mut::<Window>(self.window) {
            window
                .resolution
                .set_scale_factor_override(Some(scale as f32));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_phase_table_applies_only_to_live_mapped_roles() {
        let previous = Some((1920, 32));
        let resized = (1280, 32);
        let cases = [
            (SurfacePhase::Unmapped, ConfigureEffect::Ignore),
            (SurfacePhase::WaitingConfigure, ConfigureEffect::InitialMap),
            (
                SurfacePhase::Configured,
                ConfigureEffect::Update { size_changed: true },
            ),
            (SurfacePhase::PreparingUnmap, ConfigureEffect::Ignore),
            (SurfacePhase::Closed, ConfigureEffect::Ignore),
        ];
        for (phase, expected) in cases {
            assert_eq!(
                configure_effect(phase, previous, resized),
                expected,
                "{phase:?}"
            );
        }
        assert_eq!(
            configure_effect(SurfacePhase::Configured, previous, (1920, 32)),
            ConfigureEffect::Update {
                size_changed: false
            }
        );
    }

    #[test]
    fn configure_size_validator_rejects_protocol_and_gpu_overflow() {
        assert_eq!(validate_surface_size((1280, 32), 1.25), Ok((1600, 40)));
        assert!(matches!(
            validate_surface_size((u32::MAX, 32), 1.0),
            Err(SurfaceSizeError::LogicalOutOfRange { .. })
        ));
        assert!(matches!(
            validate_surface_size((8193, 32), 2.0),
            Err(SurfaceSizeError::PhysicalOutOfRange { .. })
        ));
        assert!(matches!(
            validate_surface_size((32, 32), f64::INFINITY),
            Err(SurfaceSizeError::InvalidScale(_))
        ));
    }
}
