//! P3 — the runtime reaction loop that applies `webd.listeners` L1
//! changes to the live [`ListenerSetControl`].
//!
//! The namespace is the system of record; this loop is the executor.
//! An operator flips `enabled` (the kill switch) or a guard field via
//! `props.set` (or the ergonomic verbs); the namespace hook enqueues a
//! [`ListenerEvent`]; this loop reconciles the listener set to match
//! and writes the observed state back into the daemon-owned columns.
//!
//! ## Drive off current state, not the event delta
//!
//! Each event triggers a re-read of the *current* row and a full
//! reconcile (enable/disable to match `enabled`, `swap_guard` from the
//! guard fields). So a dropped or duplicated event is self-healing —
//! the loop converges on whatever L1 currently says. The startup pass
//! reconciles every row once, which also recovers any change missed
//! while the daemon was down.
//!
//! ## Ownership / races
//!
//! This loop is the *only* writer of [`ListenerSetControl`]; verbs
//! write L1 only. It writes only daemon-owned columns
//! (`bound`/`bound_addr`/`active_conns`/`last_transition`/`last_error`)
//! via `WriteOrigin::backend()`, while callers write only caller-owned
//! columns — OCC (`require_version=true`) serialises the rare overlap,
//! handled by the bounded writeback retry. Spawned fire-and-forget for
//! the daemon lifetime; a writeback error never tears down serving.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use cosmix_daemon::listen::{Cidr, GuardPolicy, IpAcl, ListenerSetControl, RateLimit};
use cosmix_props::{
    Actor, MergeMode, PropValue, RecordKey, Runtime, SetOpts, Version, WriteOrigin,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

use crate::listeners_namespace::{
    ListenerEvent, ListenerRow, listener_row_from_value, namespace_name, snapshot_rows,
};

/// Drain deadline when disabling a listener — wait this long for
/// in-flight connections to finish before the accept loop's sockets
/// are released. Past it, stragglers complete in the background.
const DISABLE_DRAIN: Duration = Duration::from_secs(5);

/// Max attempts for a daemon-owned status writeback. The only expected
/// transient is an OCC `version_mismatch` when a caller flips a
/// caller-owned field between our anchor read and our write; a couple
/// of retries clears it. A persistent error self-limits here and the
/// next reconcile re-writes the status (these are eventually-consistent
/// status columns).
const WRITEBACK_ATTEMPTS: usize = 3;

/// Run the reaction loop. Spawned by `main()` as a daemon-lifetime
/// task: `tokio::spawn(listeners_reaction::run(control, runtime, rx))`.
pub async fn run(
    control: ListenerSetControl,
    runtime: Arc<Runtime>,
    mut events_rx: mpsc::Receiver<ListenerEvent>,
) {
    // Startup pass: reconcile every row once (also recovers changes
    // missed while down) and write back the observed state.
    match snapshot_rows(&runtime).await {
        Ok(rows) => {
            for row in &rows {
                reconcile(&control, &runtime, &row.id).await;
            }
        }
        Err(e) => tracing::warn!(
            target: "webd::listeners_reaction",
            error = %format!("{e:#}"),
            "startup snapshot failed — relying on per-event reconcile",
        ),
    }

    while let Some(event) = events_rx.recv().await {
        let id = match &event {
            ListenerEvent::Set { id } => id.clone(),
            ListenerEvent::Removed { id } => {
                // A listener row removed at runtime: the listener keeps
                // running (its bind came from config, not the row).
                // Nothing to apply — log for visibility.
                tracing::info!(
                    target: "webd::listeners_reaction",
                    id = %id,
                    "listeners row removed — listener left as-is (bind is config-driven)",
                );
                continue;
            }
        };
        reconcile(&control, &runtime, &id).await;
    }
    tracing::info!(
        target: "webd::listeners_reaction",
        "listeners reaction channel closed — loop exiting",
    );
}

/// Read the current row for `id` and converge the live listener to it:
/// apply the guard policy, enable/disable to match `enabled`, then
/// write the observed state back.
async fn reconcile(control: &ListenerSetControl, runtime: &Arc<Runtime>, id: &str) {
    // Skip a row with no matching listener in the set (e.g. an orphan
    // row for a listener dropped from config) — nothing to drive.
    if !control.status().iter().any(|s| s.id == id) {
        tracing::debug!(
            target: "webd::listeners_reaction",
            id = %id,
            "listeners row has no matching listener in the set — skipped",
        );
        return;
    }

    let key = RecordKey::collection(namespace_name(), id.to_string());
    let row = match runtime.store().get(&key).await {
        Ok(snap) => match listener_row_from_value(&snap.value.value) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "webd::listeners_reaction",
                    id = %id, error = %e,
                    "row failed ListenerRow deserialisation — skipping reconcile",
                );
                return;
            }
        },
        // Tombstoned/absent — handled by the Removed arm; nothing to do.
        Err(_) => return,
    };

    // 1. Guard policy — cheap, applies whether running or not.
    if let Err(e) = control.swap_guard(id, guard_from_row(&row)) {
        tracing::warn!(target: "webd::listeners_reaction", id = %id, error = %e, "swap_guard failed");
    }

    // 2. Kill switch — enable/disable to match.
    let running = control.is_running(id);
    let mut last_error: Option<String> = None;
    if row.enabled && !running {
        if let Err(e) = control.enable(id).await {
            last_error = Some(format!("enable failed: {e:#}"));
            tracing::warn!(target: "webd::listeners_reaction", id = %id, error = %format!("{e:#}"), "enable failed");
        } else {
            tracing::info!(target: "webd::listeners_reaction", id = %id, "listener enabled");
        }
    } else if !row.enabled && running {
        if let Err(e) = control.disable(id, Some(DISABLE_DRAIN)).await {
            last_error = Some(format!("disable failed: {e:#}"));
            tracing::warn!(target: "webd::listeners_reaction", id = %id, error = %format!("{e:#}"), "disable failed");
        } else {
            tracing::info!(target: "webd::listeners_reaction", id = %id, "listener disabled (socket dropped, port freed)");
        }
    }

    // 3. Write back the observed state.
    writeback_status(runtime, control, id, last_error).await;
}

