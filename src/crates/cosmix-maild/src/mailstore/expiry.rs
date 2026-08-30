//! Upload-staging expiry worker.
//!
//! v1.1 amendment §1 ("TTL policy"): JMAP `blob/upload` and
//! `Email/import` staging refs carry `expires_at = now + 1 hour` at
//! creation. This worker sweeps `blob_refs WHERE expires_at < now()`
//! periodically, tears down the temp MDS items they pinned, and
//! deletes the rows. Once the temp item is gone, core MDS GC reclaims
//! the underlying CAS blob in its own sweep.
//!
//! Persistent refs (those minted from already-stored email — `expires_at
//! IS NULL`) are not touched here.
//!
//! Each per-set tear-down is one `with_set_tx`, so the membership
//! removal, item deletion, blob_ref refcount drop, and `blob_refs`
//! row delete all commit atomically. A failure on one set logs and
//! continues to the next.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use cosmix_mds::{ContainerId, ItemId, Mds, SqliteCasMds};
use rusqlite::params;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, info, warn};

/// Default sweep cadence — chosen to be much smaller than the 1-hour
/// TTL so a stuck sweep doesn't accumulate a meaningful backlog.
pub const DEFAULT_TICK: Duration = Duration::from_secs(60);

/// In-process tokio task that runs `sweep_once` on a fixed interval.
///
/// 8d.3 leaves graceful shutdown unwired: the task lives until process
/// exit. Maild's restart/replace cycle is the supervision story.
pub struct ExpiryWorker {
    mds: Arc<SqliteCasMds>,
    tick: Duration,
}

impl ExpiryWorker {
    pub fn new(mds: Arc<SqliteCasMds>) -> Self {
        Self {
            mds,
            tick: DEFAULT_TICK,
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Spawn the worker as a background tokio task.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run_loop())
    }

    async fn run_loop(self) {
        let mut tick = interval(self.tick);
        // Skip the immediate first tick — `interval` fires once at t=0
        // by default and we don't want bootstrap to be pinned to GC.
        tick.tick().await;
        loop {
            tick.tick().await;
            match self.sweep().await {
                Ok(0) => debug!(target: "maild::expiry", "sweep: no expired refs"),
                Ok(n) => info!(target: "maild::expiry", "sweep reaped {n} blob_refs"),
                Err(e) => warn!(target: "maild::expiry", "sweep failed: {e:#}"),
            }
        }
    }

    /// Run one sweep across all known sets. Returns the number of
    /// `blob_refs` rows reaped. Public so tests and admin commands
    /// can drive the sweep without spawning the loop.
    pub async fn sweep(&self) -> Result<u64> {
        let mds = Arc::clone(&self.mds);
        tokio::task::spawn_blocking(move || sweep_blocking(&mds, now_secs())).await?
    }
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Synchronous sweep. Exposed for unit tests that drive a fake clock
/// (`now` parameter) and exercise the per-set transaction directly.
pub fn sweep_blocking(mds: &SqliteCasMds, now: i64) -> Result<u64> {
    let sets = mds.list_sets()?;
    let mut total = 0u64;
    for set in sets {
        match mds.with_set_tx(&set, |tx| sweep_set(tx, now)) {
            Ok(n) => total += n,
            Err(e) => {
                // Don't abort the whole sweep on a single failing set —
                // the next tick retries it. Log and continue.
                warn!(
                    target: "maild::expiry",
                    "sweep set {} failed: {e:#}",
                    set.0,
                );
            }
        }
    }
    Ok(total)
}

fn sweep_set(
    tx: &mut cosmix_mds::SqliteSetTx<'_>,
    now: i64,
) -> std::result::Result<u64, cosmix_mds::Error> {
    let expired: Vec<(String, Option<String>)> = {
        let mut stmt = tx
            .tx()
            .prepare(
                "SELECT blob_id, temp_item_id FROM blob_refs \
                 WHERE expires_at IS NOT NULL AND expires_at < ?1",
            )
            .map_err(|e| cosmix_mds::Error::Other(format!("prepare expired-refs query: {e}")))?;
        let rows = stmt
            .query_map(params![now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| cosmix_mds::Error::Other(format!("query expired refs: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| cosmix_mds::Error::Other(format!("collect expired refs: {e}")))?
    };

    let mut count = 0u64;
    for (blob_id_str, temp_item_id) in expired {
        if let Some(item_str) = temp_item_id.as_deref() {
            let item_uuid = uuid::Uuid::parse_str(item_str).map_err(|e| {
                cosmix_mds::Error::Other(format!(
                    "blob_refs.temp_item_id {item_str:?} not a UUID: {e}"
                ))
            })?;
            let item_id = ItemId(item_uuid);

            // Look up the (sole) membership container for the temp
            // item. Staging items are created with exactly one
            // membership (the `__upload_staging__` container); we read
            // it back rather than re-deriving by name so a future
            // refactor that moves staging items elsewhere doesn't
            // silently no-op the worker.
            //
            // Only `QueryReturnedNoRows` maps to `None` (the temp item
            // is already gone — operator surgery, partial prior sweep).
            // Any other rusqlite error must propagate so `with_set_tx`
            // rolls back; otherwise we'd commit a partial cleanup
            // (blob_refs row deleted, but temp item + membership +
            // MDS blob_ref refcount left intact) on transient SQLite
            // errors like lock contention or I/O.
            let container_str: Option<String> = match tx.tx().query_row(
                "SELECT container_id FROM membership \
                 WHERE item_id = ?1 LIMIT 1",
                params![item_id.0.to_string()],
                |r| r.get(0),
            ) {
                Ok(s) => Some(s),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => {
                    return Err(cosmix_mds::Error::Other(format!(
                        "lookup membership for temp item {}: {e}",
                        item_id.0
                    )));
                }
            };

            if let Some(c) = container_str {
                let c_uuid = uuid::Uuid::parse_str(&c).map_err(|e| {
                    cosmix_mds::Error::Other(format!(
                        "membership.container_id {c:?} not a UUID: {e}"
                    ))
                })?;
                tx.remove_membership(&item_id, &ContainerId(c_uuid))?;
            }
            // else: temp item already gone (operator surgery, partial
            // prior sweep). Fall through and drop the dangling ref.
        }

        let deleted = tx
            .tx()
            .execute(
                "DELETE FROM blob_refs WHERE blob_id = ?1",
                params![blob_id_str],
            )
            .map_err(|e| cosmix_mds::Error::Other(format!("delete blob_refs row: {e}")))?;
        if deleted == 1 {
            count += 1;
        }
    }
    Ok(count)
}
