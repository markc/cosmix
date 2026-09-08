//! Shared non-winit Bevy render targets for native layer-shell surfaces.

use crate::scene::SceneCameraKind;
use bevy::camera::RenderTarget;
use bevy::prelude::*;
use bevy::window::{CompositeAlphaMode, PresentMode, RawHandleWrapper, WindowRef};

fn parse_quoin_present_mode(value: Option<&str>) -> Result<PresentMode, &'static str> {
    match value {
        None | Some("fifo") => Ok(PresentMode::Fifo),
        Some("auto-no-vsync") => Ok(PresentMode::AutoNoVsync),
        Some(_) => Err("COSMIX_QUOIN_PRESENT_MODE must be fifo or auto-no-vsync"),
    }
}
fn quoin_present_mode() -> PresentMode {
    static MODE: std::sync::OnceLock<PresentMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        let value = std::env::var("COSMIX_QUOIN_PRESENT_MODE").ok();
        let mode = parse_quoin_present_mode(value.as_deref()).expect("invalid Quoin present mode");
        if value.is_some() {
            tracing::info!(?mode, "QUOIN_PRESENT_MODE override");
        }
        mode
    })
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HostedRenderTarget {
    pub window: Entity,
    pub camera: Entity,
}

impl HostedRenderTarget {
    /// No raw handle is exposed to Bevy until the compositor configures size.
    pub fn spawn(app: &mut App, title: String) -> Self {
        let target = Self::spawn_with_camera(app, title, SceneCameraKind::TwoD);
        app.world_mut()
            .get_mut::<Window>(target.window)
            .unwrap()
            .present_mode = quoin_present_mode();
        target
    }

    pub fn spawn_with_camera(app: &mut App, title: String, kind: SceneCameraKind) -> Self {
        let window = app
            .world_mut()
            .spawn(Window {
                title,
                resolution: bevy::window::WindowResolution::new(1, 1)
                    .with_scale_factor_override(1.0),
                transparent: true,
                focused: false,
                composite_alpha_mode: CompositeAlphaMode::PreMultiplied,
                ..default()
            })
            .id();
        let camera = Camera {
            is_active: false,
            ..default()
        };
        let target = RenderTarget::Window(WindowRef::Entity(window));
        let camera = match kind {
            SceneCameraKind::TwoD => app.world_mut().spawn((Camera2d, camera, target)).id(),
            #[cfg(feature = "scene-3d")]
            SceneCameraKind::ThreeD => app
                .world_mut()
                .spawn((Camera3d::default(), camera, target))
                .id(),
        };
        Self { window, camera }
    }

    /// Caller must run a non-pipelined extraction update before dropping the
    /// corresponding native surface/retained raw-handle owner.
    pub fn detach(self, app: &mut App) {
        app.world_mut()
            .entity_mut(self.window)
            .remove::<RawHandleWrapper>();
        if let Some(mut camera) = app.world_mut().get_mut::<Camera>(self.camera) {
            camera.is_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoin_present_mode_keeps_fifo_default_and_validates_override() {
        assert_eq!(parse_quoin_present_mode(None), Ok(PresentMode::Fifo));
        assert_eq!(
            parse_quoin_present_mode(Some("fifo")),
            Ok(PresentMode::Fifo)
        );
        assert_eq!(
            parse_quoin_present_mode(Some("auto-no-vsync")),
            Ok(PresentMode::AutoNoVsync)
        );
        assert!(parse_quoin_present_mode(Some("immediate")).is_err());
    }
    #[test]
    fn target_starts_unconfigured_and_detaches_without_destroying_entities() {
        let mut app = App::new();
        let target = HostedRenderTarget::spawn(&mut app, "test background".into());
        assert!(app.world().get::<Camera2d>(target.camera).is_some());
        assert!(matches!(
            app.world().get::<Projection>(target.camera),
            Some(Projection::Orthographic(_))
        ));
        assert!(app.world().get::<RawHandleWrapper>(target.window).is_none());
        assert!(!app.world().get::<Camera>(target.camera).unwrap().is_active);
        app.world_mut()
            .get_mut::<Camera>(target.camera)
            .unwrap()
            .is_active = true;
        target.detach(&mut app);
        assert!(!app.world().get::<Camera>(target.camera).unwrap().is_active);
        assert!(app.world().get::<Window>(target.window).is_some());
    }

    #[cfg(feature = "scene-3d")]
    #[test]
    fn three_d_target_uses_perspective_and_preserves_host_lifetime() {
        let mut app = App::new();
        let target = HostedRenderTarget::spawn_with_camera(
            &mut app,
            "test 3d background".into(),
            SceneCameraKind::ThreeD,
        );
        assert!(app.world().get::<Camera3d>(target.camera).is_some());
        assert!(app.world().get::<Camera2d>(target.camera).is_none());
        assert!(matches!(
            app.world().get::<Projection>(target.camera),
            Some(Projection::Perspective(_))
        ));
        assert!(
            matches!(app.world().get::<RenderTarget>(target.camera), Some(RenderTarget::Window(WindowRef::Entity(entity))) if *entity == target.window)
        );
        assert!(!app.world().get::<Camera>(target.camera).unwrap().is_active);
        assert!(app.world().get::<RawHandleWrapper>(target.window).is_none());
        app.world_mut()
            .get_mut::<Camera>(target.camera)
            .unwrap()
            .is_active = true;
        target.detach(&mut app);
        assert!(!app.world().get::<Camera>(target.camera).unwrap().is_active);
        assert!(app.world().get::<Camera3d>(target.camera).is_some());
        assert!(app.world().get::<Window>(target.window).is_some());
    }
}
