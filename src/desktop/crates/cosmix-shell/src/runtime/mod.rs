//! Renderer-neutral messages crossing between sources, the core model, chrome,
//! and a host. The optional Bevy adapter preserves this message/frame seam.

mod messages;
mod semantic;

#[cfg(feature = "chrome-core")]
mod bevy_runtime;

pub use messages::{
    CarouselInput, HostGeometry, KeyboardInteractivity, PageChange, PanelPresentation,
    ShellCommand, ShellCommandKind, ShellContentPresentation, ShellEffect, ShellFrame,
    ShellResizeError, ShellResizeResult, WakePolicy,
};
pub use semantic::{SceneVerb, ShellSemanticVerb, semantic_shell_command};

#[cfg(feature = "chrome-core")]
pub use bevy_runtime::{
    ShellEffects, ShellFrameState, ShellQuitHandler, ShellRuntimePlugin, ShellRuntimeSet,
    SubPanelRegistryState, forget_shell_subpanel, redeclare_shell_pages, register_shell_page,
    remove_all_owned_subpanels, remove_owned_subpanels_before, remove_shell_page,
    replace_shell_model, seed_page_thickness, set_page_thickness, set_shell_pages,
};
