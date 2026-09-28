//! In-process shell input ownership. Smithay retains client/window grabs;
//! only presses beginning on visible native panels go to Bevy picking.
use bevy::{
    camera::RenderTarget,
    picking::{
        PickingSystems,
        pointer::{Location, PointerAction, PointerButton, PointerId, PointerInput},
    },
    prelude::*,
};
use cosmix_quoin::embedded::{
    EmbeddedIconScale, EmbeddedOutput, EmbeddedPanelRegions, EmbeddedQuoinPlugin, EmbeddedWorkArea,
};
use cosmix_shell::{host::PanelRect, runtime::ShellRuntimeSet};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct InputState {
    suspended: bool,
    regions: Vec<PanelRect>,
    held: BTreeSet<u32>,
    buttons: Vec<(Vec2, u32, bool)>,
    scroll: Vec2,
    cancel: bool,
}

#[derive(Resource, Clone, Default)]
pub(crate) struct EmbeddedShellBridge(Arc<Mutex<InputState>>);

/// Forward the KMS output scale to the embedded shell's icon rasteriser.
/// Under kms-live there is no winit window, so the scene renderer's own
/// heuristic (`Window::scale_factor`) reads 1.0 while this canvas really
/// draws at `--scale` — without the forward, every panel icon rasterised at
/// logical size and drew linear-upscaled (soft icons at any source
/// resolution). Runs in PreUpdate, ahead of the shell's Input-set forward.
fn forward_output_icon_scale(
    scale: Option<Res<crate::compositor_scene::RendererOutputScale120>>,
    mut icon: ResMut<EmbeddedIconScale>,
) {
    // An injected 1.0 carries nothing the window heuristic lacks (under
    // kms-live the heuristic also reads 1.0), while in a nested winit host
    // a default 120 output scale sits next to a real HiDPI window scale —
    // pinning 1.0 there would regress dev-mode icons. `None` defers to the
    // heuristic wherever the KMS scale says 1:1.
    let value = scale
        .map(|s| f32::from(s.0) / 120.0)
        .filter(|scale| *scale != 1.0);
    if icon.0 != value {
        icon.0 = value;
    }
}

impl EmbeddedShellBridge {
    #[cfg(test)]
    pub(crate) fn for_test(regions: Vec<PanelRect>) -> Self {
        let bridge = Self::default();
        bridge.0.lock().unwrap().regions = regions;
        bridge
    }
    pub(crate) fn suspend(&self, suspended: bool) {
        self.reset();
        self.0.lock().unwrap_or_else(|p| p.into_inner()).suspended = suspended;
    }
    pub(crate) fn held(&self) -> bool {
        !self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .held
            .is_empty()
    }
    pub(crate) fn scroll(&self, delta: Vec2) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.scroll += delta;
    }
    pub(crate) fn covers(&self, x: f64, y: f64) -> bool {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        !state.suspended && (!state.held.is_empty() || covers(&state.regions, x, y))
    }

    pub(crate) fn button(
        &self,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        client_grab: bool,
    ) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let owned = state.held.contains(&button);
        if state.suspended {
            return false;
        }
        if !owned && (client_grab || !covers(&state.regions, x, y)) {
            return false;
        }
        if pressed {
            state.held.insert(button);
        } else {
            state.held.remove(&button);
        }
        if state.buttons.len() >= 64 {
            state.buttons.clear();
            state.cancel = true;
        } else {
            state
                .buttons
                .push((Vec2::new(x as f32, y as f32), button, pressed));
        }
        true
    }

    pub(crate) fn reset(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.regions.clear();
        state.buttons.clear();
        state.held.clear();
        state.scroll = Vec2::ZERO;
        state.cancel = true;
    }
}

fn covers(regions: &[PanelRect], x: f64, y: f64) -> bool {
    regions.iter().any(|r| {
        x >= f64::from(r.x)
            && y >= f64::from(r.y)
            && x < f64::from(r.x + r.width)
            && y < f64::from(r.y + r.height)
    })
}

/// `comp_service` is this compositor's registered Bus service name, so the
/// embedded Quoin's hotspot observer mirrors its own instance even when it
/// runs as e.g. `comp-nested`.
pub(crate) fn install(app: &mut App, comp_service: &str) {
    if std::env::var("COSMIX_COMP_EMBEDDED_QUOIN").as_deref() != Ok("1") {
        return;
    }
    let bridge = EmbeddedShellBridge::default();
    app.insert_resource(bridge)
        .add_plugins(EmbeddedQuoinPlugin::new().with_comp_service(comp_service))
        .init_resource::<EmbeddedIconScale>()
        .add_systems(Startup, attach_protocol)
        .add_systems(
            PreUpdate,
            (forward_output_icon_scale, pointer_input.before(PickingSystems::ProcessInput)),
        )
        .add_systems(Update, publish_regions.after(ShellRuntimeSet::Host));
}

