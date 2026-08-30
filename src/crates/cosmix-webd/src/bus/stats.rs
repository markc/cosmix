//! `webd.stats` — per-vhost response-class counters snapshot.
//!
//! Iterates `NodeState::vhosts.load().primaries` (a
//! [`crate::vhost_directory::VhostDirectory`] view, one entry per
//! `Arc<VhostState>` sorted by primary FQDN) and emits a `vhosts`
//! array of `{host, stats}` records.
//!
//! Output ordering is deterministic by primary FQDN — the directory's
//! `primaries` invariant — so cross-node comparison and snapshot
//! diffing don't fight map-iteration randomness.
//!
//! Pre-C3b this consumer walked the raw `HashMap<String,
//! Arc<VhostState>>` and dedup-grouped via a `BTreeMap` keyed by
//! `VhostState::fqdn`; C3b lifts that dedup into the directory's
//! `build` step.

use std::sync::Arc;

use crate::NodeState;

pub fn stats_body(node: &Arc<NodeState>) -> String {
    // `load()` is wait-free and the guard drops at end of expression —
    // a runtime publish during this body build cannot tear the view.
    let directory = node.vhosts.load();
    let rows: Vec<serde_json::Value> = directory
        .primaries
        .iter()
        .map(|p| {
            serde_json::json!({
                "host": p.state.fqdn,
                "stats": p.state.stats.snapshot_json(),
            })
        })
        .collect();

    serde_json::json!({
        "vhosts": rows,
        "count": rows.len(),
    })
    .to_string()
}
