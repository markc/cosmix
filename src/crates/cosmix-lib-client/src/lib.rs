//! Bus WebSocket client for connecting cosmix apps to cosmix-noded.
//!
//! Provides a simple async API for service-to-service communication
//! through the broker's WebSocket relay.
//!
//! Two backends:
//! - **native** (default): tokio-tungstenite for desktop/server apps
//! - **web** (WASM): gloo-net for browser apps
//!
//! # Example (native)
//!
//! ```no_run
//! # async fn example() -> anyhow::Result<()> {
//! use cosmix_client::NodedClient;
//!
//! // URL is caller-supplied — `lib-client` does not depend on
//! // `cosmix-lib-config` (bus/cos boundary). The
//! // `cosmix_config::client_helpers` module wraps these primitives
//! // with auto-resolve helpers when callers want the
//! // node.conf.mix-derived URL.
//! let client = NodedClient::connect("my-service", "ws://127.0.0.1:4200/ws").await?;
//! let result = client.call("files", "file.list", serde_json::json!({"path": "/tmp"})).await?;
//! println!("Files: {result}");
//! # Ok(())
//! # }
//! ```

mod types;
pub use types::IncomingCommand;

#[cfg(not(target_arch = "wasm32"))]
mod bounded;
#[cfg(not(target_arch = "wasm32"))]
pub use bounded::{BoundedIncomingEvent, BoundedIncomingReceiver};

// Re-export the typed port-call outcome so consumers of `call_typed`
// (NodedClient / SupervisedClient) get the type from this crate rather than
// reaching into `cosmix-lib-bus` directly.
#[cfg(not(target_arch = "wasm32"))]
pub use cosmix_bus::PortReply;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{NameCollision, NodedClient, RegistrationRejected};

#[cfg(not(target_arch = "wasm32"))]
mod supervised;
#[cfg(not(target_arch = "wasm32"))]
pub use supervised::{
    ConnState, MAX_INITIAL_ATTEMPTS, SubscriptionRegistry, SupervisedClient, SupervisedError,
};

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::NodedClient;