fn attach_protocol(
    mut commands: Commands,
    feed: Res<crate::protocol::ClientSceneFeed>,
    bridge: Res<EmbeddedShellBridge>,
) {
    feed.install_embedded_shell(bridge.clone());
    commands.insert_resource(cosmix_shell::runtime::ShellQuitHandler(
        feed.native_quit_callback(),
    ));
}

fn publish_regions(
    bridge: Res<EmbeddedShellBridge>,
    regions: Res<EmbeddedPanelRegions>,
    area: Res<EmbeddedWorkArea>,
    feed: Res<crate::protocol::ClientSceneFeed>,
    mut previous: Local<Option<PanelRect>>,
) {
    if area.0.is_none() && previous.is_some() {
        bridge.reset();
    }
    bridge
        .0
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .regions
        .clone_from(&regions.0);
    if *previous != area.0 {
        feed.native_work_area(area.0);
        *previous = area.0;
    }
}

fn pointer_input(
    bridge: Res<EmbeddedShellBridge>,
    output: Res<EmbeddedOutput>,
    feed: Res<crate::protocol::ClientSceneFeed>,
    scale: Res<UiScale>,
    targets: Query<&RenderTarget>,
    mut inputs: MessageWriter<PointerInput>,
    mut previous: Local<Option<Vec2>>,
) {
    let Some(target) = output
        .camera
        .and_then(|id| targets.get(id).ok())
        .and_then(|t| t.normalize(None))
    else {
        return;
    };
    let snapshot = feed.cursor_position();
    let point = Vec2::new(snapshot.x as f32, snapshot.y as f32);
    let (buttons, cancel, scroll) = {
        let mut state = bridge.0.lock().unwrap_or_else(|p| p.into_inner());
        (
            std::mem::take(&mut state.buttons),
            std::mem::take(&mut state.cancel) || state.suspended,
            std::mem::take(&mut state.scroll),
        )
    };
    let location = |p: Vec2| Location {
        target: target.clone(),
        position: p * scale.0,
    };
    if cancel || !output.active {
        inputs.write(PointerInput::new(
            PointerId::Mouse,
            location(point),
            PointerAction::Cancel,
        ));
        return;
    }
    for (p, button, pressed) in buttons {
        let button = match button {
            0x110 => PointerButton::Primary,
            0x111 => PointerButton::Secondary,
            0x112 => PointerButton::Middle,
            _ => continue,
        };
        inputs.write(PointerInput::new(
            PointerId::Mouse,
            location(p),
            PointerAction::Move { delta: Vec2::ZERO },
        ));
        inputs.write(PointerInput::new(
            PointerId::Mouse,
            location(p),
            if pressed {
                PointerAction::Press(button)
            } else {
                PointerAction::Release(button)
            },
        ));
    }
    let delta = previous.map_or(Vec2::ZERO, |last| (point - last) * scale.0);
    *previous = Some(point);
    inputs.write(PointerInput::new(
        PointerId::Mouse,
        location(point),
        PointerAction::Move { delta },
    ));
    if scroll != Vec2::ZERO {
        inputs.write(PointerInput::new(
            PointerId::Mouse,
            location(point),
            PointerAction::Scroll {
                unit: bevy::input::mouse::MouseScrollUnit::Pixel,
                x: scroll.x,
                y: scroll.y,
                phase: bevy::input::touch::TouchPhase::Moved,
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_attachment_waits_until_startup_after_feed_installation() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<EmbeddedShellBridge>()
            .add_systems(Startup, attach_protocol);
        assert!(
            !app.world()
                .contains_resource::<crate::protocol::ClientSceneFeed>()
        );
        let (_events, feed) = crate::protocol::ClientSceneFeed::test_channel();
        app.insert_resource(feed);
        app.update();
        assert!(
            app.world()
                .contains_resource::<cosmix_shell::runtime::ShellQuitHandler>()
        );
    }
    #[test]
    fn native_press_owns_release_outside_panel_but_never_steals_client_drag() {
        let bridge = EmbeddedShellBridge::default();
        bridge.0.lock().unwrap().regions.push(PanelRect {
            x: 10.,
            y: 10.,
            width: 100.,
            height: 100.,
        });
        assert!(!bridge.button(20., 20., 0x110, true, true));
        assert!(bridge.button(20., 20., 0x110, true, false));
        assert!(bridge.covers(500., 500.));
        assert!(bridge.button(500., 500., 0x110, false, false));
        assert!(!bridge.covers(500., 500.));
        assert!(!bridge.button(500., 500., 0x110, true, false));
    }
    #[test]
    fn reset_clears_native_ownership_and_cancels_queued_actions() {
        let bridge = EmbeddedShellBridge::default();
        bridge.0.lock().unwrap().regions.push(PanelRect {
            x: 0.,
            y: 0.,
            width: 100.,
            height: 100.,
        });
        assert!(bridge.button(20., 20., 0x110, true, false));
        bridge.reset();
        assert!(!bridge.covers(20., 20.));
        let state = bridge.0.lock().unwrap();
        assert!(state.buttons.is_empty() && state.cancel);
    }
}
