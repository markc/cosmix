pub mod config;
pub mod events;
pub mod registry;
pub mod state;

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::broadcast;

use self::config::DaemonConfig;
use self::events::{DaemonEvent, EventBus};
use self::state::SharedState;

pub struct Daemon {
    pub config: DaemonConfig,
    pub state: SharedState,
    pub events: EventBus,
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Self {
        Self {
            config,
            state: state::DaemonState::new(),
            events: EventBus::new(256),
        }
    }

    pub fn run(self) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .context("Failed to create tokio runtime")?;

        rt.block_on(async move {
            self.run_async().await
        })
    }

    async fn run_async(self) -> Result<()> {
        tracing::info!("cosmix daemon starting");

        // Ensure socket directory exists
        let sock_dir = std::path::Path::new(&self.config.daemon.socket)
            .parent()
            .context("Invalid socket path")?;
        std::fs::create_dir_all(sock_dir)
            .context("Failed to create socket directory")?;

        // Clean up stale socket
        let sock_path = &self.config.daemon.socket;
        if std::path::Path::new(sock_path).exists() {
            // Try connecting to see if another daemon is running
            match tokio::net::UnixStream::connect(sock_path).await {
                Ok(_) => {
                    anyhow::bail!("Another cosmix daemon is already running on {sock_path}");
                }
                Err(_) => {
                    // Stale socket, remove it
                    std::fs::remove_file(sock_path).ok();
                }
            }
        }

        // Ensure port directory exists
        std::fs::create_dir_all(&self.config.daemon.port_dir)
            .context("Failed to create port directory")?;

        let state = self.state.clone();
        let events = self.events.clone();
        let config = Arc::new(self.config);

        // Make shared state available to Lua port resolution
        *crate::lua::DAEMON_STATE.lock().unwrap() = Some(state.clone());

        // Start Wayland poller thread (populates shared state every 500ms)
        let poller_state = state.clone();
        let _poller = state::spawn_wayland_poller(poller_state, 500);

        // Do initial sync before accepting IPC (blocking, ensures state is populated)
        if let Ok((_conn, _eq, wl_state)) = crate::wayland::connect() {
            let mut s = state.write().unwrap();
            s.sync_from_wayland(&wl_state);
            tracing::info!("Initial state: {} windows, {} workspaces",
                s.windows.len(), s.workspaces.len());
        }

        // Start port watcher (scans for app sockets every 2s)
        let watcher_state = state.clone();
        let watcher_port_dir = config.daemon.port_dir.clone();
        let _port_watcher = registry::spawn_port_watcher(watcher_state, watcher_port_dir, 2000);

        // Start IPC server
        let ipc_config = config.clone();
        let ipc_state = state.clone();
        let ipc_events = events.clone();
        let ipc_handle = tokio::spawn(async move {
            if let Err(e) = crate::ipc::serve(&ipc_config.daemon.socket, ipc_state, ipc_events).await {
                tracing::error!("IPC server error: {e}");
            }
        });

        // Wait for shutdown signal
        let shutdown_events = events.clone();
        let shutdown_state = state.clone();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received SIGINT, shutting down");
            }
            _ = async {
                let mut rx = shutdown_events.subscribe();
                loop {
                    match rx.recv().await {
                        Ok(DaemonEvent::Shutdown) => break,
                        Err(broadcast::error::RecvError::Closed) => break,
                        _ => continue,
                    }
                }
            } => {
                tracing::info!("Received shutdown event");
            }
        }

        // Mark as not running
        {
            let mut s = shutdown_state.write().unwrap();
            s.running = false;
        }

        // Clean up socket
        std::fs::remove_file(&config.daemon.socket).ok();

        ipc_handle.abort();

        tracing::info!("cosmix daemon stopped");
        Ok(())
    }
}