/// Build a [`GuardPolicy`] from a row's caller-owned guard fields.
/// CIDR strings were validated at `before_set`; a stray parse error
/// here is logged and the entry skipped (defensive). `pub(crate)` so
/// the startup `ListenerSet` build seeds each listener's initial guard
/// from the same row the reaction loop later hot-swaps.
pub(crate) fn guard_from_row(row: &ListenerRow) -> GuardPolicy {
    GuardPolicy {
        per_ip_rate: row
            .rate_limit_per_ip
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
            .map(|max| RateLimit {
                max,
                per: Duration::from_secs(1),
            }),
        max_conns: row.max_conns_global.and_then(|n| u32::try_from(n).ok()),
        max_conns_per_ip: row.max_conns_per_ip.and_then(|n| u32::try_from(n).ok()),
        strict_sni: row.strict_sni,
        acl: build_acl(&row.id, &row.allow_cidrs, &row.deny_cidrs),
    }
}

fn build_acl(id: &str, allow: &[String], deny: &[String]) -> Option<IpAcl> {
    if allow.is_empty() && deny.is_empty() {
        return None;
    }
    let parse = |list: &[String]| -> Vec<Cidr> {
        list.iter()
            .filter_map(|s| match Cidr::parse(s) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(
                        target: "webd::listeners_reaction",
                        id = %id, cidr = %s, error = %e,
                        "skipping unparseable CIDR (should have been rejected at write)",
                    );
                    None
                }
            })
            .collect()
    };
    Some(IpAcl {
        allow: parse(allow),
        deny: parse(deny),
    })
}

/// Patch the daemon-owned status columns to the live observed state.
/// Backend-origin + OCC with a bounded retry (a caller flipping a
/// caller-owned field can bump the version between our read and write).
async fn writeback_status(
    runtime: &Arc<Runtime>,
    control: &ListenerSetControl,
    id: &str,
    last_error: Option<String>,
) {
    let status = control.status().into_iter().find(|s| s.id == id);
    let (bound, bound_addr, active) = match &status {
        Some(s) => (
            s.running,
            s.binds
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(","),
            i64::from(s.active_conns),
        ),
        None => (false, String::new(), 0),
    };
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();

    let mut fields: BTreeMap<String, PropValue> = BTreeMap::new();
    fields.insert("bound".into(), PropValue::Bool(bound));
    fields.insert("bound_addr".into(), PropValue::String(bound_addr));
    fields.insert("active_conns".into(), PropValue::Int(active));
    fields.insert("last_transition".into(), PropValue::String(now));
    // Only stamp last_error on a failure — clear it on success so a
    // recovered listener doesn't keep showing a stale error.
    fields.insert(
        "last_error".into(),
        match last_error {
            Some(e) => PropValue::String(e),
            None => PropValue::Null,
        },
    );

    let key = RecordKey::collection(namespace_name(), id.to_string());
    for attempt in 0..WRITEBACK_ATTEMPTS {
        let anchor = match runtime.store().version_anchor(&key).await {
            Ok(Some(v)) => v,
            Ok(None) => Version::zero(),
            Err(e) => {
                tracing::warn!(target: "webd::listeners_reaction", id = %id, error = %e, "status writeback: anchor read failed");
                return;
            }
        };
        let ts_ms = OffsetDateTime::now_utc().unix_timestamp() * 1000;
        match runtime
            .set_with_origin(
                key.clone(),
                PropValue::Object(fields.clone()),
                SetOpts {
                    expected_version: Some(anchor),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd"),
                    cause: Some("listener_reaction".into()),
                    ts_ms,
                },
                WriteOrigin::backend(),
            )
            .await
        {
            Ok(_) => return,
            Err(e) => {
                if attempt + 1 < WRITEBACK_ATTEMPTS {
                    // Most likely a concurrent caller flip → version
                    // mismatch; re-read the anchor and retry.
                    continue;
                }
                tracing::warn!(
                    target: "webd::listeners_reaction",
                    id = %id, error = %format!("{e:#}"),
                    "status writeback failed after retries — next reconcile will re-write",
                );
                return;
            }
        }
    }
}
