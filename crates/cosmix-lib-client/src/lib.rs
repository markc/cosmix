//! AMP WebSocket client for connecting cosmix apps to cosmix-hub.
//!
//! Provides a simple async API for service-to-service communication
//! through the hub's WebSocket relay.
//!
//! Two backends:
//! - **native** (default): tokio-tungstenite for desktop/server apps
//! - **web** (WASM): gloo-net for browser apps
//!
//! # Example (native)
//!
//! ```no_run
//! # async fn example() -> anyhow::Result<()> {
//! use cosmix_client::HubClient;
//!
//! let client = HubClient::connect_default("my-service").await?;
//! let result = client.call("files", "file.list", serde_json::json!({"path": "/tmp"})).await?;
//! println!("Files: {result}");
//! # Ok(())
//! # }
//! ```

mod types;
pub use types::IncomingCommand;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{HubClient, DEFAULT_HUB_URL};

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::HubClient;
