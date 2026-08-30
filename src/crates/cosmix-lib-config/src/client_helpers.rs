//! Auto-resolving wrappers around `cosmix-lib-client`'s explicit-URL
//! primitives.
//!
//! These three functions — `resolve_noded_url`,
//! `connect_default`, `connect_anonymous_default` — used to live in
//! `cosmix-lib-client::native`. They moved here on 2026-05-28 so
//! `lib-client` (bus-bound) has an explicit URL and does not depend on
//! `lib-config` (cos-bound), preserving the `bus ← cos` dependency
//! direction. lib-client retains the
//! explicit-URL primitives `NodedClient::{connect, connect_anonymous}`;
//! this module wraps them with the node.conf.mix-derived URL.
//!
//! Gated under the `client-helpers` Cargo feature so consumers that
//! don't need broker auto-resolution (e.g. `cosmix-lib-daemon` under
//! its `tls` feature) don't transitively pull `cosmix-lib-client`
//! into their dep graphs.

use anyhow::Result;
use cosmix_client::NodedClient;

use crate::node::load_node_config;

/// Resolve the broker WebSocket URL from `node.conf.mix`, falling back to
/// **loopback** (`127.0.0.1`) — never another node's WG IP.
///
/// The pre-2026-05-15 implementation fell back to the hardcoded
/// `192.0.2.5:4200` (alpha's WG IP). That made the URL resolver
/// silently cross the mesh whenever node.conf.mix was missing — the
/// symptom that surfaced this bug was a `gamma account add` landing on
/// alpha's maild because the gamma-side client found no node.conf.mix and
/// dialled alpha by default. Falling back to loopback at least keeps
/// the dial on this host; if no local noded is running the connect
/// fails loudly, which is the right failure mode for an unconfigured
/// node.
pub fn resolve_noded_url() -> String {
    match load_node_config() {
        Ok(Some(cfg)) => cfg.noded_url(),
        Ok(None) => {
            tracing::warn!(
                "node.conf.mix not found; falling back to ws://127.0.0.1:4200/ws (loopback)"
            );
            "ws://127.0.0.1:4200/ws".into()
        }
        Err(e) => {
            tracing::warn!(
                "failed to load node.conf.mix: {e}; falling back to ws://127.0.0.1:4200/ws (loopback)"
            );
            "ws://127.0.0.1:4200/ws".into()
        }
    }
}

/// Connect to the broker at the URL resolved from node.conf.mix.
pub async fn connect_default(service_name: &str) -> Result<NodedClient> {
    NodedClient::connect(service_name, &resolve_noded_url()).await
}

/// Connect + register as a named service, sending build provenance
/// (version-discovery contract). Provenance is re-sent on each register,
/// so a supervised reconnect re-publishes it — build it ONCE at process
/// start (so `started_at` is the true start) and clone it in.
pub async fn connect_default_with_provenance(
    service_name: &str,
    provenance: cosmix_bus::RegisterProvenance,
) -> Result<NodedClient> {
    NodedClient::connect_with_provenance(service_name, &resolve_noded_url(), Some(provenance)).await
}

/// Connect anonymously to the broker URL resolved from node.conf.mix.
pub async fn connect_anonymous_default() -> Result<NodedClient> {
    NodedClient::connect_anonymous(&resolve_noded_url()).await
}
