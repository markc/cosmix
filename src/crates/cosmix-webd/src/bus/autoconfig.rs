//! `webd.autoconfig.served_domains` — the autoconfig admission allowlist.
//!
//! Returns the `NodeState::served_mail_domains` set, sorted for
//! deterministic output (the underlying type is `HashSet` so
//! iteration order is randomised). A domain not in this set is
//! `404`ed by the autoconfig handler before any DNS lookup — see
//! `maild-autoconfig.md §Security` — so this verb tells operators
//! and agents exactly which Hosts the autoconfig path will accept.

use std::sync::Arc;

use crate::NodeState;

pub fn served_domains_body(node: &Arc<NodeState>) -> String {
    let mut domains: Vec<String> = node.served_mail_domains.iter().cloned().collect();
    domains.sort();
    serde_json::json!({
        "domains": domains,
        "count": domains.len(),
    })
    .to_string()
}
