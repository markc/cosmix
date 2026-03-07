use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::wayland::WorkspaceInfo;

/// Re-export ToplevelInfo as WindowInfo for a cleaner public API.
pub use crate::wayland::ToplevelInfo as WindowInfo;

/// Thread-safe shared daemon state.
pub type SharedState = Arc<RwLock<DaemonState>>;

/// Central state shared across all daemon subsystems.
#[derive(Debug)]
#[allow(dead_code)]
pub struct DaemonState {
    /// Currently tracked windows, keyed by toplevel protocol ID.
    pub windows: HashMap<u32, WindowInfo>,

    /// Currently tracked workspaces, keyed by workspace protocol ID.
    pub workspaces: HashMap<u32, WorkspaceInfo>,

    /// Current clipboard text content, if any.
    pub clipboard_text: Option<String>,

    /// Set to `false` to signal all daemon tasks to shut down.
    pub running: bool,
}

impl DaemonState {
    /// Create a new shared state wrapped in `Arc<RwLock>`.
    pub fn new() -> SharedState {
        Arc::new(RwLock::new(Self {
            windows: HashMap::new(),
            workspaces: HashMap::new(),
            clipboard_text: None,
            running: true,
        }))
    }
}
