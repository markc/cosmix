//! Layer-shell integration for compact dialog rendering.
//!
//! Uses the system `libgtk-layer-shell` via thin FFI bindings to create
//! overlay surfaces that bypass cosmic-comp's 240px toplevel minimum.

pub mod ffi;
pub mod theme;
pub mod widgets;
