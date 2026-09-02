//! Renderer-neutral messages crossing between sources, the core model, chrome,
//! and a host. The optional Bevy adapter preserves this message/frame seam.

mod messages;
mod semantic;

#[cfg(feature = "chrome-core")]
mod bevy_runtime;

pub use messages::{
    CarouselInput, HostGeometry, KeyboardInteractivity, PanelPresentation, ShellCommand,
    ShellCommandKind, ShellContentPresentation, ShellEffect, ShellFrame, WakePolicy,
};
pub use semantic::{ShellSemanticVerb, semantic_shell_command};

#[cfg(feature = "chrome-core")]
pub use bevy_runtime::{
    ShellEffects, ShellFrameState, ShellRuntimePlugin, ShellRuntimeSet, replace_shell_model,
};
