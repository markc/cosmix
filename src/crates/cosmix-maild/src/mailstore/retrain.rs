//! IMAP junk-boundary retrain drain worker.
//!
//! `move_message` / `copy_message` enqueue a `mail_retrain_outbox`
//! row whenever an IMAP MOVE/COPY crosses the per-account `\Junk`
//! special-use boundary: into Junk → train spam, out of Junk → train
//! ham. JMAP retrains *inline* (it has the message bytes in hand);
//! IMAP cannot, so it defers. This worker is the deferred half — it
//! drains the outbox and applies the exact `classifier.retrain()`
//! path JMAP uses, giving Thunderbird (and any IMAP client) the same
//! spam/ham retraining behaviour as the JMAP client and NS 3.0's
//! Dovecot imapsieve.
//!
//! ## Ordering / correctness (the load-bearing part)
//!
//! `record_label` is per-`stamp_id` latest-wins with reversal: the
//! final corpus label for a stamp is whatever the *last* applied
//! label was. So replay order must equal user-action order. Two
//! design choices guarantee this without any schema change:
//!
//! 1. **Enqueue is `INSERT OR REPLACE`** (see `mailstore/mod.rs`): a
//!    re-drag of an already-pending `(stamp, label)` mints a *fresh
//!    rowid* instead of being dropped by `OR IGNORE`.
//! 2. **Drain reads `ORDER BY rowid ASC`** — true insertion/replace
//!    order — and processes a set's rows strictly sequentially
//!    (`await` each `retrain` before the next). Fed in event order,
//!    `record_label`'s reversal converges to the correct final state
//!    for every in/out/in interleave. A transient failure halts the
//!    rest of that set's batch (see `drain_set`) so a retried earlier
//!    row can never be replayed *after* a later same-stamp row.
//!
//! The finalise step keys its `DELETE`/`UPDATE` on the **exact
//! rowid** read in the claim. A re-drag landing between claim and
//! finalise replaces the row → new rowid outside the worker's read
//! set → not clobbered → reprocessed next tick. This closes the
//! claim/finalise race without a lease column.
//!
//! ## Accepted gap
//!
//! Since mds v1.8 (`mail_retrain_outbox.item_id` FK-cascades on item
//! delete) the *ordinary* drag-to-Junk-then-hard-delete case no longer
//! dead-letters: deleting the item reaps the outbox row in the same
//! `DELETE FROM item`, so the worker never sees an orphaned row. The
//! remaining accepted gap is the *blob-only* orphaning case — the item
//! row survives but its blob was GC'd, so `get_blob` returns
//! `BlobNotFound`; that one retrain dead-letters (parked at
//! `MAX_ATTEMPTS`). This is the deliberate best-effort decision: we do
//! not pin blobs or touch the GC path. Full blob-pin parity is a
//! parked substrate follow-up. The window is small and the lost signal
//! is marginal.
//!
//! Graceful shutdown is unwired, matching `ExpiryWorker`: the task
//! lives until process exit; maild's restart/replace cycle is the
//! supervision story. Assumes one maild process per MDS root (the
//! deployment norm); the rowid guard covers re-drag-during-drain
//! regardless.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use cosmix_maild_bayesian::{
    DefaultClassifier,
    classifier::Classifier,
    types::{Label, RetrainRequest},
};
use cosmix_maild_rules::AccountId;
use cosmix_mds::{ItemId, Mds, SetId, SqliteCasMds};
use rusqlite::params;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, info, warn};

/// Drain cadence. Spam training is not latency-sensitive, but a
/// short window shrinks the accepted delete-before-drain gap. 5s is
/// the best-effort compromise from the plan.
pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

/// A row is retried at most this many times before being
/// dead-lettered (`attempts` parked at `MAX_ATTEMPTS`, skipped by
/// the claim filter forever). Bounds retry on a persistently-failing
/// classifier or a permanently-missing message.
pub const MAX_ATTEMPTS: i64 = 5;

/// Rows claimed per set per tick. Bounds the per-tick work and the
/// claim transaction's hold time.
pub const BATCH: i64 = 64;

/// One row claimed from `mail_retrain_outbox`, carrying the exact
/// `rowid` so finalise can target it precisely (the re-drag guard).
///
/// `pub(crate)` (with `pub(crate)` fields) so the rowid-guard white
/// box test can drive `claim_batch` / `finalise` directly — the
/// claim→finalise seam is exactly where the guard lives.
pub(crate) struct ClaimedRow {
    pub(crate) rowid: i64,
    /// `RetrainRequest.stamp_id` — the per-account classifier corpus
    /// key (`maild.verdict.stamp_id`). Distinct from `item_id`: it
    /// keys `record_label`, not the blob lookup.
    pub(crate) stamp_id: String,
    pub(crate) account_id: i64,
    /// MDS `ItemId` UUID string — resolves the message bytes.
    pub(crate) item_id: String,
    /// `"junk"` or `"ham"` (CHECK-constrained at the schema).
    pub(crate) label: String,
}

