//! cosmix-mesh — AMP mesh networking over WireGuard
//!
//! Provides mesh peer management, WebSocket bridge connections to remote hubs,
//! and WireGuard interface status queries.

pub mod node;
pub mod peer;
pub mod wg;

pub use node::MeshNode;
pub use peer::{MeshConfig, MeshPeers, PeerConfig};
