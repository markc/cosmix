//! `webd.routes.list` — read-only snapshot of the vhost map.
//!
//! Iterates `NodeState::vhosts.load().primaries` (a
//! [`crate::vhost_directory::VhostDirectory`] view, pre-grouped by
//! primary FQDN with the alias list already sorted). Per primary
//! emits `{host, aliases, has_cms, has_jmap, has_ws, has_docs}` —
//! booleans rather than paths, so the response stays agent-legible
//! without leaking on-disk layout.
//!
//! Output is sorted by primary FQDN (the directory's `primaries`
//! invariant) for deterministic cross-node comparison — same
//! convention `dnsd.zone.snapshot` follows.
//!
//! Pre-C3b this consumer walked the raw `HashMap<String,
//! Arc<VhostState>>` and dedup-grouped by `Arc::as_ptr` identity;
//! C3b lifts both the dedup and the alias grouping into the
//! directory's `build` step (see
//! `vhost_directory::VhostDirectory::primaries`), so this body
//! builder is now a straight map-and-sort.

use std::sync::Arc;

use crate::NodeState;

pub fn list_body(node: &Arc<NodeState>) -> String {
    // `load()` is wait-free and the guard drops at end of expression —
    // a runtime publish during this body build cannot tear the view.
    let directory = node.vhosts.load();
    let rows: Vec<serde_json::Value> = directory
        .primaries
        .iter()
        .map(|p| {
            serde_json::json!({
                "host": p.state.fqdn,
                "aliases": p.aliases,
                "has_cms": p.state.db.is_some(),
                "has_jmap": p.state.jmap_upstream.is_some(),
                "has_ws": p.state.noded_ws.is_some(),
                "has_docs": p.state.docs_dir.is_some(),
            })
        })
        .collect();

    serde_json::json!({
        "vhosts": rows,
        "count": rows.len(),
    })
    .to_string()
}
