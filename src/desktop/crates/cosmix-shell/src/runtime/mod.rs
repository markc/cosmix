//! Renderer-neutral messages crossing between sources, the core model, chrome,
//! and a host. The optional Bevy adapter preserves this message/frame seam.

mod messages;

#[cfg(feature = "chrome")]
mod bevy_runtime;

pub use messages::{
    CarouselInput, HostGeometry, KeyboardInteractivity, PanelPresentation, ShellCommand,
    ShellCommandKind, ShellContentPresentation, ShellFrame, WakePolicy,
};

#[cfg(feature = "chrome")]
pub use bevy_runtime::{ShellFrameState, ShellRuntimePlugin, ShellRuntimeSet, replace_shell_model};
