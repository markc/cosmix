use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::wayland::WorkspaceInfo;

/// Re-export ToplevelInfo as WindowInfo for a cleaner public API.
pub use crate::wayland::ToplevelInfo as WindowInfo;

use super::cliplist::ClipList;
use super::queues::QueueStore;
use super::registry::PortRegistry;

/// Thread-safe shared daemon state.
pub type SharedState = Arc<RwLock<DaemonState>>;

/// Central state shared across all daemon subsystems.
#[derive(Debug)]
pub struct DaemonState {
    /// Currently tracked windows, keyed by toplevel protocol ID.
    pub windows: HashMap<u32, WindowInfo>,

    /// Currently tracked workspaces, keyed by workspace protocol ID.
    pub workspaces: HashMap<u32, WorkspaceInfo>,

    /// Current clipboard text content, if any.
    pub clipboard_text: Option<String>,

    /// Registry of discovered app ports (Layer 3).
    pub port_registry: PortRegistry,

    /// ARexx-style Clip List — persistent global key/value store.
    pub clip_list: ClipList,

    /// Session-scoped named queues for inter-script communication.
    pub queue_store: QueueStore,

    /// Mesh handle (None if mesh is disabled).
    pub mesh: Option<crate::mesh::MeshHandle>,

    /// Set to `false` to signal all daemon tasks to shut down.
    pub running: bool,
}

impl DaemonState {
    /// Create a new shared state wrapped in `Arc<RwLock>`.
    pub fn new() -> SharedState {
        // Load persistent clip list
        let clip_list = super::cliplist::load_from_file(&super::cliplist::cliplist_path());

        Arc::new(RwLock::new(Self {
            windows: HashMap::new(),
            workspaces: HashMap::new(),
            clipboard_text: None,
            port_registry: PortRegistry::new(),
            clip_list,
            queue_store: QueueStore::new(),
            mesh: None,
            running: true,
        }))
    }

    /// Sync state from a Wayland snapshot.
    pub fn sync_from_wayland(&mut self, wl: &crate::wayland::State) {
        self.windows.clear();
        for (&id, info) in &wl.toplevels {
            if !info.app_id.is_empty() || !info.title.is_empty() {
                self.windows.insert(id, info.clone());
            }
        }
        self.workspaces.clear();
        for (&id, ws) in &wl.workspaces {
            self.workspaces.insert(id, ws.clone());
        }
    }
}

/// Spawn a dedicated OS thread that polls Wayland and keeps SharedState fresh.
/// Returns a JoinHandle. The thread exits when `state.running` becomes false.
pub fn spawn_wayland_poller(state: SharedState, poll_ms: u64) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            // Check if we should stop
            {
                let s = state.read().unwrap();
                if !s.running {
                    break;
                }
            }

            // Snapshot Wayland state
            match crate::wayland::connect() {
                Ok((_conn, _eq, wl_state)) => {
                    let mut s = state.write().unwrap();
                    s.sync_from_wayland(&wl_state);
                }
                Err(e) => {
                    tracing::debug!("Wayland poll failed: {e}");
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
        }
        tracing::debug!("Wayland poller stopped");
    })
}
