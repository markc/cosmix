//! SPEC 07 §3 (L2) + §4 (L3) for cosmix-musicd via periodic snapshot diff.
//!
//! A single 1 Hz task takes a fresh snapshot, diffs it against the previous one
//! with transient leaves (`lifecycle.uptime_s`, `playback.position_s`) filtered
//! out, and on any non-transient change:
//! 1. republishes `world.musicd` (retained full snapshot), and
//! 2. emits one `musicd.props.changed` event per changed leaf.
//!
//! musicd's non-transient state changes are infrequent (soundfont load, a
//! play/stop, render/play counters), so unlike indexd there is no high-churn
//! `corpus.*` debounce to manage — the position counter is transient and never
//! triggers a republish.

use std::collections::BTreeMap;
use std::sync::Arc;

use cosmix_props::{PropPath, PropTree, PropValue, diff, publish};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::daemon::AppState;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
pub const PROPS_CHANGED_TOPIC: &str = "musicd.props.changed";
pub const WORLD_MUSICD_TOPIC: &str = "world.musicd";

/// Run the L2/L3 publish loop. Spawn after the Bus `NodedClient` connects;
/// exits silently when the broker connection closes (the task is aborted on
/// disconnect by the reconnect loop).
pub async fn run_loop(client: Arc<cosmix_client::NodedClient>, state: Arc<Mutex<AppState>>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last: Option<PropValue> = None;

    loop {
        ticker.tick().await;

        let snapshot = crate::daemon::collect_props(&state).await;
        let cur = snapshot.redacted_snapshot();

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
            continue;
        }

        let cause = if first_run { "boot" } else { "snapshot" };

        if let Err(e) = publish_world(&client, &cur).await {
            warn!("world.musicd publish failed: {e}");
        } else {
            debug!(
                changed = non_transient_changes.len(),
                first_run, "world.musicd republished"
            );
        }

        for (path, old, new) in &non_transient_changes {
            if let Err(e) = publish_props_changed(&client, path, old, new, cause).await {
                warn!(path = %path, "musicd.props.changed publish failed: {e}");
            }
        }

        last = Some(cur);
    }
}

/// Publish the full snapshot to `world.musicd` (retained).
async fn publish_world(client: &cosmix_client::NodedClient, val: &PropValue) -> anyhow::Result<()> {
    let inner_wire = publish::build_world_message("musicd", val).to_wire();

    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), WORLD_MUSICD_TOPIC.to_string());
    headers.insert("retain".to_string(), "true".to_string());

    client
        .send_with_headers("noded", "topic.publish", &headers, &inner_wire)
        .await
}

/// Emit one `musicd.props.changed` event with `{path, old, new, ts, cause}`.
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
