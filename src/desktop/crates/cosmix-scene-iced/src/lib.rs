//! Mix Scenes adapter that draws a scene with a CPU renderer (iced through
//! tiny-skia, once `cosmix-iced-host` lands; a stand-in until then) and shows
//! it in Bevy UI through one persistent GPU texture per surface.
//!
//! Upload path: the texture is an uninitialised `Image` added to
//! `Assets<Image>` once per size and never touched again, so the only asset
//! events it causes are one `Added` per allocation. Damage rectangles are
//! copied out of the renderer's buffer and written with
//! `RenderQueue::write_texture` from the render world.
mod bridge;
mod gpu;
pub mod standin;
pub mod surface;
pub mod upload;

use bevy::input::keyboard::KeyboardInput;
use bevy::input_focus::InputFocus;
use bevy::picking::pointer::PointerInput;
use bevy::prelude::*;
use bevy::render::RenderApp;
use bevy::window::Ime;
use cosmix_scene_bevy::{SceneReconcile, SceneStore};
use cosmix_shell::runtime::ShellRuntimeSet;

pub use bridge::{
    FrameCounters, IcedSurface, IcedSurfaceGeometry, ImeOutput, RendererFactory, SceneIcedCounters,
    SceneIcedFactory, SceneIcedFocus, SceneIcedStats, SceneIcedWake,
};

/// The `adapter` argument of `shell.scene.load` that selects this adapter.
pub const ADAPTER: &str = "iced";

pub struct SceneIcedPlugin;

impl Plugin for SceneIcedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SceneStore>();
        app.world_mut()
            .resource_mut::<SceneStore>()
            .register_adapter(ADAPTER);
        if !app.world().contains_non_send::<SceneIcedFactory>() {
            app.insert_non_send(SceneIcedFactory::default());
        }
        app.insert_non_send(bridge::Renderers::default());
        app.add_message::<PointerInput>()
            .add_message::<KeyboardInput>()
            .add_message::<Ime>()
            .add_message::<AssetEvent<Image>>()
            .init_resource::<InputFocus>()
            .init_resource::<bridge::Mounts>()
            .init_resource::<SceneIcedCounters>()
            .init_resource::<SceneIcedFocus>()
            .init_resource::<SceneIcedWake>()
            .add_systems(First, bridge::roll_counters)
            .add_systems(
                Update,
                (
                    (bridge::route_pointer, bridge::route_keyboard)
                        .chain()
                        .after(ShellRuntimeSet::Input),
                    bridge::reconcile
                        .before(SceneReconcile)
                        .after(ShellRuntimeSet::Input)
                        .before(ShellRuntimeSet::Model),
                ),
            )
            .add_systems(
                PostUpdate,
                (bridge::geometry, bridge::frame)
                    .chain()
                    .after(bevy::ui::UiSystems::Layout),
            )
            .add_systems(Last, bridge::count_asset_events);
    }

    fn finish(&self, app: &mut App) {
        if app.get_sub_app(RenderApp).is_some() {
            let channel = gpu::GpuChannel::default();
            app.insert_resource(channel.clone());
            gpu::install(app, channel);
        }
    }
}

#[cfg(test)]
mod tests;
