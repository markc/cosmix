//! SPEC 07 §3 (L2) + §4 (L3) for cosmix-indexd via periodic snapshot diff.
//!
//! Unlike noded, indexd is a peer (not the broker), so it can't call
//! `broker.publish()` directly — it publishes via a `topic.publish` RPC
//! through `NodedClient::send_with_headers`, with the body being a
//! wire-encoded inner Bus message (the broker re-parses it before
//! routing).
//!
//! Strategy: a single 1 Hz task takes a fresh snapshot, diffs against
//! the previous one with transient leaves filtered out, and on any
//! non-transient change:
//! 1. republishes `world.indexd` (retained, full current snapshot
//!    including transient uptime — subscribers compute current uptime
//!    from `started_at`),
//! 2. emits one `indexd.props.changed` event per non-transient leaf.
//!
//! This is intentionally simpler than noded's mutation-hook ChangeBus.
//! indexd's mutations are infrequent (chunk store/delete batches) and
//! the daemon has no model-load/unload notification infrastructure, so
//! polling-with-diff is both cheap (one BTreeMap walk per second) and
//! more general (catches any state change, including watchdog-driven
//! unloads, without per-site plumbing).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmix_props::{PropPath, PropTree, PropValue, diff, publish};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::AppState;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Minimum spacing between `world.indexd` republishes when the ONLY changed
/// leaves are high-churn `corpus.*` counters. Under bulk indexing those
/// counters tick every second; without this debounce the daemon storms noded
/// with a retained republish every poll for minutes (observed as noded
/// "Removing dead TopicState" churn). Any non-`corpus.*` change (lifecycle,
/// config, circuit state) still republishes immediately.
const CORPUS_DEBOUNCE: Duration = Duration::from_secs(10);
pub const PROPS_CHANGED_TOPIC: &str = "indexd.props.changed";
pub const WORLD_INDEXD_TOPIC: &str = "world.indexd";

/// Run the L2/L3 publish loop. Spawn this as a tokio task after the
/// Bus NodedClient is connected. Exits silently if the broker closes.
pub async fn run_loop(client: Arc<cosmix_client::NodedClient>, state: Arc<Mutex<AppState>>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last: Option<PropValue> = None;
    let mut last_world_publish: Option<Instant> = None;

    loop {
        ticker.tick().await;

        let snapshot = crate::collect_props(&state, "internal.world_snapshot").await;
        let cur = snapshot.redacted_snapshot();

        // Filter the diff to non-transient leaves. Transient paths
        // (lifecycle.uptime_s) tick every second by design and would
        // otherwise force a republish every poll forever.
        let non_transient_changes: Vec<(PropPath, PropValue, PropValue)> = match &last {
            None => Vec::new(),
            Some(prev) => diff(prev, &cur)
                .into_iter()
                .filter(|(path, _, _)| {
                    !snapshot
                        .describe(path)
                        .map(|d| d.transient)
                        .unwrap_or(false)
                })
                .collect(),
        };

        let first_run = last.is_none();
        if !first_run && non_transient_changes.is_empty() {
            // No meaningful change — keep last as-is (so uptime drift
            // doesn't keep accumulating in the diff baseline).
            continue;
        }

        // R8 debounce: when the ONLY changed leaves are high-churn
        // `corpus.*` counters, republish at most once per CORPUS_DEBOUNCE.
        // Any non-`corpus.*` change (lifecycle/config/circuit) bypasses the
        // debounce and republishes immediately. We deliberately do NOT update
        // `last` when we skip, so the still-pending corpus change re-triggers
        // and fires at the next eligible tick — the republish is delayed, not
        // dropped.
        let only_corpus = !first_run
            && !non_transient_changes.is_empty()
            && non_transient_changes
                .iter()
                .all(|(path, _, _)| path.as_str().starts_with("corpus."));
        if only_corpus
            && let Some(t) = last_world_publish
            && t.elapsed() < CORPUS_DEBOUNCE
        {
            // Skip this tick's republish AND its props.changed events; leave
            // `last` unchanged so the pending change fires once the debounce
            // window elapses.
            continue;
        }

        let cause = if first_run {
            "boot".to_string()
        } else {
            "snapshot".to_string()
        };

        if let Err(e) = publish_world(&client, &cur).await {
            warn!("world.indexd publish failed: {e}");
        } else {
            last_world_publish = Some(Instant::now());
            debug!(
                changed = non_transient_changes.len(),
                first_run, "world.indexd republished"
            );
        }

        for (path, old, new) in &non_transient_changes {
            if let Err(e) = publish_props_changed(&client, path, old, new, &cause).await {
                warn!(path = %path, "indexd.props.changed publish failed: {e}");
            }
        }

        last = Some(cur);
    }
}

/// Publish the full redacted snapshot to `world.indexd` (retained).
/// Body of the topic.publish RPC is a wire-encoded inner Bus message
/// with `command=indexd` so subscribers can `on indexd do ... end`.
async fn publish_world(client: &cosmix_client::NodedClient, val: &PropValue) -> anyhow::Result<()> {
    let inner_wire = publish::build_world_message("indexd", val).to_wire();

    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), WORLD_INDEXD_TOPIC.to_string());
    headers.insert("retain".to_string(), "true".to_string());

    client
        .send_with_headers("noded", "topic.publish", &headers, &inner_wire)
        .await
}

/// Emit one `indexd.props.changed` event with `{path, old, new, ts, cause}`.
/// Not retained — change events are ephemeral.
async fn publish_props_changed(
    client: &cosmix_client::NodedClient,
    path: &PropPath,
    old: &PropValue,
    new: &PropValue,
    cause: &str,
) -> anyhow::Result<()> {
    let inner_wire = publish::build_props_changed_message(path, old, new, cause).to_wire();

    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), PROPS_CHANGED_TOPIC.to_string());
    headers.insert("retain".to_string(), "false".to_string());

    client
        .send_with_headers("noded", "topic.publish", &headers, &inner_wire)
        .await
}
