//! Observe Bevy's actual swapchain submission, not merely an App update.
//!
//! Bevy clears needs_initial_present even when acquisition failed. The reliable
//! signal is a texture present immediately before render_system and consumed
//! by it. With pipelined rendering disabled this resource is synchronously
//! available to the native runner after app.update().

use bevy::{
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems, renderer::render_system, view::window::ExtractedWindows,
    },
};
use std::collections::BTreeSet;

#[derive(Resource, Default)]
pub(crate) struct SubmittedWindows(pub BTreeSet<Entity>);

pub(crate) fn configure(app: &mut App) {
    app.sub_app_mut(RenderApp)
        .init_resource::<SubmittedWindows>()
        .add_systems(
            Render,
            before_render
                .in_set(RenderSystems::Render)
                .before(render_system),
        )
        .add_systems(
            Render,
            after_render
                .in_set(RenderSystems::Render)
                .after(render_system),
        );
}

fn before_render(windows: Res<ExtractedWindows>, mut submitted: ResMut<SubmittedWindows>) {
    submitted.0.clear();
    submitted.0.extend(
        windows
            .values()
            .filter(|w| w.swap_chain_texture.is_some())
            .map(|w| w.entity),
    );
}

fn after_render(windows: Res<ExtractedWindows>, mut submitted: ResMut<SubmittedWindows>) {
    submitted.0.retain(|entity| {
        windows
            .get(entity)
            .is_some_and(|w| w.swap_chain_texture.is_none())
    });
}