/// What finalise should do with a claimed row after processing.
pub(crate) enum Finalise {
    /// Retrain applied (or was a no-op idempotent re-label) — drop
    /// the row by rowid.
    Done,
    /// Transient failure (classifier error, SQLite contention) —
    /// bump `attempts`, record the error, retry next tick.
    Retry(String),
    /// The message is gone before we could drain it (accepted
    /// best-effort gap) or the row is structurally malformed — park
    /// `attempts` at `MAX_ATTEMPTS` so the claim filter never picks
    /// it up again. No infinite retry.
    DeadLetter(String),
}

/// In-process tokio task that drains `mail_retrain_outbox` on a
/// fixed interval.
pub struct RetrainOutboxWorker {
    mds: Arc<SqliteCasMds>,
    classifier: Arc<DefaultClassifier>,
    tick: Duration,
}

impl RetrainOutboxWorker {
    pub fn new(mds: Arc<SqliteCasMds>, classifier: Arc<DefaultClassifier>) -> Self {
        Self {
            mds,
            classifier,
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
        // Skip the immediate t=0 tick so process bootstrap isn't
        // pinned to a drain (same rationale as `ExpiryWorker`).
        tick.tick().await;
        loop {
            tick.tick().await;
            match self.drain_once().await {
                Ok(0) => debug!(target: "maild::retrain", "drain: outbox empty"),
                Ok(n) => info!(target: "maild::retrain", "drain applied {n} retrain rows"),
                Err(e) => warn!(target: "maild::retrain", "drain failed: {e:#}"),
            }
        }
    }

    /// Drain one pass across all known sets. Returns the number of
    /// rows successfully retrained (excludes retries / dead-letters).
    /// Public so tests can drive a drain without spawning the loop.
    ///
    /// Ticks are serialized — the loop awaits this before the next
    /// `tick.tick()`, so there is no intra-worker overlap.
    pub async fn drain_once(&self) -> Result<u64> {
        let sets = {
            let mds = Arc::clone(&self.mds);
            tokio::task::spawn_blocking(move || mds.list_sets()).await??
        };
        let mut applied = 0u64;
        for set in sets {
            match self.drain_set(set).await {
                Ok(n) => applied += n,
                Err(e) => {
                    // One bad set must not abort the pass — the next
                    // tick retries it. Log and continue.
                    warn!(
                        target: "maild::retrain",
                        "drain set {} failed: {e:#}",
                        set.0,
                    );
                }
            }
        }
        Ok(applied)
    }

    async fn drain_set(&self, set: SetId) -> Result<u64> {
        // ---- Claim tx (short, write-locked Immediate). Read the
        // batch in rowid order, then release the per-set lock before
        // any blob IO or async retrain.
        let rows = {
            let mds = Arc::clone(&self.mds);
            tokio::task::spawn_blocking(move || mds.with_set_tx(&set, claim_batch)).await??
        };

        let mut applied = 0u64;
        // Strictly sequential: `record_label` is latest-wins per
        // stamp, so the rows that *do* get applied for a stamp must
        // be applied in rowid (= event) order.
        //
        // Ordering invariant under failure: a `Retry` leaves an
        // earlier row pending while later rows of the same stamp are
        // still in this batch. If we kept draining, a later
        // opposite-label row could be applied now and the retried
        // earlier row replayed *after* it next tick — `record_label`
        // would then converge to the wrong final label. So a `Retry`
        // halts the rest of this set's batch; the unprocessed rows
        // stay untouched and are reclaimed in order next tick. A
        // `DeadLetter` does NOT halt: that event is permanently
        // dropped (never applied), so applying later rows preserves
        // the apply-in-order-among-applied invariant. (Conservative
        // across stamps — one stamp's transient failure stalls the
        // whole set for a tick — but transient failures are rare and
        // correctness outranks per-tick throughput here.)
        for row in rows {
            let outcome = self.process_row(&set, &row).await;
            let success = matches!(outcome, Finalise::Done);
            let halt = matches!(outcome, Finalise::Retry(_));
            {
                let mds = Arc::clone(&self.mds);
                let rowid = row.rowid;
                tokio::task::spawn_blocking(move || {
                    mds.with_set_tx(&set, |tx| finalise(tx, rowid, &outcome))
                })
                .await??;
            }
            if success {
                applied += 1;
            }
            if halt {
                break;
            }
        }
        Ok(applied)
    }

    /// Resolve the message bytes for one claimed row and apply the
    /// retrain. Pure read of the MDS (no set tx held here — confirmed
    /// not a `with_set_tx` re-entry); the bayes write is the
    /// classifier's own concern.
    async fn process_row(&self, set: &SetId, row: &ClaimedRow) -> Finalise {
        let label = match row.label.as_str() {
            "junk" => Label::Spam,
            "ham" => Label::Ham,
            other => {
                // CHECK constraint should make this unreachable;
                // dead-letter rather than loop on a malformed row.
                return Finalise::DeadLetter(format!("unknown label {other:?}"));
            }
        };

        let item_uuid = match uuid::Uuid::parse_str(&row.item_id) {
            Ok(u) => u,
            Err(e) => {
                return Finalise::DeadLetter(format!("item_id {:?} not a UUID: {e}", row.item_id));
            }
        };
        let item_id = ItemId(item_uuid);

        // Resolve item → blob_hash → bytes off the async runtime.
        // `fetch_item_meta`/`get_blob` use the read pool, not a set
        // tx, so this is safe outside (and never inside) `with_set_tx`.
        let mds = Arc::clone(&self.mds);
        let set_owned = *set;
        let bytes = tokio::task::spawn_blocking(move || {
            let meta = mds.fetch_item_meta(&set_owned, &item_id)?;
            mds.get_blob(&meta.blob_hash)
        })
        .await;

        let message = match bytes {
            Ok(Ok(m)) => m,
            Ok(Err(cosmix_mds::Error::ItemNotFound(_)))
            | Ok(Err(cosmix_mds::Error::BlobNotFound(_))) => {
                // Accepted best-effort gap: the message was deleted
                // (last membership gone, blob GC'd) before we drained.
                return Finalise::DeadLetter("message gone before drain".to_string());
            }
            Ok(Err(e)) => {
                // Transient (lock contention, IO) — retry next tick.
                return Finalise::Retry(format!("resolve message: {e}"));
            }
            Err(join_err) => {
                return Finalise::Retry(format!("blob fetch task: {join_err}"));
            }
        };

        let account = AccountId::new(row.account_id.to_string());
        let req = RetrainRequest {
            stamp_id: &row.stamp_id,
            account: &account,
            message: &message,
            label,
        };
        // The exact call JMAP's `retrain_for_move` makes: shared
        // tokenize + `max_tokens_per_message` cap + `record_label`
        // reversal. This is the parity guarantee.
        match self.classifier.retrain(&req).await {
            Ok(_) => Finalise::Done,
            Err(e) => Finalise::Retry(format!("classifier.retrain: {e}")),
        }
    }
}

/// Claim up to `BATCH` undrained rows in rowid (= event) order.
/// Runs inside a short `with_set_tx`; the caller releases the lock
/// immediately after.
pub(crate) fn claim_batch(
    tx: &mut cosmix_mds::SqliteSetTx<'_>,
) -> std::result::Result<Vec<ClaimedRow>, cosmix_mds::Error> {
    let mut stmt = tx
        .tx()
        .prepare(
            "SELECT rowid, stamp_id, account_id, item_id, label \
             FROM mail_retrain_outbox \
             WHERE attempts < ?1 \
             ORDER BY rowid ASC \
             LIMIT ?2",
        )
        .map_err(|e| cosmix_mds::Error::Other(format!("prepare claim query: {e}")))?;
    let rows = stmt
        .query_map(params![MAX_ATTEMPTS, BATCH], |r| {
            Ok(ClaimedRow {
                rowid: r.get(0)?,
                stamp_id: r.get(1)?,
                account_id: r.get(2)?,
                item_id: r.get(3)?,
                label: r.get(4)?,
            })
        })
        .map_err(|e| cosmix_mds::Error::Other(format!("query claim rows: {e}")))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| cosmix_mds::Error::Other(format!("collect claim rows: {e}")))
}

