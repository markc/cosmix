//! Renderer-free core of CosMix Term.
//!
//! Everything a terminal frontend needs except the window: PTY and VT grid
//! (`terminal`), tabs and pane trees (`tabs`, `panes`), the CPU glyph raster
//! (`raster`), startup settings (`config`), the diagnostic Bus service (`bus`)
//! and the verified native-session control lane (`native_session`, `control`).
//!
//! A frontend installs a [`wake::WakeFd`] waker with `TabSet::set_wake`, polls
//! its descriptor, and on readiness calls `WakeFd::drain` and then
//! `Terminal::grid_snapshot` for each visible pane, repainting the rows the
//! snapshot marks dirty.
pub mod bus;
mod clusters;
pub mod config;
pub mod control;
pub mod font;
pub mod metrics;
pub mod native_session;
pub mod panes;
pub mod raster;
pub mod session_fd;
pub mod tabs;
pub mod terminal;
pub mod version;
pub mod wake;

#[cfg(test)]
#[path = "../../../vendor/teletypewriter/patch_guard.rs"]
mod teletypewriter_patch_guard;