/// Apply the post-processing decision for one row, keyed on the
/// exact `rowid` claimed (the re-drag guard — a row replaced since
/// the claim has a different rowid and is untouched here).
pub(crate) fn finalise(
    tx: &mut cosmix_mds::SqliteSetTx<'_>,
    rowid: i64,
    outcome: &Finalise,
) -> std::result::Result<(), cosmix_mds::Error> {
    match outcome {
        Finalise::Done => {
            tx.tx()
                .execute(
                    "DELETE FROM mail_retrain_outbox WHERE rowid = ?1",
                    params![rowid],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("delete drained outbox row: {e}")))?;
        }
        Finalise::Retry(msg) => {
            tx.tx()
                .execute(
                    "UPDATE mail_retrain_outbox \
                     SET attempts = attempts + 1, last_error = ?2 \
                     WHERE rowid = ?1",
                    params![rowid, msg],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("bump outbox attempts: {e}")))?;
        }
        Finalise::DeadLetter(msg) => {
            tx.tx()
                .execute(
                    "UPDATE mail_retrain_outbox \
                     SET attempts = ?2, last_error = ?3 \
                     WHERE rowid = ?1",
                    params![rowid, MAX_ATTEMPTS, msg],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("dead-letter outbox row: {e}")))?;
        }
    }
    Ok(())
}
